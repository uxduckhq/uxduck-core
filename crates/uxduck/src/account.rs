//! The account (ADR-0013 P9, S27): the daemon signs in to UXDuck's API with
//! an emailed code, keeps the bearer token in its credential store as
//! `account.token`, and reads the account from `/v1/me`. No page ever
//! holds the token. Signed out, or offline, everything local keeps working.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uxduck_contracts::dto::account::{AccountFlights, AccountState};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_engine::account::{AccountAction, AccountOrder, AccountPort};
use uxduck_engine::runtime::Credentials as _;

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// Where the bearer token is kept.
pub const TOKEN: &str = uxduck_engine::ACCOUNT_TOKEN;

/// How often a signed-in UXDuck reads its account, which keeps the 30-day
/// session alive (the server extends a session used after a day) and
/// forgets a token the server no longer takes.
const REFRESH_EVERY: Duration = Duration::from_hours(24);
/// How soon a signed-in UXDuck reads its account again while it does not
/// know which models the hosted model offers: until then the selector has
/// none (an offline start, or a server that did not answer).
const MODELS_RETRY: Duration = Duration::from_secs(30);

/// The engine's end: every call is a message to the account task.
#[derive(Debug, Clone)]
pub struct AccountChannel {
    tx: mpsc::UnboundedSender<AccountOrder>,
    initial: AccountState,
}

impl AccountPort for AccountChannel {
    fn initial_state(&self) -> AccountState {
        self.initial.clone()
    }
    fn request(&mut self, order: AccountOrder) {
        let _ = self.tx.send(order);
    }
}

#[derive(Debug)]
struct Account {
    api: Option<String>,
    client: Option<reqwest::Client>,
    credentials: FileCredentials,
    /// Serializes token changes with accepting a response for that token.
    session_change: Mutex<()>,
    /// The email the last code went to and its challenge, which the API binds
    /// to signing in or creating the account (S158).
    challenge: Mutex<Option<(String, String)>>,
    generation: AtomicU64,
    /// What `/v1/me` said last, for answers while offline.
    last: Mutex<AccountState>,
    analytics_internal: Mutex<Option<bool>>,
    /// What `/v1/me` said the hosted model offers, last time it said.
    hosted_models: Mutex<Option<Vec<String>>>,
    /// Where plugins installed from links are unpacked (S59).
    shared_dir: std::path::PathBuf,
    /// Where account logs are joined for a replay (`cache/replay`).
    data_dir: std::path::PathBuf,
}

struct SessionSnapshot {
    token: Option<String>,
}
impl SessionSnapshot {
    fn signed_in(&self) -> Result<String, ApiError> {
        self.token
            .clone()
            .ok_or_else(|| ApiError::new(ErrorKind::InvalidValue, "sign in first"))
    }
}

/// `/v1/me`; its 5-minute signed session is for the hosted model (S29),
/// which asks for a fresh one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Me {
    user: MeUser,
    product_updates: bool,
    #[serde(default)]
    analytics_excluded: bool,
    /// The models the hosted model offers this account; absent from an
    /// older API, which leaves the configured list.
    #[serde(default)]
    hosted_models: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeUser {
    id: String,
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    source_answered: Option<bool>,
    /// The terms version the account last agreed to (ADR-0047).
    #[serde(default)]
    terms_version: Option<String>,
}

/// Starts the account task.
#[must_use]
pub fn start(
    data_dir: &std::path::Path,
    credentials: FileCredentials,
    engine: EngineHandle,
) -> AccountChannel {
    let (tx, rx) = mpsc::unbounded_channel();
    let initial = AccountState {
        signed_in: credentials.account_session().is_some(),
        offline: credentials.account_session().is_some(),
        ..AccountState::default()
    };
    let account = Arc::new(Account {
        api: crate::feedback::api_url(),
        client: crate::online::client(Duration::from_secs(20)),
        credentials,
        session_change: Mutex::new(()),
        challenge: Mutex::new(None),
        generation: AtomicU64::new(0),
        last: Mutex::new(initial.clone()),
        analytics_internal: Mutex::new(None),
        hosted_models: Mutex::new(None),
        shared_dir: data_dir.join("plugins").join("shared"),
        data_dir: data_dir.to_owned(),
    });
    tokio::spawn(refresh(account.clone(), engine.clone()));
    tokio::spawn(run(account, rx, engine));
    AccountChannel { tx, initial }
}

/// Reads the account at start, then daily; every half minute while signed
/// in without knowing which models the hosted model offers.
async fn refresh(account: Arc<Account>, engine: EngineHandle) {
    loop {
        if let (Some(api), Some(client)) = (account.api.as_deref(), account.client.as_ref()) {
            if let Err(error) = account.get(api, client).await {
                tracing::error!(?error, "cannot read account identity");
            }
            let account = account.clone();
            let _ = engine
                .run(move |engine| {
                    engine.set_account_identity(account.cached_state());
                    engine.set_hosted_models(account.hosted_models());
                    if let Some(internal) = *account
                        .analytics_internal
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                    {
                        engine.set_analytics_internal(internal);
                    }
                })
                .await;
        }
        let unknown = account.token().is_some() && account.hosted_models().is_none();
        tokio::time::sleep(if unknown { MODELS_RETRY } else { REFRESH_EVERY }).await;
    }
}

async fn run(
    account: Arc<Account>,
    mut rx: mpsc::UnboundedReceiver<AccountOrder>,
    engine: EngineHandle,
) {
    while let Some(order) = rx.recv().await {
        let account = account.clone();
        let engine = engine.clone();
        let changes_session = matches!(
            order.action,
            AccountAction::SignOut { .. } | AccountAction::Verify { .. }
        );
        let generation = if changes_session {
            account.generation.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            account.generation.load(Ordering::SeqCst)
        };
        // One at a time would hold a quick `get` behind a slow sign-in.
        tokio::spawn(async move {
            let result = if order.workspace {
                account.workspace_action(order.action, generation).await
            } else {
                account.act_at(order.action, generation).await
            };
            let result = if account.generation.load(Ordering::SeqCst) == generation {
                result
            } else {
                Err(session_changed())
            };
            let id = order.id;
            let _ = engine
                .run(move |e| {
                    e.set_account_identity(account.cached_state());
                    e.set_hosted_models(account.hosted_models());
                    if let Some(internal) = *account
                        .analytics_internal
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                    {
                        e.set_analytics_internal(internal);
                    }
                    e.account_done(&id, result);
                })
                .await;
        });
    }
}

fn environment_action(action: &AccountAction) -> Option<Result<Value, ApiError>> {
    let result = match action {
        AccountAction::Environment => crate::dev_api::get(),
        AccountAction::SetEnvironment { environment } => crate::dev_api::set(*environment),
        _ => return None,
    };
    Some(
        result
            .map_err(|e| ApiError::new(ErrorKind::InvalidValue, e))
            .and_then(|state| {
                serde_json::to_value(state)
                    .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
            }),
    )
}

fn session_changed() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidValue,
        "The signed-in account changed. Try again.",
    )
}

fn unreachable() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidValue,
        "UXDuck cannot reach its server. Check the internet connection",
    )
}

/// The verify request: the challenge, the code, and the terms version the
/// form said continuing agrees to (ADR-0047), which the account records.
fn verify_body(challenge: &str, code: &str, terms_version: Option<&str>) -> Value {
    let mut body = json!({ "challengeId": challenge, "code": code });
    if let Some(version) = terms_version {
        body["terms"] = json!({ "version": version, "client": "desktop" });
    }
    body
}

fn unreadable_reply() -> ApiError {
    ApiError::new(
        ErrorKind::Internal,
        "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
    )
}

impl Account {
    fn token(&self) -> Option<String> {
        self.credentials.account_session()
    }

    fn cached_state(&self) -> AccountState {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn snapshot(&self, generation: u64) -> Result<SessionSnapshot, ApiError> {
        let _change = self
            .session_change
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(session_changed());
        }
        Ok(SessionSnapshot {
            token: self.token(),
        })
    }

    async fn workspace_action(
        &self,
        action: AccountAction,
        generation: u64,
    ) -> Result<Value, ApiError> {
        let session = self.snapshot(generation)?;
        if self.credentials.get(TOKEN) != session.token {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Sign in as this computer's cloud owner.",
            ));
        }
        self.act_current(action, generation, &session).await
    }

    fn hosted_models(&self) -> Option<Vec<String>> {
        self.hosted_models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn remember(&self, state: &AccountState) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = state.clone();
        *self
            .analytics_internal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(state.analytics_excluded);
    }

    #[cfg(test)]
    async fn act(&self, action: AccountAction) -> Result<Value, ApiError> {
        self.act_at(action, self.generation.load(Ordering::SeqCst))
            .await
    }

    /// Any operator of this computer acts under its one sign-in (ADR-0033,
    /// one account per computer); only the person's own sign-in or sign-out
    /// changes it.
    async fn act_at(&self, action: AccountAction, generation: u64) -> Result<Value, ApiError> {
        let session = self.snapshot(generation)?;
        let result = self.act_current(action, generation, &session).await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return Err(session_changed());
        }
        result
    }

    #[allow(clippy::too_many_lines, reason = "one arm per account action")]
    async fn act_current(
        &self,
        action: AccountAction,
        generation: u64,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        if let Some(result) = environment_action(&action) {
            return result;
        }
        let (Some(api), Some(client)) = (self.api.as_deref(), self.client.as_ref()) else {
            return match action {
                AccountAction::Get => to_json(&AccountState::default()),
                _ => Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "This build of UXDuck can't sign in to an account.",
                )),
            };
        };
        match action {
            AccountAction::Environment | AccountAction::SetEnvironment { .. } => unreachable!(),
            AccountAction::Get => to_json(&self.get(api, client).await?),
            // One way in (S158): the API emails an existing account a sign-in
            // code and anyone else a code that creates their account.
            AccountAction::SignIn { email } => {
                *self
                    .challenge
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                let answer = client
                    .post(format!("{api}/v1/auth/email-code/request"))
                    .json(&json!({ "email": email, "intent": "continue" }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let id = refused_or(answer)
                    .await?
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|body| body["challengeId"].as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        ApiError::new(ErrorKind::Internal, "the server sent no challenge")
                    })?;
                *self
                    .challenge
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some((email, id));
                Ok(json!({}))
            }
            AccountAction::Verify {
                email,
                code,
                product_updates,
                terms_version,
            } => {
                let challenge = self
                    .challenge
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .filter(|(sent_to, _)| *sent_to == email)
                    .map(|(_, id)| id)
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::InvalidValue,
                            "Ask for a new code, then try again.",
                        )
                    })?;
                let answer = client
                    .post(format!("{api}/v1/auth/email-code/verify"))
                    .json(&verify_body(&challenge, &code, terms_version.as_deref()))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let token = answer
                    .headers()
                    .get("set-auth-token")
                    .and_then(|t| t.to_str().ok())
                    .map(str::to_owned);
                let answer = refused_or(answer).await?;
                // Used now; a wrong code (refused above) keeps it for another try.
                *self
                    .challenge
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                // Whether this sign-in made the account (S114).
                let created = answer
                    .json::<Value>()
                    .await
                    .is_ok_and(|body| body["isNew"] == json!(true));
                let token = token.ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::Internal,
                        "the server signed in but gave no token",
                    )
                })?;
                {
                    let _change = self
                        .session_change
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if self.generation.load(Ordering::SeqCst) != generation {
                        return Err(session_changed());
                    }
                    let mut credentials = self.credentials.clone();
                    credentials
                        .set(TOKEN, &token)
                        .map_err(|e| ApiError::new(ErrorKind::Internal, e))?;
                    self.remember(&AccountState::default());
                }
                // The account lists this session by the computer's name.
                let _ = client
                    .put(format!("{api}/v1/me/sessions/current"))
                    .bearer_auth(&token)
                    .json(&json!({
                        // The API refuses a name over 64 characters.
                        "name": crate::sync::computer_name().chars().take(64).collect::<String>()
                    }))
                    .send()
                    .await;
                if product_updates {
                    self.set_updates(api, client, &token, true).await?;
                }
                to_json(&AccountState {
                    created,
                    ..self.get(api, client).await?
                })
            }
            AccountAction::ReplayLog { device_id, id } => {
                session.signed_in()?;
                let path = crate::replay::account_log(&self.data_dir, &device_id, &id).await?;
                Ok(json!({ "path": path.display().to_string() }))
            }
            AccountAction::Flights => {
                let token = session.signed_in()?;
                let answer = client
                    .get(format!("{api}/v1/flights"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let listed = refused_or(answer)
                    .await?
                    .json::<AccountFlights>()
                    .await
                    .map_err(|_| {
                        ApiError::new(
                            ErrorKind::Internal,
                            "UXDuck couldn't read the flight list. Try again, or update UXDuck.",
                        )
                    })?;
                if self.token().as_deref() != Some(token.as_str()) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidValue,
                        "The signed-in account changed. Refresh the flight list.",
                    ));
                }
                to_json(&listed)
            }
            action @ (AccountAction::Sessions
            | AccountAction::SignOutSession { .. }
            | AccountAction::SignOutOthers
            | AccountAction::RequestDeletion) => self.sessions(api, client, action, session).await,
            AccountAction::SignOut { device } => self.sign_out(api, client, device, session).await,
            AccountAction::SetUpdates { subscribed } => {
                let token = session.signed_in()?;
                self.set_updates(api, client, &token, subscribed).await?;
                to_json(&self.get(api, client).await?)
            }
            AccountAction::SetUsername { username } => {
                let token = session.signed_in()?;
                let answer = client
                    .put(format!("{api}/v1/me/username"))
                    .bearer_auth(&token)
                    .json(&json!({ "username": username }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                to_json(&self.get(api, client).await?)
            }
            AccountAction::AcceptTerms { version, context } => {
                let token = session.signed_in()?;
                let answer = client
                    .put(format!("{api}/v1/me/terms"))
                    .bearer_auth(&token)
                    .json(&json!({ "version": version, "context": context, "client": "desktop" }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                self.last
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .terms_version = Some(version);
                Ok(json!({}))
            }
            AccountAction::SetSource { source, other } => {
                let token = session.signed_in()?;
                let mut body = json!({ "source": source });
                if let Some(other) = other {
                    body["other"] = json!(other);
                }
                let answer = client
                    .put(format!("{api}/v1/me/source"))
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                if answer.status() != reqwest::StatusCode::CONFLICT {
                    refused(answer).await?;
                }
                self.last
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .source_answered = Some(true);
                Ok(json!({}))
            }
            AccountAction::SetName { name } => {
                let token = session.signed_in()?;
                let answer = client
                    .put(format!("{api}/v1/me/name"))
                    .bearer_auth(&token)
                    .json(&json!({ "name": name }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                to_json(&self.get(api, client).await?)
            }
            action => self.learning(api, client, action, session).await,
        }
    }

    /// The account's feedback from any computer, and keeping reports sent
    /// signed out in it.
    async fn sent_feedback(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        match action {
            AccountAction::SentFeedback => {
                let answer = client
                    .get(format!("{api}/v1/feedback/mine"))
                    .bearer_auth(session.signed_in()?)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let answer = refused_or(answer).await?;
                answer.json::<Value>().await.map_err(|_| unreadable_reply())
            }
            AccountAction::ClaimFeedback { reports } => {
                let reports: Vec<Value> = reports
                    .into_iter()
                    .map(|(id, key)| json!({ "id": id, "key": key }))
                    .collect();
                let answer = client
                    .post(format!("{api}/v1/feedback/claim"))
                    .bearer_auth(session.signed_in()?)
                    .json(&json!({ "reports": reports }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let answer = refused_or(answer).await?;
                answer.json::<Value>().await.map_err(|_| unreadable_reply())
            }
            _ => Err(ApiError::new(ErrorKind::Internal, "not a feedback action")),
        }
    }

    /// Feedback, a sent conversation, and the shared data (S29).
    async fn learning(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        match action {
            AccountAction::Feedback {
                response_id,
                rating,
                comment,
            } => {
                let answer = client
                    .post(format!("{api}/v1/turns/feedback"))
                    .bearer_auth(session.signed_in()?)
                    .json(
                        &json!({ "responseId": response_id, "rating": rating, "comment": comment }),
                    )
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                // Not kept: the app offers to send the conversation.
                if answer.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(json!({ "sent": false, "notShared": true }));
                }
                refused(answer).await?;
                Ok(json!({ "sent": true, "notShared": false }))
            }
            AccountAction::SendConversation {
                thread_id,
                model,
                items,
                rating,
                comment,
            } => {
                let answer = client
                    .post(format!("{api}/v1/turns/send"))
                    .bearer_auth(session.signed_in()?)
                    .json(&json!({
                        "thread": chat_id(&self.credentials, &thread_id),
                        "model": model,
                        "items": items,
                        "rating": rating,
                        "comment": comment,
                    }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                Ok(json!({ "sent": true, "notShared": false }))
            }
            AccountAction::Versions { entity, id } => {
                self.versions(api, client, session, &entity, &id).await
            }
            action @ (AccountAction::SentFeedback | AccountAction::ClaimFeedback { .. }) => {
                self.sent_feedback(api, client, action, session).await
            }
            AccountAction::Shared | AccountAction::DeleteShared => {
                let url = format!("{api}/v1/turns/mine");
                let request = if action_is_delete(&action) {
                    client.delete(url)
                } else {
                    client.get(url)
                };
                let answer = request
                    .bearer_auth(session.signed_in()?)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                if !answer.status().is_success() {
                    refused(answer).await?;
                    return Ok(json!({}));
                }
                answer.json::<Value>().await.map_err(|_| unreadable_reply())
            }
            action @ (AccountAction::SharePlugin { .. }
            | AccountAction::PluginShares
            | AccountAction::Unshare { .. }
            | AccountAction::SharedPlugin { .. }
            | AccountAction::InstallShared { .. }) => {
                self.plugins(api, client, action, session).await
            }
            action @ (AccountAction::CreateWatchLink { .. }
            | AccountAction::WatchLinks { .. }
            | AccountAction::RevokeWatchLink { .. }
            | AccountAction::RevokeWatchLinks { .. }) => {
                self.watch_links(api, client, action, session).await
            }
            _ => Err(ApiError::new(ErrorKind::Internal, "not a learning action")),
        }
    }

    /// Where the account is signed in, signing sessions out, and asking for
    /// the link that deletes it (S114, ADR-0013 P20 to P23).
    /// One synced entity's copies in the account (C5).
    async fn versions(
        &self,
        api: &str,
        client: &reqwest::Client,
        session: &SessionSnapshot,
        entity: &str,
        id: &str,
    ) -> Result<Value, ApiError> {
        let answer = client
            .get(format!("{api}/v1/sync/versions"))
            .query(&[("entity", entity), ("id", id)])
            .bearer_auth(session.signed_in()?)
            .send()
            .await
            .map_err(|_| unreachable())?;
        if !answer.status().is_success() {
            refused(answer).await?;
            return Ok(json!({ "versions": [] }));
        }
        answer.json::<Value>().await.map_err(|_| unreadable_reply())
    }

    async fn sessions(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        let token = session.signed_in()?;
        let base = format!("{api}/v1/me");
        // This computer's own session signs out the whole way: its watch
        // links end and the token is forgotten, reachable or not.
        if let AccountAction::SignOutSession { id, device } = &action
            && self.is_current(api, client, &token, id).await?
        {
            self.sign_out(api, client, device.clone(), session).await?;
            return to_json(&AccountState::default());
        }
        let request = match &action {
            AccountAction::Sessions => client.get(format!("{base}/sessions")),
            AccountAction::SignOutSession { id, .. } => {
                client.delete(format!("{base}/sessions/{}", path_part(id)))
            }
            AccountAction::SignOutOthers => client.post(format!("{base}/sessions/sign-out-others")),
            _ => client.post(format!("{base}/delete")),
        };
        let answer = request
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|_| unreachable())?;
        let answer = refused_or(answer).await?;
        match action {
            AccountAction::Sessions => answer.json::<Value>().await.map_err(|_| {
                ApiError::new(
                    ErrorKind::Internal,
                    "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
                )
            }),
            // This computer's own session gone, `/v1/me` refuses the token,
            // which is then forgotten.
            AccountAction::SignOutSession { .. } | AccountAction::SignOutOthers => {
                to_json(&self.get(api, client).await?)
            }
            _ => Ok(json!({})),
        }
    }

    /// Whether a session is this computer's own, by the server's list.
    async fn is_current(
        &self,
        api: &str,
        client: &reqwest::Client,
        token: &str,
        id: &str,
    ) -> Result<bool, ApiError> {
        let answer = client
            .get(format!("{api}/v1/me/sessions"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| unreachable())?;
        let listed: Value = refused_or(answer).await?.json().await.unwrap_or_default();
        Ok(listed["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["id"] == id && s["current"] == true))
    }

    /// Signs out, the server told if it can be; this computer's watch links
    /// end first (S95, W9).
    async fn sign_out(
        &self,
        api: &str,
        client: &reqwest::Client,
        device: Option<String>,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        let token = session.token.clone();
        if let Some(token) = &token {
            // This computer's watch links end with the session (S95, W9).
            if let Some(device) = device {
                let _ = client
                    .delete(format!(
                        "{api}/v1/watch-links?device={}",
                        path_part(&device)
                    ))
                    .bearer_auth(token)
                    .send()
                    .await;
            }
            // Best effort: the token is forgotten here whatever the server says.
            let _ = client
                .post(format!("{api}/v1/auth/sign-out"))
                .bearer_auth(token)
                .json(&json!({}))
                .send()
                .await;
        }
        let _change = self
            .session_change
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.token() != token {
            return Ok(json!({}));
        }
        let mut credentials = self.credentials.clone();
        credentials
            .set(TOKEN, "")
            .map_err(|e| ApiError::new(ErrorKind::Internal, e))?;
        self.remember(&AccountState::default());
        Ok(json!({}))
    }

    /// Watch links (S95, ADR-0023), through `/v1/watch-links`. The page's
    /// address is the app's: `api.uxduck.com` answers for `app.uxduck.com`.
    async fn watch_links(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        let token = session.signed_in()?;
        let base = format!("{api}/v1/watch-links");
        let read = |answer: reqwest::Response| async move {
            if !answer.status().is_success() {
                refused(answer).await?;
                return Ok(Value::Null);
            }
            answer.json::<Value>().await.map_err(|_| {
                ApiError::new(
                    ErrorKind::Internal,
                    "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
                )
            })
        };
        match action {
            AccountAction::CreateWatchLink {
                device,
                vehicle_id,
                vehicle_name,
                flown_by,
                hours,
            } => {
                let answer = client
                    .post(&base)
                    .bearer_auth(&token)
                    .json(&uxduck_contracts::dto::edge::WatchLinkCreate {
                        device_id: device,
                        device_name: Some(crate::sync::computer_name()),
                        vehicle_id,
                        vehicle_name,
                        flown_by,
                        hours,
                    })
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let made = read(answer).await?;
                let link_token = made["token"].as_str().unwrap_or_default();
                Ok(json!({
                    "link": made["link"],
                    "url": format!("{}/watch/{link_token}", app_origin(api)),
                }))
            }
            AccountAction::WatchLinks { device } => {
                let answer = client
                    .get(&base)
                    .query(&[("device", device)])
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                read(answer).await
            }
            AccountAction::RevokeWatchLink { id } => {
                let answer = client
                    .delete(format!("{base}/{}", path_part(&id)))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                Ok(json!({}))
            }
            AccountAction::RevokeWatchLinks { device } => {
                let answer = client
                    .delete(&base)
                    .query(&[("device", device)])
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                Ok(json!({}))
            }
            _ => Err(ApiError::new(
                ErrorKind::Internal,
                "not a watch link action",
            )),
        }
    }

    /// Shared plugins (S59, ADR-0006 X13), through `/v1/plugins`.
    async fn plugins(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
        session: &SessionSnapshot,
    ) -> Result<Value, ApiError> {
        let token = session.signed_in()?;
        let base = format!("{api}/v1/plugins");
        let read = |answer: reqwest::Response| async move {
            if !answer.status().is_success() {
                refused(answer).await?;
                return Ok(Value::Null);
            }
            answer.json::<Value>().await.map_err(|_| {
                ApiError::new(
                    ErrorKind::Internal,
                    "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
                )
            })
        };
        match action {
            AccountAction::SharePlugin {
                plugin_id,
                name,
                version,
                path,
                module,
                capabilities,
            } => {
                let bytes = crate::plugin_bundle::pack(std::path::Path::new(&path), &module)
                    .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
                let digest = crate::plugin_bundle::digest(&bytes);
                let answer = client
                    .put(format!("{base}/bundles/{digest}"))
                    .bearer_auth(&token)
                    .header("Content-Type", "application/octet-stream")
                    .body(bytes)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                let answer = client
                    .post(format!("{base}/shares"))
                    .bearer_auth(&token)
                    .json(&json!({
                        "pluginId": plugin_id,
                        "name": name,
                        "version": version,
                        "digest": digest,
                        "capabilities": capabilities,
                    }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                read(answer).await
            }
            AccountAction::PluginShares => {
                let answer = client
                    .get(format!("{base}/shares"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                read(answer).await
            }
            AccountAction::Unshare { token: link } => {
                let answer = client
                    .delete(format!("{base}/shares/{}", path_part(&link)))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                Ok(json!({}))
            }
            AccountAction::SharedPlugin { token: link } => {
                let answer = client
                    .get(format!("{base}/shares/{}", path_part(&link)))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                read(answer).await
            }
            AccountAction::InstallShared { token: link } => {
                self.install_shared(client, &base, &token, &path_part(&link))
                    .await
            }
            _ => Err(ApiError::new(ErrorKind::Internal, "not a plugin action")),
        }
    }

    /// Fetches a link's bundle, checks it against its digest and unpacks it
    /// for the engine to install: `plugin_sync::FetchedShare`.
    async fn install_shared(
        &self,
        client: &reqwest::Client,
        base: &str,
        token: &str,
        link: &str,
    ) -> Result<Value, ApiError> {
        let answer = client
            .get(format!("{base}/shares/{link}"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| unreachable())?;
        if !answer.status().is_success() {
            refused(answer).await?;
            return Err(ApiError::new(ErrorKind::NotFound, "no such link"));
        }
        let share: Value = answer.json().await.map_err(|_| {
            ApiError::new(
                ErrorKind::Internal,
                "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
            )
        })?;
        let (Some(id), Some(digest)) = (share["pluginId"].as_str(), share["digest"].as_str())
        else {
            return Err(ApiError::new(ErrorKind::NotFound, "no such link"));
        };
        let answer = client
            .get(format!("{base}/shares/{link}/bundle"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| unreachable())?;
        if !answer.status().is_success() {
            refused(answer).await?;
            return Err(ApiError::new(ErrorKind::NotFound, "no such link"));
        }
        let bytes = answer.bytes().await.map_err(|_| unreachable())?;
        if crate::plugin_bundle::digest(&bytes) != digest {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the shared plugin's files do not match its link",
            ));
        }
        if !uxduck_core::domain::plugin::valid_id(id) {
            return Err(ApiError::new(ErrorKind::InvalidValue, "not a plugin id"));
        }
        let dir = self.shared_dir.join(id);
        crate::plugin_bundle::unpack(&bytes, &dir)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        Ok(json!({
            "path": dir.display().to_string(),
            "sharedBy": share["sharedBy"].as_str().unwrap_or("someone"),
            "token": link,
        }))
    }

    async fn set_updates(
        &self,
        api: &str,
        client: &reqwest::Client,
        token: &str,
        subscribed: bool,
    ) -> Result<(), ApiError> {
        let answer = client
            .put(format!("{api}/v1/updates/me"))
            .bearer_auth(token)
            .json(&json!({ "subscribed": subscribed }))
            .send()
            .await
            .map_err(|_| unreachable())?;
        refused(answer).await
    }

    /// The account from `/v1/me`, or what was known last when offline. A
    /// token the server no longer takes is forgotten.
    async fn get(&self, api: &str, client: &reqwest::Client) -> Result<AccountState, ApiError> {
        loop {
            let Some(token) = self.token() else {
                self.remember(&AccountState::default());
                return Ok(AccountState::default());
            };
            let answer = client
                .get(format!("{api}/v1/me"))
                .bearer_auth(&token)
                .send()
                .await;
            let unauthorized = answer
                .as_ref()
                .is_ok_and(|answer| answer.status() == reqwest::StatusCode::UNAUTHORIZED);
            let mut state = match answer {
                Ok(answer) if answer.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    AccountState::default()
                }
                Ok(answer) if answer.status().is_success() => match answer.json::<Me>().await {
                    Ok(me) => {
                        *self
                            .hosted_models
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = me.hosted_models;
                        AccountState {
                            signed_in: true,
                            account_id: Some(me.user.id),
                            operator_id: None,
                            owns_workspace: None,
                            analytics_excluded: me.analytics_excluded,
                            email: Some(me.user.email),
                            name: Some(me.user.name).filter(|n| !n.is_empty()),
                            username: me.user.username,
                            product_updates: Some(me.product_updates),
                            offline: false,
                            created: false,
                            source_answered: me.user.source_answered,
                            terms_version: me.user.terms_version,
                        }
                    }
                    Err(_) => self.offline(),
                },
                _ => self.offline(),
            };
            let _change = self
                .session_change
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Sign-in and sign-out run concurrently with account reads. Retry
            // against the current token instead of accepting an old reply.
            if self.token().as_deref() != Some(token.as_str()) {
                continue;
            }
            if unauthorized {
                let mut credentials = self.credentials.clone();
                credentials
                    .set(TOKEN, "")
                    .map_err(|e| ApiError::new(ErrorKind::Internal, e))?;
            }
            if let Some(id) = state.account_id.as_deref() {
                state.owns_workspace =
                    Some(self.credentials.remember_person(&token, id).map_err(|e| {
                        ApiError::new(
                            ErrorKind::Internal,
                            format!("Cannot save cloud ownership: {e}"),
                        )
                    })?);
            }
            if !state.offline {
                self.remember(&state);
            }
            return Ok(state);
        }
    }

    fn offline(&self) -> AccountState {
        let last = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        AccountState {
            signed_in: true,
            product_updates: None,
            offline: true,
            ..last
        }
    }
}

/// A link's token as one URL path segment: anything else is dropped.
fn path_part(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

fn action_is_delete(action: &AccountAction) -> bool {
    matches!(action, AccountAction::DeleteShared)
}

/// A chat's id for the hosted model and sent conversations: random per
/// install and per chat, so the account never sees the daemon's own ids
/// (ADR-0013, `agent_turn`).
#[must_use]
pub fn chat_id(credentials: &FileCredentials, thread_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut store = credentials.clone();
    let salt = credentials
        .get(SALT)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let mut bytes = [0u8; 16];
            let _ = getrandom::fill(&mut bytes);
            let salt = hex(&bytes);
            let _ = store.set(SALT, &salt);
            salt
        });
    let digest = Sha256::digest(format!("{salt}:{thread_id}"));
    hex(&digest[..12])
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// The salt of [`chat_id`], kept with the account's token.
const SALT: &str = "model.salt";

/// The 5-minute signed session the hosted model takes (ADR-0013), from
/// `/v1/me`, kept until a minute before it runs out.
#[derive(Debug)]
pub struct Sessions {
    api: Option<String>,
    client: Option<reqwest::Client>,
    credentials: FileCredentials,
    cached: Mutex<Option<(String, std::time::Instant)>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeSession {
    signed_session: SignedSession,
}

#[derive(Deserialize)]
struct SignedSession {
    token: String,
}

impl Sessions {
    #[must_use]
    pub fn new(credentials: FileCredentials) -> Self {
        Self {
            api: crate::feedback::api_url(),
            client: crate::online::client(Duration::from_secs(20)),
            credentials,
            cached: Mutex::new(None),
        }
    }

    /// The account server, if this UXDuck has one.
    #[must_use]
    pub fn api(&self) -> Option<&str> {
        self.api.as_deref()
    }

    pub fn credentials(&self) -> &FileCredentials {
        &self.credentials
    }

    /// A session with at least a minute left.
    ///
    /// # Errors
    /// Why there is none, in words for the chat.
    pub async fn current(&self) -> Result<String, String> {
        let fresh = self
            .cached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|(_, until)| *until > std::time::Instant::now());
        if let Some((token, _)) = fresh {
            return Ok(token);
        }
        let token = self
            .credentials
            .get(TOKEN)
            .filter(|t| !t.is_empty())
            .ok_or("Sign in to UXDuck in Settings > Account to use the agent.")?;
        let (Some(api), Some(client)) = (self.api.as_deref(), self.client.as_ref()) else {
            return Err("This UXDuck has no account server.".into());
        };
        let answer = client
            .get(format!("{api}/v1/me"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| {
                "UXDuck cannot reach its server. Check the internet connection.".to_owned()
            })?;
        if answer.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("Sign in to UXDuck again in Settings > Account to use the agent.".into());
        }
        if let Some(why) = crate::online::redirected(&answer) {
            return Err(why);
        }
        let me: MeSession = answer
            .json()
            .await
            .map_err(|_| "UXDuck's server did not give a session.".to_owned())?;
        let token = me.signed_session.token;
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some((
            token.clone(),
            std::time::Instant::now() + Duration::from_mins(4),
        ));
        Ok(token)
    }
}

/// The answer when it succeeded, else the server's reason as an error.
async fn refused_or(answer: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    if answer.status().is_success() {
        return Ok(answer);
    }
    Err(refusal(answer).await)
}

/// The server's refusal as an error, with its own words.
async fn refused(answer: reqwest::Response) -> Result<(), ApiError> {
    if answer.status().is_success() {
        return Ok(());
    }
    Err(refusal(answer).await)
}

/// Why the server refused, in its words where it gave any.
async fn refusal(answer: reqwest::Response) -> ApiError {
    if let Some(why) = crate::online::redirected(&answer) {
        return ApiError::new(ErrorKind::InvalidValue, why);
    }
    let status = answer.status();
    let body: Value = answer.json().await.unwrap_or_default();
    let message = body["message"]
        .as_str()
        .or_else(|| body["error"].as_str())
        .map_or_else(|| format!("the server said {status}"), str::to_owned);
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn to_json(state: &impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(state).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// Where the app is, for a watch page's address: the API's host with `app`
/// for `api` (`api-staging.uxduck.com` is `app-staging.uxduck.com`), else
/// the production app.
fn app_origin(api: &str) -> String {
    match api.split_once("://") {
        Some((scheme, rest)) if rest.starts_with("api") => {
            format!(
                "{scheme}://app{}",
                rest.trim_start_matches("api").trim_end_matches('/')
            )
        }
        _ => "https://app.uxduck.com".to_owned(),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod watch_tests {
    use super::app_origin;

    #[test]
    fn a_watch_page_is_on_the_app_beside_the_api() {
        assert_eq!(
            app_origin("https://api.uxduck.com"),
            "https://app.uxduck.com"
        );
        assert_eq!(
            app_origin("https://api-staging.uxduck.com/"),
            "https://app-staging.uxduck.com"
        );
        assert_eq!(
            app_origin("http://127.0.0.1:45103"),
            "https://app.uxduck.com"
        );
    }
}

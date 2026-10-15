//! The account (ADR-0013 P9, S27): the daemon signs in to UXDuck's API with
//! an emailed code, keeps the bearer token in its credential store as
//! `account.token`, and reads the account from `/v1/me`. No page ever
//! holds the token. Signed out, or offline, everything local keeps working.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uxduck_contracts::dto::account::AccountState;
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

/// The engine's end: every call is a message to the account task.
#[derive(Debug, Clone)]
pub struct AccountChannel {
    tx: mpsc::UnboundedSender<AccountOrder>,
}

impl AccountPort for AccountChannel {
    fn request(&mut self, order: AccountOrder) {
        let _ = self.tx.send(order);
    }
}

#[derive(Debug)]
struct Account {
    api: Option<String>,
    client: Option<reqwest::Client>,
    credentials: FileCredentials,
    /// What `/v1/me` said last, for answers while offline.
    last: Mutex<AccountState>,
    /// Where plugins installed from links are unpacked (S59).
    shared_dir: std::path::PathBuf,
}

/// `/v1/me`; its 5-minute signed session is for the hosted model (S29),
/// which asks for a fresh one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Me {
    user: MeUser,
    product_updates: bool,
}

#[derive(Debug, Deserialize)]
struct MeUser {
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    username: Option<String>,
}

/// Starts the account task.
#[must_use]
pub fn start(
    data_dir: &std::path::Path,
    credentials: FileCredentials,
    engine: EngineHandle,
) -> AccountChannel {
    let (tx, rx) = mpsc::unbounded_channel();
    let account = Arc::new(Account {
        api: crate::feedback::api_url(),
        client: crate::online::client(Duration::from_secs(20)),
        credentials,
        last: Mutex::new(AccountState::default()),
        shared_dir: data_dir.join("plugins").join("shared"),
    });
    tokio::spawn(refresh(account.clone()));
    tokio::spawn(run(account, rx, engine));
    AccountChannel { tx }
}

/// Reads the account soon after start, then daily.
async fn refresh(account: Arc<Account>) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if let (Some(api), Some(client)) = (account.api.as_deref(), account.client.as_ref())
            && account.token().is_some()
        {
            account.get(api, client).await;
        }
        tokio::time::sleep(REFRESH_EVERY).await;
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
        // One at a time would hold a quick `get` behind a slow sign-in.
        tokio::spawn(async move {
            let result = account.act(order.action).await;
            let id = order.id;
            let _ = engine.run(move |e| e.account_done(&id, result)).await;
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

fn unreachable() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidValue,
        "UXDuck cannot reach its server; check the internet connection",
    )
}

impl Account {
    fn token(&self) -> Option<String> {
        self.credentials.get(TOKEN).filter(|t| !t.is_empty())
    }

    fn remember(&self, state: &AccountState) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = state.clone();
    }

    async fn act(&self, action: AccountAction) -> Result<Value, ApiError> {
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
            AccountAction::Get => to_json(&self.get(api, client).await),
            AccountAction::SignIn { email } => {
                let answer = client
                    .post(format!("{api}/v1/auth/email-otp/send-verification-otp"))
                    .json(&json!({ "email": email, "type": "sign-in" }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                Ok(json!({}))
            }
            AccountAction::Verify {
                email,
                code,
                product_updates,
            } => {
                let answer = client
                    .post(format!("{api}/v1/auth/sign-in/email-otp"))
                    .json(&json!({ "email": email, "otp": code }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                let token = answer
                    .headers()
                    .get("set-auth-token")
                    .and_then(|t| t.to_str().ok())
                    .map(str::to_owned);
                let answer = refused_or(answer).await?;
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
                let mut credentials = self.credentials.clone();
                credentials
                    .set(TOKEN, &token)
                    .map_err(|e| ApiError::new(ErrorKind::Internal, e))?;
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
                    ..self.get(api, client).await
                })
            }
            action @ (AccountAction::Sessions
            | AccountAction::SignOutSession { .. }
            | AccountAction::SignOutOthers
            | AccountAction::RequestDeletion) => self.sessions(api, client, action).await,
            AccountAction::SignOut { device } => self.sign_out(api, client, device).await,
            AccountAction::SetUpdates { subscribed } => {
                let token = self
                    .token()
                    .ok_or_else(|| ApiError::new(ErrorKind::InvalidValue, "sign in first"))?;
                self.set_updates(api, client, &token, subscribed).await?;
                to_json(&self.get(api, client).await)
            }
            AccountAction::SetUsername { username } => {
                let token = self.signed_in()?;
                let answer = client
                    .put(format!("{api}/v1/me/username"))
                    .bearer_auth(&token)
                    .json(&json!({ "username": username }))
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                refused(answer).await?;
                to_json(&self.get(api, client).await)
            }
            action => self.learning(api, client, action).await,
        }
    }

    /// Feedback, a sent conversation, and the shared data (S29).
    async fn learning(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
    ) -> Result<Value, ApiError> {
        match action {
            AccountAction::Feedback {
                response_id,
                rating,
                comment,
            } => {
                let answer = client
                    .post(format!("{api}/v1/turns/feedback"))
                    .bearer_auth(self.signed_in()?)
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
                    .bearer_auth(self.signed_in()?)
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
            // The account's computers (S78), online or when last seen.
            AccountAction::Computers => {
                let answer = client
                    .get(format!("{api}/v1/computers"))
                    .bearer_auth(self.signed_in()?)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                if !answer.status().is_success() {
                    refused(answer).await?;
                    return Ok(json!({ "computers": [] }));
                }
                answer.json::<Value>().await.map_err(|_| {
                    ApiError::new(
                        ErrorKind::Internal,
                        "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
                    )
                })
            }
            AccountAction::Shared | AccountAction::DeleteShared => {
                let url = format!("{api}/v1/turns/mine");
                let request = if action_is_delete(&action) {
                    client.delete(url)
                } else {
                    client.get(url)
                };
                let answer = request
                    .bearer_auth(self.signed_in()?)
                    .send()
                    .await
                    .map_err(|_| unreachable())?;
                if !answer.status().is_success() {
                    refused(answer).await?;
                    return Ok(json!({}));
                }
                answer.json::<Value>().await.map_err(|_| {
                    ApiError::new(
                        ErrorKind::Internal,
                        "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
                    )
                })
            }
            action @ (AccountAction::SharePlugin { .. }
            | AccountAction::PluginShares
            | AccountAction::Unshare { .. }
            | AccountAction::SharedPlugin { .. }
            | AccountAction::InstallShared { .. }) => self.plugins(api, client, action).await,
            action @ (AccountAction::CreateWatchLink { .. }
            | AccountAction::WatchLinks { .. }
            | AccountAction::RevokeWatchLink { .. }
            | AccountAction::RevokeWatchLinks { .. }) => {
                self.watch_links(api, client, action).await
            }
            _ => Err(ApiError::new(ErrorKind::Internal, "not a learning action")),
        }
    }

    /// Where the account is signed in, signing sessions out, and asking for
    /// the link that deletes it (S114, ADR-0013 P20 to P23).
    async fn sessions(
        &self,
        api: &str,
        client: &reqwest::Client,
        action: AccountAction,
    ) -> Result<Value, ApiError> {
        let token = self.signed_in()?;
        let base = format!("{api}/v1/me");
        // This computer's own session signs out the whole way: its watch
        // links end and the token is forgotten, reachable or not.
        if let AccountAction::SignOutSession { id, device } = &action
            && self.is_current(api, client, &token, id).await?
        {
            self.sign_out(api, client, device.clone()).await?;
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
                to_json(&self.get(api, client).await)
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
    ) -> Result<Value, ApiError> {
        if let Some(token) = self.token() {
            // This computer's watch links end with the session (S95, W9).
            if let Some(device) = device {
                let _ = client
                    .delete(format!(
                        "{api}/v1/watch-links?device={}",
                        path_part(&device)
                    ))
                    .bearer_auth(&token)
                    .send()
                    .await;
            }
            // Best effort: the token is forgotten here whatever the server says.
            let _ = client
                .post(format!("{api}/v1/auth/sign-out"))
                .bearer_auth(&token)
                .json(&json!({}))
                .send()
                .await;
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
    ) -> Result<Value, ApiError> {
        let token = self.signed_in()?;
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
    ) -> Result<Value, ApiError> {
        let token = self.signed_in()?;
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

    fn signed_in(&self) -> Result<String, ApiError> {
        self.token()
            .ok_or_else(|| ApiError::new(ErrorKind::InvalidValue, "sign in first"))
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
    async fn get(&self, api: &str, client: &reqwest::Client) -> AccountState {
        let Some(token) = self.token() else {
            return AccountState::default();
        };
        let answer = client
            .get(format!("{api}/v1/me"))
            .bearer_auth(&token)
            .send()
            .await;
        let state = match answer {
            Ok(answer) if answer.status() == reqwest::StatusCode::UNAUTHORIZED => {
                let mut credentials = self.credentials.clone();
                let _ = credentials.set(TOKEN, "");
                AccountState::default()
            }
            Ok(answer) if answer.status().is_success() => match answer.json::<Me>().await {
                Ok(me) => AccountState {
                    signed_in: true,
                    email: Some(me.user.email),
                    name: Some(me.user.name).filter(|n| !n.is_empty()),
                    username: me.user.username,
                    product_updates: Some(me.product_updates),
                    offline: false,
                    created: false,
                },
                Err(_) => self.offline(),
            },
            _ => self.offline(),
        };
        if !state.offline {
            self.remember(&state);
        }
        state
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
                "UXDuck cannot reach its server; check the internet connection.".to_owned()
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

fn to_json(state: &AccountState) -> Result<Value, ApiError> {
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

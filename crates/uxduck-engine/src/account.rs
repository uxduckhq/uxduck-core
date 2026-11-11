//! The account (ADR-0013 P9, S27): the daemon signs in to UXDuck's API, not
//! the page, and keeps the bearer token in its credential store. Every
//! `account/*` call goes to the composition root through [`AccountPort`],
//! which does the network work and answers through
//! [`Engine::account_done`]. A root with no port, such as the browser's
//! daemon, is signed out and says accounts are not available there.

use serde_json::Value;
use uxduck_contracts::dto::account::{AccountEnvironment, AccountState, SignupSource};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// What an `account/*` call asks of the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountAction {
    Get,
    /// Recorded flight-log summaries; a read-only `AccountFlights`.
    Flights,
    /// A flight log in the account joined on this computer for a replay
    /// (ADR-0039 R12): answered with `{"path": …}`.
    ReplayLog {
        device_id: String,
        id: String,
    },
    /// One synced entity's copies in the account (C5).
    Versions {
        entity: String,
        id: String,
    },
    Environment,
    SetEnvironment {
        environment: Option<AccountEnvironment>,
    },
    SignIn {
        email: String,
    },
    Verify {
        email: String,
        code: String,
        product_updates: bool,
        /// The terms version the form said continuing agrees to.
        terms_version: Option<String>,
    },
    /// Signs out; this computer's watch links end first (S95, W9).
    SignOut {
        device: Option<String>,
    },
    SetUpdates {
        subscribed: bool,
    },
    /// Chooses or changes the username (S73).
    SetUsername {
        username: String,
    },
    /// Sets the account's name.
    SetName {
        name: String,
    },
    /// Records in the account that its person agreed to the terms
    /// (ADR-0047); sent by the engine when an operator agrees.
    AcceptTerms {
        version: String,
        context: uxduck_contracts::dto::operator::TermsContext,
    },
    /// Where a new account heard of UXDuck (S251).
    SetSource {
        source: SignupSource,
        other: Option<String>,
    },
    /// Where the account is signed in (S114): an `AccountSessions`.
    Sessions,
    /// Signs out one session; this computer's own signs out as `SignOut`
    /// does, its watch links first.
    SignOutSession {
        id: String,
        device: Option<String>,
    },
    /// Signs out every session but this computer's.
    SignOutOthers,
    /// Emails a link that deletes the account (ADR-0013 P23).
    RequestDeletion,
    /// Thumbs up or down on a kept turn (S29).
    Feedback {
        response_id: String,
        rating: &'static str,
        comment: Option<String>,
    },
    /// A conversation sent with a thumbs-down, when its turns were not
    /// kept: its items as `{kind, text, toolCall}`.
    SendConversation {
        thread_id: String,
        model: String,
        items: Vec<serde_json::Value>,
        rating: &'static str,
        comment: Option<String>,
    },
    /// The account's feedback reports: an `AccountFeedbackList`.
    SentFeedback,
    /// Reports sent signed out, kept in the account by their keys
    /// (`id`, `key`): a `ClaimFeedbackResult`.
    ClaimFeedback {
        reports: Vec<(String, String)>,
    },
    /// What the account shared, and deleting it.
    Shared,
    DeleteShared,
    /// Uploads a plugin's bundle and makes a link to it (S59): answered
    /// with a `PluginShare`.
    SharePlugin {
        plugin_id: String,
        name: String,
        version: String,
        /// Its folder, and its module or script in it.
        path: String,
        module: String,
        capabilities: Vec<String>,
    },
    /// The account's live links: a `PluginSharesResult`.
    PluginShares,
    Unshare {
        token: String,
    },
    /// What a link offers: a `PluginShare`.
    SharedPlugin {
        token: String,
    },
    /// A watch link for this computer (S95): a `WatchLinkCreateResult`.
    CreateWatchLink {
        device: String,
        vehicle_id: Option<String>,
        vehicle_name: Option<String>,
        flown_by: String,
        hours: Option<u32>,
    },
    /// This computer's watch links: a `WatchLinkList`.
    WatchLinks {
        device: String,
    },
    RevokeWatchLink {
        id: String,
    },
    /// Every watch link of this computer (W9).
    RevokeWatchLinks {
        device: String,
    },
    /// Fetches and unpacks a link's bundle: answered with a
    /// `plugin_sync::FetchedShare`, which the engine installs.
    InstallShared {
        token: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountOrder {
    /// Answered with [`Engine::account_done`].
    pub id: String,
    pub action: AccountAction,
    /// Orders without a requesting connection operate on the computer's workspace.
    pub workspace: bool,
}

/// Where account calls go; the root answers through [`Engine::account_done`].
pub trait AccountPort: Send {
    /// The native host's cached association; this is not a new identity claim.
    fn initial_state(&self) -> AccountState {
        AccountState::default()
    }
    fn request(&mut self, order: AccountOrder);
}

/// What an `account/*` request asks of the root.
fn action_of(engine: &Engine, request: ClientRequest) -> Result<AccountAction, ApiError> {
    Ok(match request {
        ClientRequest::AccountGet(_) => AccountAction::Get,
        ClientRequest::AccountFlights(_) => AccountAction::Flights,
        ClientRequest::AccountVersions(p) => AccountAction::Versions {
            entity: p.entity,
            id: p.id,
        },
        ClientRequest::AccountEnvironment(_) => AccountAction::Environment,
        ClientRequest::AccountSetEnvironment(p) => {
            allow_environment_change(engine)?;
            AccountAction::SetEnvironment {
                environment: p.environment,
            }
        }
        ClientRequest::AccountSignIn(p) => AccountAction::SignIn {
            email: p.email.trim().to_lowercase(),
        },
        ClientRequest::AccountVerify(p) => AccountAction::Verify {
            email: p.email.trim().to_lowercase(),
            code: p.code.trim().to_owned(),
            product_updates: p.product_updates,
            terms_version: p.terms_version,
        },
        ClientRequest::AccountSignOut(_) => AccountAction::SignOut {
            device: (!engine.state.sync.device_id.is_empty())
                .then(|| engine.state.sync.device_id.clone()),
        },
        ClientRequest::AccountSetUpdates(p) => AccountAction::SetUpdates {
            subscribed: p.subscribed,
        },
        ClientRequest::AccountSetUsername(p) => AccountAction::SetUsername {
            username: p.username.trim().to_lowercase(),
        },
        ClientRequest::AccountSetName(p) => AccountAction::SetName {
            name: p.name.trim().to_owned(),
        },
        ClientRequest::AccountSetSource(p) => AccountAction::SetSource {
            source: p.source,
            other: p.other,
        },
        ClientRequest::AccountSessions(_) => AccountAction::Sessions,
        ClientRequest::AccountSignOutSession(p) => AccountAction::SignOutSession {
            id: p.id.trim().to_owned(),
            device: (!engine.state.sync.device_id.is_empty())
                .then(|| engine.state.sync.device_id.clone()),
        },
        ClientRequest::AccountSignOutOthers(_) => AccountAction::SignOutOthers,
        ClientRequest::AccountRequestDeletion(_) => AccountAction::RequestDeletion,
        ClientRequest::AccountShared(_) => AccountAction::Shared,
        ClientRequest::AccountFeedback(_) => AccountAction::SentFeedback,
        ClientRequest::AccountClaimFeedback(p) => {
            if p.reports.len() > 50 {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "claim up to 50 reports at a time",
                ));
            }
            AccountAction::ClaimFeedback {
                reports: p.reports.into_iter().map(|r| (r.id, r.key)).collect(),
            }
        }
        ClientRequest::AccountDeleteShared(_) => AccountAction::DeleteShared,
        _ => return Err(ApiError::new(ErrorKind::Internal, "not an account method")),
    })
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    allow_private_account(engine, connection, &request)?;
    let action = action_of(engine, request)?;
    let needed = if matches!(
        action,
        AccountAction::Get
            | AccountAction::Flights
            | AccountAction::Versions { .. }
            | AccountAction::Environment
            | AccountAction::Shared
            | AccountAction::SentFeedback
            | AccountAction::Sessions
    ) {
        Capability::Read
    } else {
        Capability::Config
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may sign in or out",
        ));
    }
    if let AccountAction::SignIn { email } | AccountAction::Verify { email, .. } = &action
        && !plausible(email)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an email address, please",
        ));
    }
    if engine.account.is_none() {
        return match action {
            AccountAction::Get => to_value(&AccountState::default()),
            AccountAction::Sessions => {
                to_value(&uxduck_contracts::dto::account::AccountSessions::default())
            }
            _ => Err(ApiError::new(
                ErrorKind::InvalidValue,
                "signing in needs UXDuck Desktop",
            )),
        };
    }
    // Nothing more is published from the moment signing out is asked for,
    // before the account has heard of it (W9).
    if matches!(action, AccountAction::SignOut { .. }) {
        engine.signed_out_of_account();
    }
    engine.ask_account(action);
    Ok(Value::Null)
}

fn allow_private_account(
    engine: &Engine,
    connection: ConnectionId,
    request: &ClientRequest,
) -> Result<(), ApiError> {
    if !engine
        .connections
        .get(&connection)
        .and_then(|c| c.admitted.as_ref())
        .is_some_and(crate::Admitted::is_operator)
    {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Private accounts are available only to a local operator.",
        ));
    }
    if matches!(request, ClientRequest::AccountVerify(_))
        && engine
            .connections
            .get(&connection)
            .and_then(|c| c.operator_id.as_ref())
            .is_none()
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Choose a local operator before signing in.",
        ));
    }
    if matches!(
        request,
        ClientRequest::AccountVerify(_)
            | ClientRequest::AccountSignOut(_)
            | ClientRequest::AccountSignOutSession(_)
            | ClientRequest::AccountSignOutOthers(_)
    ) {
        engine.ensure_account_handover(connection)?;
    }
    Ok(())
}

/// Something with one `@` and a dot after it; the API checks properly.
fn plausible(email: &str) -> bool {
    let mut parts = email.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None) if !local.is_empty() && domain.contains('.')
    ) && email.len() <= 254
}

fn allow_environment_change(engine: &Engine) -> Result<(), ApiError> {
    if !cfg!(debug_assertions) {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            "Server selection is available in development daemons only.",
        ));
    }
    if engine
        .state
        .vehicles
        .values()
        .any(|v| v.armed && v.presence != uxduck_core::domain::vehicle::Presence::Offline)
    {
        return Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            "Disarm the vehicles before switching servers and restarting UXDuck.",
        ));
    }
    Ok(())
}

impl Engine {
    /// The computer's one sign-in, from the native account task: every local
    /// operator acts under it (ADR-0033, one account per computer).
    pub fn set_account_identity(&mut self, state: AccountState) {
        if self.account_identity == state {
            return;
        }
        // A new analytics identity only when the account changes: signing in
        // or out, or another account. Reading the same one again, online or
        // not, keeps it.
        let another_account = matches!(
            (&self.account_identity.account_id, &state.account_id),
            (Some(before), Some(after)) if before != after
        );
        if self.account_identity.signed_in != state.signed_in || another_account {
            self.reset_analytics_session();
        }
        let lost_session = self.account_identity.signed_in && !state.signed_in;
        self.account_identity = state;
        if lost_session {
            self.signed_out_of_account();
        }
        let frame = crate::rpc::router::notification(
            &uxduck_contracts::rpc::ServerNotification::AccountChanged(
                uxduck_contracts::dto::Empty {},
            ),
        );
        let readers: Vec<_> = self
            .connections
            .iter()
            .filter(|(_, c)| {
                c.admitted
                    .as_ref()
                    .is_some_and(|a| a.granted.contains(&Capability::Read))
            })
            .map(|(id, _)| *id)
            .collect();
        for connection in readers {
            self.send(connection, &frame);
        }
    }

    /// Where account calls go; accounts are unavailable until the root sets it.
    pub fn set_account(&mut self, port: Box<dyn AccountPort>) {
        self.set_account_identity(port.initial_state());
        self.account = Some(port);
    }

    /// Hands an account call to the root, to be answered through
    /// [`Engine::account_done`]; returns the order's id.
    pub(crate) fn ask_account(&mut self, action: AccountAction) -> String {
        let workspace = self.replying_to.is_none();
        let id = format!("ac{}", &self.random.token()[..16]);
        if matches!(
            action,
            AccountAction::Verify { .. }
                | AccountAction::SignOut { .. }
                | AccountAction::SignOutSession { .. }
                | AccountAction::SignOutOthers
        ) {
            self.awaiting_identity.insert(id.clone());
        }
        // Answered when the root reports back.
        if let Some(waiting) = self.replying_to.take() {
            self.awaiting_account.insert(id.clone(), waiting);
        }
        if let Some(port) = self.account.as_mut() {
            port.request(AccountOrder {
                id: id.clone(),
                action,
                workspace,
            });
        }
        id
    }

    /// The root finished an account call: answers the request that made it.
    pub fn account_done(&mut self, id: &str, result: Result<Value, ApiError>) {
        self.awaiting_identity.remove(id);
        if self.awaiting_shared_installs.remove(id) {
            self.shared_fetched(id, result);
            return;
        }
        if let Some(item) = self.awaiting_feedback_items.remove(id)
            && result
                .as_ref()
                .is_ok_and(|value| value["sent"] == serde_json::json!(true))
        {
            self.feedback_sent(&item);
        }
        let result = if let Some((connection, params)) = self.replays.awaiting.remove(id) {
            self.account_log_joined(connection, &params, result)
        } else if let Some(awaiting) = self.awaiting_watch.remove(id) {
            self.watch_done(awaiting, result)
        } else {
            result
        };
        let Some((connection, request_id)) = self.awaiting_account.remove(id) else {
            return;
        };
        // `operatorId` stays in an account's answer for older pages: the
        // asking operator, who works under the computer's sign-in.
        let result = result.map(|mut value| {
            if value.get("signedIn").is_some()
                && let Some(operator) = self
                    .connections
                    .get(&connection)
                    .and_then(|c| c.operator_id.clone())
            {
                value["operatorId"] = Value::String(operator);
            }
            value
        });
        let frame = match result {
            Ok(value) => serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(error) => crate::rpc::router::error(&request_id, &error),
        };
        self.send(connection, &frame);
    }
}

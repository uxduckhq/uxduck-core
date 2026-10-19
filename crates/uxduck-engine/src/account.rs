//! The account (ADR-0013 P9, S27): the daemon signs in to UXDuck's API, not
//! the page, and keeps the bearer token in its credential store. Every
//! `account/*` call goes to the composition root through [`AccountPort`],
//! which does the network work and answers through
//! [`Engine::account_done`]. A root with no port, such as the browser's
//! daemon, is signed out and says accounts are not available there.

use serde_json::Value;
use uxduck_contracts::dto::account::{AccountEnvironment, AccountState};
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
    /// The account's computers (S78): a `ComputerList`, which the engine
    /// answers with this computer's own state as `AccountComputers`.
    Computers,
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
}

/// Where account calls go; the root answers through [`Engine::account_done`].
pub trait AccountPort: Send {
    fn request(&mut self, order: AccountOrder);
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let action = match request {
        ClientRequest::AccountGet(_) => AccountAction::Get,
        ClientRequest::AccountFlights(_) => AccountAction::Flights,
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
        ClientRequest::AccountSessions(_) => AccountAction::Sessions,
        ClientRequest::AccountSignOutSession(p) => AccountAction::SignOutSession {
            id: p.id.trim().to_owned(),
            device: (!engine.state.sync.device_id.is_empty())
                .then(|| engine.state.sync.device_id.clone()),
        },
        ClientRequest::AccountSignOutOthers(_) => AccountAction::SignOutOthers,
        ClientRequest::AccountRequestDeletion(_) => AccountAction::RequestDeletion,
        ClientRequest::AccountShared(_) => AccountAction::Shared,
        ClientRequest::AccountComputers(_) => AccountAction::Computers,
        ClientRequest::AccountDeleteShared(_) => AccountAction::DeleteShared,
        _ => return Err(ApiError::new(ErrorKind::Internal, "not an account method")),
    };
    let needed = if matches!(
        action,
        AccountAction::Get
            | AccountAction::Flights
            | AccountAction::Environment
            | AccountAction::Shared
            | AccountAction::Computers
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
            AccountAction::Computers => to_value(&uxduck_contracts::dto::edge::AccountComputers {
                computers: Vec::new(),
                this_computer: None,
                edge: crate::edge::signed_out(),
            }),
            _ => Err(ApiError::new(
                ErrorKind::InvalidValue,
                "signing in needs UXDuck Desktop",
            )),
        };
    }
    let computers = action == AccountAction::Computers;
    let id = engine.ask_account(action);
    if computers {
        engine.awaiting_computers.insert(id);
    }
    Ok(Value::Null)
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
    /// Where account calls go; accounts are unavailable until the root sets it.
    pub fn set_account(&mut self, port: Box<dyn AccountPort>) {
        self.account = Some(port);
    }

    /// Hands an account call to the root, to be answered through
    /// [`Engine::account_done`]; returns the order's id.
    pub(crate) fn ask_account(&mut self, action: AccountAction) -> String {
        let id = format!("ac{}", &self.random.token()[..16]);
        // Answered when the root reports back.
        if let Some(waiting) = self.replying_to.take() {
            self.awaiting_account.insert(id.clone(), waiting);
        }
        if let Some(port) = self.account.as_mut() {
            port.request(AccountOrder {
                id: id.clone(),
                action,
            });
        }
        id
    }

    /// The root finished an account call: answers the request that made it.
    pub fn account_done(&mut self, id: &str, result: Result<Value, ApiError>) {
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
        let result = if self.awaiting_computers.remove(id) {
            self.computers_listed(result)
        } else if let Some(awaiting) = self.awaiting_watch.remove(id) {
            self.watch_done(awaiting, result)
        } else {
            result
        };
        let Some((connection, request_id)) = self.awaiting_account.remove(id) else {
            return;
        };
        let frame = match result {
            Ok(value) => serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
            Err(error) => crate::rpc::router::error(&request_id, &error),
        };
        self.send(connection, &frame);
    }
}

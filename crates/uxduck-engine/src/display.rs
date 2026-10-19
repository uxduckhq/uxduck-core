//! On-demand display transfer between connected UIs. Nothing is journaled.
use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::client::{
    DisplayClient, DisplayClientsResult, DisplayRequested, DisplaySnapshot,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};

use crate::rpc::{handlers::to_value, router};
use crate::{ConnectionId, Engine};

const MAX_SETTINGS_BYTES: usize = 1024 * 1024;
const PULL_TIMEOUT_US: u64 = 8_000_000;

#[derive(Debug)]
struct Pull {
    source: ConnectionId,
    requester: ConnectionId,
    rpc_id: Value,
    deadline_us: u64,
}

#[derive(Debug)]
struct Source {
    id: String,
    name: String,
}

#[derive(Debug, Default)]
pub(crate) struct DisplayRelay {
    sources: BTreeMap<ConnectionId, Source>,
    pending: BTreeMap<String, Pull>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one arm per phase of the targeted transfer"
)]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::ClientDisplayRegister(params) => {
            let name = params.name.trim();
            if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Give this client a name of 1 to 80 characters.",
                ));
            }
            if let Some(source) = engine.display.sources.get_mut(&connection) {
                name.clone_into(&mut source.name);
            } else {
                let id = format!("display-{}", engine.random.token());
                engine.display.sources.insert(
                    connection,
                    Source {
                        id,
                        name: name.to_owned(),
                    },
                );
            }
            Ok(json!({}))
        }
        ClientRequest::ClientDisplayList(_) => {
            let clients = engine
                .display
                .sources
                .iter()
                .filter_map(|(id, offered)| {
                    if *id == connection {
                        return None;
                    }
                    let source = engine.connections.get(id)?;
                    Some(DisplayClient {
                        id: offered.id.clone(),
                        name: offered.name.clone(),
                        operator_name: source
                            .operator_id
                            .as_ref()
                            .and_then(|id| engine.state.operators.get(id))
                            .map(|o| o.name.clone()),
                    })
                })
                .collect();
            to_value(&DisplayClientsResult { clients })
        }
        ClientRequest::ClientDisplayPull(params) => {
            let source = engine
                .display
                .sources
                .iter()
                .find_map(|(id, source)| (source.id == params.id).then_some(*id))
                .filter(|id| *id != connection)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::NotFound,
                        "That client disconnected. Open Remote clients to refresh the list.",
                    )
                })?;
            if engine
                .display
                .pending
                .values()
                .filter(|p| p.requester == connection)
                .count()
                >= 2
                || engine.display.pending.len() >= 64
            {
                return Err(ApiError::new(
                    ErrorKind::Busy,
                    "A display-settings pull is already waiting. Try again shortly.",
                ));
            }
            let request_id = format!("display-{}", engine.random.token());
            let (_, rpc_id) = engine
                .replying_to
                .take()
                .ok_or_else(|| ApiError::new(ErrorKind::Internal, "Missing display pull reply."))?;
            engine.display.pending.insert(
                request_id.clone(),
                Pull {
                    source,
                    requester: connection,
                    rpc_id,
                    deadline_us: engine.clock.now_us().saturating_add(PULL_TIMEOUT_US),
                },
            );
            engine.send(
                source,
                &router::notification(&ServerNotification::ClientDisplayRequested(
                    DisplayRequested { request_id },
                )),
            );
            Ok(Value::Null)
        }
        ClientRequest::ClientDisplayReply(params) => {
            let pull = engine
                .display
                .pending
                .get(&params.request_id)
                .ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, "That display-settings pull has ended.")
                })?;
            if pull.source != connection {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "Only the selected client can answer this pull.",
                ));
            }
            if params.settings.len() > MAX_SETTINGS_BYTES
                || serde_json::from_str::<Value>(&params.settings).is_err()
            {
                let pull = engine
                    .display
                    .pending
                    .remove(&params.request_id)
                    .ok_or_else(|| ApiError::new(ErrorKind::Internal, "Missing display pull."))?;
                engine.send(pull.requester, &router::error(&pull.rpc_id, &ApiError::new(ErrorKind::InvalidValue, "That client sent invalid display settings. Your settings have not changed.")));
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Send a display-settings JSON document under 1 MB.",
                ));
            }
            if let Some(pull) = engine.display.pending.remove(&params.request_id) {
                engine.send(pull.requester, &json!({ "jsonrpc": "2.0", "id": pull.rpc_id, "result": DisplaySnapshot { settings: params.settings } }));
            }
            Ok(json!({}))
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "Not a display transfer method.",
        )),
    }
}

impl Engine {
    /// Shares the existing once-a-second request-deadline wakeup; no new timer.
    pub fn expire_display_pulls(&mut self, now_us: u64) {
        let expired: Vec<String> = self
            .display
            .pending
            .iter()
            .filter(|(_, p)| p.deadline_us <= now_us)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if let Some(pull) = self.display.pending.remove(&id) {
                self.send(pull.requester, &router::error(&pull.rpc_id, &ApiError::new(ErrorKind::Busy, "That client did not answer in time. Your display settings have not changed.")));
            }
        }
    }

    pub(crate) fn disconnect_display(&mut self, connection: ConnectionId) {
        self.display.sources.remove(&connection);
        let gone: Vec<String> = self
            .display
            .pending
            .iter()
            .filter(|(_, p)| p.source == connection || p.requester == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(pull) = self.display.pending.remove(&id)
                && pull.requester != connection
            {
                self.send(
                    pull.requester,
                    &router::error(
                        &pull.rpc_id,
                        &ApiError::new(
                            ErrorKind::NotFound,
                            "That client disconnected. Your display settings have not changed.",
                        ),
                    ),
                );
            }
        }
    }
}

//! Watch links (S95, ADR-0023): made, listed and revoked at the owner's
//! computer through the account (`watchLink/*`), each journaled; how many
//! guests watch through each comes from the relay's `edge/demand`.
//! Turning off "Let my account watch this computer" ends them all (W9).

use serde_json::Value;
use uxduck_contracts::dto::edge::{
    EdgeState, VehicleWatching, WatchLinkCreateResult, WatchLinkListResult, WatchLinkStatus,
    WatchViewers, WatchViewersResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::events::{Actor, Change};

use crate::account::AccountAction;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// What an account order for a watch link does with its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Awaiting {
    Create,
    List,
    Revoke(String),
}

fn signed_in_first() -> ApiError {
    ApiError::new(
        ErrorKind::InvalidValue,
        "Sign in to your UXDuck account to share a watch link.",
    )
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let needed = if matches!(
        request,
        ClientRequest::WatchLinkList(_) | ClientRequest::WatchLinkViewers(_)
    ) {
        Capability::Read
    } else {
        Capability::Config
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may share or end watch links",
        ));
    }
    if matches!(request, ClientRequest::WatchLinkViewers(_)) {
        return to_value(&engine.viewers_by_vehicle());
    }
    if engine.account.is_none() {
        return match request {
            ClientRequest::WatchLinkList(_) => to_value(&WatchLinkListResult { links: Vec::new() }),
            _ => Err(ApiError::new(
                ErrorKind::InvalidValue,
                "watch links need UXDuck Desktop, signed in to an account",
            )),
        };
    }
    if engine.edge_status.state == EdgeState::SignedOut {
        return match request {
            ClientRequest::WatchLinkList(_) => to_value(&WatchLinkListResult { links: Vec::new() }),
            _ => Err(signed_in_first()),
        };
    }
    let device = engine.edge_device_id();
    let (action, awaiting) = match request {
        ClientRequest::WatchLinkCreate(params) => {
            // W3: only while the account may watch this computer.
            if !engine.state.settings.watch_from_account {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Turn on \"View this computer’s vehicles remotely\" in Settings › Account first.",
                ));
            }
            let flown_by = params.flown_by.trim().to_owned();
            if flown_by.is_empty() || flown_by.chars().count() > 64 {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "who flies is 1 to 64 characters",
                ));
            }
            if params.hours.is_some_and(|h| !(1..=168).contains(&h)) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "a watch link lasts 1 to 168 hours",
                ));
            }
            let vehicle_name = match &params.vehicle_id {
                Some(id) => Some(
                    engine
                        .state
                        .vehicles
                        .get(id)
                        .ok_or_else(|| {
                            ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}"))
                        })?
                        .name
                        .clone(),
                ),
                None => None,
            };
            (
                AccountAction::CreateWatchLink {
                    device,
                    vehicle_id: params.vehicle_id,
                    vehicle_name,
                    flown_by,
                    hours: params.hours,
                },
                Awaiting::Create,
            )
        }
        ClientRequest::WatchLinkList(_) => (AccountAction::WatchLinks { device }, Awaiting::List),
        ClientRequest::WatchLinkRevoke(params) => (
            AccountAction::RevokeWatchLink {
                id: params.link_id.clone(),
            },
            Awaiting::Revoke(params.link_id),
        ),
        _ => {
            return Err(ApiError::new(
                ErrorKind::Internal,
                "not a watch link method",
            ));
        }
    };
    let id = engine.ask_account(action);
    engine.awaiting_watch.insert(id, awaiting);
    Ok(Value::Null)
}

fn owner() -> Actor {
    crate::plugins::daemon()
}

impl Engine {
    /// How many guests watch through each link, from `edge/demand`: a
    /// change goes to every operator client as `watchLink/viewersChanged`.
    pub fn watch_viewers(&mut self, viewers: Vec<WatchViewers>) {
        let before = self.viewers_by_vehicle();
        self.watch_viewers = viewers;
        let now = self.viewers_by_vehicle();
        if now == before {
            return;
        }
        let frame =
            crate::rpc::router::notification(&ServerNotification::WatchLinkViewersChanged(now));
        let readers: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| {
                c.admitted
                    .as_ref()
                    .is_some_and(|a| a.granted.contains(&Capability::Read))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in readers {
            self.send(id, &frame);
        }
    }

    /// The vehicles guests watch now, with how many, for "N watching".
    #[must_use]
    pub fn viewers_by_vehicle(&self) -> WatchViewersResult {
        WatchViewersResult {
            vehicles: self
                .state
                .vehicles
                .keys()
                .map(|id| VehicleWatching {
                    vehicle_id: id.clone(),
                    watching: self.watching(id),
                })
                .filter(|v| v.watching > 0)
                .collect(),
        }
    }

    /// Guests watching a vehicle through any watch link, for "N watching".
    #[must_use]
    pub fn watching(&self, vehicle_id: &str) -> u32 {
        self.watch_viewers
            .iter()
            .filter(|v| v.vehicle_id.as_deref().is_none_or(|id| id == vehicle_id))
            .map(|v| v.count)
            .sum()
    }

    /// "Let my account watch this computer" went off: every link ends (W9).
    pub(crate) fn end_watch_links(&mut self) {
        if self.account.is_none() || self.edge_status.state == EdgeState::SignedOut {
            return;
        }
        let device = self.edge_device_id();
        // Asked on the side: the settings change that turned it off answers now.
        let replying = self.replying_to.take();
        let id = self.ask_account(AccountAction::RevokeWatchLinks { device });
        self.replying_to = replying;
        self.awaiting_watch
            .insert(id, Awaiting::Revoke(String::new()));
    }

    /// An account order for a watch link came back: journaled, and counts added.
    pub(crate) fn watch_done(
        &mut self,
        awaiting: Awaiting,
        result: Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        let value = result?;
        match awaiting {
            Awaiting::Create => {
                let created: WatchLinkCreateResult =
                    serde_json::from_value(value.clone()).map_err(|_| unreadable())?;
                self.commit_as(
                    owner(),
                    Change::WatchLinkCreated {
                        link: created.link.id.clone(),
                        vehicle: created.link.vehicle_id.clone(),
                        expires_at_ms: created.link.expires_at_ms,
                    },
                );
                Ok(value)
            }
            Awaiting::List => {
                let links: uxduck_contracts::dto::edge::WatchLinkList =
                    serde_json::from_value(value).map_err(|_| unreadable())?;
                let links = links
                    .links
                    .into_iter()
                    .map(|link| WatchLinkStatus {
                        watching: self
                            .watch_viewers
                            .iter()
                            .find(|v| v.link_id == link.id)
                            .map_or(0, |v| v.count),
                        link,
                    })
                    .collect();
                to_value(&WatchLinkListResult { links })
            }
            Awaiting::Revoke(link) => {
                self.commit_as(
                    owner(),
                    Change::WatchLinkRevoked {
                        link: (!link.is_empty()).then_some(link),
                    },
                );
                Ok(serde_json::json!({}))
            }
        }
    }
}

fn unreadable() -> ApiError {
    ApiError::new(
        ErrorKind::Internal,
        "UXDuck couldn't read the server's reply. Try again, or update UXDuck.",
    )
}

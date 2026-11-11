//! Watch links (S95, ADR-0023): made, listed and revoked at the owner's
//! computer through the account (`watchLink/*`), each journaled; how many
//! guests watch through each comes from the relay's `edge/demand`.
//! Turning off "Live sharing from this computer" ends them all (W9).

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::edge::{
    EdgeState, VehicleWatching, WatchLink, WatchLinkCreateResult, WatchLinkList,
    WatchLinkListResult, WatchLinkStatus, WatchViewers, WatchViewersResult,
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
    /// Every link of this computer, ordered while `watch_links_end_pending`.
    EndAll,
    /// This computer's links that have not ended, asked on the side.
    Live,
}

/// How long a refused or unanswered order to end every link waits for the next.
const END_RETRY_US: u64 = 30_000_000;

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
    if engine.account.is_none() || engine.edge_status.state == EdgeState::SignedOut {
        return match request {
            ClientRequest::WatchLinkList(_) => to_value(&WatchLinkListResult {
                links: Vec::new(),
                edge: engine.edge_status.clone(),
            }),
            _ if engine.account.is_none() => Err(ApiError::new(
                ErrorKind::InvalidValue,
                "watch links need UXDuck Desktop, signed in to an account",
            )),
            _ => Err(signed_in_first()),
        };
    }
    let device = engine.edge_device_id();
    let (action, awaiting) = match request {
        ClientRequest::WatchLinkCreate(params) => {
            // W3: only while live sharing is on.
            if !engine.watch_publishable() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    if engine.state.settings.live_sharing {
                        "This computer's earlier watch links are still ending. Try again in a moment."
                    } else {
                        "Turn on \"Live sharing from this computer\" in Settings › Account first."
                    },
                ));
            }
            let flown_by = params.flown_by.trim().to_owned();
            if flown_by.is_empty() || flown_by.chars().count() > 64 {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "the operator's name is 1 to 64 characters",
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

    /// Whether anything of this computer may go to the relay for watchers:
    /// the setting is on and no earlier order to end its links is waiting.
    #[must_use]
    pub(crate) fn watch_publishable(&self) -> bool {
        let settings = &self.state.settings;
        settings.live_sharing && !settings.watch_links_end_pending
    }

    /// Records, durably, that this computer's links must end, and says so
    /// to the account if it can be reached. Kept until the account answers.
    fn set_watch_links_end_pending(&mut self, pending: bool) {
        if self.state.settings.watch_links_end_pending == pending {
            return;
        }
        let mut settings = self.state.settings.clone();
        settings.watch_links_end_pending = pending;
        settings.version += 1;
        self.commit_as(owner(), Change::SettingsChanged { settings });
        if pending {
            self.watch_end_after_us = 0;
        }
    }

    /// "Live sharing from this computer" went off: every link ends (W9).
    pub(crate) fn end_watch_links(&mut self) {
        // Without an account there are no links anywhere to end.
        if self.account.is_none() {
            return;
        }
        self.set_watch_links_end_pending(true);
        self.ask_watch_links_end();
    }

    /// This computer lost its session, with or without signing out here: the
    /// account may not have heard that its links end, so it is asked at the
    /// next sign-in.
    pub(crate) fn signed_out_of_account(&mut self) {
        self.set_watch_links_end_pending(true);
        self.live_links = None;
    }

    /// Whether one of this computer's watch links has not ended.
    #[must_use]
    pub(crate) fn has_live_link(&self) -> bool {
        let now_ms = self.clock.now_us() / 1000;
        self.live_links
            .as_ref()
            .is_some_and(|links| links.values().any(|&ends| ends > now_ms))
    }

    fn know_live_links(&mut self, links: &[WatchLink]) {
        self.live_links = Some(
            links
                .iter()
                .map(|link| (link.id.clone(), link.expires_at_ms))
                .collect(),
        );
    }

    /// Asks the account which of this computer's links have not ended, while
    /// that is unknown and live sharing is on; a failed ask waits a while.
    fn ask_live_links(&mut self) {
        let settings = &self.state.settings;
        if self.live_links.is_some()
            || !settings.live_sharing
            || settings.watch_links_end_pending
            || self.account.is_none()
            || self.edge_status.state == EdgeState::SignedOut
        {
            return;
        }
        let now = self.clock.now_us();
        if now < self.live_links_after_us {
            return;
        }
        self.live_links_after_us = now.saturating_add(END_RETRY_US);
        let device = self.edge_device_id();
        let replying = self.replying_to.take();
        let id = self.ask_account(AccountAction::WatchLinks { device });
        self.replying_to = replying;
        self.awaiting_watch.insert(id, Awaiting::Live);
    }

    /// Asks the account to end this computer's links, if some must end, it
    /// can be reached and the last order is not still in its wait.
    fn ask_watch_links_end(&mut self) {
        if !self.state.settings.watch_links_end_pending
            || self.account.is_none()
            || self.edge_status.state == EdgeState::SignedOut
        {
            return;
        }
        let now = self.clock.now_us();
        if now < self.watch_end_after_us {
            return;
        }
        self.watch_end_after_us = now.saturating_add(END_RETRY_US);
        let device = self.edge_device_id();
        // Asked on the side: the settings change that turned it off answers now.
        let replying = self.replying_to.take();
        let id = self.ask_account(AccountAction::RevokeWatchLinks { device });
        self.replying_to = replying;
        self.awaiting_watch.insert(id, Awaiting::EndAll);
    }

    /// The account came back (signed in, or reconnected): any order to end
    /// links waits no longer.
    pub(crate) fn watch_account_reached(&mut self) {
        self.watch_end_after_us = 0;
        self.live_links_after_us = 0;
        self.ask_watch_links_end();
    }

    /// Asks again for links still to end whose last order failed or got no
    /// answer, and for the links that have not ended while unknown; called
    /// every second.
    pub fn watch_links_end_tick(&mut self) {
        self.ask_watch_links_end();
        self.ask_live_links();
    }

    /// An account order for a watch link came back: journaled, and counts added.
    pub(crate) fn watch_done(
        &mut self,
        awaiting: Awaiting,
        result: Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        // Refused or unreachable: an order to end every link keeps its mark, and the tick asks again.
        let value = result?;
        match awaiting {
            Awaiting::Create => {
                let created: WatchLinkCreateResult =
                    serde_json::from_value(value.clone()).map_err(|_| unreadable())?;
                self.live_links
                    .get_or_insert_default()
                    .insert(created.link.id.clone(), created.link.expires_at_ms);
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
                let links: WatchLinkList =
                    serde_json::from_value(value).map_err(|_| unreadable())?;
                self.know_live_links(&links.links);
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
                to_value(&WatchLinkListResult {
                    links,
                    edge: self.edge_status.clone(),
                })
            }
            Awaiting::Live => {
                let links: WatchLinkList =
                    serde_json::from_value(value).map_err(|_| unreadable())?;
                self.know_live_links(&links.links);
                Ok(serde_json::json!({}))
            }
            Awaiting::EndAll => {
                self.live_links = Some(BTreeMap::new());
                self.set_watch_links_end_pending(false);
                self.commit_as(owner(), Change::WatchLinkRevoked { link: None });
                Ok(serde_json::json!({}))
            }
            Awaiting::Revoke(link) => {
                if let Some(links) = self.live_links.as_mut() {
                    links.remove(&link);
                }
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

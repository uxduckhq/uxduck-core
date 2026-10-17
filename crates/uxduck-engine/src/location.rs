//! Where this computer is (S137, ADR-0030). A person turns it on with
//! `location/start`; the tray, which the system knows as UXDuck, is told
//! `tray/locationWanted` and reports what the system says with
//! `tray/location`; every connection that asked hears `location/changed`.
//! The fix lives here only: never journaled, written or synced (L3).

use std::collections::BTreeSet;

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::location::{
    LocationFix, LocationState, LocationStatus, TrayLocationParams, TrayLocationWanted,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Said when there is no menu-bar app to ask the system (L7).
pub const NO_TRAY: &str = "UXDuck can only find this computer's location while it's running on a desktop, with its icon in the menu bar.";
/// Said when the menu-bar app stops while location is on.
pub const TRAY_GONE: &str =
    "UXDuck stopped finding this computer's location. Open UXDuck again to carry on.";

#[derive(Debug, Default)]
pub(crate) struct Location {
    status: LocationStatus,
    /// A person has location on.
    wanted: bool,
    /// Connections that asked, which hear `location/changed`.
    watchers: BTreeSet<ConnectionId>,
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    operator: bool,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::LocationStatus(_) => {
            // An agent without it was asked first (approvals.rs, L4).
            if !granted.contains(&Capability::Location) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Location.denied_message(),
                ));
            }
            engine.location.watchers.insert(connection);
            to_value(&engine.location.status)
        }
        ClientRequest::LocationStart(_) => {
            require_operator(operator)?;
            engine.location.watchers.insert(connection);
            engine.start_location();
            to_value(&engine.location.status)
        }
        ClientRequest::LocationStop(_) => {
            require_operator(operator)?;
            engine.location.watchers.insert(connection);
            engine.stop_location();
            to_value(&engine.location.status)
        }
        ClientRequest::TrayLocation(params) => {
            require_operator(operator)?;
            engine.tray_location(params);
            to_value(&Empty {})
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a location method")),
    }
}

fn require_operator(operator: bool) -> Result<(), ApiError> {
    if operator {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only a person can turn this computer's location on or off",
        ))
    }
}

impl Engine {
    fn start_location(&mut self) {
        self.location.wanted = true;
        // A menu-bar app on its way (just started, or starting again) is
        // asked when it joins (`tray_joined_location`).
        let coming = self.opened.desktop && self.state.settings.show_in_menu_bar;
        if !self.has_tray() && !coming {
            self.set_location(LocationStatus {
                state: LocationState::Unsupported,
                fix: None,
                reason: Some(NO_TRAY.into()),
            });
            return;
        }
        // Already on, or already asking: the tray carries on.
        if !matches!(
            self.location.status.state,
            LocationState::On | LocationState::Asking
        ) {
            let fix = self.location.status.fix.clone();
            self.set_location(LocationStatus {
                state: LocationState::Asking,
                fix,
                reason: None,
            });
        }
        self.tell_tray_location(true);
    }

    fn stop_location(&mut self) {
        let was = self.location.wanted;
        self.location.wanted = false;
        self.set_location(LocationStatus::default());
        if was {
            self.tell_tray_location(false);
        }
    }

    fn tray_location(&mut self, params: TrayLocationParams) {
        // A late answer after location was turned off changes nothing.
        if !self.location.wanted {
            return;
        }
        let now_ms = self.clock.now_us() / 1000;
        let fix = match (params.state, params.latitude_deg, params.longitude_deg) {
            (LocationState::On, Some(latitude_deg), Some(longitude_deg))
                if latitude_deg.is_finite()
                    && longitude_deg.is_finite()
                    && latitude_deg.abs() <= 90.0
                    && longitude_deg.abs() <= 180.0 =>
            {
                Some(LocationFix {
                    latitude_deg,
                    longitude_deg,
                    accuracy_m: params.accuracy_m.filter(|a| a.is_finite() && *a >= 0.0),
                    at_ms: now_ms,
                })
            }
            // Asking again or unavailable: the last fix stays, greyed.
            (LocationState::Asking | LocationState::Unavailable, _, _) => {
                self.location.status.fix.clone()
            }
            _ => None,
        };
        let state = match params.state {
            LocationState::On if fix.is_none() => LocationState::Unavailable,
            LocationState::Off => LocationState::Unavailable,
            state => state,
        };
        let reason = match state {
            LocationState::On | LocationState::Asking => None,
            _ => params.reason,
        };
        self.set_location(LocationStatus { state, fix, reason });
    }

    /// The tray's connection closed: say so when location was on, keeping
    /// the last fix, and ask again when a tray comes back.
    pub(crate) fn location_disconnected(&mut self, id: ConnectionId) {
        self.location.watchers.remove(&id);
        let tray = self.tray_watchers.contains(&id);
        if tray && self.location.wanted && self.tray_watchers.len() == 1 {
            let fix = self.location.status.fix.clone();
            self.set_location(LocationStatus {
                state: LocationState::Unavailable,
                fix,
                reason: Some(TRAY_GONE.into()),
            });
        }
    }

    /// A tray started showing the menu: it asks the system if location is on.
    pub(crate) fn tray_joined_location(&mut self, connection: ConnectionId) {
        if !self.location.wanted {
            return;
        }
        let frame = crate::rpc::router::notification(&ServerNotification::TrayLocationWanted(
            TrayLocationWanted { on: true },
        ));
        self.send(connection, &frame);
        if !matches!(
            self.location.status.state,
            LocationState::On | LocationState::Asking
        ) {
            let fix = self.location.status.fix.clone();
            self.set_location(LocationStatus {
                state: LocationState::Asking,
                fix,
                reason: None,
            });
        }
    }

    fn tell_tray_location(&mut self, on: bool) {
        let frame = crate::rpc::router::notification(&ServerNotification::TrayLocationWanted(
            TrayLocationWanted { on },
        ));
        let trays: Vec<ConnectionId> = self.tray_watchers.iter().copied().collect();
        for connection in trays {
            self.send(connection, &frame);
        }
    }

    fn set_location(&mut self, status: LocationStatus) {
        if self.location.status == status {
            return;
        }
        self.location.status = status.clone();
        let frame = crate::rpc::router::notification(&ServerNotification::LocationChanged(status));
        // Only those still allowed: an agent's "for this connection" or
        // "Always allow", or a person.
        let watchers: Vec<ConnectionId> = self
            .location
            .watchers
            .iter()
            .copied()
            .filter(|id| {
                self.connections
                    .get(id)
                    .and_then(|c| c.admitted.as_ref())
                    .is_some_and(|a| {
                        a.granted.contains(&Capability::Location)
                            || a.extra.contains(&Capability::Location)
                    })
            })
            .collect();
        for connection in watchers {
            self.send(connection, &frame);
        }
    }
}

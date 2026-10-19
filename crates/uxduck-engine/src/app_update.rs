//! Full app update consent and restart admission. The host downloads and installs;
//! this engine alone decides when the current state permits a handoff.
use serde_json::Value;
use uxduck_contracts::dto::tray::TrayAlert;
use uxduck_contracts::dto::update::{AppUpdateStatus, UpdatePhase};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::command::ReceiptStatus;
use uxduck_core::domain::vehicle::Presence;

use crate::{Engine, rpc::handlers::to_value};

#[derive(Default)]
pub(crate) struct Updates {
    pub status: AppUpdateStatus,
    consent: Option<Consent>,
    dismissed: bool,
    check: bool,
    pub installing: bool,
}

struct Consent {
    version: String,
    when_idle: bool,
}

pub(crate) fn handle(
    engine: &mut Engine,
    request: &ClientRequest,
    operator: bool,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    if !matches!(request, ClientRequest::UpdateStatus(_)) && !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Only the UXDuck app on this computer can manage its updates.",
        ));
    }
    match request {
        ClientRequest::UpdateStatus(_) => {}
        ClientRequest::UpdateCheck(_) => engine.app_update.check = true,
        ClientRequest::UpdateLater(_) => {
            engine.app_update.consent = None;
            engine.app_update.dismissed = true;
        }
        ClientRequest::UpdateInstall(params) => {
            if engine.app_update.status.phase != UpdatePhase::Ready
                || engine.app_update.status.version.as_deref() != Some(&params.version)
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "That update is no longer ready. Check for updates again.",
                ));
            }
            let blockers = engine.app_update_blockers();
            if !params.when_idle && !blockers.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("UXDuck cannot restart yet: {}", blockers.join("; ")),
                ));
            }
            engine.app_update.consent = Some(Consent {
                version: params.version.clone(),
                when_idle: params.when_idle,
            });
            engine.app_update.dismissed = false;
        }
        _ => return Err(ApiError::new(ErrorKind::Internal, "not an update method")),
    }
    engine.push_tray();
    to_value(&engine.app_update_status())
}

impl Engine {
    /// Evaluate current state, including vehicles last seen armed before losing
    /// contact. Missing simulator provenance is treated as real hardware.
    #[must_use]
    pub fn app_update_blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        for vehicle in self.state.vehicles.values() {
            let simulated = self
                .state
                .links
                .get(&vehicle.link_id)
                .is_some_and(|link| link.kind.is_simulator());
            if !simulated && vehicle.armed {
                blockers.push(if vehicle.presence == Presence::Offline {
                    format!(
                        "{} was last seen armed. Reconnect and confirm it is disarmed",
                        vehicle.name
                    )
                } else {
                    format!("{} is armed", vehicle.name)
                });
            } else if !simulated && vehicle.presence != Presence::Offline {
                blockers.push(format!("{} is connected", vehicle.name));
            }
        }
        // Includes simulations, forwarding and every recording, including cameras
        // not assigned to a vehicle. Also covers a simulator before discovery.
        for stop in self.stop_preview().stops {
            use uxduck_contracts::dto::app::StopKind;
            let reason = match stop.kind {
                StopKind::Simulation => "Simulations are running. Stop them before installing",
                StopKind::ForwardLink => "Forwarding is active",
                StopKind::Recording => "A recording is in progress",
                StopKind::Vehicle => continue,
            };
            blockers.push(reason.into());
        }
        if self.state.controllers.values().any(|c| c.claim.is_some())
            || !self.remote.leases.is_empty()
        {
            blockers.push("A controller or remote control session is active".into());
        }
        if self
            .state
            .vehicles
            .keys()
            .any(|id| self.transfers.uses_vehicle(id))
            || self.state.file_transfers.values().any(|t| t.state.active())
            || !self.transfers.plan_jobs.is_empty()
        {
            blockers.push("A vehicle transfer is in progress".into());
        }
        if self
            .state
            .receipts
            .values()
            .any(|r| r.status == ReceiptStatus::Sent)
            || !self.state.approvals.is_empty()
            || !self.turns.is_empty()
        {
            blockers.push("A command, approval or AI task is in progress".into());
        }
        blockers
    }

    #[must_use]
    pub fn app_update_installing(&self) -> bool {
        self.app_update.installing
    }

    fn has_update_ui(&self) -> bool {
        self.connections.values().any(|c| {
            c.admitted
                .as_ref()
                .is_some_and(|a| matches!(a.kind, ClientKind::OperatorUi | ClientKind::Shell))
        })
    }

    #[must_use]
    pub fn app_update_visible(&self) -> bool {
        self.has_visible_ui()
    }

    #[must_use]
    pub fn app_update_status(&self) -> AppUpdateStatus {
        let mut status = self.app_update.status.clone();
        status.current.clone_from(&self.config.daemon_version);
        status.blockers = self.app_update_blockers();
        if self
            .app_update
            .consent
            .as_ref()
            .is_some_and(|c| c.when_idle)
            && self.has_update_ui()
        {
            status
                .blockers
                .push("Close UXDuck's windows and browser tabs when you have finished".into());
        }
        status.install_when_idle = self.app_update.consent.is_some();
        status.dismissed = self.app_update.dismissed;
        status
    }

    /// A new version always requires fresh consent. Download errors don't start
    /// a pending installation, and consent never survives quitting UXDuck.
    pub fn app_update_publish(&mut self, status: AppUpdateStatus) {
        if self.app_update.status.version != status.version || status.phase != UpdatePhase::Ready {
            self.app_update.consent = None;
            self.app_update.dismissed = false;
        }
        self.app_update.status = status;
        self.push_tray();
    }

    #[must_use]
    pub fn app_update_pending(&self) -> bool {
        self.app_update.check || self.app_update.consent.is_some()
    }

    pub fn app_update_take_check(&mut self) -> bool {
        std::mem::take(&mut self.app_update.check)
    }

    #[must_use]
    pub fn app_update_approved(&self) -> bool {
        self.app_update.status.phase == UpdatePhase::Ready
            && self.app_update.consent.as_ref().is_some_and(|consent| {
                self.app_update.status.version.as_deref() == Some(consent.version.as_str())
                    && (!consent.when_idle || !self.has_update_ui())
            })
            && self.app_update_blockers().is_empty()
    }

    /// Called only after the installer worker is ready. This final check and
    /// freeze run in the same engine turn, preventing new RPC work before stop.
    pub fn app_update_handoff(&mut self) -> bool {
        if !self.app_update_approved() {
            return false;
        }
        self.app_update.installing = true;
        self.app_update.status.phase = UpdatePhase::Installing;
        self.push_tray();
        self.stop = true;
        true
    }

    pub(crate) fn app_update_notice(&self) -> Option<TrayAlert> {
        let status = &self.app_update.status;
        // Only a genuinely empty ground station gets a system notification.
        // Known real vehicles, even offline, keep updates inside the app.
        if status.phase != UpdatePhase::Ready
            || self.app_update.dismissed
            || self.app_update.consent.is_some()
            || !self.state.vehicles.is_empty()
            || !self.app_update_blockers().is_empty()
        {
            return None;
        }
        Some(TrayAlert {
            id: status.version.clone()?,
            title: "A UXDuck update is ready".into(),
            message:
                "Open UXDuck to review and install it. Nothing installs without your permission."
                    .into(),
        })
    }
}

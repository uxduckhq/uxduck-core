//! UXDuck on a vehicle's companion computers (S297, ADR-0061 B1, B5, B9,
//! B10): how each computer's UXDuck compares with this one's, what Install,
//! Update and Remove will do, running them over the computer's pinned SSH
//! through [`CompanionPort`], and a notice, once per version, when a vehicle
//! that is connected and disarmed has a computer running an older UXDuck.

use serde_json::Value;
use uxduck_contracts::dto::companion_software::{
    ComputerInstallParams, ComputerSoftware, ComputerSoftwareParams, ComputerSoftwareResult,
    InstallPlan,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::companion::{CompanionComputer, CompanionRole};
use uxduck_core::domain::companion_software::{
    self as software, InstallInputs, InstallRecord, InstallState, Plan, SoftwareStatus,
};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};

use crate::commands::Raise;
use crate::companions::{CheckAnswer, CompanionOrder, Found, through};
use crate::{ConnectionId, Engine};

/// The notice's condition, with the computer and `<theirs>><ours>` after it.
const NOTICE: &str = "companion-update:";

/// What an Install, Update or Remove answered: how it went, and what the
/// computer runs now when it could be read after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallAnswer {
    pub vehicle_id: String,
    pub computer_id: String,
    pub record: InstallRecord,
    pub found: Option<Found>,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn inputs(params: &ComputerInstallParams) -> InstallInputs {
    InstallInputs {
        to: params.to.clone(),
        serial: params.serial.clone(),
    }
}

fn carries_mavlink(computer: &CompanionComputer) -> bool {
    computer.role == CompanionRole::Forward
}

impl Engine {
    fn ground_version(&self) -> String {
        self.config.daemon_version.clone()
    }

    fn software_vehicle(&self, id: &str) -> Result<Vehicle, ApiError> {
        self.vehicle(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("vehicle {id} not found")))
    }

    /// `computer/software`: each computer's UXDuck from its last check.
    pub(crate) fn computer_software(
        &self,
        params: &ComputerSoftwareParams,
    ) -> Result<Value, ApiError> {
        let vehicle = self.software_vehicle(&params.vehicle_id)?;
        let ground = self.ground_version();
        let computers = vehicle
            .companions
            .iter()
            .map(|computer| {
                let s = software::software(&ground, computer);
                ComputerSoftware {
                    computer_id: computer.id.clone(),
                    name: computer.name.clone(),
                    status: s.status.into(),
                    version: s.version,
                    missing: s.missing,
                    words: s.words,
                    offer: s.offer.map(Into::into),
                    managed: s.managed,
                    carries_mavlink: carries_mavlink(computer),
                    install: computer.install.as_ref().map(Into::into),
                }
            })
            .collect();
        serde_json::to_value(ComputerSoftwareResult {
            ground,
            armed: vehicle.armed,
            computers,
        })
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
    }

    fn install_plan_for(
        &self,
        params: &ComputerInstallParams,
    ) -> Result<(Vehicle, Plan), ApiError> {
        let vehicle = self.software_vehicle(&params.vehicle_id)?;
        let computer = vehicle
            .companions
            .iter()
            .find(|c| c.id == params.computer_id)
            .ok_or_else(|| invalid("Companion computer not found."))?;
        let plan = software::plan(
            &self.ground_version(),
            computer,
            params.action.into(),
            &inputs(params),
        )
        .map_err(invalid)?;
        Ok((vehicle, plan))
    }

    /// `computer/installPlan`: what it would do, changing nothing.
    pub(crate) fn computer_install_plan(
        &self,
        params: &ComputerInstallParams,
    ) -> Result<Value, ApiError> {
        let (_, plan) = self.install_plan_for(params)?;
        serde_json::to_value(InstallPlan::from(plan))
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
    }

    /// `computer/install`: runs the plan on the computer, only while the
    /// vehicle is disarmed (ADR-0061 B10), one operation per computer.
    pub(crate) fn install_computer(
        &mut self,
        connection: ConnectionId,
        params: &ComputerInstallParams,
    ) -> Result<Value, ApiError> {
        if self.companions.port.is_none() {
            return Err(invalid(
                "Use UXDuck Desktop or the uxduck daemon to install UXDuck on a companion computer.",
            ));
        }
        let (mut vehicle, plan) = self.install_plan_for(params)?;
        if vehicle.armed {
            return Err(invalid(format!(
                "Disarm {} first. UXDuck on its computers is installed, updated and removed only on the ground.",
                vehicle.name
            )));
        }
        let through = through(&vehicle, &params.computer_id);
        let at_us = self.clock.now_us();
        let computer = vehicle
            .companions
            .iter_mut()
            .find(|c| c.id == params.computer_id)
            .ok_or_else(|| invalid("Companion computer not found."))?;
        if computer
            .check
            .as_ref()
            .and_then(|c| c.host_key.as_ref())
            .is_none()
        {
            return Err(invalid(
                "Check the computer first to verify its SSH host key.",
            ));
        }
        let waits = self.take_computer(&computer.id, false)?;
        computer.install = Some(InstallRecord {
            action: plan.action,
            state: InstallState::Running,
            at_us,
            version: plan.version.clone(),
            steps: Vec::new(),
            problem: None,
        });
        let order = CompanionOrder::Install {
            vehicle_id: vehicle.id.clone(),
            computer: Box::new(computer.clone()),
            through,
            plan: Box::new(plan),
        };
        let vehicle = self.save_companions(Some(connection), vehicle);
        self.send_companion(params.computer_id.clone(), order, waits, true);
        serde_json::to_value(uxduck_contracts::dto::vehicle::Vehicle::from(&vehicle))
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
    }

    /// An Install, Update or Remove's answer from the host: kept with the
    /// computer, then what it runs now, as a check would read it.
    pub fn companion_installed(&mut self, answer: InstallAnswer) {
        if let Some(mut vehicle) = self.vehicle(&answer.vehicle_id).cloned()
            && let Some(computer) = vehicle
                .companions
                .iter_mut()
                .find(|c| c.id == answer.computer_id)
        {
            let mut record = answer.record;
            record.at_us = self.clock.now_us();
            computer.install = Some(record);
            self.save_companions(None, vehicle);
        }
        match answer.found {
            Some(found) => self.companion_checked(CheckAnswer {
                vehicle_id: answer.vehicle_id,
                computer_id: answer.computer_id,
                result: Ok(found),
            }),
            None => self.companions_free(&answer.computer_id),
        }
    }

    /// Says once per version, where people look, that a connected and
    /// disarmed vehicle's computer runs an older UXDuck, and takes back what
    /// it said once that is no longer so. Never in flight: an armed vehicle
    /// waits for its disarm (ADR-0061 B10).
    pub fn review_companion_software(&mut self) {
        let ground = self.ground_version();
        let on = self.state.settings.companion_update_notices;
        let stale: Vec<String> = self
            .state
            .notifications
            .values()
            .filter(|n| n.state != NotificationState::Resolved)
            .filter_map(|n| n.condition.as_deref()?.strip_prefix(NOTICE))
            .filter(|rest| {
                let Some((computer_id, key)) = rest.split_once(':') else {
                    return true;
                };
                let still = self.state.vehicles.values().any(|v| {
                    v.companions.iter().any(|c| {
                        c.id == computer_id && notice_key(&ground, c).as_deref() == Some(key)
                    })
                });
                !on || !still
            })
            .map(str::to_owned)
            .collect();
        for rest in stale {
            self.resolve(&format!("{NOTICE}{rest}"));
        }
        if !on {
            return;
        }
        let vehicles: Vec<Vehicle> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.presence == Presence::Online && !v.armed)
            .filter(|v| {
                v.companions.iter().any(|c| {
                    notice_key(&ground, c).is_some_and(|k| c.update_noticed.as_ref() != Some(&k))
                })
            })
            .cloned()
            .collect();
        for mut vehicle in vehicles {
            for computer in &mut vehicle.companions {
                let Some(key) = notice_key(&ground, computer) else {
                    continue;
                };
                if computer.update_noticed.as_ref() == Some(&key) {
                    continue;
                }
                let s = software::software(&ground, computer);
                let theirs = s.version.unwrap_or_default();
                let mut body = format!("This computer has {ground}. Update while disarmed.");
                if !s.missing.is_empty() {
                    body.push_str(" It needs it for ");
                    body.push_str(&s.missing.join(" and "));
                    body.push('.');
                }
                if carries_mavlink(computer) {
                    body.push_str(" MAVLink pauses for a few seconds while it restarts.");
                }
                self.raise(Raise {
                    severity: if s.status == SoftwareStatus::UpdateNeeded {
                        Severity::Warning
                    } else {
                        Severity::Info
                    },
                    kind: NotificationKind::Transient,
                    subject: Subject::Vehicle {
                        id: vehicle.id.clone(),
                    },
                    title: format!("{}'s {} runs UXDuck {theirs}", vehicle.name, computer.name),
                    body: Some(body),
                    actions: vec![NotificationAction {
                        label: "Update".into(),
                        action: Action::OpenCompanionSoftware {
                            vehicle_id: vehicle.id.clone(),
                            computer_id: computer.id.clone(),
                        },
                    }],
                    condition: Some(format!("{NOTICE}{}:{key}", computer.id)),
                    report: None,
                });
                computer.update_noticed = Some(key);
            }
            self.save_companions(None, vehicle);
        }
    }
}

/// `<theirs>><ours>` when the computer runs an older UXDuck than `ground`.
fn notice_key(ground: &str, computer: &CompanionComputer) -> Option<String> {
    let s = software::software(ground, computer);
    matches!(
        s.status,
        SoftwareStatus::UpdateAvailable | SoftwareStatus::UpdateNeeded
    )
    .then(|| format!("{}>{ground}", s.version.unwrap_or_default()))
}

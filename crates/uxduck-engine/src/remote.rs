//! Control from away (S80, ADR-0003 C9 and C10). Someone signed in to the
//! same account sends commands through the relay; the daemon's edge task
//! checks the cloud's signed grant and opens one `remote-operator`
//! connection per person, with `Trust::Relay`. Their commands then take the
//! computer's own command path, with its receipts and journal, plus these
//! rules:
//!
//! - nothing at all unless "Allow remote control from my account" is on at
//!   this computer, and only the classes it allows;
//! - the control lease for Mission and Critical commands, which anyone at
//!   the computer can take back;
//! - a Critical command the setting does not allow asks the people at the
//!   computer first, every time;
//! - each command once, by the id its sender's app gave it, and none older
//!   than its class's maximum age;
//! - nothing but commands and the lease from away: no settings, no
//!   parameters, no approvals for more.

use std::collections::VecDeque;

use serde_json::Value;
use uxduck_contracts::dto::command::Receipt as ReceiptDto;
use uxduck_contracts::dto::edge::{Lease, LeaseParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::command::{Command, CommandClass, RemoteBy};
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::policy::admission::Trust;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Commands from away remembered by their sender's id, for duplicates.
const REMEMBERED: usize = 500;

/// A vehicle's lease, and the connection of whoever holds it.
#[derive(Debug, Clone)]
pub(crate) struct Held {
    pub lease: Lease,
    pub connection: ConnectionId,
}

/// What the engine keeps for control from away: soft, never journaled.
#[derive(Debug, Default)]
pub(crate) struct Remote {
    /// By vehicle id.
    pub leases: std::collections::BTreeMap<String, Held>,
    /// (user id, command id, receipt id), oldest first.
    pub commands: VecDeque<(String, String, String)>,
}

fn lease_condition(vehicle_id: &str) -> String {
    format!("lease:{vehicle_id}")
}

fn class_word(class: CommandClass) -> &'static str {
    match class {
        CommandClass::Safety => "safety",
        CommandClass::Mission => "mission",
        CommandClass::Payload => "payload",
        CommandClass::Critical => "critical",
    }
}

impl Engine {
    /// Who is on the other end of a connection from away: their user id and name.
    #[must_use]
    pub fn remote_sender(&self, connection: ConnectionId) -> Option<(String, String)> {
        match &self.connections.get(&connection)?.trust {
            Trust::Relay { user_id, name } => Some((user_id.clone(), name.clone())),
            _ => None,
        }
    }

    /// Before anything else for a connection from away: what it may not ask
    /// at all, and a command it already sent. `None` lets it through.
    pub(crate) fn remote_gate(
        &mut self,
        connection: ConnectionId,
        request: &ClientRequest,
    ) -> Option<Result<Value, ApiError>> {
        let (user_id, _) = self.remote_sender(connection)?;
        let denied = |why: String| Some(Err(ApiError::new(ErrorKind::CapabilityDenied, why)));
        let params = match request {
            ClientRequest::CommandSend(params) => params,
            ClientRequest::CommandList(_)
            | ClientRequest::LeaseTake(_)
            | ClientRequest::LeaseRelease(_) => return None,
            _ => return denied("Remotely, you can only send commands and take control.".into()),
        };
        let allowed = &self.state.settings.remote_control;
        if allowed.is_empty() {
            return denied(
                "remote control is off at this computer. It is turned on there, in Settings".into(),
            );
        }
        let Some(command_id) = params.command_id.as_deref().filter(|id| !id.is_empty()) else {
            return Some(Err(ApiError::new(
                ErrorKind::InvalidParams,
                "a command from away needs its commandId",
            )));
        };
        // Sent twice, carried out once: the first one's receipt, as it is now.
        if let Some(receipt) = self.remote_receipt(&user_id, command_id) {
            return Some(to_value(&receipt));
        }
        let command: Command = params.command.clone().into();
        let class = command.class();
        if class != CommandClass::Critical && !allowed.contains(&class) {
            return denied(format!(
                "remote control at this computer does not allow {} commands",
                class_word(class)
            ));
        }
        if class.needs_lease() {
            let vehicle = self.vehicle(&params.vehicle_id).map(|v| v.id.clone());
            let holder = vehicle
                .as_ref()
                .and_then(|id| self.remote.leases.get(id))
                .map(|held| held.lease.clone());
            match holder {
                Some(lease) if lease.user_id == user_id => {}
                Some(lease) => {
                    return Some(Err(ApiError::new(
                        ErrorKind::Busy,
                        format!("{} has control of this vehicle", lease.name),
                    )));
                }
                None => {
                    return Some(Err(ApiError::new(
                        ErrorKind::Busy,
                        format!("take control first: {} commands need it", class_word(class)),
                    )));
                }
            }
        }
        None
    }

    /// Whether a request from away asks the people at the computer first,
    /// every time: a Critical command the setting does not allow.
    pub(crate) fn remote_critical_asks(
        &self,
        connection: ConnectionId,
        request: &ClientRequest,
    ) -> bool {
        let ClientRequest::CommandSend(params) = request else {
            return false;
        };
        self.remote_sender(connection).is_some()
            && Command::from(params.command.clone()).class() == CommandClass::Critical
            && !self
                .state
                .settings
                .remote_control
                .contains(&CommandClass::Critical)
    }

    /// For a command from away about to be sent: who sent it, and why it
    /// is too old if it is. An approved one is not too old: the people at
    /// the computer just said yes.
    pub(crate) fn remote_command(
        &self,
        connection: ConnectionId,
        command: &Command,
        command_id: Option<&str>,
        sent_at_ms: Option<u64>,
    ) -> Option<(RemoteBy, Option<String>)> {
        let (user_id, name) = self.remote_sender(connection)?;
        let by = RemoteBy {
            user_id,
            name,
            command_id: command_id.unwrap_or_default().to_owned(),
        };
        if self.running_approved {
            return Some((by, None));
        }
        let offset = self.edge_status.clock_offset_ms.unwrap_or(0);
        let now_ms = i64::try_from(self.clock.now_us() / 1000).unwrap_or(i64::MAX);
        let relay_now = u64::try_from(now_ms.saturating_add(offset)).unwrap_or(0);
        let max = command.max_age_ms();
        let age = sent_at_ms.map_or(u64::MAX, |at| relay_now.saturating_sub(at));
        #[allow(clippy::cast_precision_loss, reason = "seconds, to a tenth")]
        let expired = (age > max).then(|| {
            if age == u64::MAX {
                "it came without the time it was sent".to_owned()
            } else {
                format!(
                    "it was sent {:.1} s ago, more than the {:.0} s {} may be from away",
                    age as f64 / 1000.0,
                    max as f64 / 1000.0,
                    command.label()
                )
            }
        });
        Some((by, expired))
    }

    /// Remembers a command from away by its sender's id.
    pub(crate) fn remember_remote(&mut self, by: &RemoteBy, receipt_id: &str) {
        if by.command_id.is_empty() {
            return;
        }
        self.remote.commands.push_back((
            by.user_id.clone(),
            by.command_id.clone(),
            receipt_id.to_owned(),
        ));
        while self.remote.commands.len() > REMEMBERED {
            self.remote.commands.pop_front();
        }
    }

    /// A command from away's receipt, by its sender and the id they gave it:
    /// for a duplicate, and after a connection dropped before the answer.
    #[must_use]
    pub fn remote_receipt(&self, user_id: &str, command_id: &str) -> Option<ReceiptDto> {
        let receipt_id = self
            .remote
            .commands
            .iter()
            .rev()
            .find(|(user, command, _)| user == user_id && command == command_id)
            .map(|(_, _, receipt)| receipt)?;
        self.state.receipts.get(receipt_id).map(ReceiptDto::from)
    }

    /// Who holds a vehicle's lease from away.
    #[must_use]
    pub fn lease_of(&self, vehicle_id: &str) -> Option<Lease> {
        self.remote
            .leases
            .get(vehicle_id)
            .map(|held| held.lease.clone())
    }

    /// Someone at the computer: an operator client that is not from away.
    pub(crate) fn someone_here(&self) -> bool {
        self.connections.values().any(|c| {
            !matches!(c.trust, Trust::Relay { .. })
                && c.admitted.as_ref().is_some_and(|a| a.operator)
        })
    }

    /// `lease/take` and `lease/release` (S80).
    pub(crate) fn lease(
        &mut self,
        connection: ConnectionId,
        params: &LeaseParams,
        take: bool,
        operator: bool,
    ) -> Result<Value, ApiError> {
        let vehicle = self.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            )
        })?;
        let sender = self.remote_sender(connection);
        let held = self.lease_of(&vehicle.id);
        if !take {
            return self.give_back(&vehicle, sender.as_ref(), held, operator);
        }
        let Some((user_id, name)) = sender else {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "You're at the computer, so you already have control.",
            ));
        };
        let allowed = &self.state.settings.remote_control;
        if !allowed
            .iter()
            .any(|class| matches!(class, CommandClass::Mission | CommandClass::Critical))
        {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "remote control at this computer allows no commands that need control",
            ));
        }
        // With nobody at the computer, the vehicle's own failsafe is what
        // happens if this person disconnects (S81): it must be on, listen to
        // UXDuck, and be the only vehicle its heartbeat would stop for.
        let plan = self.failsafe_plan(&vehicle);
        if let Some(refused) = self.unattended_refusal(&vehicle, &plan) {
            return Err(refused);
        }
        match held {
            Some(lease) if lease.user_id == user_id => return to_value(&lease),
            Some(lease) => {
                return Err(ApiError::new(
                    ErrorKind::Busy,
                    format!("{} has control of {}", lease.name, vehicle.name),
                ));
            }
            None => {}
        }
        let lease = Lease {
            vehicle_id: vehicle.id.clone(),
            user_id,
            name: name.clone(),
            since_us: self.clock.now_us(),
            failsafe: Some(plan.sentence.clone()),
        };
        self.remote.leases.insert(
            vehicle.id.clone(),
            Held {
                lease: lease.clone(),
                connection,
            },
        );
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!("{name} has control of {} from away", vehicle.name),
            body: Some(format!(
                "Commands they send go to the vehicle as yours would. If they disconnect while nobody is here, {}",
                plan.sentence.replacen(
                    "If you lose connection and nobody is at the ground station, ",
                    "",
                    1
                )
            )),
            actions: vec![NotificationAction {
                label: "Take back".into(),
                action: Action::TakeBack {
                    vehicle_id: vehicle.id.clone(),
                },
            }],
            condition: Some(lease_condition(&vehicle.id)),
            report: None,
        });
        to_value(&lease)
    }

    /// `lease/release`: its holder gives it back, or someone at the
    /// computer takes it back.
    fn give_back(
        &mut self,
        vehicle: &uxduck_core::domain::vehicle::Vehicle,
        sender: Option<&(String, String)>,
        held: Option<Lease>,
        operator: bool,
    ) -> Result<Value, ApiError> {
        let Some(lease) = held else {
            return Ok(serde_json::json!({}));
        };
        match sender {
            Some((user_id, _)) if *user_id != lease.user_id => {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    format!("{} has control of {}", lease.name, vehicle.name),
                ));
            }
            None if !operator => {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "only an operator at the computer can take control back",
                ));
            }
            _ => {}
        }
        self.end_lease(&vehicle.id, &vehicle.name, &lease, sender.is_none());
        Ok(serde_json::json!({}))
    }

    /// Why a vehicle may not be left to someone away with nobody at the
    /// computer (S81): its own failsafe is what happens if they disconnect,
    /// so it must be on, listen to UXDuck, and be the only vehicle its
    /// heartbeat would stop for.
    fn unattended_refusal(
        &self,
        vehicle: &uxduck_core::domain::vehicle::Vehicle,
        plan: &uxduck_core::domain::failsafe::FailsafePlan,
    ) -> Option<ApiError> {
        if self.someone_here() {
            return None;
        }
        if let Some(problem) = &plan.problem {
            return Some(ApiError::new(
                ErrorKind::CapabilityDenied,
                format!(
                    "Nobody is at the computer {} is connected to, and {problem}. Fix that at the computer, or keep someone there.",
                    vehicle.name
                ),
            ));
        }
        let shared = self
            .state
            .links
            .get(&vehicle.link_id)
            .is_some_and(|link| matches!(link.kind, LinkKind::Serial { .. }))
            && self
                .state
                .vehicles
                .values()
                .any(|v| v.id != vehicle.id && v.link_id == vehicle.link_id && !v.dismissed);
        shared.then(|| {
            ApiError::new(
                ErrorKind::CapabilityDenied,
                format!(
                    "{} shares a radio with other vehicles, so UXDuck can't set off its failsafe without setting off theirs. Keep someone at the computer.",
                    vehicle.name
                ),
            )
        })
    }

    /// Ends a lease and says so at the computer.
    fn end_lease(&mut self, vehicle_id: &str, vehicle_name: &str, lease: &Lease, taken_back: bool) {
        self.remote.leases.remove(vehicle_id);
        self.resolve(&lease_condition(vehicle_id));
        let title = if taken_back {
            format!(
                "You took control of {vehicle_name} back from {}",
                lease.name
            )
        } else {
            format!("{} gave back control of {vehicle_name}", lease.name)
        };
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: vehicle_id.to_owned(),
            },
            title,
            body: None,
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }

    /// A connection from away closed: its leases end, and the computer is told.
    pub(crate) fn release_leases_of(&mut self, connection: ConnectionId) {
        let ended: Vec<Held> = self
            .remote
            .leases
            .values()
            .filter(|held| held.connection == connection)
            .cloned()
            .collect();
        for held in ended {
            let vehicle_id = held.lease.vehicle_id.clone();
            let name = self
                .vehicle(&vehicle_id)
                .map_or_else(|| vehicle_id.clone(), |v| v.name.clone());
            self.remote.leases.remove(&vehicle_id);
            self.resolve(&lease_condition(&vehicle_id));
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle {
                    id: vehicle_id.clone(),
                },
                title: format!(
                    "{} is no longer connected: control of {name} is back here",
                    held.lease.name
                ),
                body: None,
                actions: Vec::new(),
                condition: None,
                report: None,
            });
        }
    }
}

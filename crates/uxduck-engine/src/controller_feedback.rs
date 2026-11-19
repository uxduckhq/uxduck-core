//! A controller's battery and rumble (S419, ADR-0005 A7).
//!
//! The platform says how much charge a controller has, and the engine keeps
//! it on the controller for the app. A controller flying a vehicle whose
//! battery reaches a level, low and then critical, raises a notice once per
//! level with what the platform reported. The levels are `UPower`'s
//! defaults, which Linux desktops warn a peripheral's battery at, so a
//! controller is called low where its own platform would call it low.
//!
//! Rumble tells the pilot without looking. It is decided here from what the
//! engine already does and says, and changes none of it:
//!
//! - a hold completed and its command went: one short pulse;
//! - a command from the controller held back here, never sent, or refused
//!   by the vehicle: two short pulses;
//! - the route paused, or the vehicle it flies no longer heard: one long
//!   pulse.
//!
//! Only a connected, claimed controller rumbles, and only while Settings ›
//! Controllers › Rumble is on.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::command::ReceiptStatus;
use uxduck_core::domain::controller::{ControllerPower, PowerLevel, Rumble};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::{Actor, Change};

use crate::Engine;
use crate::commands::Raise;

/// What the engine remembers to rumble and warn once.
#[derive(Debug, Default)]
pub(crate) struct Feedback {
    /// The level each controller was last warned of on its battery.
    warned: BTreeMap<String, PowerLevel>,
    /// A controller's commands on their way: receipt id to controller id.
    receipts: BTreeMap<String, String>,
    /// Controllers whose route is paused, so a pause is felt once.
    paused: BTreeSet<String>,
    /// Vehicles not heard, so their loss is felt once.
    offline: BTreeSet<String>,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
        identity: None,
    }
}

fn condition(id: &str, level: PowerLevel) -> String {
    match level {
        PowerLevel::Low => format!("controller-battery-low:{id}"),
        PowerLevel::Critical => format!("controller-battery-critical:{id}"),
    }
}

impl Engine {
    /// A controller's power as its platform reports it now.
    pub fn controller_power(&mut self, id: &str, power: ControllerPower) {
        let Some(mut controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        if controller.power == power {
            return;
        }
        controller.power = power;
        controller.version += 1;
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
    }

    /// Warns of a flying controller's battery once per level, and clears
    /// the warnings once it is charging or above them again.
    fn review_controller_power(&mut self, id: &str) {
        let Some(controller) = self.state.controllers.get(id) else {
            return;
        };
        let level = controller.power.level();
        let warned = self.controller_feedback.warned.get(id).copied();
        if let Some(was) = warned
            && level.is_none_or(|now| now < was)
        {
            for old in [PowerLevel::Low, PowerLevel::Critical] {
                if level.is_none_or(|now| old > now) {
                    self.resolve(&condition(id, old));
                }
            }
            match level {
                Some(now) => self.controller_feedback.warned.insert(id.to_owned(), now),
                None => self.controller_feedback.warned.remove(id),
            };
            return;
        }
        let Some(level) = level.filter(|now| warned != Some(*now)) else {
            return;
        };
        let ControllerPower::Discharging { percent } = controller.power else {
            return;
        };
        let flying = controller.connected && controller.claim.is_some();
        let Some(target) = controller.target.clone().filter(|_| flying) else {
            return;
        };
        let name = controller.name.clone();
        let vehicle = self
            .vehicle(&target)
            .map_or_else(|| target.clone(), |v| v.name.clone());
        self.controller_feedback.warned.insert(id.to_owned(), level);
        if level == PowerLevel::Critical {
            self.resolve(&condition(id, PowerLevel::Low));
        }
        let (severity, said) = match level {
            PowerLevel::Low => (Severity::Warning, "low"),
            PowerLevel::Critical => (Severity::Alarm, "critical"),
        };
        self.raise(Raise {
            severity,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle { id: target },
            title: format!("{name}'s battery is {said}"),
            body: Some(format!(
                "{name} reports {percent}% left while it flies {vehicle}. Charge it or plug it in before it runs out and stops flying {vehicle}."
            )),
            actions: Vec::new(),
            condition: Some(condition(id, level)),
            report: None,
        });
    }

    /// Rumbles a connected, claimed controller, while rumble is on.
    pub(crate) fn controller_rumble(&mut self, id: &str, rumble: Rumble) {
        if !self.state.settings.controller_rumble {
            return;
        }
        if !self
            .state
            .controllers
            .get(id)
            .is_some_and(|c| c.connected && c.claim.is_some())
        {
            return;
        }
        self.links.rumble(id, rumble);
    }

    /// A controller's command went with this receipt: it rumbles if the
    /// vehicle refuses it.
    pub(crate) fn controller_command_sent(&mut self, id: &str, receipt_id: &str) {
        self.controller_feedback
            .receipts
            .insert(receipt_id.to_owned(), id.to_owned());
        let status = self.state.receipts.get(receipt_id).map(|r| r.status);
        if let Some(status) = status.filter(|s| *s != ReceiptStatus::Sent) {
            self.controller_command_ended(receipt_id, status);
        }
    }

    fn controller_command_ended(&mut self, receipt_id: &str, status: ReceiptStatus) {
        let Some(id) = self.controller_feedback.receipts.remove(receipt_id) else {
            return;
        };
        if matches!(status, ReceiptStatus::Rejected | ReceiptStatus::NotSent) {
            self.controller_rumble(&id, Rumble::Refused);
        }
    }

    /// After each change: a controller's command answered, its route
    /// paused, the vehicle it flies lost, and its battery seen again as
    /// it changes or starts flying.
    pub(crate) fn controller_feedback_after(&mut self, change: &Change) {
        match change {
            Change::ReceiptUpserted { receipt } if receipt.status != ReceiptStatus::Sent => {
                if self.controller_feedback.receipts.contains_key(&receipt.id) {
                    self.controller_command_ended(&receipt.id, receipt.status);
                }
            }
            Change::ControllerUpserted { controller } => {
                let id = controller.id.clone();
                if controller.input_pause.is_some() && controller.claim.is_some() {
                    // A pause for a vehicle already lost was felt as the loss.
                    let lost = controller
                        .target
                        .as_ref()
                        .is_some_and(|v| self.controller_feedback.offline.contains(v));
                    if self.controller_feedback.paused.insert(id.clone()) && !lost {
                        self.controller_rumble(&id, Rumble::Lost);
                    }
                } else {
                    self.controller_feedback.paused.remove(&id);
                }
                if !controller.connected {
                    self.controller_feedback
                        .receipts
                        .retain(|_, sender| *sender != id);
                }
                if controller.power.level().is_some()
                    || self.controller_feedback.warned.contains_key(&id)
                {
                    self.review_controller_power(&id);
                }
            }
            Change::ControllerRemoved { id } => {
                self.controller_feedback.paused.remove(id);
                self.controller_feedback.warned.remove(id);
                self.controller_feedback
                    .receipts
                    .retain(|_, sender| sender != id);
            }
            Change::VehicleUpserted { vehicle } if vehicle.presence == Presence::Offline => {
                if !self.controller_feedback.offline.insert(vehicle.id.clone()) {
                    return;
                }
                let flying: Vec<String> = self
                    .state
                    .controllers
                    .values()
                    .filter(|c| {
                        c.claim.is_some()
                            && c.input_pause.is_none()
                            && c.target.as_deref() == Some(&vehicle.id)
                    })
                    .map(|c| c.id.clone())
                    .collect();
                for id in flying {
                    self.controller_rumble(&id, Rumble::Lost);
                }
            }
            Change::VehicleUpserted { vehicle } => {
                self.controller_feedback.offline.remove(&vehicle.id);
            }
            Change::VehicleRemoved { id } => {
                self.controller_feedback.offline.remove(id);
            }
            _ => {}
        }
    }
}

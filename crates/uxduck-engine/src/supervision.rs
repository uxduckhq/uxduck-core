//! Lost supervision hands over to the vehicle (S81, ADR-0003 C16). UXDuck
//! sends a 1 Hz ground station heartbeat on every writable vehicle link, as
//! Mission Planner does. When nobody supervises a vehicle any more, at the
//! computer or from away, it waits the grace period and then stops that
//! heartbeat, so the vehicle's own lost ground station failsafe acts, as if
//! the computer had crashed. UXDuck invents no recovery of its own. When
//! someone supervises it again, the heartbeat resumes; the vehicle stays in
//! whatever mode its failsafe chose.
//!
//! A vehicle is supervised while an operator is connected at the computer, a
//! writable forward link is up (another ground station may be sending its
//! heartbeats through it), or someone away holds its control lease.

use std::collections::BTreeMap;

use uxduck_core::domain::failsafe::{FailsafePlan, describe};
use uxduck_core::domain::ground_station::GroundStationIds;
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::link::{LinkPolicy, LinkStatus};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::{TransferStatus, parameter_id};
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;
use crate::commands::Raise;
use crate::transfers::same_boot;

/// Each vehicle's supervision, as `supervise` last saw it.
#[derive(Debug, Default)]
pub(crate) struct Supervision {
    /// Since when nobody has supervised each vehicle, by id.
    unsupervised_since: BTreeMap<String, u64>,
    /// Vehicles whose heartbeat is withheld now.
    withheld: std::collections::BTreeSet<String>,
    /// Each vehicle's warning that it drops UXDuck's manual control, as
    /// last said ("title\nbody"), by condition, kept while it holds so a
    /// dismissed one is said again once it clears or changes.
    ground_station_said: BTreeMap<String, String>,
}

fn condition(vehicle_id: &str) -> String {
    format!("supervision:{vehicle_id}")
}

/// A vehicle dropping the manual control UXDuck sends it (FB-259).
const IGNORED: &str = "ground-station-id:";

impl Engine {
    /// What this vehicle's lost ground station failsafe would do, in one
    /// sentence, from the parameters UXDuck last downloaded.
    #[must_use]
    pub fn failsafe_plan(&self, vehicle: &Vehicle) -> FailsafePlan {
        describe(
            Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type),
            &vehicle.name,
            self.state.settings.supervision_grace_s,
            self.state.settings.gcs_system_id,
            |name| {
                self.state
                    .parameters
                    .get(&parameter_id(&vehicle.id, name))
                    .map(|p| p.value)
            },
        )
    }

    /// The ground station ids an `ArduPilot` vehicle's parameters say it
    /// listens to, from the download of its current boot, judged the way the
    /// parameter cache is (`same_boot`). None for any other autopilot,
    /// before that download, or when they can't say.
    #[must_use]
    pub fn ground_station_ids(&self, vehicle: &Vehicle) -> Option<GroundStationIds> {
        if vehicle.autopilot != "Ardupilotmega" {
            return None;
        }
        if !self.params_current(&vehicle.id) {
            return None;
        }
        GroundStationIds::read(|name| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        })
    }

    /// Whether UXDuck holds the parameters a vehicle has in its current
    /// boot: a finished download from that boot, judged the way the
    /// parameter cache is (`same_boot`).
    fn params_current(&self, vehicle_id: &str) -> bool {
        let set = self.parameter_set(vehicle_id);
        set.status == TransferStatus::Synced
            && self
                .transfers
                .boots
                .get(vehicle_id)
                .is_some_and(|boot| same_boot(set.boot_instant_us, *boot))
    }

    /// Once a second (FB-259): an `ArduPilot` vehicle drops every
    /// `RC_CHANNELS_OVERRIDE` and `MANUAL_CONTROL` from a system id outside
    /// its ground station ids, and says nothing back. While a controller or
    /// the keyboard's input is routed to one that won't take UXDuck's Ground
    /// station ID, say so, with its parameter, its value and UXDuck's id.
    /// It clears once nothing is routed there (released, or paused while a
    /// simulation restarts) or the ids match, and a dismissal stays quiet only
    /// while the same mismatch holds. What UXDuck sends, and how, stays as it
    /// is, and no parameter is written.
    pub fn review_ground_station_ids(&mut self) {
        let ours = self.state.settings.gcs_system_id;
        let mut ignored: BTreeMap<String, (String, String, String)> = BTreeMap::new();
        for vehicle in self.state.vehicles.values() {
            if !self
                .driving(vehicle)
                .iter()
                .any(|controller| self.routes_pilot_input(controller))
            {
                continue;
            }
            let Some(ids) = self.ground_station_ids(vehicle) else {
                continue;
            };
            if ids.accepts(ours) {
                continue;
            }
            let fix = match ids.would_take() {
                Some(id) => format!(
                    "Change UXDuck's Ground station ID to {id}, or set {} on the vehicle.",
                    ids.setting_for(ours)
                ),
                None => format!("Set {} on the vehicle.", ids.setting_for(ours)),
            };
            ignored.insert(
                format!("{IGNORED}{}", vehicle.id),
                (
                    vehicle.id.clone(),
                    format!("{} ignores the sticks and keys UXDuck sends", vehicle.name),
                    format!(
                        "Its {}, and UXDuck's Ground station ID is {ours}. ArduPilot takes manual control only from its ground station, so it drops what UXDuck sends. {fix}",
                        ids.described()
                    ),
                ),
            );
        }
        // What was said, shown or dismissed: a notice still up, and what this
        // review said last, which a dismissed notice no longer shows.
        let mut said = std::mem::take(&mut self.supervision.ground_station_said);
        for notice in self.state.notifications.values() {
            if let Some(condition) = notice.condition.as_ref().filter(|c| c.starts_with(IGNORED)) {
                said.entry(condition.clone()).or_insert_with(|| {
                    format!(
                        "{}\n{}",
                        notice.title,
                        notice.body.as_deref().unwrap_or_default()
                    )
                });
            }
        }
        for (condition, text) in said {
            let same = ignored
                .get(&condition)
                .is_some_and(|(_, title, body)| format!("{title}\n{body}") == text);
            if !same {
                self.resolve(&condition);
            }
        }
        for (condition, (id, title, body)) in ignored {
            self.supervision
                .ground_station_said
                .insert(condition.clone(), format!("{title}\n{body}"));
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Persistent,
                subject: Subject::Vehicle { id },
                title,
                body: Some(body),
                actions: Vec::new(),
                condition: Some(condition),
                report: None,
            });
        }
    }

    /// A controller's input stopped going to a vehicle (released, paused,
    /// lost). Once nothing else is routed there, its warning ends, dismissal
    /// and all, so the next time it is flown it is said again, even when that
    /// is before the next review.
    pub(crate) fn pilot_route_ended(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        if self
            .driving(&vehicle)
            .iter()
            .any(|controller| self.routes_pilot_input(controller))
        {
            return;
        }
        let condition = format!("{IGNORED}{}", vehicle.id);
        self.supervision.ground_station_said.remove(&condition);
        self.resolve(&condition);
    }

    /// Someone at the computer: an operator client.
    fn someone_here(&self) -> bool {
        self.connections
            .values()
            .any(|c| c.admitted.as_ref().is_some_and(|a| a.operator))
    }

    /// Whether someone is there to supervise a vehicle: an operator at the
    /// computer, or a writable forward link up.
    fn supervised(&self) -> bool {
        self.someone_here()
            || self.state.links.values().any(|link| {
                link.kind.is_forward()
                    && link.policy == LinkPolicy::ReadWrite
                    && link.status == LinkStatus::Up
            })
    }

    /// Once a second: stops the heartbeat to vehicles nobody has supervised
    /// for the grace period, and resumes it for those supervised again.
    pub fn supervise(&mut self, now_us: u64) {
        let grace_us = u64::from(self.state.settings.supervision_grace_s) * 1_000_000;
        let vehicles: Vec<Vehicle> = self
            .state
            .vehicles
            .values()
            .filter(|v| !v.dismissed)
            .cloned()
            .collect();
        for vehicle in &vehicles {
            let id = vehicle.id.clone();
            if self.supervised() {
                self.supervision.unsupervised_since.remove(&id);
                if self.supervision.withheld.remove(&id) {
                    self.resume_heartbeat(vehicle);
                }
                continue;
            }
            let since = *self
                .supervision
                .unsupervised_since
                .entry(id.clone())
                .or_insert(now_us);
            if !self.supervision.withheld.contains(&id) && now_us.saturating_sub(since) >= grace_us
            {
                self.supervision.withheld.insert(id.clone());
                self.withhold_heartbeat(vehicle);
            }
        }
        self.forget_gone(&vehicles);
    }

    /// A vehicle that is gone (its simulation stopped, its record merged
    /// into another, or dismissed) is nobody's to supervise: its alert
    /// resolves rather than staying at the foot of the sidebar for good
    /// (QA 2026-10-24).
    fn forget_gone(&mut self, vehicles: &[Vehicle]) {
        let here = |id: &str| vehicles.iter().any(|v| v.id == id);
        self.supervision.unsupervised_since.retain(|id, _| here(id));
        self.supervision.withheld.retain(|id| here(id));
        let gone: Vec<String> = self
            .state
            .notifications
            .values()
            .filter_map(|n| n.condition.as_deref()?.strip_prefix("supervision:"))
            .filter(|id| !here(id))
            .map(condition)
            .collect();
        for gone in gone {
            self.resolve(&gone);
        }
    }

    /// Whether UXDuck withholds its heartbeat to a vehicle now.
    #[must_use]
    pub fn heartbeat_withheld(&self, vehicle_id: &str) -> bool {
        self.supervision.withheld.contains(vehicle_id)
    }

    fn withhold_heartbeat(&mut self, vehicle: &Vehicle) {
        self.links.ground_heartbeat(&vehicle.id, false);
        // On the ground and disarmed nothing happens, so nothing is said.
        if !vehicle.armed {
            return;
        }
        let plan = self.failsafe_plan(vehicle);
        // Only an ArduPilot vehicle that takes UXDuck as its ground station
        // notices the heartbeat stop. One on another id takes no notice, and
        // one whose ids are not read yet may or may not. Without this boot's
        // parameters, its failsafe settings are unknown too.
        let ours = self.state.settings.gcs_system_id;
        let ids = self.ground_station_ids(vehicle);
        let (body, failsafe) = match ids {
            Some(ids) if !ids.accepts(ours) => {
                let problem = format!(
                    "{} follows a different ground station, so it won't notice UXDuck stopping. Its {}, and UXDuck's Ground station ID is {ours}. Set {} on the vehicle for it to notice.",
                    vehicle.name,
                    ids.described(),
                    ids.setting_for(ours)
                );
                (problem.clone(), problem)
            }
            None if vehicle.autopilot == "Ardupilotmega" && !self.params_current(&vehicle.id) => {
                let unknown = format!(
                    "UXDuck hasn't read {}'s parameters for this boot, so it can't tell whether the vehicle will notice or what it would do. If it accepts UXDuck's ID {ours}, it will treat this as a lost ground station.",
                    vehicle.name
                );
                (unknown.clone(), unknown)
            }
            None if vehicle.autopilot == "Ardupilotmega" => {
                let unsure = format!(
                    "UXDuck can't read which ground station {} follows from its parameters, so it can't tell whether the vehicle will notice. If it accepts UXDuck's ID {ours}, it will treat this as a lost ground station. {}",
                    vehicle.name, plan.sentence
                );
                (unsure.clone(), unsure)
            }
            _ => (
                format!(
                    "The vehicle will treat this as a lost ground station. {}",
                    plan.sentence
                ),
                plan.sentence,
            ),
        };
        self.raise(Raise {
            severity: Severity::Alarm,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!(
                "UXDuck stopped its ground station heartbeat to {} because nobody is supervising it",
                vehicle.name
            ),
            body: Some(body),
            actions: Vec::new(),
            condition: Some(condition(&vehicle.id)),
            report: None,
        });
        self.record(&vehicle.id, HistoryEvent::SupervisionLost { failsafe });
    }

    fn resume_heartbeat(&mut self, vehicle: &Vehicle) {
        self.links.ground_heartbeat(&vehicle.id, true);
        let told = self
            .state
            .notifications
            .values()
            .any(|n| n.condition.as_deref() == Some(condition(&vehicle.id).as_str()));
        self.resolve(&condition(&vehicle.id));
        if !told {
            return;
        }
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!(
                "UXDuck's ground station heartbeat to {} resumed. It stays in the mode its failsafe chose",
                vehicle.name
            ),
            body: None,
            actions: Vec::new(),
            condition: None,
            report: None,
        });
        self.record(&vehicle.id, HistoryEvent::SupervisionResumed);
    }
}

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
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::link::{LinkPolicy, LinkStatus};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;
use crate::commands::Raise;

/// Each vehicle's supervision, as `supervise` last saw it.
#[derive(Debug, Default)]
pub(crate) struct Supervision {
    /// Since when nobody has supervised each vehicle, by id.
    unsupervised_since: BTreeMap<String, u64>,
    /// Vehicles whose heartbeat is withheld now.
    withheld: std::collections::BTreeSet<String>,
}

fn condition(vehicle_id: &str) -> String {
    format!("supervision:{vehicle_id}")
}

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

    /// Whether someone is there to supervise a vehicle: an operator at the
    /// computer, a writable forward link up, or its lease holder from away.
    fn supervised(&self, vehicle_id: &str) -> bool {
        self.someone_here()
            || self.lease_of(vehicle_id).is_some()
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
        for vehicle in vehicles {
            let id = vehicle.id.clone();
            if self.supervised(&id) {
                self.supervision.unsupervised_since.remove(&id);
                if self.supervision.withheld.remove(&id) {
                    self.resume_heartbeat(&vehicle);
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
                self.withhold_heartbeat(&vehicle);
            }
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
            body: Some(format!(
                "The vehicle will treat this as a lost ground station. {}",
                plan.sentence
            )),
            actions: Vec::new(),
            condition: Some(condition(&vehicle.id)),
            report: None,
        });
        self.record(
            &vehicle.id,
            HistoryEvent::SupervisionLost {
                failsafe: plan.sentence,
            },
        );
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

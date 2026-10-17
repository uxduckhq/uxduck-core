//! A simulated vehicle back on the ground after flying (maintainer,
//! 2026-10-07). A beginner who crashes one had no way back but stopping it
//! and starting another from the form, and nothing said it had crashed: a
//! simulated vehicle's own messages notify no one (`messages`). So UXDuck
//! watches each simulated vehicle itself. Once it has flown (armed, above
//! [`FLOWN_M`] above home), a disarm raises one notice with "Start again",
//! which starts the simulation again from where it started (`sim/restart`).
//! A crash message from the autopilot ("Crash: Disarming" from `ArduCopter`,
//! "Crash detected" from `ArduPlane`, which may well stay armed) raises the
//! same notice at once and names the crash. The notice clears when the
//! vehicle arms again, or its simulation starts again or stops. Real
//! vehicles get none of this: a landing is theirs to judge, and nothing
//! starts them again.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;
use crate::commands::Raise;

/// Above this height above home, armed, a vehicle has flown.
const FLOWN_M: f64 = 5.0;

/// A crash reported this long before the disarm is the disarm's reason.
const CRASH_US: u64 = 30_000_000;

/// Which simulated vehicles have flown since their simulator started, and
/// which reported a crash, by vehicle id.
#[derive(Debug, Default)]
pub(crate) struct Flights {
    flown: BTreeSet<String>,
    crashed_us: BTreeMap<String, u64>,
}

/// Whether an autopilot's message reports a crash: `ArduCopter`'s "Crash:
/// Disarming", `ArduPlane`'s "Crash detected" (with or without an action).
pub(crate) fn is_crash(text: &str) -> bool {
    text.starts_with("Crash")
}

fn condition(vehicle_id: &str) -> String {
    format!("grounded:{vehicle_id}")
}

impl Engine {
    /// Telemetry arrived: note a simulated vehicle flying.
    pub(crate) fn review_flight(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        if !vehicle.armed || !self.is_simulated(vehicle) {
            return;
        }
        let id = vehicle.id.clone();
        if self.height_above_home(&id).is_some_and(|h| h >= FLOWN_M) {
            self.flights.flown.insert(id);
        }
    }

    /// A simulated vehicle said it crashed: say so, with the way back.
    pub(crate) fn crash_reported(&mut self, vehicle: &Vehicle) {
        let now = self.clock.now_us();
        self.flights.crashed_us.insert(vehicle.id.clone(), now);
        self.on_the_ground(vehicle, true);
    }

    /// A vehicle disarmed: a simulated one that had flown is on the ground,
    /// crashed if it said so just before.
    pub(crate) fn vehicle_disarmed(&mut self, vehicle: &Vehicle) {
        if !self.flights.flown.remove(&vehicle.id) {
            return;
        }
        let now = self.clock.now_us();
        let crashed = self
            .flights
            .crashed_us
            .get(&vehicle.id)
            .is_some_and(|at| now.saturating_sub(*at) < CRASH_US);
        self.on_the_ground(vehicle, crashed);
    }

    /// A vehicle armed: whatever put it on the ground is over.
    pub(crate) fn vehicle_armed(&mut self, vehicle: &Vehicle) {
        self.flights.crashed_us.remove(&vehicle.id);
        self.resolve(&condition(&vehicle.id));
    }

    /// A simulation starts again or stops: its vehicles start afresh.
    pub(crate) fn forget_flights_on(&mut self, link_id: &str) {
        let on_link: Vec<String> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.link_id == link_id)
            .map(|v| v.id.clone())
            .collect();
        for id in on_link {
            self.flights.flown.remove(&id);
            self.flights.crashed_us.remove(&id);
            self.resolve(&condition(&id));
        }
    }

    fn on_the_ground(&mut self, vehicle: &Vehicle, crashed: bool) {
        if !self.is_simulated(vehicle) || self.is_launching(&vehicle.link_id) {
            return;
        }
        let (title, body) = if crashed {
            (
                format!("{} crashed", vehicle.name),
                "It's on the ground where it came down. Start again puts it back where it started.",
            )
        } else {
            (
                format!("{} is on the ground", vehicle.name),
                "It's disarmed. Start again puts it back where it started.",
            )
        };
        self.raise_or_update(Raise {
            severity: if crashed {
                Severity::Warning
            } else {
                Severity::Info
            },
            kind: if crashed {
                NotificationKind::Persistent
            } else {
                NotificationKind::Transient
            },
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title,
            body: Some(body.into()),
            actions: vec![NotificationAction {
                label: "Start again".into(),
                action: Action::RestartSimulation {
                    link_id: vehicle.link_id.clone(),
                },
            }],
            condition: Some(condition(&vehicle.id)),
            report: None,
        });
    }
}

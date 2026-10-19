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

/// Hitting the ground at least this fast, in metres a second, is a crash.
/// A copter lands at 0.5 (`LAND_SPD_MS`), and one let down without throttle
/// at 2.5 at most (`PILOT_SPD_UP`, which `PILOT_SPD_DN` takes by default).
const HARD_HIT_MPS: f64 = 3.0;

/// Whether an autopilot's message reports a crash: `ArduCopter`'s "Crash:
/// Disarming", `ArduPlane`'s "Crash detected" (with or without an action),
/// or the simulator's "SIM Hit ground at 3.983069 m/s" (maintainer,
/// 2026-10-17) when that fast. The simulator says it at every touchdown,
/// gentle ones too, with the speed straight down.
pub(crate) fn is_crash(text: &str) -> bool {
    if text.starts_with("Crash") {
        return true;
    }
    text.strip_prefix("SIM Hit ground at ")
        .and_then(|rest| rest.strip_suffix(" m/s"))
        .and_then(|speed| speed.trim().parse::<f64>().ok())
        .is_some_and(|speed| speed.abs() >= HARD_HIT_MPS)
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
            // The web tells a crash by this warning on `grounded:` and
            // shows it over the maps (CrashFlash.tsx).
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

#[cfg(test)]
mod tests {
    use super::is_crash;

    #[test]
    fn crash_messages() {
        assert!(is_crash("Crash: Disarming"));
        assert!(is_crash("Crash detected"));
        assert!(is_crash("SIM Hit ground at 3.983069 m/s"));
        assert!(is_crash("SIM Hit ground at 12.500000 m/s"));
        // A landing, or a let-down without throttle, is no crash.
        assert!(!is_crash("SIM Hit ground at 0.512345 m/s"));
        assert!(!is_crash("SIM Hit ground at 2.499000 m/s"));
        assert!(!is_crash("SIM Hit ground at fast m/s"));
        assert!(!is_crash("Land complete"));
    }
}

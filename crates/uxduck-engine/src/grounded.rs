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
//! starts them again. A rover, boat or submarine never flies, however high
//! above home it goes, so only a crash raises the notice for one.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::link::{SCENERY_CLEAR_M, SceneryHit};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::telemetry_layout::VehicleClass;
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Change;

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

fn scenery_condition(link_id: &str) -> String {
    format!("scenery-hit:{link_id}")
}

impl Engine {
    /// Telemetry arrived: note a simulated vehicle flying.
    pub(crate) fn review_flight(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        if !vehicle.armed
            || !self.is_simulated(vehicle)
            || VehicleClass::of(&vehicle.vehicle_type).never_flies()
        {
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

    /// Same crash UI as an autopilot report, with the actual scenery cause
    /// and where, for a simulation the scenery stopped (PX4's).
    pub(crate) fn scenery_crash_reported(&mut self, vehicle: &Vehicle, at: Option<SceneryHit>) {
        self.flights
            .crashed_us
            .insert(vehicle.id.clone(), self.clock.now_us());
        let deg = |e7: i32| f64::from(e7) / 1e7;
        let place = at.map_or_else(String::new, |at| {
            format!(
                " at {:.5}, {:.5}",
                deg(at.latitude_e7),
                deg(at.longitude_e7)
            )
        });
        let body = format!(
            "It hit the 3D scenery{place} and the simulation has stopped. Start again puts it \
             back where it started."
        );
        self.grounded_notice(vehicle, true, Some(&body));
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
        self.resolve(&scenery_condition(link_id));
    }

    /// Where a vehicle is over the ground now, by the autopilot's estimate.
    pub(crate) fn ground_position(&self, vehicle_id: &str) -> Option<SceneryHit> {
        let at = self
            .telemetry(vehicle_id)?
            .positions
            .iter()
            .find(|p| p.source == "fused")?;
        #[allow(clippy::cast_possible_truncation, reason = "±180° × 1e7 fits an i32")]
        let e7 = |deg: f64| (deg * 1e7).round() as i32;
        Some(SceneryHit {
            latitude_e7: e7(at.latitude_deg),
            longitude_e7: e7(at.longitude_deg),
        })
    }

    /// How far a vehicle is from a spot across the ground, in metres; none
    /// without a position.
    pub(crate) fn metres_from(&self, vehicle_id: &str, spot: SceneryHit) -> Option<f64> {
        let at = self.ground_position(vehicle_id)?;
        let deg = |e7: i32| f64::from(e7) / 1e7;
        let mid = deg(at.latitude_e7.midpoint(spot.latitude_e7)).to_radians();
        let north = (deg(at.latitude_e7) - deg(spot.latitude_e7)).to_radians();
        let east = (deg(at.longitude_e7) - deg(spot.longitude_e7)).to_radians() * mid.cos();
        Some(6_371_000.0 * north.hypot(east))
    }

    /// A simulated vehicle paused where it touched the 3D scenery (`sim/crash`):
    /// say where, with Continue anyway, Start again and "Not a real crash",
    /// which reports the spot so wrong scenery can be found.
    pub(crate) fn scenery_hit_reported(&mut self, vehicle: &Vehicle, at: SceneryHit) {
        let deg = |e7: i32| f64::from(e7) / 1e7;
        self.raise_or_update(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!("{} touched the 3D scenery", vehicle.name),
            body: Some(format!(
                "It's paused where it touched, at {:.5}, {:.5}. If the scenery is wrong \
                 there, continue anyway: contact nearby is ignored until it's clear. Or \
                 start again from where it started.",
                deg(at.latitude_e7),
                deg(at.longitude_e7),
            )),
            actions: vec![
                NotificationAction {
                    label: "Continue anyway".into(),
                    action: Action::ResumeSimulation {
                        link_id: vehicle.link_id.clone(),
                    },
                },
                NotificationAction {
                    label: "Start again".into(),
                    action: Action::RestartSimulation {
                        link_id: vehicle.link_id.clone(),
                    },
                },
            ],
            condition: Some(scenery_condition(&vehicle.link_id)),
            report: Some("scenery-hit".into()),
        });
    }

    /// A simulation paused by the scenery resumed: its notice is answered.
    pub(crate) fn scenery_hit_answered(&mut self, link_id: &str) {
        self.resolve(&scenery_condition(link_id));
    }

    /// Telemetry arrived: a vehicle that carried on past the scenery and is
    /// now clear of the spot counts contact again.
    pub(crate) fn review_scenery_hit(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let Some(link) = self.state.links.get(&vehicle.link_id) else {
            return;
        };
        let Some(hit) = link.scenery_hit else {
            return;
        };
        if link.paused
            || self
                .metres_from(vehicle_id, hit)
                .is_none_or(|m| m < SCENERY_CLEAR_M)
        {
            return;
        }
        let mut link = link.clone();
        link.scenery_hit = None;
        link.version += 1;
        self.commit_as(
            uxduck_core::events::Actor {
                client_id: "data-plane".into(),
                operator_id: None,
                identity: None,
            },
            Change::LinkUpserted { link },
        );
    }

    fn on_the_ground(&mut self, vehicle: &Vehicle, crashed: bool) {
        self.grounded_notice(vehicle, crashed, None);
    }

    fn grounded_notice(&mut self, vehicle: &Vehicle, crashed: bool, body_override: Option<&str>) {
        if !self.is_simulated(vehicle) || self.is_launching(&vehicle.link_id) {
            return;
        }
        let (title, body) = if crashed && VehicleClass::of(&vehicle.vehicle_type).never_flies() {
            (
                format!("{} crashed", vehicle.name),
                "It stopped where it crashed. Start again puts it back where it started.",
            )
        } else if crashed {
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
            body: Some(body_override.unwrap_or(body).into()),
            actions: vec![NotificationAction {
                label: "Start again".into(),
                action: Action::RestartSimulation {
                    link_id: vehicle.link_id.clone(),
                },
            }],
            condition: Some(condition(&vehicle.id)),
            // Only the scenery's say-so (a PX4 simulation it stopped) may be wrong.
            report: body_override.map(|_| "scenery-hit".into()),
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

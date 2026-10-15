//! Flight time (S103, maintainer 2026-10-08): how long each vehicle has
//! been armed, and for one that flies, how long it has been in the air
//! since it armed. Worked out here, not in the page, so a reload or a
//! second window shows the same and the agent and plugins read it too.
//!
//! - `flight.armed_s`, "Time armed": from arming, as UXDuck first saw it
//!   armed. After a disarm it holds the flight's time until the next arm.
//! - `flight.air_s`, "Time in the air": planes, VTOLs and multicopters
//!   only, the time off the ground since arming, added up over hops. Off
//!   the ground is what the autopilot says (`flight.state`, from
//!   `EXTENDED_SYS_STATE`) when it says it; otherwise above
//!   [`AIR_ABOVE_M`] above home, and back down below [`GROUND_BELOW_M`].
//!
//! Sent with the vehicle's telemetry, at most once a second each, never
//! journaled; a daemon start forgets them.

use std::collections::BTreeMap;

use uxduck_core::domain::command::FLYING_ABOVE_HOME_M;
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry, off_the_ground};
use uxduck_core::domain::telemetry_layout::VehicleClass;

use crate::Engine;

/// Off the ground above this height above home, with no word from the autopilot.
const AIR_ABOVE_M: f64 = FLYING_ABOVE_HOME_M;
/// Back on the ground below this, so a hover at the line does not flicker.
const GROUND_BELOW_M: f64 = 1.0;

const SOURCE: &str = "flight";

/// Each vehicle's flight time, by vehicle id.
#[derive(Debug, Default)]
pub(crate) struct FlightTimes(BTreeMap<String, Timer>);

#[derive(Debug, Default)]
struct Timer {
    /// While armed: when.
    armed_at_us: Option<u64>,
    /// After a disarm: how long it was armed.
    armed_us: u64,
    /// While in the air: since when.
    air_since_us: Option<u64>,
    /// Time in the air banked before the current hop.
    air_us: u64,
    /// Whole seconds last sent, so a second is sent once.
    sent: Option<(u64, u64)>,
}

impl Timer {
    fn armed_us(&self, now_us: u64) -> u64 {
        self.armed_at_us
            .map_or(self.armed_us, |at| now_us.saturating_sub(at))
    }

    fn air_us(&self, now_us: u64) -> u64 {
        self.air_us + self.air_since_us.map_or(0, |at| now_us.saturating_sub(at))
    }
}

/// Whether vehicles of this `MAV_TYPE` fly.
fn flies(vehicle_type: &str) -> bool {
    vehicle_type == "Airship"
        || matches!(
            VehicleClass::of(vehicle_type),
            VehicleClass::Plane | VehicleClass::Copter
        )
}

#[allow(
    clippy::cast_precision_loss,
    reason = "microseconds of one flight fit an f64 exactly"
)]
fn seconds(us: u64) -> f64 {
    us as f64 / 1e6
}

impl Engine {
    /// A vehicle armed: its times start again.
    pub(crate) fn flight_armed(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        self.flight_times.0.insert(
            vehicle_id.to_owned(),
            Timer {
                armed_at_us: Some(now),
                ..Timer::default()
            },
        );
        self.review_flight_time(vehicle_id);
    }

    /// A vehicle disarmed: its times stop and hold.
    pub(crate) fn flight_disarmed(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        if let Some(timer) = self.flight_times.0.get_mut(vehicle_id) {
            if let Some(at) = timer.armed_at_us.take() {
                timer.armed_us = now.saturating_sub(at);
            }
            if let Some(at) = timer.air_since_us.take() {
                timer.air_us += now.saturating_sub(at);
            }
            timer.sent = None;
        }
        self.review_flight_time(vehicle_id);
    }

    /// Whether a vehicle is off the ground now: the autopilot's word, else
    /// its height above home, with `was` for the band between the two lines.
    pub(crate) fn off_the_ground(&self, vehicle_id: &str, was: bool) -> bool {
        let said = self
            .telemetry(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == "flight.state"))
            .and_then(|i| i.text.as_deref())
            .and_then(off_the_ground);
        if let Some(said) = said {
            return said;
        }
        match self.height_above_home(vehicle_id) {
            Some(h) if h > AIR_ABOVE_M => true,
            Some(h) if h < GROUND_BELOW_M => false,
            Some(_) => was,
            None => false,
        }
    }

    /// Telemetry arrived, or the vehicle armed or disarmed: start or stop
    /// its time in the air, and send its times when a second has passed.
    pub(crate) fn review_flight_time(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let (id, armed, aerial) = (
            vehicle.id.clone(),
            vehicle.armed,
            flies(&vehicle.vehicle_type),
        );
        let now = self.clock.now_us();
        // Armed when UXDuck first heard it: from then.
        if armed
            && self
                .flight_times
                .0
                .get(&id)
                .is_none_or(|t| t.armed_at_us.is_none())
        {
            self.flight_times.0.insert(
                id.clone(),
                Timer {
                    armed_at_us: Some(now),
                    ..Timer::default()
                },
            );
        }
        let flying = self
            .flight_times
            .0
            .get(&id)
            .is_some_and(|t| t.air_since_us.is_some());
        let up = armed && aerial && self.off_the_ground(&id, flying);
        let Some(timer) = self.flight_times.0.get_mut(&id) else {
            return;
        };
        match (up, timer.air_since_us) {
            (true, None) => timer.air_since_us = Some(now),
            (false, Some(at)) => {
                timer.air_us += now.saturating_sub(at);
                timer.air_since_us = None;
            }
            _ => {}
        }
        let (armed_us, air_us) = (timer.armed_us(now), timer.air_us(now));
        let whole = (armed_us / 1_000_000, air_us / 1_000_000);
        if timer.sent == Some(whole) {
            return;
        }
        // Noted before sending: sending reviews again, and must stop there.
        timer.sent = Some(whole);
        let item = |key: &str, label: &str, us: u64| TelemetryItem {
            key: key.into(),
            label: label.into(),
            unit: Unit::Second,
            value: Some(seconds(us)),
            text: None,
            source: SOURCE.into(),
            component_id: 0,
            time_us: now,
        };
        let mut items = vec![item("flight.armed_s", "Time armed", armed_us)];
        if aerial {
            items.push(item("flight.air_s", "Time in the air", air_us));
        }
        self.telemetry_observed(VehicleTelemetry {
            vehicle_id: id,
            positions: Vec::new(),
            items,
        });
    }
}

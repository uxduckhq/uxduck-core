//! Whether a vehicle's reported position is where it is (FB-313). `ArduPlane`
//! and Rover with no GPS fix still report a position, the one DCM reckons
//! from 0, 0 with its airspeed, so a vehicle on the bench is drawn off the
//! coast of Africa. A position counts with a 3D GPS fix, or when the EKF says
//! it has an absolute position of its own, from beacons or another source,
//! or while it dead-reckons through a short GPS loss; never by how far it is
//! from 0, 0, since someone may fly there. Commands, arming and the engine's
//! own checks still read every position the vehicle sends: this decides only
//! what is shown and offered to a person.

use uxduck_contracts::dto::telemetry::PositionEstimate as PositionDto;
use uxduck_core::domain::telemetry::{PositionEstimate, TelemetryItem, Unit, VehicleTelemetry};

use crate::Engine;

/// `GPS_FIX_TYPE_3D_FIX`: the least fix `ArduPilot` and PX4 navigate by.
const FIX_3D: f64 = 3.0;

/// Whether a vehicle's position can be used, from its values by key: its
/// GPS's fix (`gps.fix_type`) and its EKF's own word (`ekf.position_abs`).
/// None while it has said neither, as a vehicle that sends no GPS or EKF
/// report does, so its position is shown as before.
#[must_use]
pub(crate) fn position_usable(value: impl Fn(&str) -> Option<f64>) -> Option<bool> {
    let fix = value("gps.fix_type");
    let ekf = value("ekf.position_abs");
    if fix.is_some_and(|fix| fix >= FIX_3D) || ekf == Some(1.0) {
        return Some(true);
    }
    (fix.is_some() || ekf.is_some()).then_some(false)
}

/// [`position_usable`] of what a vehicle has said so far.
#[must_use]
pub(crate) fn telemetry_position_usable(telemetry: &VehicleTelemetry) -> Option<bool> {
    position_usable(|key| {
        telemetry
            .items
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.value)
    })
}

/// What a vehicle whose position is not usable lacks, from its GPS's fix.
fn lacks(fix: Option<f64>) -> &'static str {
    match fix {
        Some(fix) if fix < 1.0 => "No GPS",
        Some(fix) if (2.0..FIX_3D).contains(&fix) => "Only a 2D GPS fix",
        _ => "No GPS fix",
    }
}

fn fix_of(telemetry: &VehicleTelemetry) -> Option<f64> {
    telemetry
        .items
        .iter()
        .find(|i| i.key == "gps.fix_type")
        .and_then(|i| i.value)
}

/// Marks positions, as the API gives them, not usable (FB-313), each with
/// why: `lacks` is what the vehicle lacked when it sent them.
pub(crate) fn mark_unusable<'a>(
    lacks: &str,
    positions: impl IntoIterator<Item = &'a mut PositionDto>,
) {
    let reason = format!("{lacks} and no EKF position, so this is not where the vehicle is");
    for position in positions {
        position.usable = Some(false);
        position.reason = Some(reason.clone());
    }
}

/// Marks a vehicle's positions, as the API gives them, not usable when they
/// are not (FB-313), each with why.
pub(crate) fn mark_positions(telemetry: &VehicleTelemetry, positions: &mut [PositionDto]) {
    if telemetry_position_usable(telemetry) == Some(false) {
        mark_unusable(lacks(fix_of(telemetry)), positions);
    }
}

impl Engine {
    /// `position.usable`, 1 or 0, for a vehicle that has reported a
    /// position, when this update changes it: in the same update as the
    /// positions it judges, so a reader never takes one before its verdict.
    pub(crate) fn position_usable_item(&self, update: &VehicleTelemetry) -> Option<TelemetryItem> {
        let latest = self.telemetry.get(&update.vehicle_id);
        let placed = update
            .positions
            .first()
            .or_else(|| latest.and_then(|t| t.positions.first()))?;
        let value = |key: &str| {
            update
                .items
                .iter()
                .find(|i| i.key == key)
                .or_else(|| latest.and_then(|t| t.items.iter().find(|i| i.key == key)))
                .and_then(|i| i.value)
        };
        let usable = if position_usable(value)? { 1.0 } else { 0.0 };
        if value("position.usable") == Some(usable) {
            return None;
        }
        Some(TelemetryItem {
            key: "position.usable".into(),
            label: "Position usable".into(),
            unit: Unit::None,
            value: Some(usable),
            text: None,
            source: "position".into(),
            component_id: placed.component_id,
            time_us: self.clock.now_us(),
        })
    }

    /// What the vehicle lacked when it sent this update's positions, when
    /// they are not where it is: kept with them in its recent history, so
    /// `telemetry/window` marks them as `telemetry/get` does.
    pub(crate) fn update_lacks(&self, update: &VehicleTelemetry) -> Option<&'static str> {
        if update.positions.is_empty() {
            return None;
        }
        let latest = self.telemetry.get(&update.vehicle_id);
        let value = |key: &str| {
            update
                .items
                .iter()
                .find(|i| i.key == key)
                .or_else(|| latest.and_then(|t| t.items.iter().find(|i| i.key == key)))
                .and_then(|i| i.value)
        };
        if position_usable(value)? {
            return None;
        }
        Some(lacks(value("gps.fix_type")))
    }

    /// Keeps the vehicle's best position from this update, the fused one
    /// first, when it says where the vehicle is: what a watcher is shown
    /// while it has no fix.
    pub(crate) fn keep_placed_position(&mut self, update: &VehicleTelemetry) {
        let Some(best) = update
            .positions
            .iter()
            .find(|p| p.source == "fused")
            .or_else(|| update.positions.first())
        else {
            return;
        };
        let usable = self
            .telemetry
            .get(&update.vehicle_id)
            .and_then(telemetry_position_usable);
        if usable != Some(false) {
            self.placed_positions
                .insert(update.vehicle_id.clone(), best.clone());
        }
    }

    /// Where a watcher sees a vehicle whose position is not usable: its last
    /// usable place, with its height, speed, course and heading now, marked
    /// as not where it is now.
    pub(crate) fn placed_for_watchers(&self, telemetry: &VehicleTelemetry) -> Option<PositionDto> {
        let placed = self.placed_positions.get(&telemetry.vehicle_id)?;
        let mut shown: PositionEstimate = telemetry
            .positions
            .iter()
            .find(|p| p.source == "fused")
            .or_else(|| telemetry.positions.first())
            .unwrap_or(placed)
            .clone();
        shown.latitude_deg = placed.latitude_deg;
        shown.longitude_deg = placed.longitude_deg;
        shown.horizontal_accuracy_m = placed.horizontal_accuracy_m;
        let mut dto = PositionDto::from(&shown);
        dto.usable = Some(false);
        dto.reason = Some(format!(
            "{} and no EKF position now. Shown where it last had one",
            lacks(fix_of(telemetry))
        ));
        Some(dto)
    }
}

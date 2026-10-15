//! Telemetry: the generic outputs producers write into (ADR-0006) and panels
//! and maps render. Contracts are SI with explicit frames (ADR-0001 units and
//! frames): metres, seconds, degrees, WGS84. Not journaled; it lives with the
//! vehicle's live record (ADR-0004).

use serde::{Deserialize, Serialize};

/// What an altitude is measured from. Shown next to every altitude. The
/// default, above home, is what an operator flies by until they choose
/// another in Settings (maintainer, 2026-10-05).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AltitudeFrame {
    /// Above mean sea level.
    Amsl,
    /// Above the WGS84 ellipsoid.
    Ellipsoid,
    /// Above the home position.
    #[default]
    AboveHome,
    /// Above the ground below the vehicle: the autopilot's terrain data
    /// (`TERRAIN_REPORT`), not a rangefinder.
    AboveGround,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Altitude {
    pub meters: f64,
    pub frame: AltitudeFrame,
}

/// Where a vehicle is, according to one source. The map draws every
/// estimate the same way and never knows which producer made it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionEstimate {
    /// The producer: `fused` (the autopilot's estimate), `gps`, or a plugin's id.
    pub source: String,
    pub component_id: u8,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// The same altitude in each frame the source reports.
    pub altitudes: Vec<Altitude>,
    /// Direction of travel or of the nose, degrees from true north.
    pub heading_deg: Option<f64>,
    /// Direction of travel over the ground, degrees from true north; none
    /// when too slow to have one (S77's flight path marker).
    pub course_deg: Option<f64>,
    pub ground_speed_mps: Option<f64>,
    pub climb_mps: Option<f64>,
    pub horizontal_accuracy_m: Option<f64>,
    pub vertical_accuracy_m: Option<f64>,
    /// When the daemon received it.
    pub time_us: u64,
}

/// The unit of a telemetry value. Presentation converts; contracts do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    Meter,
    MeterPerSecond,
    Degree,
    DegreePerSecond,
    Volt,
    Ampere,
    Percent,
    /// Vibration (S72).
    MeterPerSecondSquared,
    /// Battery used (S72).
    MilliampHour,
    /// Temperatures (S72).
    DegreeCelsius,
    /// Durations, such as a stopwatch's (S101); shown as `m:ss`.
    Second,
    /// A moment (S104): seconds since 1970 in UTC, shown in the time zone
    /// of whoever looks.
    Time,
    Count,
    /// No unit: a ratio or a label.
    None,
}

impl Unit {
    /// What an item's key ends in for this unit (ADR-0006, "Telemetry item
    /// names"); counts and unitless values have none.
    #[must_use]
    pub const fn suffix(self) -> Option<&'static str> {
        match self {
            Self::Meter => Some("_m"),
            Self::MeterPerSecond => Some("_mps"),
            Self::Degree => Some("_deg"),
            Self::DegreePerSecond => Some("_degps"),
            Self::Volt => Some("_v"),
            Self::Ampere => Some("_a"),
            Self::Percent => Some("_pct"),
            Self::MeterPerSecondSquared => Some("_mps2"),
            Self::MilliampHour => Some("_mah"),
            Self::DegreeCelsius => Some("_degc"),
            Self::Second => Some("_s"),
            Self::Time => Some("_at"),
            Self::Count | Self::None => None,
        }
    }
}

const SUFFIXES: [&str; 12] = [
    "_m", "_mps", "_deg", "_degps", "_v", "_a", "_pct", "_mps2", "_mah", "_degc", "_s", "_at",
];

/// Whether a key says its unit as ADR-0006 asks: `group.name`, lower case
/// words joined by `_`, ending in the unit's suffix, or in none for a count
/// or a unitless value.
#[must_use]
pub fn key_says_unit(key: &str, unit: Unit) -> bool {
    let Some((group, name)) = key.split_once('.') else {
        return false;
    };
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if !word(group) || !word(name) {
        return false;
    }
    match unit.suffix() {
        Some(suffix) => name.ends_with(suffix),
        None => !SUFFIXES.iter().any(|s| name.ends_with(s)),
    }
}

/// `flight.state` (S103): where the autopilot says the vehicle is, from
/// `MAV_LANDED_STATE` 1 to 4, in words.
pub const LANDED_STATES: [&str; 4] = ["On the ground", "In the air", "Taking off", "Landing"];

/// Whether a `flight.state` says the vehicle is off the ground: taking off
/// and landing count as in the air. None for words that are not a state.
#[must_use]
pub fn off_the_ground(state: &str) -> Option<bool> {
    let at = LANDED_STATES.iter().position(|s| *s == state)?;
    Some(at != 0)
}

/// A named value for panels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryItem {
    /// Stable, such as `battery.voltage_v`.
    pub key: String,
    /// For people, such as "Battery".
    pub label: String,
    pub unit: Unit,
    /// A number, or absent when the item is text (such as the GPS fix).
    pub value: Option<f64>,
    pub text: Option<String>,
    pub source: String,
    pub component_id: u8,
    pub time_us: u64,
}

/// A vehicle's latest telemetry: one estimate per source, one item per key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTelemetry {
    pub vehicle_id: String,
    pub positions: Vec<PositionEstimate>,
    pub items: Vec<TelemetryItem>,
}

impl VehicleTelemetry {
    /// Takes newer values from `update`, by source and by key.
    pub fn merge(&mut self, update: &Self) {
        for position in &update.positions {
            match self
                .positions
                .iter_mut()
                .find(|p| p.source == position.source && p.component_id == position.component_id)
            {
                Some(existing) => *existing = position.clone(),
                None => self.positions.push(position.clone()),
            }
        }
        for item in &update.items {
            match self
                .items
                .iter_mut()
                .find(|i| i.key == item.key && i.component_id == item.component_id)
            {
                Some(existing) => *existing = item.clone(),
                None => self.items.push(item.clone()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, value: f64) -> TelemetryItem {
        TelemetryItem {
            key: key.into(),
            label: key.into(),
            unit: Unit::Volt,
            value: Some(value),
            text: None,
            source: "core".into(),
            component_id: 1,
            time_us: 0,
        }
    }

    #[test]
    fn merge_keeps_one_value_per_key() {
        let mut telemetry = VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: vec![],
            items: vec![
                item("battery.voltage_v", 12.0),
                item("battery.current_a", 3.0),
            ],
        };
        telemetry.merge(&VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: vec![],
            items: vec![item("battery.voltage_v", 11.5)],
        });
        assert_eq!(telemetry.items.len(), 2);
        assert_eq!(telemetry.items[0].value, Some(11.5));
    }
}

//! Spoken turns (ADR-0012 V5): what the agent is handed with words said
//! aloud in talk mode. The selected vehicle's readings come first, as the
//! daemon has them, in the operator's units and each with its age, so a
//! question such as "what's the airspeed?" is answered in one model call
//! with no tool round. The agent's instructions say what a message that
//! starts this way is and how to answer it.

use std::fmt::Write as _;

use uxduck_contracts::dto::{telemetry::LinkHealth, thread::SPOKEN_MARK};
use uxduck_core::domain::{
    operator::{
        AltitudeUnit, DistanceUnit, QuantityUnits, SpeedUnit, TemperatureUnit, VerticalSpeedUnit,
    },
    telemetry::{AltitudeFrame, PositionEstimate, TelemetryItem, Unit, VehicleTelemetry},
    thread::Spoken,
    vehicle::{Presence, Vehicle},
};

use crate::Engine;

const FEET_PER_METRE: f64 = 3.280_839_895;
const FEET_PER_MILE: f64 = 5280.0;

impl Engine {
    /// The words said aloud, after the readings of the thread's vehicle.
    pub(crate) fn spoken_text(
        &self,
        vehicle_id: Option<&str>,
        spoken: Spoken,
        words: &str,
    ) -> String {
        let readings = vehicle_id.and_then(|id| self.vehicle(id)).map_or_else(
            || "No vehicle is selected.".to_owned(),
            |vehicle| {
                readings(
                    vehicle,
                    self.telemetry(&vehicle.id),
                    self.link_health_of(&vehicle.id).as_ref(),
                    spoken.units,
                    self.clock.now_us(),
                )
            },
        );
        format!("{SPOKEN_MARK} {readings}]\n\n{words}")
    }
}

/// A vehicle's state and readings, one per line.
fn readings(
    vehicle: &Vehicle,
    telemetry: Option<&VehicleTelemetry>,
    link: Option<&LinkHealth>,
    units: QuantityUnits,
    now_us: u64,
) -> String {
    let presence = match vehicle.presence {
        Presence::Online => "online",
        Presence::Stale => "not heard for a few seconds",
        Presence::Offline => "offline",
    };
    let armed = if vehicle.armed { "armed" } else { "disarmed" };
    let mode = vehicle
        .mode
        .as_deref()
        .map_or_else(String::new, |mode| format!(", mode {mode}"));
    let mut out = format!(
        "Readings of \"{}\" (id {}) from the daemon now, in the operator's units: {presence}, {armed}{mode}.",
        vehicle.name, vehicle.id
    );
    let mut items: Vec<&TelemetryItem> = telemetry
        .map(|t| t.items.iter().collect())
        .unwrap_or_default();
    let positions = telemetry.map_or(&[][..], |t| t.positions.as_slice());
    if items.is_empty() && positions.is_empty() {
        out.push_str(" No readings have arrived from it.");
        return out;
    }
    items.sort_by(|a, b| a.key.cmp(&b.key).then(a.component_id.cmp(&b.component_id)));
    for item in items {
        let _ = write!(
            out,
            "\n- {} ({}): {}, {}",
            item.label,
            item.key,
            shown(item, units),
            age(now_us, item.time_us)
        );
    }
    for position in positions {
        let _ = write!(out, "\n- {}", place(position, units, now_us));
    }
    if let Some(link) = link {
        let mut parts = Vec::new();
        if let Some(ms) = link.round_trip_ms {
            let one_way = link
                .one_way_ms
                .map_or(String::new(), |ms| format!(" (one way about {ms:.0} ms)"));
            parts.push(format!("round trip {ms:.0} ms{one_way}"));
        }
        if let Some(loss) = link.loss_percent {
            parts.push(format!("{loss:.0}% of messages lost"));
        }
        if let Some(ms) = link.age_ms {
            parts.push(format!(
                "last heard {} ago",
                age(ms * 1000, 0).trim_end_matches(" old")
            ));
        }
        if !parts.is_empty() {
            let _ = write!(out, "\n- Link: {}", parts.join(", "));
        }
    }
    out
}

/// A position estimate: where, how high in each frame, and when.
fn place(position: &PositionEstimate, units: QuantityUnits, now_us: u64) -> String {
    let mut heights = String::new();
    for altitude in &position.altitudes {
        let frame = match altitude.frame {
            AltitudeFrame::Amsl => "above sea level",
            AltitudeFrame::Ellipsoid => "above the ellipsoid",
            AltitudeFrame::AboveHome => "above home",
            AltitudeFrame::AboveGround => "above the ground",
        };
        let _ = write!(
            heights,
            ", {} {frame}",
            height(altitude.meters, units.altitude)
        );
    }
    format!(
        "Position ({}): {:.6}, {:.6}{heights}, {}",
        position.source,
        position.latitude_deg,
        position.longitude_deg,
        age(now_us, position.time_us)
    )
}

/// A value in the operator's unit for its quantity, as UXDuck shows it
/// (showValue in apps/web/src/lib/units.ts).
fn shown(item: &TelemetryItem, units: QuantityUnits) -> String {
    let Some(value) = item.value else {
        return item.text.clone().unwrap_or_else(|| "no value".to_owned());
    };
    let name = format!("{} {}", item.key, item.label).to_lowercase();
    let named = |words: &[&str]| words.iter().any(|word| name.contains(word));
    match item.unit {
        Unit::Meter if named(&["altitude", "height", "depth", "elevation", "rangefinder"]) => {
            height(value, units.altitude)
        }
        Unit::Meter => distance(value, units.distance),
        Unit::MeterPerSecond if named(&["climb", "vertical", "sink"]) => match units.vertical_speed
        {
            VerticalSpeedUnit::FeetPerMinute => {
                format!("{:.0} ft/min", value * FEET_PER_METRE * 60.0)
            }
            VerticalSpeedUnit::MetersPerSecond => format!("{value:.1} m/s"),
        },
        Unit::MeterPerSecond => match units.speed {
            SpeedUnit::MetersPerSecond => format!("{value:.1} m/s"),
            SpeedUnit::KilometersPerHour => format!("{:.0} km/h", value * 3.6),
            SpeedUnit::Knots => format!("{:.1} kn", value * 1.943_844_492),
            SpeedUnit::MilesPerHour => format!("{:.1} mph", value * 2.236_936_292),
        },
        Unit::DegreeCelsius => match units.temperature {
            TemperatureUnit::Celsius => format!("{value:.1} °C"),
            TemperatureUnit::Fahrenheit => format!("{:.1} °F", value * 9.0 / 5.0 + 32.0),
        },
        Unit::Degree => format!("{value:.1}°"),
        Unit::DegreePerSecond => format!("{value:.1}°/s"),
        Unit::Volt => format!("{value:.2} V"),
        Unit::Ampere => format!("{value:.1} A"),
        Unit::Percent => format!("{value:.0}%"),
        Unit::MeterPerSecondSquared => format!("{value:.1} m/s²"),
        Unit::MilliampHour => format!("{value:.0} mAh"),
        Unit::Second => duration(value),
        Unit::Time => format!("{value:.0} seconds since 1970 UTC"),
        Unit::Count => format!("{value:.0}"),
        Unit::None => format!("{value:.2}"),
    }
}

fn height(metres: f64, unit: AltitudeUnit) -> String {
    match unit {
        AltitudeUnit::Feet => format!("{:.0} ft", metres * FEET_PER_METRE),
        AltitudeUnit::Meters => format!("{metres:.1} m"),
    }
}

/// A distance that can be long, as a ruler reads it (showLongDistance).
fn distance(metres: f64, unit: DistanceUnit) -> String {
    let long = |n: f64| {
        if n < 10.0 {
            format!("{n:.2}")
        } else if n < 100.0 {
            format!("{n:.1}")
        } else {
            format!("{n:.0}")
        }
    };
    match unit {
        DistanceUnit::Miles if metres * FEET_PER_METRE < 1000.0 => {
            format!("{:.0} ft", metres * FEET_PER_METRE)
        }
        DistanceUnit::Miles => format!("{} mi", long(metres * FEET_PER_METRE / FEET_PER_MILE)),
        DistanceUnit::Kilometers if (metres * 10.0).round() < 1000.0 => format!("{metres:.1} m"),
        DistanceUnit::Kilometers if metres.round() < 1000.0 => format!("{metres:.0} m"),
        DistanceUnit::Kilometers => format!("{} km", long(metres / 1000.0)),
    }
}

/// A duration in words rather than as a clock reads it, which a voice
/// could take for a time of day.
fn duration(seconds: f64) -> String {
    let whole = seconds.abs().floor();
    let minutes = (whole / 60.0).floor();
    let rest = whole - minutes * 60.0;
    let sign = if seconds < 0.0 && whole > 0.0 {
        "-"
    } else {
        ""
    };
    if minutes > 0.0 {
        format!("{sign}{minutes:.0} min {rest:.0} s")
    } else {
        format!("{sign}{rest:.0} s")
    }
}

/// How long ago something arrived: in tenths of a second while it is fresh.
fn age(now_us: u64, then_us: u64) -> String {
    let ms = now_us.saturating_sub(then_us) / 1000;
    match ms {
        0..10_000 => format!("{}.{} s old", ms / 1000, ms % 1000 / 100),
        10_000..120_000 => format!("{} s old", ms / 1000),
        _ => format!("{} min old", ms / 60_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_core::domain::telemetry::Altitude;

    fn item(key: &str, label: &str, unit: Unit, value: f64, time_us: u64) -> TelemetryItem {
        TelemetryItem {
            key: key.into(),
            label: label.into(),
            unit,
            value: Some(value),
            text: None,
            source: "test".into(),
            component_id: 1,
            time_us,
        }
    }

    fn vehicle() -> Vehicle {
        serde_json::from_value(serde_json::json!({
            "id": "v1", "name": "Scout", "aliases": [], "linkId": "lk", "peer": "p",
            "systemId": 1, "componentIds": [1], "autopilot": "Ardupilotmega",
            "vehicleType": "Quadrotor", "presence": "online", "lastHeartbeatUs": 0,
            "armed": true, "mode": "LOITER", "saved": false, "dismissed": false, "version": 1
        }))
        .unwrap()
    }

    #[test]
    fn readings_are_in_the_operators_units_with_their_age() {
        let telemetry = VehicleTelemetry {
            vehicle_id: "v1".into(),
            positions: vec![PositionEstimate {
                source: "fused".into(),
                component_id: 1,
                latitude_deg: -35.363_261,
                longitude_deg: 149.165_23,
                altitudes: vec![Altitude {
                    meters: 30.48,
                    frame: AltitudeFrame::AboveHome,
                }],
                heading_deg: None,
                course_deg: None,
                ground_speed_mps: None,
                climb_mps: None,
                horizontal_accuracy_m: None,
                vertical_accuracy_m: None,
                time_us: 9_800_000,
            }],
            items: vec![
                item(
                    "speed.air_mps",
                    "Airspeed",
                    Unit::MeterPerSecond,
                    10.0,
                    9_800_000,
                ),
                item(
                    "speed.climb_mps",
                    "Climb",
                    Unit::MeterPerSecond,
                    1.0,
                    9_800_000,
                ),
                item("home.distance_m", "Home", Unit::Meter, 2000.0, 9_000_000),
                item(
                    "battery.voltage_v",
                    "Battery",
                    Unit::Volt,
                    12.345,
                    1_000_000,
                ),
            ],
        };
        let imperial = QuantityUnits {
            distance: DistanceUnit::Miles,
            altitude: AltitudeUnit::Feet,
            speed: SpeedUnit::Knots,
            vertical_speed: VerticalSpeedUnit::FeetPerMinute,
            temperature: TemperatureUnit::Fahrenheit,
        };
        let link = LinkHealth {
            round_trip_ms: Some(41.6),
            one_way_ms: Some(20.8),
            round_trip_missing: None,
            loss_percent: Some(2.0),
            age_ms: Some(300),
        };
        let text = readings(
            &vehicle(),
            Some(&telemetry),
            Some(&link),
            imperial,
            10_000_000,
        );
        assert_eq!(
            text,
            "Readings of \"Scout\" (id v1) from the daemon now, in the operator's units: online, armed, mode LOITER.\n\
             - Battery (battery.voltage_v): 12.35 V, 9.0 s old\n\
             - Home (home.distance_m): 1.24 mi, 1.0 s old\n\
             - Airspeed (speed.air_mps): 19.4 kn, 0.2 s old\n\
             - Climb (speed.climb_mps): 197 ft/min, 0.2 s old\n\
             - Position (fused): -35.363261, 149.165230, 100 ft above home, 0.2 s old\n\
             - Link: round trip 42 ms (one way about 21 ms), 2% of messages lost, last heard 0.3 s ago"
        );
        let metric = readings(
            &vehicle(),
            Some(&telemetry),
            None,
            QuantityUnits::default(),
            200_000_000,
        );
        assert!(
            metric.contains("Airspeed (speed.air_mps): 10.0 m/s, 3 min old"),
            "{metric}"
        );
        assert!(
            metric.contains("Home (home.distance_m): 2.00 km"),
            "{metric}"
        );
    }

    #[test]
    fn a_vehicle_with_no_readings_says_so() {
        let text = readings(&vehicle(), None, None, QuantityUnits::default(), 0);
        assert!(
            text.ends_with("armed, mode LOITER. No readings have arrived from it."),
            "{text}"
        );
    }
}

//! Telemetry on the wire (ADR-0006 generic outputs, ADR-0001 units and
//! frames). SI units with explicit altitude frames; presentation converts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What an altitude is measured from. Above home is the default an
/// operator flies by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AltitudeFrame {
    /// Above mean sea level.
    Amsl,
    /// Above the WGS84 ellipsoid.
    Ellipsoid,
    /// Above the home position.
    #[default]
    AboveHome,
    /// Above the ground below the vehicle, from the autopilot's terrain
    /// data.
    AboveGround,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Altitude {
    pub meters: f64,
    pub frame: AltitudeFrame,
}

/// Where a vehicle is, according to one source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PositionEstimate {
    /// The producer: `fused` (the autopilot's estimate), `gps`, or a plugin's id.
    pub source: String,
    pub component_id: u8,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// The same altitude in each frame the source reports.
    pub altitudes: Vec<Altitude>,
    /// Degrees from true north.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// Direction of travel over the ground, degrees from true north; absent
    /// when too slow to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub course_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_speed_mps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub climb_mps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizontal_accuracy_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_accuracy_m: Option<f64>,
    /// When the daemon received it.
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
    Meter,
    MeterPerSecond,
    Degree,
    DegreePerSecond,
    Volt,
    Ampere,
    Percent,
    /// m/s², for vibration (S72).
    MeterPerSecondSquared,
    /// mAh, for battery used (S72).
    MilliampHour,
    /// °C (S72).
    DegreeCelsius,
    /// Durations, such as a stopwatch's (S101); shown as `m:ss`.
    Second,
    /// A moment (S104): seconds since 1970 in UTC, key suffix `_at`, shown
    /// in the time zone of whoever looks.
    Time,
    Count,
    None,
}

/// A named, unit-bearing value for panels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryItem {
    /// Stable, such as `battery.voltage_v`.
    pub key: String,
    pub label: String,
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// For values that are words, such as the GPS fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How fast `value` changes per second until the next update, when a
    /// plugin says so (S104): the app counts on in between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_per_s: Option<f64>,
    pub source: String,
    pub component_id: u8,
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
}

/// A vehicle's telemetry: one estimate per source, one item per key. In
/// `telemetry/update`, only what changed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTelemetry {
    pub vehicle_id: String,
    pub positions: Vec<PositionEstimate>,
    pub items: Vec<TelemetryItem>,
    /// How its MAVLink link is doing (S261), measured once a second while
    /// it has an address: absent in an update that does not change it, and
    /// from daemons before S261.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_health: Option<LinkHealth>,
}

/// How the link to a vehicle is doing (S261, ADR-0004 2026-11-01), as
/// Stats for nerds shows it: MAVLink's round trip, the share of the
/// vehicle's messages lost, and how long since it was last heard.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkHealth {
    /// The median round trip of MAVLink's own ping (`TIMESYNC`, sent once
    /// a second from the ground station id) over the replies of the last
    /// 5 s, in milliseconds. Absent, `roundTripMissing` says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_ms: Option<f64>,
    /// The one-way estimate (FB-106): half of `roundTripMs`, assuming both
    /// directions take equally long, as telemetry and commands each travel
    /// one way. Absent with `roundTripMs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_way_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_trip_missing: Option<RoundTripMissing>,
    /// Of the messages the vehicle sent in the last 10 s, the share lost,
    /// from gaps in their MAVLink sequence numbers, in percent. Absent when
    /// nothing was heard in that time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_percent: Option<f64>,
    /// Since the vehicle was last heard, any message, in milliseconds: its
    /// telemetry's age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub age_ms: Option<u64>,
}

/// Why a vehicle has no round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RoundTripMissing {
    /// Asked, and its first reply has not come yet.
    Waiting,
    /// Asked, and no reply in the last 5 s: it does not answer `TIMESYNC`,
    /// or the replies are lost.
    NoReply,
    /// Its link is read-only: UXDuck sends nothing on it.
    ReadOnly,
    /// Nobody supervises it, so its ground station heartbeat is withheld
    /// (S81), and the ping with it.
    Unsupervised,
    /// Its simulation is paused.
    Paused,
    /// UXDuck sends it nothing where it is heard.
    NotSent,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySubscribeResult {
    /// The latest of every vehicle; updates follow as `telemetry/update`.
    pub vehicles: Vec<VehicleTelemetry>,
}

/// A formula to try (ADR-0056), as a calculated value would have it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryCalculateParams {
    /// Such as `battery.voltage_v * battery.current_a`.
    pub formula: String,
    /// The vehicle to work it out for now; without one it is only read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// What a formula reads, and its value now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryCalculateResult {
    /// For the vehicle now; absent without one, while an item or parameter
    /// it reads has no value, or when the answer is not a number (a
    /// division by zero).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The telemetry items it reads, each once.
    pub items: Vec<String>,
    /// The parameters it reads, each once.
    pub params: Vec<String>,
    /// Those items and parameters the vehicle has no recent value for.
    pub missing: Vec<String>,
}

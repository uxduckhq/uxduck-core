//! Other aircraft near the vehicles (ADR-0034, S168): one picture merged
//! from every source a person turned on, and every source UXDuck knows of,
//! on or off. Live state: kept in memory, never journaled; only a source's
//! switch is.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What kind of aircraft.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficCategory {
    #[default]
    Unknown,
    Light,
    Small,
    Large,
    Heavy,
    Fast,
    Rotorcraft,
    Glider,
    Balloon,
    Parachute,
    Ultralight,
    Uav,
    Space,
    Surface,
    Obstacle,
}

/// How the aircraft's position reached its receiver: `adsb` on 1090 MHz,
/// `uat` on 978 MHz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficProtocol {
    #[default]
    Adsb,
    Uat,
}

/// Reported height reference: pressure, GNSS above sea level (MAVLink),
/// or GNSS above the WGS84 ellipsoid (readsb).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficAltitudeFrame {
    Pressure,
    Geometric,
    Ellipsoid,
}

/// One aircraft: its freshest report from the sources that are on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficAircraft {
    /// Its ICAO 24-bit address, six hexadecimal digits in capitals.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Its transponder code, four octal digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squawk: Option<String>,
    pub category: TrafficCategory,
    pub protocol: TrafficProtocol,
    /// Absent for an aircraft that gave no position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_frame: Option<TrafficAltitudeFrame>,
    /// Normalized EGM96 sea-level height; absent for incomparable/unknown altitude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amsl_m: Option<f64>,
    /// WGS84 height for globe placement; absent when altitude is incomparable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ellipsoid_m: Option<f64>,
    /// Comparisons against the named locations. Old daemons omit these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relative: Vec<TrafficRelative>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_vehicle_ids: Vec<String>,
    /// Degrees from true north.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_speed_mps: Option<f64>,
    /// Up is positive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub climb_mps: Option<f64>,
    /// Its receiver marked it as made up, such as a simulator's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub simulated: bool,
    /// When it was last heard, Unix milliseconds.
    #[schemars(schema_with = "crate::safe::integer")]
    pub heard_at_ms: u64,
    /// Not heard for 10 s: drawn grey. Gone after 60 s.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// The ids of the sources that hear it now.
    pub sources: Vec<String>,
    /// One of our own vehicles (S169), heard by a receiver: its ADS-B out.
    /// Never drawn or alerted on as traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// How it was known to be ours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<TrafficOwnMatch>,
}

/// How an aircraft was known to be one of our vehicles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficOwnMatch {
    /// Its address is the vehicle's (`ADSB_ICAO_ID`).
    Address,
    /// It is where the vehicle is: within 30 m across and 15 m up or down
    /// (50 m for a pressure altitude). Probably the vehicle.
    Nearby,
}

/// Where a source's traffic comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficSourceKind {
    /// A vehicle's own ADS-B receiver, over MAVLink.
    Vehicle,
    /// A plugin, such as an internet feed (S170): on while the plugin is.
    Plugin,
    /// A receiver on the network, on a link of its own (S172): on while the
    /// link is.
    Receiver,
}

/// How a source is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficSourceState {
    /// Reported in the last 60 s.
    Hearing,
    /// Nothing for 60 s.
    #[default]
    Quiet,
    /// Sent too much at once and is ignored for a while.
    Paused,
}

/// Why a report was refused.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum TrafficRefusalReason {
    /// Outside the Earth, or at 0°, 0°.
    Position,
    /// Below -500 m or above 20 km.
    Altitude,
    /// Moved faster than any aircraft since the source last reported it.
    Jump,
    /// More aircraft than one source may give.
    TooMany,
    /// Arrived while the source was paused.
    Paused,
    /// An address that is not six hexadecimal digits (a plugin's).
    Address,
}

/// How many reports a source had refused, for one reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficRefusal {
    pub reason: TrafficRefusalReason,
    #[schemars(schema_with = "crate::safe::integer")]
    pub count: u64,
}

/// A source of traffic, on or off.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSource {
    /// `vehicle:<vehicle id>`.
    pub id: String,
    pub kind: TrafficSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// Its traffic is in the picture.
    pub on: bool,
    pub state: TrafficSourceState,
    /// How many aircraft it hears now, on or off.
    pub aircraft: u32,
    /// When it last reported, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_heard_at_ms: Option<u64>,
    /// How it is doing, in its own words (a plugin's, S171).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The note is a problem.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub problem: bool,
    /// Who to credit wherever its traffic is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<String>,
    /// What it sent that was refused, since UXDuck started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<TrafficRefusal>,
}

/// `traffic/status` answers this and `traffic/changed` pushes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficStatus {
    /// Explicit controls and normalized altitude comparisons are supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controls_version: Option<u32>,
    pub aircraft: Vec<TrafficAircraft>,
    pub sources: Vec<TrafficSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<TrafficReference>,
}

/// `vehicle/traffic`: whether the traffic a vehicle reports is used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTrafficParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    pub on: bool,
}

/// A reference used by display filters and by the independent alert comparisons.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficReference {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amsl_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_accuracy_m: Option<f64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficRelative {
    pub reference_id: String,
    pub distance_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above_m: Option<f64>,
    /// Comparison margin from reported own-vehicle accuracy and geoid interpolation.
    pub margin_m: f64,
}
/// Coverage requested from a traffic feed, independent of vehicle arming.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrafficFeedContext {
    pub references: Vec<TrafficReference>,
    pub radius_m: u32,
    pub refresh_s: u32,
}

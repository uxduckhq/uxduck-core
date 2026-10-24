//! Flights on the wire (C8): arm to disarm, as UXDuck saw them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a flight ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FlightEnd {
    /// Still armed.
    Open,
    Disarmed,
    /// UXDuck stopped while it was armed; ended when UXDuck last heard it.
    Interrupted,
    /// It went offline while armed; ended when UXDuck last heard it.
    ContactLost,
}

/// Someone who acted on the vehicle during a flight, from their first
/// command or claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightOperator {
    pub operator_id: String,
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Flight {
    pub id: String,
    pub vehicle_id: String,
    pub vehicle_name: String,
    pub vehicle_type: String,
    #[serde(default)]
    pub simulated: bool,
    /// UXDuck first heard it already armed, so it began earlier.
    #[serde(default)]
    pub seen_armed: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub armed_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    pub end: FlightEnd,
    #[schemars(schema_with = "crate::safe::integer")]
    pub air_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_speed_mps: Option<f64>,
    #[serde(default)]
    pub modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_start_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_end_pct: Option<f64>,
    #[serde(default)]
    pub operators: Vec<FlightOperator>,
    /// Saved to the account, so it is in the logbook on every computer (C9).
    #[serde(default)]
    pub keep: bool,
    /// The parameters it armed with (C7), when UXDuck had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<FlightParams>,
}

/// The parameters a flight armed with (C7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightParams {
    /// SHA-256 of every parameter, one `NAME=value` line each, by name:
    /// the same digest is the same configuration.
    pub digest: String,
    pub count: u32,
    /// The vehicle's newest baseline then, and how many parameters
    /// differed from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_differ: Option<u32>,
}

/// `flight/keep`: saves a flight to the account, or keeps it here only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightKeepParams {
    pub id: String,
    pub keep: bool,
    /// Every real flight from now on too ("Save every flight to my
    /// account"), or none when `keep` is false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always: bool,
}

/// `flight/list`: this computer's flights, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListParams {
    /// Only this vehicle's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// At most this many (default 100, at most 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Also totals since this time, such as the start of the month where
    /// the page is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub totals_since_us: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightListResult {
    pub flights: Vec<Flight>,
    /// Time in the air over every flight listed or not (C9).
    #[serde(default)]
    pub totals: FlightTotals,
}

/// Time in the air, real and simulated apart (C9), all time and since
/// `totalsSinceUs` (zero when not asked).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightTotals {
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_air_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_air_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub real_air_us_since: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub simulated_air_us_since: u64,
    /// Flights in all, of which `flights` holds the newest.
    pub count: u32,
}

impl From<&uxduck_core::domain::flight::Flight> for Flight {
    fn from(f: &uxduck_core::domain::flight::Flight) -> Self {
        use uxduck_core::domain::flight::FlightEnd as Core;
        Self {
            id: f.id.clone(),
            vehicle_id: f.vehicle_id.clone(),
            vehicle_name: f.vehicle_name.clone(),
            vehicle_type: f.vehicle_type.clone(),
            simulated: f.simulated,
            seen_armed: f.seen_armed,
            armed_at_us: f.armed_at_us,
            ended_at_us: f.ended_at_us,
            end: match f.end {
                Core::Open => FlightEnd::Open,
                Core::Disarmed => FlightEnd::Disarmed,
                Core::Interrupted => FlightEnd::Interrupted,
                Core::ContactLost => FlightEnd::ContactLost,
            },
            air_us: f.air_us,
            max_height_m: f.max_height_m,
            max_speed_mps: f.max_speed_mps,
            modes: f.modes.clone(),
            battery_start_pct: f.battery_start_pct,
            battery_end_pct: f.battery_end_pct,
            operators: f
                .operators
                .iter()
                .map(|o| FlightOperator {
                    operator_id: o.operator_id.clone(),
                    name: o.name.clone(),
                    from_us: o.from_us,
                })
                .collect(),
            keep: f.keep,
            params: f.params.as_ref().map(|p| FlightParams {
                digest: p.digest.clone(),
                count: p.count,
                baseline_id: p.baseline_id.clone(),
                baseline_name: p.baseline_name.clone(),
                baseline_differ: p.baseline_differ,
            }),
        }
    }
}

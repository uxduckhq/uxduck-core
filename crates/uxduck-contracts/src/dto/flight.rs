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
    /// Someone ended it while the vehicle could not be heard (`flight/end`).
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

#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a fact about the flight"
)]
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
    /// Recorded on this computer, rather than had from the account.
    #[serde(default)]
    pub recorded_here: bool,
    /// Deleted from the account after it was saved; it stays here only.
    #[serde(default)]
    pub left_account: bool,
    /// While open: when UXDuck last heard the vehicle, if it has gone
    /// offline since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub contact_lost_at_us: Option<u64>,
    /// The mission it flew, if it flew one (C8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<FlightMission>,
    /// The parameters it armed with (C7), when UXDuck had them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<FlightParams>,
}

/// A mission a flight flew (C8): flights of one mission share a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightMission {
    /// The mission on the vehicle by its content, when UXDuck knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    /// The vehicle said the mission was complete during this flight.
    #[serde(default)]
    pub completed: bool,
    /// The first flight of the run.
    pub run_id: String,
}

/// `flight/end`: ends an open flight whose vehicle cannot be heard, at when
/// it was last heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightEndParams {
    pub id: String,
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

/// `flight/keep`: saves a flight to the account, or with `keep: false`
/// deletes it from the account: gone from the computers that had it from
/// there, kept on the one that recorded it. There is no "unsave".
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
            recorded_here: f.recorded_here,
            left_account: f.left_account,
            contact_lost_at_us: f.contact_lost_at_us,
            mission: f.mission.as_ref().map(|m| FlightMission {
                mission_id: m.mission_id.clone(),
                plan_id: m.plan_id.clone(),
                plan_name: m.plan_name.clone(),
                completed: m.completed,
                run_id: m.run_id.clone(),
            }),
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

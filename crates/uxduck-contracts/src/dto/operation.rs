//! Operations (ADR-0039 R16): the flights of one event, made by itself
//! where two or more vehicles flew together, or by a person.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An operation and what it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub id: String,
    /// The person's name for it. Absent for one made by itself: show it as
    /// its first day and its vehicles, "24 Oct · 4 vehicles".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Made by itself and not edited since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// Its flights, oldest first, as `flight/list` names them.
    pub flight_ids: Vec<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub to_us: u64,
    /// How many vehicles flew in it.
    pub vehicles: u32,
    /// How many of its flights are in the account.
    pub kept: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_ids: Vec<String>,
    /// Each flight's clock as moved by hand in its replay (R17).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clock_offsets: Vec<FlightClockOffset>,
    /// Made by `plan/simulate` (ADR-0011 M18): its runs and how each went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<Simulation>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Plans flown by simulated vehicles at once (ADR-0011 M18).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Simulation {
    pub runs: Vec<SimulationRun>,
    /// The runs UXDuck recommended for this computer when they started.
    pub recommended: u32,
    /// The longest a run may fly, in the simulator's minutes.
    pub limit_min: u32,
}

/// One simulated vehicle flying one plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimulationRun {
    /// The simulation's link, kept stopped once the run is over.
    pub link_id: String,
    pub vehicle_name: String,
    pub plan_id: String,
    pub plan_name: String,
    pub speedup: u8,
    /// Wind in metres a second, and the way it blows from, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_mps: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_from_deg: Option<u16>,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preparing: bool,
    /// Why it ended as it did, in a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Its flight, once it armed: `replay/open` plays it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// It went outside the plan's fence at some point.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub breached: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    /// Its simulator starting, until the vehicle is ready.
    Starting,
    /// The plan going up, with its fence and wind.
    Uploading,
    /// Armed in its mission.
    Flying,
    /// Flew its mission to the end.
    Finished,
    /// Disarmed, or stopped flying it, before its mission's end.
    EndedEarly,
    /// Still flying at the time limit.
    TimedOut,
    /// It never got going: the reason is its detail.
    Failed,
    /// Its simulation was stopped, or UXDuck stopped, before it ended.
    Stopped,
}

/// `plan/simulate` and `plan/simulateCheck`: plans to fly in simulation,
/// each as many times as `count` says, all at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanSimulateParams {
    pub runs: Vec<SimulationRunParams>,
    /// The operation's name; by default the plan's name and "simulated".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The longest a run may fly, in the simulator's minutes: 1 to 600,
    /// 60 by default. A run still flying then is stopped as timed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_min: Option<u32>,
}

/// One plan's runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimulationRunParams {
    pub plan_id: String,
    /// How many vehicles fly it: 1 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// Times real time, 1 to 10: 1 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speedup: Option<u8>,
    /// Wind in metres a second, 0 to 30; ArduPilot only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_mps: Option<u8>,
    /// Where the wind blows from, in degrees from north: 0 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wind_from_deg: Option<u16>,
}

/// Result of `plan/simulateCheck`: whether `plan/simulate` would start
/// these, and how many this computer is recommended to run at once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanSimulateCheckResult {
    /// Vehicles the runs would start.
    pub runs: u32,
    /// For this computer at the fastest speed-up asked for.
    pub recommended: u32,
    /// Said when `runs` is over `recommended`; never a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Why `plan/simulate` would refuse them, one sentence each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Result of `plan/simulate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanSimulateResult {
    pub operation: Operation,
    /// As `plan/simulateCheck` says, when they are more than recommended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// A flight's clock moved by hand to match the others' (R17).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightClockOffset {
    pub flight_id: String,
    /// Added to its time; negative is earlier.
    #[schemars(schema_with = "crate::safe::signed_integer")]
    pub offset_us: i64,
}

/// Result of `operation/list`, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationListResult {
    pub operations: Vec<Operation>,
}

/// `operation/create`: an operation of these flights, by hand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Flights from `flight/list`; each leaves any operation it was in.
    #[serde(default)]
    pub flight_ids: Vec<String>,
}

/// `operation/rename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationRenameParams {
    pub id: String,
    /// Empty goes back to its day and its vehicles.
    pub name: String,
}

/// `operation/addFlights` and `operation/removeFlights`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperationFlightsParams {
    pub id: String,
    pub flight_ids: Vec<String>,
}

//! The Tuning tab (S329, ADR-0054): which of a vehicle's parameters fill
//! each part of it, with their values, for the tab and the agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `tuning/state`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningStateParams {
    pub vehicle_id: String,
}

/// A set of controllers a vehicle is tuned by. A VTOL has two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningController {
    /// Copter's multicopter controllers (`ATC_`).
    Copter,
    /// A quadplane's hover (`Q_A_`).
    Hover,
    /// Plane's fixed-wing rate controllers.
    Plane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningAxis {
    Roll,
    Pitch,
    Yaw,
}

/// One term of a rate controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningTerm {
    P,
    I,
    D,
    Ff,
    /// The most the I term may add.
    Imax,
    /// The target filter, in Hz.
    Fltt,
    /// The D term's filter, in Hz.
    Fltd,
    /// The error filter, in Hz.
    Flte,
}

/// How the vehicle is asked to move: never a gain (ADR-0054 T4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningFeel {
    /// A time constant: smaller follows the sticks more sharply.
    Responsiveness,
    MaxLean,
    MaxRollRate,
    MaxPitchRate,
    MaxYawRate,
    ClimbSpeed,
    DescentSpeed,
    RollLimit,
    PitchUpLimit,
    PitchDownLimit,
    /// A time constant: smaller is sharper.
    RollResponse,
    /// A time constant: smaller is sharper.
    PitchResponse,
}

/// The vehicle's parameter in a part of the tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningParam {
    pub name: String,
    pub value: f64,
    /// The firmware's default, when the vehicle said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
    /// From the vehicle's definitions; names that changed between releases
    /// changed their units too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningFeelPart {
    pub feel: TuningFeel,
    pub param: TuningParam,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningTermPart {
    pub term: TuningTerm,
    pub param: TuningParam,
}

/// One axis's rate controller: the terms the vehicle has.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningRate {
    pub axis: TuningAxis,
    pub terms: Vec<TuningTermPart>,
}

/// One axis's angle controller P.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningAngle {
    pub axis: TuningAxis,
    pub param: TuningParam,
}

/// One controller's parts the vehicle has.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningControllerParts {
    pub controller: TuningController,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feel: Vec<TuningFeelPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rates: Vec<TuningRate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub angles: Vec<TuningAngle>,
    /// ArduPilot's tuning process for it, in order (S331).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<TuningStepState>,
    /// Its AutoTune, where the vehicle has it (S332).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autotune: Option<TuningAutotune>,
}

/// How a controller's AutoTune works, and how the transmitter enters it
/// now (S332, ADR-0054 T10). A person flies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningAutotune {
    /// As `set-mode` takes it.
    pub mode: String,
    /// As `FLTMODEn` holds it.
    pub mode_number: u32,
    /// The bitmask of axes it tunes.
    pub axes: String,
    /// How hard it pushes: `AUTOTUNE_AGGR` or `AUTOTUNE_LEVEL`.
    pub strength: String,
    /// The `RCx_OPTION` that turns it on from a switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_option: Option<u32>,
    /// What the firmware does with the gains it found.
    pub keeps: String,
    /// The mode switch's channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    /// The mode switch's positions (1 to 6) that are AutoTune.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<u32>,
    /// The channels whose switch turns it on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub switches: Vec<u32>,
}

/// Where an AutoTune is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningAutotunePhase {
    Running,
    /// Every axis done, not kept yet.
    Done,
    Kept,
    /// It left AutoTune on the gains from before.
    Stopped,
    Failed,
}

/// A parameter's value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningValue {
    pub name: String,
    pub value: f64,
}

/// The vehicle's last AutoTune, from its status texts (S332).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningAutotuneRun {
    pub controller: TuningController,
    pub phase: TuningAutotunePhase,
    /// Where it is, in one line.
    pub line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<TuningAxis>,
    /// The axes that have their gains.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<TuningAxis>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    /// Its tuning values when it started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<TuningValue>,
}

/// A step of ArduPilot's tuning process (ADR-0054 T3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningStep {
    Prepare,
    FirstFlight,
    Filters,
    Tune,
    Feel,
}

/// A step's state, from the vehicle's parameters: advice, gating nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningStepState {
    pub step: TuningStep,
    pub done: bool,
    pub why: String,
    /// The parameters it reads.
    pub names: Vec<String>,
    /// When one of them last changed, from the vehicle's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub changed_at_us: Option<u64>,
}

/// Why a vehicle has no Tuning tab (ADR-0054 T14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningUnsupported {
    /// Not ArduPilot.
    Autopilot,
    /// A traditional helicopter.
    Helicopter,
    /// Rovers, boats, submarines, blimps and trackers.
    VehicleType,
    /// Its parameters have not been read yet.
    NotRead,
}

/// `tuning/state`'s result: the vehicle's tuning parameters by part, as
/// the Tuning tab lays them out. Values are the vehicle's now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningState {
    pub vehicle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<TuningUnsupported>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<TuningControllerParts>,
    /// Its last AutoTune (S332).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autotune: Option<TuningAutotuneRun>,
    /// How each axis flew in the last 10 s, as `tuning/response` sums it
    /// up, without its samples (S333).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flying: Vec<TuningAxisSummary>,
    /// The tuning changes in the last minute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<TuningMark>,
}

/// An axis's summary, without its samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningAxisSummary {
    pub axis: TuningAxis,
    pub source: TuningSource,
    pub summary: TuningSummary,
}

/// `tuning/response`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningResponseParams {
    pub vehicle_id: String,
    /// Only samples and marks after this time; none gives the last minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub after_us: Option<u64>,
}

/// Where an axis's samples come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningSource {
    /// The rate controller's `PID_TUNING`, with its terms.
    Pid,
    /// `ATTITUDE`'s body rates against `ATTITUDE_TARGET`'s.
    Attitude,
}

/// One moment of an axis: rates in degrees per second, terms in the
/// controller's output units.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningSample {
    #[schemars(schema_with = "crate::safe::integer")]
    pub t_us: u64,
    pub desired: f64,
    pub achieved: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ff: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningVerdict {
    Gathering,
    /// Hardly asked to turn: nothing to judge.
    Still,
    Following,
    Slow,
    Overshooting,
    Oscillating,
}

/// The last ten seconds of an axis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningSummary {
    pub verdict: TuningVerdict,
    /// The most it was asked, in degrees per second.
    pub asked_degps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_error_pct: Option<f64>,
    /// Past the most asked, in percent of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overshoot_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oscillation_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oscillation_degps: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningAxisResponse {
    pub axis: TuningAxis,
    pub source: TuningSource,
    pub samples: Vec<TuningSample>,
    pub summary: TuningSummary,
}

/// A tuning parameter changed while samples came.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningMark {
    #[schemars(schema_with = "crate::safe::integer")]
    pub t_us: u64,
    pub name: String,
    pub from: f64,
    pub to: f64,
}

/// `tuning/response`'s result (S330): what each rate controller was asked
/// and did, with a summary, and the tuning changes made meanwhile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningResponse {
    pub vehicle_id: String,
    /// Its time now: a replay's when it is one.
    #[schemars(schema_with = "crate::safe::integer")]
    pub now_us: u64,
    /// UXDuck asked it for its samples 25 times a second.
    pub live: bool,
    pub axes: Vec<TuningAxisResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<TuningMark>,
}

/// `tuning/live`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningLiveParams {
    pub vehicle_id: String,
    pub on: bool,
}

/// `tuning/live`'s result: false while the vehicle is not heard or its
/// link is read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningLiveResult {
    pub sent: bool,
}

/// A battery's chemistry, for its full and empty voltages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TuningChemistry {
    Lipo,
    LipoHv,
    LiIon,
}

/// `tuning/prepare`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningPrepareParams {
    pub vehicle_id: String,
    pub controller: TuningController,
    /// 1 to 40.
    pub prop_inches: f64,
    /// 1 to 24.
    pub cells: u32,
    pub chemistry: TuningChemistry,
}

/// A starting value for a parameter the vehicle has.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningProposal {
    pub name: String,
    pub value: f64,
    /// Its value now.
    pub now: f64,
    pub why: String,
}

/// `tuning/prepare`'s result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TuningPrepare {
    pub vehicle_id: String,
    pub proposals: Vec<TuningProposal>,
}

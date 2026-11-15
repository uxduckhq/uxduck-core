//! A vehicle's checklist on the wire (ADR-0038 K7 to K11, K14, K16, K17;
//! S193, S353): manual steps a person ticks and rules over a telemetry
//! item or a parameter, each under a group when it has one. A rule can ask
//! for a person's tick too, once it passes. It belongs to one vehicle: copying from another vehicle, a
//! checklist file or a pasted list makes an independent copy. Its steps are
//! checks in `preflight/state` with source `checklist`.
//!
//! Each time the preflight is done is a run of the checklist (K18, K19;
//! S350): started and finished by a person, with the ticks on its steps.
//! A run as it ended is in `checklist/runs` and the vehicle's history.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::history::Author;
use crate::dto::preflight::{
    Check, CheckRef, CheckSeverity, CheckStatus, CheckWhen, PreflightOverride,
};

/// How a rule compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RuleOp {
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
}

/// Ticked by a person, or a rule UXDuck checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    Manual,
    Rule,
}

/// Who ticked a step, and when. A tick stays until a person clears it:
/// nothing clears it on landing, a battery change or a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub by: Author,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
}

/// A finish of a preflight that a person undid with `checklist/reopen`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistReopening {
    /// Who had finished it, and when.
    pub finished: Tick,
    /// Who reopened it, and when.
    pub reopened: Tick,
}

/// One time a vehicle's preflight is done: a run of its checklist. The
/// ticks on the steps are its ticks. A person starts it, with
/// `checklist/reset` or the first tick, finishes it with
/// `checklist/finish`, and can reopen it with `checklist/reopen`. It
/// never holds or opens Arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRun {
    pub id: String,
    /// Who started it, and when.
    pub started: Tick,
    /// Who finished it, and when. From then its steps stay as they are
    /// and its preflight steps take no tick, until it is reopened or a
    /// new preflight starts. Its launch steps still take a tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<Tick>,
    /// Each earlier finish of it that a person reopened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reopenings: Vec<ChecklistReopening>,
    /// When the vehicle last restarted since the run started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub restarted_at_us: Option<u64>,
}

/// A step as written: in a checklist file, or added or copied in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepInput {
    pub kind: StepKind,
    pub title: String,
    /// The concept the step sits under, such as "Battery".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// A rule a person ticks too, once it passes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// `warn` (the default) only warns. `block` holds the launch until
    /// the step passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CheckSeverity>,
    /// Where it counts, `preflight` when left out.
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RuleOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The value's unit, from the item's key when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_s: Option<f64>,
}

/// One step of a vehicle's checklist.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistStep {
    /// Unique in the checklist; its check is `checklist:<id>`.
    pub id: String,
    pub kind: StepKind,
    pub title: String,
    /// The concept it sits under in the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// A rule a person ticks too, once it passes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    pub severity: CheckSeverity,
    /// Whether it counts in the preflight or at the launch.
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RuleOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_s: Option<f64>,
    /// A person's tick, on a manual step or a rule with `confirm`, until
    /// a person clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<Tick>,
    /// Ticked before the vehicle's last flight began.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flown_since: bool,
}

/// A rule the vehicle can hold itself with one of its parameters, so that
/// arming from its radio is held too: `BATT_ARM_VOLT` for "the battery is
/// at least 22.2 V".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamOffer {
    pub step_id: String,
    pub name: String,
    /// What it would be set to: the rule's value.
    pub value: f64,
    /// What it is on the vehicle now, when UXDuck has read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    /// The vehicle holds the rule already.
    pub held: bool,
    /// Why setting it would not hold arming, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A vehicle's checklist, in `preflight/state`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Checklist {
    pub steps: Vec<ChecklistStep>,
    /// Where its steps were last copied from, in words. Nothing is linked:
    /// changing either checklist leaves the other as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_from: Option<String>,
    /// Rules the vehicle can hold itself, with the parameter to set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offers: Vec<ParamOffer>,
    /// The preflight under way, or the last one finished. None until a
    /// person starts one or ticks a step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ChecklistRun>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// A checklist file (`uxduck.checklist/1`): steps without ids or ticks, a
/// starting point to copy into a vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistFile {
    /// `uxduck.checklist/1`.
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub steps: Vec<StepInput>,
}

/// A change to one step: what is given changes, the rest stays. A tick
/// goes with new words, a changed rule or a change of `confirm`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepChange {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StepKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Null takes the step out of its group.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    #[allow(clippy::option_option, reason = "absent, null or a value")]
    pub group: Option<Option<String>>,
    /// Whether a person ticks the rule too, once it passes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CheckSeverity>,
    /// Moved, it keeps its tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<CheckWhen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RuleOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Null puts the unit from the item's key back.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    #[allow(clippy::option_option, reason = "absent, null or a value")]
    pub unit: Option<Option<String>>,
    /// Null for no holding.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    #[allow(clippy::option_option, reason = "absent, null or a value")]
    pub hold_s: Option<Option<f64>>,
}

/// `checklist/update`: applied together, or not at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistUpdateParams {
    pub vehicle_id: String,
    /// Added at the end, in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<StepInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub change: Vec<StepChange>,
    /// Step ids to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Every step's id in the new order, after adding and removing; steps
    /// left out keep their place at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
}

/// `checklist/copy`: one source. Ticks are never copied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistCopyParams {
    pub vehicle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_vehicle_id: Option<String>,
    /// A checklist file's JSON (`uxduck.checklist/1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Where the steps came from, in words, such as the file's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Replace the vehicle's steps; otherwise they are added after them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

/// `checklist/tick`: a person ticks a manual step or a rule with
/// `confirm`, or clears its tick. A rule with `confirm` is ticked only
/// while its rule passes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistTickParams {
    pub vehicle_id: String,
    pub step_id: String,
    pub ticked: bool,
}

/// `checklist/runs`: a vehicle's earlier preflights.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRunsParams {
    pub vehicle_id: String,
    /// At most this many (default 20, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A vehicle's earlier preflights, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRunsResult {
    pub runs: Vec<ChecklistRunRecord>,
}

/// A preflight as it ended: finished by a person, which locked it, or
/// left unfinished when a new one started. One that was reopened and
/// finished again has a record for each finish, with the same `id`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRunRecord {
    pub id: String,
    pub started: Tick,
    /// None when it was left unfinished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<Tick>,
    /// Each earlier finish of it that a person reopened, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reopenings: Vec<ChecklistReopening>,
    /// Each step as it stood: its tick, its rule's value and its status.
    pub steps: Vec<RecordStep>,
    /// The vehicle's own reasons, UXDuck's checks and plugins' checks as
    /// they stood.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
}

/// A step as it stood when the vehicle armed, or when a preflight ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordStep {
    pub title: String,
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// A rule that asked for a person's tick too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    pub severity: CheckSeverity,
    /// Whether it counted in the preflight or at the launch.
    #[serde(default, skip_serializing_if = "CheckWhen::is_preflight")]
    pub when: CheckWhen,
    pub status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick: Option<Tick>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RuleOp>,
    /// The rule's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_s: Option<f64>,
    /// What the rule read then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// The checklist as it stood when a vehicle armed, with what its launch
/// went past: in its history and its flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRecord {
    pub steps: Vec<RecordStep>,
    /// Checks set to hold that UXDuck's launch was sent past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<CheckRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_by: Option<PreflightOverride>,
    /// It left the ground without UXDuck launching it while these checks
    /// set to hold did not pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<CheckRef>,
    /// The preflight it armed under: under way, finished or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ChecklistRun>,
}

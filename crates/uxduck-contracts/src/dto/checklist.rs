//! A vehicle's checklist on the wire (ADR-0038 K7 to K11, K14; S193):
//! manual steps a person ticks and rules over a telemetry item or a
//! parameter. It belongs to one vehicle: copying from another vehicle, a
//! checklist file or a pasted list makes an independent copy. Its steps are
//! checks in `preflight/state` with source `checklist`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::history::Author;
use crate::dto::preflight::{CheckRef, CheckSeverity, CheckStatus, PreflightOverride};

/// How a rule compares its value with the step's.
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

/// A manual step for a person to tick, or a rule UXDuck checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    Manual,
    Rule,
}

/// Who ticked a manual step, and when. A tick stays until a person clears
/// it: nothing clears it on landing, a battery change or a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub by: Author,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
}

/// A step as written: in a checklist file, or added or copied in. A rule
/// reads one telemetry item (`item`, such as `battery.voltage_v` or
/// `battery.cell_v`) or one parameter (`param`), compares it (`op`) with
/// `value`, in the item's unit, and passes once that has held for `holdS`
/// seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepInput {
    pub kind: StepKind,
    pub title: String,
    /// Block (the default) stops arming until it passes; warn only warns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CheckSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RuleOp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// How people read the value's unit; from the item's key or the
    /// parameter's definition when left out.
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
    pub severity: CheckSeverity,
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
    /// A manual step's tick, until a person clears it.
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

/// A change to one step: what is given changes, the rest stays. Giving
/// `item` reads that item instead of a parameter, and `param` the other
/// way round. A manual step's tick stays through a change of words.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StepChange {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StepKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CheckSeverity>,
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

/// `checklist/update`: steps to add, change and remove on one vehicle, and
/// optionally their new order. Applied together, or not at all.
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

/// `checklist/copy`: copies steps into one vehicle's checklist from one
/// source: another vehicle's checklist, a checklist file, or a pasted
/// Markdown or Notion list (each line or `- [ ]` item a manual step,
/// headings left out). Ticks are never copied.
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

/// `checklist/tick`: a person ticks a manual step, or clears its tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistTickParams {
    pub vehicle_id: String,
    pub step_id: String,
    pub ticked: bool,
}

/// A step as it stood when the vehicle armed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordStep {
    pub title: String,
    pub kind: StepKind,
    pub severity: CheckSeverity,
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
    /// What the rule read when the vehicle armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// The checklist as it stood when a vehicle armed, with every override: in
/// its history and its flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistRecord {
    pub steps: Vec<RecordStep>,
    /// Blocking checks it was armed past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<CheckRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_by: Option<PreflightOverride>,
    /// It armed without UXDuck sending it while these blocking checks did
    /// not pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<CheckRef>,
}

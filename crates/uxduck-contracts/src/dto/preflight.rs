//! Preflight checks on the wire (ADR-0038 K1 to K6, S192): a vehicle's list
//! of checks, pushed as the `preflight` entity, and how its UXDuck checks
//! are set. Every detail is in words, the same for the interface, the agent
//! and the terminal.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::setup::SetupRow;

/// Where a check comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSource {
    /// The vehicle's own pre-arm reasons: listed first, never turned off.
    /// UXDuck never holds anything for them. The vehicle refuses to arm by itself.
    Vehicle,
    /// UXDuck's own checks: each on or off per vehicle.
    Uxduck,
    /// A step of the vehicle's checklist.
    Checklist,
    /// A plugin's check.
    Plugin,
}

/// `block` holds the launch until the check passes. `warn` only warns.
/// Arming is never held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckSeverity {
    Block,
    Warn,
}

/// Where a checklist step counts: in the `preflight`, or at the `launch`,
/// where and when the vehicle takes off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckWhen {
    #[default]
    Preflight,
    Launch,
}

impl CheckWhen {
    /// For serde: the default is left off the wire.
    #[must_use]
    pub fn is_preflight(&self) -> bool {
        *self == Self::Preflight
    }
}

/// Where a check stands. A check that holds the launch holds it in every
/// status but `pass`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// Something under way that ends by itself, such as terrain loading.
    Waiting,
    /// UXDuck cannot tell.
    Unknown,
    /// For a person to tick.
    ToDo,
}

/// Where a check's fix is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CheckFix {
    /// A row of the vehicle's Setup tab.
    Setup { row: SetupRow },
    /// A saved plan.
    Plan { plan_id: String },
    /// The Plans page.
    Plans,
    /// The vehicle's Mission tab.
    Mission,
    /// The Cockpit's controllers.
    Controllers,
    /// The vehicle's cameras.
    Cameras,
    /// Restarting the vehicle.
    Restart,
    /// The vehicle's messages.
    Messages,
    /// The vehicle's link.
    Connections,
    /// The plugin a check comes from, on the Plugins page (S194).
    Plugin { plugin_id: String },
    /// Setting home to where the vehicle is now.
    Home,
}

/// UXDuck's own checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OwnCheck {
    /// No setup procedure is running.
    Setup,
    /// No restart is due after a calibration or a parameter change that
    /// takes effect on restarting.
    Restart,
    /// The vehicle was heard just now, and its radio's signal, when it
    /// reports one, is at least the threshold.
    Link,
    /// "Controller input": the vehicle says it has RC input (`rc.status`),
    /// from its receiver or from a controller through UXDuck, which it
    /// counts as RC. Fails when it has lost it or has had none since it
    /// started. `input` asks for one of the two.
    RcReceiver,
    /// A controller driving the vehicle has roll, pitch and yaw within the
    /// threshold of centre and its throttle no higher than that above its
    /// middle.
    Controller,
    /// The plan chosen for the flight is the one on the vehicle, read back
    /// the same, and neither has changed since. Passes with no plan chosen.
    Plan,
    /// The vehicle has loaded terrain, when its mission flies above it.
    Terrain,
    /// Every camera that records while the vehicle is armed is sending.
    Recording,
    /// Home is within the threshold of the vehicle, in metres. A return
    /// goes to home. Counts at the launch only. A home farther away fails
    /// once the vehicle is armed. Disarmed it passes, saying that most
    /// autopilots move home when they arm.
    Home,
}

/// One check in a vehicle's list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// Unique in the list: `uxduck:plan`, `vehicle:<reason>`.
    pub id: String,
    pub source: CheckSource,
    pub title: String,
    /// The concept it sits under in the list: "Plan" or "Controller" for
    /// UXDuck's own checks, a checklist step's own group ("Battery"). A
    /// step given the same group sits with them. None for the vehicle's
    /// own reasons and plugins' checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub severity: CheckSeverity,
    /// Where a checklist step counts. Left out for the vehicle's own
    /// reasons, UXDuck's own checks and plugins' checks, which are read
    /// live and count in the preflight and at the launch alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<CheckWhen>,
    pub status: CheckStatus,
    /// What UXDuck found, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<CheckFix>,
    /// Holds a launch now: set to hold, not the vehicle's, and not
    /// passing.
    pub gates: bool,
    /// The plugin a `plugin` check is from (S194), its id `plugin:<plugin
    /// id>:<check id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<CheckPlugin>,
}

/// The plugin a check comes from (S194).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckPlugin {
    pub id: String,
    pub name: String,
    /// Its blocking checks ask to hold the launch but only warn, because
    /// no person has allowed them to yet (K13).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_back: bool,
}

/// Where a vehicle's controller input may come from for "Controller input"
/// to pass: `either`, as its own failsafe takes it, `rc` for its own RC
/// receiver, or `link` for a controller through UXDuck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InputPath {
    #[default]
    Either,
    Rc,
    Link,
}

/// A check's threshold and its range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckThreshold {
    pub value: f64,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub unit: String,
    /// What the number is: "Least radio signal".
    pub label: String,
}

/// One of UXDuck's own checks as set for the vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnCheckSetting {
    pub check: OwnCheck,
    /// Its id in `checks` while on.
    pub id: String,
    pub title: String,
    pub on: bool,
    pub severity: CheckSeverity,
    pub default_severity: CheckSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<CheckThreshold>,
    /// Where its input may come from, for the check that asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// `launch` for a check of the place of the launch. Left out for the
    /// rest, which count in the preflight and at the launch alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<CheckWhen>,
}

/// A check as it stood when a command was refused for it or sent past it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckRef {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How a launch went past failing checks set to hold it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum PreflightOverride {
    /// The operator held Take off anyway, naming the checks.
    Operator,
    /// An operator allowed a client's request that listed them.
    Approval { approval_id: String },
}

/// A vehicle's preflight checks, keyed by vehicle id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub vehicle_id: String,
    /// The vehicle's own first, then UXDuck's, then the vehicle's
    /// checklist's steps, then plugins' checks by plugin.
    pub checks: Vec<Check>,
    /// Each of UXDuck's own checks as set for the vehicle, on or off.
    pub settings: Vec<OwnCheckSetting>,
    /// The plan chosen for its next flight: picked, or uploaded since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    /// False once the vehicle has launched (in the air, or armed for one
    /// that does not fly): the list is the one it launched with.
    pub evaluating: bool,
    /// The vehicle's own checklist (S193): manual steps and rules, with
    /// who ticked each and when. Its steps are in `checks` as
    /// `checklist:<id>`.
    #[serde(default)]
    pub checklist: crate::dto::checklist::Checklist,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `preflight/state` and `preflight/configure`: a vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreflightParams {
    pub vehicle_id: String,
}

/// `preflight/configure`: one of UXDuck's own checks, or the plan for the
/// next flight. What is left out stays as it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreflightConfigureParams {
    pub vehicle_id: String,
    /// The check to change; `on`, `severity`, `threshold` and `input` need
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<OwnCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<CheckSeverity>,
    /// A number in the check's range; null puts its default back.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    #[allow(clippy::option_option, reason = "absent, null or a value")]
    pub threshold: Option<Option<f64>>,
    /// Where "Controller input" may come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPath>,
    /// The plan for the next flight; null for none, flown by hand.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    #[allow(clippy::option_option, reason = "absent, null or a value")]
    pub plan_id: Option<Option<String>>,
}

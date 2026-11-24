//! UXDuck on a vehicle's companion computers on the wire (S297, ADR-0061
//! B1, B5, B9, B10): what a check found of it, how it compares with this
//! computer's, what Install, Update and Remove will do, and how the last
//! one went.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a computer's own setup says about UXDuck there, as a check reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SoftwareFound {
    /// Where `/usr/local/bin/uxduck` points, or `file` when it is a file
    /// someone put there. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The versions kept under `/opt/uxduck`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// Whether `/etc/uxduck/companion.toml` is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub config: bool,
    /// The features that file turns on: `sender`, `recorder`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// What its UXDuck offers by option, such as `config`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<String>,
    /// The systemd services that run `uxduck companion` or `uxduck forward`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<FoundUnit>,
    /// Whether the user UXDuck signs in as may run `sudo` without a password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sudo: Option<bool>,
    /// Its serial ports, for `uxduck forward`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serial: Vec<String>,
    /// Whether it runs systemd.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub systemd: bool,
    /// `getconf LONG_BIT`: 32 on a 32-bit system, even on a 64-bit kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<u8>,
}

/// A systemd service on the computer that runs UXDuck.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoundUnit {
    pub name: String,
    /// Written by UXDuck's Install.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
    /// Its `User=`. Absent, it runs as root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// It has drop-ins, which change what it runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drop_ins: bool,
    /// Its `ExecStart=`, as written.
    pub exec: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InstallAction {
    Install,
    Update,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InstallState {
    Running,
    Done,
    /// The new version did not start, and what was there before was put back.
    RolledBack,
    Failed,
}

/// One step as it ran on the computer, with what it said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallStep {
    pub title: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// The last Install, Update or Remove on a computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    pub action: InstallAction,
    pub state: InstallState,
    /// When it started or finished.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<InstallStep>,
    /// Why it failed or was put back, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// How a computer's UXDuck compares with this computer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SoftwareStatus {
    NotChecked,
    Checking,
    /// The last check could not reach it.
    Unreachable,
    NotInstalled,
    UpToDate,
    /// Older, and has every feature its `companion.toml` turns on.
    UpdateAvailable,
    /// Older, and missing a feature its `companion.toml` turns on.
    UpdateNeeded,
    /// Newer than this computer's.
    Newer,
    /// No UXDuck build runs on it, such as a 32-bit system.
    Unsupported,
    /// Its `uxduck --version` is not a version UXDuck can compare.
    Unknown,
}

/// One computer's UXDuck against this computer's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerSoftware {
    pub computer_id: String,
    pub name: String,
    pub status: SoftwareStatus,
    /// Its version, as its last check read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The features its `companion.toml` turns on that its version lacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// The status in a few words.
    pub words: String,
    /// What a person may do: Install or Update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<InstallAction>,
    /// UXDuck installed it, so Remove is offered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub managed: bool,
    /// It carries the vehicle's MAVLink, which pauses while it restarts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub carries_mavlink: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallRecord>,
}

/// `computer/software`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerSoftwareParams {
    pub vehicle_id: String,
}

/// `computer/software`: each of a vehicle's computers' UXDuck.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerSoftwareResult {
    /// This computer's UXDuck, which Install and Update put there.
    pub ground: String,
    /// Whether the vehicle is armed: Install, Update and Remove wait for a
    /// disarm.
    pub armed: bool,
    pub computers: Vec<ComputerSoftware>,
}

/// `computer/installPlan` and `computer/install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerInstallParams {
    pub vehicle_id: String,
    pub computer_id: String,
    pub action: InstallAction,
    /// Where it sends its readings, `udp:HOST:PORT`, for a computer with no
    /// UXDuck service to copy it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The serial port wired to the flight controller, `serial:DEVICE@BAUD`,
    /// for a computer that forwards its MAVLink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

/// `computer/installPlan`: exactly what it will do, before it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstallPlan {
    pub action: InstallAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The build it copies, such as `aarch64-unknown-linux-gnu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The user the program runs as.
    pub user: String,
    pub steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keeps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mavlink_pauses: bool,
    /// The service's file, whole, as it will be written.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// The polkit rule, whole, as it will be written.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub polkit: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

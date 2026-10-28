//! Installed plugins (ADR-0006). A plugin is a client: it acts under its
//! own client record, `plugin:<id>`, with the capabilities confirmed at
//! install, and contributes declarative panels the host draws.

use serde::{Deserialize, Serialize};

use crate::domain::client::Capability;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Muted,
    Warning,
}

/// What a panel shows (ADR-0006 X5); the contracts document each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Widget {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        heading: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Value {
        item: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Gauge {
        item: String,
        label: String,
        min: f64,
        max: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warn_below: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warn_above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Chart {
        item: String,
        label: String,
        window_s: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Table {
        item: String,
        label: String,
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// One item, large, to fill its tab: a clock, a stopwatch (S104).
    Display {
        item: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Button {
        label: String,
        command: String,
        /// The command's parameters, as JSON text (S101).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// A time of day to pick, sent to `command` as its next moment.
    TimeInput {
        label: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Panel {
    pub id: String,
    pub title: String,
    pub widgets: Vec<Widget>,
    /// A tab of its own, opened from "+", rather than a part of Control (S101).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tab: bool,
    /// A tab that follows a vehicle, as Telemetry does (S105).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle: bool,
}

/// What a plugin receives (ADR-0006 X1): telemetry items by key or
/// pattern, MAVLink messages by name, raw frames by id, each at a cadence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscriptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Subscription<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Subscription<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Subscription<u32>>,
    /// Called every this many milliseconds, whatever arrives (S101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_ms: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription<T> {
    pub to: Vec<T>,
    pub cadence_ms: u32,
}

/// A command a plugin offers (X10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredCommand {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Its parameters' JSON Schema, as JSON text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    pub risk: crate::domain::command::Risk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
}

/// Which agent drafted a plugin, and in which chat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WrittenBy {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Where a plugin came from (X13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum PluginOrigin {
    /// The account: installed on another of the operator's computers.
    Synced { digest: String },
    /// A link someone shared, opened here.
    Shared { shared_by: String, token: String },
}

/// A plugin in the account (X13 "Mine"): what each of the operator's
/// computers installs. Its files are kept by the API under `digest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPlugin {
    pub id: String,
    pub name: String,
    pub version: String,
    /// SHA-256 of its bundle.
    pub digest: String,
    /// What its manifest asks for.
    pub capabilities: Vec<Capability>,
    /// When it was published or removed: the newest wins.
    pub at_us: u64,
    /// Removed from the computer it came from, so from every other.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginStatus {
    Starting,
    Running,
    Failed,
    /// Turned off by an operator (S104): installed, not running.
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The directory it was installed from.
    pub path: String,
    /// The module, relative to `path`; empty for a script.
    pub wasm: String,
    /// The TypeScript or JavaScript file, relative to `path`, for a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Its vendor's dialect file, relative to `path` (S60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    pub capabilities: Vec<Capability>,
    /// The plugin ABI it was built for; plugins installed before v2 have none.
    #[serde(default = "abi_one")]
    pub abi: u32,
    #[serde(default)]
    pub subscribe: Subscriptions,
    /// The MAVLink messages it may send, by name.
    #[serde(default)]
    pub sends: Vec<String>,
    #[serde(default)]
    pub commands: Vec<DeclaredCommand>,
    /// Set when an agent drafted it (X12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_by: Option<WrittenBy>,
    /// What its manifest asks for beyond `capabilities`, waiting for an
    /// operator here (X13): a plugin synced from another computer runs
    /// with `read` until then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<Capability>,
    /// Where it came from when it was not installed from a folder here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PluginOrigin>,
    pub status: PluginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub panels: Vec<Panel>,
    /// Bumped on every change, as entity versions are.
    pub revision: u64,
    /// Whether it runs (S104): an operator turns a plugin off and on
    /// without removing it, and it stays as they left it.
    #[serde(default = "on", skip_serializing_if = "Clone::clone")]
    pub enabled: bool,
    /// Shipped with UXDuck (S104): installed from UXDuck's own copy,
    /// updated with it, turned off rather than removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub included: bool,
    /// The hosts it may fetch from over HTTPS (S170), from its manifest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// It is a traffic source, listed in Airspace on or off (S171).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic_source: bool,
    /// Its preflight checks, from its manifest (S194, ADR-0038 K12).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<DeclaredCheck>,
    /// Whether its blocking checks may stop arming (K13): a person's
    /// answer on this computer. None until someone answers; until `true`
    /// they only warn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_block: Option<bool>,
}

/// A preflight check a plugin declares (S194).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredCheck {
    pub id: String,
    pub title: String,
    pub severity: crate::domain::preflight::CheckSeverity,
    /// A report older than this no longer counts.
    pub stale_ms: u32,
}

const fn on() -> bool {
    true
}

const fn abi_one() -> u32 {
    1
}

impl Plugin {
    /// Whether it declares a check that asks to stop arming (K13).
    #[must_use]
    pub fn asks_to_block(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.severity == crate::domain::preflight::CheckSeverity::Block)
    }

    /// Whether its blocking checks stop arming: a person allowed it (K13).
    #[must_use]
    pub fn checks_may_block(&self) -> bool {
        self.checks_block == Some(true)
    }

    /// The client id its actions are journaled under.
    #[must_use]
    pub fn client_id(&self) -> String {
        client_id(&self.id)
    }
}

#[must_use]
pub fn client_id(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

/// Whether an id is lower case letters, digits and dashes.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

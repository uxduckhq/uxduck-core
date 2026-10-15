//! The MAVLink inspector: the latest message of each type a vehicle sent and
//! UXDuck sent to it, with its rate and fields, polled while the tab is open.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `mavlink/inspect`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MavlinkInspectParams {
    pub vehicle_id: String,
}

/// Which way a message went.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum InspectDirection {
    /// From the vehicle.
    #[default]
    Received,
    /// From UXDuck to the vehicle.
    Sent,
}

/// The latest message of one type, one way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectedMessage {
    pub direction: InspectDirection,
    /// The MAVLink message id.
    pub id: u32,
    /// MAVLink's name, such as `GLOBAL_POSITION_INT`; `#<id>` when this
    /// daemon does not know the message.
    pub name: String,
    pub system_id: u8,
    pub component_id: u8,
    /// Messages a second lately; 0 once they stop.
    pub hz: f64,
    /// How many since the vehicle was first heard in this run of UXDuck.
    #[schemars(schema_with = "crate::safe::integer")]
    pub count: u64,
    /// Milliseconds since the last one.
    #[schemars(schema_with = "crate::safe::integer")]
    pub age_ms: u64,
    /// The dialect whose page on mavlink.io documents it: `common` or
    /// `ardupilotmega`. None for a message neither has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// The fields by MAVLink's own names, with raw values in MAVLink's
    /// units (`GLOBAL_POSITION_INT.relative_alt` in mm). None when not
    /// decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "fields")]
    pub fields: Option<Map<String, Value>>,
}

/// An object of any values, or nothing. schemars writes `true` for each
/// value, which the TypeScript generator drops.
fn fields(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["object", "null"], "additionalProperties": {} })
}

/// `mavlink/inspect`: one entry per message type and direction, received
/// first, then by name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MavlinkInspectResult {
    /// False where this daemon cannot decode messages: messages then carry
    /// no fields. Every UXDuck decodes ArduPilot's dialect, the browser's
    /// too; a message a plugin's dialect brings decodes while the plugin
    /// is installed.
    pub decoded: bool,
    pub messages: Vec<InspectedMessage>,
}

/// `mavlink/history`: earlier messages of one type, one way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MavlinkHistoryParams {
    /// A vehicle or, for recorded packet queries, `linkId` must be supplied.
    #[serde(default)]
    pub vehicle_id: String,
    #[serde(default)]
    pub direction: InspectDirection,
    /// The MAVLink message id.
    #[serde(default)]
    pub id: u32,
    /// Query this connection, including a forward with no vehicle of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// Inclusive Unix microseconds. Detailed queries default to the last minute;
    /// at most ten minutes per query, with explicit coverage and pagination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub from_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub to_us: Option<u64>,
    /// Ignore `id` and return every message type in the selected window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_messages: bool,
    /// Include exact frame bytes as hex; omitted by default to keep answers small.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_raw: bool,
    /// A previous answer's `older`, for the page after it; none for the
    /// newest. Opaque: it stays valid while the messages it points past
    /// are kept, and one this daemon did not write is an `invalid-value`
    /// error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// How many to answer: 50 when omitted, at most 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl MavlinkHistoryParams {
    #[must_use]
    pub fn detailed(&self) -> bool {
        self.link_id.is_some()
            || self.from_us.is_some()
            || self.to_us.is_some()
            || self.all_messages
            || self.include_raw
    }
}

/// One earlier message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PastMessage {
    /// When UXDuck received or sent it, in microseconds since the Unix
    /// epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub system_id: u8,
    pub component_id: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// None for old tlogs, which did not record a peer; never inferred from
    /// the connection's current settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// FNV-1a-64 over the entire original frame, including its wire header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_hex: Option<String>,
    /// As `InspectedMessage.fields`. None when not decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "fields")]
    pub fields: Option<Map<String, Value>>,
}

/// `mavlink/history`: newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MavlinkHistoryResult {
    /// Newest first; the first may be the one the inspector shows live.
    pub messages: Vec<PastMessage>,
    /// Pass as `before` for the next page; none when nothing older is
    /// kept. From tlogs it can lead to an empty last page, when only
    /// earlier files are left and none of them holds the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older: Option<String>,
    /// True when read from the vehicle's tlogs (what it sent, in the
    /// installed UXDuck); false when kept in memory, the last 200 of each.
    pub from_log: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<super::diagnostics::DiagnosticCoverage>,
}

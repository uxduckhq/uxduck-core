//! A fork's own MAVLink messages on the wire (ADR-0044): the dialects people
//! add, kept as the messages that are new or differ from UXDuck's built-in
//! ArduPilot dialect. A vehicle uses those matching its firmware's commit, or
//! those a person chose for it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A dialect a person added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Dialect {
    pub id: String,
    pub label: String,
    /// The names of the files it was read from.
    pub files: Vec<String>,
    /// Vehicles built from any of these commits use it, unless a person
    /// chose other dialects for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    /// Every vehicle uses it, whatever was chosen; it only adds messages.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every_vehicle: bool,
    /// The vehicles using it now, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_by: Vec<String>,
    /// New and changed messages, by id.
    pub messages: Vec<DialectMessage>,
    /// Enums the messages' fields name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<DialectEnum>,
    /// `MAV_CMD` entries UXDuck's built-in dialect does not have: the
    /// commands a plan using this dialect offers by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<DialectCommand>,
    /// Messages the files had that are the same as UXDuck's built-in
    /// ArduPilot dialect, so not kept.
    pub unchanged: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub added_at_us: u64,
    /// Standard messages whose layout this dialect changes and that
    /// UXDuck reads for its own telemetry, modes, missions or parameters:
    /// from the vehicles it describes, UXDuck's own readers no longer see
    /// them (they are shown, by this dialect, in the MAVLink inspector).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<String>,
    /// The firmware profile it came with (S254): removed with it, not on
    /// its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Stored by an older UXDuck, which didn't keep which of its commands
    /// have no place, so a mission's readback still checks their frame.
    /// Adding the same files again under the same name reads it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub add_again: bool,
}

/// How a kept message differs from UXDuck's built-in dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MessageChange {
    /// An id the built-in dialect does not have.
    New,
    /// Its name or the fields before `<extensions/>` differ, so its
    /// checksum differs.
    Layout,
    /// Only the fields after `<extensions/>` differ.
    Extensions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectMessage {
    pub id: u32,
    pub name: String,
    pub change: MessageChange,
    /// The checksum's seed for this definition.
    pub crc_extra: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// In declared order; extensions last.
    pub fields: Vec<DialectField>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectField {
    pub name: String,
    /// MAVLink's type of the value or of an array's items: `uint8_t` to
    /// `int64_t`, `float`, `double` or `char`.
    pub kind: String,
    /// Items in an array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extension: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// The enum naming its values.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A command a dialect adds, as its `MAV_CMD` entry declares it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectCommand {
    pub value: u16,
    /// Such as `MAV_CMD_ACME_DROP`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Params 5 to 7 are a place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_location: bool,
    /// The file says `hasLocation="false"`: it never has a place, so a
    /// mission read back is not checked for its frame.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_location: bool,
    /// The parameters it uses; reserved ones are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<DialectCommandParam>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectCommandParam {
    /// 1 to 7.
    pub index: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectEnum {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bitmask: bool,
    pub entries: Vec<DialectEnumEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectEnumEntry {
    #[schemars(schema_with = "crate::safe::integer")]
    pub value: u64,
    pub name: String,
}

/// `mavlink/dialects/list`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectsListParams {
    /// Only the dialects describing this vehicle, in the order they apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectsListResult {
    /// Newest first, or for a vehicle, those it uses in the order they
    /// apply.
    pub dialects: Vec<Dialect>,
    /// For a vehicle: what was chosen for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<DialectChoice>,
}

/// Which dialects a vehicle uses. Those for every vehicle always apply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum DialectChoice {
    /// Those whose commits match the commit its firmware reports.
    #[default]
    Automatic,
    /// These, by id, whatever it runs; none when empty.
    Chosen { ids: Vec<String> },
}

/// `mavlink/dialects/use`: which dialects a vehicle uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectsUseParams {
    pub vehicle_id: String,
    pub choice: DialectChoice,
}

/// One MAVLink XML file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectFile {
    /// Its name, such as `acme.xml`.
    pub name: String,
    pub text: String,
}

/// `mavlink/dialects/add`: one or more MAVLink XML files, read together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectAddParams {
    /// A fork's file and any it includes that differ from ArduPilot's,
    /// such as its own `ardupilotmega.xml`.
    pub files: Vec<DialectFile>,
    /// Its name; defaults to the first file's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Used by vehicles built from these commits (7 to 40 hex characters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    /// Used by every vehicle, which only a dialect that changes no
    /// standard message may.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every_vehicle: bool,
    /// Also chosen for this vehicle, beside the dialects it uses now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// `mavlink/dialects/bind`: which vehicles use a dialect without a choice
/// made for them: those built from its commits, or every vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialectBindParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every_vehicle: bool,
}

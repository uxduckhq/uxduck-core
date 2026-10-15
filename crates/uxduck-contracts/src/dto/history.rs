//! A vehicle's history on the wire (ADR-0022): notes people write and
//! events UXDuck records. Called history, never log.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NoteCategory {
    Maintenance,
    Repair,
    Damage,
    Configuration,
    Calibration,
    Firmware,
    Test,
    Other,
}

/// What UXDuck recorded by itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "camelCase"
)]
pub enum HistoryEvent {
    FirstSeen {
        vehicle_type: String,
    },
    Renamed {
        from: String,
        to: String,
    },
    FirmwareChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    TypeChanged {
        from: String,
        to: String,
    },
    SnapshotSaved {
        snapshot_id: String,
        name: String,
    },
    /// A parameter's value changed (S74). Changes of one parameter in one
    /// sitting fold into one entry: `from` is the value before the first,
    /// `to` the value after the last.
    ParameterChanged {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<f64>,
        to: f64,
        source: ParamSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// How many changes the entry stands for, when more than one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changes: Option<u32>,
        /// From the first of them to the last, when more than one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(schema_with = "crate::safe::optional_integer")]
        span_us: Option<u64>,
    },
    /// Nobody supervised it, so UXDuck stopped its ground station heartbeat
    /// and its own failsafe took over (S81).
    SupervisionLost {
        /// What its failsafe was set to do.
        failsafe: String,
    },
    /// Someone supervises it again, and the heartbeat is back (S81).
    SupervisionResumed,
}

/// Who or what changed a parameter (S74).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ParamSource {
    /// Someone in UXDuck: the entry's author says who.
    Person,
    Agent,
    /// Another ground station: one was heard when the vehicle reported it.
    OtherStation,
    /// The vehicle, unasked: by itself, or by a ground station UXDuck does
    /// not hear.
    Vehicle,
    /// Found on a download, such as after the vehicle restarted.
    Download,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NoteVersion {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub happened_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub replaced_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    rename_all = "kebab-case",
    tag = "type",
    rename_all_fields = "camelCase"
)]
pub enum EntryKind {
    Note {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<NoteCategory>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        /// Earlier versions, oldest first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        earlier: Vec<NoteVersion>,
    },
    Event {
        event: HistoryEvent,
    },
}

/// Who wrote an entry: a person, or a client acting for one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Deletion {
    pub by: Author,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
}

/// One entry of a vehicle's history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub vehicle_id: String,
    pub kind: EntryKind,
    /// When it happened.
    #[schemars(schema_with = "crate::safe::integer")]
    pub happened_at_us: u64,
    /// When it was written; never changes.
    #[schemars(schema_with = "crate::safe::integer")]
    pub written_at_us: u64,
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub edited_at_us: Option<u64>,
    /// A deleted note stays, marked: by whom and when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Deletion>,
    /// Kept on this computer, as its vehicle is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `vehicle/history`: entries newest first, filtered and paged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleHistoryParams {
    /// With none, the only vehicle there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// `note` or `event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    /// Words in the text, any case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Only what happened since this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub since_us: Option<u64>,
    /// Deleted notes too, marked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_deleted: bool,
    /// At most this many (default 20, at most 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleHistoryResult {
    pub vehicle_id: String,
    pub vehicle_name: String,
    pub entries: Vec<HistoryEntry>,
    /// Matches in all, before paging.
    pub total: u32,
}

/// `vehicle/addNote`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddNoteParams {
    pub vehicle_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// When it happened, if not now; never later than now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub happened_at_us: Option<u64>,
}

/// `vehicle/editNote`: what changes; the rest stays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EditNoteParams {
    pub id: String,
    /// The version edited from; refused with `version-conflict` if it moved.
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<NoteCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub happened_at_us: Option<u64>,
}

/// `param/history`: changes of a vehicle's parameters, newest first (S74).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamHistoryParams {
    /// With none, the only vehicle there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// One parameter, such as `RTL_ALT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// At most this many (default 50, at most 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One change of value, with who or what made it and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamChange {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<f64>,
    pub to: f64,
    pub source: ParamSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When it changed: the first of several folded into one (`changes`).
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub by: Author,
    /// How many changes this one stands for, when more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<u32>,
    /// From the first of them to the last, when more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub span_us: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamHistoryResult {
    pub vehicle_id: String,
    pub changes: Vec<ParamChange>,
}

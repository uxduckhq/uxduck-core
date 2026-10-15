//! Threads, items and the agent (ADR-0009).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::ApiError;

#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a choice the user makes"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub model: String,
    /// Covers non-control actions only; control always needs approval.
    pub auto_approve: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
    /// The folder it is filed in; absent when unfiled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// Sorts siblings in the order the user placed them.
    pub sort_key: String,
    pub pinned: bool,
    /// Out of the sidebar; still on the management page.
    pub archived: bool,
    /// Present on a deleted thread (a tombstone). Its items stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub deleted_at_us: Option<u64>,
    /// "Don't use this chat to improve UXDuck": never kept for learning.
    pub exclude_from_learning: bool,
}

/// A folder of threads; folders nest (ADR-0009).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub sort_key: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
    /// Present on a deleted folder (a tombstone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub deleted_at_us: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FolderCreateParams {
    pub name: String,
    /// Absent for a top-level folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Changes the fields given; `parentId: null` moves it to the top level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FolderUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_id: Option<Option<String>>,
    /// Place it directly before this sibling. When absent and it moves to
    /// another folder, it goes last there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
}

/// Deletes a thread or folder as a tombstone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "action"
)]
pub enum BulkAction {
    /// To a folder, or to the top level with `folderId: null`.
    Move {
        folder_id: Option<String>,
    },
    Archive,
    Unarchive,
    Pin,
    Unpin,
    Delete,
}

/// One action on many threads from the management page. Applied to the
/// current version of each (no `expectedVersion`); deleted or unknown ids
/// are skipped and reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBulkParams {
    pub thread_ids: Vec<String>,
    #[serde(flatten)]
    pub action: BulkAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadBulkResult {
    /// The threads changed.
    pub changed: Vec<String>,
    /// Ids that were unknown or already deleted.
    pub skipped: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    ToolCall,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    /// Still in progress when the daemon stopped.
    Interrupted,
}

/// A tool call and its receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// The API method called, such as `vehicle/list`.
    pub method: String,
    #[schemars(schema_with = "crate::safe::any")]
    pub params: Value,
    /// The client that made the call: the agent's connection.
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_any")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub thread_id: String,
    /// Position in the thread, from 0.
    #[schemars(schema_with = "crate::safe::integer")]
    pub index: u64,
    pub kind: ItemKind,
    pub status: ItemStatus,
    /// Message text; for an agent message in progress, what has streamed so far.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCall>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
    /// Thumbs up or down on an agent message (S29).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Feedback>,
    /// What the operator attached to their message (ADR-0020 H2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// Something handed to the agent with a message, from an info popover's
/// "Ask the agent" (ADR-0020 H2). At most 8 on a message; the title, key,
/// value, unit and plugin up to 120 bytes each, the help up to 2000.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// What it is, such as "Battery voltage".
    pub title: String,
    /// Its telemetry key or setting, such as `battery.voltage_v`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Its value when attached, as shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The plugin that wrote its title and help, whose words the agent is
    /// told are the plugin's (H3); absent for UXDuck's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// The popover's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Rating {
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Feedback {
    pub rating: Rating,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// It reached UXDuck, with the kept turn or with the conversation.
    pub sent: bool,
}

/// Thumbs up or down on an agent message, with an optional comment
/// (ADR-0013 P13). With `send`, the conversation goes with it when its
/// turns were not kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadFeedbackParams {
    pub item_id: String,
    pub rating: Rating,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub send: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadFeedbackResult {
    /// It reached UXDuck.
    pub sent: bool,
    /// The chat's turns were not kept, so the feedback went nowhere; the
    /// app offers to send the conversation.
    pub not_shared: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCreateParams {
    /// The vehicle the thread is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Defaults to the first message, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Defaults to the daemon's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// A user message; the agent answers in the same thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSendParams {
    pub thread_id: String,
    pub text: String,
    /// Chips from the input (ADR-0020 H2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A vehicle id, or `null` for no vehicle.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub vehicle_id: Option<Option<String>>,
    /// A folder id, or `null` for unfiled.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub folder_id: Option<Option<String>>,
    /// Place it directly before this sibling. When absent and it moves to
    /// another folder, it goes last there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_learning: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCancelParams {
    pub thread_id: String,
}

/// Streamed text for an agent message in progress. Not journaled; the
/// completed item carries the whole text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentItemDelta {
    pub thread_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    /// Models the selector offers.
    pub models: Vec<String>,
    pub default_model: String,
    /// `hosted` (UXDuck's model, S29); `openai` for a developer's own OpenAI
    /// or Anthropic key in a development build; or `fake` for development and tests.
    pub provider: String,
    /// Whether the default model can answer: signed in for `hosted`, a
    /// developer's own key for `openai`.
    pub ready: bool,
    /// The models that can answer now. In a development build a model needs
    /// its company's key, or a signed-in UXDuck for the hosted model.
    #[serde(default)]
    pub ready_models: Vec<String>,
}

/// Stores a secret in the daemon's credential store. Local socket only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsSetParams {
    /// Such as `openai.key`.
    pub name: String,
    /// Empty removes it.
    pub value: String,
}

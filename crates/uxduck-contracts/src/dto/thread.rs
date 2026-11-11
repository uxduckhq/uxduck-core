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
    /// How hard the model thinks, one of `AgentConfig.efforts` for its
    /// model; absent is the model's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
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
    /// A follow-up waiting for the current turn or explicit resume.
    Queued,
    /// An update waiting to be applied to the active turn.
    Steering,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    /// Still in progress when the daemon stopped.
    Interrupted,
    /// Taken back with an operator's message they edited or stopped at once:
    /// out of the chat and the agent's history, kept as a record.
    Superseded,
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
    /// The operator said it aloud in talk mode (ADR-0012 V4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spoken: bool,
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
    /// A file uploaded with `attachment/begin` (S246); `title` is its name,
    /// up to 255 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<AttachedFile>,
}

/// A file attached to a message (S246), kept by UXDuck under its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachedFile {
    /// From `attachment/begin`.
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

/// The largest file `attachment/begin` takes: a long flight's video.
pub const ATTACHMENT_MAX: u64 = 8 * 1024 * 1024 * 1024;

/// The most `attachment/append` takes at once, before base64.
pub const ATTACHMENT_CHUNK: u64 = 4 * 1024 * 1024;

/// Starts uploading a file to attach to a message (S246). Its bytes follow
/// in order with `attachment/append`, then `attachment/finish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentBeginParams {
    /// The file's name, such as `2026-10-30 14-02-17.tlog`.
    pub name: String,
    /// Its size, up to 8 GiB.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentBeginResult {
    pub id: String,
}

/// The next of an upload's bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentAppendParams {
    pub id: String,
    /// Where these bytes go: the bytes received so far.
    #[schemars(schema_with = "crate::safe::integer")]
    pub offset: u64,
    /// Up to 4 MiB, base64.
    pub content_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentAppendResult {
    /// The bytes received so far.
    #[schemars(schema_with = "crate::safe::integer")]
    pub received: u64,
}

/// Ends an upload begun with `attachment/begin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentFinishParams {
    pub id: String,
    /// Give it up and remove what came, as removing it from the input
    /// does; only before it is sent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discard: bool,
}

/// Reads an attached file as text, as `files/read` reads a vehicle's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentReadParams {
    /// The attachment's id, from the operator's message.
    pub id: String,
    /// Where to start, in bytes; from the start when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentReadResult {
    pub name: String,
    /// The whole file's size.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// Up to 64 KB of it from `offset`; bytes that are not UTF-8 replaced.
    pub text: String,
    /// Not all of it from `offset`: read on from `offset` plus 65536.
    pub truncated: bool,
    /// It is not text, such as a tlog or a video: `text` is empty.
    pub binary: bool,
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
    /// Defaults to the model's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A user message; the agent answers in the same thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSendParams {
    pub thread_id: String,
    /// While working, queue by default; steer updates the active task.
    #[serde(default, skip_serializing_if = "FollowUpBehavior::is_queue")]
    pub behavior: FollowUpBehavior,
    pub text: String,
    /// Chips from the input (ADR-0020 H2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Said aloud in talk mode (ADR-0012 V4): the turn runs on a fast
    /// model, with the vehicle's readings, to be answered aloud (V5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken: Option<SpokenTurn>,
}

/// What starts the words of a spoken turn as the agent gets them, before
/// the vehicle's readings; the agent's instructions name it.
pub const SPOKEN_MARK: &str = "[Spoken in talk mode.";

/// A message said aloud in talk mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpokenTurn {
    /// The operator's units, which the reply reads numbers in; the page
    /// keeps them (ADR-0036).
    pub units: super::operator::QuantityUnits,
}

/// Sends new words in place of one of the operator's earlier messages: the
/// turn in progress stops, that message and everything after it leave the
/// chat and the agent's history, and the new message starts at once.
/// Follow-ups still waiting stay queued. The message's files may go again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEditParams {
    pub thread_id: String,
    /// The operator's message to replace.
    pub item_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Said aloud, as for `ThreadSendParams::spoken`: open mic's turn
    /// started early and replaced by the whole of what was said (S287).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken: Option<SpokenTurn>,
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
    /// A reasoning effort the model takes, or `null` for its default.
    /// Changing the model drops an effort the new one does not take.
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub effort: Option<Option<String>>,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCancelParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Also take back the message the turn answers when nothing has come of
    /// it: no tool call, and the agent has written nothing or began only
    /// moments ago. It leaves the chat for the operator to change and send.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub take_back: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCancelResult {
    /// The message taken back, with what was attached, for the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_back: Option<Item>,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spoken_turns: bool,
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
    /// Local provider connections; contains no credentials or authorization URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<AgentConnection>,
    /// The models whose reasoning effort a chat can choose.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<ModelEfforts>,
    /// The fast model spoken turns run on (ADR-0012 V5a); absent when none
    /// is offered, and then they run on the chat's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talk_model: Option<String>,
}

/// The reasoning efforts one model takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelEfforts {
    pub model: String,
    /// Least thinking first, such as `low`, `medium`, `high`.
    pub levels: Vec<String>,
    /// The one a chat that chooses none uses; absent when the provider does
    /// not say (Claude Code), so a chat that chooses none reads "Default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentConnection {
    pub id: String,
    pub title: String,
    pub status: ConnectionStatus,
    pub detail: String,
    pub models: Vec<ConnectedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<DeviceLogin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLogin {
    pub url: String,
    pub code: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionStatus {
    Unavailable,
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedModel {
    /// Provider-qualified id: billing never changes implicitly.
    pub id: String,
    pub title: String,
    /// The reasoning efforts the plan offers for it, least first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    /// The effort it uses when a chat chooses none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentConnectionParams {
    pub provider: String,
    pub action: ConnectionAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionAction {
    Refresh,
    Connect,
    Disconnect,
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

/// What sending during a turn means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FollowUpBehavior {
    #[default]
    Queue,
    Steer,
}

/// Manage saved follow-ups. Item operations require their current version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadQueueParams {
    pub thread_id: String,
    #[serde(flatten)]
    pub action: QueueAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum QueueAction {
    Resume,
    Edit {
        item_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        expected_version: u64,
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
    Delete {
        item_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        expected_version: u64,
    },
    Up {
        item_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        expected_version: u64,
    },
    Down {
        item_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        expected_version: u64,
    },
    Steer {
        item_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        expected_version: u64,
    },
}

impl FollowUpBehavior {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if receives a reference"
    )]
    fn is_queue(&self) -> bool {
        *self == Self::Queue
    }
}

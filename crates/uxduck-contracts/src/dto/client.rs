//! Clients, capabilities and approvals on the wire (ADR-0005).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::initialize::{Capability, ClientKind};

/// A client identity and what it may do, remembered across connections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientRecord {
    pub id: String,
    pub name: String,
    pub kind: ClientKind,
    /// The program on the other end, where the transport shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Operators always hold every capability.
    pub capabilities: Vec<Capability>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientRow {
    pub client: ClientRecord,
    /// Open connections now.
    pub connections: u32,
    pub operator: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientListResult {
    pub clients: Vec<ClientRow>,
}

/// Replaces a non-operator client's capabilities. Live: its open
/// connections get `client/capabilitiesChanged` and the next request uses
/// the new set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientSetCapabilitiesParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    pub capabilities: Vec<Capability>,
}

/// Unpairs a phone or tablet paired on the network (S97): its record and
/// session go, and its open page must pair again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientForgetParams {
    pub id: String,
}

/// What this connection may now do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitiesChanged {
    pub granted: Vec<Capability>,
}

/// A request of this connection's waits for an operator's answer. Its own
/// answer comes when the operator answers or the wait ends, so the client
/// waits that long and no shorter of its own. A client that stops waiting
/// sooner says so with `cancel`, which withdraws the request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeld {
    /// The held request's id.
    pub id: crate::dto::trace::RequestId,
    /// What is asked for, as the operator reads it after "wants to".
    pub summary: String,
    /// How long from now UXDuck waits for an answer before nothing is done.
    #[schemars(schema_with = "crate::safe::integer")]
    pub wait_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
    /// Nobody answered by the deadline: denied, the safe default.
    Expired,
}

/// How long an allowance lasts: this call, the connection, or stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Remember {
    Once,
    Connection,
    Always,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    /// The client record asking.
    pub client_id: String,
    pub client_name: String,
    pub capability: Capability,
    pub method: String,
    /// For people: "arm Copter 1".
    pub summary: String,
    /// What a person is asked, in place of "wants to" and the summary,
    /// when the request reads better as a question: "Can the assistant see
    /// Front on Copter 1? It asked for the picture now."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Launching past these checks set to hold: allowing it is the
    /// operator's override.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preflight: Vec<crate::dto::preflight::CheckRef>,
    /// The chat's tool call waiting on it, when an in-app agent asked: the
    /// chat shows its buttons on that call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The command an agent asks to run on a companion computer
    /// (`computer/run`), as a person reads it: whole, never shortened, with
    /// every character they could not otherwise see made visible (a newline
    /// as `⏎` and then the line break, a tab as `⇥`, and any other control,
    /// invisible or bidirectional character as its code point, `⟨U+202E⟩`).
    /// Show all of it before offering an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command_shown: Option<String>,
    pub state: ApprovalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember: Option<Remember>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub deadline_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalListResult {
    /// Pending approvals, oldest first.
    pub approvals: Vec<Approval>,
}

/// Answers an approval. The client that asked cannot answer its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalResolveParams {
    pub id: String,
    pub allow: bool,
    /// For an allowance; `once` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember: Option<Remember>,
}

/// A connected UI that can provide its current local display settings on request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayClient {
    /// Connection-scoped; a reconnect gets a new identity.
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayClientsResult {
    pub clients: Vec<DisplayClient>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayRegisterParams {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayPullParams {
    pub id: String,
}

/// Sent only to the selected source client; no display state is published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayRequested {
    pub request_id: String,
}

/// An opaque, versioned display-settings document, validated by the receiving UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplaySnapshot {
    pub settings: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplayReplyParams {
    pub request_id: String,
    pub settings: String,
}

/// Local tool activity. Independent of analytics, with no arguments or results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolActivityCall {
    pub id: String,
    pub session_id: String,
    pub client_id: String,
    pub source: crate::dto::analytics::ToolTraceSource,
    pub method: String,
    pub status: ToolActivityStatus,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub duration_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub approval_wait_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ToolActivityStatus {
    Running,
    WaitingForApproval,
    Completed,
    Failed,
    Cancelled,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolActivityResult {
    pub calls: Vec<ToolActivityCall>,
    /// Completed records evicted by the local memory budget. Active calls stay.
    #[schemars(schema_with = "crate::safe::integer")]
    pub discarded: u64,
    /// The memory budget for recent completed records, not a call-count cap.
    pub retained_bytes: u32,
}

//! Bounded, read-only diagnostic evidence for operators, the agent and MCP.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCoverage {
    #[schemars(schema_with = "crate::safe::integer")]
    pub requested_from_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub requested_to_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub available_from_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub available_to_us: Option<u64>,
    /// True when a retention/scan limit removed evidence, rather than just
    /// because a further page is available.
    pub truncated: bool,
    pub source: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PacketEndpoint {
    pub link_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PacketEvidence {
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub endpoint: PacketEndpoint,
    pub message_id: u32,
    pub system_id: u8,
    pub component_id: u8,
    pub sequence: u8,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_hex: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RouterDecision {
    Forwarded,
    DroppedLoop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RouterEvent {
    pub id: String,
    pub packet: PacketEvidence,
    pub decision: RouterDecision,
    pub destinations: Vec<PacketEndpoint>,
    /// At most 64 destinations are retained for one decision.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destinations_truncated: bool,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_packet: Option<PacketEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_link_id: Option<String>,
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub window_us: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RouterEventsParams {
    /// Return events involving this link as source, destination or blamed forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub from_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub to_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// 50 by default; 1–200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<RouterDecision>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_raw: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RouterEventsResult {
    pub events: Vec<RouterEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older: Option<String>,
    pub coverage: DiagnosticCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkDiagnoseParams {
    /// Exact link id/name or vehicle id/name; ambiguous names are refused.
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LinkDiagnoseResult {
    pub link: super::link::Link,
    pub vehicles: Vec<super::vehicle::Vehicle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<super::link::Link>,
    pub forwards: Vec<super::link::Link>,
    /// Relationship lists contain at most 200 entries each.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relationships_truncated: bool,
    pub routing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportDiagnostics>,
    pub recent_events: RouterEventsResult,
    /// Last preserved loop shutdown, even when outside the recent time window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_incident: Option<RouterEvent>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SocketProtocol {
    #[default]
    Udp,
    Tcp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SocketInspectParams {
    #[serde(default)]
    pub protocol: SocketProtocol,
    pub port: u16,
    /// Local IP filter, where endpoint inspection is supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SocketOwner {
    pub pid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_address: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SocketInspectResult {
    pub protocol: SocketProtocol,
    pub port: u16,
    pub owners: Vec<SocketOwner>,
    pub available: bool,
    pub complete: bool,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonLogsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub from_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub to_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// Literal substring, before redaction; never interpreted as code or regex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonLogEntry {
    #[schemars(schema_with = "crate::safe::integer")]
    pub id: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonLogsResult {
    pub entries: Vec<DaemonLogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older: Option<String>,
    pub coverage: DiagnosticCoverage,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransportDiagnostics {
    #[schemars(schema_with = "crate::safe::integer")]
    pub generation: u64,
    pub configured_destination: Option<String>,
    pub remote_endpoint: Option<String>,
    pub local_endpoint: Option<String>,
    pub wildcard_bind: bool,
    pub socket_open: bool,
    /// Confirmed by Network.framework in the daemon process, or unknown.
    pub local_network_permission: Option<String>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub local_network_checked_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_receive_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_valid_mavlink_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_heartbeat_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_send_attempt_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_socket_send_us: Option<u64>,
    pub send_error: Option<TransportError>,
    pub receive_error: Option<TransportError>,
    pub recovery_error: Option<TransportError>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub registration_attempts: u64,
    pub registration_suppressed: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub recreation_attempts: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub backoff_ms: u64,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub next_retry_us: Option<u64>,
    pub recovery_reason: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub dropped_receive_events: u64,
    /// Bounded recent socket attempts. Each record is one attempt and its
    /// outcome, never a second packet. OS acceptance is not delivery.
    pub sends: Vec<SocketSend>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransportError {
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub operation: String,
    pub kind: String,
    pub os_code: Option<i32>,
    pub endpoint: Option<String>,
    pub message: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SocketSend {
    #[schemars(schema_with = "crate::safe::integer")]
    pub id: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub generation: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub completed_us: Option<u64>,
    pub endpoint: String,
    pub registration: bool,
    pub fingerprint: String,
    pub message_id: Option<u32>,
    /// attempted, accepted-by-os, or failed. Never claims peer delivery.
    pub outcome: String,
    pub error: Option<TransportError>,
}

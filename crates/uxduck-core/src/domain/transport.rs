//! Evidence from the transport, independent of vehicle presence.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportDiagnostics {
    pub generation: u64,
    pub configured_destination: Option<String>,
    pub remote_endpoint: Option<String>,
    pub local_endpoint: Option<String>,
    pub wildcard_bind: bool,
    pub socket_open: bool,
    /// Confirmed by Network.framework in the daemon process, or unknown.
    pub local_network_permission: Option<String>,
    pub local_network_checked_us: Option<u64>,
    pub last_receive_us: Option<u64>,
    pub last_valid_mavlink_us: Option<u64>,
    pub last_heartbeat_us: Option<u64>,
    pub last_send_attempt_us: Option<u64>,
    pub last_socket_send_us: Option<u64>,
    pub send_error: Option<TransportError>,
    pub receive_error: Option<TransportError>,
    pub recovery_error: Option<TransportError>,
    pub registration_attempts: u64,
    pub registration_suppressed: bool,
    pub recreation_attempts: u64,
    pub backoff_ms: u64,
    pub next_retry_us: Option<u64>,
    pub recovery_reason: Option<String>,
    pub dropped_receive_events: u64,
    /// Bounded recent socket attempts. Each record is one attempt and its
    /// outcome, never a second packet. OS acceptance is not delivery.
    pub sends: Vec<SocketSend>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportError {
    pub at_us: u64,
    pub operation: String,
    pub kind: String,
    pub os_code: Option<i32>,
    pub endpoint: Option<String>,
    pub message: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketSend {
    /// Registration payload retained internally for packet history, never in the snapshot JSON.
    #[serde(skip)]
    pub registration_bytes: Vec<u8>,
    pub id: u64,
    pub generation: u64,
    pub at_us: u64,
    pub completed_us: Option<u64>,
    pub endpoint: String,
    pub registration: bool,
    pub fingerprint: String,
    pub message_id: Option<u32>,
    /// attempted, accepted-by-os, or failed. Never claims peer delivery.
    pub outcome: String,
    pub error: Option<TransportError>,
}

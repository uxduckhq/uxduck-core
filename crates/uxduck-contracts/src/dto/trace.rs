//! The inspector tap (ADR-0001): every frame in and out, with sensitive
//! values redacted before they leave the daemon.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC id: a number or a string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(#[schemars(schema_with = "crate::safe::signed_integer")] i64),
    String(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TraceDirection {
    /// Client to daemon.
    In,
    /// Daemon to client.
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FrameKind {
    Request,
    Response,
    Error,
    Notification,
    /// Not parseable as JSON-RPC.
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Socket,
    WebSocket,
    Stdio,
    /// A client inside the daemon, such as the local agent host.
    Internal,
    /// A page and the daemon in its own Web Worker (S20).
    MessagePort,
}

/// One frame seen by the tap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TraceFrame {
    /// Microseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub ts_us: u64,
    pub direction: TraceDirection,
    pub kind: FrameKind,
    /// The connection's id, as returned by its `initialize`; before that, a
    /// connection id the daemon assigned on accept.
    pub client_id: String,
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// Size of the frame on the wire.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// On responses: time since the matching request arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub latency_us: Option<u64>,
    /// The frame with sensitive values replaced by `"[redacted]"`.
    #[schemars(schema_with = "crate::safe::any")]
    pub payload: Value,
}

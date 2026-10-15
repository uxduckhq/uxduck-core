//! `initialize`: the handshake, frozen at protocol `v1` (ADR-0001).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::version::ReleaseStage;

/// What a client declares itself to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    OperatorUi,
    OperatorCli,
    Shell,
    Agent,
    ThirdParty,
    Exporter,
    RemoteOperator,
    /// A plugin the daemon runs (ADR-0006), never a socket client.
    Plugin,
}

/// What a client may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Read,
    Control,
    Config,
    Approve,
}

/// Who is connecting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// A stable name, for example `uxduck-web` or the MCP client's own name.
    pub name: String,
    /// The client's semantic version.
    pub version: String,
    pub kind: ClientKind,
}

/// What the client supports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub protocol_version: u32,
    /// Every protocol version the client talks, when it talks more than
    /// `protocolVersion` (ADR-0025 D4: an interface talks the current
    /// protocol and the two before it). The daemon admits a client that
    /// lists its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_protocols: Vec<u32>,
    /// Whether the client accepts binary frames on the same connection.
    #[serde(default)]
    pub binary_channels: bool,
    /// Whether the client can show approval prompts.
    #[serde(default)]
    pub approvals: bool,
    /// Notification methods the client does not render.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opt_out_notifications: Vec<String>,
}

/// Parameters of `initialize`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: ClientCapabilities,
}

/// What the daemon supports.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonCapabilities {
    pub video: bool,
    pub mcp: bool,
    /// Where it runs (ADR-0001 D16). In a browser it has no real vehicles,
    /// links, forwarding or plugins, and the UI hides them.
    #[serde(default)]
    pub runs_in: RunsIn,
    /// It starts simulated vehicles: `link/add` with a `simulator` link.
    #[serde(default)]
    pub simulator: bool,
}

/// Where a daemon runs (ADR-0001 D16).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunsIn {
    /// The installed program.
    #[default]
    Installed,
    /// A reduced daemon in the page's own Web Worker, for simulation only.
    Browser,
}

/// Result of `initialize`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub daemon_version: String,
    pub protocol_version: u32,
    /// Clients older than this get the `update-required` state.
    pub min_client_version: String,
    pub capabilities: DaemonCapabilities,
    /// The id the daemon assigned to this connection.
    pub client_id: String,
    /// The capabilities granted on admission.
    pub granted: Vec<Capability>,
    /// What this release may be used for (ADR-0013 P17).
    #[serde(default = "default_release_stage")]
    pub release_stage: ReleaseStage,
}

const fn default_release_stage() -> ReleaseStage {
    ReleaseStage::GroundTesting
}

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
    /// Someone away through the account's relay, before remote control
    /// was removed (ADR-0048); recorded clients may still say it.
    RemoteOperator,
    /// A plugin the daemon runs (ADR-0006), never a socket client.
    Plugin,
    /// UXDuck's page on a phone or tablet on this computer's network (S97),
    /// as the daemon records it; a page declares `operator-ui`.
    NetworkUi,
}

/// What a client may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Read,
    Control,
    Config,
    Approve,
    /// Reading where this computer is (S137): an agent asks first.
    Location,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent additive negotiated wire capabilities, not mutually exclusive state; separate fields and defaults preserve compatibility"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mission_read_float_bits: bool,
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
    /// It reads a marker's `shape`, `colour`, `dashed` and `outlineOnly`,
    /// and the look `building` with its `outline` and `heightM` (ADR-0052,
    /// ADR-0053). UXDuck's page from before them is told to update: it
    /// rejects a building and shows a line, an area or a circle as a point.
    #[serde(default)]
    pub marker_shapes: bool,
    /// It reads a replay mark of kind `step`, a mission step reached
    /// (S343). UXDuck's page from before it rejects the whole replay when
    /// a mark has that kind, so the daemon leaves those marks out for it.
    #[serde(default)]
    pub replay_step_marks: bool,
}

/// Parameters of `initialize`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: ClientCapabilities,
}

/// What the daemon supports.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each says whether the daemon has a feature"
)]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonCapabilities {
    pub video: bool,
    pub mcp: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mission_read_float_bits: bool,
    /// Where it runs (ADR-0001 D16). In a browser it has no real vehicles,
    /// links, forwarding or plugins, and the UI hides them.
    #[serde(default)]
    pub runs_in: RunsIn,
    /// It starts simulated vehicles: `link/add` with a `simulator` link.
    #[serde(default)]
    pub simulator: bool,
    /// A mapping may leave roll, pitch or yaw unused (`InputMapping::unused`).
    #[serde(default)]
    pub unused_controls: bool,
    /// It arms a real vehicle only for an operator who agreed to the current
    /// Terms of Service, and takes `terms/accept` (ADR-0047).
    #[serde(default)]
    pub terms: bool,
    #[serde(default)]
    pub survey_planning: bool,
    /// Terrain areas are polygons, enabled or disabled (ADR-0051 A5):
    /// `terrain/keepArea` and `terrain/areaSize` take points. It also
    /// writes them to vehicles' cards (A6 to A10): `terrain/putArea` and
    /// `settings/update`'s `vehicleTerrainAreas`. An older daemon reads
    /// as false, and neither is sent to it.
    #[serde(default)]
    pub terrain_polygons: bool,
    /// It keeps a marker's `shape`, `colour`, `dashed` and `outlineOnly`,
    /// and a building's look, `outline` and `heightM` (ADR-0052, ADR-0053).
    /// An older daemon ignores them and keeps a plain point.
    #[serde(default)]
    pub marker_shapes: bool,
    /// It sends and reads MAVLink command numbers as they are, losing none:
    /// a mission item's `MAV_CMD` and `MAV_FRAME`, and the command and
    /// frame of `COMMAND_LONG` and `COMMAND_INT`. It does not mean the
    /// autopilot accepts them.
    #[serde(default)]
    pub raw_mission_numbers: bool,
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

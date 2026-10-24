//! Daemon settings, storage and restarts on the wire (ADR-0007).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClosePolicy {
    KeepRunning,
    /// Quit after a short grace period, unless a keep-alive link or a
    /// headless job needs the daemon.
    Quit,
}

/// What opening UXDuck from its app icon does; not MCP or login startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchAction {
    #[default]
    Window,
    Browser,
    MenuBar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StorageCategory {
    Tlogs,
    EventLog,
    Database,
    Backups,
    Logs,
    Plugins,
    Video,
    /// Downloaded simulator builds (S17, S18).
    Simulators,
    SimulatorLogs,
    SimulatorData,
    /// Parameter definition sets, downloaded and added (S67).
    Parameters,
    /// Copies of vehicles' files, by vehicle (S121).
    VehicleFiles,
    /// Optional demo footage, removed explicitly from the catalogue.
    DemoVideos,
}

/// Delete a category's files older than `days`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoDelete {
    pub category: StorageCategory,
    pub days: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PilotInputMethod {
    #[default]
    Automatic,
    ManualControl,
    RcChannelsOverride,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PilotInputSetting {
    pub vehicle_id: String,
    pub method: PilotInputMethod,
}

/// The daemon's settings; one entity, id `daemon`.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a choice the user makes"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Absent on older daemons. This computer receives beta and stable releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_beta: Option<bool>,
    /// What happens when the last window closes, as set here or, when never
    /// set, on the command line.
    pub close_policy: ClosePolicy,
    /// Whether `closePolicy` was set here (otherwise it is the command
    /// line's).
    pub close_policy_set: bool,
    /// Age-based deletion policies; the simulator-log size budget is independent.
    pub auto_delete: Vec<AutoDelete>,
    /// Total simulator log budget in bytes; zero disables it. Absent on older hosts.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulator_log_max_bytes: Option<u64>,

    /// The desktop shell that registered itself; `uxduck open` starts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    /// Absent on older daemons, so a newer interface can ask for an update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_action: Option<LaunchAction>,
    /// Usage analytics (Settings > Data): on until switched off.
    pub usage_analytics: bool,
    /// Whether conversations may be kept to improve the agent.
    pub share_conversations: bool,
    /// Whether flight data may be shared, flight by flight.
    pub share_flight_data: bool,
    /// UDP ports UXDuck listens on for video besides its own (5600), added
    /// by hand.
    #[serde(default)]
    pub video_ports: Vec<u16>,
    /// When cameras are recorded.
    #[serde(default)]
    pub video_recording: crate::dto::video::RecordPolicy,
    /// The tray icon while UXDuck runs on a desktop.
    #[serde(default = "on")]
    pub show_in_menu_bar: bool,
    /// "Connect this computer to my account": while signed in, the
    /// account sees it online (S78).
    #[serde(default = "on")]
    pub connect_to_account: bool,
    /// "Let my account watch this computer": the vehicles someone of the
    /// same account watches, live, while they watch (S79).
    #[serde(default = "on")]
    pub watch_from_account: bool,
    /// "Allow remote control from my account": the command classes someone
    /// of the same account may send from away (S80). Empty is off.
    #[serde(default)]
    pub remote_control: Vec<crate::dto::command::CommandClass>,
    /// Legacy first selected network address (S97). Absent is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_address: Option<String>,
    /// All selected network addresses. Empty is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_addresses: Vec<String>,
    /// Seconds a vehicle may go unsupervised before UXDuck stops its ground
    /// station heartbeat to it, so its own failsafe acts (S81).
    #[serde(default = "grace")]
    pub supervision_grace_s: u32,
    /// The MAVLink system id UXDuck sends as, 1 to 255; 255 by default. A
    /// vehicle's `SYSID_MYGCS` counts only this id's heartbeats.
    #[serde(default = "gcs_system_id")]
    pub gcs_system_id: u8,
    /// "Store flights and recordings in my account" (S82).
    #[serde(default = "on")]
    pub store_flights: bool,
    /// "Save every flight to my account" (C9).
    #[serde(default)]
    pub save_flights: bool,
    /// Uploads go on while a vehicle is armed (S82).
    #[serde(default = "on")]
    pub upload_while_armed: bool,
    /// A recording's segments go while it records; off, once it ends (S83).
    #[serde(default)]
    pub upload_video_while_flying: bool,
    /// UXDuck opens at login (S108): as the system says when it was asked,
    /// else as last set here.
    #[serde(default)]
    pub start_at_login: bool,
    /// What the system says of opening at login, when UXDuck could ask it:
    /// on, off (perhaps switched off there), waiting for the person's OK in
    /// System Settings, or not possible here and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_item: Option<LoginItem>,
    /// A vehicle message this severe or more becomes a notification: 0
    /// emergency to 7 debug; 4, warnings, by default.
    #[serde(default = "warning")]
    pub message_notify_severity: u8,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pilot_input_methods: Vec<PilotInputSetting>,
    /// The last run's simulations start again when UXDuck starts; off, as
    /// it starts, they are removed.
    #[serde(default)]
    pub recreate_simulations: bool,
    /// Settings › Airspace (S174): traffic this near an armed vehicle,
    /// across, raises a warning. 2 km when an older daemon left it out.
    #[serde(default = "traffic_alert_m")]
    pub traffic_alert_m: u32,
    /// And this near up or down: 300 m when left out.
    #[serde(default = "traffic_alert_up_m")]
    pub traffic_alert_up_m: u32,
    /// Settings › Terrain: the dataset the map reads and vehicles are
    /// answered from, unless one is chosen for a vehicle (S23).
    #[serde(default)]
    pub terrain_dataset: crate::dto::terrain::TerrainDataset,
    /// Datasets chosen for single vehicles (S119); the rest use the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_terrain_datasets: Vec<crate::dto::terrain::VehicleTerrainDataset>,
    /// Areas whose terrain is kept offline (S120), changed with
    /// `terrain/keepArea` and `terrain/removeArea`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terrain_areas: Vec<crate::dto::terrain::TerrainArea>,
}

const fn on() -> bool {
    true
}

const fn grace() -> u32 {
    10
}

const fn gcs_system_id() -> u8 {
    255
}

const fn warning() -> u8 {
    4
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUpdateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_beta: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_action: Option<LaunchAction>,
    /// Set one vehicle's pilot input method. Requires its controller released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pilot_input: Option<PilotInputSetting>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_policy: Option<ClosePolicy>,
    /// Replaces the whole list when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_delete: Option<Vec<AutoDelete>>,
    /// Total simulator log budget in bytes; zero disables it. Absent on older hosts.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulator_log_max_bytes: Option<u64>,

    /// The desktop shell registering itself; an empty string forgets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_analytics: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_conversations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_flight_data: Option<bool>,
    /// Replaces the whole list when present; at most eight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_ports: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_recording: Option<crate::dto::video::RecordPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_in_menu_bar: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_to_account: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_from_account: Option<bool>,
    /// Replaces the classes; only from this computer, never from away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_control: Option<Vec<crate::dto::command::CommandClass>>,
    /// One of this computer's addresses to also listen on, for phones and
    /// tablets on that network (S97); an empty string turns it off. Only
    /// from this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_address: Option<String>,
    /// Replaces the selected addresses; empty turns off all network listeners.
    /// Only from this computer. Cannot be combined with networkAddress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_addresses: Option<Vec<String>>,
    /// 5 to 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision_grace_s: Option<u32>,
    /// 1 to 255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcs_system_id: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_flights: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_flights: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_while_armed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_video_while_flying: Option<bool>,
    /// Registers UXDuck to start at login, or removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at_login: Option<bool>,
    /// 0 to 7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_notify_severity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recreate_simulations: Option<bool>,
    /// 100 m to 20 km (S174).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_alert_m: Option<u32>,
    /// 30 m to 3 km (S174).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_alert_up_m: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terrain_dataset: Option<crate::dto::terrain::TerrainDataset>,
    /// Chooses one vehicle's dataset, or gives it back to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_terrain: Option<crate::dto::terrain::VehicleTerrainDataset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CategoryUsage {
    pub category: StorageCategory,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    pub files: u32,
    /// Whether a policy may delete its old files.
    pub deletable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsage {
    pub data_dir: String,
    pub categories: Vec<CategoryUsage>,
    /// Native simulator logs, by simulation; absent on older hosts.
    #[serde(default)]
    pub simulator_logs: Vec<SimulatorLogUsage>,
    /// Free and total space on the data directory's disk, where known.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Flights waiting to be stored in the account (S82).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<UploadStatus>,
}

/// What is waiting to go to the account, and why it is not going, if it is not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadStatus {
    #[schemars(schema_with = "crate::safe::integer")]
    pub chunks: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// What the account keeps of this person's, flights and recordings
    /// from all their computers, in bytes (S83); unknown until asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub stored_bytes: Option<u64>,
}

/// How often each of the daemon's tasks has woken since it started (S24),
/// so an idle daemon that is not idle can be traced to its cause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Wakeups {
    /// Microseconds since the Unix epoch when counting began.
    #[schemars(schema_with = "crate::safe::integer")]
    pub since_us: u64,
    /// Most first.
    pub tasks: Vec<TaskWakeups>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskWakeups {
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub count: u64,
}

/// Which UXDuck answers and how it was built (S87): `daemon/about`, for the
/// About page and bug reports. The handshake is frozen, so build details
/// come here instead. UXDuck in the browser answers with its wasm build's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonAbout {
    /// Its version, as the handshake's `daemonVersion`.
    pub version: String,
    pub runs_in: crate::dto::initialize::RunsIn,
    pub protocol_version: u32,
    pub release_stage: crate::version::ReleaseStage,
    /// The commit it was built from, short; none when it was built outside
    /// a git checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// When it was built, as RFC 3339 in UTC; none when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
    /// The target it was built for, such as `aarch64-apple-darwin` or
    /// `wasm32-unknown-unknown`.
    pub target: String,
    /// Whether it is a development build (debug assertions on).
    #[serde(default)]
    pub dev: bool,
    /// Where it keeps its data; none in the browser, which keeps it in the
    /// page's storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    /// The port it listens on for browsers; none in the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Microseconds since it started.
    #[schemars(schema_with = "crate::safe::integer")]
    pub uptime_us: u64,
}

/// Restarts the daemon: a final checkpoint, then the same program starts
/// again in its place. Needs `confirm` while any vehicle is armed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRestartParams {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// Opening UXDuck at login, as the system has it (S108, ADR-0025 D5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoginItem {
    pub state: LoginItemState,
    /// Why, in words: "Switched off in Task Manager › Startup apps.", or
    /// what makes it impossible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LoginItemState {
    On,
    Off,
    /// macOS registered it but waits for the person to allow it in System
    /// Settings › General › Login Items.
    NeedsApproval,
    /// Not on this computer, such as a build not installed as an app.
    Unavailable,
}

/// Logs of one native simulation. Running simulations cannot be cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimulatorLogUsage {
    pub simulation_id: String,
    /// Its vehicle's name when still known to UXDuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    pub files: u32,
    pub running: bool,
}

const fn traffic_alert_m() -> u32 {
    2_000
}

const fn traffic_alert_up_m() -> u32 {
    300
}

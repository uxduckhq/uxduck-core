//! Daemon-wide settings an operator changes from any client (ADR-0007
//! storage policy; ADR-0001 shutdown policy).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How UXDuck sends a controller's pilot axes. Automatic preserves UXDuck's
/// existing family defaults; it is not a recommendation by the firmware.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PilotInputMethod {
    #[default]
    Automatic,
    ManualControl,
    RcChannelsOverride,
}

impl PilotInputMethod {
    #[must_use]
    pub fn effective(self, family: Option<crate::domain::modes::Family>) -> Self {
        use crate::domain::modes::Family;
        match self {
            Self::Automatic => match family {
                Some(Family::Copter | Family::Plane | Family::Rover | Family::Blimp) => {
                    Self::RcChannelsOverride
                }
                _ => Self::ManualControl,
            },
            explicit => explicit,
        }
    }
}

/// What the daemon does when the last window closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClosePolicy {
    KeepRunning,
    /// Quit after a short grace period, unless a keep-alive link or a
    /// headless job needs the daemon.
    Quit,
}

/// What a deliberate app launch opens. Background startup never uses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchAction {
    #[default]
    Window,
    Browser,
    MenuBar,
}

/// Where the daemon's disk space goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageCategory {
    /// Raw MAVLink, byte for byte.
    Tlogs,
    /// The journal of every change.
    EventLog,
    /// Control state, projections and the checkpoint.
    Database,
    /// Backups taken before migrations.
    Backups,
    Logs,
    Plugins,
    Video,
    /// Downloaded simulator builds (S17, S18).
    Simulators,
    /// Flight and process logs, separate from builds and saved state.
    SimulatorLogs,
    /// Saved parameters, missions and other simulator working files.
    SimulatorData,
    /// Parameter definition sets, downloaded and added (S67).
    Parameters,
    /// Copies of vehicles' files, by vehicle (S121, ADR-0027 F5).
    VehicleFiles,
    /// Optional demo footage, removed explicitly from the catalogue.
    DemoVideos,
}

impl StorageCategory {
    pub const ALL: [Self; 13] = [
        Self::Tlogs,
        Self::EventLog,
        Self::Database,
        Self::Backups,
        Self::Logs,
        Self::Plugins,
        Self::Video,
        Self::Simulators,
        Self::Parameters,
        Self::VehicleFiles,
        Self::DemoVideos,
        Self::SimulatorLogs,
        Self::SimulatorData,
    ];

    /// Whether old files may be deleted by a policy. The event log and the
    /// database are the record itself and never are.
    #[must_use]
    pub const fn deletable(self) -> bool {
        matches!(
            self,
            Self::Tlogs | Self::Backups | Self::Logs | Self::Video | Self::SimulatorLogs
        )
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a choice the user makes"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Receive newer beta releases as well as stable releases on this computer.
    #[serde(default)]
    pub include_beta: bool,
    /// Unset: the command line's choice (`--on-last-client-closed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_policy: Option<ClosePolicy>,
    /// Optional age policies for ordinary storage categories (ADR-0007).
    /// Simulator log retention is separate so legacy settings and an
    /// explicit no-age-limit choice remain distinguishable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auto_delete_days: BTreeMap<StorageCategory, u32>,
    /// Missing in older checkpoints means seven days; null explicitly keeps logs.
    /// Projected through autoDelete on the wire, so Keep survives restarts.
    #[serde(default = "simulator_log_retention")]
    pub simulator_log_retention_days: Option<u32>,
    /// A total log budget; zero explicitly disables the size limit.
    #[serde(default = "simulator_log_budget")]
    pub simulator_log_max_bytes: u64,
    /// The desktop shell, once it has registered: `uxduck open` starts it
    /// instead of a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(default)]
    pub launch_action: LaunchAction,
    /// Usage analytics (ADR-0013 P1, P2): on until switched off. Off, the
    /// daemon keeps no install id and sends no events (ADR-0016 O15).
    #[serde(default = "on")]
    pub usage_analytics: bool,
    /// Conversations may be kept to improve the agent (S29): on until
    /// switched off. Each chat can also say no.
    #[serde(default = "on")]
    pub share_conversations: bool,
    /// Flight data is shared only when this is on, flight by flight.
    #[serde(default)]
    pub share_flight_data: bool,
    /// UDP ports UXDuck listens on for video besides `--video-port`, added
    /// by hand (S39).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_ports: Vec<u16>,
    /// When cameras are recorded (S40).
    #[serde(default)]
    pub video_recording: crate::domain::video::RecordPolicy,
    /// The tray icon while UXDuck runs on a desktop (S64): on until
    /// switched off.
    #[serde(default = "on")]
    pub show_in_menu_bar: bool,
    /// UXDuck registered as a user service that starts at login (S64): off
    /// until the person turns it on.
    #[serde(default)]
    pub start_at_login: bool,
    /// A vehicle message this severe or more (`MAV_SEVERITY`, 0 most) also
    /// becomes a notification (S26); warnings by default.
    #[serde(default = "warning")]
    pub message_notify_severity: u8,
    /// Whether starting at login was suggested once, when a forward link was
    /// added.
    #[serde(default)]
    pub start_at_login_suggested: bool,
    /// "Connect this computer to my account" (S78, ADR-0003): while signed
    /// in, the account sees it online. On until switched off; signed out,
    /// nothing leaves.
    #[serde(default = "on")]
    pub connect_to_account: bool,
    /// "Let my account watch this computer" (S79, ADR-0003 open decision
    /// 4): live values of the vehicles someone signed in to the same
    /// account is watching, only while they watch. On until switched off.
    #[serde(default = "on")]
    pub watch_from_account: bool,
    /// "Allow remote control from my account" (S80, ADR-0003 C9): the
    /// command classes someone signed in to the same account may send from
    /// away. Empty, as it starts, is off; set only at this computer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_control: Vec<crate::domain::command::CommandClass>,
    /// Legacy first network address (S97), retained for old settings and clients.
    /// Set only at this computer, alongside `network_addresses`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_address: Option<String>,
    /// Explicitly selected network addresses; empty uses the legacy address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_addresses: Vec<String>,
    /// How long a vehicle may go unsupervised before UXDuck stops its
    /// ground station heartbeat to it (S81, ADR-0003 C16), in seconds.
    #[serde(default = "grace")]
    pub supervision_grace_s: u32,
    /// The MAVLink system id UXDuck sends as, 1 to 255: 255, the usual
    /// ground station and `uxduck_mavlink::GCS_SYSTEM_ID`, until changed.
    /// A vehicle's `SYSID_MYGCS` counts only this id's heartbeats.
    #[serde(default = "gcs_system_id")]
    pub gcs_system_id: u8,
    /// "Store flights and recordings in my account" (S82, ADR-0003 C17):
    /// while signed in, each flight's tlog goes to the account as it is
    /// recorded. Off until the person turns it on (maintainer, 2026-10-24:
    /// opt-in, offered when setting up); signed out, nothing leaves. Kept
    /// under a new name so the old default, on, which nobody chose, is
    /// not carried over.
    #[serde(rename = "uploadFlightLogs", default)]
    pub store_flights: bool,
    /// "Save every flight to my account" (C9): each real flight is kept as it
    /// ends instead of suggested. Off, as it starts (maintainer, 2026-10-23:
    /// flight data off by default).
    #[serde(default)]
    pub save_flights: bool,
    /// Uploads go on while a vehicle is armed; off keeps the link for
    /// flying until it lands (S82).
    #[serde(default = "on")]
    pub upload_while_armed: bool,
    /// "Upload video while flying" (S83, ADR-0003 open decision 7): a
    /// recording's segments go while it is still recording. Off, as it
    /// starts, they go once it ends, which is after landing.
    #[serde(default)]
    pub upload_video_while_flying: bool,
    /// Per-vehicle choices on this computer, keyed by vehicle identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pilot_input_methods: BTreeMap<String, PilotInputMethod>,
    /// "Recreate simulations when UXDuck starts" (maintainer, 2026-10-11):
    /// on, the last run's simulations start again afresh; off, as it
    /// starts, they are not remembered and go with their vehicles.
    #[serde(default)]
    pub recreate_simulations: bool,
    /// Settings › Airspace (S174, ADR-0034): traffic this near an armed
    /// vehicle across, and this near up or down, raises a warning.
    #[serde(default = "traffic_alert_m")]
    pub traffic_alert_m: u32,
    #[serde(default = "traffic_alert_up_m")]
    pub traffic_alert_up_m: u32,
    #[serde(default)]
    pub traffic_controls: TrafficControls,
    /// Settings › Terrain (S23, ADR-0026 T4): the dataset the map reads
    /// and vehicles are answered from, unless one is chosen for a vehicle.
    #[serde(default)]
    pub terrain_dataset: TerrainDataset,
    /// Datasets chosen for single vehicles, by vehicle id (S119).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vehicle_terrain_datasets: BTreeMap<String, TerrainDataset>,
    /// Areas whose terrain is kept offline (S120).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terrain_areas: Vec<TerrainArea>,
    pub version: u64,
}

/// An area whose terrain is kept offline (S120, ADR-0026 T6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerrainArea {
    pub id: String,
    pub name: String,
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
}

/// Areas are compared as written; their corners are never NaN.
impl Eq for TerrainArea {}

/// A terrain dataset (ADR-0026 T1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerrainDataset {
    /// JAXA's AW3D30, as ArduPilot's SRTM1: the default.
    #[default]
    Alos,
    /// Copernicus GLO-30, as QGroundControl serves.
    Copernicus,
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            include_beta: false,
            close_policy: None,
            auto_delete_days: BTreeMap::new(),
            simulator_log_retention_days: simulator_log_retention(),
            simulator_log_max_bytes: simulator_log_budget(),
            shell_path: None,
            launch_action: LaunchAction::default(),
            usage_analytics: true,
            share_conversations: true,
            share_flight_data: false,
            video_ports: Vec::new(),
            video_recording: crate::domain::video::RecordPolicy::default(),
            show_in_menu_bar: true,
            start_at_login: false,
            start_at_login_suggested: false,
            message_notify_severity: 4,
            connect_to_account: true,
            watch_from_account: true,
            remote_control: Vec::new(),
            network_address: None,
            network_addresses: Vec::new(),
            supervision_grace_s: 10,
            gcs_system_id: 255,
            store_flights: false,
            save_flights: false,
            upload_while_armed: true,
            upload_video_while_flying: false,
            pilot_input_methods: BTreeMap::new(),
            recreate_simulations: false,
            traffic_alert_m: traffic_alert_m(),
            traffic_alert_up_m: traffic_alert_up_m(),
            traffic_controls: TrafficControls::default(),
            terrain_dataset: TerrainDataset::Alos,
            vehicle_terrain_datasets: BTreeMap::new(),
            terrain_areas: Vec::new(),
            version: 0,
        }
    }
}

const fn simulator_log_budget() -> u64 {
    5_000_000_000
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Serde needs Some(7) for an absent field, while explicit null means no age limit"
)]
const fn simulator_log_retention() -> Option<u32> {
    Some(7)
}

/// 2 km across (S174).
const fn traffic_alert_m() -> u32 {
    2_000
}

/// 300 m up or down (S174).
const fn traffic_alert_up_m() -> u32 {
    300
}

#[cfg(test)]
mod simulator_retention_tests {
    use super::*;

    #[test]
    fn old_settings_default_to_seven_days_and_explicit_keep_survives_serialization() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("simulatorLogRetentionDays");
        value
            .as_object_mut()
            .unwrap()
            .remove("simulatorLogMaxBytes");
        let old: Settings = serde_json::from_value(value).unwrap();
        assert_eq!(old.simulator_log_retention_days, Some(7));
        assert_eq!(old.simulator_log_max_bytes, 5_000_000_000);
        let mut keep = old;
        keep.simulator_log_retention_days = None;
        keep.simulator_log_max_bytes = 0;
        let reloaded: Settings =
            serde_json::from_value(serde_json::to_value(keep).unwrap()).unwrap();
        assert_eq!(reloaded.simulator_log_retention_days, None);
        assert_eq!(reloaded.simulator_log_max_bytes, 0);
    }
}

/// Traffic choices are explicit and never depend on arming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent operator-selected switches"
)]
pub struct TrafficControls {
    pub range_m: u32,
    pub limit_range: bool,
    pub height_m: u32,
    pub limit_height: bool,
    pub all_vehicles: bool,
    pub vehicle_ids: Vec<String>,
    pub device_location: bool,
    pub sites: Vec<TrafficSite>,
    pub always_label: bool,
    pub refresh_s: u32,
    pub alerts_enabled: bool,
}
impl Default for TrafficControls {
    fn default() -> Self {
        Self {
            range_m: 10_000,
            limit_range: true,
            height_m: 1_000,
            limit_height: true,
            all_vehicles: true,
            vehicle_ids: Vec::new(),
            device_location: false,
            sites: Vec::new(),
            always_label: false,
            refresh_s: 30,
            alerts_enabled: true,
        }
    }
}
/// A chosen fixed ground location; altitude is optional, in metres above sea level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficSite {
    pub id: String,
    pub name: String,
    pub latitude_e7: i32,
    pub longitude_e7: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<i32>,
}

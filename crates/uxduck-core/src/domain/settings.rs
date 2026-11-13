//! Daemon-wide settings an operator changes from any client (ADR-0007
//! storage policy; ADR-0001 shutdown policy).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::plan::LatLon;

/// Simulator logs are kept under this until the operator chooses storage
/// limits (maintainer, 2026-11-03).
pub const DEFAULT_SIMULATOR_LOG_MAX_BYTES: u64 = 5_000_000_000;

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
    /// database are the record itself, and telemetry logs and recordings
    /// are the operator's own, so none of them ever are.
    #[must_use]
    pub const fn deletable(self) -> bool {
        matches!(self, Self::Backups | Self::Logs | Self::SimulatorLogs)
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
    /// Age policies explicitly chosen by the operator; empty keeps files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auto_delete_days: BTreeMap<StorageCategory, u32>,
    #[serde(default)]
    pub simulator_log_retention_days: Option<u32>,
    #[serde(default)]
    pub simulator_log_max_bytes: u64,
    /// Distinguishes an explicit choice from legacy automatic defaults.
    /// Legacy simulator defaults are reset once; custom choices are preserved.
    /// Until then, simulator logs are kept under the default limit.
    #[serde(default)]
    pub storage_cleanup_chosen: bool,
    /// The first cleanup of simulator logs was told; later ones are quiet
    /// (maintainer, 2026-11-03).
    #[serde(default)]
    pub simulator_cleanup_told: bool,
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
    /// How a camera that sends no IDR is re-encoded for live view.
    #[serde(default)]
    pub video_reencoder: crate::domain::video::Reencoder,
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
    /// "Live sharing from this computer" (ADR-0048): while signed in, it
    /// keeps its connection to the relay and watch links show its vehicles
    /// live while someone watches. On until switched off; signed out,
    /// nothing leaves. Was "Let my account watch this computer" (S79).
    #[serde(default = "on", alias = "watchFromAccount")]
    pub live_sharing: bool,
    /// "Connect this computer to my account" (S78), read from settings
    /// written before it joined live sharing: off turns live sharing off
    /// once (`merge_live_sharing`), then it is gone.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub connect_to_account: bool,
    /// This computer's watch links must end (S95, W9, UXD-44): set when
    /// live sharing goes off or the computer signs out, cleared only once
    /// the account says they ended. While set, nothing is published to the
    /// relay, whatever the setting says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch_links_end_pending: bool,
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
    /// Shared connection preference. Unknown and new links follow in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connection_order: Vec<String>,
    /// "Store flights and recordings in my account" (S82, ADR-0003 C17):
    /// while signed in, each flight's tlog goes to the account as it is
    /// recorded. Off until the person turns it on (maintainer, 2026-10-24:
    /// opt-in, offered when setting up); signed out, nothing leaves. Kept
    /// under a new name so the old default, on, which nobody chose, is
    /// not carried over.
    #[serde(rename = "uploadFlightLogs", default)]
    pub store_flights: bool,
    /// "Add every flight to my logbook" (C9): each real flight is kept as it
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
    /// "What to sync" (ADR-0046 B11): what goes with a flight synced to
    /// the account, automatically or by hand. Its summary and telemetry
    /// log until changed.
    #[serde(default)]
    pub sync_content: SyncContent,
    /// "Wait for unmetered networks" (B12): telemetry logs and video wait
    /// while this computer's connection is metered. On until switched off.
    #[serde(default = "on")]
    pub wait_for_unmetered: bool,
    /// The two old switches were made one (ADR-0046 B8), once. A new
    /// computer starts merged; settings saved before read as not yet.
    #[serde(default, alias = "backupsMerged")]
    pub sync_merged: bool,
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
    /// Vehicles that never get areas written to them ("Keep areas off this
    /// vehicle", ADR-0051 A7), by vehicle id.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub vehicles_without_terrain_areas: BTreeSet<String>,
    /// What UXDuck wrote to vehicles' cards and the vehicles acknowledged
    /// (ADR-0051 A6, A8): kept so it is not written again, is shown as on
    /// the vehicle, and can be removed from it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terrain_on_vehicles: Vec<TerrainOnVehicle>,
    pub version: u64,
}

/// An area's blocks on a vehicle's card, as UXDuck wrote them (ADR-0051
/// A6): the area as it was then, the grid, the dataset and release the
/// heights came from, and every block the vehicle acknowledged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerrainOnVehicle {
    pub vehicle_id: String,
    pub area_id: String,
    /// The area's corners when it was written.
    pub points: Vec<LatLon>,
    pub spacing_m: u16,
    pub dataset: TerrainDataset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Blocks written, by degree file (`S36E149`): `grid_idx_x`,
    /// `grid_idx_y`.
    pub blocks: BTreeMap<String, Vec<(u16, u16)>>,
    /// Blocks left off because a height was missing.
    #[serde(default)]
    pub skipped: u32,
    /// Every block of the area is on the card, or was skipped.
    #[serde(default)]
    pub complete: bool,
    pub written_at_us: u64,
    /// The person removed it from the vehicle (A10): not written there
    /// again until they put it there.
    #[serde(default)]
    pub removed: bool,
}

/// Corners and blocks are compared as written; never NaN.
impl Eq for TerrainOnVehicle {}

/// An area whose terrain is kept offline (S120, ADR-0026 T6): a polygon
/// drawn on the map (ADR-0051 A5). Kept on this computer while enabled; a
/// disabled one is still drawn, and not kept.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredTerrainArea")]
pub struct TerrainArea {
    pub id: String,
    pub name: String,
    /// Its corners in order; the last joins the first.
    pub points: Vec<LatLon>,
    pub enabled: bool,
}

/// Areas are compared as written; their corners are never NaN.
impl Eq for TerrainArea {}

/// An area as stored: a polygon, or a rectangle from before ADR-0051,
/// read as its four corners.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTerrainArea {
    id: String,
    name: String,
    #[serde(default)]
    points: Vec<LatLon>,
    #[serde(default = "on")]
    enabled: bool,
    #[serde(default)]
    south_deg: Option<f64>,
    #[serde(default)]
    west_deg: Option<f64>,
    #[serde(default)]
    north_deg: Option<f64>,
    #[serde(default)]
    east_deg: Option<f64>,
}

impl From<StoredTerrainArea> for TerrainArea {
    fn from(stored: StoredTerrainArea) -> Self {
        let corner = |latitude_deg, longitude_deg| LatLon {
            latitude_deg,
            longitude_deg,
        };
        let points = match (
            stored.points.is_empty(),
            stored.south_deg,
            stored.west_deg,
            stored.north_deg,
            stored.east_deg,
        ) {
            (true, Some(s), Some(w), Some(n), Some(e)) => {
                vec![corner(s, w), corner(s, e), corner(n, e), corner(n, w)]
            }
            _ => stored.points,
        };
        Self {
            id: stored.id,
            name: stored.name,
            points,
            enabled: stored.enabled,
        }
    }
}

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

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
const fn is_on(value: &bool) -> bool {
    *value
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
            simulator_log_retention_days: None,
            simulator_log_max_bytes: 0,
            storage_cleanup_chosen: false,
            simulator_cleanup_told: false,
            shell_path: None,
            launch_action: LaunchAction::default(),
            usage_analytics: true,
            share_conversations: true,
            share_flight_data: false,
            video_ports: Vec::new(),
            video_recording: crate::domain::video::RecordPolicy::default(),
            video_reencoder: crate::domain::video::Reencoder::default(),
            show_in_menu_bar: true,
            start_at_login: false,
            start_at_login_suggested: false,
            message_notify_severity: 4,
            live_sharing: true,
            connect_to_account: true,
            watch_links_end_pending: false,
            network_address: None,
            network_addresses: Vec::new(),
            supervision_grace_s: 10,
            connection_order: Vec::new(),
            gcs_system_id: 255,
            store_flights: false,
            save_flights: false,
            upload_while_armed: true,
            upload_video_while_flying: false,
            sync_content: SyncContent::default(),
            wait_for_unmetered: true,
            sync_merged: true,
            pilot_input_methods: BTreeMap::new(),
            recreate_simulations: false,
            traffic_alert_m: traffic_alert_m(),
            traffic_alert_up_m: traffic_alert_up_m(),
            traffic_controls: TrafficControls::default(),
            terrain_dataset: TerrainDataset::Alos,
            vehicle_terrain_datasets: BTreeMap::new(),
            terrain_areas: Vec::new(),
            vehicles_without_terrain_areas: BTreeSet::new(),
            terrain_on_vehicles: Vec::new(),
            version: 0,
        }
    }
}

/// What a synced flight takes to the account (ADR-0046 B11), each level
/// with everything before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncContent {
    /// Its summary: times, height, modes, who flew; about 1 KB.
    Summaries,
    /// Its telemetry log too, to replay it anywhere.
    #[default]
    Logs,
    /// Its camera video too, on a plan that takes video.
    Video,
}

impl Settings {
    /// The simulator log size limit in force: the default until the operator
    /// chooses storage limits. Zero keeps every log.
    #[must_use]
    pub fn simulator_log_limit(&self) -> u64 {
        if self.storage_cleanup_chosen {
            self.simulator_log_max_bytes
        } else {
            DEFAULT_SIMULATOR_LOG_MAX_BYTES
        }
    }

    /// "Sync real flights automatically" (ADR-0046 B8) replaces "Add
    /// every flight to my logbook" and "Store flight logs and videos in my
    /// account": a computer where either was on has both on, once. One that
    /// stored flights keeps its video going too.
    pub fn merge_sync_switches(&mut self) {
        if self.sync_merged {
            return;
        }
        self.sync_merged = true;
        if self.store_flights {
            self.sync_content = SyncContent::Video;
        }
        let on = self.save_flights || self.store_flights;
        self.save_flights = on;
        self.store_flights = on;
    }

    /// "Live sharing from this computer" replaces "Connect this computer to
    /// my account" and "View this computer's vehicles remotely"
    /// (ADR-0048): a computer where either was off has it off, once.
    pub fn merge_live_sharing(&mut self) {
        if !self.connect_to_account {
            self.live_sharing = false;
            self.connect_to_account = true;
        }
    }
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
mod terrain_area_tests {
    use super::*;

    #[test]
    fn a_rectangle_kept_before_polygons_reads_as_its_four_corners() {
        let old = serde_json::json!({
            "id": "ta1", "name": "Snowdonia",
            "southDeg": 53.0, "westDeg": -5.0, "northDeg": 54.0, "eastDeg": -4.0
        });
        let area: TerrainArea = serde_json::from_value(old).unwrap();
        let corners: Vec<(f64, f64)> = area
            .points
            .iter()
            .map(|p| (p.latitude_deg, p.longitude_deg))
            .collect();
        assert_eq!(
            corners,
            [(53.0, -5.0), (53.0, -4.0), (54.0, -4.0), (54.0, -5.0)]
        );
        assert!(area.enabled, "kept areas were all kept");
        // Written back as a polygon, and read the same.
        let written = serde_json::to_value(&area).unwrap();
        assert!(written.get("southDeg").is_none(), "{written}");
        assert_eq!(written["enabled"], true);
        let again: TerrainArea = serde_json::from_value(written).unwrap();
        assert_eq!(again, area);
        // Settings from before keep their areas.
        let mut settings = serde_json::to_value(Settings::default()).unwrap();
        settings["terrainAreas"] = serde_json::json!([{
            "id": "ta1", "name": "Snowdonia",
            "southDeg": 53.0, "westDeg": -5.0, "northDeg": 54.0, "eastDeg": -4.0
        }]);
        let settings: Settings = serde_json::from_value(settings).unwrap();
        assert_eq!(settings.terrain_areas, [area]);
    }

    #[test]
    fn a_polygon_keeps_its_points_and_whether_it_is_enabled() {
        let drawn = serde_json::json!({
            "id": "ta2", "name": "Valley", "enabled": false,
            "points": [
                { "latitudeDeg": 1.0, "longitudeDeg": 2.0 },
                { "latitudeDeg": 1.5, "longitudeDeg": 2.5 },
                { "latitudeDeg": 1.0, "longitudeDeg": 3.0 }
            ]
        });
        let area: TerrainArea = serde_json::from_value(drawn.clone()).unwrap();
        assert_eq!(area.points.len(), 3);
        assert!(!area.enabled);
        assert_eq!(serde_json::to_value(&area).unwrap(), drawn);
    }
}

#[cfg(test)]
mod simulator_retention_tests {
    use super::*;

    #[test]
    fn either_old_sync_switch_turns_both_on_once() {
        let old = Settings {
            sync_merged: false,
            ..Settings::default()
        };
        let mut saved = Settings {
            save_flights: true,
            ..old.clone()
        };
        saved.merge_sync_switches();
        assert!(saved.save_flights && saved.store_flights);
        assert_eq!(saved.sync_content, SyncContent::Logs);
        let mut stored = Settings {
            store_flights: true,
            ..old.clone()
        };
        stored.merge_sync_switches();
        assert!(stored.save_flights && stored.store_flights);
        assert_eq!(stored.sync_content, SyncContent::Video);
        let mut neither = old;
        neither.merge_sync_switches();
        assert!(!neither.save_flights && !neither.store_flights && neither.sync_merged);
        // A new computer has nothing to merge.
        let mut new = Settings {
            save_flights: true,
            ..Settings::default()
        };
        new.merge_sync_switches();
        assert!(!new.store_flights);
        // Once: switched off afterwards, it stays off.
        stored.save_flights = false;
        stored.store_flights = false;
        stored.merge_sync_switches();
        assert!(!stored.save_flights && !stored.store_flights);
        // Settings saved before the merge read as not merged yet.
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("syncMerged");
        object.remove("syncContent");
        object.remove("waitForUnmetered");
        let old: Settings = serde_json::from_value(value.clone()).unwrap();
        assert!(!old.sync_merged);
        assert_eq!(old.sync_content, SyncContent::Logs);
        assert!(old.wait_for_unmetered);
        // Merged under #525's name, it stays merged.
        value
            .as_object_mut()
            .unwrap()
            .insert("backupsMerged".into(), true.into());
        let merged: Settings = serde_json::from_value(value).unwrap();
        assert!(merged.sync_merged);
    }

    #[test]
    fn either_old_account_switch_off_turns_live_sharing_off_once() {
        let read = |old: &serde_json::Value| {
            let mut value = serde_json::to_value(Settings::default()).unwrap();
            let object = value.as_object_mut().unwrap();
            object.remove("liveSharing");
            object.extend(old.as_object().unwrap().clone());
            let mut settings: Settings = serde_json::from_value(value).unwrap();
            settings.merge_live_sharing();
            settings
        };
        let on = serde_json::json!({ "connectToAccount": true, "watchFromAccount": true });
        assert!(read(&on).live_sharing);
        assert!(read(&serde_json::json!({})).live_sharing);
        assert!(!read(&serde_json::json!({ "connectToAccount": false })).live_sharing);
        assert!(!read(&serde_json::json!({ "watchFromAccount": false })).live_sharing);
        // Remote control is dropped, whatever it allowed.
        assert!(read(&serde_json::json!({ "remoteControl": ["safety", "mission"] })).live_sharing);
        // Once: turned on again, it stays on, and the old switch is not written.
        let mut merged = read(&serde_json::json!({ "connectToAccount": false }));
        merged.live_sharing = true;
        merged.merge_live_sharing();
        assert!(merged.live_sharing);
        let written = serde_json::to_value(&merged).unwrap();
        assert!(written.get("connectToAccount").is_none());
        assert_eq!(written["liveSharing"], true);
    }

    #[test]
    fn old_settings_use_the_default_log_limit_and_explicit_keep_survives_serialization() {
        let mut value = serde_json::to_value(Settings::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("simulatorLogRetentionDays");
        value
            .as_object_mut()
            .unwrap()
            .remove("simulatorLogMaxBytes");
        value
            .as_object_mut()
            .unwrap()
            .remove("storageCleanupChosen");
        let old: Settings = serde_json::from_value(value).unwrap();
        assert!(!old.storage_cleanup_chosen);
        assert!(!old.simulator_cleanup_told);
        assert_eq!(old.simulator_log_retention_days, None);
        assert_eq!(old.simulator_log_max_bytes, 0);
        assert_eq!(old.simulator_log_limit(), DEFAULT_SIMULATOR_LOG_MAX_BYTES);
        let mut keep = old;
        keep.storage_cleanup_chosen = true;
        keep.simulator_log_retention_days = None;
        keep.simulator_log_max_bytes = 0;
        let reloaded: Settings =
            serde_json::from_value(serde_json::to_value(keep).unwrap()).unwrap();
        assert_eq!(reloaded.simulator_log_retention_days, None);
        assert_eq!(reloaded.simulator_log_max_bytes, 0);
        assert_eq!(reloaded.simulator_log_limit(), 0);
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

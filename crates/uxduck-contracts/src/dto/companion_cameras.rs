//! The cameras on a companion computer on the wire (ADR-0060).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What `MediaMTX` on a companion computer serves, as UXDuck last read it
/// over SSH from its local API (ADR-0060 C1, C8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCameras {
    /// The systemd unit running a `mediamtx` binary; absent when none runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Its configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// Its API, such as `127.0.0.1:9997`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// UXDuck's drop-in for the unit, whether or not it exists yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_in: Option<String>,
    /// The unit's user, when it is a login user (C7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linger: Option<CameraLinger>,
    pub paths: Vec<CompanionCameraPath>,
    pub watchdog: CameraWatchdog,
    /// What the picture change that made this read did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<PictureApplied>,
    /// Why the cameras could not be read, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Why `/etc/uxduck/companion.toml`'s recordings could not be read,
    /// for people (ADR-0066 R10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_problem: Option<String>,
    /// The sound capture devices on the computer (ADR-0067 M2, M14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub microphones: Vec<CompanionMicrophoneDevice>,
    /// Why the computer cannot send sound, for people, such as `GStreamer`
    /// missing (ADR-0067 M4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_problem: Option<String>,
}

/// A sound capture device on a companion computer (ADR-0067 M2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionMicrophoneDevice {
    /// Stable across reboots and replugging into the same port:
    /// `usb:<bus path>` for a USB device, else `card:<ALSA id>`.
    pub id: String,
    /// The card's long name, such as "USB `PnP` Sound Device".
    pub name: String,
    /// The ALSA card id, such as `Device`.
    pub card: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usb: bool,
}

/// Which microphone a camera's sound comes from (ADR-0067 M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionMicrophone {
    /// `auto` (also when `companion.toml` has no `microphone`), `none`,
    /// or a device's `id`.
    pub choice: String,
    /// The device the choice gives now, by its id, when one is plugged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// The `MediaMTX` unit's login user (C7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraLinger {
    pub user: String,
    /// `loginctl enable-linger` is on for it.
    pub lingering: bool,
    /// systemd-logind removes its shared memory when its last session ends.
    pub remove_ipc: bool,
    /// Signing in over SSH as this user can stop the cameras: picture
    /// changes wait until lingering is on or the person dismisses this.
    pub at_risk: bool,
}

/// A camera's picture on a `rpiCamera` path (C3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PictureSettings {
    pub width: u32,
    pub height: u32,
    /// Pictures a second.
    pub fps: u32,
    /// The sensor mode, `width:height:bit depth:packing` such as
    /// `2304:1296:10:P`. Empty lets the camera choose.
    #[serde(default)]
    pub mode: String,
    /// Pictures from one keyframe to the next.
    pub idr_period: u32,
    /// Bits a second.
    pub bitrate: u32,
}

/// UXDuck's presets (C4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PicturePreset {
    LowLatency,
    Detail,
    Fastest,
    DataSaver,
}

/// A preset a camera can take, with its numbers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresetSettings {
    pub preset: PicturePreset,
    pub settings: PictureSettings,
}

/// One path in `MediaMTX`'s configuration, with its live state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCameraPath {
    pub name: String,
    /// `rpiCamera`, `publisher`, or the scheme of the address it pulls from.
    pub source: String,
    /// `rpiCameraCamID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_id: Option<u32>,
    /// The sensor, such as `imx708`, when the computer says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<String>,
    /// Its effective picture, for a `rpiCamera` path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PictureSettings>,
    /// The preset its picture matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<PicturePreset>,
    /// The presets it can take. Empty when it cannot be adjusted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<PresetSettings>,
    /// Why UXDuck cannot set its picture, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// UXDuck's drop-in sets its picture.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub set_by_uxduck: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_demand: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready: bool,
    #[serde(default)]
    pub readers: u32,
    /// Bytes it has received since `MediaMTX` started.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// Its recording on the vehicle, when `uxduck companion` records it
    /// (ADR-0066 R1, R10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<VehicleRecording>,
    /// Its microphone, when `companion.toml` has a `[cameras.<name>]`
    /// table for it (ADR-0067 M3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<CompanionMicrophone>,
}

/// When `uxduck companion` records a camera on the vehicle (ADR-0066 R2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleRecordMode {
    /// While the vehicle is armed. The default.
    Armed,
    Always,
    /// Only when a ground station asks.
    Off,
}

/// A camera's recording on the vehicle (ADR-0066 R1, R10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleRecording {
    /// The `MediaMTX` path recorded, such as `front_record`: a second,
    /// fixed-quality stream of the same camera.
    pub path: String,
    pub mode: VehicleRecordMode,
    /// That path's picture, when `MediaMTX` has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PictureSettings>,
}

/// The camera watchdog (C9, C10).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraWatchdog {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub installed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    /// It stopped restarting after three restarts in ten minutes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gave_up: bool,
    /// Its recent restarts, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restarts: Vec<WatchdogRestart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogRestart {
    /// When, by the computer's clock.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    /// The camera that had stopped.
    pub path: String,
}

/// How a picture change ended (C5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PictureOutcome {
    Applied,
    /// A camera did not come back, and the computer put the old settings back.
    RolledBack,
    /// The old settings did not bring every camera back either.
    RollbackFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PictureApplied {
    /// The path whose picture was changed.
    pub path: String,
    pub outcome: PictureOutcome,
    /// The camera that did not come back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
    /// `MediaMTX`'s last lines for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

/// `computer/cameras`: read the computer's cameras, or make one change
/// (ADR-0060). Changes need a disarmed vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerCamerasParams {
    pub vehicle_id: String,
    pub computer_id: String,
    /// A new picture for one path, applied by restarting `MediaMTX`, which
    /// pauses every camera on the computer for a few seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<PictureApplyParams>,
    /// Turn lingering on for the `MediaMTX` unit's user
    /// (`loginctl enable-linger`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linger: bool,
    /// Install or remove the camera watchdog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<WatchdogChange>,
    /// When a camera records on the vehicle (ADR-0066 R9): written to
    /// `/etc/uxduck/companion.toml`, which `uxduck companion` reads again.
    /// No camera pauses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordModeParams>,
    /// Which microphone a camera's sound comes from (ADR-0067 M3, M14):
    /// written to `/etc/uxduck/companion.toml`, which `uxduck companion`
    /// reads again. No camera pauses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<MicrophoneParams>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MicrophoneParams {
    /// The camera's path, such as `front`.
    pub path: String,
    /// `auto`, `none`, or a device's `id` from `microphones`.
    pub choice: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordModeParams {
    /// The camera's path, such as `front`, whose recording it is.
    pub path: String,
    pub mode: VehicleRecordMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PictureApplyParams {
    pub path: String,
    pub settings: PictureSettings,
    /// The person dismissed the warning that signing in over SSH can stop
    /// these cameras.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub despite_linger: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WatchdogChange {
    Install,
    Remove,
}

/// `computer/recordings` (S429): the recordings a camera computer made on
/// the vehicle, listed over the pinned SSH. Name the computer, or a
/// picture to list the computer that records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRecordingsParams {
    pub vehicle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<String>,
    /// A picture (`VideoSource`): the computer whose camera its address
    /// names, and only that camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

/// `computer/recordings`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRecordingsResult {
    pub vehicle_id: String,
    pub computer_id: String,
    pub computer_name: String,
    /// Each camera that records on the vehicle.
    pub cameras: Vec<CameraRecordings>,
    /// Why the computer could not list them, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// One camera's recordings on the vehicle, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraRecordings {
    /// Its live path, such as `front`.
    pub camera: String,
    /// The `MediaMTX` path that records it, such as `front_record`.
    pub path: String,
    /// The folder its segments are in on the computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The picture on this computer whose address names this camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub recordings: Vec<RecordingOnVehicle>,
    /// Why its recordings could not be listed, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// One recording on the vehicle: from one arming or Record press to its
/// end, however many segments `MediaMTX` wrote it in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordingOnVehicle {
    /// Its record path and its first segment's name.
    pub id: String,
    /// By the camera computer's clock.
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub duration_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    pub segments: u32,
    /// Its last segment is still being written: it downloads once it ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
    /// Why it cannot be downloaded, for people, besides still recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_downloadable: Option<String>,
    /// Its download, running, waiting, failed or done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<String>,
    /// The recording on this computer it was downloaded as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
}

/// `computer/download` (S429): copy one listed recording to this
/// computer's recordings, resumably, checked against the computer's hash.
/// It answers with the transfer, which pauses, resumes and cancels as
/// `files/pause`, `files/resume` and `files/cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerDownloadParams {
    pub vehicle_id: String,
    pub computer_id: String,
    /// A recording's id as `computer/recordings` listed it.
    pub recording: String,
    /// Download now even while this computer's connection is metered
    /// (ADR-0046 B12). Without it, a download on a metered connection
    /// waits for an unmetered one when Wait for unmetered networks is on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub over_metered: bool,
}

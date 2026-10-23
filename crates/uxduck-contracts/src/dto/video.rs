//! Camera streams on the wire (ADR-0017 V4 to V6, V15). The pictures go on
//! the `/video` socket, never here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VideoContainer {
    /// MPEG-TS straight in UDP datagrams.
    Ts,
    /// RTP carrying MPEG-TS.
    RtpTs,
    /// RTP carrying video whose codec is not known yet.
    Rtp,
    /// RTP carrying H.264 straight (RFC 6184).
    RtpH264,
    /// RTP carrying H.265 straight (RFC 7798).
    RtpH265,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VideoState {
    Receiving,
    /// Nothing for a few seconds; the same source if it comes back.
    Stopped,
    /// Arriving, but nothing UXDuck can relay, such as an unknown codec.
    Unsupported,
}

/// Counted since the source was first seen, except the bitrate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoCounters {
    #[schemars(schema_with = "crate::safe::integer")]
    pub bitrate_bps: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub datagrams: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub pictures: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub keyframes: u64,
    /// Packets lost on the way (continuity gaps, transport errors).
    #[schemars(schema_with = "crate::safe::integer")]
    pub lost: u64,
    /// Pictures that lost packets.
    #[schemars(schema_with = "crate::safe::integer")]
    pub damaged: u64,
    /// PES whose length field disagreed with what arrived.
    #[schemars(schema_with = "crate::safe::integer")]
    pub pes_length_mismatches: u64,
    /// The most bytes a length field understated.
    #[schemars(schema_with = "crate::safe::signed_integer")]
    pub pes_length_max_excess: i64,
    /// Delimiters, filler, SEI and invalid NAL units dropped.
    #[schemars(schema_with = "crate::safe::integer")]
    pub nals_dropped: u64,
    /// KLV local sets read (S41).
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub klv: u64,
    /// Of those, the ones whose checksum failed, ignored.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub klv_bad: u64,
}

/// Prerecorded footage played as a camera, independent of vehicle motion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoFileInput {
    /// Absolute path on the computer running UXDuck; never uploaded.
    pub path: String,
    pub name: String,
    pub repeat: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo_id: Option<String>,
}

/// A camera attached to the computer running the daemon, never to a UI session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent choices made with the camera"
)]
pub struct CaptureInput {
    /// Opaque platform device identity returned by `video/devices`.
    pub device_id: String,
    /// The device's name when selected; retained while unplugged.
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    /// Analog input standard; Auto keeps the device's default mode.
    #[serde(default)]
    pub standard: CaptureStandard,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// Bob deinterlacing preserves each field's time for analog FPV.
    #[serde(default)]
    pub deinterlace: bool,
    /// Record `soundInput` with the pictures (S148), the one chosen. Not
    /// set and not `noSound`, the camera's own microphone is recorded when
    /// it has one (S161): the sound input named like it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_sound: bool,
    /// The sound input chosen with the camera, kept while it isn't used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_input: Option<SoundInput>,
    /// No sound, not even the camera's own microphone (S161).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_sound: bool,
}

/// A sound input chosen with a local camera (S148): a microphone, or a
/// capture adapter's audio.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SoundInput {
    /// Opaque platform identity from `video/devices`' sound inputs.
    pub device_id: String,
    /// Its name when chosen; kept while unplugged.
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureStandard {
    #[default]
    Auto,
    Pal,
    Ntsc,
}

/// A camera stream arriving at UXDuck.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about a camera, each said by hand or not"
)]
pub struct VideoSource {
    /// Recording intent, including while reconnecting. Record can stop it then too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording_requested: bool,
    pub id: String,
    /// Its address, such as `udp://127.0.0.1:5600` or an RTSP address
    /// without its password, until someone renames it.
    pub name: String,
    /// Someone named it (`video/rename`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_by_hand: bool,
    /// The UDP port it arrives on.
    pub port: u16,
    /// The sender's address and port.
    pub sender: String,
    pub container: VideoContainer,
    /// The WebCodecs codec string, such as `hvc1.1.2.L120` or `avc1.64001f`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    pub state: VideoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    /// The vehicle it belongs to: by the sender's address, or chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assigned_by_hand: bool,
    /// Its vehicle's own camera takes a photo and records along with
    /// UXDuck (S145): on for a stream the vehicle's camera announced, else
    /// off, until set by hand (`video/vehicleCamera`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_camera: bool,
    /// "Record with the others" is off (S153): neither the recording
    /// setting nor `video/recordAll` starts it, only `video/record`
    /// (`video/recordWithOthers`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_alone: bool,
    /// It carries KLV metadata.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub klv: bool,
    /// Where UXDuck fetches it, such as an RTSP address, without its password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Added by hand: it stays until removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added_by_hand: bool,
    /// Local device capture and its encoding provenance; absent for network cameras.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<VideoFileInput>,
    /// Every video track an RTSP camera offers, the one playing marked;
    /// empty until it answers, and for UDP. `video/track` chooses another.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<VideoTrack>,
    /// Its sound, once its stream is known, and whether UXDuck records it
    /// (S147).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<VideoAudio>,
    pub counters: VideoCounters,
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_seen_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub changed_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// One video track of an RTSP camera, as its SDP describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoTrack {
    /// From 1, in the camera's order: what `#track=N` on the address names.
    pub number: u32,
    /// "H.264", "H.265" or "MPEG-TS".
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    /// The one this source plays.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoListResult {
    /// With their latest counters.
    pub sources: Vec<VideoSource>,
    /// The UDP port UXDuck listens on for video, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Why it is not listening, such as another program holding the port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_error: Option<String>,
    /// The open `/video` sockets: what each was sent and what it dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets: Vec<VideoSocket>,
    /// UDP ports added by hand, and why one is not listening.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ports: Vec<VideoPortStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPortStatus {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAddParams {
    /// An RTSP address (`rtsp://user:password@host:554/path`), or a UDP port
    /// to listen on (`udp://:5601` or `udp:5601`).
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAddResult {
    /// The RTSP source, connecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VideoSource>,
    /// The UDP port now listened on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoTrackParams {
    /// An RTSP source added by hand.
    pub source_id: String,
    /// The track to play, from 1 as in `tracks`; 1 is the camera's first,
    /// the default. Kept with the source as `#track=N` on its address.
    pub track: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRenameParams {
    pub source_id: String,
    /// The new name; empty goes back to the source's address.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRevealParams {
    /// A recording to select; absent opens the folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// A photo to select instead, in the pictures folder (S144).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRevealResult {
    /// The folder shown: recordings', or photos' for a photo.
    pub folder: String,
}

/// A camera's sound (S147).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAudio {
    /// "AAC, 48 kHz, stereo", or the kind of sound UXDuck does not record,
    /// such as "MPEG audio".
    pub name: String,
    pub recorded: bool,
}

/// The largest photo `video/photo` takes, in bytes.
pub const PHOTO_MAX: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoParams {
    /// The camera the picture is from.
    pub source_id: String,
    /// The picture as a JPEG, base64, up to 8 MB.
    pub jpeg_base64: String,
    /// The person's offset from UTC, in minutes (+60 for UTC+1), for the
    /// file's name and its EXIF time.
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoVehicleCameraParams {
    pub source_id: String,
    /// Whether the vehicle's own camera takes photos and records along.
    pub on: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoShowParams {
    pub photo_id: String,
}

/// A photo from the video (S144, ADR-0017 V19), kept in the platform's
/// pictures folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhoto {
    pub id: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub taken_at_us: u64,
    /// Its name in the pictures folder, such as `2026-10-18 10.42.03 Alpha - Front.jpg`:
    /// the vehicle and the camera, or the camera alone.
    pub file_name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
    /// Above mean sea level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// The receipt of the vehicle's camera told to take one too (S145).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_receipt_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRemoveParams {
    /// A source added by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// A UDP port added by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// One open `/video` socket, for the inspector (V15).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoSocket {
    pub source_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub sent: u64,
    /// Pictures dropped for it: before a keyframe, or when it fell 200 ms behind.
    #[schemars(schema_with = "crate::safe::integer")]
    pub dropped: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAssignParams {
    pub source_id: String,
    /// The vehicle, or absent to go back to matching by sender address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoWatchParams {
    pub source_id: String,
}

/// A map needs live metadata, without a media socket or decoder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoMetadataWatchResult {
    /// Release with `video/unwatch`; disconnecting also releases it.
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoWatchResult {
    /// Open a WebSocket on this path of the daemon's origin within 30 s;
    /// its binary messages are the source's access units, from a keyframe.
    pub path: String,
    /// Names this watch for `video/unwatch`; closing the socket also ends it.
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoUnwatchParams {
    pub token: String,
}

/// When a camera's video is recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RecordPolicy {
    /// Only when someone presses Record.
    ByHand,
    /// While its vehicle is armed.
    #[default]
    WhileArmed,
    /// Whenever it sends.
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RecordingState {
    Recording,
    Done,
    /// UXDuck stopped while recording; it ends at the last whole fragment.
    Recovered,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RecordingTrigger {
    ByHand,
    Armed,
    Always,
}

/// A camera's original, kept without re-encoding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecording {
    pub id: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub duration_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub state: RecordingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub trigger: RecordingTrigger,
    /// Capture settings at the start; these pictures were encoded by UXDuck's helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<VideoFileInput>,
    /// The receipt of the vehicle's camera told to record too (S145).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_receipt_id: Option<String>,
    /// It has the camera's sound as a second track (S147).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sound: bool,
    /// Its file in the media folder, such as `2026-10-18 10.42.03
    /// Alpha - Front.ts` (S150): the vehicle and the camera, or the camera
    /// alone; none for one made before, named `<id>.mp4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecordAllParams {
    /// Start every camera that records with the others and isn't paused
    /// or unplayable (`true`), or stop every camera recording (`false`),
    /// whatever the policy says.
    pub on: bool,
    /// Only this vehicle's cameras (S154); every camera without.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecordWithOthersParams {
    pub source_id: String,
    /// Whether the recording setting and Record all start this camera; its
    /// own Record does either way.
    pub on: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecordParams {
    pub source_id: String,
    /// Start (`true`) or stop (`false`), whatever the policy says, until the
    /// request is explicitly stopped or the daemon restarts.
    pub on: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlaybackParams {
    pub recording_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPlaybackResult {
    /// An MP4 on the daemon's origin, good for an hour, for a `<video>`.
    pub path: String,
    /// The recording's original MPEG-TS on the daemon's origin, for Save…,
    /// when it is kept as one (S151).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
}

/// A video device, or a sound input, visible to the daemon's operating system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDevice {
    pub id: String,
    pub name: String,
    /// Whether the platform identity survives enumeration changes.
    pub persistent: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoDevicesResult {
    pub devices: Vec<CaptureDevice>,
    /// Microphones and capture adapters' audio, for a camera's sound (S148).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sound_inputs: Vec<CaptureDevice>,
    pub scanning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoCaptureParams {
    pub input: CaptureInput,
}

/// Opens a fixed camera/microphone privacy pane on the daemon's computer.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VideoCaptureSettingsParams {
    #[serde(default)]
    pub microphone: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoCaptureEnableParams {
    pub source_id: String,
    pub enabled: bool,
}

/// Add or replace a file camera. Exactly one of path and demoId is required.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoFileParams {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub demo_id: Option<String>,
    pub repeat: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoDemosParams {
    #[serde(default)]
    pub action: DemoAction,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DemoAction {
    #[default]
    List,
    Refresh,
    Download,
    Cancel,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DemoVideo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub duration_seconds: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// SHA-256 of the complete MP4; also its immutable object name.
    pub sha256: String,
    pub credit: String,
    pub source_url: String,
    pub license: String,
    pub thumbnail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DemoDownload {
    pub id: String,
    pub ready: bool,
    pub downloading: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub received_bytes: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoDemosResult {
    pub clips: Vec<DemoVideo>,
    pub downloads: Vec<DemoDownload>,
    pub refreshing: bool,
    #[serde(default)]
    pub error: Option<String>,
}

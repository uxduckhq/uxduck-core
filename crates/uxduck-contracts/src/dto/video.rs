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
    /// Recovery points that were not keyframes: a camera with periodic
    /// intra refresh has these and no keyframe after it starts.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub recovery_points: u64,
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
    /// How long UXDuck's daemon holds a picture, in microseconds: from its
    /// first datagram's arrival to its send on the `/video` socket, the
    /// median over the last second, the longest of the browsers watching
    /// (S262). Absent while no browser watches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub hold_us: Option<u64>,
    /// RFC 3550 interarrival jitter in microseconds: how unevenly pictures
    /// arrive against their presentation times (S262).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub jitter_us: Option<u64>,
    /// Where the camera says when it captured a picture; absent for a
    /// camera that sends no clock (S262).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_clock: Option<CameraClock>,
    /// Microseconds from capture by the camera's clock to arrival at
    /// UXDuck, the median over the last second, the camera's clock matched
    /// to this computer's by the way `cameraClockMatches` marks used.
    /// Absent with a `cameraClock` when the result is not plausible: under
    /// 0 by more than the match's uncertainty (a slightly negative one reads
    /// 0), or over 5 s (S262, S355).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub camera_delay_us: Option<u64>,
    /// The same median before that check, in microseconds: negative when
    /// the camera's clock dates pictures after they arrive, so its times
    /// are not capture times, or, measured over RTSP, its sender reports
    /// are dated late (S355).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub camera_delay_signed_us: Option<i64>,
    /// The ways of matching the camera's clock to this computer's, in the
    /// order they are tried, and the one used: the first with a measurement
    /// (S355). Absent for a camera that sends no clock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub camera_clock_matches: Vec<ClockMatch>,
}

/// One way of matching a camera's clock to this computer's, and what it
/// found (S355).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClockMatch {
    pub way: ClockWay,
    /// The onboard computer id, 191 to 194, of the companion computer
    /// serving the camera, for `companion` (S357).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<u8>,
    /// The camera's clock minus this computer's in microseconds, once this
    /// way has measured it: negative when the camera's is behind. 0 for
    /// `assumed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub offset_us: Option<i64>,
    /// Give or take, in microseconds, once measured. Absent for `assumed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub within_us: Option<u64>,
    /// This way's offset dates the pictures.
    #[serde(default)]
    pub used: bool,
    /// Why this way is not used, when that is not because a way before it
    /// is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<ClockGap>,
}

/// A way of matching a camera's clock to this computer's, in the order they
/// are tried (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClockWay {
    /// The clock of the companion computer serving the camera, measured by
    /// MAVLink TIMESYNC: the computer in the profile of the camera's vehicle
    /// whose host is the camera's (S357).
    Companion,
    /// The camera server's clock, from its RTCP sender reports and its
    /// round trips: `GET_PARAMETER` over RTSP, ICE's consent checks over
    /// WHEP, whose `MediaMTX` sends no RTCP round trip (ADR-0064 W11). Used
    /// only for sender reports' times.
    Rtsp,
    /// The two clocks assumed in step.
    Assumed,
}

/// Why a way of matching a camera's clock is not used (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClockGap {
    /// The camera is not reached this way: not over RTSP or WHEP, or no
    /// companion computer serves it.
    NotServed,
    /// No sender report in the last minute.
    NoReports,
    /// No answer to a request in the last minute.
    NoAnswers,
    /// Read only one way, from the companion computer's status messages:
    /// off by however long they took, so it dates no picture (S357).
    OneWay,
    /// The RTSP server's sender reports say when each picture was taken but
    /// not when they were sent, as gortsplib's (`MediaMTX`'s RTSP library) do:
    /// measured by them its clock reads behind by at least a picture's time
    /// from the sensor to the server, so it dates no picture.
    DatedByPictures,
    /// Not the clock the camera dates its pictures by, as for a KLV time
    /// stamp, which may come from GPS on another machine.
    OtherClock,
}

/// Where a camera says when it captured a picture (S262).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraClock {
    /// RTCP sender reports: its RTP clock at a wall-clock time.
    SenderReports,
    /// The Precision Time Stamp in its KLV (MISB ST 0601 tag 2).
    Klv,
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
    /// The encoder re-encoding it for live view now (`h264_videotoolbox`,
    /// `libx264`, ...), for a camera that sends no IDR; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reencoding: Option<String>,
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
    /// What the picture says while it plays, such as a WebRTC camera's
    /// connection carrying too little for its pictures (ADR-0064 W7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_note: Option<String>,
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
    /// Recorded on the vehicle only (ADR-0066 R6, `video/recordWhere`):
    /// this computer records nothing of it while its vehicle has a camera
    /// that records it, and records it as before while the vehicle has none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_only: bool,
    /// The companion computer serving an RTSP camera, by its id in the
    /// vehicle's `companions`, chosen by hand (S358, `video/servedBy`): its
    /// TIMESYNC clock dates the pictures. Absent: the computer whose host is
    /// the camera address's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
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
    /// Other addresses of this same camera, in preference order. Only one plays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_urls: Vec<String>,
    /// The names someone typed for its addresses (S430), by place: the first
    /// names `url`, the rest `fallbackUrls` in order, such as `Radio
    /// WebRTC`. Empty or missing where none was typed, where the app names
    /// the address by the vehicle connection on its host and its protocol,
    /// such as `Plane 4G · WebRTC`. `video/addresses` sets them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub address_names: Vec<String>,
    /// Direct local routes and an optional smaller stream from the same camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internet: Option<CameraInternet>,
    /// The address currently being tried or played, redacted on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_url: Option<String>,
    /// Why it is on `activeUrl` and since when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_choice: Option<AddressChoice>,
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
    /// Whether its sound can be heard, and why not (ADR-0067 M13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<VideoSound>,
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
    /// An RTSP address (`rtsp://user:password@host:554/path`), a WHEP
    /// address played over WebRTC (`http://user:password@host:8889/path/whep`,
    /// or `https://…` with `#sha256=` and the hex SHA-256 of the certificate
    /// it must present, ADR-0064 W5, W10), or a UDP port to listen on
    /// (`udp://:5601` or `udp:5601`).
    pub url: String,
    /// What to call the camera ("Front"); its address when absent or empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The vehicle it belongs to, chosen by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAddResult {
    /// The RTSP source, connecting; for a UDP port, the camera on it: one
    /// already arriving there, or one waiting for its first picture, which
    /// the first stream to arrive becomes, keeping its name and vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VideoSource>,
    /// The UDP port now listened on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAddressesParams {
    pub source_id: String,
    /// RTSP or WHEP addresses in order, the preferred one first, with no
    /// fixed count. A WHEP address goes before the RTSP address on its host,
    /// which is its fallback (ADR-0064 W5). Unchanged redacted addresses
    /// retain their stored credentials.
    pub urls: Vec<String>,
    /// A name for each of `urls`, in the same order, empty for none (S430).
    /// Absent: each address keeps the name it had, wherever it moved, and a
    /// new address has none. Changing only names keeps the video playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
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

/// What a camera's speaker can do (ADR-0067 M12, M13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoSound {
    pub state: VideoSoundState,
    /// Where the sound comes from, for people, such as "USB `PnP` Sound
    /// Device on the Plane".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<String>,
    /// What it is, for people, such as "Opus, mono".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Why it cannot be heard, for people, naming the vehicle, such as "No
    /// microphone on the Plane's camera computer. Plug in a USB microphone."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The companion computer camera whose microphone this is, for the
    /// speaker's Microphone row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion: Option<VideoSoundCompanion>,
    /// Sound is coming now: a window listens or this computer records it
    /// (ADR-0067 M8).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flowing: bool,
    /// The bit/s the camera sends it at now, when its sender says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub bitrate: Option<u64>,
}

/// Whether a camera's sound can be heard (ADR-0067 M9, M13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VideoSoundState {
    /// It can be heard: unmuting asks for it.
    Available,
    /// Its camera computer has no microphone.
    NoMicrophone,
    /// The microphone chosen for it is not plugged in.
    Unplugged,
    /// Its microphone is set to None on the camera computer.
    Off,
    /// The camera computer cannot send sound, such as `GStreamer` missing.
    Failed,
    /// The camera sends no sound.
    NoSound,
    /// It sends sound UXDuck cannot play, such as G.711.
    Unsupported,
}

/// A companion computer's camera, by the vehicle page's ids.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoSoundCompanion {
    pub vehicle_id: String,
    pub computer_id: String,
    /// The camera's path, such as `front`.
    pub path: String,
}

/// Why a camera with fallback addresses is on the address it tries or
/// plays (ADR-0057 D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AddressReason {
    /// Its first address, or an earlier one that answered again.
    Preferred,
    /// The address before showed no pictures for 5 s.
    NoPictures,
    /// The address before could not connect or start.
    ConnectionFailed,
    /// The vehicle moved to the connection on this address's host.
    FollowedVehicle,
    /// The vehicle came back to the preferred address's connection, which
    /// answered a probe.
    ReturnedWithVehicle,
    /// Nothing came from the address before for 2 s while this one was set
    /// up on a connection the vehicle is heard on (ADR-0057 D13).
    Stalled,
    /// This address's connection, higher in the vehicle's order, answered
    /// for the 10 s the vehicle needs to go back, but a hold kept the
    /// vehicle where it was, so the camera went back ahead of it (ADR-0057
    /// D13).
    AheadOfVehicle,
}

/// Why a camera is on its active address, since when, and when it next
/// checks an earlier one (ADR-0057 D10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddressChoice {
    pub reason: AddressReason,
    #[schemars(schema_with = "crate::safe::integer")]
    pub since_us: u64,
    /// The vehicle connection whose remote host is this address's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// While on a fallback: when it next probes the earlier addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub next_check_us: Option<u64>,
    /// The address kept set up to move to by PLAY alone (ADR-0057 D12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standby: Option<AddressStandby>,
    /// How the move to this address went (ADR-0057 D14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<AddressMove>,
    /// While the camera is on its vehicle's connection: a hold that keeps
    /// the vehicle off a connection higher in its order that an address of
    /// the camera is on. For a camera that went ahead of its vehicle: the
    /// hold as it was then (ADR-0057 D13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<AddressAhead>,
}

/// A connection higher in a vehicle's order, heard again, that a hold
/// keeps the vehicle off (ADR-0057 D13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddressAhead {
    /// The connection higher in the order.
    pub link_id: String,
    /// The connection the vehicle stays on.
    pub vehicle_link_id: String,
    /// What keeps the vehicle there.
    pub holds: Vec<crate::dto::vehicle::RouteHold>,
}

/// The address a camera keeps set up, without playing, on another vehicle
/// connection that is heard (ADR-0057 D12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddressStandby {
    /// The vehicle connection whose remote host is its host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// Set up and answering, so a move there needs only PLAY.
    pub ready: bool,
}

/// When each step of a camera's move to another address happened, in
/// microseconds since the Unix epoch on the computer running UXDuck
/// (ADR-0057 D14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddressMove {
    /// The last picture from the address it left, if it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_picture_us: Option<u64>,
    /// When it chose this address.
    #[schemars(schema_with = "crate::safe::integer")]
    pub decided_us: u64,
    /// When it asked this address to play.
    #[schemars(schema_with = "crate::safe::integer")]
    pub play_sent_us: u64,
    /// The first picture from this address.
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_picture_us: u64,
    /// The first keyframe from it, where a browser starts showing video.
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_keyframe_us: u64,
    /// Whether this address was already set up, so the move needed only PLAY.
    pub pre_opened: bool,
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
pub struct VideoServedByParams {
    /// An RTSP camera with a vehicle.
    pub source_id: String,
    /// The id of a companion computer in the camera's vehicle's
    /// `companions`. Absent: the computer whose host is the camera
    /// address's host, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoShowParams {
    pub photo_id: String,
}

/// Photos whose small previews a list shows, all at once, so a list of
/// many photos needs one request rather than one each.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoThumbnailsParams {
    pub photo_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoThumbnailsResult {
    /// One for each photo asked for that UXDuck has.
    pub thumbnails: Vec<VideoPhotoThumbnail>,
    /// The photos asked for that UXDuck no longer has, deleted since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhotoThumbnail {
    pub photo_id: String,
    /// A JPEG at most 320 px on its long edge, on the daemon's origin, good
    /// for an hour. Made from the photo when first asked for, and again
    /// when the photo's file changes. One that can't be made answers with
    /// why, in words.
    pub path: String,
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

/// A clip's length when none is asked for, in seconds.
pub const LOOK_SECONDS_DEFAULT: f64 = 3.0;

/// The long edge of a frame for the assistant, in pixels. Claude and GPT
/// models scale a larger picture down before they read it, so more pixels
/// cost bytes and tokens without detail.
pub const LOOK_LONG_EDGE: u32 = 1024;

/// The most bytes of JPEG a look's frames take together: 8 MB. The daemon
/// holds a look's frames in memory while it answers, about three times
/// over (the JPEGs, their base64 and the reply's text), and 2 looks may run
/// at once, so this keeps looks to tens of MB of the daemon's memory. It is
/// also about as much as the largest provider requests carry: the hosted
/// API takes 8 MB, Anthropic 32 MB and OpenAI 50 MB, as base64.
pub const LOOK_REPLY_BYTES: u64 = 8 * 1024 * 1024;

/// A look through a camera for the assistant (FB-244): the picture now, or
/// a short clip of frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoFrameParams {
    /// The camera, a video source's id. Absent: the vehicle's only camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// The vehicle whose camera to look through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// 1, the default, for the picture now. 2 or more for a clip of that
    /// many frames, evenly spaced over `seconds`. A clip has no more frames
    /// than the camera sends pictures in that time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u32>,
    /// A clip's length in seconds, more than 0. 3 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    /// The most bytes of JPEG the frames may take together. UXDuck sets it
    /// for a model whose requests are limited in size, and the frames are
    /// made smaller to fit. Absent, `LOOK_REPLY_BYTES`. More than that is
    /// held to it, and the result's note says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub max_bytes: Option<u64>,
}

/// One frame of a look, as a JPEG.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoFrame {
    /// Seconds after the look's first frame.
    pub offset_s: f64,
    /// When the camera's picture arrived, in microseconds since the epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub width: u32,
    pub height: u32,
    /// The JPEG, base64. The assistant is shown it as an image.
    pub jpeg_base64: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoFrameResult {
    pub source_id: String,
    /// The camera's name.
    pub camera: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// The camera's own picture size, before scaling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_height: Option<u32>,
    /// In time order.
    pub frames: Vec<VideoFrame>,
    /// What was changed on the way, such as the size the frames were made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

/// How a camera that sends no IDR is re-encoded for live view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Reencoder {
    /// Hardware when it works, software when it does not.
    #[default]
    Automatic,
    /// Only the hardware encoder.
    Hardware,
    /// Only software (x264).
    Software,
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
    /// A video added to a flight from elsewhere: the flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// How an added video's start was put on the flight's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<VideoAlignment>,
    /// Kept in a flight's `.uxflight` on another disk rather than on this
    /// computer (ADR-0062), and played from there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed: Option<PackedVideo>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Where a recording kept in a `.uxflight` is (ADR-0062).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackedVideo {
    /// The `.uxflight` file.
    pub path: String,
    /// Its disk, by name.
    pub place: String,
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

/// Where a camera's recordings are made (ADR-0066 R6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RecordPlace {
    /// On this computer only: the vehicle's camera is not told.
    Computer,
    /// On the vehicle only, by its camera that records this picture. This
    /// computer records it while the vehicle has no such camera.
    Vehicle,
    /// On this computer, and on the vehicle by its camera.
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecordWhereParams {
    pub source_id: String,
    pub place: RecordPlace,
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

/// How an added video was put on the flight's clock (ADR-0039 R8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AlignedBy {
    /// MISB ST 0601 time stamps in the stream: exact.
    Klv,
    /// The file's own creation time.
    Metadata,
    /// The simulator's computer's clock, corrected by the vehicle's.
    VehicleClock,
    /// Set by a person: a nudge, a time, the take-off or the landing.
    Hand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAlignment {
    pub by: AlignedBy,
    /// Where the file's own clock put its start.
    #[schemars(schema_with = "crate::safe::integer")]
    pub own_start_us: u64,
}

/// `flight/addVideo`: a video from elsewhere kept with a flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightAddVideoParams {
    pub flight_id: String,
    /// The file on the computer running UXDuck: MP4, MOV, M4V or MPEG-TS.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Or a video the operator attached to a chat message (S246): its
    /// attachment id, in place of `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
}

/// `video/align`: an added video's start on the flight's clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoAlignParams {
    pub recording_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraInternet {
    pub local_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_data_url: Option<String>,
}

/// Configure direct local routes and the cellular low-data stream of one camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VideoInternetParams {
    pub source_id: String,
    pub internet: CameraInternet,
}

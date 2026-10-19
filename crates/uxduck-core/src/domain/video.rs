//! Camera streams the daemon receives (ADR-0017 V4): one source per
//! listening port and sender address, with what is known of its picture and
//! counters that answer "why is my video bad". The pictures never pass
//! through here; they go from the listener straight to the `/video` socket.

use serde::{Deserialize, Serialize};

/// A camera attached to the computer running the daemon, never to a UI session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    /// Record the sound input's sound with the pictures (S148); off unless
    /// turned on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_sound: bool,
    /// The sound input chosen with the camera, kept while sound is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound_input: Option<SoundInput>,
}

/// A sound input chosen with a local camera (S148): a microphone, or a
/// capture adapter's audio.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundInput {
    /// Opaque platform identity from `video/devices`' sound inputs.
    pub device_id: String,
    /// Its name when chosen; kept while unplugged.
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureStandard {
    #[default]
    Auto,
    Pal,
    Ntsc,
}

/// How the stream arrives (V3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoContainer {
    /// MPEG-TS packets straight in UDP datagrams.
    Ts,
    /// RTP carrying MPEG-TS (payload type 33).
    RtpTs,
    /// RTP carrying video whose codec is not known yet.
    Rtp,
    /// RTP carrying H.264 straight (RFC 6184).
    RtpH264,
    /// RTP carrying H.265 straight (RFC 7798).
    RtpH265,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoState {
    Receiving,
    /// Nothing for a few seconds; the same source if it comes back.
    Stopped,
    /// Arriving, but nothing UXDuck can relay, such as an unknown codec.
    Unsupported,
}

/// A camera's sound (S147): what it is, and whether UXDuck records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoAudio {
    /// "AAC, 48 kHz, stereo", or the kind of sound UXDuck does not record.
    pub name: String,
    pub recorded: bool,
}

/// Counted since the source was first seen, except the bitrate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoCounters {
    /// Over the last second or so.
    pub bitrate_bps: u64,
    pub datagrams: u64,
    pub pictures: u64,
    pub keyframes: u64,
    /// Continuity gaps and transport errors: packets lost on the way.
    pub lost: u64,
    /// Pictures that lost packets.
    pub damaged: u64,
    /// PES whose length field disagreed with what arrived (V2).
    pub pes_length_mismatches: u64,
    /// The most bytes a length field understated.
    pub pes_length_max_excess: i64,
    /// NAL units dropped: delimiters, filler, SEI, invalid headers.
    pub nals_dropped: u64,
    /// KLV local sets read, and those whose checksum failed (S41).
    #[serde(default)]
    pub klv: u64,
    #[serde(default)]
    pub klv_bad: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about a camera, each said by hand or not"
)]
pub struct VideoSource {
    /// Recording is requested, including while waiting for the camera to return.
    /// Recomputed from policy and session intent when the daemon starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording_requested: bool,
    /// `udp-<port>-<sender address>`, so a sender that comes back is the
    /// same source.
    pub id: String,
    /// What the source is called: its address, such as
    /// `udp://127.0.0.1:5600`, until someone renames it.
    pub name: String,
    /// Someone named it; the address no longer does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_by_hand: bool,
    pub port: u16,
    /// The latest sender, with its port.
    pub sender: String,
    pub container: VideoContainer,
    /// The WebCodecs codec string from the SPS, such as `hvc1.1.2.L120`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<f64>,
    pub state: VideoState,
    /// Why it is unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Someone chose the vehicle; the sender's address no longer decides.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assigned_by_hand: bool,
    /// Its vehicle's own camera takes a photo and records along with
    /// UXDuck (S145, ADR-0017 V20): on for a stream the vehicle's camera
    /// announced, off for others, until set by hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_camera: bool,
    /// Someone set `vehicle_camera`; announcements no longer do.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_camera_by_hand: bool,
    /// "Record with the others" is off (S153, ADR-0017 V24): neither the
    /// recording setting nor Record all starts it, only its own Record.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_alone: bool,
    /// It carries KLV metadata.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub klv: bool,
    /// Where UXDuck fetches it, such as an RTSP address added by hand (S39).
    /// Kept with any password, which the wire never shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Someone added it; it stays until removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added_by_hand: bool,
    /// Local device capture and its encoding provenance; absent for network cameras.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureInput>,
    /// Every video track an RTSP camera offers, the one playing marked
    /// (S90); empty until it answers, and for UDP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<VideoTrack>,
    /// Its sound, once its stream is known (S147).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<VideoAudio>,
    pub counters: VideoCounters,
    pub first_seen_us: u64,
    /// When it last started or stopped receiving.
    pub changed_at_us: u64,
    pub version: u64,
}

/// One video track of an RTSP camera, as its SDP describes it (S90).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoTrack {
    /// From 1, in the SDP's order: what `#track=N` on the address names.
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

/// What the listener reports about one sender, about once a second.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoReport {
    pub port: u16,
    pub sender: String,
    pub container: VideoContainer,
    pub codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    /// Why nothing can be relayed, when nothing can.
    pub unsupported: Option<String>,
    pub klv: bool,
    pub counters: VideoCounters,
    /// The source it is about, when not the port and sender's own (RTSP).
    pub id: Option<String>,
    /// Something to say while it plays, such as a KLV track the camera
    /// refused (S91); its `state_detail`.
    pub note: Option<String>,
    /// Its sound, once its stream is known (S147).
    pub audio: Option<VideoAudio>,
}

impl VideoReport {
    /// The source this sender is: the port and the sender's address
    /// without its port, which a camera may change when it restarts.
    #[must_use]
    pub fn source_id(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| source_id(self.port, &self.sender))
    }
}

#[must_use]
pub fn source_id(port: u16, sender: &str) -> String {
    format!("udp-{port}-{}", sender_host(sender))
}

/// A second stream from a sender already sending to `port`, told apart by
/// the port it sends from: `udp-5600-127.0.0.1-from-51234`.
#[must_use]
pub fn split_source_id(port: u16, sender: &str) -> String {
    let from = sender.rsplit_once(':').map_or("", |(_, p)| p);
    format!("{}-from-{from}", source_id(port, sender))
}

/// What a UDP source is called until someone renames it: where it arrives,
/// `udp://127.0.0.1:5600`, and for a second stream from the same sender,
/// the port it comes from too.
#[must_use]
pub fn udp_name(id: &str, port: u16, sender: &str) -> String {
    let host = sender_host(sender);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let name = format!("udp://{host}:{port}");
    match id.rsplit_once("-from-") {
        Some((_, from)) => format!("{name} (from :{from})"),
        None => name,
    }
}

/// `10.0.0.5` from `10.0.0.5:5000`, `::1` from `[::1]:5000`.
#[must_use]
pub fn sender_host(sender: &str) -> &str {
    let host = sender.rsplit_once(':').map_or(sender, |(host, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            host
        } else {
            sender
        }
    });
    host.trim_start_matches('[').trim_end_matches(']')
}

/// When a camera's video is recorded (ADR-0017 V16, S40).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordPolicy {
    /// Only when someone presses Record.
    ByHand,
    /// While its vehicle is armed: a flight, or a test on the ground.
    #[default]
    WhileArmed,
    /// Whenever it sends.
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordingState {
    Recording,
    Done,
    /// UXDuck stopped while recording; it ends at the last whole fragment.
    Recovered,
    Failed,
}

/// Why a recording started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordingTrigger {
    ByHand,
    Armed,
    Always,
}

/// A camera's original, kept without re-encoding in the Video storage
/// category.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoRecording {
    pub id: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_us: Option<u64>,
    pub duration_us: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub state: RecordingState,
    /// Why it failed, or what recovery found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub trigger: RecordingTrigger,
    /// Capture settings at the start; these pictures were encoded by UXDuck's helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureInput>,
    /// The receipt of the vehicle's camera told to record too (S145).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_receipt_id: Option<String>,
    /// It has the camera's sound as a second track (S147).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sound: bool,
    /// Its file in the media folder, named by the local time and the
    /// vehicle (S150); none for one made before, which is `<id>.mp4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub version: u64,
}

impl VideoRecording {
    /// Its file in the media folder: its name, or `<id>.mp4` for one made
    /// before recordings had names.
    #[must_use]
    pub fn file(&self) -> String {
        self.file_name
            .clone()
            .unwrap_or_else(|| format!("{}.mp4", self.id))
    }
}

/// A photo from the video (ADR-0017 V19, S144): the picture a Camera tab
/// showed, kept as a JPEG in the platform's pictures folder, with where
/// the camera's vehicle was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoPhoto {
    pub id: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub taken_at_us: u64,
    /// Its name in the pictures folder.
    pub file_name: String,
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
    pub version: u64,
}

/// What a camera's KLV says (MISB ST 0601, S41), in degrees and metres:
/// where it is, where it looks, and what it sees on the ground.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CameraMetadata {
    pub platform_designation: Option<String>,
    pub image_source: Option<String>,
    pub sensor_latitude_deg: Option<f64>,
    pub sensor_longitude_deg: Option<f64>,
    pub sensor_altitude_m: Option<f64>,
    pub platform_heading_deg: Option<f64>,
    pub horizontal_fov_deg: Option<f64>,
    pub vertical_fov_deg: Option<f64>,
    pub sensor_azimuth_deg: Option<f64>,
    pub sensor_elevation_deg: Option<f64>,
    pub slant_range_m: Option<f64>,
    pub frame_latitude_deg: Option<f64>,
    pub frame_longitude_deg: Option<f64>,
    pub frame_elevation_m: Option<f64>,
    /// The picture's corners on the ground, as (latitude, longitude).
    pub corners: Option<[(f64, f64); 4]>,
}

/// An address without its password, to show: `rtsp://admin@10.0.0.5/main`.
#[must_use]
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, None), |(a, p)| (a, Some(p)));
    let shown = match authority.rsplit_once('@') {
        Some((credentials, host)) => {
            let user = credentials.split_once(':').map_or(credentials, |(u, _)| u);
            format!("{user}@{host}")
        }
        None => authority.to_owned(),
    };
    match path {
        Some(path) => format!("{scheme}://{shown}/{path}"),
        None => format!("{scheme}://{shown}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_is_its_port_and_sender_host() {
        assert_eq!(source_id(5600, "10.0.0.5:41000"), "udp-5600-10.0.0.5");
        assert_eq!(source_id(5600, "[::1]:41000"), "udp-5600-::1");
        assert_eq!(sender_host("10.0.0.5"), "10.0.0.5");
    }

    #[test]
    fn a_udp_source_is_named_by_where_it_arrives() {
        let id = source_id(5600, "127.0.0.1:51000");
        assert_eq!(
            udp_name(&id, 5600, "127.0.0.1:51000"),
            "udp://127.0.0.1:5600"
        );
        let second = split_source_id(5600, "127.0.0.1:51234");
        assert_eq!(second, "udp-5600-127.0.0.1-from-51234");
        assert_eq!(
            udp_name(&second, 5600, "127.0.0.1:51234"),
            "udp://127.0.0.1:5600 (from :51234)"
        );
        assert_eq!(
            udp_name(&source_id(5600, "[::1]:4"), 5600, "[::1]:4"),
            "udp://[::1]:5600"
        );
    }

    #[test]
    fn a_password_is_never_shown() {
        assert_eq!(
            redact("rtsp://admin:secret@10.0.0.5:554/main"),
            "rtsp://admin@10.0.0.5:554/main"
        );
        assert_eq!(redact("rtsp://10.0.0.5/a@b"), "rtsp://10.0.0.5/a@b");
        assert_eq!(redact("rtsp://cam"), "rtsp://cam");
    }
}

//! Camera streams the daemon receives (ADR-0017 V4): one source per
//! listening port and sender address, with what is known of its picture and
//! counters that answer "why is my video bad". The pictures never pass
//! through here; they go from the listener straight to the `/video` socket.

use serde::{Deserialize, Serialize};

/// Prerecorded footage played as a camera, independent of vehicle motion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Record `sound_input` with the pictures (S148), the one chosen. Not
    /// set and not `no_sound`, the camera's own microphone is recorded when
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

/// Why a camera with fallback addresses is on the address it tries or
/// plays (ADR-0057 D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressChoice {
    pub reason: AddressReason,
    pub since_us: u64,
    /// The vehicle connection whose remote host is this address's host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// While on a fallback: when it next probes the earlier addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressAhead {
    /// The connection higher in the order.
    pub link_id: String,
    /// The connection the vehicle stays on.
    pub vehicle_link_id: String,
    /// What keeps the vehicle there.
    pub holds: Vec<crate::domain::vehicle::RouteHold>,
}

/// The address a camera keeps set up, without playing, on another vehicle
/// connection that is heard (ADR-0057 D12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressStandby {
    /// The vehicle connection whose remote host is its host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_id: Option<String>,
    /// Set up and answering, so a move there needs only PLAY.
    pub ready: bool,
}

/// When each step of a camera's move to another address happened, on the
/// computer running UXDuck (ADR-0057 D14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddressMove {
    /// The last picture from the address it left, if it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_picture_us: Option<u64>,
    /// When it chose this address.
    pub decided_us: u64,
    /// When it asked this address to play.
    pub play_sent_us: u64,
    /// The first picture from this address.
    pub first_picture_us: u64,
    /// The first keyframe from it, where a browser starts showing video.
    pub first_keyframe_us: u64,
    /// Whether this address was already set up, so the move needed only PLAY.
    pub pre_opened: bool,
}

/// Counted since the source was first seen, except the bitrate. A counter
/// missing from a checkpoint or event an older build wrote reads as 0.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct VideoCounters {
    /// Over the last second or so.
    pub bitrate_bps: u64,
    pub datagrams: u64,
    pub pictures: u64,
    pub keyframes: u64,
    /// Recovery points that were not keyframes: a camera with periodic
    /// intra refresh has these and no keyframe after it starts.
    pub recovery_points: u64,
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
    pub klv: u64,
    pub klv_bad: u64,
    /// How long the daemon holds a picture, from its first datagram's
    /// arrival to its send on a `/video` socket: the median over the last
    /// second, the longest of the browsers watching (S262). None while no
    /// browser watches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_us: Option<u64>,
    /// RFC 3550 interarrival jitter, from arrival against presentation
    /// time (S262).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter_us: Option<u64>,
    /// Where the camera says when it captured a picture; none for a camera
    /// that sends no clock (S262).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_clock: Option<CameraClock>,
    /// From capture by that clock to arrival, the median over the last
    /// second, the camera's clock matched as `camera_clock_matches` says;
    /// none when that is not plausible (S262, S355).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_delay_us: Option<u64>,
    /// The same median before the check, so a camera whose clock dates
    /// pictures after they arrive can say by how much (S355).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_delay_signed_us: Option<i64>,
    /// The ways of matching the camera's clock to this computer's, in the
    /// order they are tried, and the one used (S355). Empty for a camera
    /// that sends no clock.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub camera_clock_matches: Vec<ClockMatch>,
}

impl VideoCounters {
    /// Nothing arriving: no bitrate, and nothing to say about how late.
    pub fn stop(&mut self) {
        self.bitrate_bps = 0;
        self.hold_us = None;
        self.jitter_us = None;
        self.camera_clock = None;
        self.camera_delay_us = None;
        self.camera_delay_signed_us = None;
        self.camera_clock_matches.clear();
    }
}

/// One way of matching a camera's clock to this computer's, and what it
/// found (S355).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockMatch {
    pub way: ClockWay,
    /// The companion computer's onboard id, for `Companion` (S357).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<u8>,
    /// The camera's clock minus this computer's, once measured: 0 for
    /// `Assumed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_us: Option<i64>,
    /// Give or take, once measured; none for `Assumed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_us: Option<u64>,
    /// Whether this way dates the pictures: the first with a measurement.
    #[serde(default)]
    pub used: bool,
    /// Why it is not used, when that is not because a way before it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<ClockGap>,
}

/// A way of matching a camera's clock to this computer's (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClockWay {
    /// The clock of the companion computer serving the camera, measured by
    /// MAVLink TIMESYNC (S357). Tried first.
    Companion,
    /// The RTSP server's clock, from its sender reports and `GET_PARAMETER`
    /// round trips. Used only for sender reports' times.
    Rtsp,
    /// The two clocks assumed in step (S262).
    Assumed,
}

/// Why a way of matching a camera's clock is not used (S355).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClockGap {
    /// The camera is not reached this way: not over RTSP, or no companion
    /// computer serves it.
    NotServed,
    /// No sender report in the last minute.
    NoReports,
    /// No answer to a request in the last minute.
    NoAnswers,
    /// Read only one way, from the companion computer's status messages:
    /// too rough to date pictures (S357).
    OneWay,
    /// The RTSP server's sender reports say when each picture was taken but
    /// not when they were sent, as gortsplib's (`MediaMTX`) do, so its clock
    /// dates no picture.
    DatedByPictures,
    /// Not the clock the camera dates its pictures by, as for a KLV time
    /// stamp, which may come from GPS on another machine.
    OtherClock,
}

/// Where a camera says when it captured a picture (S262).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraClock {
    /// RTCP sender reports: its RTP clock at a wall-clock time.
    SenderReports,
    /// The Precision Time Stamp in its KLV (MISB ST 0601 tag 2).
    Klv,
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
    /// The encoder re-encoding it for live view now (`h264_videotoolbox`,
    /// `libx264`, ...), for a camera that sends no IDR; none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reencoding: Option<String>,
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
    /// What its picture says while it plays, such as a WebRTC camera's
    /// connection carrying too little for its pictures (ADR-0064 W7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_note: Option<String>,
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
    /// Recorded on the vehicle only (ADR-0066 R6): this computer records
    /// nothing of it while its vehicle has a camera that records it, and
    /// records it as before while the vehicle has none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle_only: bool,
    /// The companion computer serving an RTSP camera, by its id in its
    /// vehicle's profile, chosen by hand (S358): its clock dates the
    /// pictures. None: the computer at the camera address's host (S357).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
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
    /// Other addresses of this same camera, in preference order. Only one plays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_urls: Vec<String>,
    /// Direct local routes and an optional smaller stream from the same camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internet: Option<CameraInternet>,
    /// The address currently being tried or played, redacted on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_url: Option<String>,
    /// Why it is on `active_url` and since when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_choice: Option<AddressChoice>,
    /// Someone added it; it stays until removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added_by_hand: bool,
    /// Used here from the account (FB-229): removing it here leaves it in
    /// the account for the computer it came from.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_account: bool,
    /// Local device capture and its encoding provenance; absent for network cameras.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureInput>,
    /// A local file or downloaded demo, never a live camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<VideoFileInput>,
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
    /// What the picture says while it plays, such as a slow connection;
    /// its `picture_note`.
    pub picture_note: Option<String>,
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

/// Whether a camera address is a WHEP endpoint, played over WebRTC: an
/// `http://` or `https://` address among its RTSP ones (ADR-0064 W5).
#[must_use]
pub fn is_whep(url: &str) -> bool {
    let url = url.trim_start();
    ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

/// `10.0.0.5` from `rtsp://admin:secret@10.0.0.5:8554/main`, `::1` from
/// `rtsp://[::1]/main`.
#[must_use]
pub fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .map_or(bracketed, |(host, _)| host);
    }
    host_port
        .split_once(':')
        .map_or(host_port, |(host, _)| host)
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

/// How a camera that sends no IDR is re-encoded for live view (ADR-0017):
/// the computer's hardware encoder when it has one, else software.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reencoder {
    /// Hardware when it works, software when it does not.
    #[default]
    Automatic,
    /// Only the hardware encoder: fails rather than use the CPU.
    Hardware,
    /// Only software (x264), whatever hardware there is.
    Software,
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

/// How an added video was put on the flight's clock (ADR-0039 R8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlignedBy {
    /// MISB ST 0601 time stamps in the stream: exact.
    Klv,
    /// The file's own creation time.
    Metadata,
    /// The file's clock was the simulator's computer's, corrected by the
    /// vehicle's clock offset.
    VehicleClock,
    /// Set by a person: a nudge, a time, or the take-off or landing.
    Hand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoAlignment {
    pub by: AlignedBy,
    /// Where the file's own clock put its start, before any change by hand.
    pub own_start_us: u64,
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
    /// A local file or downloaded demo, never a live camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<VideoFileInput>,
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
    /// A video added to a flight from elsewhere (ADR-0039 R8): the flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// How an added video's start was put on the flight's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<VideoAlignment>,
    /// Its file is in a flight's `.uxflight` on another disk, not on this
    /// computer (ADR-0062): Move took it there, or the flight was opened
    /// from the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed: Option<PackedVideo>,
    pub version: u64,
}

/// Where in a `.uxflight` a recording is kept (ADR-0062).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackedVideo {
    /// The `.uxflight` file.
    pub path: String,
    /// The original as recorded, such as `video/<file>.ts`.
    pub entry: String,
    /// The copy every player plays, `video/<stem>.mp4`: the original
    /// itself when it is an MP4.
    pub playable: String,
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

/// An address to show (FB-229): its password and the value of any login
/// parameter hidden, its user name kept, as in `rtsp://admin@10.0.0.5/main`.
/// An address whose login cannot be told from its host shows only what
/// follows its last `@`.
#[must_use]
pub fn redact(url: &str) -> String {
    let login = split_login(url);
    let Some((scheme, rest)) = login.bare.split_once("://") else {
        return login.bare;
    };
    // What comes before its last @ may hold a password only with a colon.
    if login.ambiguous
        && let Some((before, host)) = rest.rsplit_once('@')
        && before.contains(':')
    {
        return format!("{scheme}://{host}");
    }
    match login.user.filter(|user| !user.is_empty()) {
        Some(user) => format!("{scheme}://{user}@{rest}"),
        None => login.bare.clone(),
    }
}

/// An address as it goes to the account (FB-229), and whether it had a
/// login. The user name and password before the host go, however they are
/// written (percent-encoded, with an `@`, `:`, `?`, `#` or `/` in the
/// password, before an IPv6 host), and so does the value of every parameter
/// that carries one. None when any `@` is left after that, so the login's
/// end cannot be told: nothing of that address goes. The account server
/// refuses any `@` after `://` by the same rule.
#[must_use]
pub fn without_login(url: &str) -> Option<(String, bool)> {
    let login = split_login(url);
    (!login.ambiguous).then_some((login.bare, login.had))
}

/// An address with the login typed on this computer, written as the
/// camera's players read it: percent-encoded, so an `@`, `:` or `/` in it
/// stays part of it. An address that took its user name and password as
/// parameters, as in `/user=…&password=…`, gets them back there. Any
/// other takes them before its host, replacing any login it had.
#[must_use]
pub fn with_login(url: &str, user: &str, password: &str) -> String {
    let bare = split_login(url).bare;
    let Some((scheme, rest)) = bare.split_once("://") else {
        return bare;
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(host_end);
    let mut filled = false;
    let tail = map_segments(tail, |segment| {
        let (key, value) = segment.split_once('=')?;
        if !value.is_empty() {
            return None;
        }
        let value = match login_key(key)? {
            LoginKey::User => user,
            LoginKey::Password => password,
            LoginKey::Other => return None,
        };
        filled = true;
        Some(format!("{key}={}", percent_encode(value)))
    });
    if filled {
        format!("{scheme}://{host}{tail}")
    } else if password.is_empty() {
        format!("{scheme}://{}@{host}{tail}", percent_encode(user))
    } else {
        format!(
            "{scheme}://{}:{}@{host}{tail}",
            percent_encode(user),
            percent_encode(password)
        )
    }
}

/// Whether an address as it came from the account has an emptied login
/// parameter that a user name and password cannot fill, such as a token or
/// a signature: someone edits its address on this computer instead.
#[must_use]
pub fn needs_its_address(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let tail = &rest[rest.find(['/', '?', '#']).unwrap_or(rest.len())..];
    let mut other = false;
    map_segments(tail, |segment| {
        if let Some((key, "")) = segment.split_once('=') {
            other |= login_key(key) == Some(LoginKey::Other);
        }
        None
    });
    other
}

/// An address split from its login.
struct Login {
    /// The address with its user name, password and login parameters out.
    bare: String,
    /// The user name before its host, if it had one.
    user: Option<String>,
    /// It had a login.
    had: bool,
    /// An `@` is left after its login is taken out.
    ambiguous: bool,
    /// The address from its host on, as written: its parameters' values
    /// kept.
    from_host: String,
}

fn split_login(url: &str) -> Login {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return Login {
            bare: url.to_owned(),
            user: None,
            had: false,
            ambiguous: false,
            from_host: url.to_owned(),
        };
    };
    let host_end_from = |from: usize| {
        rest[from..]
            .find(['/', '?', '#'])
            .map_or(rest.len(), |i| from + i)
    };
    // The login ends at the last @ before the path. A raw ? or # may be in
    // a password, so neither ends it, but the host ends at the first one
    // after it.
    let path = rest.find('/').unwrap_or(rest.len());
    let mut host_start = rest[..path].rfind('@').map_or(0, |at| at + 1);
    let mut host_end = host_end_from(host_start);
    // A login with a raw /: what came before it has no port, so it is no
    // host. The login ends at the first @ before a host.
    if host_start == 0 && !rest.starts_with('[') {
        let host = &rest[..host_end];
        let no_port = host
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()));
        let tail = &rest[host_end..];
        let tail = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
        let login_end = tail
            .match_indices('@')
            .map(|(i, _)| host_end + i)
            .find(|&at| {
                let next = &rest[at + 1..host_end_from(at + 1)];
                no_port && !next.is_empty() && !next.contains('@')
            });
        if let Some(at) = login_end {
            host_start = at + 1;
            host_end = host_end_from(host_start);
        }
    }
    let user = (host_start > 0).then(|| {
        let login = &rest[..host_start - 1];
        login
            .split_once(':')
            .map_or(login, |(user, _)| user)
            .to_owned()
    });
    let mut had = user.is_some();
    let mut strip = |segment: &str| {
        let (key, value) = segment.split_once('=')?;
        if value.is_empty() || login_key(key).is_none() {
            return None;
        }
        had = true;
        Some(format!("{key}="))
    };
    // A host never holds parameters, so one that seems to, such as
    // `cam;pwd=…`, is no address UXDuck can tell apart from its login.
    let raw_host = &rest[host_start..host_end];
    let host = map_segments(raw_host, &mut strip);
    let tail = map_segments(&rest[host_end..], &mut strip);
    let bare = format!("{scheme}://{host}{tail}");
    let ambiguous = bare[scheme.len() + 3..].contains('@') || raw_host.contains(HOST_PARAMETERS);
    Login {
        bare,
        user,
        had,
        ambiguous,
        from_host: rest[host_start..].to_owned(),
    }
}

/// What in a host can only be a parameter, never part of a host name.
const HOST_PARAMETERS: [char; 5] = ['=', ';', ',', '+', '&'];

/// Whether what keeps an address from the account is an `@` in its path
/// or query alone: before them is a host, with a port if any, and what
/// follows the `@` is no host, or is in the query.
#[must_use]
pub fn unclear_at_is_in_its_path(url: &str) -> bool {
    let Some((_, rest)) = url.trim().split_once("://") else {
        return false;
    };
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let plain_host = !host.contains('@')
        && !host.contains(HOST_PARAMETERS)
        && (host.starts_with('[')
            || host
                .split_once(':')
                .is_none_or(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())));
    let in_query = rest
        .find(['?', '#'])
        .is_some_and(|query| rest.find('@').is_some_and(|at| at > query));
    let after = rest.rsplit_once('@').map_or("", |(_, after)| after);
    let after = &after[..after.find(['/', '?', '#']).unwrap_or(after.len())];
    plain_host && (in_query || !after.contains(['.', ':', '[']))
}

/// Whether two addresses are one: the same but for their passwords. The
/// values of their parameters count, so `?keyframe=1` and `?keyframe=2`
/// are two addresses, though both are shown as `?keyframe=`.
#[must_use]
pub fn same_address(a: &str, b: &str) -> bool {
    let (a, b) = (split_login(a), split_login(b));
    a.from_host == b.from_host && a.user == b.user
}

/// What a parameter of a camera's address carries, if it is a login.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginKey {
    User,
    Password,
    /// A token, key, secret or signature, which no user name or password
    /// stands for.
    Other,
}

/// A parameter's key as a login, read without case, `_`, `-` or
/// percent-encoding: `password`, `Pass_Word`, `loginpas`, `X-Amz-Signature`.
fn login_key(key: &str) -> Option<LoginKey> {
    let key: String = percent_decode(key)
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .collect();
    if ["pas", "pwd", "psw"].iter().any(|part| key.contains(part)) || key == "pw" || key == "p" {
        return Some(LoginKey::Password);
    }
    if ["u", "usr", "user", "username"].contains(&key.as_str())
        || (key.contains("login") && key.contains("use"))
    {
        return Some(LoginKey::User);
    }
    let other = [
        "token",
        "secret",
        "key",
        "auth",
        "sig",
        "login",
        "credential",
        "session",
        "accesscode",
        "bearer",
    ]
    .iter()
    .any(|part| key.contains(part))
        || key == "jwt"
        || key == "sid";
    other.then_some(LoginKey::Other)
}

/// Each part of an address's path, query and fragment, replaced where
/// `replace` says. Parts end at `/ ? & ; # , +` and at an `_` before a
/// `key=` that follows a value, as in `/user=admin_password=…_channel=1`.
fn map_segments(tail: &str, mut replace: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(tail.len());
    let mut start = 0;
    let mut push = |out: &mut String, segment: &str| match replace(segment) {
        Some(replaced) => out.push_str(&replaced),
        None => out.push_str(segment),
    };
    for (i, c) in tail.char_indices() {
        let ends = matches!(c, '/' | '?' | '&' | ';' | '#' | ',' | '+')
            || (c == '_' && tail[start..i].contains('=') && starts_with_key(&tail[i + 1..]));
        if ends {
            push(&mut out, &tail[start..i]);
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    push(&mut out, &tail[start..]);
    out
}

fn starts_with_key(text: &str) -> bool {
    text.find('=').is_some_and(|eq| {
        eq > 0
            && text[..eq]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'%' | b'-' | b'_'))
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases the account server tests too (apps/api sync.test.ts).
    #[derive(serde::Deserialize)]
    struct Addresses {
        stripped: Vec<(String, String)>,
        unclear: Vec<String>,
        plain: Vec<String>,
    }

    #[test]
    fn no_form_of_a_login_goes_with_an_address() {
        let cases: Addresses =
            serde_json::from_str(include_str!("camera-addresses.json")).expect("the cases");
        for (given, kept) in &cases.stripped {
            let (shared, had) = without_login(given).expect(given);
            assert_eq!(&shared, kept, "{given}");
            assert!(had, "{given}");
            let lower = shared.to_lowercase();
            assert!(
                ["hush", "=secret", ":secret", "admin"]
                    .iter()
                    .all(|value| !lower.contains(value)),
                "{given}"
            );
            assert_eq!(without_login(kept), Some((kept.clone(), false)), "{kept}");
        }
        // Where the login ends cannot be told: none of it goes.
        for unclear in &cases.unclear {
            assert_eq!(without_login(unclear), None, "{unclear}");
        }
        for plain in &cases.plain {
            assert_eq!(
                without_login(plain),
                Some((plain.clone(), false)),
                "{plain}"
            );
        }
    }

    #[test]
    fn two_addresses_are_one_but_for_their_passwords() {
        assert!(same_address(
            "rtsp://admin:secret@cam/main?keyframe=1",
            "rtsp://admin@cam/main?keyframe=1"
        ));
        assert!(!same_address(
            "rtsp://cam/main?keyframe=1",
            "rtsp://cam/main?keyframe=2"
        ));
        assert_eq!(
            redact("rtsp://cam/main?keyframe=1"),
            redact("rtsp://cam/main?keyframe=2")
        );
        assert!(!same_address(
            "rtsp://admin@cam/main",
            "rtsp://viewer@cam/main"
        ));
    }

    #[test]
    fn a_login_typed_here_is_written_as_players_read_it() {
        assert_eq!(
            with_login("rtsp://[fd7a::1]:8554/front#track=2", "ad@min", "p:a/s s"),
            "rtsp://ad%40min:p%3Aa%2Fs%20s@[fd7a::1]:8554/front#track=2"
        );
        assert_eq!(
            with_login("rtsp://old:login@cam/x", "admin", ""),
            "rtsp://admin@cam/x"
        );
        assert_eq!(
            redact(&with_login("rtsp://cam/x", "admin", "secret")),
            "rtsp://admin@cam/x"
        );
        // An address that took its login as parameters gets it back there.
        assert_eq!(
            with_login(
                "rtsp://10.0.0.5:554/user=_password=_channel=1_stream=0.sdp",
                "admin",
                "p@ss"
            ),
            "rtsp://10.0.0.5:554/user=admin_password=p%40ss_channel=1_stream=0.sdp"
        );
        assert_eq!(
            with_login("http://cam/cgi?loginuse=&loginpas=&chn=0", "admin", "pw"),
            "http://cam/cgi?loginuse=admin&loginpas=pw&chn=0"
        );
        assert!(!needs_its_address(
            "rtsp://10.0.0.5/user=&password=&channel=1"
        ));
        // A token no user name or password stands for: its address is
        // edited here instead.
        assert!(needs_its_address("http://gw/front/whep?token="));
        assert!(!needs_its_address("rtsp://10.0.0.5/main"));
    }

    #[test]
    fn an_http_address_is_whep_and_an_rtsp_one_is_not() {
        assert!(is_whep("http://10.0.0.5:8889/front/whep"));
        assert!(is_whep("HTTPS://gw/front/whep#sha256=00"));
        assert!(!is_whep("rtsp://10.0.0.5:8554/front"));
        assert!(!is_whep("udp:5601"));
        assert!(!is_whep("http"));
        assert_eq!(
            url_host("https://viewer:secret@10.0.0.5:8889/front/whep"),
            "10.0.0.5"
        );
    }

    #[test]
    fn a_source_is_its_port_and_sender_host() {
        assert_eq!(source_id(5600, "10.0.0.5:41000"), "udp-5600-10.0.0.5");
        assert_eq!(source_id(5600, "[::1]:41000"), "udp-5600-::1");
        assert_eq!(sender_host("10.0.0.5"), "10.0.0.5");
    }

    #[test]
    fn an_rtsp_address_has_the_host_a_connection_peer_has() {
        let url = "rtsp://admin:secret@192.168.50.21:8554/front#track=2";
        assert_eq!(url_host(url), sender_host("192.168.50.21:14550"));
        assert_eq!(url_host("rtsp://100.76.177.45/front"), "100.76.177.45");
        assert_eq!(url_host("rtsp://[fd7a::1]:8554/front"), "fd7a::1");
        assert_eq!(url_host("rtsp://user@camera.local"), "camera.local");
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
        assert_eq!(redact("rtsp://admin:12/34@cam/main"), "rtsp://cam/main");
        assert_eq!(
            redact("rtsp://admin:secret@cam/x?password=secret&token=secret"),
            "rtsp://admin@cam/x?password=&token="
        );
        assert_eq!(
            redact("rtsp://10.0.0.5:554/user=admin_password=secret_channel=1"),
            "rtsp://10.0.0.5:554/user=_password=_channel=1"
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInternet {
    pub local_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_data_url: Option<String>,
}

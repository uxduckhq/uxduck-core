//! An RTSP camera (ADR-0017 V3, S39): DESCRIBE, SETUP with RTP interleaved
//! on the same TCP connection (no UDP ports to open or forward), PLAY, a
//! keepalive, and reconnecting with backoff until the source is removed.
//! Basic and Digest authentication from the address's user and password.
//! The pictures go through a `Source` to the hub, as from the UDP listener.
//!
//! A sound track (S149) is set up on channels 4 and 5 beside the video and
//! any KLV, its frames lined up with the pictures by PLAY's `RTP-Info` and
//! the two tracks' RTCP sender reports. Sound UXDuck does not record is
//! named and not set up.
//!
//! The keepalive's `GET_PARAMETER` is timed by its `CSeq`, and the video's
//! sender reports by when they were read: together they measure the
//! server's clock against this computer's (S355), so its reports date
//! pictures even when the two clocks are days apart. Once a second it asks
//! the engine for the companion computer serving the camera, whose TIMESYNC
//! clock dates them first (S357).
//!
//! A camera with addresses on two of its vehicle's connections keeps the
//! next one set up but not playing while that connection is heard, so a
//! move there needs only PLAY (ADR-0057 D12). It moves there when its own
//! address goes silent for 2 s, and with its vehicle, which alone brings
//! it back from an address the vehicle uses (D13). Each move is timed for
//! the engine and the log (D14).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uxduck_core::domain::video::{AddressMove, AddressReason, VideoTrack, is_whep, redact};
use uxduck_engine::Engine;
use uxduck_engine::video::AddressRoute;
use uxduck_media::audio::AudioUnit;
use uxduck_media::latency::Arrival;
use uxduck_media::nal::Codec;
use uxduck_media::rtcp::{AudioSync, sender_report};
use uxduck_media::rtp_audio::{AudioDepacketizer, Format};
use uxduck_media::rtp_klv::KlvDepacketizer;
use uxduck_media::source::{Sound, Source};

use super::hub::Hub;
use super::listener::{item, metadata_due, report};
use crate::daemon::engine_task::EngineHandle;

/// Waits between attempts; the last repeats.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// Nothing at all from the camera for this long: the connection is dead.
pub(super) const SILENT_FOR: Duration = Duration::from_secs(5);
/// Nothing at all from a playing camera for 1 s: ask it once whether it is
/// still there. A camera that stops its pictures still answers.
const PING_AFTER: Duration = Duration::from_secs(1);
/// Nothing at all for 2 s, not even that answer: the address has stalled,
/// and a camera whose next address is set up on a connection the vehicle
/// is heard on moves there (ADR-0057 D13). Linux, which companion
/// computers run, resends a lost packet after at least 200 ms and doubles
/// the wait each time, so the pictures of a 1 s radio hiccup resume after
/// about 1.4 s. That stays short of 2 s, so a hiccup never spends a move
/// and the 4G data the camera uses until it returns. A cut is still found
/// about 1 s before the vehicle's connection is, at 3 s.
pub(super) const STALL_AFTER: Duration = Duration::from_secs(2);
/// Setting up and starting an address, at most.
pub(super) const START_WITHIN: Duration = Duration::from_secs(15);
/// A camera that went back ahead of its vehicle and stayed this long had a
/// steady connection, so its next fall back starts the waits again
/// (ADR-0057 D13). Six times the 10 s the vehicle needs before it goes back
/// itself: a radio at the edge of its range that drops within a minute has
/// not come back.
const AHEAD_STAYS: Duration = Duration::from_mins(1);
/// How long a camera that fell back soon after it went ahead waits before
/// it goes ahead again, doubling with each such fall back up to
/// `AHEAD_WAIT_MAX`. 30 s is what a camera that left on its own waits
/// before its first probe (decision 3), so going ahead is never the quicker
/// way to flap.
const AHEAD_WAIT: Duration = Duration::from_secs(30);
/// Each fall back freezes the picture for the 2 s of a stall and the wait
/// for a keyframe. At this cap a radio that keeps dropping freezes it at
/// most once every 5 min, about 1% of the time, and one that has come back
/// for good is used again within 5 min. The vehicle still takes the camera
/// back with it as soon as it returns.
const AHEAD_WAIT_MAX: Duration = Duration::from_mins(5);
/// How long a camera on a fallback waits before it probes the earlier
/// addresses (ADR-0057 D3).
const RETURN_WAIT: Duration = Duration::from_secs(30);
/// The longest wait before a camera tries again to go back to a WHEP
/// address whose session answered but showed no keyframe within
/// `START_WITHIN`. Each such try opens a WebRTC session beside the
/// fallback and spends up to 15 s of video on the vehicle's link, which may
/// be metered, so each failed try doubles the wait from `RETURN_WAIT`. At
/// 5 min a WHEP address that answers but never carries a picture costs at
/// most 15 s of video in 5 min, and one that has come back for good is used
/// again within 5 min.
const RETURN_WAIT_MAX: Duration = Duration::from_mins(5);
/// Why playing ends when the pictures stop.
pub(super) const NO_PICTURES: &str = "no video pictures for 5 s";
const OFFLINE_LOG_EVERY: Duration = Duration::from_mins(5);
pub(super) const REPORT_EVERY: Duration = Duration::from_secs(1);
/// Sound with neither `RTP-Info` nor sender reports for this long is lined
/// up by when it arrives (S149).
const ALIGN_BY_ARRIVAL_AFTER: Duration = Duration::from_secs(3);
/// `GET_PARAMETER` once a second for the first 10 s of playing, then every
/// 5 s, or the keepalive if the server asks for one sooner: round trips
/// enough for the fastest to time its clock (S355), never a load.
const ASK_OFTEN: Duration = Duration::from_secs(1);
const ASK_OFTEN_FOR: Duration = Duration::from_secs(10);
const ASK_EVERY: Duration = Duration::from_secs(5);
const USER_AGENT: &str = concat!("UXDuck/", env!("CARGO_PKG_VERSION"));

/// An RTSP address, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub host: String,
    pub port: u16,
    /// What requests name: the address without user and password.
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// The video track chosen with `#track=N` (S90), from 1; the first
    /// playable one when absent. Never sent to the camera.
    pub track: Option<usize>,
}

/// # Errors
/// An address that is not `rtsp://[user[:password]@]host[:port][/path][#track=N]`.
pub fn address(url: &str) -> Result<Address, String> {
    let rest = url
        .strip_prefix("rtsp://")
        .ok_or_else(|| "an RTSP address starts with rtsp://".to_owned())?;
    let (rest, track) = match rest.split_once('#') {
        Some((bare, fragment)) => (
            bare,
            Some(
                fragment
                    .strip_prefix("track=")
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| format!("#{fragment} is not a track, such as #track=2"))?,
            ),
        ),
        None => (rest, None),
    };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let (credentials, host_port) = match authority.rsplit_once('@') {
        Some((c, h)) => (Some(c), h),
        None => (None, authority),
    };
    let (user, password) = match credentials {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (Some(decode(u)), Some(decode(p))),
            None => (Some(decode(c)), None),
        },
        None => (None, None),
    };
    // An IPv6 address is in brackets, which it connects without.
    let (host, port) = match host_port.strip_prefix('[') {
        Some(bracketed) => bracketed
            .split_once(']')
            .map(|(host, rest)| (host, rest.strip_prefix(':')))
            .ok_or_else(|| "the RTSP address has no closing ]".to_owned())?,
        None => match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        },
    };
    let port = port.map_or(Ok(554), |p| {
        p.parse().map_err(|_| format!("{p} is not a port"))
    })?;
    let host = host.to_owned();
    if host.is_empty() {
        return Err("the RTSP address has no host".into());
    }
    Ok(Address {
        url: format!("rtsp://{host_port}/{path}"),
        host,
        port,
        user,
        password,
        track,
    })
}

/// `%40` as `@` in a user or password.
pub(super) fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How a track's RTP carries the pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    /// H.264 (RFC 6184) or H.265 (RFC 7798) straight.
    Video(Codec),
    /// MPEG-TS (RFC 2250, MP2T), often with KLV beside the video.
    Ts,
}

/// One video track the SDP offers that UXDuck can play.
#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    pub payload: Payload,
    /// The track's address for SETUP.
    pub control: String,
    /// `sprop-parameter-sets` (H.264) or `sprop-vps`, `-sps`, `-pps` (H.265).
    pub parameter_sets: Vec<Vec<u8>>,
    /// From `a=framesize`, `a=x-dimensions` or `a=cliprect`, when given.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// From `a=framerate` or `a=x-framerate`, when given.
    pub frame_rate: Option<f64>,
}

impl Video {
    /// The track as the engine shows it (S90), numbered from 1.
    fn describe(&self, number: usize, playing: bool) -> VideoTrack {
        VideoTrack {
            number: u32::try_from(number).unwrap_or(u32::MAX),
            codec: match self.payload {
                Payload::Video(Codec::H264) => "H.264",
                Payload::Video(Codec::H265) => "H.265",
                Payload::Ts => "MPEG-TS",
            }
            .to_owned(),
            width: self.width,
            height: self.height,
            frame_rate: self.frame_rate,
            playing,
        }
    }
}

/// Every track of an SDP that UXDuck can play, in order (S90): H.264 or
/// H.265 video, or MPEG-TS, which cameras send as payload type 33 with or
/// without an `a=rtpmap`, under `m=video` or `m=application`.
///
/// # Errors
/// An SDP without one UXDuck can play.
pub fn tracks(sdp: &str, base: &str) -> Result<Vec<Video>, String> {
    let found: Vec<Video> = sections(sdp)
        .iter()
        .filter_map(|lines| track(lines, base))
        .collect();
    if found.is_empty() {
        return Err("the camera offers no H.264, H.265 or MPEG-TS video".into());
    }
    Ok(found)
}

/// An SDP's media sections, each from its `m=` line.
fn sections(sdp: &str) -> Vec<Vec<&str>> {
    let mut all: Vec<Vec<&str>> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            all.push(vec![line]);
        } else if let Some(lines) = all.last_mut() {
            lines.push(line);
        }
    }
    all
}

/// A track's address for SETUP, from its `a=control`.
fn resolve(control: Option<&str>, base: &str) -> String {
    match control {
        None | Some("*") => base.to_owned(),
        Some(c) if c.starts_with("rtsp://") => c.to_owned(),
        Some(c) => format!(
            "{}/{}",
            base.trim_end_matches('/'),
            c.trim_start_matches('/')
        ),
    }
}

/// A KLV track beside the video (RFC 6597, S91): `m=application` with
/// `smpte336m` in its `a=rtpmap`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KlvTrack {
    pub control: String,
    /// Its RTP clock, from the `a=rtpmap`; 1000 Hz when not given.
    pub clock_rate: u32,
}

/// The first KLV track of an SDP, if it has one.
pub fn klv_track(sdp: &str, base: &str) -> Option<KlvTrack> {
    sections(sdp).iter().find_map(|lines| {
        let media = lines.first()?.strip_prefix("m=")?;
        if !media.starts_with("application ") {
            return None;
        }
        let payload_types: Vec<&str> = media.split_whitespace().skip(3).collect();
        let clock_rate = lines[1..].iter().find_map(|line| {
            let (pt, encoding) = line.strip_prefix("a=rtpmap:")?.split_once(' ')?;
            let mut parts = encoding.split('/');
            let named = parts.next()?.eq_ignore_ascii_case("smpte336m");
            (named && payload_types.contains(&pt))
                .then(|| parts.next().and_then(|r| r.parse().ok()).unwrap_or(1000))
        })?;
        let control = lines[1..]
            .iter()
            .find_map(|line| line.strip_prefix("a=control:"));
        Some(KlvTrack {
            control: resolve(control, base),
            clock_rate,
        })
    })
}

/// A sound track (S149): `m=audio`, the first with a payload type UXDuck
/// can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundTrack {
    pub control: String,
    /// Its RTP clock, from the `a=rtpmap`.
    pub clock_rate: u32,
    /// How it is carried, or what to call sound UXDuck does not record.
    pub format: Result<Format, String>,
}

impl SoundTrack {
    /// What the source says before the first frame: what it is, and
    /// whether it is recorded.
    fn sound(&self) -> Sound {
        match &self.format {
            Ok(format) => Sound {
                name: AudioDepacketizer::new(*format, self.clock_rate)
                    .config()
                    .map_or_else(|| "AAC".to_owned(), |c| c.describe()),
                recorded: true,
            },
            Err(name) => Sound {
                name: name.clone(),
                recorded: false,
            },
        }
    }
}

/// The first sound track of an SDP, if it has one.
pub fn sound_track(sdp: &str, base: &str) -> Option<SoundTrack> {
    sections(sdp).iter().find_map(|lines| {
        let media = lines.first()?.strip_prefix("m=")?;
        if !media.starts_with("audio ") {
            return None;
        }
        let payload_type = media.split_whitespace().nth(3)?;
        let rtpmap = lines[1..].iter().find_map(|line| {
            let (pt, encoding) = line.strip_prefix("a=rtpmap:")?.split_once(' ')?;
            (pt == payload_type).then_some(encoding.trim())
        });
        let fmtp = lines[1..]
            .iter()
            .find_map(|line| {
                let (pt, params) = line.strip_prefix("a=fmtp:")?.split_once(' ')?;
                (pt == payload_type).then_some(params)
            })
            .unwrap_or("");
        // Static payload types need no `a=rtpmap` (RFC 3551).
        let (encoding, clock_rate) = match rtpmap {
            Some(map) => {
                let mut parts = map.split('/');
                let name = parts.next().unwrap_or("");
                (
                    name,
                    parts.next().and_then(|r| r.parse().ok()).unwrap_or(8000),
                )
            }
            None => match payload_type {
                "0" => ("PCMU", 8000),
                "8" => ("PCMA", 8000),
                "9" => ("G722", 8000),
                "14" => ("MPA", 90_000),
                _ => return None,
            },
        };
        let control = lines[1..]
            .iter()
            .find_map(|line| line.strip_prefix("a=control:"));
        Some(SoundTrack {
            control: resolve(control, base),
            clock_rate,
            format: uxduck_media::rtp_audio::format(encoding, fmtp),
        })
    })
}

/// Each track's `rtptime` from PLAY's `RTP-Info`, by its `url`.
fn rtp_info(header: &str) -> Vec<(String, u32)> {
    header
        .split(',')
        .filter_map(|entry| {
            let mut url = None;
            let mut rtptime = None;
            for field in entry.split(';').map(str::trim) {
                if let Some(u) = field.strip_prefix("url=") {
                    url = Some(u.trim_matches('"').to_owned());
                } else if let Some(t) = field.strip_prefix("rtptime=") {
                    rtptime = t.parse().ok();
                }
            }
            Some((url?, rtptime?))
        })
        .collect()
}

/// The `rtptime` `RTP-Info` gave a track, matching its control whole or
/// by its end, as cameras write either.
fn rtptime_of(info: &[(String, u32)], control: &str) -> Option<u32> {
    info.iter().find_map(|(url, time)| {
        (url == control || control.ends_with(url.as_str()) || url.ends_with(control))
            .then_some(*time)
    })
}

/// A KLV track's RTP timestamps as the video's microseconds (S91): the
/// first unit is taken as at the latest picture, and the rest follow at the
/// track's own clock. Without RTCP the two clocks share no origin.
#[derive(Debug)]
struct KlvClock {
    rate: u64,
    last: Option<u32>,
    extended: u64,
    anchor: Option<(u64, u64)>,
}

impl KlvClock {
    fn new(rate: u32) -> Self {
        Self {
            rate: u64::from(rate.max(1)),
            last: None,
            extended: 0,
            anchor: None,
        }
    }

    fn at_us(&mut self, timestamp: u32, video_us: u64) -> u64 {
        if let Some(last) = self.last {
            #[allow(clippy::cast_possible_wrap, reason = "the step between two units")]
            let step = i64::from(timestamp.wrapping_sub(last) as i32);
            self.extended = self.extended.saturating_add_signed(step);
        } else {
            self.extended = u64::from(timestamp);
        }
        self.last = Some(timestamp);
        let (ticks, us) = *self.anchor.get_or_insert((self.extended, video_us));
        let since = i128::from(self.extended) - i128::from(ticks);
        let at = i128::from(us) + since * 1_000_000 / i128::from(self.rate);
        u64::try_from(at.max(0)).unwrap_or(u64::MAX)
    }
}

/// One media section, from its `m=` line: a track UXDuck can play, or none.
fn track(lines: &[&str], base: &str) -> Option<Video> {
    let media = lines.first()?.strip_prefix("m=")?;
    let in_video = media.starts_with("video ");
    if !in_video && !media.starts_with("application ") {
        return None;
    }
    let payload_types: Vec<&str> = media.split_whitespace().skip(3).collect();
    // 33 is MP2T's static type, which needs no `a=rtpmap`.
    let mut payload = payload_types.contains(&"33").then_some(Payload::Ts);
    let mut control = None;
    let mut parameter_sets = Vec::new();
    let (mut width, mut height, mut frame_rate) = (None, None, None);
    for line in &lines[1..] {
        if let Some(map) = line.strip_prefix("a=rtpmap:") {
            let (pt, encoding) = map.split_once(' ').unwrap_or((map, ""));
            if payload_types.contains(&pt) && payload.is_none() {
                let name = encoding
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                payload = match name.as_str() {
                    "H264" if in_video => Some(Payload::Video(Codec::H264)),
                    "H265" | "HEVC" if in_video => Some(Payload::Video(Codec::H265)),
                    "MP2T" => Some(Payload::Ts),
                    _ => None,
                };
            }
        } else if let Some(c) = line.strip_prefix("a=control:") {
            control = Some(c.to_owned());
        } else if let Some(fmtp) = line.strip_prefix("a=fmtp:") {
            let params = fmtp.split_once(' ').map_or("", |(_, p)| p);
            for param in params.split(';').map(str::trim) {
                let Some((key, value)) = param.split_once('=') else {
                    continue;
                };
                if matches!(
                    key,
                    "sprop-parameter-sets" | "sprop-vps" | "sprop-sps" | "sprop-pps"
                ) {
                    for set in value.split(',') {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(set)
                            && !bytes.is_empty()
                        {
                            parameter_sets.push(bytes);
                        }
                    }
                }
            }
        } else if let Some(size) = line.strip_prefix("a=framesize:") {
            // `a=framesize:96 1920-1080`
            if let Some((w, h)) = size.split_once(' ').and_then(|(_, s)| s.split_once('-')) {
                width = w.trim().parse().ok().or(width);
                height = h.trim().parse().ok().or(height);
            }
        } else if let Some(size) = line.strip_prefix("a=x-dimensions:") {
            // `a=x-dimensions:1920,1080`
            if let Some((w, h)) = size.split_once(',') {
                width = w.trim().parse().ok().or(width);
                height = h.trim().parse().ok().or(height);
            }
        } else if let Some(rect) = line.strip_prefix("a=cliprect:") {
            // `a=cliprect:0,0,1080,1920`: top, left, bottom, right.
            let edges: Vec<u32> = rect
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            if let [top, left, bottom, right] = edges[..] {
                width = width.or(right.checked_sub(left));
                height = height.or(bottom.checked_sub(top));
            }
        } else if let Some(rate) = line
            .strip_prefix("a=framerate:")
            .or_else(|| line.strip_prefix("a=x-framerate:"))
        {
            frame_rate = rate.trim().parse::<f64>().ok().filter(|r| *r > 0.0);
        }
    }
    let payload = payload?;
    let control = resolve(control.as_deref(), base);
    Some(Video {
        payload,
        control,
        parameter_sets,
        width,
        height,
        frame_rate,
    })
}

fn md5_hex(text: &str) -> String {
    Md5::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(32), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// The `Authorization` header a challenge asks for.
#[must_use]
pub fn authorization(
    challenge: &str,
    address: &Address,
    method: &str,
    uri: &str,
) -> Option<String> {
    let user = address.user.as_deref()?;
    let password = address.password.as_deref().unwrap_or("");
    if challenge
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("basic")
    {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        return Some(format!("Basic {token}"));
    }
    let fields: HashMap<String, String> = challenge
        .trim_start()
        .get(6..)?
        .split(',')
        .filter_map(|f| f.trim().split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim_matches('"').to_owned()))
        .collect();
    let realm = fields.get("realm")?;
    let nonce = fields.get("nonce")?;
    let ha1 = md5_hex(&format!("{user}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let qop_auth = fields
        .get("qop")
        .is_some_and(|q| q.split(',').any(|v| v.trim() == "auth"));
    Some(if qop_auth {
        let cnonce = "0a4f113b";
        let response = md5_hex(&format!("{ha1}:{nonce}:00000001:{cnonce}:auth:{ha2}"));
        format!(
            r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", qop=auth, nc=00000001, cnonce="{cnonce}", response="{response}""#
        )
    } else {
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        format!(
            r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{response}""#
        )
    })
}

/// A response, or an RTP packet on an interleaved channel.
#[derive(Debug)]
pub(super) enum Frame {
    Response {
        status: u16,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    },
    Interleaved {
        channel: u8,
        data: Vec<u8>,
    },
}

/// One RTSP connection.
pub(super) struct Session {
    stream: TcpStream,
    buffer: Vec<u8>,
    /// Where each read lands, on the heap so the futures stay small.
    scratch: Vec<u8>,
    cseq: u32,
    address: Address,
    challenge: Option<String>,
    /// The `Session` header SETUP gave.
    pub(super) id: Option<String>,
    /// When the latest read came: the last bytes of the frame just parsed
    /// arrived then (S355).
    read_at: Arrival,
}

impl Session {
    pub(super) async fn connect(address: Address) -> Result<Self, String> {
        let stream = tokio::time::timeout(
            SILENT_FOR,
            TcpStream::connect((address.host.as_str(), address.port)),
        )
        .await
        .map_err(|_| "no answer".to_owned())?
        .map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        Ok(Self {
            stream,
            buffer: Vec::new(),
            scratch: vec![0; 16_384],
            cseq: 0,
            address,
            challenge: None,
            id: None,
            read_at: Arrival::now(),
        })
    }

    fn peer(&self) -> Option<SocketAddr> {
        self.stream.peer_addr().ok()
    }

    async fn send(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
    ) -> Result<(), String> {
        self.cseq += 1;
        let mut request = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n",
            self.cseq
        );
        if let Some(challenge) = &self.challenge
            && let Some(auth) = authorization(challenge, &self.address, method, uri)
        {
            let _ = write!(request, "Authorization: {auth}\r\n");
        }
        if let Some(session) = &self.id {
            let _ = write!(request, "Session: {session}\r\n");
        }
        for (name, value) in extra {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        request.push_str("\r\n");
        self.stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// A request and its answer, asking again once with credentials.
    pub(super) async fn ask(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
    ) -> Result<(HashMap<String, String>, Vec<u8>), String> {
        for attempt in 0..2 {
            self.send(method, uri, extra).await?;
            loop {
                match self.frame().await? {
                    // A late answer to an earlier request, such as a keepalive.
                    Frame::Response { headers, .. } if !answers(&headers, self.cseq) => {}
                    Frame::Response {
                        status: 401,
                        headers,
                        ..
                    } if attempt == 0 => {
                        if self.address.user.is_none() {
                            return Err(
                                "the camera wants a user and password in its address".into()
                            );
                        }
                        self.challenge = headers.get("www-authenticate").cloned();
                        break;
                    }
                    Frame::Response {
                        status: 200,
                        headers,
                        body,
                    } => return Ok((headers, body)),
                    Frame::Response { status: 401, .. } => {
                        return Err("the camera refused the user and password".into());
                    }
                    Frame::Response { status, .. } => {
                        return Err(format!("{method} was answered {status}"));
                    }
                    Frame::Interleaved { .. } => {}
                }
            }
        }
        Err(format!("{method} was not answered"))
    }

    async fn fill(&mut self) -> Result<(), String> {
        tokio::time::timeout(SILENT_FOR, self.read())
            .await
            .map_err(|_| "the camera went quiet".to_owned())?
    }

    async fn read(&mut self) -> Result<(), String> {
        let read = self
            .stream
            .read(&mut self.scratch)
            .await
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the camera closed the connection".into());
        }
        self.read_at = Arrival::now();
        self.buffer.extend_from_slice(&self.scratch[..read]);
        Ok(())
    }

    pub(super) async fn frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some((frame, used)) = parse_frame(&self.buffer)? {
                self.buffer.drain(..used);
                return Ok(frame);
            }
            self.fill().await?;
        }
    }

    /// The next frame however long it takes, for a session that is set up
    /// and not playing, which the camera has no reason to send to.
    async fn idle_frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some((frame, used)) = parse_frame(&self.buffer)? {
                self.buffer.drain(..used);
                return Ok(frame);
            }
            self.read().await?;
        }
    }

    /// `GET_PARAMETER`, which any answer shows the session still holds,
    /// and its `CSeq`.
    pub(super) async fn ping(&mut self, uri: &str) -> Result<u32, String> {
        self.send("GET_PARAMETER", uri, &[]).await?;
        Ok(self.cseq)
    }
}

/// Whether an answer is to the request with `cseq`. One without a `CSeq`
/// is taken as the answer.
fn answers(headers: &HashMap<String, String>, cseq: u32) -> bool {
    headers
        .get("cseq")
        .and_then(|c| c.parse::<u32>().ok())
        .is_none_or(|c| c == cseq)
}

/// A whole frame at the start of `b` and its length, or `None` for more.
fn parse_frame(b: &[u8]) -> Result<Option<(Frame, usize)>, String> {
    if b.first() == Some(&b'$') {
        if b.len() < 4 {
            return Ok(None);
        }
        let len = usize::from(u16::from_be_bytes([b[2], b[3]]));
        return Ok(b.get(4..4 + len).map(|data| {
            (
                Frame::Interleaved {
                    channel: b[1],
                    data: data.to_vec(),
                },
                4 + len,
            )
        }));
    }
    let Some(end) = b.windows(4).position(|w| w == b"\r\n\r\n") else {
        if b.len() > 64 * 1024 {
            return Err("an answer too long to be RTSP".into());
        }
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&b[..end]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    if !status_line.starts_with("RTSP/") {
        return Err(format!("not RTSP: {status_line}"));
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let start = end + 4;
    Ok(b.get(start..start + length).map(|body| {
        (
            Frame::Response {
                status,
                headers,
                body: body.to_vec(),
            },
            start + length,
        )
    }))
}

/// A camera address set up but not yet playing: its connection, the track
/// to play, its KLV and sound tracks if set up, and what to say about it.
/// The address a camera keeps ready to move to waits like this (ADR-0057
/// D12).
struct Prepared {
    session: Session,
    base: String,
    video: Video,
    /// Every video track offered, told to the engine when it plays.
    described: Vec<VideoTrack>,
    klv: Option<(KlvDepacketizer, KlvClock)>,
    /// The sound track and its control, when it is set up (S149).
    sound: Option<(SoundPlaying, String)>,
    /// What the source says about the sound, set up or not.
    sound_description: Option<Sound>,
    note: Option<String>,
    keepalive: Duration,
    /// The `Server` header, which says whether its sender reports are
    /// dated when sent.
    server: Option<String>,
}

/// An RTSP camera's sound track while it plays (S149).
struct SoundPlaying {
    depacketizer: AudioDepacketizer,
    sync: AudioSync,
    /// When the first sound came, for lining it up by arrival.
    first: Option<Instant>,
}

impl SoundPlaying {
    /// A packet's frames on the pictures' clock. Sound not yet lined up,
    /// or before any picture, is left out.
    fn push(&mut self, packet: &[u8], source: &Source) -> Vec<AudioUnit> {
        let Some((header, payload)) = uxduck_media::rtp::parse(packet) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for frame in self.depacketizer.push(&header, payload) {
            let first = *self.first.get_or_insert_with(Instant::now);
            if self.sync.alignment().is_none()
                && first.elapsed() >= ALIGN_BY_ARRIVAL_AFTER
                && let Some(video_rtp) = source.video_rtp()
            {
                self.sync.arrival(video_rtp, frame.timestamp);
            }
            let (Some(config), Some(at)) = (
                self.depacketizer.config(),
                self.sync
                    .video_timestamp(frame.timestamp)
                    .and_then(|t| source.video_us_at(t)),
            ) else {
                continue;
            };
            out.push(AudioUnit {
                data: frame.data,
                timestamp_us: at,
                config,
                samples: frame.samples,
            });
        }
        out
    }
}

/// SETUP of a sound track on channels 4 and 5, unless UXDuck does not
/// record its sound: the track playing, with its control for `RTP-Info`,
/// and what the source says about it. A camera that refuses it still
/// plays its video, and `note` says so.
async fn set_up_sound(
    s: &mut Session,
    source_id: &str,
    track: SoundTrack,
    note: &mut Option<String>,
) -> (Option<(SoundPlaying, String)>, Option<Sound>) {
    let description = track.sound();
    let Ok(format) = track.format else {
        return (None, Some(description));
    };
    let transport = [("Transport", "RTP/AVP/TCP;unicast;interleaved=4-5".into())];
    match s.ask("SETUP", &track.control, &transport).await {
        Ok(_) => {
            let playing = SoundPlaying {
                depacketizer: AudioDepacketizer::new(format, track.clock_rate),
                sync: AudioSync::new(track.clock_rate),
                first: None,
            };
            (Some((playing, track.control)), Some(description))
        }
        Err(error) => {
            tracing::info!(source = %source_id, %error, "an RTSP camera refused its sound track");
            let refused = "The camera refused its sound track, so recordings have no sound.";
            *note = Some(match note.take() {
                Some(klv) => format!("{refused} {klv}"),
                None => format!("{refused} The video plays."),
            });
            (None, None)
        }
    }
}

/// PLAY, and the sound lined up by PLAY's `RTP-Info` when the camera gives
/// each track's start there (S149).
async fn start_playing(
    s: &mut Session,
    base: &str,
    video: &str,
    sound: Option<(SoundPlaying, String)>,
) -> Result<Option<SoundPlaying>, String> {
    let (headers, _) = s
        .ask("PLAY", base, &[("Range", "npt=0.000-".into())])
        .await?;
    let info = headers
        .get("rtp-info")
        .map(|h| rtp_info(h))
        .unwrap_or_default();
    Ok(sound.map(|(mut playing, control)| {
        if let (Some(v), Some(a)) = (rtptime_of(&info, video), rtptime_of(&info, &control)) {
            playing.sync.rtp_info(v, a);
        }
        playing
    }))
}

/// DESCRIBE, and SETUP of the chosen track and any KLV and sound tracks:
/// everything but PLAY. `offered` hears the tracks as soon as they are
/// known, even when the one chosen is not among them.
async fn prepare(
    source_id: &str,
    address: &Address,
    offered: impl FnOnce(Vec<VideoTrack>),
) -> Result<Prepared, String> {
    let mut s = Session::connect(address.clone()).await?;
    let (headers, body) = s
        .ask(
            "DESCRIBE",
            &address.url,
            &[("Accept", "application/sdp".into())],
        )
        .await?;
    let base = headers
        .get("content-base")
        .or_else(|| headers.get("content-location"))
        .cloned()
        .unwrap_or_else(|| address.url.clone());
    let server = headers.get("server").cloned();
    let sdp = String::from_utf8_lossy(&body);
    let available = tracks(&sdp, &base)?;
    let chosen = address.track.unwrap_or(1);
    let described: Vec<VideoTrack> = available
        .iter()
        .enumerate()
        .map(|(i, t)| t.describe(i + 1, i + 1 == chosen))
        .collect();
    offered(described.clone());
    let count = available.len();
    let video = available.into_iter().nth(chosen - 1).ok_or_else(|| {
        format!(
            "the camera offers {count} video track{}, not track {chosen}",
            if count == 1 { "" } else { "s" }
        )
    })?;
    let (headers, _) = s
        .ask(
            "SETUP",
            &video.control,
            &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1".into())],
        )
        .await?;
    let session = headers.get("session").ok_or("SETUP gave no session")?;
    let timeout = session
        .split(';')
        .find_map(|p| p.trim().strip_prefix("timeout="))
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(60);
    s.id = Some(session.split(';').next().unwrap_or(session).to_owned());
    // KLV in a track of its own (S91), on the same connection. MPEG-TS
    // carries its own. A camera that refuses it still plays its video.
    let mut klv = None;
    let mut note = None;
    if video.payload != Payload::Ts
        && let Some(track) = klv_track(&sdp, &base)
    {
        match s
            .ask(
                "SETUP",
                &track.control,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3".into())],
            )
            .await
        {
            Ok(_) => {
                klv = Some((KlvDepacketizer::new(), KlvClock::new(track.clock_rate)));
            }
            Err(error) => {
                tracing::info!(source = %source_id, %error, "an RTSP camera refused its KLV track");
                note = Some(
                    "The camera refused its KLV track, so its footprint is not on the map. The video plays."
                        .to_owned(),
                );
            }
        }
    }
    // Sound in a track of its own (S149); MPEG-TS carries its own.
    let (sound, sound_description) = match sound_track(&sdp, &base) {
        Some(track) if video.payload != Payload::Ts => {
            set_up_sound(&mut s, source_id, track, &mut note).await
        }
        _ => (None, None),
    };
    Ok(Prepared {
        session: s,
        base,
        video,
        described,
        klv,
        sound,
        sound_description,
        note,
        keepalive: Duration::from_secs((timeout / 2).clamp(5, 25)),
        server,
    })
}

/// Log an outage once, then occasional reminders until pictures return.
#[derive(Default)]
pub(super) struct ConnectionLog {
    offline: Option<Offline>,
}

struct Offline {
    since: Instant,
    last_logged: Instant,
    failures: u64,
}

impl ConnectionLog {
    fn failed(&mut self, source_id: &str, error: &str, now: Instant) {
        let Some(offline) = self.offline.as_mut() else {
            self.offline = Some(Offline {
                since: now,
                last_logged: now,
                failures: 1,
            });
            tracing::info!(source = %source_id, %error, "an RTSP camera stopped");
            return;
        };
        offline.failures = offline.failures.saturating_add(1);
        if now.duration_since(offline.last_logged) >= OFFLINE_LOG_EVERY {
            offline.last_logged = now;
            tracing::info!(
                source = %source_id,
                %error,
                failures = offline.failures,
                offline_seconds = now.duration_since(offline.since).as_secs(),
                "an RTSP camera is still offline; retrying"
            );
        }
    }

    pub(super) fn receiving(&mut self, source_id: &str, now: Instant) {
        if let Some(offline) = self.offline.take() {
            tracing::info!(
                source = %source_id,
                failures = offline.failures,
                offline_seconds = now.duration_since(offline.since).as_secs(),
                "an RTSP camera recovered"
            );
        }
    }
}

/// The camera a worker serves, and how it tells the engine.
#[derive(Clone, Copy)]
pub(super) struct Camera<'a> {
    pub(super) id: &'a str,
    epoch: u64,
    pub(super) hub: &'a Hub,
    pub(super) engine: &'a EngineHandle,
}

impl Camera<'_> {
    /// Runs `event` in the engine while this worker is the camera's.
    pub(super) fn post(self, event: impl FnOnce(&mut Engine, &str) + Send + 'static) {
        let (id, epoch) = (self.id.to_owned(), self.epoch);
        self.engine
            .post(move |e| e.rtsp_event(&id, epoch, |e| event(e, &id)));
    }
}

/// This computer's clock, as the engine's.
pub(super) fn wall_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// When `at` was, on this computer's clock.
pub(super) fn wall_us_at(at: Instant) -> u64 {
    wall_us().saturating_sub(u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX))
}

/// A move to another address, timed until its first keyframe (ADR-0057
/// D14).
pub(super) struct Moving {
    pub(super) reason: AddressReason,
    pub(super) last_picture_us: Option<u64>,
    pub(super) decided_us: u64,
    pub(super) pre_opened: bool,
    pub(super) play_sent_us: Option<u64>,
    pub(super) first_picture_us: Option<u64>,
}

/// What playing an address leaves for the worker: when its last picture
/// came, and the move to it while it has not shown a keyframe yet.
#[derive(Default)]
pub(super) struct Progress {
    pub(super) last_picture: Option<Instant>,
    pub(super) moving: Option<Moving>,
}

impl Progress {
    /// A picture. The first after a move is timed, and the first keyframe
    /// ends the move: it is logged once and told to the engine.
    pub(super) fn picture(&mut self, camera: Camera<'_>, url: &str, key: bool) {
        self.last_picture = Some(Instant::now());
        let Some(moving) = self.moving.as_mut() else {
            return;
        };
        let now = wall_us();
        let first_picture_us = *moving.first_picture_us.get_or_insert(now);
        if !key {
            return;
        }
        let Some(moving) = self.moving.take() else {
            return;
        };
        let moved = AddressMove {
            last_picture_us: moving.last_picture_us,
            decided_us: moving.decided_us,
            play_sent_us: moving.play_sent_us.unwrap_or(moving.decided_us),
            first_picture_us,
            first_keyframe_us: now,
            pre_opened: moving.pre_opened,
        };
        let ms = |from: u64, to: u64| to.saturating_sub(from) / 1000;
        tracing::info!(
            source = %camera.id,
            to = %redact(url),
            reason = ?moving.reason,
            pre_opened = moved.pre_opened,
            silent_ms = ?moved.last_picture_us.map(|at| ms(at, moved.decided_us)),
            play_sent_ms = ms(moved.decided_us, moved.play_sent_us),
            first_picture_ms = ms(moved.decided_us, moved.first_picture_us),
            first_keyframe_ms = ms(moved.decided_us, moved.first_keyframe_us),
            gap_ms = ?moved.last_picture_us.map(|at| ms(at, moved.first_keyframe_us)),
            "an RTSP camera moved to another address"
        );
        camera.post(move |e, id| e.rtsp_moved(id, moved));
    }
}

/// Plays an address, set up now or already, and relays until it fails.
/// `silent` says when nothing has come for `STALL_AFTER`.
#[allow(clippy::too_many_lines, reason = "one arm per interleaved channel")]
async fn play(
    camera: Camera<'_>,
    url: &str,
    pre_opened: Option<Prepared>,
    connection_log: &mut ConnectionLog,
    silent: &tokio::sync::watch::Sender<bool>,
    progress: &mut Progress,
) -> Result<(), String> {
    let source_id = camera.id;
    let address = address(url)?;
    let (prepared, mut sound, play_sent_us) = tokio::time::timeout(START_WITHIN, async {
        let mut prepared = match pre_opened {
            Some(prepared) => {
                let described = prepared.described.clone();
                camera.post(move |e, id| e.video_tracks(id, described));
                prepared
            }
            None => {
                prepare(source_id, &address, |described| {
                    camera.post(move |e, id| e.video_tracks(id, described));
                })
                .await?
            }
        };
        let sent = wall_us();
        let sound = start_playing(
            &mut prepared.session,
            &prepared.base,
            &prepared.video.control,
            prepared.sound.take(),
        )
        .await?;
        Ok::<_, String>((prepared, sound, sent))
    })
    .await
    .map_err(|_| "the camera did not start within 15 s".to_owned())??;
    if prepared.klv.is_some() {
        camera.post(Engine::video_metadata_announced);
    }
    if let Some(moving) = progress.moving.as_mut() {
        moving.play_sent_us = Some(play_sent_us);
    }
    let Prepared {
        session: mut s,
        base,
        video,
        mut klv,
        sound_description,
        note,
        keepalive,
        server,
        ..
    } = prepared;
    let peer = s
        .peer()
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], address.port)));
    let mut source = match video.payload {
        Payload::Video(codec) => Source::rtp(codec, &video.parameter_sets),
        // As from the UDP listener: the demuxer finds the video and KLV.
        Payload::Ts => Source::new(),
    }
    .over_rtsp(server.as_deref());
    if let Some(description) = sound_description {
        source.sound_track(description);
    }
    let playing = Instant::now();
    let mut last_picture = tokio::time::Instant::now();
    // When anything at all last came, and whether this silence was asked about.
    let mut last_heard = last_picture;
    let mut pinged = false;
    let mut last_keepalive = playing;
    // The GET_PARAMETER awaiting its answer: its CSeq and when it was sent.
    let mut asked: Option<(u32, Instant)> = None;
    let mut reported = (Instant::now(), 0u64, 0u64);
    let mut klv_posted = None;
    let mut video_us = 0u64;
    let relay = |output: uxduck_media::source::Output, posted: &mut Option<Instant>| {
        if let Some(metadata) = metadata_due(&output, posted) {
            camera.post(move |e, id| e.video_klv(id, &metadata));
        }
        camera.hub.publish(source_id, &item(output));
    };
    loop {
        let mut wake = last_picture + SILENT_FOR;
        if !pinged {
            wake = wake.min(last_heard + PING_AFTER);
        }
        if !*silent.borrow() {
            wake = wake.min(last_heard + STALL_AFTER);
        }
        let Ok(frame) = tokio::time::timeout_at(wake, s.frame()).await else {
            let now = tokio::time::Instant::now();
            if now >= last_picture + SILENT_FOR {
                return Err(NO_PICTURES.to_owned());
            }
            if !pinged && now >= last_heard + PING_AFTER {
                pinged = true;
                asked = Some((s.ping(&base).await?, Instant::now()));
            }
            if now >= last_heard + STALL_AFTER {
                silent.send_replace(true);
            }
            continue;
        };
        let frame = frame?;
        last_heard = tokio::time::Instant::now();
        pinged = false;
        silent.send_if_modified(|was| std::mem::replace(was, false));
        match frame {
            Frame::Interleaved { channel: 0, data } => {
                // MPEG-TS over RTSP is kept as received; RTP video is not TS.
                camera.hub.publish_ts(source_id, &data);
                for output in source.push(&data) {
                    if let uxduck_media::source::Output::Unit(unit) = &output {
                        last_picture = tokio::time::Instant::now();
                        video_us = unit.timestamp_us;
                        connection_log.receiving(source_id, Instant::now());
                        progress.picture(camera, url, unit.key);
                    }
                    relay(output, &mut klv_posted);
                }
            }
            Frame::Interleaved { channel: 2, data } => {
                if let Some((depacketizer, clock)) = klv.as_mut()
                    && let Some((header, payload)) = uxduck_media::rtp::parse(&data)
                    && let Some(unit) = depacketizer.push(&header, payload)
                {
                    let at = clock.at_us(unit.timestamp, video_us);
                    relay(source.klv_unit(at, unit.data), &mut klv_posted);
                }
            }
            Frame::Interleaved { channel: 4, data } => {
                if let Some(sound) = sound.as_mut() {
                    for unit in sound.push(&data, &source) {
                        relay(source.audio_unit(unit), &mut klv_posted);
                    }
                }
            }
            // The video's and the sound's sender reports line them up (S149);
            // the video's dates its pictures by the camera's clock (S262).
            Frame::Interleaved { channel: 1, data } => {
                if let Some(report) = sender_report(&data) {
                    source.server_report(report, s.read_at);
                    if let Some(sound) = sound.as_mut() {
                        sound.sync.video_report(report);
                    }
                }
            }
            Frame::Interleaved { channel: 5, data } => {
                if let (Some(sound), Some(report)) = (sound.as_mut(), sender_report(&data)) {
                    sound.sync.audio_report(report);
                }
            }
            // Any answer to the keepalive is a round trip (S355).
            Frame::Response { headers, .. } => {
                if let Some((cseq, sent)) = asked
                    && answers(&headers, cseq)
                {
                    source.round_trip(sent, s.read_at.at);
                    asked = None;
                }
            }
            // KLV's RTCP.
            Frame::Interleaved { .. } => {}
        }
        if reported.0.elapsed() >= REPORT_EVERY {
            let (id, host) = (source_id.to_owned(), address.host.clone());
            source.companion(
                camera
                    .engine
                    .run(move |e| e.camera_companion(&id, &host))
                    .await
                    .ok()
                    .flatten(),
            );
            let status = source.status();
            let elapsed = reported.0.elapsed().as_secs_f64();
            let mut observed = report(
                address.port,
                &peer,
                &status,
                elapsed,
                reported.1,
                reported.2,
            );
            observed.id = Some(source_id.to_owned());
            observed.note.clone_from(&note);
            observed.counters.hold_us = camera.hub.hold_us(source_id);
            reported = (
                Instant::now(),
                status.counters.bytes,
                status.counters.pictures,
            );
            camera.post(move |e, _| e.rtsp_observed(observed));
        }
        let every = if playing.elapsed() < ASK_OFTEN_FOR {
            ASK_OFTEN
        } else {
            ASK_EVERY.min(keepalive)
        };
        if last_keepalive.elapsed() >= every {
            last_keepalive = Instant::now();
            asked = Some((s.ping(&base).await?, last_keepalive));
        }
    }
}

/// A preference probe never SETUPs or PLAYs, so the standby carries no media.
async fn probe(url: &str) -> Result<(), String> {
    if is_whep(url) {
        return super::whep::probe(url).await;
    }
    let address = address(url)?;
    let mut session = Session::connect(address.clone()).await?;
    let (_, body) = session
        .ask(
            "DESCRIBE",
            &address.url,
            &[("Accept", "application/sdp".into())],
        )
        .await?;
    let offered = tracks(&String::from_utf8_lossy(&body), &address.url)?;
    if offered.len() < address.track.unwrap_or(1) {
        return Err("the requested video track is absent".into());
    }
    Ok(())
}

/// Whether an address answers: the session kept set up there when it is
/// ready, which a `GET_PARAMETER` asks, else a DESCRIBE on a connection
/// of its own.
async fn answers_now(url: &str, kept: Option<Standby>) -> bool {
    match kept {
        Some(standby) => standby.ping().await,
        None => matches!(
            tokio::time::timeout(SILENT_FOR, probe(url)).await,
            Ok(Ok(()))
        ),
    }
}

/// The wait before the probes of the earlier addresses, after `failed`
/// tries in a row to go back to a WHEP address that showed no keyframe.
fn return_wait(failed: u32) -> Duration {
    RETURN_WAIT
        .saturating_mul(2u32.saturating_pow(failed))
        .min(RETURN_WAIT_MAX)
}

/// Wait `hold`, then for a preferred address, one of those at the places
/// `to`, to answer twice. Playback continues during these small control-only probes.
/// The delay also limits flapping when a server answers DESCRIBE but its
/// video still cannot play. `next` hears how long until each round of
/// probes, or, with nothing to go back to on its own, that there is none.
async fn preferred(
    urls: &[String],
    to: Vec<usize>,
    kept: tokio::sync::watch::Receiver<Option<Standby>>,
    next: impl Fn(Option<Duration>),
    hold: Duration,
) -> usize {
    if to.is_empty() {
        next(None);
        return std::future::pending().await;
    }
    next(Some(hold));
    tokio::time::sleep(hold).await;
    let mut previous = None;
    loop {
        let mut healthy = None;
        for &index in &to {
            let ready = ready_at(kept.borrow().as_ref(), index);
            if answers_now(&urls[index], ready).await {
                healthy = Some(index);
                break;
            }
        }
        if healthy.is_some() && healthy == previous {
            return healthy.unwrap_or(0);
        }
        previous = healthy;
        next(Some(Duration::from_secs(15)));
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

/// What the worker asks of the address it keeps set up.
enum Ask {
    /// Its session, when ready, to PLAY.
    Take(tokio::sync::oneshot::Sender<Prepared>),
    /// Whether it answers now, for a preference probe.
    Ping(tokio::sync::oneshot::Sender<bool>),
}

/// The camera's next address on another vehicle connection that is heard,
/// kept set up without playing, so a move there needs only PLAY (ADR-0057
/// D12). It closes when its last handle is dropped.
#[derive(Clone)]
struct Standby {
    index: usize,
    asks: tokio::sync::mpsc::UnboundedSender<Ask>,
    ready: tokio::sync::watch::Receiver<bool>,
}

impl Standby {
    fn open(source_id: &str, index: usize, url: &str, gate: Arc<tokio::sync::Mutex<()>>) -> Self {
        let (asks, asked) = tokio::sync::mpsc::unbounded_channel();
        let (set_up, ready) = tokio::sync::watch::channel(false);
        tokio::spawn(keep_ready(
            source_id.to_owned(),
            url.to_owned(),
            asked,
            set_up,
            gate,
        ));
        Self { index, asks, ready }
    }

    fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Its session, if it is set up.
    async fn take(&self) -> Option<Prepared> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.asks.send(Ask::Take(reply)).ok()?;
        answer.await.ok()
    }

    async fn ping(&self) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.asks.send(Ask::Ping(reply)).is_ok() && answer.await.unwrap_or(false)
    }
}

/// The address kept set up at `index`, when it is ready.
fn ready_at(kept: Option<&Standby>, index: usize) -> Option<Standby> {
    kept.filter(|s| s.index == index && s.is_ready()).cloned()
}

/// Goes back ahead of the vehicle to `url`, after `wait` and once it
/// answers: the address kept set up a `GET_PARAMETER` (ADR-0057 D13), a
/// WHEP address, never kept set up, a probe (ADR-0064 W14).
async fn go_ahead(url: &str, kept: Option<Standby>, wait: Option<Duration>) -> bool {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    answers_now(url, kept).await
}

/// How a camera that went back ahead of its vehicle fares there, so one
/// that keeps falling back waits longer each time (ADR-0057 D13).
#[derive(Debug, Default)]
struct Ahead {
    /// When it went ahead, while it is still on that address.
    since: Option<Instant>,
    /// The wait after its last fall back.
    wait: Duration,
    /// When it may go ahead again.
    not_before: Option<Instant>,
}

impl Ahead {
    fn went(&mut self, now: Instant) {
        self.since = Some(now);
    }

    /// It left the address it went ahead to: by a stall or a failure when
    /// it `fell_back`, else with its vehicle.
    fn left(&mut self, now: Instant, fell_back: bool) {
        let Some(since) = self.since.take() else {
            return;
        };
        if now.duration_since(since) >= AHEAD_STAYS {
            *self = Self::default();
        } else if fell_back {
            self.wait = (self.wait * 2).clamp(AHEAD_WAIT, AHEAD_WAIT_MAX);
            self.not_before = Some(now + self.wait);
        }
    }

    /// How long until it may go ahead again.
    fn waiting(&self, now: Instant) -> Option<Duration> {
        self.not_before
            .map(|at| at.saturating_duration_since(now))
            .filter(|left| !left.is_zero())
    }
}

/// The address to keep set up while `current` plays (ADR-0057 D12): the
/// first whose vehicle connection is heard and is not the one `current` is
/// on. None for a camera whose addresses are on no connection of its
/// vehicle: nothing says whether one is reachable. A WHEP address is never
/// kept set up: its session follows the vehicle with an ICE restart
/// instead (ADR-0064 W14).
fn standby_for(urls: &[String], routes: &[Option<AddressRoute>], current: usize) -> Option<usize> {
    let on = routes
        .get(current)
        .and_then(Option::as_ref)
        .map(|r| r.link_id.as_str());
    routes
        .iter()
        .enumerate()
        .filter_map(|(index, route)| {
            let route = route.as_ref()?;
            let whep = urls.get(index).is_some_and(|url| is_whep(url));
            (index != current && route.up && on != Some(route.link_id.as_str()) && !whep)
                .then_some((!route.returns, index))
        })
        .min()
        .map(|(_, index)| index)
}

/// Where a camera on its vehicle's connection goes back ahead of it: its
/// address on the connection the vehicle would go back to, higher in its
/// order and answering for 10 s, while a hold keeps the vehicle where it is
/// (ADR-0057 D13).
fn ahead_to(routes: &[Option<AddressRoute>], current: usize) -> Option<usize> {
    if !with_vehicle(routes, current) {
        return None;
    }
    routes.iter().enumerate().find_map(|(index, route)| {
        route
            .as_ref()
            .filter(|r| index != current && r.returns && r.up)
            .map(|_| index)
    })
}

/// Whether the address playing is on the connection the vehicle uses: the
/// camera then returns to an earlier address only with the vehicle, never
/// on its own (ADR-0057 D13).
fn with_vehicle(routes: &[Option<AddressRoute>], current: usize) -> bool {
    routes
        .get(current)
        .and_then(Option::as_ref)
        .is_some_and(|r| r.in_use)
}

/// The earlier addresses a camera playing `current` may go back to on its
/// own: those the engine allows (ADR-0057 D11), any before it says, and
/// always its address on the connection its vehicle uses, so a camera
/// that left it while the vehicle stayed comes back (ADR-0057 D13). On the
/// connection its vehicle uses, only the earlier addresses on that same
/// connection, such as the WHEP address before the RTSP one on the same
/// host: going back to one never takes the video off the vehicle's
/// connection, which is what waits for the vehicle.
fn may_go_back(
    allowed: Option<&[usize]>,
    routes: &[Option<AddressRoute>],
    current: usize,
) -> Vec<usize> {
    if let Some(here) = routes
        .get(current)
        .and_then(Option::as_ref)
        .filter(|r| r.in_use)
    {
        return (0..current)
            .filter(|index| {
                routes
                    .get(*index)
                    .and_then(Option::as_ref)
                    .is_some_and(|r| r.link_id == here.link_id)
            })
            .collect();
    }
    (0..current)
        .filter(|index| allowed.is_none_or(|to| to.contains(index)) || with_vehicle(routes, *index))
        .collect()
}

/// Keeps `url` set up and answering, setting it up again with backoff when
/// it fails, until the worker takes it or drops every handle.
async fn keep_ready(
    source_id: String,
    url: String,
    mut asks: tokio::sync::mpsc::UnboundedReceiver<Ask>,
    ready: tokio::sync::watch::Sender<bool>,
    _gate: Arc<tokio::sync::Mutex<()>>,
) {
    let Ok(address) = address(&url) else {
        return;
    };
    let mut failures = 0usize;
    loop {
        let Some(set_up) = set_up_standby(&source_id, &address, failures, &mut asks).await else {
            return;
        };
        let prepared = match set_up {
            Ok(prepared) => prepared,
            Err(error) => {
                if failures == 0 {
                    tracing::info!(
                        source = %source_id,
                        to = %redact(&url),
                        %error,
                        "an RTSP camera's next address could not be set up, retrying"
                    );
                }
                failures = failures.saturating_add(1);
                continue;
            }
        };
        ready.send_replace(true);
        tracing::info!(
            source = %source_id,
            to = %redact(&url),
            "an RTSP camera's next address is set up"
        );
        let Some(error) = keep_answering(prepared, &mut asks, &ready).await else {
            return;
        };
        ready.send_replace(false);
        tracing::info!(
            source = %source_id,
            to = %redact(&url),
            %error,
            "an RTSP camera's next address stopped answering, setting it up again"
        );
        failures = 1;
    }
}

/// Sets an address up, after the backoff `failures` earn, and tells the
/// worker it is not ready meanwhile. None once the worker drops it.
async fn set_up_standby(
    source_id: &str,
    address: &Address,
    failures: usize,
    asks: &mut tokio::sync::mpsc::UnboundedReceiver<Ask>,
) -> Option<Result<Prepared, String>> {
    let wait = failures
        .checked_sub(1)
        .map(|failed| BACKOFF[failed.min(BACKOFF.len() - 1)]);
    let setting_up = async move {
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
        tokio::time::timeout(START_WITHIN, prepare(source_id, address, drop))
            .await
            .map_err(|_| "the camera did not start within 15 s".to_owned())?
    };
    tokio::pin!(setting_up);
    loop {
        tokio::select! {
            ask = asks.recv() => match ask? {
                // Not ready: the worker sets the address up itself.
                Ask::Take(_) => {}
                Ask::Ping(reply) => {
                    let _ = reply.send(false);
                }
            },
            result = &mut setting_up => return Some(result),
        }
    }
}

/// A `GET_PARAMETER` each keepalive, and when the worker asks, each
/// answered within 5 s. Why it stopped answering, or None once the worker
/// takes it or drops it.
async fn keep_answering(
    mut prepared: Prepared,
    asks: &mut tokio::sync::mpsc::UnboundedReceiver<Ask>,
    ready: &tokio::sync::watch::Sender<bool>,
) -> Option<String> {
    let mut next_ping = tokio::time::Instant::now() + prepared.keepalive;
    // The GET_PARAMETER awaiting its answer: its CSeq, by when, and who asked.
    let mut waiting: Option<(
        u32,
        tokio::time::Instant,
        Vec<tokio::sync::oneshot::Sender<bool>>,
    )> = None;
    loop {
        let deadline = waiting.as_ref().map_or(next_ping, |w| w.1);
        tokio::select! {
            ask = asks.recv() => match ask? {
                Ask::Take(reply) => {
                    ready.send_replace(false);
                    let _ = reply.send(prepared);
                    return None;
                }
                Ask::Ping(reply) => match waiting.as_mut() {
                    Some((_, _, askers)) => askers.push(reply),
                    None => match prepared.session.ping(&prepared.base).await {
                        Ok(cseq) => {
                            let by = tokio::time::Instant::now() + SILENT_FOR;
                            waiting = Some((cseq, by, vec![reply]));
                        }
                        Err(error) => return Some(error),
                    },
                },
            },
            frame = prepared.session.idle_frame() => match frame {
                Err(error) => return Some(error),
                Ok(Frame::Response { status, headers, .. })
                    if waiting.as_ref().is_some_and(|w| answers(&headers, w.0)) =>
                {
                    // 454: the camera no longer holds the session.
                    if status == 454 {
                        return Some("the camera ended the session".to_owned());
                    }
                    for asker in waiting.take().map(|w| w.2).unwrap_or_default() {
                        let _ = asker.send(true);
                    }
                    next_ping = tokio::time::Instant::now() + prepared.keepalive;
                }
                Ok(_) => {}
            },
            () = tokio::time::sleep_until(deadline) => {
                if waiting.is_some() {
                    return Some("no answer for 5 s".to_owned());
                }
                match prepared.session.ping(&prepared.base).await {
                    Ok(cseq) => {
                        let by = tokio::time::Instant::now() + SILENT_FOR;
                        waiting = Some((cseq, by, Vec::new()));
                    }
                    Err(error) => return Some(error),
                }
            }
        }
    }
}

/// Keeps the address `standby_for` names set up, and only that one, and
/// tells the engine when it changes. `lent` holds the one the engine was
/// told of: after a move took it, `standby` is empty and `lent` is not.
fn keep_standby(
    camera: Camera<'_>,
    urls: &[String],
    current: usize,
    routes: &[Option<AddressRoute>],
    standby: &mut Option<Standby>,
    lent: &tokio::sync::watch::Sender<Option<Standby>>,
    gate: &Arc<tokio::sync::Mutex<()>>,
) {
    let wanted = standby_for(urls, routes, current);
    if lent.borrow().as_ref().map(|s| s.index) == wanted {
        return;
    }
    *standby = wanted.map(|index| Standby::open(camera.id, index, &urls[index], gate.clone()));
    lent.send_replace(standby.clone());
    let told = wanted.map(|index| (urls[index].clone(), false));
    camera.post(move |e, id| e.rtsp_standby(id, told));
}

/// The shared gate survives rapid edits that cancel a replacement worker
/// before its predecessor has finished dropping the old TCP session.
#[derive(Debug)]
pub struct Worker {
    pub task: tokio::task::JoinHandle<()>,
    gate: Arc<tokio::sync::Mutex<()>>,
    steer: tokio::sync::watch::Sender<Option<usize>>,
    routes: tokio::sync::watch::Sender<Vec<Option<AddressRoute>>>,
    may_return: tokio::sync::watch::Sender<Option<Vec<usize>>>,
}

impl Worker {
    pub fn finished(&self) -> bool {
        self.task.is_finished() && Arc::strong_count(&self.gate) == 1
    }

    /// The vehicle moved to the connection on the address at `index`
    /// (ADR-0057 D10). Only the latest move counts.
    pub fn steer(&self, index: usize) {
        self.steer.send_replace(Some(index));
    }

    /// The vehicle connection on each address's host (ADR-0057 D12).
    pub fn route(&self, routes: &[Option<AddressRoute>]) {
        self.routes.send_replace(routes.to_vec());
    }

    /// The earlier addresses, by place, it may go back to on its own
    /// (ADR-0057 D11). Only the latest counts.
    pub fn may_return(&self, to: &[usize]) {
        self.may_return.send_replace(Some(to.to_vec()));
    }
}

/// Why one address ends.
enum Move {
    Failed(String),
    Recovered(usize),
    Steered(usize, AddressReason),
    /// Nothing came for `STALL_AFTER`, and the address at this index is set up.
    Stalled(usize),
    /// Back ahead of the vehicle to the address set up at this index.
    Ahead(usize),
    /// Back to the WHEP address at this index, whose session opened beside
    /// the address playing when the return was decided and has shown its
    /// first keyframe.
    Warmed(usize, u64, Box<super::whep::Warm>),
}

/// One worker per physical camera, with at most one PLAY session. Dropping
/// `play` closes its TCP session before the next address plays.
#[allow(
    clippy::too_many_lines,
    reason = "one worker loop: pick an address, play it until it moves, back off"
)]
pub fn spawn(
    runtime: &tokio::runtime::Handle,
    source_id: String,
    urls: Vec<String>,
    epoch: u64,
    previous: Option<Worker>,
    hub: Arc<Hub>,
    engine: EngineHandle,
) -> Worker {
    let gate = previous.map_or_else(
        || Arc::new(tokio::sync::Mutex::new(())),
        |previous| {
            previous.task.abort();
            previous.gate
        },
    );
    let held = gate.clone();
    let (steer, mut steered) = tokio::sync::watch::channel(None);
    let (routes, mut heard) = tokio::sync::watch::channel(Vec::new());
    let (may_return, mut allowed) = tokio::sync::watch::channel(None::<Vec<usize>>);
    let task = runtime.spawn(async move {
        let _session = held.lock().await;
        if urls.is_empty() {
            return;
        }
        let camera = Camera {
            id: &source_id,
            epoch,
            hub: &hub,
            engine: &engine,
        };
        let mut current = 0;
        let mut reason = AddressReason::Preferred;
        let mut failures = 0usize;
        let mut connection_log = ConnectionLog::default();
        // The next address kept set up, and lent to preference probes.
        let mut standby: Option<Standby> = None;
        let (lent, _) = tokio::sync::watch::channel(None::<Standby>);
        let mut progress = Progress::default();
        // The last picture before a move, and when it was decided.
        let mut decided: Option<(Option<u64>, u64)> = None;
        let mut ahead = Ahead::default();
        // A WHEP session opened beside the address that played, to go on.
        let mut warmed: Option<super::whep::Warm> = None;
        // Tries in a row to go back to a WHEP address that showed no keyframe.
        let mut warm_failed = 0u32;
        let next_check = |wait: Option<Duration>| {
            let in_us = wait.map(|w| u64::try_from(w.as_micros()).unwrap_or(u64::MAX));
            camera.post(move |e, id| e.rtsp_next_check(id, in_us));
        };
        loop {
            // A move to a fallback that came before connecting goes first.
            let pending = steered
                .has_changed()
                .unwrap_or(false)
                .then(|| *steered.borrow())
                .flatten();
            if let Some(index) = pending.filter(|index| (1..urls.len()).contains(index)) {
                steered.mark_unchanged();
                if index != current {
                    current = index;
                    reason = AddressReason::FollowedVehicle;
                }
            }
            // The address kept set up there needs only PLAY.
            let pre_opened = match standby.take() {
                Some(kept) if kept.index == current => kept.take().await,
                kept => {
                    standby = kept;
                    None
                }
            };
            progress.moving = decided.take().map(|(last_picture_us, decided_us)| Moving {
                reason,
                last_picture_us,
                decided_us,
                pre_opened: pre_opened.is_some() || warmed.is_some(),
                play_sent_us: None,
                first_picture_us: None,
            });
            hub.restart_source(&source_id);
            let url = &urls[current];
            let active = url.clone();
            camera.post(move |e, id| e.rtsp_address(id, active, reason));
            let mut routes = heard.borrow_and_update().clone();
            keep_standby(camera, &urls, current, &routes, &mut standby, &lent, &held);
            let mut following = with_vehicle(&routes, current);
            let mut returning =
                may_go_back(allowed.borrow_and_update().as_deref(), &routes, current);
            let started = Instant::now();
            let (silent, mut silenced) = tokio::sync::watch::channel(false);
            // A WHEP session follows the vehicle with an ICE restart
            // (ADR-0064 W14), so a move to another WHEP address asks it.
            let (follow, mut follows) = tokio::sync::mpsc::unbounded_channel();
            let warm = warmed.take();
            let outcome = {
                let playing = async {
                    let (log, progress) = (&mut connection_log, &mut progress);
                    if is_whep(url) {
                        Box::pin(super::whep::play(camera, url, warm, log, &silent, progress, &mut follows))
                            .await
                    } else {
                        play(camera, url, pre_opened, log, &silent, progress).await
                    }
                };
                tokio::pin!(playing);
                let recovered = preferred(
                    &urls,
                    returning.clone(),
                    lent.subscribe(),
                    next_check,
                    return_wait(warm_failed),
                );
                tokio::pin!(recovered);
                // A WHEP session opening beside the address playing, to go
                // back to at its first keyframe: where, since when, and it.
                let mut warming = None;
                // A return to the first address waits for it to answer, and
                // the current address keeps playing meanwhile.
                let mut probing = None;
                // Ahead of the vehicle, once the connection it would go back
                // to has answered for 10 s and the address there answers.
                let mut going = None;
                // An ICE restart asked of the WHEP session: where to, why,
                // and its outcome.
                let mut restarting = None;
                let mut rethink = true;
                loop {
                    if rethink {
                        rethink = false;
                        let to = ahead_to(&routes, current);
                        let wait = to.and(ahead.waiting(Instant::now()));
                        // On the vehicle's connection with an earlier address
                        // on it, the probes of that address say when.
                        if following && (wait.is_some() || returning.is_empty()) {
                            next_check(wait);
                        }
                        going = to
                            .and_then(|to| {
                                if is_whep(&urls[to]) {
                                    return Some((to, None));
                                }
                                ready_at(standby.as_ref(), to).map(|kept| (to, Some(kept)))
                            })
                            .map(|(to, kept)| (to, Box::pin(go_ahead(&urls[to], kept, wait))));
                    }
                    tokio::select! {
                        biased;
                        Ok(()) = steered.changed() => {
                            let Some(index) = *steered.borrow_and_update() else {
                                continue;
                            };
                            probing = None;
                            if index == current || index >= urls.len() {
                                continue;
                            }
                            if index > 0 {
                                let because = AddressReason::FollowedVehicle;
                                if let Some(outcome) = super::whep::follow(&follow, &urls[current], &urls[index], because) {
                                    restarting = Some((index, because, outcome));
                                    continue;
                                }
                                break Move::Steered(index, because);
                            }
                            let kept = ready_at(standby.as_ref(), 0);
                            probing = Some(Box::pin(answers_now(&urls[0], kept)));
                        }
                        Ok(()) = heard.changed() => {
                            routes.clone_from(&heard.borrow_and_update());
                            keep_standby(
                                camera,
                                &urls,
                                current,
                                &routes,
                                &mut standby,
                                &lent,
                                &held,
                            );
                            let was = following;
                            following = with_vehicle(&routes, current);
                            rethink = true;
                            // Ahead of a vehicle that no longer follows: back
                            // with it. A fall back when the connection went quiet.
                            if reason == AddressReason::AheadOfVehicle
                                && let Some(here) = routes.get(current).and_then(Option::as_ref)
                                && !here.returns
                                && !here.in_use
                                && let Some(to) = routes
                                    .iter()
                                    .position(|r| r.as_ref().is_some_and(|r| r.in_use))
                            {
                                ahead.left(Instant::now(), !here.up);
                                let because = AddressReason::FollowedVehicle;
                                if let Some(outcome) = super::whep::follow(&follow, &urls[current], &urls[to], because) {
                                    restarting = Some((to, because, outcome));
                                    continue;
                                }
                                break Move::Steered(to, because);
                            }
                            let now = may_go_back(allowed.borrow().as_deref(), &routes, current);
                            if now != returning || (was && !following) {
                                returning = now;
                                recovered.set(preferred(&urls, returning.clone(), lent.subscribe(), next_check, return_wait(warm_failed)));
                            }
                        }
                        Some(answered) = async {
                            match probing.as_mut() {
                                Some(answer) => Some(answer.await),
                                None => None,
                            }
                        }, if probing.is_some() => {
                            probing = None;
                            if answered {
                                let because = AddressReason::ReturnedWithVehicle;
                                if let Some(outcome) = super::whep::follow(&follow, &urls[current], &urls[0], because) {
                                    restarting = Some((0, because, outcome));
                                    continue;
                                }
                                break Move::Steered(0, because);
                            }
                        }
                        Some((to, answered)) = async {
                            match going.as_mut() {
                                Some((to, going)) => Some((*to, going.await)),
                                None => None,
                            }
                        }, if going.is_some() => {
                            going = None;
                            if answered {
                                let because = AddressReason::AheadOfVehicle;
                                if let Some(outcome) = super::whep::follow(&follow, &urls[current], &urls[to], because) {
                                    restarting = Some((to, because, outcome));
                                    continue;
                                }
                                break Move::Ahead(to);
                            }
                        }
                        Some(restarted) = async {
                            match restarting.as_mut() {
                                Some((_, _, outcome)) => Some(outcome.await),
                                None => None,
                            }
                        }, if restarting.is_some() => {
                            let Some((index, because, _)) = restarting.take() else {
                                continue;
                            };
                            if !matches!(restarted, Ok(Ok(()))) {
                                let error = match restarted {
                                    Ok(Err(error)) => error,
                                    _ => "the WHEP session ended".to_owned(),
                                };
                                tracing::info!(source = %source_id, %error, "a WHEP camera could not restart ICE, so it opens a new session");
                                // A new session there, then the next address (W14).
                                break if because == AddressReason::AheadOfVehicle {
                                    Move::Ahead(index)
                                } else {
                                    Move::Steered(index, because)
                                };
                            }
                            // The same session plays on at the new address.
                            current = index;
                            reason = because;
                            failures = 0;
                            match because {
                                AddressReason::AheadOfVehicle => ahead.went(Instant::now()),
                                _ => ahead.left(Instant::now(), false),
                            }
                            let active = urls[current].clone();
                            camera.post(move |e, id| e.rtsp_address(id, active, because));
                            keep_standby(camera, &urls, current, &routes, &mut standby, &lent, &held);
                            following = with_vehicle(&routes, current);
                            returning = may_go_back(allowed.borrow().as_deref(), &routes, current);
                            recovered.set(preferred(&urls, returning.clone(), lent.subscribe(), next_check, return_wait(warm_failed)));
                            rethink = true;
                        }
                        Ok(()) = allowed.changed() => {
                            let now = may_go_back(allowed.borrow_and_update().as_deref(), &routes, current);
                            if now != returning {
                                returning = now;
                                recovered.set(preferred(&urls, returning.clone(), lent.subscribe(), next_check, return_wait(warm_failed)));
                            }
                        }
                        result = &mut playing => {
                            break Move::Failed(result.err().unwrap_or_default());
                        }
                        Ok(()) = async { silenced.wait_for(|s| *s).await.map(drop) },
                            if standby.as_ref().is_some_and(Standby::is_ready) => {
                            break Move::Stalled(standby.as_ref().map_or(0, |s| s.index));
                        }
                        Ok(()) = async {
                            match standby.as_mut() {
                                Some(kept) => kept.ready.changed().await,
                                None => std::future::pending().await,
                            }
                        }, if standby.is_some() => {
                            let told = standby
                                .as_ref()
                                .map(|s| (urls[s.index].clone(), s.is_ready()));
                            camera.post(move |e, id| e.rtsp_standby(id, told));
                            rethink = true;
                        }
                        index = &mut recovered, if current > 0
                            && warming.is_none()
                            && (!following || !returning.is_empty()) => {
                            if !is_whep(&urls[index]) {
                                break Move::Recovered(index);
                            }
                            // The RTSP address plays on until WebRTC's first
                            // keyframe, so the picture is never empty.
                            tracing::info!(
                                source = %source_id,
                                to = %redact(&urls[index]),
                                "a camera opens WebRTC beside the address playing, to go back at its first keyframe"
                            );
                            warming = Some((index, wall_us(), Box::pin(super::whep::warm(&urls[index]))));
                        }
                        Some((index, began_us, result)) = async {
                            match warming.as_mut() {
                                Some((index, began_us, warm)) => Some((*index, *began_us, warm.await)),
                                None => None,
                            }
                        }, if warming.is_some() => {
                            warming = None;
                            match result {
                                Ok(warm) => {
                                    warm_failed = 0;
                                    break Move::Warmed(index, began_us, Box::new(warm));
                                }
                                Err(error) => {
                                    warm_failed = warm_failed.saturating_add(1);
                                    let wait = return_wait(warm_failed);
                                    tracing::info!(
                                        source = %source_id,
                                        to = %redact(&urls[index]),
                                        %error,
                                        wait_s = wait.as_secs(),
                                        "a camera could not go back to WebRTC, so it stays and tries again later"
                                    );
                                    recovered.set(preferred(&urls, returning.clone(), lent.subscribe(), next_check, wait));
                                }
                            }
                        }
                    }
                }
            }; // `playing` and its socket have been dropped here.
            // An ICE restart may have moved it on.
            let url = &urls[current];
            decided = Some((progress.last_picture.take().map(wall_us_at), wall_us()));
            if let Some(moving) = progress.moving.take() {
                tracing::info!(
                    source = %source_id,
                    to = %redact(url),
                    reason = ?moving.reason,
                    "an RTSP camera left an address before its first keyframe"
                );
            }
            let (next, because, why) = match outcome {
                Move::Recovered(index) => (
                    index,
                    AddressReason::Preferred,
                    "Returning to a preferred camera address".to_owned(),
                ),
                Move::Steered(index, because) => (
                    index,
                    because,
                    "Following the vehicle's connection".to_owned(),
                ),
                Move::Stalled(index) => (
                    index,
                    AddressReason::Stalled,
                    format!("Nothing from {} for 2 s", redact(url)),
                ),
                Move::Ahead(index) => (
                    index,
                    AddressReason::AheadOfVehicle,
                    "Going back ahead of the vehicle".to_owned(),
                ),
                Move::Warmed(index, began_us, warm) => {
                    warmed = Some(*warm);
                    // Timed from when the return was decided and its
                    // session opened, beside the address that played.
                    decided = decided.map(|(last_picture_us, _)| (last_picture_us, began_us));
                    (
                        index,
                        AddressReason::Preferred,
                        "Returning to WebRTC on a preferred camera address".to_owned(),
                    )
                }
                Move::Failed(error) => {
                    if started.elapsed() > Duration::from_secs(30) {
                        failures = 0;
                    }
                    connection_log.failed(&source_id, &error, Instant::now());
                    let because = if error == NO_PICTURES {
                        AddressReason::NoPictures
                    } else {
                        AddressReason::ConnectionFailed
                    };
                    (
                        (current + 1) % urls.len(),
                        because,
                        format!("Can't reach {}: {error}", redact(url)),
                    )
                }
            };
            failures = match because {
                AddressReason::NoPictures | AddressReason::ConnectionFailed => {
                    failures.saturating_add(1)
                }
                _ => 0,
            };
            let now = Instant::now();
            match because {
                AddressReason::AheadOfVehicle => ahead.went(now),
                AddressReason::Stalled
                | AddressReason::NoPictures
                | AddressReason::ConnectionFailed => ahead.left(now, true),
                _ => ahead.left(now, false),
            }
            current = next;
            reason = because;
            let id = source_id.clone();
            // Finish detaching the current recording before new media can
            // arrive. Posting without awaiting can mix two session clocks
            // in the old segment while the engine is busy.
            if engine
                .run(move |e| e.rtsp_event(&id, epoch, |e| e.rtsp_stopped(&id, why)))
                .await
                .is_err()
            {
                return;
            }
            if failures >= urls.len() {
                tokio::time::sleep(BACKOFF[(failures / urls.len() - 1).min(BACKOFF.len() - 1)])
                    .await;
            }
        }
    });
    Worker {
        task,
        gate,
        steer,
        routes,
        may_return,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs(run: impl FnOnce()) -> String {
        use std::io::Write;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(bytes.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn outage_logs_are_throttled_until_video_returns_and_reset_for_each_outage() {
        let output = logs(|| {
            let now = Instant::now();
            let mut camera = ConnectionLog::default();
            // An initial healthy connection is not a recovery.
            camera.receiving("camera", now);
            for seconds in [0, 1, 2, 299, 300, 301, 599, 600] {
                // Changing errors must not bypass the throttle.
                let error = if seconds % 2 == 0 {
                    "no answer"
                } else {
                    "closed"
                };
                camera.failed("camera", error, now + Duration::from_secs(seconds));
            }
            camera.receiving("camera", now + Duration::from_secs(603));
            camera.receiving("camera", now + Duration::from_secs(604));
            camera.failed("camera", "closed", now + Duration::from_secs(604));
            camera.receiving("camera", now + Duration::from_secs(605));
            // Each source has its own throttle.
            let mut other = ConnectionLog::default();
            other.failed("other", "no answer", now + Duration::from_secs(605));
        });
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 7, "{output}");
        assert!(lines[0].contains("an RTSP camera stopped"), "{output}");
        for (line, seconds) in [(lines[1], 300), (lines[2], 600)] {
            assert!(line.contains("is still offline; retrying"), "{output}");
            assert!(
                line.contains(&format!("offline_seconds={seconds}")),
                "{output}"
            );
        }
        assert!(lines[3].contains("an RTSP camera recovered"), "{output}");
        assert!(
            lines[3].contains("failures=8 offline_seconds=603"),
            "{output}"
        );
        assert!(lines[4].contains("an RTSP camera stopped"), "{output}");
        assert!(lines[5].contains("an RTSP camera recovered"), "{output}");
        assert!(
            lines[5].contains("failures=1 offline_seconds=1"),
            "{output}"
        );
        assert!(lines[6].contains("source=other"), "{output}");
    }

    #[tokio::test]
    async fn with_nothing_to_go_back_to_a_camera_waits_for_its_vehicle() {
        let told = std::sync::Mutex::new(Vec::new());
        let urls = vec!["rtsp://127.0.0.1:9/radio".to_owned()];
        let (_lent, kept) = tokio::sync::watch::channel(None);
        let waited = tokio::time::timeout(
            Duration::from_millis(100),
            preferred(
                &urls,
                Vec::new(),
                kept,
                |next| {
                    told.lock().unwrap().push(next);
                },
                return_wait(0),
            ),
        )
        .await;
        assert!(waited.is_err(), "it never goes back on its own");
        assert_eq!(
            *told.lock().unwrap(),
            [None],
            "and says there is no next check, not one in 30 s"
        );
    }

    #[test]
    fn an_overnight_outage_logs_at_most_one_reminder_every_five_minutes() {
        let output = logs(|| {
            let now = Instant::now();
            let mut camera = ConnectionLog::default();
            for seconds in (0..=8 * 60 * 60).step_by(15) {
                camera.failed("camera", "no answer", now + Duration::from_secs(seconds));
            }
        });
        assert_eq!(output.matches("an RTSP camera stopped").count(), 1);
        assert_eq!(output.matches("is still offline; retrying").count(), 96);
        assert!(!output.contains("recovered"));
    }

    fn video(sdp: &str, base: &str) -> Result<Video, String> {
        tracks(sdp, base).map(|mut all| all.swap_remove(0))
    }

    #[test]
    fn an_address_comes_apart_and_requests_never_carry_the_password() {
        let a = address("rtsp://admin:p%40ss@192.168.144.25:8554/main.264").unwrap();
        assert_eq!(a.host, "192.168.144.25");
        assert_eq!(a.port, 8554);
        assert_eq!(a.url, "rtsp://192.168.144.25:8554/main.264");
        assert_eq!(a.user.as_deref(), Some("admin"));
        assert_eq!(a.password.as_deref(), Some("p@ss"));
        assert_eq!(address("rtsp://cam").unwrap().port, 554);
        assert!(address("http://cam").is_err());
        // An IPv6 address connects without its brackets.
        let v6 = address("rtsp://[::1]:8554/front").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8554));
        assert_eq!(v6.url, "rtsp://[::1]:8554/front");
        assert_eq!(address("rtsp://[fd7a::1]/front").unwrap().host, "fd7a::1");
    }

    #[test]
    fn the_sdp_gives_the_codec_its_track_and_parameter_sets() {
        let sdp = "v=0\r\nm=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/48000\r\na=control:trackID=2\r\n\
                   m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
                   a=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z0IAH+kCwEkg,aM4xsg==\r\n\
                   a=control:trackID=1\r\n";
        let v = video(sdp, "rtsp://cam/main/").unwrap();
        assert_eq!(v.payload, Payload::Video(Codec::H264));
        assert_eq!(v.control, "rtsp://cam/main/trackID=1");
        assert_eq!(v.parameter_sets.len(), 2);
        assert_eq!(v.parameter_sets[0][0], 0x67);
        let hevc = "m=video 0 RTP/AVP 98\r\na=rtpmap:98 H265/90000\r\na=control:rtsp://cam/v\r\n";
        assert_eq!(
            video(hevc, "rtsp://cam/").unwrap().payload,
            Payload::Video(Codec::H265)
        );
        assert_eq!(video(hevc, "rtsp://cam/").unwrap().control, "rtsp://cam/v");
        assert!(video("m=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\n", "x").is_err());
    }

    #[test]
    fn mpeg_ts_is_a_track_with_or_without_an_rtpmap() {
        // Static type 33, no rtpmap, after a track UXDuck can't play.
        let sdp = "v=0\r\nm=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\na=control:trackID=0\r\n\
                   m=video 0 RTP/AVP 33\r\na=control:trackID=1\r\n";
        let v = video(sdp, "rtsp://cam/").unwrap();
        assert_eq!(v.payload, Payload::Ts);
        assert_eq!(v.control, "rtsp://cam/trackID=1");
        // A dynamic type named MP2T, under m=application.
        let dynamic = "m=application 0 RTP/AVP 96\r\na=rtpmap:96 MP2T/90000\r\n";
        assert_eq!(video(dynamic, "rtsp://cam/").unwrap().payload, Payload::Ts);
        // H.264 is video only: not under m=application.
        let data = "m=application 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
        assert!(video(data, "x").is_err());
    }

    #[test]
    fn every_playable_track_is_offered_in_order_with_its_size_and_rate() {
        // A gimbal's EO and IR, with audio and a JPEG preview between them.
        let sdp = "v=0\r\n\
                   m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=framesize:96 1920-1080\r\n\
                   a=framerate:30\r\na=control:trackID=0\r\n\
                   m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/48000\r\na=control:trackID=1\r\n\
                   m=video 0 RTP/AVP 26\r\na=rtpmap:26 JPEG/90000\r\na=control:trackID=2\r\n\
                   m=video 0 RTP/AVP 98\r\na=rtpmap:98 H265/90000\r\na=x-dimensions:640,512\r\n\
                   a=x-framerate:9\r\na=control:trackID=3\r\n\
                   m=application 0 RTP/AVP 33\r\na=cliprect:0,0,720,1280\r\na=control:trackID=4\r\n";
        let all = tracks(sdp, "rtsp://cam/").unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].payload, Payload::Video(Codec::H264));
        assert_eq!((all[0].width, all[0].height), (Some(1920), Some(1080)));
        assert_eq!(all[0].frame_rate, Some(30.0));
        assert_eq!(all[1].payload, Payload::Video(Codec::H265));
        assert_eq!(all[1].control, "rtsp://cam/trackID=3");
        assert_eq!((all[1].width, all[1].height), (Some(640), Some(512)));
        assert_eq!(all[1].frame_rate, Some(9.0));
        assert_eq!(all[2].payload, Payload::Ts);
        assert_eq!((all[2].width, all[2].height), (Some(1280), Some(720)));
        assert_eq!(all[2].frame_rate, None);
        let described = all[1].describe(2, true);
        assert_eq!(described.codec, "H.265");
        assert!(described.playing);
    }

    #[test]
    fn a_klv_track_is_found_beside_the_video_and_timed_against_it() {
        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=1\r\n\
                   m=application 0 RTP/AVP 97\r\na=rtpmap:97 SMPTE336M/1000\r\na=control:trackID=2\r\n";
        assert_eq!(
            klv_track(sdp, "rtsp://cam/live/"),
            Some(KlvTrack {
                control: "rtsp://cam/live/trackID=2".into(),
                clock_rate: 1000
            })
        );
        // MPEG-TS under m=application is video, not a KLV track; it is not
        // listed as one, and its own KLV comes through the demuxer.
        let ts = "m=application 0 RTP/AVP 33\r\na=control:trackID=1\r\n";
        assert_eq!(klv_track(ts, "rtsp://cam/"), None);
        assert_eq!(tracks(sdp, "rtsp://cam/live/").unwrap().len(), 1);

        // The first unit lands on the latest picture; the next ones follow
        // at 1000 Hz, across the timestamp's wrap.
        let mut clock = KlvClock::new(1000);
        assert_eq!(clock.at_us(u32::MAX - 49, 5_000_000), 5_000_000);
        assert_eq!(clock.at_us(50, 9_000_000), 5_100_000);
        assert_eq!(clock.at_us(150, 0), 5_200_000);
    }

    #[test]
    fn a_sound_track_is_found_with_its_format_or_named_when_not_recorded() {
        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=1\r\n\
                   m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/48000/2\r\n\
                   a=fmtp:97 streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190\r\n\
                   a=control:trackID=2\r\n";
        let track = sound_track(sdp, "rtsp://cam/live/").unwrap();
        assert_eq!(track.control, "rtsp://cam/live/trackID=2");
        assert_eq!(track.clock_rate, 48_000);
        assert!(matches!(track.format, Ok(Format::Mpeg4Generic { .. })));
        assert_eq!(
            track.sound(),
            Sound {
                name: "AAC, 48 kHz, stereo".into(),
                recorded: true
            }
        );
        // The sound track is never offered as video.
        assert_eq!(tracks(sdp, "rtsp://cam/live/").unwrap().len(), 1);

        let latm =
            "m=audio 0 RTP/AVP 98\r\na=rtpmap:98 MP4A-LATM/44100/2\r\na=fmtp:98 cpresent=1\r\n";
        let track = sound_track(latm, "rtsp://cam/").unwrap();
        assert_eq!(track.control, "rtsp://cam/");
        assert_eq!(
            track.sound().name,
            "AAC",
            "until the stream gives its configuration"
        );

        let opus = "m=audio 0 RTP/AVP 111\r\na=rtpmap:111 opus/48000/2\r\na=control:audio\r\n";
        assert_eq!(
            sound_track(opus, "rtsp://cam/").unwrap().sound().name,
            "Opus, 48 kHz, mono"
        );

        // G.711 by its static type, with no rtpmap: named, not recorded.
        let g711 = "m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\nm=audio 0 RTP/AVP 0\r\na=control:trackID=2\r\n";
        assert_eq!(
            sound_track(g711, "rtsp://cam/").unwrap().sound(),
            Sound {
                name: "G.711 µ-law".into(),
                recorded: false
            }
        );
        assert_eq!(sound_track(RTSP_VIDEO_ONLY, "rtsp://cam/"), None);
    }

    const RTSP_VIDEO_ONLY: &str = "m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";

    #[test]
    fn rtp_info_gives_each_tracks_start_by_its_address() {
        let info = rtp_info(
            "url=rtsp://cam/live/trackID=1;seq=1000;rtptime=3000000, url=trackID=2;seq=7;rtptime=160000",
        );
        assert_eq!(
            info,
            [
                ("rtsp://cam/live/trackID=1".to_owned(), 3_000_000),
                ("trackID=2".to_owned(), 160_000)
            ]
        );
        assert_eq!(
            rtptime_of(&info, "rtsp://cam/live/trackID=1"),
            Some(3_000_000)
        );
        assert_eq!(
            rtptime_of(&info, "rtsp://cam/live/trackID=2"),
            Some(160_000)
        );
        assert_eq!(rtptime_of(&info, "rtsp://cam/live/trackID=3"), None);
        // An entry without its rtptime says nothing.
        assert!(rtp_info("url=rtsp://cam/a;seq=1").is_empty());
    }

    #[test]
    fn a_track_chosen_on_the_address_never_goes_on_the_wire() {
        let a = address("rtsp://admin:pw@cam:8554/live#track=2").unwrap();
        assert_eq!(a.track, Some(2));
        assert_eq!(a.url, "rtsp://cam:8554/live");
        assert_eq!(address("rtsp://cam#track=3").unwrap().url, "rtsp://cam/");
        assert_eq!(address("rtsp://cam/live").unwrap().track, None);
        assert!(address("rtsp://cam/live#track=0").is_err());
        assert!(address("rtsp://cam/live#stream=2").is_err());
    }

    #[test]
    fn digest_authentication_matches_rfc_2617s_example() {
        // RFC 2617 section 3.5, with RTSP's method and the same values.
        let a = Address {
            host: "h".into(),
            port: 554,
            url: "rtsp://h/".into(),
            user: Some("Mufasa".into()),
            password: Some("Circle Of Life".into()),
            track: None,
        };
        let header = authorization(
            r#"Digest realm="testrealm@host.com", qop="auth", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#,
            &a,
            "GET",
            "/dir/index.html",
        )
        .unwrap();
        assert!(
            header.contains(r#"response="6629fae49393a05397450978507c4ef1""#),
            "{header}"
        );
        let basic = authorization(r#"Basic realm="cam""#, &a, "DESCRIBE", "rtsp://h/").unwrap();
        assert_eq!(basic, "Basic TXVmYXNhOkNpcmNsZSBPZiBMaWZl");
    }

    fn route(link: &str, up: bool, in_use: bool) -> AddressRoute {
        AddressRoute {
            link_id: link.into(),
            up,
            in_use,
            returns: false,
        }
    }

    #[test]
    fn the_address_kept_set_up_is_the_first_on_another_connection_the_vehicle_is_heard_on() {
        let radio = Some(route("radio", true, true));
        let internet = Some(route("internet", true, false));
        assert_eq!(
            standby_for(&[], &[radio.clone(), internet.clone()], 0),
            Some(1)
        );
        // A second address on the radio's host is on the same connection.
        assert_eq!(
            standby_for(&[], &[radio.clone(), radio.clone(), internet.clone()], 0),
            Some(2)
        );
        // Not while the vehicle is not heard there.
        assert_eq!(
            standby_for(&[], &[radio, Some(route("internet", false, false))], 0),
            None
        );
        // Playing the internet address: the radio one, once heard again.
        let back = [
            Some(route("radio", true, false)),
            Some(route("internet", true, true)),
        ];
        assert_eq!(standby_for(&[], &back, 1), Some(0));
        // An address on no connection plays with a heard one kept set up.
        assert_eq!(standby_for(&[], &[None, internet], 0), Some(1));
        // Without a vehicle nothing says an address is reachable.
        assert_eq!(standby_for(&[], &[None, None], 0), None);
        assert_eq!(standby_for(&[], &[], 0), None);
    }

    /// A WHEP address is never kept set up: a WHEP camera follows its
    /// vehicle with an ICE restart instead (ADR-0064 W14). The RTSP address
    /// after it on that connection is kept set up.
    #[test]
    fn a_whep_address_is_never_the_one_kept_set_up() {
        let urls = [
            "http://radio:8889/cam/whep".to_owned(),
            "http://internet:8889/cam/whep".to_owned(),
            "rtsp://radio:8554/cam".to_owned(),
            "rtsp://internet:8554/cam".to_owned(),
        ];
        let radio = Some(route("radio", true, true));
        let internet = Some(route("internet", true, false));
        let routes = [radio.clone(), internet.clone(), radio, internet];
        assert_eq!(standby_for(&urls, &routes, 0), Some(3));
        assert_eq!(standby_for(&urls[..2], &routes[..2], 0), None);
    }

    /// The engine says which earlier addresses a camera may go back to on
    /// its own (ADR-0057 D11). Its address on the connection its vehicle
    /// uses is always one, so a camera that stalled away while the vehicle
    /// stayed comes back (ADR-0057 D13).
    #[test]
    fn a_camera_may_go_back_to_what_the_engine_allows_and_to_its_vehicles_connection() {
        let routes = [
            Some(route("radio", true, true)),
            Some(route("internet", true, false)),
            None,
        ];
        assert_eq!(may_go_back(None, &routes, 2), [0, 1], "before it is told");
        assert_eq!(may_go_back(Some(&[]), &routes, 2), [0]);
        assert_eq!(may_go_back(Some(&[1]), &routes, 2), [0, 1]);
        assert_eq!(may_go_back(Some(&[0, 1, 2]), &routes, 1), [0]);
        let elsewhere = [
            Some(route("radio", true, false)),
            Some(route("internet", true, true)),
        ];
        assert!(may_go_back(Some(&[]), &elsewhere, 1).is_empty());
        assert!(may_go_back(None, &[], 0).is_empty());
    }

    /// The Plane's camera on Tailscale: a WHEP and an RTSP address on the
    /// radio, then on Tailscale, the vehicle on Tailscale. On the RTSP
    /// address there after WHEP failed, it may go back to WHEP on the same
    /// connection on its own, and still waits for the vehicle for the radio,
    /// whatever the engine allows.
    #[test]
    fn on_its_vehicles_connection_a_camera_may_go_back_to_an_earlier_address_on_that_connection() {
        let routes = [
            Some(route("radio", true, false)),
            Some(route("radio", true, false)),
            Some(route("tailscale", true, true)),
            Some(route("tailscale", true, true)),
        ];
        assert_eq!(may_go_back(None, &routes, 3), [2]);
        assert_eq!(may_go_back(Some(&[0, 1]), &routes, 3), [2]);
        assert_eq!(may_go_back(Some(&[]), &routes, 3), [2]);
        assert!(may_go_back(None, &routes, 2).is_empty());
    }

    /// Each failed try to go back to WHEP doubles the wait, up to 5 min.
    #[test]
    fn a_failed_return_to_whep_waits_longer_each_time() {
        let waits: Vec<u64> = (0..7).map(|n| return_wait(n).as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(return_wait(u32::MAX), RETURN_WAIT_MAX);
    }

    /// A camera on its vehicle's connection goes back ahead of it only to an
    /// address on the connection the vehicle would go back to but a hold
    /// keeps it off, and keeps that address set up first (ADR-0057 D13).
    #[test]
    fn a_camera_goes_ahead_only_to_the_connection_its_held_vehicle_would_go_back_to() {
        let returning = AddressRoute {
            returns: true,
            ..route("radio", true, false)
        };
        let routes = [
            Some(route("lan", true, false)),
            Some(returning.clone()),
            Some(route("internet", true, true)),
        ];
        assert_eq!(ahead_to(&routes, 2), Some(1));
        assert_eq!(
            standby_for(&[], &routes, 2),
            Some(1),
            "set up before the LAN"
        );
        // Not from an address the vehicle does not use, nor to one not heard.
        assert_eq!(ahead_to(&routes, 0), None);
        let unheard = AddressRoute {
            up: false,
            ..returning
        };
        assert_eq!(
            ahead_to(&[Some(unheard), Some(route("internet", true, true))], 1),
            None
        );
        // Nothing held: the vehicle goes back itself, and the camera with it.
        assert_eq!(
            ahead_to(
                &[
                    Some(route("radio", true, false)),
                    Some(route("internet", true, true))
                ],
                1
            ),
            None
        );
    }

    /// One that falls back soon after it went ahead waits 30 s, then twice
    /// as long each time up to 5 min. One that stayed a minute starts again
    /// (ADR-0057 D13).
    #[test]
    fn a_camera_that_keeps_falling_back_waits_longer_each_time_and_starts_again_once_it_stays() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut ahead = Ahead::default();
        assert_eq!(ahead.waiting(start), None);
        let mut waits = Vec::new();
        let mut now = 0;
        for _ in 0..6 {
            ahead.went(at(now));
            ahead.left(at(now + 10), true);
            waits.push(ahead.waiting(at(now + 10)).unwrap().as_secs());
            now += 10 + waits.last().unwrap();
        }
        assert_eq!(waits, [30, 60, 120, 240, 300, 300]);
        assert_eq!(ahead.waiting(at(now)), None, "over once it has waited");
        // Moved on with its vehicle: no fall back, the wait is kept.
        ahead.went(at(now));
        ahead.left(at(now + 5), false);
        ahead.went(at(now + 10));
        ahead.left(at(now + 20), true);
        assert_eq!(ahead.waiting(at(now + 20)), Some(Duration::from_mins(5)));
        // Stayed a minute: the next fall back waits 30 s again.
        now += 320;
        ahead.went(at(now));
        ahead.left(at(now + 61), true);
        assert_eq!(ahead.waiting(at(now + 61)), None);
        ahead.went(at(now + 100));
        ahead.left(at(now + 110), true);
        assert_eq!(ahead.waiting(at(now + 110)), Some(Duration::from_secs(30)));
    }

    #[test]
    fn only_an_address_on_the_connection_the_vehicle_uses_stays_with_it() {
        let routes = [
            Some(route("radio", true, false)),
            Some(route("internet", true, true)),
        ];
        assert!(with_vehicle(&routes, 1));
        assert!(!with_vehicle(&routes, 0));
        assert!(!with_vehicle(&[None, None], 1));
        assert!(!with_vehicle(&[], 0));
    }

    /// The address kept set up answers keepalives. One that comes late
    /// must not be taken for PLAY's answer, whose `RTP-Info` lines up the
    /// sound (S149).
    #[tokio::test]
    async fn a_late_answer_to_a_keepalive_is_not_taken_for_plays() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let camera = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut asked = Vec::new();
            let mut buffer = [0u8; 1024];
            while asked.windows(4).filter(|w| w == b"\r\n\r\n").count() < 2 {
                let read = stream.read(&mut buffer).await.unwrap();
                asked.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(
                    b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n\
                      RTSP/1.0 200 OK\r\nCSeq: 2\r\nRTP-Info: url=rtsp://cam/v;rtptime=9\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let at = address(&format!("rtsp://127.0.0.1:{port}/v")).unwrap();
        let mut session = Session::connect(at).await.unwrap();
        assert_eq!(session.ping("rtsp://cam/v").await.unwrap(), 1);
        let (headers, _) = session.ask("PLAY", "rtsp://cam/v", &[]).await.unwrap();
        assert_eq!(
            headers.get("rtp-info").map(String::as_str),
            Some("url=rtsp://cam/v;rtptime=9")
        );
        camera.await.unwrap();
        let without = HashMap::new();
        assert!(answers(&without, 3), "an answer without a CSeq is taken");
    }

    #[test]
    fn frames_are_read_from_a_mixed_stream() {
        let mut b = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 2\r\n\r\nhi".to_vec();
        b.extend_from_slice(&[b'$', 0, 0, 3, 1, 2, 3, b'$']);
        let (first, used) = parse_frame(&b).unwrap().unwrap();
        assert!(matches!(first, Frame::Response { status: 200, ref body, .. } if body == b"hi"));
        let (second, used2) = parse_frame(&b[used..]).unwrap().unwrap();
        assert!(
            matches!(second, Frame::Interleaved { channel: 0, ref data } if data == &[1, 2, 3])
        );
        assert!(parse_frame(&b[used + used2..]).unwrap().is_none());
    }
}

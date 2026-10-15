//! An RTSP camera (ADR-0017 V3, S39): DESCRIBE, SETUP with RTP interleaved
//! on the same TCP connection (no UDP ports to open or forward), PLAY, a
//! keepalive, and reconnecting with backoff until the source is removed.
//! Basic and Digest authentication from the address's user and password.
//! The pictures go through a `Source` to the hub, as from the UDP listener.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uxduck_core::domain::video::{VideoTrack, redact};
use uxduck_media::nal::Codec;
use uxduck_media::rtp_klv::KlvDepacketizer;
use uxduck_media::source::Source;

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
const SILENT_FOR: Duration = Duration::from_secs(5);
const REPORT_EVERY: Duration = Duration::from_secs(1);
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
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
            h.to_owned(),
            p.parse().map_err(|_| format!("{p} is not a port"))?,
        ),
        _ => (host_port.to_owned(), 554),
    };
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
fn decode(s: &str) -> String {
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
enum Frame {
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
struct Session {
    stream: TcpStream,
    buffer: Vec<u8>,
    /// Where each read lands, on the heap so the futures stay small.
    scratch: Vec<u8>,
    cseq: u32,
    address: Address,
    challenge: Option<String>,
    /// The `Session` header SETUP gave.
    id: Option<String>,
}

impl Session {
    async fn connect(address: Address) -> Result<Self, String> {
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
    async fn ask(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, String)],
    ) -> Result<(HashMap<String, String>, Vec<u8>), String> {
        for attempt in 0..2 {
            self.send(method, uri, extra).await?;
            loop {
                match self.frame().await? {
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
        let read = tokio::time::timeout(SILENT_FOR, self.stream.read(&mut self.scratch))
            .await
            .map_err(|_| "the camera went quiet".to_owned())?
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the camera closed the connection".into());
        }
        self.buffer.extend_from_slice(&self.scratch[..read]);
        Ok(())
    }

    async fn frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some((frame, used)) = parse_frame(&self.buffer)? {
                self.buffer.drain(..used);
                return Ok(frame);
            }
            self.fill().await?;
        }
    }
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

/// A camera asked to play: its connection, the track playing, its KLV
/// track if it has one set up, and what to say about it.
struct Playing {
    session: Session,
    base: String,
    video: Video,
    klv: Option<(KlvDepacketizer, KlvClock)>,
    note: Option<String>,
    keepalive: Duration,
}

/// DESCRIBE, SETUP of the chosen track and any KLV track, and PLAY.
async fn set_up(
    source_id: &str,
    address: &Address,
    engine: &EngineHandle,
) -> Result<Playing, String> {
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
    let sdp = String::from_utf8_lossy(&body);
    let offered = tracks(&sdp, &base)?;
    let chosen = address.track.unwrap_or(1);
    let described: Vec<VideoTrack> = offered
        .iter()
        .enumerate()
        .map(|(i, t)| t.describe(i + 1, i + 1 == chosen))
        .collect();
    let id = source_id.to_owned();
    engine.post(move |e| e.video_tracks(&id, described));
    let count = offered.len();
    let video = offered.into_iter().nth(chosen - 1).ok_or_else(|| {
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
            Ok(_) => klv = Some((KlvDepacketizer::new(), KlvClock::new(track.clock_rate))),
            Err(error) => {
                tracing::info!(source = %source_id, %error, "an RTSP camera refused its KLV track");
                note = Some(
                    "The camera refused its KLV track, so its footprint is not on the map. The video plays."
                        .to_owned(),
                );
            }
        }
    }
    s.ask("PLAY", &base, &[("Range", "npt=0.000-".into())])
        .await?;
    Ok(Playing {
        session: s,
        base,
        video,
        klv,
        note,
        keepalive: Duration::from_secs((timeout / 2).clamp(5, 25)),
    })
}

/// Connects, plays and relays until the connection fails.
async fn play(source_id: &str, url: &str, hub: &Hub, engine: &EngineHandle) -> Result<(), String> {
    let address = address(url)?;
    let Playing {
        session: mut s,
        base,
        video,
        mut klv,
        note,
        keepalive,
    } = set_up(source_id, &address, engine).await?;
    let peer = s
        .peer()
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], address.port)));
    let mut source = match video.payload {
        Payload::Video(codec) => Source::rtp(codec, &video.parameter_sets),
        // As from the UDP listener: the demuxer finds the video and KLV.
        Payload::Ts => Source::new(),
    };
    let mut last_keepalive = Instant::now();
    let mut reported = (Instant::now(), 0u64, 0u64);
    let mut klv_posted = None;
    let mut video_us = 0u64;
    let relay = |output: uxduck_media::source::Output, posted: &mut Option<Instant>| {
        if let Some(metadata) = metadata_due(&output, posted) {
            let id = source_id.to_owned();
            engine.post(move |e| e.video_klv(&id, &metadata));
        }
        hub.publish(source_id, &item(output));
    };
    loop {
        match s.frame().await? {
            Frame::Interleaved { channel: 0, data } => {
                for output in source.push(&data) {
                    if let uxduck_media::source::Output::Unit(unit) = &output {
                        video_us = unit.timestamp_us;
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
            // RTCP and keepalive answers.
            Frame::Interleaved { .. } | Frame::Response { .. } => {}
        }
        if reported.0.elapsed() >= REPORT_EVERY {
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
            reported = (
                Instant::now(),
                status.counters.bytes,
                status.counters.pictures,
            );
            engine.post(move |e| e.video_observed(observed));
        }
        if last_keepalive.elapsed() >= keepalive {
            last_keepalive = Instant::now();
            s.send("GET_PARAMETER", &base, &[]).await?;
        }
    }
}

/// Keeps an RTSP source playing, reconnecting with backoff, until aborted.
pub fn spawn(
    runtime: &tokio::runtime::Handle,
    source_id: String,
    url: String,
    hub: Arc<Hub>,
    engine: EngineHandle,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let mut failures = 0usize;
        loop {
            let started = Instant::now();
            let error = play(&source_id, &url, &hub, &engine)
                .await
                .err()
                .unwrap_or_default();
            // Playing for a while is a fresh start for the backoff.
            if started.elapsed() > Duration::from_secs(30) {
                failures = 0;
            }
            let why = format!("Can't reach {}: {error}", redact(&url));
            tracing::info!(source = %source_id, %error, "an RTSP camera stopped");
            let id = source_id.clone();
            engine.post(move |e| e.video_stopped_because(&id, Some(why)));
            tokio::time::sleep(BACKOFF[failures.min(BACKOFF.len() - 1)]).await;
            failures += 1;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

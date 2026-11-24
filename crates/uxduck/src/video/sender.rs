//! UXDuck's video sender (ADR-0064 W6 to W9 and W14, S385): a feature of
//! `uxduck companion` on the computer that serves the vehicle's cameras. For
//! each camera it reads the `MediaMTX` path over localhost RTSP while someone
//! watches, and sends the pictures over WebRTC without re-encoding them,
//! answering WHEP itself on the ports and paths `MediaMTX`'s own WebRTC uses: TCP
//! 8889 for `/<path>/whep` and UDP 8189 for every session's media, with host
//! candidates only on the interfaces named, so a camera's WHEP address
//! does not change when the sender takes over from `MediaMTX`.
//!
//! `str0m`'s Google Congestion Control estimates what each viewer's path
//! carries from its TWCC feedback, its pacer spreads the pictures out, and
//! NACKs are answered with RTX. The lowest estimate among a camera's viewers
//! sets its encoder through `MediaMTX`'s API (`rule.rs`, `mediamtx.rs`), live and
//! whether or not the vehicle is armed (W9), because a live change of
//! bitrate, frame rate or keyframe interval never stops the camera. When
//! the last viewer leaves, the camera goes back to its configured settings.
//!
//! A WHEP PATCH with new ICE credentials restarts ICE in the same session
//! (W14), as `MediaMTX` does: the session's offer is taken again with the new
//! credentials and the PATCH's candidates, which `MediaMTX` reads only under a
//! numeric `a=mid`, the m-line's index, and the answer's credentials and
//! candidates go back. A PATCH without credentials adds its candidates.
//!
//! Every camera's WHEP answer carries sound (ADR-0067 M9): an offer's audio
//! m-line, Opus only, is answered `sendonly` whether or not a microphone is
//! there, so one plugged in later needs no new session. A viewer gets sound
//! only after it asks, with a PATCH to the session's address whose body is
//! `{"sound": true}` or `{"sound": false}` (`application/json`), which changes
//! nothing in the media, so the picture never blinks. `GET` on the session's
//! address answers its sound as the PATCH does. Each listening viewer gets
//! the live step its own estimate allows (`listening.rs`), and while anyone
//! listens the camera follows the lowest estimate less what that viewer's
//! sound costs (M6). The sound's packets carry the time their sound was
//! captured on the pictures' clock, so the sender reports line them up.
//!
//! `GET /` answers what each camera follows and its sound, and the
//! microphones, as JSON.
//!
//! WHEP is plain HTTP here: the certificate made on the vehicle and pinned
//! over SSH (W10) comes with S384. DTLS-SRTP encrypts the media either way.

mod listening;
pub(in crate::video) mod mediamtx;
mod reader;
mod rule;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

use super::companion::sound::{self, CameraSound, Feed, STEPS};
use listening::Listener;
pub use mediamtx::{Api, Camera};
pub use reader::Frame;
pub use rule::{Rule, Settings};

const SERVER: &str = concat!("UXDuck/", env!("CARGO_PKG_VERSION"));
/// A viewer whose ICE has been disconnected this long is let go, as
/// `MediaMTX` lets one go about 30 s after its connection went (ADR-0064's
/// cloud test). Until then it can come back with an ICE restart (W14), and
/// its estimate no longer counts as the lowest.
const LOST_FOR: Duration = Duration::from_secs(30);
/// How often the configured interfaces are read for addresses, so one that
/// comes up late, as Tailscale's does about 150 s after boot, is used.
const INTERFACES_EVERY: Duration = Duration::from_secs(2);
/// How often the sound's RTCP sender reports go, which the daemon lines
/// the sound up with the pictures by (ADR-0067 M11): str0m's default of
/// 5 s for audio would leave a newly unmuted camera's sound lined up by
/// arrival for that long.
const SOUND_REPORTS_EVERY: Duration = Duration::from_secs(1);
/// The floor a camera follows down to unless set (ADR-0064 W7, the
/// maintainer's choice on 2026-11-17).
const FLOOR: u64 = 150_000;

/// The `[video]` section of `/etc/uxduck/companion.toml` (ADR-0061 B4). The
/// cameras are its `[cameras.<path>]` tables, shared with recording.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `MediaMTX`'s API, with `user:password@` when it asks for one.
    #[serde(default = "default_api")]
    pub mediamtx_api: String,
    /// `MediaMTX`'s RTSP server, where each camera's path is read.
    #[serde(default = "default_rtsp")]
    pub mediamtx_rtsp: String,
    /// The interfaces whose addresses are offered to viewers, such as the
    /// radio LAN and `tailscale0`. A modem's is never named.
    pub interfaces: Vec<String>,
    /// Where WHEP is answered.
    #[serde(default = "default_whep")]
    pub whep: SocketAddr,
    /// The UDP port every session's media uses, on each interface's address.
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    /// The cameras' configured settings, kept across a restart of the sender.
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
}

/// A camera the sender answers for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraConfig {
    /// The `MediaMTX` path, such as `front`.
    pub path: String,
    /// The lowest bitrate the camera follows down to, bit/s.
    pub floor_bitrate: u64,
    /// Whether a Raspberry Pi camera follows the connection. One that does
    /// not is sent at its own rate.
    pub follow: bool,
}

pub(crate) fn default_api() -> String {
    "http://127.0.0.1:9997".into()
}
pub(crate) fn default_rtsp() -> String {
    "rtsp://127.0.0.1:8554".into()
}
fn default_whep() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8889))
}
fn default_udp_port() -> u16 {
    8189
}
fn default_state_file() -> PathBuf {
    PathBuf::from("/var/lib/uxduck/video-sender.json")
}
pub(crate) fn default_floor() -> u64 {
    FLOOR
}

impl Config {
    /// # Errors
    /// What is wrong with it, in words a person can act on.
    pub fn check(&self, cameras: &[CameraConfig]) -> Result<(), String> {
        if cameras.is_empty() {
            return Err(
                "[video] has no camera to send: add one as [cameras.front], named by its MediaMTX path"
                    .into(),
            );
        }
        if self.interfaces.is_empty() {
            return Err("[video] names no interface: add the ones viewers reach, such as interfaces = [\"eth0\", \"tailscale0\"]".into());
        }
        let mut seen = BTreeSet::new();
        for camera in cameras {
            let path = &camera.path;
            let allowed = |c: char| c.is_ascii_alphanumeric() || "_-./~".contains(c);
            if path.is_empty()
                || path.starts_with('/')
                || path.ends_with('/')
                || !path.chars().all(allowed)
            {
                return Err(format!(
                    "{path:?} is not a MediaMTX path, such as \"front\""
                ));
            }
            if path.split('/').any(|part| part == "whep") {
                return Err(format!(
                    "{path:?} has \"whep\" in it, which WHEP's addresses use"
                ));
            }
            if !seen.insert(path) {
                return Err(format!("the camera {path} is named twice"));
            }
            if camera.floor_bitrate == 0 {
                return Err(format!("the camera {path} needs a floor_bitrate above 0"));
            }
        }
        if self.udp_port == 0 {
            return Err("[video] udp_port must be a port, such as 8189".into());
        }
        Ok(())
    }
}

/// Where viewers are offered addresses: the configured interfaces, or
/// addresses given straight, for tests.
#[derive(Clone, Debug)]
pub enum Hosts {
    Interfaces(Vec<String>),
    #[cfg_attr(
        not(all(test, target_os = "linux")),
        expect(dead_code, reason = "tests give addresses straight")
    )]
    Addresses(Vec<IpAddr>),
}

pub struct Options {
    pub api: Api,
    pub hosts: Hosts,
    pub whep: SocketAddr,
    pub udp_port: u16,
    pub state_file: PathBuf,
    pub cameras: Vec<CameraConfig>,
    /// The frame rate each camera records at on the vehicle while it does
    /// (ADR-0066), by its path: the lowest it may follow down to.
    pub recording: watch::Receiver<BTreeMap<String, f64>>,
    /// Each camera's sound and its packets (ADR-0067).
    pub sound: Feed,
}

/// The sender, running.
pub struct Running {
    /// Where WHEP is answered.
    #[cfg_attr(
        not(all(test, target_os = "linux")),
        expect(dead_code, reason = "tests ask it")
    )]
    pub http: SocketAddr,
    pub task: tokio::task::JoinHandle<Result<(), String>>,
    /// The paths someone watches, to read, and where their pictures go.
    pub wanted: watch::Receiver<BTreeSet<String>>,
    pub pictures: mpsc::Sender<(String, Frame)>,
}

/// Runs the sender from `companion.toml`'s `[video]` until it fails.
///
/// # Errors
/// Bad configuration, `MediaMTX` not answering at the start, a port in use,
/// or a failure later.
pub async fn run(
    config: Config,
    cameras: Vec<CameraConfig>,
    recording: watch::Receiver<BTreeMap<String, f64>>,
    sound: Feed,
) -> Result<(), String> {
    config.check(&cameras)?;
    let running = start(Options {
        api: Api::new(&config.mediamtx_api)?,
        hosts: Hosts::Interfaces(config.interfaces.clone()),
        whep: config.whep,
        udp_port: config.udp_port,
        state_file: config.state_file.clone(),
        cameras,
        recording,
        sound,
    })
    .await?;
    let readers = tokio::spawn(read(
        config.mediamtx_rtsp.trim_end_matches('/').to_owned(),
        running.wanted.clone(),
        running.pictures.clone(),
    ));
    let ended = running.task.await;
    readers.abort();
    ended.map_err(|e| format!("the video sender stopped: {e}"))?
}

/// Reads each path someone watches from `MediaMTX`'s RTSP server, and stops
/// reading it when nobody does.
async fn read(
    rtsp: String,
    mut wanted: watch::Receiver<BTreeSet<String>>,
    pictures: mpsc::Sender<(String, Frame)>,
) {
    let mut reading: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
    loop {
        let now = wanted.borrow_and_update().clone();
        reading.retain(|path, task| {
            let keep = now.contains(path);
            if !keep {
                task.abort();
            }
            keep
        });
        for path in now {
            reading.entry(path.clone()).or_insert_with(|| {
                tokio::spawn(reader::read(
                    path.clone(),
                    format!("{rtsp}/{path}"),
                    pictures.clone(),
                ))
            });
        }
        if wanted.changed().await.is_err() {
            for task in reading.values() {
                task.abort();
            }
            return;
        }
    }
}

/// Reads the cameras from `MediaMTX`, binds WHEP's port and the media port on
/// each address, and starts serving. The configured settings go back to
/// every camera first (W8).
///
/// # Errors
/// `MediaMTX` not answering, a path it does not have, or a port in use.
#[allow(
    clippy::too_many_lines,
    reason = "the sender's parts started in order, each needing the one before"
)]
pub async fn start(options: Options) -> Result<Running, String> {
    let paths: Vec<String> = options.cameras.iter().map(|c| c.path.clone()).collect();
    let (started, cameras) =
        mediamtx::configured(&options.api, &options.state_file, &paths).await?;
    for config in &options.cameras {
        match &cameras[&config.path] {
            Camera::RpiCamera(_) if !config.follow => tracing::info!(
                path = %config.path,
                "following the connection is off for this camera, so it is sent at its own rate"
            ),
            Camera::RpiCamera(own) if config.floor_bitrate > own.bitrate => tracing::warn!(
                path = %config.path,
                floor = config.floor_bitrate,
                bitrate = own.bitrate,
                "the camera's floor is above its bitrate, so it follows down to its bitrate only"
            ),
            Camera::RpiCamera(own) => {
                tracing::info!(path = %config.path, ?own, "a Raspberry Pi camera, following the connection");
            }
            Camera::Other { source } => tracing::info!(
                path = %config.path,
                %source,
                "not a Raspberry Pi camera, so it is sent at its own rate"
            ),
        }
    }
    let (datagrams_tx, datagrams) = mpsc::channel(1024);
    let mut sockets = Sockets {
        hosts: options.hosts,
        port: options.udp_port,
        bound: BTreeMap::new(),
        datagrams: datagrams_tx,
        missing: BTreeSet::new(),
    };
    sockets.refresh(true).await?;
    let listener = tokio::net::TcpListener::bind(options.whep).await.map_err(|e| {
        format!(
            "the video sender cannot answer WHEP on {}: {e}. Is MediaMTX's own WebRTC still on? Set webrtc: no in its configuration first",
            options.whep
        )
    })?;
    let http = listener
        .local_addr()
        .map_err(|e| format!("the video sender's WHEP port: {e}"))?;
    let (requests_tx, requests) = mpsc::channel(64);
    let router = axum::Router::new().fallback(answer).with_state(requests_tx);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!(%error, "the video sender's WHEP server stopped");
        }
    });

    let (configured_tx, configured) = watch::channel(cameras.clone());
    let (wanted_tx, wanted) = watch::channel(BTreeMap::new());
    tokio::spawn(mediamtx::keep(
        options.api,
        options.state_file,
        started,
        configured_tx,
        wanted,
    ));
    let (paths_tx, paths_rx) = watch::channel(BTreeSet::new());
    let (pictures_tx, pictures) = mpsc::channel(256);
    let core = Core {
        cameras: options
            .cameras
            .iter()
            .map(|config| {
                (
                    config.path.clone(),
                    CameraState {
                        floor: config.floor_bitrate,
                        follow: config.follow,
                        record_fps: None,
                        camera: cameras[&config.path].clone(),
                        rule: None,
                        made: 0,
                        made_last: 0,
                        estimate: None,
                    },
                )
            })
            .collect(),
        viewers: HashMap::new(),
        sockets,
        paths: paths_tx,
        wanted: wanted_tx.clone(),
        sound: options.sound.status.borrow().clone(),
    };
    tracing::info!(%http, port = options.udp_port, "the video sender answers WHEP");
    Ok(Running {
        http,
        task: tokio::spawn(core.serve(
            requests,
            datagrams,
            pictures,
            configured,
            options.recording,
            options.sound,
        )),
        wanted: paths_rx,
        pictures: pictures_tx,
    })
}

/// A request from WHEP's HTTP side to the sender's task.
enum Request {
    Offer {
        path: String,
        sdp: String,
        reply: oneshot::Sender<Reply>,
    },
    Patch {
        path: String,
        id: String,
        content_type: String,
        body: String,
        reply: oneshot::Sender<Reply>,
    },
    Delete {
        path: String,
        id: String,
        reply: oneshot::Sender<Reply>,
    },
    /// The session's sound (ADR-0067 M9).
    Session {
        path: String,
        id: String,
        reply: oneshot::Sender<Reply>,
    },
    Status {
        reply: oneshot::Sender<Reply>,
    },
}

struct Reply {
    status: StatusCode,
    headers: Vec<(header::HeaderName, String)>,
    content_type: &'static str,
    body: String,
}

impl Reply {
    fn text(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.body).into_response();
        let headers = response.headers_mut();
        let mut set = |name: header::HeaderName, value: &str| {
            if let Ok(value) = header::HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        };
        set(header::SERVER, SERVER);
        set(header::CONTENT_TYPE, self.content_type);
        for (name, value) in self.headers {
            set(name, &value);
        }
        response
    }
}

/// WHEP over HTTP: `POST /<path>/whep` with an offer, `PATCH` and `DELETE` on
/// the session's address, `OPTIONS` for a probe, and `GET /` for the status.
async fn answer(
    State(core): State<mpsc::Sender<Request>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(body) = String::from_utf8(body.to_vec()) else {
        return Reply::text(StatusCode::BAD_REQUEST, "The request's body is not text.")
            .into_response();
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let full = uri.path();
    let (reply, replied) = oneshot::channel();
    let request = if full == "/" && method == Method::GET {
        Request::Status { reply }
    } else if let Some(path) = full.strip_prefix('/').and_then(|p| p.strip_suffix("/whep")) {
        match method {
            Method::OPTIONS => {
                return Reply {
                    status: StatusCode::NO_CONTENT,
                    headers: vec![
                        (header::ALLOW, "OPTIONS, POST".into()),
                        (
                            header::HeaderName::from_static("accept-post"),
                            "application/sdp".into(),
                        ),
                    ],
                    content_type: "text/plain; charset=utf-8",
                    body: String::new(),
                }
                .into_response();
            }
            Method::POST if content_type != "application/sdp" => {
                return Reply::text(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "A WHEP offer is application/sdp.",
                )
                .into_response();
            }
            Method::POST => Request::Offer {
                path: path.to_owned(),
                sdp: body,
                reply,
            },
            _ => return Reply::text(StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
        }
    } else if let Some((path, id)) = full.strip_prefix('/').and_then(|p| p.rsplit_once("/whep/")) {
        let (path, id) = (path.to_owned(), id.to_owned());
        match method {
            Method::PATCH => Request::Patch {
                path,
                id,
                content_type,
                body,
                reply,
            },
            Method::DELETE => Request::Delete { path, id, reply },
            Method::GET => Request::Session { path, id, reply },
            _ => return Reply::text(StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
        }
    } else {
        return Reply::text(StatusCode::NOT_FOUND, "Not a WHEP address.").into_response();
    };
    if core.send(request).await.is_err() {
        return Reply::text(StatusCode::SERVICE_UNAVAILABLE, "The video sender stopped.")
            .into_response();
    }
    replied.await.map_or_else(
        |_| {
            Reply::text(StatusCode::SERVICE_UNAVAILABLE, "The video sender stopped.")
                .into_response()
        },
        IntoResponse::into_response,
    )
}

/// A datagram on one of the media sockets.
struct Datagram {
    local: SocketAddr,
    from: SocketAddr,
    at: Instant,
    data: Vec<u8>,
}

struct Bound {
    socket: Arc<UdpSocket>,
    interface: String,
    reader: tokio::task::JoinHandle<()>,
    /// Bytes sent from it, which say which connection carries the video.
    sent: u64,
}

impl Drop for Bound {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// The media port on each address of the configured interfaces.
struct Sockets {
    hosts: Hosts,
    port: u16,
    bound: BTreeMap<SocketAddr, Bound>,
    datagrams: mpsc::Sender<Datagram>,
    /// Interfaces with no address, said once each.
    missing: BTreeSet<String>,
}

impl Sockets {
    /// Each interface's addresses now: never loopback unless given
    /// straight, nor an IPv6 link-local address, which needs its interface
    /// named to be reached.
    fn addresses(&mut self) -> Vec<(String, IpAddr)> {
        let names = match &self.hosts {
            Hosts::Addresses(ips) => {
                return ips.iter().map(|ip| ("given".to_owned(), *ip)).collect();
            }
            Hosts::Interfaces(names) => names.clone(),
        };
        let found: Vec<(String, IpAddr)> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| names.contains(&i.name))
            .map(|i| (i.name.clone(), i.ip()))
            .filter(|(_, ip)| match ip {
                IpAddr::V4(v4) => !v4.is_loopback(),
                IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unicast_link_local(),
            })
            .collect();
        for name in &names {
            let has = found.iter().any(|(n, _)| n == name);
            if !has && self.missing.insert(name.clone()) {
                tracing::warn!(interface = %name, "no address on this interface yet, so viewers there are not offered it");
            }
            if has && self.missing.remove(name) {
                tracing::info!(interface = %name, "the interface has an address now");
            }
        }
        found
    }

    /// Binds the media port on new addresses and lets gone ones go. At the
    /// start, an address that cannot be bound is an error.
    async fn refresh(&mut self, first: bool) -> Result<(), String> {
        let now = self.addresses();
        self.bound
            .retain(|address, _| now.iter().any(|(_, ip)| *ip == address.ip()));
        for (interface, ip) in now {
            if self.bound.keys().any(|a| a.ip() == ip) {
                continue;
            }
            let socket = match UdpSocket::bind((ip, self.port)).await {
                Ok(socket) => Arc::new(socket),
                Err(e) => {
                    let error = format!(
                        "the video sender cannot use UDP {} on {ip} ({interface}): {e}. Is MediaMTX's own WebRTC still on? Set webrtc: no in its configuration first",
                        self.port
                    );
                    if first {
                        return Err(error);
                    }
                    if self.missing.insert(format!("{ip}")) {
                        tracing::error!(%error);
                    }
                    continue;
                }
            };
            let local = socket
                .local_addr()
                .map_err(|e| format!("the video sender's UDP socket on {ip}: {e}"))?;
            self.missing.remove(&format!("{ip}"));
            tracing::info!(%local, %interface, "offering this address to viewers");
            let reader = tokio::spawn(receive(socket.clone(), local, self.datagrams.clone()));
            self.bound.insert(
                local,
                Bound {
                    socket,
                    interface,
                    reader,
                    sent: 0,
                },
            );
        }
        Ok(())
    }

    fn candidates(&self) -> Vec<Candidate> {
        self.bound
            .keys()
            .filter_map(|a| Candidate::host(*a, "udp").ok())
            .collect()
    }

    /// Sends from the socket `str0m` chose. A datagram the socket cannot take
    /// now is dropped, a loss WebRTC sees and repairs.
    fn send(&mut self, from: SocketAddr, to: SocketAddr, data: &[u8]) {
        if let Some(bound) = self.bound.get_mut(&from)
            && bound.socket.try_send_to(data, to).is_ok()
        {
            bound.sent += data.len() as u64;
        }
    }
}

async fn receive(socket: Arc<UdpSocket>, local: SocketAddr, datagrams: mpsc::Sender<Datagram>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else {
            // An error is an earlier datagram that went nowhere.
            continue;
        };
        let datagram = Datagram {
            local,
            from,
            at: Instant::now(),
            data: buf[..n].to_vec(),
        };
        if datagrams.send(datagram).await.is_err() {
            return;
        }
    }
}

struct CameraState {
    floor: u64,
    /// Whether a Raspberry Pi camera follows the connection.
    follow: bool,
    /// While it records on the vehicle, the recording's frame rate.
    record_fps: Option<f64>,
    camera: Camera,
    /// While someone watches a Raspberry Pi camera.
    rule: Option<Rule>,
    /// Bits read from `MediaMTX` this second, and the last whole second.
    made: u64,
    made_last: u64,
    /// The lowest estimate among the viewers.
    estimate: Option<u64>,
}

/// One WHEP session.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a session's independent facts, each its own yes or no"
)]
struct Viewer {
    path: String,
    rtc: Rtc,
    /// The offer as it now stands, for an ICE restart.
    offer: String,
    video: Option<Mid>,
    pt: Option<Pt>,
    /// The offer's audio m-line, and the Opus payload type to send on it.
    audio: Option<Mid>,
    audio_pt: Option<Pt>,
    /// Whether the viewer asked for sound (M9).
    listening: bool,
    listener: Listener,
    connected: bool,
    /// Whether a keyframe has gone, so it never starts on a picture that
    /// cannot be decoded.
    keyed: bool,
    estimate: Option<u64>,
    lost_since: Option<Instant>,
    dead: bool,
}

impl Viewer {
    /// Counts toward the lowest estimate: connected and not lost.
    fn counts(&self) -> bool {
        self.connected && self.lost_since.is_none()
    }

    /// The step it is sent while it listens and its camera has sound.
    fn hears(&self, camera: Option<&CameraSound>) -> Option<usize> {
        (self.listening
            && self.audio.is_some()
            && camera.is_some_and(|c| c.state == sound::State::Available))
        .then(|| self.listener.step())
    }
}

struct Core {
    cameras: BTreeMap<String, CameraState>,
    viewers: HashMap<String, Viewer>,
    sockets: Sockets,
    paths: watch::Sender<BTreeSet<String>>,
    wanted: watch::Sender<BTreeMap<String, Option<Settings>>>,
    /// The microphones and each camera's sound, as the manager last said.
    sound: sound::Status,
}

impl Core {
    async fn serve(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        mut datagrams: mpsc::Receiver<Datagram>,
        mut pictures: mpsc::Receiver<(String, Frame)>,
        mut configured: watch::Receiver<BTreeMap<String, Camera>>,
        mut recording: watch::Receiver<BTreeMap<String, f64>>,
        sound: Feed,
    ) -> Result<(), String> {
        let Feed {
            status: mut sound_status,
            mut packets,
        } = sound;
        let mut second = tokio::time::interval(Duration::from_secs(1));
        let mut interfaces = tokio::time::interval(INTERFACES_EVERY);
        loop {
            let deadline = self.drive();
            tokio::select! {
                Some(request) = requests.recv() => self.request(request),
                Some(datagram) = datagrams.recv() => self.datagram(&datagram),
                Some((path, frame)) = pictures.recv() => self.picture(&path, frame),
                Some(packet) = packets.recv() => self.sound(packet),
                Ok(()) = sound_status.changed() => {
                    self.sound = sound_status.borrow_and_update().clone();
                    let paths: Vec<String> = self.cameras.keys().cloned().collect();
                    for path in paths {
                        self.follow(&path);
                    }
                }
                Ok(()) = configured.changed() => {
                    let cameras = configured.borrow_and_update().clone();
                    self.reconfigure(cameras);
                }
                Ok(()) = recording.changed() => {
                    let recording = recording.borrow_and_update().clone();
                    self.recording(&recording);
                }
                _ = second.tick() => self.second(),
                _ = interfaces.tick() => {
                    let _ = self.sockets.refresh(false).await;
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    let now = Instant::now();
                    for viewer in self.viewers.values_mut() {
                        if let Err(error) = viewer.rtc.handle_input(Input::Timeout(now)) {
                            tracing::debug!(%error, "a viewer's WebRTC failed");
                            viewer.dead = true;
                        }
                    }
                }
            }
        }
    }

    /// Takes every viewer's output: datagrams to send and events. The
    /// soonest any of them wants to be woken.
    fn drive(&mut self) -> Instant {
        let mut soonest = Instant::now() + Duration::from_secs(1);
        let mut estimated = BTreeSet::new();
        for (id, viewer) in &mut self.viewers {
            if viewer.dead {
                continue;
            }
            loop {
                let output = match viewer.rtc.poll_output() {
                    Ok(output) => output,
                    Err(error) => {
                        tracing::info!(session = %id, %error, "a viewer's WebRTC failed");
                        viewer.dead = true;
                        break;
                    }
                };
                match output {
                    Output::Timeout(at) => {
                        soonest = soonest.min(at);
                        break;
                    }
                    Output::Transmit(t) => self.sockets.send(t.source, t.destination, &t.contents),
                    Output::Event(event) => match event {
                        Event::MediaAdded(added) => match added.kind {
                            MediaKind::Video => viewer.video = Some(added.mid),
                            MediaKind::Audio => viewer.audio = Some(added.mid),
                        },
                        Event::Connected => {
                            tracing::info!(session = %id, path = %viewer.path, "a viewer connected");
                            viewer.connected = true;
                        }
                        Event::IceConnectionStateChange(state) => {
                            tracing::debug!(session = %id, ?state, "a viewer's ICE");
                            match state {
                                IceConnectionState::Disconnected => {
                                    viewer.lost_since.get_or_insert_with(Instant::now);
                                }
                                IceConnectionState::Connected | IceConnectionState::Completed => {
                                    viewer.lost_since = None;
                                }
                                _ => {}
                            }
                            estimated.insert(viewer.path.clone());
                        }
                        Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                            viewer.estimate = Some(estimate.as_u64());
                            viewer.listener.estimate(viewer.estimate, Instant::now());
                            estimated.insert(viewer.path.clone());
                        }
                        _ => {}
                    },
                }
            }
            if viewer.lost_since.is_some_and(|at| at.elapsed() >= LOST_FOR)
                || !viewer.rtc.is_alive()
            {
                viewer.dead = true;
            }
        }
        let dead: Vec<String> = self
            .viewers
            .iter()
            .filter(|(_, v)| v.dead)
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            tracing::info!(session = %id, "a viewer went away");
            self.close(&id);
        }
        for path in estimated {
            self.follow(&path);
        }
        soonest
    }

    /// The lowest estimate among the camera's viewers, each less what its
    /// sound costs while it listens (M6), to its rule.
    fn follow(&mut self, path: &str) {
        let sound = self.sound.cameras.get(path);
        let lowest = self
            .viewers
            .values()
            .filter(|v| v.path == path && v.counts())
            .filter_map(|v| Some(listening::for_the_picture(v.estimate?, v.hears(sound))))
            .min();
        let Some(camera) = self.cameras.get_mut(path) else {
            return;
        };
        camera.estimate = lowest;
        if let (Some(rule), Some(lowest)) = (camera.rule.as_mut(), lowest)
            && let Some(settings) = rule.estimate(lowest, Instant::now())
        {
            let target = rule.target();
            publish(&self.wanted, path, Some(settings), lowest, target);
        }
    }

    fn second(&mut self) {
        // A step up is taken once the estimate allowed it for 5 s.
        let now = Instant::now();
        let mut stepped = BTreeSet::new();
        for viewer in self.viewers.values_mut().filter(|v| v.listening) {
            let step = viewer.listener.step();
            viewer.listener.estimate(viewer.estimate, now);
            if viewer.listener.step() != step {
                stepped.insert(viewer.path.clone());
            }
        }
        for path in stepped {
            self.follow(&path);
        }
        for (path, camera) in &mut self.cameras {
            camera.made_last = std::mem::take(&mut camera.made);
            if let Some(rule) = camera.rule.as_mut()
                && let Some(settings) = rule.second(camera.made_last)
            {
                let target = rule.target();
                publish(
                    &self.wanted,
                    path,
                    Some(settings),
                    camera.estimate.unwrap_or_default(),
                    target,
                );
            }
        }
    }

    /// A camera that started or stopped recording on the vehicle: its frame
    /// rate is held at the recording's while it records.
    fn recording(&mut self, recording: &BTreeMap<String, f64>) {
        for (path, camera) in &mut self.cameras {
            let fps = recording.get(path).copied();
            if fps == camera.record_fps {
                continue;
            }
            tracing::info!(%path, recording_fps = ?fps, "the camera's recording on the vehicle changed");
            camera.record_fps = fps;
            if let Some(rule) = camera.rule.as_mut()
                && let Some(settings) = rule.hold(fps)
            {
                let target = rule.target();
                publish(
                    &self.wanted,
                    path,
                    Some(settings),
                    camera.estimate.unwrap_or_default(),
                    target,
                );
            }
        }
    }

    fn reconfigure(&mut self, cameras: BTreeMap<String, Camera>) {
        for (path, now) in cameras {
            let Some(camera) = self.cameras.get_mut(&path) else {
                continue;
            };
            if camera.camera == now {
                continue;
            }
            tracing::info!(%path, ?now, "the camera's configured settings changed");
            if let (Some(rule), Camera::RpiCamera(own)) = (camera.rule.as_mut(), &now)
                && let Some(settings) = rule.reconfigure(*own, camera.floor)
            {
                let target = rule.target();
                publish(
                    &self.wanted,
                    &path,
                    Some(settings),
                    camera.estimate.unwrap_or_default(),
                    target,
                );
            }
            if let (Camera::RpiCamera(own), true) = (&now, camera.follow) {
                for viewer in self.viewers.values_mut().filter(|v| v.path == path) {
                    let desired = with_sound(desired(own.bitrate), viewer.listening);
                    viewer.rtc.bwe().set_bitrate(Bitrate::ZERO, desired);
                }
            }
            camera.camera = now;
        }
    }

    fn datagram(&mut self, datagram: &Datagram) {
        let Ok(contents) = datagram.data.as_slice().try_into() else {
            return;
        };
        let input = Input::Receive(
            datagram.at,
            Receive {
                proto: Protocol::Udp,
                source: datagram.from,
                destination: datagram.local,
                contents,
            },
        );
        if let Some(viewer) = self
            .viewers
            .values_mut()
            .find(|v| !v.dead && v.rtc.accepts(&input))
            && let Err(error) = viewer.rtc.handle_input(input)
        {
            tracing::debug!(%error, "a viewer's WebRTC failed");
            viewer.dead = true;
        }
    }

    /// A picture from the camera, to each of its viewers that is connected,
    /// starting each at a keyframe.
    fn picture(&mut self, path: &str, frame: Frame) {
        let Some(camera) = self.cameras.get_mut(path) else {
            return;
        };
        camera.made += 8 * frame.annex_b.len() as u64;
        // When it was taken, on this computer's monotonic clock, so the
        // sender's reports carry the camera's times (W11).
        let ago = SystemTime::now()
            .duration_since(frame.captured)
            .unwrap_or_default();
        let now = Instant::now();
        let wallclock = now.checked_sub(ago).unwrap_or(now);
        let time = MediaTime::from_90khz(frame.rtp);
        let data: Arc<[u8]> = frame.annex_b.into();
        for viewer in self.viewers.values_mut() {
            if viewer.path != path
                || !viewer.connected
                || viewer.dead
                || !(viewer.keyed || frame.key)
            {
                continue;
            }
            let Some(mid) = viewer.video else {
                continue;
            };
            let Some(writer) = viewer.rtc.writer(mid) else {
                continue;
            };
            let pt = if let Some(pt) = viewer.pt {
                pt
            } else {
                let Some(pt) = h264(&writer, &data) else {
                    tracing::warn!(%path, "the viewer offered no H.264 this camera can send");
                    viewer.dead = true;
                    continue;
                };
                viewer.pt = Some(pt);
                pt
            };
            if let Err(error) = writer.write(pt, wallclock, time, data.clone()) {
                tracing::info!(%path, %error, "cannot send a picture to a viewer");
                viewer.dead = true;
                continue;
            }
            viewer.keyed = true;
        }
    }

    fn request(&mut self, request: Request) {
        match request {
            Request::Offer { path, sdp, reply } => {
                let _ = reply.send(self.offer(&path, &sdp));
            }
            Request::Patch {
                path,
                id,
                content_type,
                body,
                reply,
            } if content_type == "application/json" => {
                let _ = reply.send(self.listen(&path, &id, &body));
            }
            Request::Patch {
                path,
                id,
                content_type,
                body,
                reply,
            } => {
                let _ = reply.send(self.patch(&path, &id, &content_type, &body));
            }
            Request::Session { path, id, reply } => {
                let _ = reply.send(match self.viewers.get(&id).filter(|v| v.path == path) {
                    Some(viewer) => self.session(viewer),
                    None => Reply::text(StatusCode::NOT_FOUND, "There is no such session."),
                });
            }
            Request::Delete { path, id, reply } => {
                let found = self.viewers.get(&id).is_some_and(|v| v.path == path);
                let _ = reply.send(if found {
                    tracing::info!(session = %id, %path, "a viewer ended its session");
                    self.close(&id);
                    Reply::text(StatusCode::OK, "")
                } else {
                    Reply::text(StatusCode::NOT_FOUND, "There is no such session.")
                });
            }
            Request::Status { reply } => {
                let _ = reply.send(Reply {
                    status: StatusCode::OK,
                    headers: Vec::new(),
                    content_type: "application/json",
                    body: self.status().to_string(),
                });
            }
        }
    }

    fn offer(&mut self, path: &str, sdp: &str) -> Reply {
        let Some(camera) = self.cameras.get_mut(path) else {
            return Reply::text(
                StatusCode::NOT_FOUND,
                format!("There is no camera {path} here."),
            );
        };
        let offer = match SdpOffer::from_sdp_string(sdp) {
            Ok(offer) => offer,
            Err(e) => {
                return Reply::text(
                    StatusCode::BAD_REQUEST,
                    format!("The offer is not SDP the sender reads: {e}"),
                );
            }
        };
        let candidates = self.sockets.candidates();
        if candidates.is_empty() {
            return Reply::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "The video sender has no address on its interfaces yet.",
            );
        }
        let ceiling = match &camera.camera {
            Camera::RpiCamera(own) if camera.follow => Some(own.bitrate),
            _ => None,
        };
        // The estimate, which the pacer sends at, starts at what the camera
        // makes as configured, the ceiling, so the pictures do not queue
        // before the first change lands. A camera UXDuck cannot set is sent
        // at its own rate (W6), so its pacer starts at UXDuck's highest
        // camera bitrate (ADR-0060 C3), and its estimate is shown, not followed.
        let mut rtc = RtcConfig::new()
            .clear_codecs()
            .enable_h264(true)
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)))
            .set_rtcp_report_interval_audio(SOUND_REPORTS_EVERY)
            .enable_bwe(Some(ceiling.map_or(Bitrate::mbps(25), Bitrate::bps)))
            .build(Instant::now());
        for candidate in candidates {
            rtc.add_local_candidate(candidate);
        }
        let answer = match rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => answer.to_sdp_string(),
            Err(e) => {
                return Reply::text(
                    StatusCode::BAD_REQUEST,
                    format!("The offer was refused: {e}"),
                );
            }
        };
        if let Some(ceiling) = ceiling {
            rtc.bwe().set_bitrate(Bitrate::ZERO, desired(ceiling));
        }
        let id = session_id();
        let first = !self.viewers.values().any(|v| v.path == path);
        if first {
            if let (Camera::RpiCamera(own), true) = (&camera.camera, camera.follow) {
                let mut rule = Rule::new(*own, camera.floor);
                rule.hold(camera.record_fps);
                camera.rule = Some(rule);
            }
            self.paths.send_modify(|paths| {
                paths.insert(path.to_owned());
            });
        }
        tracing::info!(session = %id, %path, "a viewer asked for the camera");
        self.viewers.insert(
            id.clone(),
            Viewer {
                path: path.to_owned(),
                rtc,
                offer: sdp.to_owned(),
                video: None,
                pt: None,
                audio: None,
                audio_pt: None,
                listening: false,
                listener: Listener::default(),
                connected: false,
                keyed: false,
                estimate: None,
                lost_since: None,
                dead: false,
            },
        );
        Reply {
            status: StatusCode::CREATED,
            headers: vec![
                (header::LOCATION, format!("/{path}/whep/{id}")),
                (header::ETAG, "\"*\"".into()),
                (
                    header::HeaderName::from_static("accept-patch"),
                    "application/trickle-ice-sdpfrag, application/json".into(),
                ),
            ],
            content_type: "application/sdp",
            body: answer,
        }
    }

    /// A viewer's sound on or off (M9): `{"sound": true}` or `{"sound":
    /// false}`. Nothing in the media changes, so the picture goes on.
    fn listen(&mut self, path: &str, id: &str, body: &str) -> Reply {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Asked {
            sound: bool,
        }
        let Ok(Asked { sound: on }) = serde_json::from_str(body) else {
            return Reply::text(
                StatusCode::BAD_REQUEST,
                "A sound PATCH is {\"sound\": true} or {\"sound\": false}.",
            );
        };
        let ceiling = match self.cameras.get(path).map(|c| (&c.camera, c.follow)) {
            Some((Camera::RpiCamera(own), true)) => Some(own.bitrate),
            _ => None,
        };
        let Some(viewer) = self.viewers.get_mut(id).filter(|v| v.path == path) else {
            return Reply::text(StatusCode::NOT_FOUND, "There is no such session.");
        };
        if viewer.listening != on {
            tracing::info!(session = %id, %path, sound = on, "a viewer turned its sound {}", if on { "on" } else { "off" });
            viewer.listening = on;
            if on {
                viewer.listener.start(viewer.estimate, Instant::now());
            }
            if let Some(ceiling) = ceiling {
                let desired = with_sound(desired(ceiling), on);
                viewer.rtc.bwe().set_bitrate(Bitrate::ZERO, desired);
            }
            self.follow(path);
        }
        match self.viewers.get(id) {
            Some(viewer) => self.session(viewer),
            None => Reply::text(StatusCode::NOT_FOUND, "There is no such session."),
        }
    }

    /// A session's sound, as its PATCH and GET answer it.
    fn session(&self, viewer: &Viewer) -> Reply {
        let camera = self.sound.cameras.get(&viewer.path);
        let mut sound = sound_json(camera, viewer.hears(camera).map(|step| STEPS[step].bitrate));
        sound["on"] = viewer.listening.into();
        if viewer.audio.is_none() {
            sound["reason"] = "This session has no sound track. Open it again with an audio m-line to hear the camera.".into();
        }
        Reply {
            status: StatusCode::OK,
            headers: Vec::new(),
            content_type: "application/json",
            body: serde_json::json!({ "sound": sound }).to_string(),
        }
    }

    /// A packet of a camera's sound, to each of its viewers that listens
    /// and gets that step (M5).
    fn sound(&mut self, packet: sound::Packet) {
        let available = self
            .sound
            .cameras
            .get(&packet.path)
            .is_some_and(|c| c.state == sound::State::Available);
        if !available {
            return;
        }
        let length = Duration::from_millis(u64::from(STEPS[packet.step].ms));
        // When its sound began, on the pictures' clock (W11).
        let wallclock = packet.at.checked_sub(length).unwrap_or(packet.at);
        let data: Arc<[u8]> = packet.payload.into();
        for viewer in self.viewers.values_mut() {
            if viewer.path != packet.path || !viewer.listening || !viewer.connected || viewer.dead {
                continue;
            }
            let Some(mid) = viewer.audio else {
                continue;
            };
            let Some(time) =
                viewer
                    .listener
                    .packet(packet.step, packet.ssrc, packet.timestamp, packet.at)
            else {
                continue;
            };
            let Some(writer) = viewer.rtc.writer(mid) else {
                continue;
            };
            let pt = if let Some(pt) = viewer.audio_pt {
                pt
            } else {
                let Some(pt) = writer
                    .payload_params()
                    .find(|p| p.spec().codec == Codec::Opus)
                    .map(str0m::format::PayloadParams::pt)
                else {
                    continue;
                };
                viewer.audio_pt = Some(pt);
                pt
            };
            let time = MediaTime::new(time, Frequency::FORTY_EIGHT_KHZ);
            if let Err(error) = writer.write(pt, wallclock, time, data.clone()) {
                tracing::info!(path = %packet.path, %error, "cannot send sound to a viewer");
                viewer.dead = true;
            }
        }
    }

    /// An ICE restart or more candidates (RFC 9725, W14).
    fn patch(&mut self, path: &str, id: &str, content_type: &str, body: &str) -> Reply {
        let candidates = self.sockets.candidates();
        let Some(viewer) = self.viewers.get_mut(id).filter(|v| v.path == path) else {
            return Reply::text(StatusCode::NOT_FOUND, "There is no such session.");
        };
        if content_type != "application/trickle-ice-sdpfrag" {
            return Reply::text(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "A WHEP PATCH is application/trickle-ice-sdpfrag.",
            );
        }
        let fragment = Fragment::read(body, m_lines(&viewer.offer));
        let (Some(ufrag), Some(pwd)) = (&fragment.ufrag, &fragment.pwd) else {
            for line in &fragment.candidates {
                if let Ok(candidate) = Candidate::from_sdp_string(line) {
                    viewer.rtc.add_remote_candidate(candidate);
                }
            }
            return Reply::text(StatusCode::NO_CONTENT, "");
        };
        let restarted = restart_offer(&viewer.offer, ufrag, pwd, &fragment.candidates);
        let offer = match SdpOffer::from_sdp_string(&restarted) {
            Ok(offer) => offer,
            Err(e) => {
                return Reply::text(
                    StatusCode::BAD_REQUEST,
                    format!("The ICE restart is not SDP the sender reads: {e}"),
                );
            }
        };
        let answer = match viewer.rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => answer.to_sdp_string(),
            Err(e) => {
                return Reply::text(
                    StatusCode::BAD_REQUEST,
                    format!("The ICE restart was refused: {e}"),
                );
            }
        };
        viewer.offer = restarted;
        for candidate in &candidates {
            viewer.rtc.add_local_candidate(candidate.clone());
        }
        let value = |key: &str| {
            answer
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let mut fragment = format!(
            "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\n",
            value("a=ice-ufrag:"),
            value("a=ice-pwd:")
        );
        for candidate in &candidates {
            let _ = write!(fragment, "a={}\r\n", candidate.to_sdp_string());
        }
        fragment.push_str("a=end-of-candidates\r\n");
        tracing::info!(session = %id, %path, "a viewer restarted ICE");
        Reply {
            status: StatusCode::OK,
            headers: vec![(header::ETAG, "\"*\"".into())],
            content_type: "application/trickle-ice-sdpfrag",
            body: fragment,
        }
    }

    /// Lets a viewer go. The camera's last viewer puts the camera back to
    /// its configured settings and stops its reading (W8).
    fn close(&mut self, id: &str) {
        let Some(mut viewer) = self.viewers.remove(id) else {
            return;
        };
        viewer.rtc.disconnect();
        let path = viewer.path;
        if self.viewers.values().any(|v| v.path == path) {
            self.follow(&path);
            return;
        }
        if let Some(camera) = self.cameras.get_mut(&path) {
            camera.estimate = None;
            if camera.rule.take().is_some() {
                tracing::info!(%path, "the camera's last viewer left: putting its configured settings back");
                self.wanted.send_modify(|wanted| {
                    wanted.insert(path.clone(), None);
                });
            }
        }
        self.paths.send_modify(|paths| {
            paths.remove(&path);
        });
    }

    fn status(&self) -> serde_json::Value {
        let cameras: Vec<serde_json::Value> = self
            .cameras
            .iter()
            .map(|(path, camera)| {
                let viewers = self.viewers.values().filter(|v| &v.path == path).count();
                let (source, configured) = match &camera.camera {
                    Camera::RpiCamera(own) => ("rpiCamera".to_owned(), Some(*own)),
                    Camera::Other { source } => (source.clone(), None),
                };
                let held = camera.rule.as_ref().is_some_and(Rule::held);
                let sound = self.sound.cameras.get(path);
                let bitrate = self
                    .viewers
                    .values()
                    .filter(|v| &v.path == path)
                    .filter_map(|v| v.hears(sound))
                    .map(|step| STEPS[step].bitrate)
                    .max();
                let note = camera
                    .estimate
                    .filter(|e| camera.rule.is_some() && (*e < camera.floor || held))
                    .map(|e| match camera.record_fps {
                        Some(fps) if held => format!(
                            "The connection carries about {} kbit/s, under what {} frames a second need while this camera records, so its picture runs late.",
                            e / 1000,
                            fps.round()
                        ),
                        _ => format!(
                            "The connection carries about {} kbit/s, under this camera's lowest {} kbit/s, so its picture runs late.",
                            e / 1000,
                            camera.floor / 1000
                        ),
                    });
                serde_json::json!({
                    "path": path,
                    "source": source,
                    "viewers": viewers,
                    "configured": configured,
                    "floorBitrate": camera.floor,
                    "estimate": camera.estimate,
                    "target": camera.rule.as_ref().map(Rule::target),
                    "following": camera.rule.as_ref().map(Rule::settings),
                    "readBitrate": camera.made_last,
                    "follow": camera.follow,
                    "recordingFps": camera.record_fps,
                    "note": note,
                    "sound": sound_json(sound, bitrate),
                    "listeners": self.viewers.values().filter(|v| &v.path == path && v.listening).count(),
                })
            })
            .collect();
        let addresses: Vec<serde_json::Value> = self
            .sockets
            .bound
            .iter()
            .map(|(address, bound)| serde_json::json!({ "interface": bound.interface, "address": address.to_string(), "sentBytes": bound.sent }))
            .collect();
        serde_json::json!({ "cameras": cameras, "addresses": addresses, "microphones": self.sound.microphones })
    }
}

/// What a camera's sound is, as a session's and the status's `sound` say it
/// (M9): `sending` when `bitrate` goes to a viewer now.
fn sound_json(camera: Option<&CameraSound>, bitrate: Option<u32>) -> serde_json::Value {
    let Some(camera) = camera else {
        return serde_json::json!({
            "state": sound::State::NoMicrophone,
            "microphone": null,
            "codec": null,
            "bitrate": null,
            "reason": null,
        });
    };
    let state = if bitrate.is_some() {
        serde_json::json!("sending")
    } else {
        serde_json::json!(camera.state)
    };
    let codec = (camera.state == sound::State::Available).then_some("Opus, 48 kHz, mono");
    serde_json::json!({
        "state": state,
        "microphone": camera.microphone,
        "codec": codec,
        "bitrate": bitrate,
        "reason": camera.reason,
    })
}

/// What GCC probes toward with the highest step's sound on top while the
/// viewer listens.
fn with_sound(desired: Bitrate, listening: bool) -> Bitrate {
    if listening {
        Bitrate::bps(desired.as_u64() + u64::from(STEPS[0].wire()))
    } else {
        desired
    }
}

/// The bitrate GCC probes toward: enough for the target to reach the
/// ceiling at 0.85 of the estimate.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "bit/s"
)]
fn desired(ceiling: u64) -> Bitrate {
    Bitrate::bps((ceiling as f64 / rule::SHARE_OF_ESTIMATE) as u64)
}

fn publish(
    wanted: &watch::Sender<BTreeMap<String, Option<Settings>>>,
    path: &str,
    settings: Option<Settings>,
    estimate: u64,
    target: u64,
) {
    if let Some(s) = settings {
        tracing::info!(
            %path,
            estimate,
            target,
            bitrate = s.bitrate,
            fps = s.fps,
            idr = s.idr,
            "following the connection"
        );
    }
    wanted.send_modify(|wanted| {
        wanted.insert(path.to_owned(), settings);
    });
}

/// The H.264 payload type to send: packetization mode 1, the camera's
/// profile when the viewer offered it.
fn h264(writer: &str0m::media::Writer<'_>, picture: &[u8]) -> Option<Pt> {
    let profile = uxduck_media::nal::split_annex_b(picture)
        .into_iter()
        .find(|nal| nal.first().map(|b| b & 0x1f) == Some(uxduck_media::nal::h264::SPS))
        .and_then(|sps| sps.get(1).copied());
    let usable: Vec<_> = writer
        .payload_params()
        .filter(|p| p.spec().codec == Codec::H264 && p.spec().format.packetization_mode == Some(1))
        .collect();
    usable
        .iter()
        .find(|p| {
            profile.is_some_and(|profile| {
                p.spec()
                    .format
                    .profile_level_id
                    .is_some_and(|id| (id >> 16) & 0xff == u32::from(profile))
            })
        })
        .or(usable.first())
        .map(|p| p.pt())
}

fn session_id() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().fold(String::new(), |mut id, b| {
        let _ = write!(id, "{b:02x}");
        id
    })
}

fn m_lines(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.starts_with("m=")).count()
}

/// A PATCH's ICE credentials and candidates. A candidate counts only under
/// a numeric `a=mid` naming one of the offer's m-lines, as `MediaMTX` reads
/// them.
struct Fragment {
    ufrag: Option<String>,
    pwd: Option<String>,
    candidates: Vec<String>,
}

impl Fragment {
    fn read(body: &str, m_lines: usize) -> Self {
        let mut fragment = Self {
            ufrag: None,
            pwd: None,
            candidates: Vec::new(),
        };
        // Lines before any m= line, or under one whose mid is a number.
        let mut taken: Option<bool> = None;
        let mut section: Vec<String> = Vec::new();
        let flush = |taken: Option<bool>, section: &mut Vec<String>, out: &mut Vec<String>| {
            if taken == Some(true) {
                out.append(section);
            }
            section.clear();
        };
        for line in body.lines().map(str::trim) {
            if line.starts_with("m=") {
                flush(taken, &mut section, &mut fragment.candidates);
                taken = Some(false);
            } else if let Some(mid) = line.strip_prefix("a=mid:") {
                taken = Some(mid.parse::<usize>().is_ok_and(|index| index < m_lines));
            } else if let Some(candidate) = line.strip_prefix("a=candidate:") {
                section.push(format!("candidate:{candidate}"));
            } else if let Some(ufrag) = line.strip_prefix("a=ice-ufrag:") {
                fragment.ufrag = Some(ufrag.to_owned());
            } else if let Some(pwd) = line.strip_prefix("a=ice-pwd:") {
                fragment.pwd = Some(pwd.to_owned());
            }
        }
        flush(taken, &mut section, &mut fragment.candidates);
        fragment
    }
}

/// The session's offer again with new ICE credentials and only the PATCH's
/// candidates, in its first m-line, as the remote side's ICE restart.
fn restart_offer(offer: &str, ufrag: &str, pwd: &str, candidates: &[String]) -> String {
    let mut out = String::with_capacity(offer.len());
    let mut placed = false;
    let mut in_media = false;
    for line in offer.lines() {
        if line.starts_with("a=candidate:") || line.starts_with("a=end-of-candidates") {
            continue;
        }
        if line.starts_with("a=ice-ufrag:") {
            let _ = write!(out, "a=ice-ufrag:{ufrag}\r\n");
        } else if line.starts_with("a=ice-pwd:") {
            let _ = write!(out, "a=ice-pwd:{pwd}\r\n");
        } else {
            if line.starts_with("m=") {
                if in_media && !placed {
                    for c in candidates {
                        let _ = write!(out, "a={c}\r\n");
                    }
                    placed = true;
                }
                in_media = true;
            }
            let _ = write!(out, "{line}\r\n");
        }
    }
    if !placed {
        for c in candidates {
            let _ = write!(out, "a={c}\r\n");
        }
    }
    out
}

#[cfg(test)]
mod tests;

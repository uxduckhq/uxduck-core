//! Camera streams (ADR-0017 V4, V5, V6, V14, S38): the `videoSource`
//! entities, which vehicle each belongs to, the "Video from …" notification,
//! and who may open the `/video` socket for which source. The listener
//! reports what arrives; the pictures never pass through the engine.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{
    VideoAddParams, VideoAddResult, VideoAddressesParams, VideoAssignParams,
    VideoCaptureEnableParams, VideoCaptureParams, VideoDevicesResult, VideoListResult,
    VideoMetadataWatchResult, VideoPortStatus, VideoRemoveParams, VideoRenameParams,
    VideoRevealParams, VideoRevealResult, VideoServedByParams, VideoSocket,
    VideoSource as VideoSourceDto, VideoTrackParams, VideoUnwatchParams, VideoWatchParams,
    VideoWatchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::telemetry::ClockMeasured;
use uxduck_core::domain::vehicle::{Vehicle, VehicleRoute};
use uxduck_core::domain::video::{
    AddressAhead, AddressChoice, AddressMove, AddressReason, AddressStandby, CaptureInput,
    VideoReport, VideoSource, VideoState, VideoTrack, redact, sender_host, udp_name, url_host,
};
use uxduck_core::events::{Actor, Change};
use uxduck_media::latency::{Companion, CompanionClock, Offset};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

/// A source that comes back within this long is the same stream, and does
/// not notify again (V4, V14).
pub const SAME_SOURCE_US: u64 = 30_000_000;
/// While a source receives, its counters are journaled at most this often;
/// `video/list` has the latest.
const JOURNAL_COUNTERS_US: u64 = 30_000_000;
/// The condition of the warning that UXDuck cannot listen for video.
const LISTEN_CONDITION: &str = "video-listen";
/// While a UDP port added by hand is not listened on.
const PORTS_CONDITION: &str = "video-ports";

/// Where watches go: the daemon's `/video` hub.
/// The vehicle connection on an RTSP camera address's host, as its worker
/// needs it (ADR-0057 D12): which one, whether the vehicle is heard on it,
/// and whether the vehicle uses it now. `returns` while the vehicle would
/// go back to it, higher in its order and answering for 10 s, but a hold
/// keeps the vehicle where it is (`connections.returnsTo`, ADR-0057 D13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressRoute {
    pub link_id: String,
    pub up: bool,
    pub in_use: bool,
    pub returns: bool,
}

pub trait VideoPort: Send + std::fmt::Debug {
    /// A client may open `/video?token=<token>` for this source.
    fn allow(&mut self, token: &str, source_id: &str);
    /// The watch is over: close its socket, if open.
    fn revoke(&mut self, token: &str);
    /// The open sockets, for `video/list`.
    fn sockets(&self) -> Vec<VideoSocket> {
        Vec::new()
    }
    /// Listens on another UDP port for video (S39).
    ///
    /// # Errors
    /// Why it cannot, such as another program holding the port.
    fn listen(&mut self, _port: u16) -> Result<(), String> {
        Err("video needs UXDuck Desktop".into())
    }
    fn unlisten(&mut self, _port: u16) {}
    /// Connects to an RTSP source and keeps reconnecting until closed.
    fn open_rtsp(&mut self, _source_id: &str, _urls: &[String], _epoch: u64) {}
    fn close_rtsp(&mut self, _source_id: &str) {}
    /// Moves an open RTSP camera to its address at `index`, because its
    /// vehicle moved to the connection on that address's host (ADR-0057
    /// D10): straight away, or to the first address once it answers a probe.
    fn steer_rtsp(&mut self, _source_id: &str, _index: usize) {}
    /// Each address of an open RTSP camera, in its worker's order: the
    /// vehicle connection on its host, if any (ADR-0057 D12).
    fn route_rtsp(&mut self, _source_id: &str, _routes: &[Option<AddressRoute>]) {}
    /// The earlier addresses, by place, an open RTSP camera may go back to
    /// on its own once they answer its probes (ADR-0057 D10, amended
    /// 2026-11-16). A worker told nothing may go back to any.
    fn rtsp_may_return(&mut self, _source_id: &str, _to: &[usize]) {}
    /// Whether a UDP source should be demuxed instead of just discovered.
    fn demand(&mut self, _source_id: &str, _needed: bool) {}
    /// Cached enumeration; the host refreshes outside the engine thread.
    fn devices(&mut self) -> VideoDevicesResult {
        VideoDevicesResult {
            error: Some("Local cameras need UXDuck running on this computer.".into()),
            ..VideoDevicesResult::default()
        }
    }
    /// Validate dependencies and a known device without blocking the engine.
    ///
    /// # Errors
    /// Missing capture runtime, failed discovery, or an unknown device.
    fn check_capture(&self, _input: &CaptureInput) -> Result<(), String> {
        Err("Local camera capture is not available on this host.".into())
    }
    fn open_capture(&mut self, _source_id: &str, _input: &CaptureInput, _epoch: u64) {}
    fn close_capture(&mut self, _source_id: &str) {}
    /// Deliberate selection/enable only. Restoring a source never calls this.
    fn select_capture(&mut self, _source_id: &str, _input: &CaptureInput) {}
    /// # Errors
    /// The host has no supported privacy settings route, or it could not open.
    fn capture_settings(&mut self, _microphone: bool) -> Result<(), String> {
        Err("Open camera privacy settings on the computer running UXDuck.".into())
    }
    /// # Errors
    /// A missing helper or an unreadable/unsupported file.
    fn check_file(&self, _path: &str) -> Result<(), String> {
        Err("Video files need UXDuck Desktop running on this computer.".into())
    }
    fn open_file(
        &mut self,
        _id: &str,
        _input: &uxduck_core::domain::video::VideoFileInput,
        _epoch: u64,
    ) {
    }
    fn close_file(&mut self, _id: &str) {}
    /// # Errors
    /// Unavailable storage, an unknown clip, or an invalid download action.
    fn demos(
        &mut self,
        _params: &uxduck_contracts::dto::video::VideoDemosParams,
    ) -> Result<uxduck_contracts::dto::video::VideoDemosResult, String> {
        Err("Demo footage needs UXDuck Desktop running on this computer.".into())
    }
    /// # Errors
    /// The chosen clip is not downloaded or its cached file is unavailable.
    fn demo_file(&self, _id: &str) -> Result<uxduck_core::domain::video::VideoFileInput, String> {
        Err("Download this demo before playing it.".into())
    }

    /// Starts writing a camera's pictures to a recording (S40), and its
    /// sound when the stream has AAC (`sound`, S147), in the media folder
    /// as `<stem>.mp4`, or `<stem> 2.mp4` and on when that is taken: the
    /// name it got (S150).
    fn start_recording(
        &mut self,
        _recording_id: &str,
        _source_id: &str,
        _sound: bool,
        stem: &str,
    ) -> String {
        format!("{stem}.mp4")
    }
    /// The computer's offset from UTC at `at_us`, in minutes, for names by
    /// local time (S150).
    fn utc_offset_minutes(&self, _at_us: u64) -> i32 {
        0
    }
    /// Finishes a recording; the daemon reports its size when it has.
    fn stop_recording(&mut self, _recording_id: &str) {}
    /// Deletes an ended recording's file in the media folder, with what
    /// UXDuck kept for it (S348). One already gone is deleted.
    ///
    /// # Errors
    /// Why it is still there, in words.
    fn delete_recording(&mut self, _recording_id: &str, _file: &str) -> Result<(), String> {
        Err("Recordings need UXDuck Desktop.".into())
    }
    /// Cuts a recording interrupted by a crash, its file named, back to
    /// its last whole fragment: its size and length, or `None` if nothing
    /// is left.
    fn recover_recording(&mut self, _file: &str) -> Option<(u64, u64)> {
        None
    }
    /// `/recordings/<id>.mp4?token=` answers with the recording's bytes
    /// for an hour, from where they are `at`.
    fn allow_playback(&mut self, _token: &str, _recording_id: &str, _at: &crate::runtime::VideoAt) {
    }
    /// Shows the media folder in the file manager, with a recording's file
    /// selected if one is named: the folder.
    ///
    /// # Errors
    /// Why it cannot, such as no file manager to open.
    fn reveal(&mut self, _file: Option<&str>) -> Result<String, String> {
        Err("Recordings need UXDuck Desktop.".into())
    }
    /// Looks through a camera for the assistant (FB-244): `order.frames`
    /// JPEGs over `order.seconds`, reported with `Engine::video_looked`.
    ///
    /// # Errors
    /// Why it cannot start, such as no capture helper.
    fn look(&mut self, _order: crate::looks::LookOrder) -> Result<(), String> {
        Err("Camera images need UXDuck Desktop.".into())
    }
    /// Stops a look the engine no longer waits for: its request was
    /// cancelled or its connection closed. Its answer is not wanted.
    fn stop_look(&mut self, _id: &str) {}
    /// Writes a photo (S144) to the pictures folder as `<stem>.jpg`, or
    /// `<stem> 2.jpg` and on when that is taken: the name it got.
    ///
    /// # Errors
    /// Why it cannot, such as a full disk.
    fn save_photo(&mut self, _stem: &str, _jpeg: &[u8]) -> Result<String, String> {
        Err("Photos need UXDuck Desktop.".into())
    }
    /// `/photos/<photo id>.jpg?token=` answers with that file for an hour.
    fn allow_photo(&mut self, _token: &str, _photo_id: &str, _file_name: &str) {}
    /// Shows the pictures folder with a photo selected: the folder.
    ///
    /// # Errors
    /// Why it cannot, such as no file manager to open.
    fn reveal_photo(&mut self, _file_name: &str) -> Result<String, String> {
        Err("Photos need UXDuck Desktop.".into())
    }
}

/// What `video/add` was given.
/// An RTSP address with `#track=N`, or without one for the first track,
/// the default.
fn with_track(url: &str, track: u32) -> String {
    let bare = url.split_once('#').map_or(url, |(bare, _)| bare);
    if track <= 1 {
        bare.to_owned()
    } else {
        format!("{bare}#track={track}")
    }
}

enum Input {
    Udp(u16),
    Rtsp {
        host: String,
        port: u16,
    },
    /// A WHEP endpoint, played over WebRTC (ADR-0064 W5).
    Whep {
        host: String,
        port: u16,
    },
}

/// `#sha256=` and the 32 bytes of a certificate's SHA-256 fingerprint in
/// hex, with or without colons: the certificate an https:// WHEP address
/// must present (ADR-0064 W10). Never sent to the camera.
fn whep_pin(fragment: &str) -> bool {
    fragment.strip_prefix("sha256=").is_some_and(|hex| {
        let digits: String = hex.chars().filter(|c| *c != ':').collect();
        digits.len() == 64 && digits.chars().all(|c| c.is_ascii_hexdigit())
    })
}

fn input(url: &str) -> Result<Input, ApiError> {
    let invalid = |why: &str| ApiError::new(ErrorKind::InvalidValue, why.to_owned());
    let url = url.trim();
    if let Some(rest) = url
        .strip_prefix("udp://")
        .or_else(|| url.strip_prefix("udp:"))
    {
        let port = rest.rsplit(':').next().unwrap_or(rest);
        return port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .map(Input::Udp)
            .ok_or_else(|| invalid("a UDP port is a number from 1 to 65535, such as udp:5601"));
    }
    if uxduck_core::domain::video::is_whep(url) {
        let (scheme, rest) = url.split_once("://").unwrap_or_default();
        let secure = scheme.eq_ignore_ascii_case("https");
        let (rest, fragment) = rest
            .split_once('#')
            .map_or((rest, None), |(b, f)| (b, Some(f)));
        if let Some(fragment) = fragment {
            if !whep_pin(fragment) {
                return Err(invalid(
                    "after # a WHEP address takes only its certificate's fingerprint, such as #sha256= and 64 hex digits",
                ));
            }
            if !secure {
                return Err(invalid(
                    "a certificate fingerprint needs an https:// WHEP address",
                ));
            }
        }
        let authority = rest.split(['/', '?']).next().unwrap_or_default();
        let host = uxduck_core::domain::video::url_host(url);
        let port = match authority
            .rsplit_once('@')
            .map_or(authority, |(_, h)| h)
            .rsplit_once(':')
        {
            Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => port
                .parse::<u16>()
                .map_err(|_| invalid("the WHEP address has a port that is not a number"))?,
            _ if secure => 443,
            _ => 80,
        };
        if host.is_empty() {
            return Err(invalid("the WHEP address has no host"));
        }
        return Ok(Input::Whep {
            host: host.to_owned(),
            port,
        });
    }
    let Some(rest) = url.strip_prefix("rtsp://") else {
        return Err(invalid(
            "add an RTSP address (rtsp://…), a WHEP address (http://…/whep) or a UDP port (udp:5601)",
        ));
    };
    // `#track=N` (S90) is UXDuck's, never part of the camera's address.
    let (rest, fragment) = rest
        .split_once('#')
        .map_or((rest, None), |(b, f)| (b, Some(f)));
    if let Some(fragment) = fragment
        && fragment
            .strip_prefix("track=")
            .and_then(|n| n.parse::<u32>().ok())
            .is_none_or(|n| n < 1)
    {
        return Err(invalid(
            "after # an RTSP address takes only a track, such as #track=2",
        ));
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => (
            host,
            port.parse::<u16>()
                .map_err(|_| invalid("the RTSP address has a port that is not a number"))?,
        ),
        _ => (host_port, 554),
    };
    if host.is_empty() {
        return Err(invalid("the RTSP address has no host"));
    }
    Ok(Input::Rtsp {
        host: host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
        port,
    })
}

#[derive(Debug, Default)]
pub(crate) struct Video {
    pub(crate) port: Option<Box<dyn VideoPort>>,
    /// Counters newer than the journaled ones, by source id.
    live: BTreeMap<String, uxduck_core::domain::video::VideoCounters>,
    journaled_at: BTreeMap<String, u64>,
    /// Watches by token: the connection that asked, and the source.
    watches: BTreeMap<String, (ConnectionId, String)>,
    metadata_watches: BTreeMap<String, (ConnectionId, String)>,
    pending_watches: BTreeMap<String, u64>,
    remote_watches: std::collections::BTreeSet<String>,
    needed: std::collections::BTreeSet<String>,
    /// UDP ports added by hand that could not be listened on, and why.
    extra_errors: BTreeMap<u16, String>,
    /// UDP ports a vehicle's camera named, and that vehicle (S39).
    announced_ports: BTreeMap<u16, String>,
    /// Of those, the ports listened on only because a camera named them.
    announced_listens: std::collections::BTreeSet<u16>,
    /// Ports listened on only while someone watches a source there, such
    /// as one remembered from a daemon that listened on another port.
    watched_listens: std::collections::BTreeSet<u16>,
    /// Announced RTSP sources fetched this session.
    opened: std::collections::BTreeSet<String>,
    rtsp_epochs: BTreeMap<String, u64>,
    /// The addresses each open RTSP worker was given, in its order: a steer
    /// names one by its place in this list (ADR-0057 D10).
    rtsp_urls: BTreeMap<String, Vec<String>>,
    /// The routes each open RTSP worker was last told, so it hears changes only.
    rtsp_routes: BTreeMap<String, Vec<Option<AddressRoute>>>,
    /// What each open RTSP worker was last told it may go back to on its
    /// own, by place in `rtsp_urls`.
    rtsp_may_return: BTreeMap<String, Vec<usize>>,
    next_rtsp_epoch: u64,
    /// The recording in progress for each camera (S40).
    pub(crate) recording: BTreeMap<String, String>,
    /// Vehicles whose onboard cameras were told to record (S145): the start
    /// receipt, shared by every stream because the command targets all cameras.
    pub(crate) camera_recording: BTreeMap<String, String>,
    /// Each configured capture run, independent of updates to its source counters.
    capture_epochs: BTreeMap<String, u64>,
    /// Permission failures need an explicit selection to request/recheck access.
    capture_permission_issues: std::collections::BTreeSet<String>,
    file_epochs: BTreeMap<String, u64>,
    /// Session-local recording intent, independent of signal loss.
    pub(crate) by_hand: BTreeMap<String, bool>,
    /// When each recording's progress was last journaled.
    pub(crate) progress_journaled: BTreeMap<String, u64>,
    /// Recordings whose files were deleted outside UXDuck this session
    /// (S151), for telling what of them never reached the account.
    pub(crate) forgotten: BTreeMap<String, uxduck_core::domain::video::VideoRecording>,
    /// The assistant's looks waiting for the video host (FB-244), by id.
    pub(crate) looks: BTreeMap<String, crate::looks::Waiting>,
    /// The UDP port the listener holds, or why it holds none.
    listening: Option<u16>,
    listen_error: Option<String>,
}

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
        identity: None,
    }
}

/// What a source is called until someone renames it: where it comes from.
fn default_name(source: &VideoSource) -> String {
    if let Some(input) = &source.file {
        return input.name.clone();
    }
    if let Some(input) = &source.capture {
        return input.name.clone();
    }
    source
        .url
        .as_deref()
        .map_or_else(|| udp_name(&source.id, source.port, &source.sender), redact)
}

/// The id of the camera waiting on a UDP port added by hand, before any
/// stream arrives there: `udp-5601`. The first stream to arrive, whose id is
/// `udp-5601-<sender>`, becomes it.
fn waiting_id(port: u16) -> String {
    format!("udp-{port}")
}

/// A camera added on a UDP port that nothing has sent to yet.
fn is_waiting(source: &VideoSource) -> bool {
    source.id == waiting_id(source.port)
        && source.url.is_none()
        && source.capture.is_none()
        && source.file.is_none()
}

/// A name typed by hand: trimmed, empty for none, and kept whole however
/// long (maintainer, 2026-11-15). Where it does not fit, the window shortens
/// it on screen.
fn hand_name(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no video source {id}"))
}

/// Whether two addresses can say a camera and a vehicle are the same
/// machine. Loopback cannot: on one computer, everything is loopback.
fn same_machine(a: &str, b: &str) -> bool {
    let (a, b) = (sender_host(a), sender_host(b));
    let local = |host: &str| {
        host.parse::<std::net::IpAddr>()
            .map_or(host == "localhost", |ip| {
                ip.is_loopback() || ip.is_unspecified()
            })
    };
    a == b && !local(a)
}

/// Whether a camera address is on a vehicle connection's remote host: the
/// hosts as written, never resolved (ADR-0057 D10).
fn on_route(url: &str, route: &VehicleRoute) -> bool {
    url_host(url).eq_ignore_ascii_case(sender_host(&route.peer))
}

/// The connection a vehicle uses now: its selected route, else its
/// telemetry route.
pub(crate) fn route_in_use(vehicle: &Vehicle) -> Option<&VehicleRoute> {
    let routes = &vehicle.connections.as_ref()?.routes;
    routes
        .iter()
        .find(|r| r.selected)
        .or_else(|| routes.iter().find(|r| r.telemetry))
}

fn validate_capture(input: &CaptureInput) -> Result<(), ApiError> {
    let invalid = |why: &str| ApiError::new(ErrorKind::InvalidValue, why);
    if input.no_sound && input.record_sound {
        return Err(invalid("Choose No sound or a sound input, not both."));
    }
    if input.device_id.is_empty()
        || input.device_id.len() > 1024
        || input.name.trim().is_empty()
        || input.name.len() > 256
    {
        return Err(invalid("Choose a camera from this computer's device list."));
    }
    if input.width.is_some() != input.height.is_some()
        || input
            .width
            .is_some_and(|n| !(16..=4096).contains(&n) || n % 2 != 0)
        || input
            .height
            .is_some_and(|n| !(16..=4096).contains(&n) || n % 2 != 0)
        || input
            .frame_rate
            .is_some_and(|n| !n.is_finite() || !(1.0..=120.0).contains(&n))
    {
        return Err(invalid(
            "Choose an even picture size from 16 to 4096 and a frame rate from 1 to 120.",
        ));
    }
    // Sound needs its input chosen (S148); an input with sound off is kept.
    match &input.sound_input {
        None if input.record_sound => {
            return Err(invalid("Choose a sound input to record sound."));
        }
        Some(sound)
            if sound.device_id.is_empty()
                || sound.device_id.len() > 1024
                || sound.name.trim().is_empty()
                || sound.name.len() > 256 =>
        {
            return Err(invalid(
                "Choose a sound input from this computer's device list.",
            ));
        }
        _ => {}
    }
    Ok(())
}

impl Engine {
    /// The composition root's `/video` hub. Sources that were receiving
    /// when the daemon last stopped are stopped now.
    pub fn set_video(&mut self, port: Box<dyn VideoPort>) {
        self.video.port = Some(port);
        let ports = self.state.settings.video_ports.clone();
        self.listen_video_ports(&[], &ports);
        // A second stream from one address is known by the port it sent
        // from, which the sender picks anew each time: forgotten at start
        // unless someone named it, chose its vehicle, or it was recorded.
        let stale: Vec<String> = self
            .state
            .video_sources
            .values()
            .filter(|s| s.id.contains("-from-") && !s.named_by_hand && !s.assigned_by_hand)
            .filter(|s| {
                !self
                    .state
                    .video_recordings
                    .values()
                    .any(|r| r.source_id == s.id)
            })
            .map(|s| s.id.clone())
            .collect();
        for id in stale {
            self.commit_as(actor(), Change::VideoSourceRemoved { id });
        }
        // Named before 2026-10-05 by the host alone: now by the address.
        let renamed: Vec<VideoSource> = self
            .state
            .video_sources
            .values()
            .filter(|s| !s.named_by_hand && s.name != default_name(s))
            .cloned()
            .collect();
        for mut source in renamed {
            source.name = default_name(&source);
            source.version += 1;
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
        let now = self.clock.now_us();
        let receiving: Vec<VideoSource> = self
            .state
            .video_sources
            .values()
            .filter(|s| s.state != VideoState::Stopped)
            .cloned()
            .collect();
        for mut source in receiving {
            source.state = VideoState::Stopped;
            source.state_detail = None;
            source.counters.stop();
            source.changed_at_us = now;
            source.version += 1;
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
        self.recover_recordings();
        self.review_recordings();
    }

    /// Whether the listener holds its port, and if not, why not (V3): a
    /// busy port is explained once, and the warning goes when it listens.
    pub fn video_listening(&mut self, port: Option<u16>, error: Option<String>) {
        self.video.listening = port;
        self.video.listen_error.clone_from(&error);
        match error {
            Some(error) => self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::System,
                title: "UXDuck can't receive video".into(),
                body: Some(error),
                actions: Vec::new(),
                condition: Some(LISTEN_CONDITION.into()),
                report: None,
            }),
            None => self.resolve(LISTEN_CONDITION),
        }
    }

    /// Listens on the ports added and stops on the ports taken away.
    pub(crate) fn listen_video_ports(&mut self, before: &[u16], after: &[u16]) {
        let Some(port) = self.video.port.as_mut() else {
            return;
        };
        // Looked up in sets, so a long list takes time in proportion to it.
        let was: std::collections::HashSet<u16> = before.iter().copied().collect();
        let now: std::collections::HashSet<u16> = after.iter().copied().collect();
        for p in before.iter().filter(|p| !now.contains(p)) {
            port.unlisten(*p);
            self.video.extra_errors.remove(p);
        }
        // A port that could not be opened is tried again whenever the list
        // changes: taking others away may have made room for it.
        let wanted: Vec<u16> = after
            .iter()
            .filter(|p| !was.contains(p) || self.video.extra_errors.contains_key(p))
            .copied()
            .collect();
        for p in &wanted {
            match port.listen(*p) {
                Ok(()) => {
                    self.video.extra_errors.remove(p);
                }
                Err(error) => {
                    self.video.extra_errors.insert(*p, error);
                }
            }
        }
        // Said while a port added by hand is not listened on, as the port
        // UXDuck listens on anyway is, and gone once they all are. The
        // first one's reason is given: a busy port names who holds it, and
        // a computer with no more sockets to give names its own number.
        match self.video.extra_errors.values().next().cloned() {
            Some(first) => {
                let ports = self.video.extra_errors.len();
                self.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Transient,
                    subject: Subject::System,
                    title: if ports == 1 {
                        "UXDuck is not listening on a UDP port you added".into()
                    } else {
                        format!("UXDuck is not listening on {ports} UDP ports you added")
                    },
                    body: Some(first),
                    actions: Vec::new(),
                    condition: Some(PORTS_CONDITION.into()),
                    report: None,
                });
            }
            None => self.resolve(PORTS_CONDITION),
        }
    }

    /// The vehicle whose link hears the sender's address, if one does (V5).
    fn vehicle_by_address(&self, sender: &str) -> Option<String> {
        self.state
            .vehicles
            .values()
            .filter(|v| !v.dismissed && same_machine(&v.peer, sender))
            .max_by_key(|v| v.last_heartbeat_us)
            .map(|v| v.id.clone())
    }

    /// A vehicle's camera named a stream (the MAVLink camera protocol, V3,
    /// V5): an RTSP stream becomes a source of that vehicle, fetched once a
    /// session; a UDP port is listened on, and what arrives there is that
    /// vehicle's.
    pub fn video_stream_announced(
        &mut self,
        vehicle_id: &str,
        component_id: u8,
        stream_id: u8,
        uri: &str,
    ) {
        let uri = uri.trim();
        if let Some(port) = uri
            .strip_prefix("udp://")
            .and_then(|rest| rest.rsplit(':').next())
            .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
        {
            self.video
                .announced_ports
                .insert(port, vehicle_id.to_owned());
            if !self.listened(port)
                && let Some(video) = self.video.port.as_mut()
                && video.listen(port).is_ok()
            {
                self.video.announced_listens.insert(port);
            }
            let unclaimed: Vec<VideoSource> = self
                .state
                .video_sources
                .values()
                .filter(|s| {
                    s.port == port
                        && s.url.is_none()
                        && s.capture.is_none()
                        && s.file.is_none()
                        && s.vehicle_id.is_none()
                })
                .cloned()
                .collect();
            // Unless its cameras say none of them takes images or records (S404).
            let captures = self.vehicle_captures(vehicle_id);
            for mut source in unclaimed {
                source.vehicle_id = Some(vehicle_id.to_owned());
                // The vehicle's own camera named this port (V20).
                source.vehicle_camera |= !source.vehicle_camera_by_hand && captures;
                source.version += 1;
                self.commit_as(actor(), Change::VideoSourceUpserted { source });
            }
            return;
        }
        let Ok(Input::Rtsp { host, port }) = input(uri) else {
            return;
        };
        let id = format!("cam-{vehicle_id}-{component_id}-{stream_id}");
        let current = self.state.video_sources.get(&id);
        if current.is_some_and(|s| s.url.as_deref() == Some(uri)) && self.video.opened.contains(&id)
        {
            return;
        }
        if current.is_none_or(|s| s.url.as_deref() != Some(uri)) {
            if self.video.opened.remove(&id)
                && let Some(port) = self.video.port.as_mut()
            {
                port.close_rtsp(&id);
            }
            let now = self.clock.now_us();
            let source = VideoSource {
                recording_requested: false,
                reencoding: None,
                id: id.clone(),
                name: redact(uri),
                named_by_hand: false,
                port,
                sender: format!("{host}:{port}"),
                container: uxduck_core::domain::video::VideoContainer::Rtp,
                codec: None,
                width: None,
                height: None,
                frame_rate: None,
                state: VideoState::Stopped,
                state_detail: Some(format!(
                    "Connecting to {}",
                    uxduck_core::domain::video::redact(uri)
                )),
                vehicle_id: Some(vehicle_id.to_owned()),
                assigned_by_hand: false,
                // The vehicle's own camera announced it: it can be told to
                // shoot (V20), unless its cameras say none of them does (S404).
                vehicle_camera: self.vehicle_captures(vehicle_id),
                vehicle_camera_by_hand: false,
                served_by: None,
                record_alone: false,
                klv: false,
                url: Some(uri.to_owned()),
                fallback_urls: Vec::new(),
                internet: None,
                active_url: None,
                active_choice: None,
                added_by_hand: false,
                capture: None,
                file: None,
                tracks: Vec::new(),
                audio: None,
                counters: uxduck_core::domain::video::VideoCounters::default(),
                first_seen_us: now,
                // Long ago, so its first picture says "Video from …".
                changed_at_us: 0,
                version: current.map_or(1, |s| s.version + 1),
            };
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
        self.review_recordings();
    }

    /// Ignore reports already queued by an RTSP task that has been closed/removed.
    pub fn rtsp_observed(&mut self, report: VideoReport) {
        if self.video.opened.contains(&report.source_id()) {
            self.video_observed(report);
        }
    }

    /// The companion computer serving an RTSP camera playing from `host`,
    /// and its clock (S357), among the computers in the profile of the
    /// camera's vehicle: the one chosen for the camera (S358), else the one
    /// whose Raspberry Pi camera one of the camera's addresses names, as
    /// `/front` does through a gateway's relay (ADR-0060 C8), else the one
    /// whose host is `host`, letter case and IPv6 brackets aside. A chosen
    /// computer no longer in the profile serves nothing. The RTSP task asks
    /// once a second, and the camera's ⓘ tries its clock before the RTSP
    /// server's.
    #[must_use]
    pub fn camera_companion(&self, source_id: &str, host: &str) -> Option<Companion> {
        let source = self.state.video_sources.get(source_id)?;
        let vehicle = self.vehicle(source.vehicle_id.as_deref()?)?;
        let bare = |host: &str| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase()
        };
        let computers = &vehicle.companions;
        let computer = match &source.served_by {
            Some(id) => computers.iter().find(|c| &c.id == id),
            None => computers
                .iter()
                .find(|c| {
                    c.check
                        .as_ref()
                        .and_then(|check| check.cameras.as_ref())
                        .and_then(|cameras| {
                            cameras.named_by(
                                source
                                    .url
                                    .iter()
                                    .chain(&source.fallback_urls)
                                    .map(String::as_str),
                            )
                        })
                        .is_some()
                })
                .or_else(|| computers.iter().find(|c| bare(&c.host) == bare(host))),
        }?;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds to microseconds, well within range"
        )]
        let us = |ms: f64| (ms * 1000.0).round() as i64;
        let clock =
            self.computer_clock(&vehicle.id, computer.component_id)
                .map(|clock| match clock.measured {
                    ClockMeasured::Timesync { within_ms } => CompanionClock::Timesync(Offset {
                        offset_us: us(clock.offset_ms),
                        within_us: us(within_ms).unsigned_abs(),
                    }),
                    ClockMeasured::OneWay => CompanionClock::OneWay {
                        offset_us: us(clock.offset_ms),
                    },
                });
        Some(Companion {
            computer_id: computer.component_id,
            clock,
        })
    }

    pub fn rtsp_stopped(&mut self, source_id: &str, why: String) {
        if self.video.opened.contains(source_id) {
            self.video_stopped_because(source_id, Some(why));
        }
    }

    /// What the listener heard from one sender in the last second or so.
    #[allow(clippy::too_many_lines, reason = "one source's report, field by field")]
    pub fn video_observed(&mut self, report: VideoReport) {
        let now = self.clock.now_us();
        let id = report.source_id();
        let before = self.state.video_sources.get(&id).cloned();
        // An aborted RTSP task may already have posted its last report.
        if before.as_ref().is_some_and(|s| s.url.is_some()) && !self.video.needed.contains(&id) {
            return;
        }
        let state = if report.unsupported.is_some() {
            VideoState::Unsupported
        } else {
            VideoState::Receiving
        };
        // The first stream on a port added by hand becomes the camera
        // waiting there, with the name, vehicle and settings given to it.
        let waiting = (before.is_none()
            && id.starts_with(&format!("{}-", waiting_id(report.port))))
        .then(|| self.state.video_sources.get(&waiting_id(report.port)))
        .flatten()
        .filter(|s| is_waiting(s))
        .cloned();
        let mut source = before.clone().unwrap_or_else(|| VideoSource {
            recording_requested: false,
            reencoding: None,
            id: id.clone(),
            name: udp_name(&id, report.port, &report.sender),
            named_by_hand: false,
            port: report.port,
            sender: report.sender.clone(),
            container: report.container,
            codec: None,
            width: None,
            height: None,
            frame_rate: None,
            state,
            state_detail: None,
            vehicle_id: None,
            assigned_by_hand: false,
            vehicle_camera: false,
            vehicle_camera_by_hand: false,
            served_by: None,
            record_alone: false,
            klv: false,
            url: None,
            fallback_urls: Vec::new(),
            internet: None,
            active_url: None,
            active_choice: None,
            added_by_hand: false,
            capture: None,
            file: None,
            tracks: Vec::new(),
            audio: None,
            counters: report.counters.clone(),
            first_seen_us: now,
            changed_at_us: now,
            version: 0,
        });
        if let Some(waiting) = waiting {
            source.name = waiting.name;
            source.named_by_hand = waiting.named_by_hand;
            source.vehicle_id = waiting.vehicle_id;
            source.assigned_by_hand = waiting.assigned_by_hand;
            source.vehicle_camera = waiting.vehicle_camera;
            source.vehicle_camera_by_hand = waiting.vehicle_camera_by_hand;
            source.record_alone = waiting.record_alone;
            source.added_by_hand = true;
            self.commit_as(actor(), Change::VideoSourceRemoved { id: waiting.id });
        }
        let was_stopped = before
            .as_ref()
            .is_some_and(|b| b.state == VideoState::Stopped);
        source.sender.clone_from(&report.sender);
        if source.url.is_none() && !source.named_by_hand {
            source.name = default_name(&source);
        }
        source.container = report.container;
        // A picture size or codec, once known, stays until it changes.
        if report.codec.is_some() {
            source.codec.clone_from(&report.codec);
            source.width = report.width;
            source.height = report.height;
        }
        if report.frame_rate.is_some() {
            source.frame_rate = report.frame_rate;
        }
        source.state = state;
        source.state_detail = report.unsupported.clone().or_else(|| report.note.clone());
        // Its sound, once known, stays until the stream says otherwise (S147).
        if report.audio.is_some() {
            source.audio.clone_from(&report.audio);
        }
        if source.url.is_some() && report.id.is_none() {
            // A sender on a port is never an added source.
            return;
        }
        source.klv |= report.klv;
        // A source with an address of its own (RTSP) keeps the vehicle it
        // was added or announced for.
        if !source.assigned_by_hand
            && source.url.is_none()
            && source.capture.is_none()
            && source.file.is_none()
        {
            // The address, then a camera's announcement, then what its KLV
            // named (kept from before).
            let announced = self.video.announced_ports.get(&report.port).cloned();
            source.vehicle_id = self
                .vehicle_by_address(&report.sender)
                .or_else(|| announced.clone())
                .or_else(|| before.as_ref().and_then(|b| b.vehicle_id.clone()));
            // On a port the vehicle's own camera named, the stream is that
            // camera's (V20).
            // Unless its cameras say none of them takes images or
            // records (S404).
            if !source.vehicle_camera_by_hand
                && let Some(announced) = &announced
            {
                source.vehicle_camera = source.vehicle_id.as_deref() == Some(announced)
                    && self.vehicle_captures(announced);
            }
        }
        if was_stopped {
            source.changed_at_us = now;
        }
        self.video.live.insert(id.clone(), report.counters.clone());
        let mut journaled = source.clone();
        journaled.counters = before
            .as_ref()
            .map_or_else(|| report.counters.clone(), |b| b.counters.clone());
        let changed = before.as_ref() != Some(&journaled);
        let counters_due = self
            .video
            .journaled_at
            .get(&id)
            .is_none_or(|at| now.saturating_sub(*at) >= JOURNAL_COUNTERS_US);
        if changed || counters_due {
            source.counters = report.counters;
            source.version += 1;
            self.video.journaled_at.insert(id.clone(), now);
            self.commit_as(
                actor(),
                Change::VideoSourceUpserted {
                    source: source.clone(),
                },
            );
        }
        self.review_recordings();
        // A new stream notifies once; one back within 30 s is the same (V14).
        let new = before.as_ref().is_none_or(|b| {
            b.state == VideoState::Stopped && now.saturating_sub(b.changed_at_us) > SAME_SOURCE_US
        });
        if new {
            self.notify_new_video(&source);
        }
    }

    /// A camera that sends no IDR could not be re-encoded for live view.
    /// The encoder now re-encoding a camera that sends no IDR, or none.
    pub fn video_reencoding(&mut self, source_id: &str, encoder: Option<String>) {
        let Some(mut source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        if source.reencoding == encoder {
            return;
        }
        source.reencoding = encoder;
        source.version += 1;
        self.commit_as(actor(), Change::VideoSourceUpserted { source });
    }

    /// How cameras that send no IDR are re-encoded (Settings › Cameras).
    #[must_use]
    pub fn video_reencoder(&self) -> uxduck_core::domain::video::Reencoder {
        self.state.settings.video_reencoder
    }

    pub fn video_reencode_failed(&mut self, source_id: &str, why: String) {
        let Some(source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Transient,
            subject: Subject::VideoSource {
                id: source.id.clone(),
            },
            title: format!("Can't play {} live", source.name),
            body: Some(why),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }

    fn notify_new_video(&mut self, source: &VideoSource) {
        let from = source
            .vehicle_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .map_or_else(|| source.name.clone(), |v| v.name.clone());
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::VideoSource {
                id: source.id.clone(),
            },
            title: format!("Video from {from}"),
            body: None,
            actions: vec![NotificationAction {
                label: "Open video".into(),
                action: Action::OpenVideo {
                    source_id: source.id.clone(),
                },
            }],
            condition: None,
            report: None,
        });
    }

    /// Nothing has come from a sender for a few seconds.
    pub fn video_stopped(&mut self, source_id: &str) {
        self.video_stopped_because(source_id, None);
    }

    /// An RTSP source lost its connection or could not make one, and why.
    pub fn video_stopped_because(&mut self, source_id: &str, why: Option<String>) {
        let Some(mut source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        if source.state == VideoState::Stopped && source.state_detail == why {
            return;
        }
        if let Some(live) = self.video.live.get(source_id) {
            source.counters = live.clone();
        }
        source.counters.stop();
        if source.state != VideoState::Stopped {
            source.changed_at_us = self.clock.now_us();
        }
        source.state = VideoState::Stopped;
        source.state_detail = why;
        source.version += 1;
        self.video
            .journaled_at
            .insert(source.id.clone(), self.clock.now_us());
        self.commit_as(actor(), Change::VideoSourceUpserted { source });
        // Signal loss ends the current segment while recording intent stays.
        // Recording intent survives signal loss; only an explicit stop clears it.
        self.review_recordings();
    }

    /// A `/video` socket closed: its watch is over.
    pub fn video_socket_closed(&mut self, token: &str) {
        self.video.watches.remove(token);
        self.video.pending_watches.remove(token);
        self.release_watched_ports();
        self.review_recordings();
    }

    /// Whether a listener holds `port`, whoever asked for it.
    fn listened(&self, port: u16) -> bool {
        self.video.listening == Some(port)
            || self.state.settings.video_ports.contains(&port)
            || self.video.announced_listens.contains(&port)
            || self.video.watched_listens.contains(&port)
    }

    /// A UDP source is watched on a port nothing listens on: listen there
    /// while it is watched, or say on the source why that can't be.
    fn listen_for_watch(&mut self, source_id: &str) {
        let Some(source) = self.state.video_sources.get(source_id) else {
            return;
        };
        if source.file.is_some()
            || source.capture.is_some()
            || source.url.is_some()
            || self.listened(source.port)
        {
            return;
        }
        let (port, stopped) = (source.port, source.state == VideoState::Stopped);
        let Some(video) = self.video.port.as_mut() else {
            return;
        };
        match video.listen(port) {
            Ok(()) => {
                self.video.watched_listens.insert(port);
                if stopped {
                    self.video_stopped_because(source_id, None);
                }
            }
            Err(error) if stopped => {
                self.video_stopped_because(
                    source_id,
                    Some(format!("Can't receive this camera: {error}")),
                );
            }
            Err(_) => {}
        }
    }

    /// Stops listening on the ports opened for watches that have all ended.
    fn release_watched_ports(&mut self) {
        let mut watched: std::collections::BTreeSet<u16> = self
            .video
            .watches
            .values()
            .chain(self.video.metadata_watches.values())
            .filter_map(|(_, id)| self.state.video_sources.get(id))
            .map(|s| s.port)
            .collect();
        watched.extend(
            self.video
                .needed
                .iter()
                .filter_map(|id| self.state.video_sources.get(id))
                .map(|s| s.port),
        );
        let idle: Vec<u16> = self
            .video
            .watched_listens
            .iter()
            .filter(|p| !watched.contains(p))
            .copied()
            .collect();
        for p in idle {
            self.video.watched_listens.remove(&p);
            // Added by hand or named by a camera since: theirs now.
            if self.state.settings.video_ports.contains(&p) {
                continue;
            }
            if self.video.announced_ports.contains_key(&p) {
                self.video.announced_listens.insert(p);
                continue;
            }
            if let Some(video) = self.video.port.as_mut() {
                video.unlisten(p);
            }
        }
    }

    /// Ends a connection's watches when it goes.
    pub(crate) fn drop_video_watches(&mut self, connection: ConnectionId) {
        let tokens: Vec<String> = self
            .video
            .watches
            .iter()
            .filter(|(_, (c, _))| *c == connection)
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.video.watches.remove(&token);
            if let Some(port) = self.video.port.as_mut() {
                port.revoke(&token);
            }
        }
        self.video
            .metadata_watches
            .retain(|_, (c, _)| *c != connection);
        self.video
            .pending_watches
            .retain(|token, _| self.video.watches.contains_key(token));
        self.release_watched_ports();
        self.review_recordings();
    }

    fn video_sources(&self) -> Vec<VideoSourceDto> {
        self.state
            .video_sources
            .values()
            .map(|source| {
                let mut dto = VideoSourceDto::from(source);
                if source.state != VideoState::Stopped
                    && let Some(live) = self.video.live.get(&source.id)
                {
                    dto.counters = live.into();
                }
                dto
            })
            .collect()
    }

    fn video_assign(&mut self, params: VideoAssignParams) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        if let Some(vehicle_id) = &params.vehicle_id
            && !self.state.vehicles.contains_key(vehicle_id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {vehicle_id}"),
            ));
        }
        source.assigned_by_hand = params.vehicle_id.is_some();
        let vehicle_id = match params.vehicle_id {
            Some(id) => Some(id),
            None => self.vehicle_by_address(&source.sender),
        };
        // A chosen computer is the old vehicle's (S358).
        if vehicle_id != source.vehicle_id {
            source.served_by = None;
        }
        source.vehicle_id = vehicle_id;
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    /// Updates the routes to one physical camera, retaining its identity.
    #[allow(
        clippy::too_many_lines,
        reason = "validate camera routes and preserve identity in one atomic edit"
    )]
    fn video_addresses(&mut self, params: &VideoAddressesParams) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        if source.url.is_none() || !source.added_by_hand {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Only RTSP cameras you added can have fallback addresses.",
            ));
        }
        if params.urls.is_empty() || params.urls.len() > 4 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Give one preferred address and up to three fallbacks.",
            ));
        }
        let previous: Vec<_> = source
            .url
            .iter()
            .chain(source.fallback_urls.iter())
            .collect();
        let mut urls = Vec::new();
        for given in &params.urls {
            let given = given.trim();
            let url = previous
                .iter()
                .find(|u| redact(u) == given)
                .map_or_else(|| given.to_owned(), |u| (*u).clone());
            if !matches!(input(&url)?, Input::Rtsp { .. } | Input::Whep { .. }) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Fallback addresses must use RTSP or WHEP.",
                ));
            }
            if urls.iter().any(|u: &String| redact(u) == redact(&url)) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Each camera address must be different.",
                ));
            }
            if source
                .internet
                .as_ref()
                .and_then(|i| i.low_data_url.as_ref())
                .is_some_and(|low| redact(low) == redact(&url))
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Keep the low-data address separate from the camera's normal addresses.",
                ));
            }
            if self.state.video_sources.values().any(|other| {
                other.id != source.id
                    && other
                        .url
                        .iter()
                        .chain(other.fallback_urls.iter())
                        .chain(
                            other
                                .internet
                                .iter()
                                .filter_map(|i| i.low_data_url.as_ref()),
                        )
                        .any(|u| redact(u) == redact(&url))
            }) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "That address belongs to another camera. Remove the duplicate camera first, then add its address here.",
                ));
            }
            urls.push(url);
        }
        if previous.iter().copied().eq(urls.iter()) {
            return Ok(source);
        }
        let primary = urls.remove(0);
        if let Input::Rtsp { host, port } | Input::Whep { host, port } = input(&primary)? {
            source.port = port;
            source.sender = format!("{host}:{port}");
        }
        source.url = Some(primary);
        source.fallback_urls = urls;
        if let Some(internet) = &mut source.internet {
            internet.local_urls = source
                .url
                .iter()
                .chain(&source.fallback_urls)
                .filter(|url| {
                    internet
                        .local_urls
                        .iter()
                        .any(|local| redact(local) == redact(url))
                })
                .cloned()
                .collect();
        }
        source.active_url = None;
        source.active_choice = None;
        source.tracks.clear();
        if !source.named_by_hand {
            source.name = redact(source.url.as_deref().unwrap_or_default());
        }
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.video.rtsp_urls.remove(&source.id);
        self.video.rtsp_epochs.remove(&source.id);
        if self.video.opened.remove(&source.id)
            && let Some(port) = self.video.port.as_mut()
        {
            port.close_rtsp(&source.id);
        }
        self.video_stopped_because(&source.id, Some("Camera addresses changed".into()));
        self.review_recordings();
        Ok(self
            .state
            .video_sources
            .get(&source.id)
            .cloned()
            .unwrap_or(source))
    }

    /// Run a callback only for the current RTSP worker. Closed workers cannot
    /// revive an idle camera or overwrite a newly configured stream.
    pub fn rtsp_event(&mut self, id: &str, epoch: u64, event: impl FnOnce(&mut Self)) {
        if self.video.opened.contains(id) && self.video.rtsp_epochs.get(id) == Some(&epoch) {
            event(self);
        }
    }

    /// The RTSP worker tries `url` now, for `reason`. A move with the
    /// vehicle says where it connects, a failure keeps saying what failed.
    pub fn rtsp_address(&mut self, id: &str, url: String, reason: AddressReason) {
        let Some(mut source) = self.state.video_sources.get(id).cloned() else {
            return;
        };
        if matches!(
            reason,
            AddressReason::FollowedVehicle
                | AddressReason::ReturnedWithVehicle
                | AddressReason::Stalled
                | AddressReason::AheadOfVehicle
        ) && source.state == VideoState::Stopped
        {
            source.state_detail = Some(self.connecting(&source, &url));
        }
        // The address kept ready outlives a move. The worker says when it changes.
        let standby = source.active_choice.take().and_then(|c| c.standby);
        source.active_choice = (!source.fallback_urls.is_empty()).then(|| AddressChoice {
            reason,
            since_us: self.clock.now_us(),
            link_id: self.address_link(&source, &url),
            next_check_us: None,
            standby,
            moved: None,
            ahead: self.camera_ahead(&source, &url, reason),
        });
        source.active_url = Some(url);
        source.version += 1;
        self.commit_as(actor(), Change::VideoSourceUpserted { source });
    }

    /// While on a fallback, the RTSP worker probes the earlier addresses
    /// again in `in_us`, or, with none, waits for the vehicle (ADR-0057
    /// D11 and D13). On its vehicle's connection after it went ahead and
    /// fell back, when it may go ahead again (D13).
    pub fn rtsp_next_check(&mut self, id: &str, in_us: Option<u64>) {
        let now = self.clock.now_us();
        self.change_choice(id, |c| {
            c.next_check_us = in_us.map(|us| now.saturating_add(us));
        });
    }

    /// The address the RTSP worker keeps set up to move to, and whether it
    /// is ready (ADR-0057 D12).
    pub fn rtsp_standby(&mut self, id: &str, standby: Option<(String, bool)>) {
        let Some(source) = self.state.video_sources.get(id) else {
            return;
        };
        let standby = standby.map(|(url, ready)| AddressStandby {
            link_id: self.address_link(source, &url),
            ready,
        });
        self.change_choice(id, |c| c.standby = standby);
    }

    /// How the RTSP worker's move to its active address went (ADR-0057 D14).
    pub fn rtsp_moved(&mut self, id: &str, moved: AddressMove) {
        self.change_choice(id, |c| c.moved = Some(moved));
    }

    fn change_choice(&mut self, id: &str, change: impl FnOnce(&mut AddressChoice)) {
        let Some(mut source) = self.state.video_sources.get(id).cloned() else {
            return;
        };
        let Some(choice) = source.active_choice.as_mut() else {
            return;
        };
        let before = choice.clone();
        change(choice);
        if *choice == before {
            return;
        }
        source.version += 1;
        self.commit_as(actor(), Change::VideoSourceUpserted { source });
    }

    /// The vehicle of a camera with fallback addresses: its own, else the
    /// vehicle last heard on one of its addresses' hosts.
    fn camera_vehicle(&self, source: &VideoSource) -> Option<&Vehicle> {
        if source.fallback_urls.is_empty() {
            return None;
        }
        if let Some(id) = &source.vehicle_id {
            return self.state.vehicles.get(id);
        }
        let urls: Vec<&String> = source.url.iter().chain(&source.fallback_urls).collect();
        self.state
            .vehicles
            .values()
            .filter(|v| !v.dismissed)
            .filter(|v| {
                v.connections.as_ref().is_some_and(|c| {
                    c.routes
                        .iter()
                        .any(|r| urls.iter().any(|url| on_route(url, r)))
                })
            })
            .max_by_key(|v| v.last_heartbeat_us)
    }

    /// What a vehicle's open cameras receive over one of its connections
    /// now, in bits a second (ADR-0004 2026-11-16). A camera counts on the
    /// connection on whose host the address it plays is, and as 0 on any
    /// other it has an address on. None while no open camera has one there.
    pub(crate) fn camera_bps_on(&self, vehicle: &Vehicle, link_id: &str) -> Option<u64> {
        let peers: Vec<&str> = vehicle
            .connections
            .as_ref()?
            .routes
            .iter()
            .filter(|r| r.link_id == link_id)
            .map(|r| sender_host(&r.peer))
            .collect();
        let here = |host: &str| peers.iter().any(|peer| host.eq_ignore_ascii_case(peer));
        let mut total = None;
        for source in self.state.video_sources.values() {
            let open =
                self.video.opened.contains(&source.id) || source.state == VideoState::Receiving;
            let its = source.vehicle_id.as_deref() == Some(vehicle.id.as_str())
                || self
                    .camera_vehicle(source)
                    .is_some_and(|v| v.id == vehicle.id);
            if !open || !its {
                continue;
            }
            let (mut hosts, playing): (Vec<&str>, &str) = match &source.url {
                Some(url) => (
                    source.fallback_urls.iter().map(|u| url_host(u)).collect(),
                    url_host(source.active_url.as_deref().unwrap_or(url)),
                ),
                None => (Vec::new(), sender_host(&source.sender)),
            };
            hosts.extend(source.url.as_deref().map(url_host));
            if !here(playing) && !hosts.into_iter().any(here) {
                continue;
            }
            let receiving = source.state == VideoState::Receiving && here(playing);
            *total.get_or_insert(0) += self
                .video
                .live
                .get(&source.id)
                .filter(|_| receiving)
                .map_or(0, |c| c.bitrate_bps);
        }
        total
    }

    /// A hold keeping a camera's vehicle off a connection higher in its
    /// order that an address of the camera is on (ADR-0057 D13): the one it
    /// would go back to, else the highest heard. None while a person has
    /// switched the vehicle by hand, as nothing then takes it back.
    fn held_back(&self, source: &VideoSource) -> Option<AddressAhead> {
        let urls = self.video.rtsp_urls.get(&source.id)?;
        let vehicle = self.camera_vehicle(source)?;
        let connections = vehicle.connections.as_ref()?;
        let using = route_in_use(vehicle)?;
        if connections.holds.is_empty() || connections.chosen.is_some() {
            return None;
        }
        let higher = connections
            .routes
            .iter()
            .filter(|r| r.priority < using.priority && r.reachable && r.writable)
            .filter(|r| urls.iter().any(|url| on_route(url, r)))
            .min_by_key(|r| {
                (
                    connections.returns_to.as_deref() != Some(&r.link_id),
                    r.priority,
                )
            })?;
        Some(AddressAhead {
            link_id: higher.link_id.clone(),
            vehicle_link_id: using.link_id.clone(),
            holds: connections.holds.clone(),
        })
    }

    /// What `ahead` says on the choice of a camera on `url`: the hold while
    /// it is on its vehicle's connection, or when it went ahead of it.
    fn camera_ahead(
        &self,
        source: &VideoSource,
        url: &str,
        reason: AddressReason,
    ) -> Option<AddressAhead> {
        let ahead = self.held_back(source)?;
        (reason == AddressReason::AheadOfVehicle
            || self.address_link(source, url).as_deref() == Some(ahead.vehicle_link_id.as_str()))
        .then_some(ahead)
    }

    /// The vehicle connection on an address's host, which names it.
    fn address_link(&self, source: &VideoSource, url: &str) -> Option<String> {
        self.camera_vehicle(source)?
            .connections
            .as_ref()?
            .routes
            .iter()
            .find(|r| on_route(url, r))
            .map(|r| r.link_id.clone())
    }

    /// `Connecting to the Internet address, rtsp://…`: the address tried,
    /// named by the vehicle connection on its host when it has one.
    fn connecting(&self, source: &VideoSource, url: &str) -> String {
        match self
            .address_link(source, url)
            .and_then(|id| self.state.links.get(&id))
        {
            Some(link) => format!("Connecting to the {} address, {}", link.name, redact(url)),
            None => format!("Connecting to {}", redact(url)),
        }
    }

    /// Moves an open camera to the address on the host of the connection
    /// its vehicle uses, when the address it is on is on another of the
    /// vehicle's connections (ADR-0057 D10). An address on none of them
    /// gives no hint, and its health checks decide alone.
    /// Tells an open camera which earlier addresses it may go back to on
    /// its own (ADR-0057 D10, amended 2026-11-16): one on none of its
    /// vehicle's connections, or on a connection ranked above the one the
    /// vehicle uses. One on a connection ranked lower, or any while the
    /// person has switched by hand, waits for the vehicle to move there, so
    /// the video stays on the connection the person put the vehicle on.
    /// Without a vehicle using a connection, it may go back to any.
    fn hold_camera(&mut self, source_id: &str) {
        let Some(urls) = self
            .video
            .rtsp_urls
            .get(source_id)
            .filter(|_| self.video.opened.contains(source_id))
        else {
            return;
        };
        let vehicle = self
            .state
            .video_sources
            .get(source_id)
            .and_then(|source| self.camera_vehicle(source));
        let using = vehicle.and_then(route_in_use);
        let connections = vehicle.and_then(|v| v.connections.as_ref());
        let to: Vec<usize> = urls
            .iter()
            .enumerate()
            .filter(|(_, url)| {
                let (Some(using), Some(connections)) = (using, connections) else {
                    return true;
                };
                connections
                    .routes
                    .iter()
                    .find(|r| on_route(url, r))
                    .is_none_or(|r| connections.chosen.is_none() && r.priority < using.priority)
            })
            .map(|(index, _)| index)
            .collect();
        if self.video.rtsp_may_return.get(source_id) == Some(&to) {
            return;
        }
        if let Some(port) = self.video.port.as_mut() {
            port.rtsp_may_return(source_id, &to);
        }
        self.video.rtsp_may_return.insert(source_id.to_owned(), to);
    }

    fn steer_camera(&mut self, source_id: &str) {
        self.hold_camera(source_id);
        // Only the addresses its worker was given count, in their order.
        let Some(urls) = self
            .video
            .rtsp_urls
            .get(source_id)
            .filter(|_| self.video.opened.contains(source_id))
        else {
            return;
        };
        let Some(source) = self.state.video_sources.get(source_id) else {
            return;
        };
        let (Some(first), Some(vehicle)) = (urls.first(), self.camera_vehicle(source)) else {
            return;
        };
        let (Some(using), Some(connections)) = (route_in_use(vehicle), &vehicle.connections) else {
            return;
        };
        let current = source
            .active_url
            .as_ref()
            .filter(|url| urls.contains(url))
            .unwrap_or(first);
        let elsewhere = connections
            .routes
            .iter()
            .any(|r| r.link_id != using.link_id && on_route(current, r));
        if !elsewhere || on_route(current, using) {
            return;
        }
        let Some(index) = urls.iter().position(|url| on_route(url, using)) else {
            return;
        };
        if let Some(port) = self.video.port.as_mut() {
            port.steer_rtsp(source_id, index);
        }
    }

    /// Tells an open camera's worker which vehicle connection each of its
    /// addresses is on, whether the vehicle is heard there, and which one
    /// it uses (ADR-0057 D12 and D13), when that changed.
    fn route_camera(&mut self, source_id: &str) {
        let Some(urls) = self
            .video
            .rtsp_urls
            .get(source_id)
            .filter(|_| self.video.opened.contains(source_id))
        else {
            return;
        };
        let Some(source) = self.state.video_sources.get(source_id) else {
            return;
        };
        let vehicle = self.camera_vehicle(source);
        let using = vehicle.and_then(route_in_use).map(|r| r.link_id.as_str());
        let returns_to = vehicle
            .and_then(|v| v.connections.as_ref())
            .and_then(|c| c.returns_to.as_deref());
        let routes: Vec<Option<AddressRoute>> = urls
            .iter()
            .map(|url| {
                let route = vehicle?
                    .connections
                    .as_ref()?
                    .routes
                    .iter()
                    .find(|r| on_route(url, r))?;
                Some(AddressRoute {
                    link_id: route.link_id.clone(),
                    up: route.reachable,
                    in_use: using == Some(route.link_id.as_str()),
                    returns: returns_to == Some(route.link_id.as_str()),
                })
            })
            .collect();
        if self.video.rtsp_routes.get(source_id) == Some(&routes) {
            return;
        }
        if let Some(port) = self.video.port.as_mut() {
            port.route_rtsp(source_id, &routes);
        }
        self.video.rtsp_routes.insert(source_id.to_owned(), routes);
    }

    /// A vehicle changed: its cameras with fallback addresses hear how its
    /// connections are and what they may go back to, and when the
    /// connection it uses moved, they follow it, and say which connection
    /// each address is on.
    pub(crate) fn video_vehicle_changed(&mut self, before: Option<&Vehicle>, after: &Vehicle) {
        let cameras: Vec<VideoSource> = self
            .state
            .video_sources
            .values()
            .filter(|s| self.video.opened.contains(&s.id))
            .filter(|s| self.camera_vehicle(s).is_some_and(|v| v.id == after.id))
            .cloned()
            .collect();
        // A new order or a switch by hand changes what a camera may go back
        // to, without moving the vehicle, and a hold what it may go ahead to.
        for source in &cameras {
            self.route_camera(&source.id);
            self.hold_camera(&source.id);
            if let (Some(url), Some(choice)) = (&source.active_url, &source.active_choice) {
                let ahead = self.camera_ahead(source, url, choice.reason);
                self.change_choice(&source.id, |c| c.ahead = ahead);
            }
        }
        let link = |v: &Vehicle| route_in_use(v).map(|r| r.link_id.clone());
        if before.is_some_and(|b| link(b) == link(after)) {
            return;
        }
        for source in &cameras {
            self.steer_camera(&source.id);
            let Some(mut source) = self.state.video_sources.get(&source.id).cloned() else {
                continue;
            };
            let Some(url) = source.active_url.as_deref() else {
                continue;
            };
            let link_id = self.address_link(&source, url);
            let Some(choice) = source
                .active_choice
                .as_mut()
                .filter(|c| c.link_id != link_id)
            else {
                continue;
            };
            choice.link_id = link_id;
            source.version += 1;
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "edit the active route without changing its data policy and restart its worker atomically"
    )]
    fn video_track(&mut self, params: &VideoTrackParams) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        let Some(url) = source
            .url
            .clone()
            .filter(|_| source.added_by_hand && source.capture.is_none() && source.file.is_none())
        else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Only RTSP cameras you added can play another track.",
            ));
        };
        if params.track == 0 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "tracks are numbered from 1",
            ));
        }
        if let Some(offered) = u32::try_from(source.tracks.len()).ok().filter(|n| *n > 0)
            && params.track > offered
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "the camera offers {offered} video track{}",
                    if offered == 1 { "" } else { "s" }
                ),
            ));
        }
        let current = source
            .active_url
            .as_ref()
            .filter(|active| {
                source
                    .url
                    .iter()
                    .chain(&source.fallback_urls)
                    .chain(
                        source
                            .internet
                            .iter()
                            .filter_map(|i| i.low_data_url.as_ref()),
                    )
                    .any(|saved| saved == *active)
            })
            .unwrap_or(&url)
            .clone();
        let next = with_track(&current, params.track);
        if current == next {
            return Ok(source);
        }
        if self.state.video_sources.values().any(|other| {
            other
                .url
                .iter()
                .chain(&other.fallback_urls)
                .chain(
                    other
                        .internet
                        .iter()
                        .filter_map(|i| i.low_data_url.as_ref()),
                )
                .any(|saved| {
                    (other.id != source.id || *saved != current) && redact(saved) == redact(&next)
                })
        }) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "That track address is already configured. Keep each camera address separate.",
            ));
        }
        if source.url.as_deref() == Some(current.as_str()) {
            if !source.named_by_hand {
                source.name = redact(&next);
            }
            source.url = Some(next.clone());
        }
        for fallback in &mut source.fallback_urls {
            if *fallback == current {
                fallback.clone_from(&next);
            }
        }
        if let Some(internet) = &mut source.internet {
            for local in &mut internet.local_urls {
                if *local == current {
                    local.clone_from(&next);
                }
            }
            if internet.low_data_url.as_deref() == Some(current.as_str()) {
                internet.low_data_url = Some(next);
            }
        }
        for track in &mut source.tracks {
            track.playing = track.number == params.track;
        }
        source.active_url = None;
        source.active_choice = None;
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.video.rtsp_urls.remove(&source.id);
        self.video.rtsp_epochs.remove(&source.id);
        if self.video.opened.remove(&source.id)
            && let Some(port) = self.video.port.as_mut()
        {
            port.close_rtsp(&source.id);
        }
        self.video_stopped_because(&source.id, Some("Camera track changed".into()));
        self.review_recordings();
        Ok(self
            .state
            .video_sources
            .get(&source.id)
            .cloned()
            .unwrap_or(source))
    }

    /// The tracks an RTSP camera offers, from its SDP, each time it answers.
    pub fn video_metadata_announced(&mut self, source_id: &str) {
        if let Some(mut source) = self.state.video_sources.get(source_id).cloned()
            && !source.klv
        {
            source.klv = true;
            source.version += 1;
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
    }

    pub fn video_tracks(&mut self, source_id: &str, tracks: Vec<VideoTrack>) {
        let Some(mut source) = self.state.video_sources.get(source_id).cloned() else {
            return;
        };
        if source.tracks == tracks {
            return;
        }
        source.tracks = tracks;
        source.version += 1;
        self.commit_as(actor(), Change::VideoSourceUpserted { source });
    }

    fn video_rename(&mut self, params: &VideoRenameParams) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        let name = params.name.trim();
        source.named_by_hand = !name.is_empty();
        source.name = if name.is_empty() {
            default_name(&source)
        } else {
            name.to_owned()
        };
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        Ok(source)
    }

    /// The companion computer serving an RTSP camera (S358): one of its
    /// vehicle's, or none to find it by the camera address's host.
    fn video_served_by(&mut self, params: VideoServedByParams) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        if source.url.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Only an RTSP camera is served by a companion computer.",
            ));
        }
        if let Some(id) = &params.computer_id {
            let theirs = source
                .vehicle_id
                .as_deref()
                .and_then(|v| self.vehicle(v))
                .is_some_and(|v| v.companions.iter().any(|c| &c.id == id));
            if !theirs {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Choose one of the companion computers of the camera's vehicle.",
                ));
            }
        }
        source.served_by = params.computer_id;
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        Ok(source)
    }

    /// Whether the vehicle's own camera shoots along (S145): set by hand, kept.
    fn video_vehicle_camera(
        &mut self,
        params: &uxduck_contracts::dto::video::VideoVehicleCameraParams,
    ) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no video source {}", params.source_id),
                )
            })?;
        if source.file.is_some() && params.on {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Recorded footage cannot trigger a vehicle's camera.",
            ));
        }
        source.vehicle_camera = params.on;
        source.vehicle_camera_by_hand = true;
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    fn video_reveal(&mut self, params: &VideoRevealParams) -> Result<VideoRevealResult, ApiError> {
        if let Some(id) = &params.photo_id {
            return self
                .reveal_photo(id)
                .map(|folder| VideoRevealResult { folder });
        }
        let file = match &params.recording_id {
            Some(id) => {
                let recording = self.state.video_recordings.get(id).ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, format!("no recording {id}"))
                })?;
                Some(
                    recording
                        .packed
                        .as_ref()
                        .map_or_else(|| recording.file(), |packed| packed.path.clone()),
                )
            }
            None => None,
        };
        let port = self.video.port.as_mut().ok_or_else(|| {
            ApiError::new(ErrorKind::InvalidRequest, "Recordings need UXDuck Desktop.")
        })?;
        port.reveal(file.as_deref())
            .map(|folder| VideoRevealResult { folder })
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))
    }

    /// A capture report can only update its still-enabled configuration. Late
    /// subprocess output after removal, pause or reconfiguration is discarded.
    pub fn capture_observed(&mut self, epoch: u64, input: &CaptureInput, report: VideoReport) {
        let id = report.source_id();
        if self.video.capture_epochs.get(&id) == Some(&epoch)
            && self
                .state
                .video_sources
                .get(&id)
                .and_then(|s| s.capture.as_ref())
                == Some(input)
            && input.enabled
        {
            self.video_observed(report);
        }
    }

    pub fn capture_stopped(
        &mut self,
        source_id: &str,
        input: &CaptureInput,
        epoch: u64,
        why: String,
    ) {
        if self.video.capture_epochs.get(source_id) == Some(&epoch)
            && self
                .state
                .video_sources
                .get(source_id)
                .and_then(|s| s.capture.as_ref())
                == Some(input)
            && input.enabled
        {
            self.video_stopped_because(source_id, Some(why));
        }
    }

    /// Permission failures never restart a helper by themselves. A later explicit
    /// selection of the same configuration must be able to request access again.
    pub fn capture_permission_issue(&mut self, source_id: &str, input: &CaptureInput, epoch: u64) {
        if self.video.capture_epochs.get(source_id) == Some(&epoch)
            && self
                .state
                .video_sources
                .get(source_id)
                .and_then(|s| s.capture.as_ref())
                == Some(input)
        {
            self.video
                .capture_permission_issues
                .insert(source_id.to_owned());
        }
    }

    pub fn file_observed(
        &mut self,
        epoch: u64,
        input: &uxduck_core::domain::video::VideoFileInput,
        report: VideoReport,
    ) {
        let id = report.source_id();
        if self.video.file_epochs.get(&id) == Some(&epoch)
            && self
                .state
                .video_sources
                .get(&id)
                .and_then(|s| s.file.as_ref())
                == Some(input)
        {
            self.video_observed(report);
        }
    }
    pub fn file_stopped(&mut self, id: &str, epoch: u64, why: String) {
        if self.video.file_epochs.get(id) == Some(&epoch) {
            self.video_stopped_because(id, Some(why));
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "a new file source and its persisted fields, one by one"
    )]
    fn video_file(
        &mut self,
        params: uxduck_contracts::dto::video::VideoFileParams,
    ) -> Result<VideoSource, ApiError> {
        let invalid = |why| ApiError::new(ErrorKind::InvalidValue, why);
        let port = self
            .video
            .port
            .as_ref()
            .ok_or_else(|| invalid("Video files need UXDuck Desktop.".to_owned()))?;
        let mut input = match (params.path, params.demo_id) {
            (Some(path), None) if !path.is_empty() && path.len() <= 4096 => {
                port.check_file(&path).map_err(invalid)?;
                let name = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("Recorded video")
                    .to_owned();
                uxduck_core::domain::video::VideoFileInput {
                    path,
                    name,
                    repeat: params.repeat,
                    demo_id: None,
                }
            }
            (None, Some(id)) => port.demo_file(&id).map_err(invalid)?,
            _ => {
                return Err(invalid(
                    "Choose one video file or downloaded demo.".to_owned(),
                ));
            }
        };
        input.repeat = params.repeat;
        let now = self.clock.now_us();
        let mut source = if let Some(id) = params.source_id {
            let source = self
                .state
                .video_sources
                .get(&id)
                .cloned()
                .ok_or_else(|| not_found(&id))?;
            if source.file.is_none() {
                return Err(invalid(
                    "Only a recorded-video camera can have its footage replaced.".to_owned(),
                ));
            }
            source
        } else {
            // However many (maintainer, 2026-11-15): one costs nothing until
            // it is watched or recorded.
            // The same source shape and lifecycle as a manually added camera.
            VideoSource {
                recording_requested: false,
                reencoding: None,
                id: format!("vs{}", &self.random.token()[..16]),
                name: input.name.clone(),
                named_by_hand: false,
                port: 0,
                sender: "recorded video".into(),
                container: uxduck_core::domain::video::VideoContainer::Ts,
                codec: None,
                width: None,
                height: None,
                frame_rate: None,
                state: VideoState::Stopped,
                state_detail: None,
                vehicle_id: None,
                assigned_by_hand: false,
                vehicle_camera: false,
                vehicle_camera_by_hand: false,
                served_by: None,
                record_alone: false,
                klv: false,
                url: None,
                fallback_urls: Vec::new(),
                internet: None,
                active_url: None,
                active_choice: None,
                added_by_hand: true,
                capture: None,
                file: None,
                tracks: Vec::new(),
                audio: None,
                counters: uxduck_core::domain::video::VideoCounters::default(),
                first_seen_us: now,
                changed_at_us: now,
                version: 0,
            }
        };
        // Choosing the same file again is an explicit replay, including after
        // a once-only run reached EOF. Configuration equality cannot skip it.
        if let Some(port) = self.video.port.as_mut() {
            port.close_file(&source.id);
        }
        self.video.file_epochs.remove(&source.id);
        if let Some(recording) = self.video.recording.remove(&source.id)
            && let Some(port) = self.video.port.as_mut()
        {
            port.stop_recording(&recording);
        }
        if !source.named_by_hand {
            source.name.clone_from(&input.name);
        }
        source.file = Some(input);
        source.codec = None;
        source.width = None;
        source.height = None;
        source.frame_rate = None;
        source.audio = None;
        source.state = VideoState::Stopped;
        source.state_detail = Some("Starting recorded video".into());
        source.changed_at_us = now;
        source.version += 1;
        self.video.live.remove(&source.id);
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    fn video_devices(&mut self) -> VideoDevicesResult {
        self.video.port.as_mut().map_or_else(
            || VideoDevicesResult {
                error: Some("Local cameras need UXDuck running on this computer.".into()),
                ..VideoDevicesResult::default()
            },
            |p| p.devices(),
        )
    }

    #[allow(clippy::too_many_lines, reason = "a new source's fields, one by one")]
    fn video_capture(&mut self, params: VideoCaptureParams) -> Result<VideoSource, ApiError> {
        let input: CaptureInput = params.input.into();
        let invalid = |why: &str| ApiError::new(ErrorKind::InvalidValue, why);
        validate_capture(&input)?;
        let port = self
            .video
            .port
            .as_ref()
            .ok_or_else(|| invalid("Local cameras need UXDuck running on this computer."))?;
        port.check_capture(&input)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        let previous = self
            .state
            .video_sources
            .values()
            .find(|s| {
                s.capture
                    .as_ref()
                    .is_some_and(|c| c.device_id == input.device_id)
            })
            .cloned();
        if let Some(same) = &previous
            && same.capture.as_ref() == Some(&input)
            && !self.video.capture_permission_issues.contains(&same.id)
        {
            return Ok(same.clone());
        }
        // However many the computer has (maintainer, 2026-11-15): one costs
        // nothing until it is watched or recorded.
        let now = self.clock.now_us();
        let mut source = previous.unwrap_or_else(|| VideoSource {
            recording_requested: false,
            reencoding: None,
            id: format!("vs{}", &self.random.token()[..16]),
            name: input.name.clone(),
            named_by_hand: false,
            port: 0,
            sender: "local capture".into(),
            container: uxduck_core::domain::video::VideoContainer::Ts,
            codec: None,
            width: None,
            height: None,
            frame_rate: None,
            state: VideoState::Stopped,
            state_detail: None,
            vehicle_id: None,
            assigned_by_hand: false,
            vehicle_camera: false,
            vehicle_camera_by_hand: false,
            served_by: None,
            record_alone: false,
            klv: false,
            url: None,
            fallback_urls: Vec::new(),
            internet: None,
            active_url: None,
            active_choice: None,
            added_by_hand: true,
            capture: None,
            file: None,
            tracks: Vec::new(),
            audio: None,
            counters: uxduck_core::domain::video::VideoCounters::default(),
            first_seen_us: now,
            changed_at_us: now,
            version: 0,
        });
        self.video.capture_epochs.remove(&source.id);
        self.video.capture_permission_issues.remove(&source.id);
        if let Some(port) = self.video.port.as_mut() {
            if source.capture.is_some() {
                port.close_capture(&source.id);
            }
            if input.enabled {
                port.select_capture(&source.id, &input);
            }
        }
        if !source.named_by_hand {
            source.name.clone_from(&input.name);
        }
        source.capture = Some(input.clone());
        source.state = VideoState::Stopped;
        source.state_detail = Some(
            if input.enabled {
                "Starting camera capture"
            } else {
                "Camera capture paused"
            }
            .into(),
        );
        source.changed_at_us = now;
        source.version = self
            .state
            .video_sources
            .get(&source.id)
            .map_or(source.version, |s| s.version)
            + 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    fn video_capture_enable(
        &mut self,
        params: &VideoCaptureEnableParams,
    ) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        let input = source
            .capture
            .as_mut()
            .ok_or_else(|| ApiError::new(ErrorKind::InvalidValue, "This is not a local camera."))?;
        if input.enabled == params.enabled
            && !self.video.capture_permission_issues.contains(&source.id)
        {
            return Ok(source);
        }
        input.enabled = params.enabled;
        let input = input.clone();
        self.video.capture_epochs.remove(&source.id);
        self.video.capture_permission_issues.remove(&source.id);
        if let Some(port) = self.video.port.as_mut() {
            port.close_capture(&source.id);
            if input.enabled {
                port.select_capture(&source.id, &input);
            }
        }
        source.state = VideoState::Stopped;
        source.state_detail = Some(
            if input.enabled {
                "Starting camera capture"
            } else {
                "Camera capture paused"
            }
            .into(),
        );
        source.changed_at_us = self.clock.now_us();
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    /// Listens on a UDP port added by hand, kept in settings; the port
    /// UXDuck listens on anyway needs no adding.
    fn add_video_port(&mut self, port: u16) -> Result<(), ApiError> {
        let mut settings = self.state.settings.clone();
        // The port UXDuck listens on anyway needs no adding, but a
        // camera on it can still be named before it sends.
        // However many (maintainer, 2026-11-15): each is a socket, and one
        // the computer cannot open is said in its own words below.
        if self.video.listening != Some(port) && !settings.video_ports.contains(&port) {
            let before = settings.video_ports.clone();
            settings.video_ports.push(port);
            settings.version += 1;
            let after = settings.video_ports.clone();
            self.commit_as(actor(), Change::SettingsChanged { settings });
            self.listen_video_ports(&before, &after);
        }
        if let Some(error) = self.video.extra_errors.get(&port) {
            return Err(ApiError::new(ErrorKind::PortInUse, error.clone()));
        }
        Ok(())
    }

    fn video_add(&mut self, params: &VideoAddParams) -> Result<VideoAddResult, ApiError> {
        let url = params.url.as_str();
        let name = hand_name(params.name.as_deref());
        if let Some(vehicle_id) = &params.vehicle_id
            && !self.state.vehicles.contains_key(vehicle_id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {vehicle_id}"),
            ));
        }
        match input(url)? {
            Input::Udp(port) => {
                self.add_video_port(port)?;
                let source = self.camera_on_port(port, name, params.vehicle_id.clone());
                Ok(VideoAddResult {
                    source: Some((&source).into()),
                    port: Some(port),
                })
            }
            Input::Rtsp { host, port } | Input::Whep { host, port } => {
                let url = url.trim().to_owned();
                if let Some(same) = self
                    .state
                    .video_sources
                    .values()
                    .find(|s| s.url.as_deref() == Some(url.as_str()))
                    .cloned()
                {
                    let same = self.name_and_assign(same, name, params.vehicle_id.clone());
                    return Ok(VideoAddResult {
                        source: Some((&same).into()),
                        port: None,
                    });
                }
                if self.state.video_sources.values().any(|s| {
                    s.fallback_urls
                        .iter()
                        .chain(s.internet.iter().filter_map(|i| i.low_data_url.as_ref()))
                        .any(|u| redact(u) == redact(&url))
                }) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidValue,
                        "This address is already a fallback for another camera.",
                    ));
                }
                let now = self.clock.now_us();
                let source = VideoSource {
                    recording_requested: false,
                    reencoding: None,
                    id: format!("vs{}", &self.random.token()[..16]),
                    name: name.clone().unwrap_or_else(|| redact(&url)),
                    named_by_hand: name.is_some(),
                    port,
                    sender: format!("{host}:{port}"),
                    container: uxduck_core::domain::video::VideoContainer::Rtp,
                    codec: None,
                    width: None,
                    height: None,
                    frame_rate: None,
                    state: VideoState::Stopped,
                    state_detail: Some(format!(
                        "Connecting to {}",
                        uxduck_core::domain::video::redact(&url)
                    )),
                    vehicle_id: params.vehicle_id.clone(),
                    assigned_by_hand: params.vehicle_id.is_some(),
                    vehicle_camera: false,
                    vehicle_camera_by_hand: false,
                    served_by: None,
                    record_alone: false,
                    klv: false,
                    url: Some(url.clone()),
                    fallback_urls: Vec::new(),
                    internet: None,
                    active_url: None,
                    active_choice: None,
                    added_by_hand: true,
                    capture: None,
                    file: None,
                    tracks: Vec::new(),
                    audio: None,
                    counters: uxduck_core::domain::video::VideoCounters::default(),
                    first_seen_us: now,
                    changed_at_us: now,
                    version: 1,
                };
                self.commit_as(
                    actor(),
                    Change::VideoSourceUpserted {
                        source: source.clone(),
                    },
                );
                self.review_recordings();
                Ok(VideoAddResult {
                    source: Some((&source).into()),
                    port: None,
                })
            }
        }
    }

    /// The camera a UDP port added by hand stands for (S39): the one waiting
    /// there, else the first already arriving, else a new one waiting for
    /// its first picture, so it can be named and given its vehicle before
    /// anything is switched on (maintainer, 2026-10-29).
    fn camera_on_port(
        &mut self,
        port: u16,
        name: Option<String>,
        vehicle_id: Option<String>,
    ) -> VideoSource {
        let existing = self
            .state
            .video_sources
            .get(&waiting_id(port))
            .or_else(|| {
                self.state
                    .video_sources
                    .values()
                    .filter(|s| {
                        s.port == port && s.url.is_none() && s.capture.is_none() && s.file.is_none()
                    })
                    .min_by_key(|s| s.first_seen_us)
            })
            .cloned();
        if let Some(source) = existing {
            return self.name_and_assign(source, name, vehicle_id);
        }
        let now = self.clock.now_us();
        let id = waiting_id(port);
        let source = VideoSource {
            recording_requested: false,
            reencoding: None,
            name: name.clone().unwrap_or_else(|| udp_name(&id, port, "")),
            named_by_hand: name.is_some(),
            id,
            port,
            sender: String::new(),
            container: uxduck_core::domain::video::VideoContainer::Ts,
            codec: None,
            width: None,
            height: None,
            frame_rate: None,
            state: VideoState::Stopped,
            state_detail: Some(format!("Waiting for video on UDP {port}")),
            assigned_by_hand: vehicle_id.is_some(),
            vehicle_id,
            vehicle_camera: false,
            vehicle_camera_by_hand: false,
            served_by: None,
            record_alone: false,
            klv: false,
            url: None,
            fallback_urls: Vec::new(),
            internet: None,
            active_url: None,
            active_choice: None,
            added_by_hand: true,
            capture: None,
            file: None,
            tracks: Vec::new(),
            audio: None,
            counters: uxduck_core::domain::video::VideoCounters::default(),
            first_seen_us: now,
            // Long ago, so its first picture says "Video from …".
            changed_at_us: 0,
            version: 1,
        };
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        source
    }

    /// Applies what was typed with a camera added again: a name, a vehicle.
    fn name_and_assign(
        &mut self,
        mut source: VideoSource,
        name: Option<String>,
        vehicle_id: Option<String>,
    ) -> VideoSource {
        if name.is_none() && vehicle_id.is_none() {
            return source;
        }
        if let Some(name) = name {
            source.name = name;
            source.named_by_hand = true;
        }
        if let Some(vehicle_id) = vehicle_id {
            source.vehicle_id = Some(vehicle_id);
            source.assigned_by_hand = true;
        }
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        source
    }

    fn video_remove(&mut self, params: &VideoRemoveParams) -> Result<(), ApiError> {
        if let Some(port) = params.port {
            let mut settings = self.state.settings.clone();
            if !settings.video_ports.contains(&port) {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("UDP {port} was not added by hand"),
                ));
            }
            let before = settings.video_ports.clone();
            settings.video_ports.retain(|p| *p != port);
            settings.version += 1;
            let after = settings.video_ports.clone();
            self.commit_as(actor(), Change::SettingsChanged { settings });
            self.listen_video_ports(&before, &after);
            if self.state.video_sources.contains_key(&waiting_id(port)) {
                self.commit_as(
                    actor(),
                    Change::VideoSourceRemoved {
                        id: waiting_id(port),
                    },
                );
            }
        }
        if let Some(id) = &params.source_id {
            let source = self
                .state
                .video_sources
                .get(id)
                .ok_or_else(|| not_found(id))?;
            if !source.added_by_hand {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "UXDuck found this camera by itself, so it can't be removed. Only cameras you added can be.",
                ));
            }
            let tokens: Vec<_> = self
                .video
                .watches
                .iter()
                .filter(|(_, (_, source))| source == id)
                .map(|(token, _)| token.clone())
                .collect();
            for token in tokens {
                self.video.pending_watches.remove(&token);
                if let Some(port) = self.video.port.as_mut() {
                    port.revoke(&token);
                }
            }
            if let Some(port) = self.video.port.as_mut() {
                port.close_rtsp(id);
                port.close_capture(id);
                port.close_file(id);
                if let Some(recording) = self.video.recording.remove(id) {
                    port.stop_recording(&recording);
                }
            }
            self.video.capture_epochs.remove(id);
            self.video.capture_permission_issues.remove(id);
            self.video.file_epochs.remove(id);
            self.video.opened.remove(id);
            self.video.rtsp_urls.remove(id);
            self.video.rtsp_epochs.remove(id);
            self.video.needed.remove(id);
            if let Some(port) = self.video.port.as_mut() {
                port.demand(id, false);
            }
            self.video.watches.retain(|_, (_, source)| source != id);
            self.video
                .metadata_watches
                .retain(|_, (_, source)| source != id);
            self.video.remote_watches.remove(id);
            self.video.live.remove(id);
            self.video.by_hand.remove(id);
            // A camera still waiting was its port: nothing else is there.
            let waiting = is_waiting(source).then_some(source.port);
            self.commit_as(actor(), Change::VideoSourceRemoved { id: id.clone() });
            if let Some(port) = waiting
                && self.state.settings.video_ports.contains(&port)
                && !self.state.video_sources.values().any(|s| s.port == port)
            {
                let mut settings = self.state.settings.clone();
                let before = settings.video_ports.clone();
                settings.video_ports.retain(|p| *p != port);
                settings.version += 1;
                let after = settings.video_ports.clone();
                self.commit_as(actor(), Change::SettingsChanged { settings });
                self.listen_video_ports(&before, &after);
            }
        }
        Ok(())
    }

    fn video_watch(
        &mut self,
        connection: ConnectionId,
        params: &VideoWatchParams,
    ) -> Result<VideoWatchResult, ApiError> {
        if !self.state.video_sources.contains_key(&params.source_id) {
            return Err(not_found(&params.source_id));
        }
        let Some(port) = self.video.port.as_mut() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video needs UXDuck Desktop",
            ));
        };
        let token = format!("vw{}", &self.random.token()[..32]);
        port.allow(&token, &params.source_id);
        self.video
            .watches
            .insert(token.clone(), (connection, params.source_id.clone()));
        self.video
            .pending_watches
            .insert(token.clone(), self.clock.now_us() + 30_000_000);
        self.listen_for_watch(&params.source_id);
        self.review_recordings();
        Ok(VideoWatchResult {
            path: format!("/video?token={token}"),
            token,
        })
    }

    /// Remote publishers are consumers too, even with every local window closed.
    pub fn video_remote_wanted(&mut self, cameras: Vec<String>) {
        self.video.remote_watches = cameras.into_iter().collect();
        self.review_recordings();
    }

    /// A media socket has consumed its short-lived opening token.
    pub fn video_socket_opened(&mut self, token: &str) {
        self.video.pending_watches.remove(token);
    }

    /// Unused media tokens must not keep cameras running indefinitely.
    pub fn expire_video_watches(&mut self, now: u64) {
        let expired: Vec<_> = self
            .video
            .pending_watches
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            self.video_socket_closed(&token);
            if let Some(port) = self.video.port.as_mut() {
                port.revoke(&token);
            }
        }
    }

    #[allow(clippy::too_many_lines, reason = "one pass over every source's demand")]
    pub(crate) fn review_video_demand(&mut self) {
        if self.video.port.is_none() {
            return;
        }
        let watched: std::collections::BTreeSet<_> = self
            .video
            .watches
            .values()
            .chain(self.video.metadata_watches.values())
            .map(|(_, id)| id.clone())
            .chain(self.video.remote_watches.iter().cloned())
            .chain(self.video.looks.values().map(|look| look.source_id.clone()))
            .collect();
        let sources: Vec<_> = self.state.video_sources.values().cloned().collect();
        let mut opened = Vec::new();
        for mut source in sources {
            let recording = self.recording_intent(&source).is_some()
                && source.capture.as_ref().is_none_or(|c| c.enabled);
            // Nobody watching (after a restart, say): nothing re-encodes it.
            let stale_reencoding = source.reencoding.is_some() && !watched.contains(&source.id);
            if source.recording_requested != recording || stale_reencoding {
                source.recording_requested = recording;
                if stale_reencoding {
                    source.reencoding = None;
                }
                source.version += 1;
                self.commit_as(
                    actor(),
                    Change::VideoSourceUpserted {
                        source: source.clone(),
                    },
                );
            }
            let (urls, restricted) = self.camera_internet_routes(&source);
            if source.url.is_some()
                && self.video.opened.contains(&source.id)
                && self.video.rtsp_urls.get(&source.id) != Some(&urls)
            {
                self.video.opened.remove(&source.id);
                self.video.rtsp_urls.remove(&source.id);
                self.video.rtsp_epochs.remove(&source.id);
                if let Some(port) = self.video.port.as_mut() {
                    port.close_rtsp(&source.id);
                    if let Some(recording) = self.video.recording.remove(&source.id) {
                        port.stop_recording(&recording);
                    }
                }
                source.state = VideoState::Stopped;
                source.active_url = None;
                source.active_choice = None;
                source.counters.stop();
                source.state_detail = Some("Camera connection policy changed".into());
                source.version += 1;
                self.commit_as(
                    actor(),
                    Change::VideoSourceUpserted {
                        source: source.clone(),
                    },
                );
            }
            let wanted = (watched.contains(&source.id) || recording)
                && source.capture.as_ref().is_none_or(|c| c.enabled)
                && (source.url.is_none() || !urls.is_empty());
            let was_needed = self.video.needed.contains(&source.id);
            if wanted != was_needed {
                if wanted {
                    self.video.needed.insert(source.id.clone());
                } else {
                    self.video.needed.remove(&source.id);
                }
                if let Some(port) = self.video.port.as_mut() {
                    port.demand(&source.id, wanted);
                }
            }
            if wanted {
                if !was_needed && source.state == VideoState::Stopped {
                    source.state_detail = Some(if let Some(url) = &source.url {
                        self.connecting(&source, url)
                    } else if source.file.is_some() {
                        "Starting recorded video".into()
                    } else if source.capture.is_some() {
                        "Starting camera capture".into()
                    } else {
                        "Waiting for video".into()
                    });
                    source.version += 1;
                    self.commit_as(
                        actor(),
                        Change::VideoSourceUpserted {
                            source: source.clone(),
                        },
                    );
                }
                if source.url.is_some() {
                    if self.video.opened.insert(source.id.clone())
                        && let Some(port) = self.video.port.as_mut()
                    {
                        self.video.next_rtsp_epoch += 1;
                        let epoch = self.video.next_rtsp_epoch;
                        self.video.rtsp_epochs.insert(source.id.clone(), epoch);
                        self.video.rtsp_urls.insert(source.id.clone(), urls.clone());
                        self.video.rtsp_routes.remove(&source.id);
                        self.video.rtsp_may_return.remove(&source.id);
                        port.open_rtsp(&source.id, &urls, epoch);
                        opened.push(source.id.clone());
                    }
                } else if let Some(input) = source.file.clone() {
                    if !self.video.file_epochs.contains_key(&source.id) {
                        self.video
                            .file_epochs
                            .insert(source.id.clone(), source.version);
                        if let Some(port) = self.video.port.as_mut() {
                            port.open_file(&source.id, &input, source.version);
                        }
                    }
                } else if let Some(input) = source.capture.clone() {
                    if !self.video.capture_epochs.contains_key(&source.id) {
                        // Whether a local camera sends sound is known again
                        // with each start: its own microphone may be gone or
                        // refused, or sound turned off (S161).
                        if source.audio.is_some() {
                            source.audio = None;
                            source.version += 1;
                            self.commit_as(
                                actor(),
                                Change::VideoSourceUpserted {
                                    source: source.clone(),
                                },
                            );
                        }
                        // Idle/enable/configuration transitions advance the source version.
                        self.video
                            .capture_epochs
                            .insert(source.id.clone(), source.version);
                        if let Some(port) = self.video.port.as_mut() {
                            port.open_capture(&source.id, &input, source.version);
                        }
                    }
                } else {
                    self.listen_for_watch(&source.id);
                }
            } else {
                self.video.rtsp_urls.remove(&source.id);
                self.video.rtsp_epochs.remove(&source.id);
                if self.video.opened.remove(&source.id)
                    && let Some(port) = self.video.port.as_mut()
                {
                    port.close_rtsp(&source.id);
                }
                if self.video.capture_epochs.remove(&source.id).is_some()
                    && let Some(port) = self.video.port.as_mut()
                {
                    port.close_capture(&source.id);
                }
                if self.video.file_epochs.remove(&source.id).is_some()
                    && let Some(port) = self.video.port.as_mut()
                {
                    port.close_file(&source.id);
                }
                let detail = if (watched.contains(&source.id) || recording) && restricted.is_some()
                {
                    restricted.unwrap_or_default()
                } else if source.capture.as_ref().is_some_and(|c| !c.enabled) {
                    "Camera capture paused".to_owned()
                } else if self.video.capture_permission_issues.contains(&source.id) {
                    source
                        .state_detail
                        .clone()
                        .unwrap_or_else(|| "Camera idle".into())
                } else {
                    "Camera idle".to_owned()
                };
                if source.state != VideoState::Stopped
                    || source.active_url.is_some()
                    || source.state_detail.as_deref() != Some(detail.as_str())
                {
                    if source.state != VideoState::Stopped {
                        source.changed_at_us = self.clock.now_us();
                    }
                    source.state = VideoState::Stopped;
                    source.active_url = None;
                    source.active_choice = None;
                    source.state_detail = Some(detail);
                    source.counters.stop();
                    source.version += 1;
                    self.commit_as(actor(), Change::VideoSourceUpserted { source });
                }
            }
        }
        // A vehicle already on another address's connection: start there.
        for id in opened {
            self.route_camera(&id);
            self.steer_camera(&id);
        }
        self.release_watched_ports();
    }

    fn video_metadata_watch(
        &mut self,
        connection: ConnectionId,
        params: &VideoWatchParams,
    ) -> Result<VideoMetadataWatchResult, ApiError> {
        if !self.state.video_sources.contains_key(&params.source_id) {
            return Err(not_found(&params.source_id));
        }
        if self.video.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video needs UXDuck Desktop",
            ));
        }
        let token = format!("vm{}", &self.random.token()[..32]);
        self.video
            .metadata_watches
            .insert(token.clone(), (connection, params.source_id.clone()));
        self.listen_for_watch(&params.source_id);
        self.review_recordings();
        Ok(VideoMetadataWatchResult { token })
    }

    fn video_unwatch(&mut self, connection: ConnectionId, token: &str) {
        let owned = self
            .video
            .watches
            .get(token)
            .or_else(|| self.video.metadata_watches.get(token))
            .is_some_and(|(c, _)| *c == connection);
        if owned {
            self.video.watches.remove(token);
            self.video.metadata_watches.remove(token);
            self.video.pending_watches.remove(token);
            if let Some(port) = self.video.port.as_mut() {
                port.revoke(token);
            }
            self.release_watched_ports();
            self.review_recordings();
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one arm per video method")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::VideoCaptureSettings(params) => {
            require(Capability::Config)?;
            engine
                .video
                .port
                .as_mut()
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::InvalidRequest,
                        "Camera privacy settings need UXDuck on this computer.",
                    )
                })?
                .capture_settings(params.microphone)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&uxduck_contracts::dto::Empty {})
        }
        ClientRequest::VideoFile(params) => {
            require(Capability::Config)?;
            let source = engine.video_file(params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoDemos(params) => {
            require(Capability::Config)?;
            if params.action == uxduck_contracts::dto::video::DemoAction::Remove
                && engine
                    .state
                    .video_sources
                    .values()
                    .any(|s| s.file.as_ref().is_some_and(|f| f.demo_id == params.id))
            {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "This demo is used by a camera. Replace its footage or remove the camera first.",
                ));
            }
            let result = engine
                .video
                .port
                .as_mut()
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::InvalidValue,
                        "Demo footage needs UXDuck Desktop.",
                    )
                })?
                .demos(&params)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
            to_value(&result)
        }
        ClientRequest::VideoDevices(_) => {
            require(Capability::Config)?;
            to_value(&engine.video_devices())
        }
        ClientRequest::VideoCapture(params) => {
            require(Capability::Config)?;
            to_value(&VideoSourceDto::from(&engine.video_capture(params)?))
        }
        ClientRequest::VideoCaptureEnable(params) => {
            require(Capability::Config)?;
            to_value(&VideoSourceDto::from(
                &engine.video_capture_enable(&params)?,
            ))
        }
        ClientRequest::VideoList(Empty {}) => {
            require(Capability::Read)?;
            to_value(&VideoListResult {
                sources: engine.video_sources(),
                port: engine.video.listening,
                listen_error: engine.video.listen_error.clone(),
                sockets: engine
                    .video
                    .port
                    .as_ref()
                    .map(|p| p.sockets())
                    .unwrap_or_default(),
                extra_ports: engine
                    .state
                    .settings
                    .video_ports
                    .iter()
                    .map(|p| VideoPortStatus {
                        port: *p,
                        error: engine.video.extra_errors.get(p).cloned(),
                    })
                    .collect(),
            })
        }
        ClientRequest::VideoAssign(params) => {
            require(Capability::Config)?;
            let source = engine.video_assign(params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoMetadataWatch(params) => {
            require(Capability::Read)?;
            to_value(&engine.video_metadata_watch(connection, &params)?)
        }
        ClientRequest::VideoWatch(params) => {
            require(Capability::Read)?;
            to_value(&engine.video_watch(connection, &params)?)
        }
        ClientRequest::VideoUnwatch(VideoUnwatchParams { token }) => {
            require(Capability::Read)?;
            engine.video_unwatch(connection, &token);
            to_value(&Empty {})
        }
        ClientRequest::VideoAdd(params) => {
            require(Capability::Config)?;
            to_value(&engine.video_add(&params)?)
        }
        ClientRequest::VideoRemove(params) => {
            require(Capability::Config)?;
            engine.video_remove(&params)?;
            to_value(&Empty {})
        }
        ClientRequest::VideoRecord(params) => {
            require(Capability::Config)?;
            crate::recordings::record(engine, &params)
        }
        ClientRequest::VideoRecordAll(params) => {
            require(Capability::Config)?;
            crate::recordings::record_all(engine, &params)
        }
        ClientRequest::VideoRecordWithOthers(params) => {
            require(Capability::Config)?;
            let source = crate::recordings::record_with_others(engine, &params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoPlayback(params) => {
            require(Capability::Read)?;
            crate::recordings::playback(engine, &params)
        }
        ClientRequest::VideoRename(params) => {
            require(Capability::Config)?;
            let source = engine.video_rename(&params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoInternet(params) => {
            require(Capability::Config)?;
            let source = engine.video_internet(&params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoAddresses(params) => {
            require(Capability::Config)?;
            let source = engine.video_addresses(&params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoTrack(params) => {
            require(Capability::Config)?;
            let source = engine.video_track(&params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoReveal(params) => {
            require(Capability::Read)?;
            to_value(&engine.video_reveal(&params)?)
        }
        ClientRequest::VideoPhoto(params) => {
            require(Capability::Config)?;
            crate::photos::photo(engine, &params)
        }
        ClientRequest::VideoFrame(params) => {
            require(Capability::Read)?;
            engine.video_frame(connection, &params)
        }
        ClientRequest::VideoPhotoShow(params) => {
            require(Capability::Read)?;
            crate::photos::show(engine, &params)
        }
        ClientRequest::VideoVehicleCamera(params) => {
            require(Capability::Config)?;
            let source = engine.video_vehicle_camera(&params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        ClientRequest::VideoServedBy(params) => {
            require(Capability::Config)?;
            let source = engine.video_served_by(params)?;
            to_value(&VideoSourceDto::from(&source))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a video method")),
    }
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::VideoSourceUpserted { source } => vec![EntityChange {
            video_source: Some(source.into()),
            ..upsert(EntityType::VideoSource, &source.id)
        }],
        Change::VideoSourceRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::VideoSource, id)
        }],
        _ => Vec::new(),
    }
}

impl Engine {
    pub(crate) fn vehicle_video_wanted(&self, vehicle_id: &str) -> bool {
        self.state
            .video_sources
            .values()
            .filter(|s| s.vehicle_id.as_deref() == Some(vehicle_id))
            .any(|s| {
                self.recording_intent(s).is_some()
                    || self.video.remote_watches.contains(&s.id)
                    || self
                        .video
                        .watches
                        .values()
                        .chain(self.video.metadata_watches.values())
                        .any(|(_, id)| id == &s.id)
            })
    }

    fn camera_internet_routes(&self, source: &VideoSource) -> (Vec<String>, Option<String>) {
        use uxduck_core::domain::companion::{CellularVideoPolicy, CheckState, InternetTransport};
        let urls: Vec<_> = source
            .url
            .iter()
            .chain(&source.fallback_urls)
            .cloned()
            .collect();
        let gateway = source
            .vehicle_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .and_then(|v| v.companions.iter().find(|c| c.connectivity.is_some()));
        let Some(gateway) = gateway else {
            return (urls, None);
        };
        let policy = gateway
            .connectivity
            .as_ref()
            .map_or(CellularVideoPolicy::Allow, |c| c.video);
        if policy == CellularVideoPolicy::Allow {
            return (urls, None);
        }
        let now = self.clock.now_us();
        let transport = gateway
            .check
            .as_ref()
            .filter(|c| c.state != CheckState::Failed)
            .and_then(|c| c.network.as_ref())
            .filter(|n| {
                n.sampled_at_us > 0
                    && n.sampled_at_us >= self.started_us
                    && now
                        .checked_sub(n.sampled_at_us)
                        .is_some_and(|age| age < 15_000_000)
            })
            .map_or(
                InternetTransport::Unknown,
                uxduck_core::domain::companion::CompanionNetwork::transport,
            );
        if transport == InternetTransport::Wifi {
            return (urls, None);
        }
        let mut local: Vec<_> = urls
            .into_iter()
            .filter(|url| {
                source
                    .internet
                    .as_ref()
                    .is_some_and(|i| i.local_urls.contains(url))
            })
            .collect();
        if policy == CellularVideoPolicy::LowData
            && transport == InternetTransport::Cellular
            && let Some(low) = source
                .internet
                .as_ref()
                .and_then(|i| i.low_data_url.as_ref())
        {
            local.push(low.clone());
        }
        let detail = if transport == InternetTransport::Unknown {
            "Remote video paused until the gateway's internet connection is known"
        } else if policy == CellularVideoPolicy::LowData {
            "Remote video paused: configure a low-data camera address for cellular"
        } else {
            "Remote video paused: telemetry only on cellular"
        };
        (local, Some(detail.into()))
    }

    fn video_internet(
        &mut self,
        params: &uxduck_contracts::dto::video::VideoInternetParams,
    ) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| not_found(&params.source_id))?;
        if source.url.is_none() || !source.added_by_hand {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Internet video controls require an RTSP camera you added.",
            ));
        }
        let urls: Vec<_> = source.url.iter().chain(&source.fallback_urls).collect();
        let mut local_urls: Vec<String> = Vec::new();
        for local in &params.internet.local_urls {
            let Some(url) = urls.iter().find(|u| redact(u) == redact(local.trim())) else {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Choose direct local addresses from this camera's saved addresses.",
                ));
            };
            if !local_urls.contains(*url) {
                local_urls.push((*url).clone());
            }
        }
        let low_data_url = params
            .internet
            .low_data_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(|given| {
                source
                    .internet
                    .as_ref()
                    .and_then(|i| i.low_data_url.as_ref())
                    .filter(|old| redact(old) == given.trim())
                    .map_or_else(|| given.trim().to_owned(), Clone::clone)
            });
        if let Some(low) = &low_data_url {
            if !matches!(input(low)?, Input::Rtsp { .. }) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Use an RTSP address for the camera's low-data stream.",
                ));
            }
            if self.state.video_sources.values().any(|s| {
                s.url
                    .iter()
                    .chain(&s.fallback_urls)
                    .chain(
                        s.internet
                            .iter()
                            .filter_map(|i| i.low_data_url.as_ref())
                            .filter(|_| s.id != source.id),
                    )
                    .any(|u| redact(u) == redact(low))
            }) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "The low-data stream needs its own address, separate from existing camera addresses.",
                ));
            }
        }
        source.internet = Some(uxduck_core::domain::video::CameraInternet {
            local_urls,
            low_data_url,
        });
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(self
            .state
            .video_sources
            .get(&source.id)
            .cloned()
            .unwrap_or(source))
    }
}

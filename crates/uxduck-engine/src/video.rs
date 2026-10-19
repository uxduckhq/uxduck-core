//! Camera streams (ADR-0017 V4, V5, V6, V14, S38): the `videoSource`
//! entities, which vehicle each belongs to, the "Video from …" notification,
//! and who may open the `/video` socket for which source. The listener
//! reports what arrives; the pictures never pass through the engine.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{
    VideoAddParams, VideoAddResult, VideoAssignParams, VideoCaptureEnableParams,
    VideoCaptureParams, VideoDevicesResult, VideoListResult, VideoMetadataWatchResult,
    VideoPortStatus, VideoRemoveParams, VideoRenameParams, VideoRevealParams, VideoRevealResult,
    VideoSocket, VideoSource as VideoSourceDto, VideoTrackParams, VideoUnwatchParams,
    VideoWatchParams, VideoWatchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::video::{
    CaptureInput, VideoReport, VideoSource, VideoState, VideoTrack, redact, sender_host, udp_name,
};
use uxduck_core::events::{Actor, Change};

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

/// Where watches go: the daemon's `/video` hub.
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
    fn open_rtsp(&mut self, _source_id: &str, _url: &str) {}
    fn close_rtsp(&mut self, _source_id: &str) {}
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
    /// Cuts a recording interrupted by a crash, its file named, back to
    /// its last whole fragment: its size and length, or `None` if nothing
    /// is left.
    fn recover_recording(&mut self, _file: &str) -> Option<(u64, u64)> {
        None
    }
    /// `/recordings/<id>.mp4?token=` answers with the recording's file for
    /// an hour.
    fn allow_playback(&mut self, _token: &str, _recording_id: &str, _file: &str) {}
    /// Shows the media folder in the file manager, with a recording's file
    /// selected if one is named: the folder.
    ///
    /// # Errors
    /// Why it cannot, such as no file manager to open.
    fn reveal(&mut self, _file: Option<&str>) -> Result<String, String> {
        Err("Recordings need UXDuck Desktop.".into())
    }
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

/// The most UDP ports added by hand.
pub const MAX_VIDEO_PORTS: usize = 8;

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
    Rtsp { host: String, port: u16 },
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
    let Some(rest) = url.strip_prefix("rtsp://") else {
        return Err(invalid(
            "add an RTSP address (rtsp://…) or a UDP port (udp:5601)",
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
    /// The recording in progress for each camera (S40).
    pub(crate) recording: BTreeMap<String, String>,
    /// Cameras whose vehicle's own camera was told to record (S145): its
    /// vehicle and the receipt.
    pub(crate) camera_recording: BTreeMap<String, (String, String)>,
    /// Each configured capture run, independent of updates to its source counters.
    capture_epochs: BTreeMap<String, u64>,
    /// Session-local recording intent, independent of signal loss.
    pub(crate) by_hand: BTreeMap<String, bool>,
    /// When each recording's progress was last journaled.
    pub(crate) progress_journaled: BTreeMap<String, u64>,
    /// Recordings whose files were deleted outside UXDuck this session
    /// (S151), for telling what of them never reached the account.
    pub(crate) forgotten: BTreeMap<String, uxduck_core::domain::video::VideoRecording>,
    /// The UDP port the listener holds, or why it holds none.
    listening: Option<u16>,
    listen_error: Option<String>,
}

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
    }
}

/// What a source is called until someone renames it: where it comes from.
fn default_name(source: &VideoSource) -> String {
    if let Some(input) = &source.capture {
        return input.name.clone();
    }
    source
        .url
        .as_deref()
        .map_or_else(|| udp_name(&source.id, source.port, &source.sender), redact)
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

fn validate_capture(input: &CaptureInput) -> Result<(), ApiError> {
    let invalid = |why: &str| ApiError::new(ErrorKind::InvalidValue, why);
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
            source.counters.bitrate_bps = 0;
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
        for p in before.iter().filter(|p| !after.contains(p)) {
            port.unlisten(*p);
            self.video.extra_errors.remove(p);
        }
        for p in after.iter().filter(|p| !before.contains(p)) {
            match port.listen(*p) {
                Ok(()) => {
                    self.video.extra_errors.remove(p);
                }
                Err(error) => {
                    self.video.extra_errors.insert(*p, error);
                }
            }
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
                        && s.vehicle_id.is_none()
                })
                .cloned()
                .collect();
            for mut source in unclaimed {
                source.vehicle_id = Some(vehicle_id.to_owned());
                // The vehicle's own camera named this port (V20).
                source.vehicle_camera |= !source.vehicle_camera_by_hand;
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
                // The vehicle's own camera announced it: it can be told to shoot (V20).
                vehicle_camera: true,
                vehicle_camera_by_hand: false,
                record_alone: false,
                klv: false,
                url: Some(uri.to_owned()),
                added_by_hand: false,
                capture: None,
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
        let mut source = before.clone().unwrap_or_else(|| VideoSource {
            recording_requested: false,
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
            record_alone: false,
            klv: false,
            url: None,
            added_by_hand: false,
            capture: None,
            tracks: Vec::new(),
            audio: None,
            counters: report.counters.clone(),
            first_seen_us: now,
            changed_at_us: now,
            version: 0,
        });
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
        if !source.assigned_by_hand && source.url.is_none() && source.capture.is_none() {
            // The address, then a camera's announcement, then what its KLV
            // named (kept from before).
            let announced = self.video.announced_ports.get(&report.port).cloned();
            source.vehicle_id = self
                .vehicle_by_address(&report.sender)
                .or_else(|| announced.clone())
                .or_else(|| before.as_ref().and_then(|b| b.vehicle_id.clone()));
            // On a port the vehicle's own camera named, the stream is that
            // camera's (V20).
            if !source.vehicle_camera_by_hand && announced.is_some() {
                source.vehicle_camera = source.vehicle_id == announced;
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
        source.counters.bitrate_bps = 0;
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
        if source.capture.is_some() || source.url.is_some() || self.listened(source.port) {
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
        source.vehicle_id = match params.vehicle_id {
            Some(id) => Some(id),
            None => self.vehicle_by_address(&source.sender),
        };
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

    /// Plays another of an RTSP camera's tracks (S90): the choice goes on
    /// its address as `#track=N`, which never goes on the wire, and the
    /// camera is opened again.
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
            .filter(|_| source.added_by_hand && source.capture.is_none())
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
        let url = with_track(&url, params.track);
        if source.url.as_deref() == Some(url.as_str()) {
            return Ok(source);
        }
        if !source.named_by_hand {
            source.name = redact(&url);
        }
        source.url = Some(url.clone());
        for track in &mut source.tracks {
            track.playing = track.number == params.track;
        }
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        if self.video.opened.remove(&source.id)
            && let Some(port) = self.video.port.as_mut()
        {
            port.close_rtsp(&source.id);
        }
        self.review_recordings();
        Ok(source)
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
        if name.chars().count() > 80 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "a camera's name is at most 80 characters",
            ));
        }
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
            Some(id) => Some(
                self.state
                    .video_recordings
                    .get(id)
                    .ok_or_else(|| {
                        ApiError::new(ErrorKind::NotFound, format!("no recording {id}"))
                    })?
                    .file(),
            ),
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
        {
            return Ok(same.clone());
        }
        if previous.is_none()
            && self
                .state
                .video_sources
                .values()
                .filter(|s| s.capture.is_some())
                .count()
                >= 8
        {
            return Err(invalid(
                "UXDuck supports at most eight configured local cameras.",
            ));
        }
        let now = self.clock.now_us();
        let mut source = previous.unwrap_or_else(|| VideoSource {
            recording_requested: false,
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
            record_alone: false,
            klv: false,
            url: None,
            added_by_hand: true,
            capture: None,
            tracks: Vec::new(),
            audio: None,
            counters: uxduck_core::domain::video::VideoCounters::default(),
            first_seen_us: now,
            changed_at_us: now,
            version: 0,
        });
        if self.video.capture_epochs.remove(&source.id).is_some()
            && let Some(port) = self.video.port.as_mut()
        {
            port.close_capture(&source.id);
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
        if input.enabled == params.enabled {
            return Ok(source);
        }
        input.enabled = params.enabled;
        let input = input.clone();
        if self.video.capture_epochs.remove(&source.id).is_some()
            && let Some(port) = self.video.port.as_mut()
        {
            port.close_capture(&source.id);
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

    fn video_add(&mut self, url: &str) -> Result<VideoAddResult, ApiError> {
        match input(url)? {
            Input::Udp(port) => {
                let mut settings = self.state.settings.clone();
                if !settings.video_ports.contains(&port) {
                    if settings.video_ports.len() >= MAX_VIDEO_PORTS {
                        return Err(ApiError::new(
                            ErrorKind::InvalidValue,
                            format!(
                                "UXDuck listens on at most {MAX_VIDEO_PORTS} ports added by hand"
                            ),
                        ));
                    }
                    if self.video.listening == Some(port) {
                        return Err(ApiError::new(
                            ErrorKind::InvalidValue,
                            format!("UXDuck already listens on UDP {port}"),
                        ));
                    }
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
                Ok(VideoAddResult {
                    source: None,
                    port: Some(port),
                })
            }
            Input::Rtsp { host, port } => {
                let url = url.trim().to_owned();
                if let Some(same) = self
                    .state
                    .video_sources
                    .values()
                    .find(|s| s.url.as_deref() == Some(url.as_str()))
                {
                    return Ok(VideoAddResult {
                        source: Some(same.into()),
                        port: None,
                    });
                }
                let now = self.clock.now_us();
                let source = VideoSource {
                    recording_requested: false,
                    id: format!("vs{}", &self.random.token()[..16]),
                    name: redact(&url),
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
                        uxduck_core::domain::video::redact(&url)
                    )),
                    vehicle_id: None,
                    assigned_by_hand: false,
                    vehicle_camera: false,
                    vehicle_camera_by_hand: false,
                    record_alone: false,
                    klv: false,
                    url: Some(url.clone()),
                    added_by_hand: true,
                    capture: None,
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
                if let Some(recording) = self.video.recording.remove(id) {
                    port.stop_recording(&recording);
                }
            }
            self.video.capture_epochs.remove(id);
            self.video.opened.remove(id);
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
            self.commit_as(actor(), Change::VideoSourceRemoved { id: id.clone() });
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
            .collect();
        let sources: Vec<_> = self.state.video_sources.values().cloned().collect();
        for mut source in sources {
            let recording = self.recording_intent(&source).is_some()
                && source.capture.as_ref().is_none_or(|c| c.enabled);
            if source.recording_requested != recording {
                source.recording_requested = recording;
                source.version += 1;
                self.commit_as(
                    actor(),
                    Change::VideoSourceUpserted {
                        source: source.clone(),
                    },
                );
            }
            let wanted = (watched.contains(&source.id) || recording)
                && source.capture.as_ref().is_none_or(|c| c.enabled);
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
                        format!("Connecting to {}", redact(url))
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
                if let Some(url) = &source.url {
                    if self.video.opened.insert(source.id.clone())
                        && let Some(port) = self.video.port.as_mut()
                    {
                        port.open_rtsp(&source.id, url);
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
                let detail = if source.capture.as_ref().is_some_and(|c| !c.enabled) {
                    "Camera capture paused"
                } else {
                    "Camera idle"
                };
                if source.state != VideoState::Stopped
                    || source.state_detail.as_deref() != Some(detail)
                {
                    if source.state != VideoState::Stopped {
                        source.changed_at_us = self.clock.now_us();
                    }
                    source.state = VideoState::Stopped;
                    source.state_detail = Some(detail.into());
                    source.counters.bitrate_bps = 0;
                    source.version += 1;
                    self.commit_as(actor(), Change::VideoSourceUpserted { source });
                }
            }
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
        ClientRequest::VideoAdd(VideoAddParams { url }) => {
            require(Capability::Config)?;
            to_value(&engine.video_add(&url)?)
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
        ClientRequest::VideoPhotoShow(params) => {
            require(Capability::Read)?;
            crate::photos::show(engine, &params)
        }
        ClientRequest::VideoVehicleCamera(params) => {
            require(Capability::Config)?;
            let source = engine.video_vehicle_camera(&params)?;
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

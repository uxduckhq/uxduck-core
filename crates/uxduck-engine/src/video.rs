//! Camera streams (ADR-0017 V4, V5, V6, V14, S38): the `videoSource`
//! entities, which vehicle each belongs to, the "Video from …" notification,
//! and who may open the `/video` socket for which source. The listener
//! reports what arrives; the pictures never pass through the engine.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{
    VideoAddParams, VideoAddResult, VideoAssignParams, VideoListResult, VideoPortStatus,
    VideoRemoveParams, VideoRenameParams, VideoRevealParams, VideoRevealResult, VideoSocket,
    VideoSource as VideoSourceDto, VideoTrackParams, VideoUnwatchParams, VideoWatchParams,
    VideoWatchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};
use uxduck_core::domain::video::{
    VideoReport, VideoSource, VideoState, VideoTrack, redact, sender_host, udp_name,
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
    /// Starts writing a camera's pictures to a recording (S40).
    fn start_recording(&mut self, _recording_id: &str, _source_id: &str) {}
    /// Finishes a recording; the daemon reports its size when it has.
    fn stop_recording(&mut self, _recording_id: &str) {}
    /// Cuts a recording interrupted by a crash back to its last whole
    /// fragment: its size and length, or `None` if nothing is left.
    fn recover_recording(&mut self, _recording_id: &str) -> Option<(u64, u64)> {
        None
    }
    /// `/recordings/<id>.mp4?token=` may be fetched for an hour.
    fn allow_playback(&mut self, _token: &str, _recording_id: &str) {}
    /// Shows the recordings folder in the file manager, with a recording
    /// selected if one is named: the folder.
    ///
    /// # Errors
    /// Why it cannot, such as no file manager to open.
    fn reveal(&mut self, _recording_id: Option<&str>) -> Result<String, String> {
        Err("Recordings need UXDuck Desktop.".into())
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
    /// UDP ports added by hand that could not be listened on, and why.
    extra_errors: BTreeMap<u16, String>,
    /// UDP ports a vehicle's camera named, and that vehicle (S39).
    announced_ports: BTreeMap<u16, String>,
    /// Of those, the ports listened on only because a camera named them.
    announced_listens: std::collections::BTreeSet<u16>,
    /// Announced RTSP sources fetched this session.
    opened: std::collections::BTreeSet<String>,
    /// The recording in progress for each camera (S40).
    pub(crate) recording: BTreeMap<String, String>,
    /// Record or not by hand, for each camera, until it stops.
    pub(crate) by_hand: BTreeMap<String, bool>,
    /// When each recording's progress was last journaled.
    pub(crate) progress_journaled: BTreeMap<String, u64>,
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

impl Engine {
    /// The composition root's `/video` hub. Sources that were receiving
    /// when the daemon last stopped are stopped now.
    pub fn set_video(&mut self, port: Box<dyn VideoPort>) {
        self.video.port = Some(port);
        let ports = self.state.settings.video_ports.clone();
        self.listen_video_ports(&[], &ports);
        let fetched: Vec<(String, String)> = self
            .state
            .video_sources
            .values()
            .filter(|s| s.added_by_hand)
            .filter_map(|s| s.url.clone().map(|url| (s.id.clone(), url)))
            .collect();
        if let Some(port) = self.video.port.as_mut() {
            for (id, url) in &fetched {
                port.open_rtsp(id, url);
            }
        }
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
            let listened = self.video.listening == Some(port)
                || self.state.settings.video_ports.contains(&port)
                || self.video.announced_listens.contains(&port);
            if !listened
                && let Some(video) = self.video.port.as_mut()
                && video.listen(port).is_ok()
            {
                self.video.announced_listens.insert(port);
            }
            let unclaimed: Vec<VideoSource> = self
                .state
                .video_sources
                .values()
                .filter(|s| s.port == port && s.url.is_none() && s.vehicle_id.is_none())
                .cloned()
                .collect();
            for mut source in unclaimed {
                source.vehicle_id = Some(vehicle_id.to_owned());
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
            let now = self.clock.now_us();
            let source = VideoSource {
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
                klv: false,
                url: Some(uri.to_owned()),
                added_by_hand: false,
                tracks: Vec::new(),
                counters: uxduck_core::domain::video::VideoCounters::default(),
                first_seen_us: now,
                // Long ago, so its first picture says "Video from …".
                changed_at_us: 0,
                version: current.map_or(1, |s| s.version + 1),
            };
            self.commit_as(actor(), Change::VideoSourceUpserted { source });
        }
        if let Some(video) = self.video.port.as_mut() {
            video.open_rtsp(&id, uri);
            self.video.opened.insert(id);
        }
    }

    /// What the listener heard from one sender in the last second or so.
    pub fn video_observed(&mut self, report: VideoReport) {
        let now = self.clock.now_us();
        let id = report.source_id();
        let before = self.state.video_sources.get(&id).cloned();
        let state = if report.unsupported.is_some() {
            VideoState::Unsupported
        } else {
            VideoState::Receiving
        };
        let mut source = before.clone().unwrap_or_else(|| VideoSource {
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
            klv: false,
            url: None,
            added_by_hand: false,
            tracks: Vec::new(),
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
        if source.url.is_some() && report.id.is_none() {
            // A sender on a port is never an added source.
            return;
        }
        source.klv |= report.klv;
        // A source with an address of its own (RTSP) keeps the vehicle it
        // was added or announced for.
        if !source.assigned_by_hand && source.url.is_none() {
            // The address, then a camera's announcement, then what its KLV
            // named (kept from before).
            source.vehicle_id = self
                .vehicle_by_address(&report.sender)
                .or_else(|| self.video.announced_ports.get(&report.port).cloned())
                .or_else(|| before.as_ref().and_then(|b| b.vehicle_id.clone()));
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
        // A camera that stops ends its recording and what was asked by hand.
        self.video.by_hand.remove(source_id);
        self.review_recordings();
    }

    /// A `/video` socket closed: its watch is over.
    pub fn video_socket_closed(&mut self, token: &str) {
        self.video.watches.remove(token);
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
        let Some(url) = source.url.clone().filter(|_| source.added_by_hand) else {
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
        if let Some(port) = self.video.port.as_mut() {
            port.open_rtsp(&source.id, &url);
        }
        Ok(source)
    }

    /// The tracks an RTSP camera offers, from its SDP, each time it answers.
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

    fn video_reveal(&mut self, params: &VideoRevealParams) -> Result<VideoRevealResult, ApiError> {
        if let Some(id) = &params.recording_id
            && !self.state.video_recordings.contains_key(id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no recording {id}"),
            ));
        }
        let port = self.video.port.as_mut().ok_or_else(|| {
            ApiError::new(ErrorKind::InvalidRequest, "Recordings need UXDuck Desktop.")
        })?;
        port.reveal(params.recording_id.as_deref())
            .map(|folder| VideoRevealResult { folder })
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))
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
                    klv: false,
                    url: Some(url.clone()),
                    added_by_hand: true,
                    tracks: Vec::new(),
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
                if let Some(port) = self.video.port.as_mut() {
                    port.open_rtsp(&source.id, &url);
                }
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
            if let Some(port) = self.video.port.as_mut() {
                port.close_rtsp(id);
            }
            self.video.live.remove(id);
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
        Ok(VideoWatchResult {
            path: format!("/video?token={token}"),
            token,
        })
    }

    fn video_unwatch(&mut self, connection: ConnectionId, token: &str) {
        if self
            .video
            .watches
            .get(token)
            .is_some_and(|(c, _)| *c == connection)
        {
            self.video.watches.remove(token);
            if let Some(port) = self.video.port.as_mut() {
                port.revoke(token);
            }
        }
    }
}

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

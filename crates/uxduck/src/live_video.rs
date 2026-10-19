//! Live video from anywhere (S84, ADR-0003 C11, C12, "Live path"): while
//! someone away watches one of this computer's cameras, its live rendition
//! goes to a cloud media node, and only then. The relay says which cameras
//! are watched (`EdgeDemand::video`); each one asks the media coordinator
//! for the nodes and a publish grant, and a publisher sends it to the
//! first node that takes it, failing over down the list without asking
//! again.
//!
//! The publisher is `ffmpeg` (`UXDUCK_LIVE_FFMPEG` names another): the
//! camera's pictures go into it as they arrive, from the hub like any
//! socket's, and come out as H.264 without B-frames, sized for the uplink,
//! over WHIP or SRT (C12), whichever the coordinator prefers and this
//! `ffmpeg` has. Supporting both is the choice that is easiest to change:
//! the prototype's answer is one var in the API (`MEDIA_CONTRIBUTION`).
//!
//! The live rendition yields to telemetry and commands on the uplink: the
//! relay's round trip is the same uplink they use, and when it grows, or
//! the publisher falls 200 ms behind, video steps down a rung: bitrate
//! first, then frame rate, then resolution. It climbs back slowly, and not
//! while commands from away are being sent.
//!
//! What is stored is never this: the camera's original is recorded here
//! without re-encoding (S40) and stored later (S83).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::time::Instant;
use uxduck_contracts::dto::edge::{
    Contribution, MediaPublishGrant, MediaPublishRequest, RemoteCamera,
};

use crate::daemon::engine_task::EngineHandle;
use crate::video::hub::Item;

/// Stop publishing and retrying as soon as the last remote viewer leaves.
pub const LINGER: Duration = Duration::ZERO;

/// Which cameras to publish, from what the relay says is watched.
#[derive(Debug)]
pub struct Demand {
    running: BTreeSet<String>,
    unwanted_since: HashMap<String, Instant>,
    linger: Duration,
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            running: BTreeSet::new(),
            unwanted_since: HashMap::new(),
            linger: LINGER,
        }
    }
}

impl Demand {
    /// What to start and what to stop now: a watched camera at once, one
    /// nobody watches after [`LINGER`].
    pub fn update(&mut self, wanted: &[String], now: Instant) -> (Vec<String>, Vec<String>) {
        let wanted: BTreeSet<&String> = wanted.iter().collect();
        let mut start = Vec::new();
        for camera in &wanted {
            self.unwanted_since.remove(*camera);
            if self.running.insert((*camera).clone()) {
                start.push((*camera).clone());
            }
        }
        let mut stop = Vec::new();
        for camera in &self.running {
            if wanted.contains(camera) {
                continue;
            }
            let since = *self.unwanted_since.entry(camera.clone()).or_insert(now);
            if now.duration_since(since) >= self.linger {
                stop.push(camera.clone());
            }
        }
        for camera in &stop {
            self.running.remove(camera);
            self.unwanted_since.remove(camera);
        }
        (start, stop)
    }

    #[cfg(test)]
    #[must_use]
    pub fn running(&self) -> &BTreeSet<String> {
        &self.running
    }
}

/// One step of the live rendition (ADR-0003 "Two renditions": 0.5 to 4
/// Mbps, keyframes every second or two, adapted to the uplink).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rung {
    pub kbps: u32,
    pub fps: u32,
    /// Lines; a picture already smaller is left as it is.
    pub height: u32,
}

/// Best first: bitrate goes down first, then frame rate, then resolution.
pub const LADDER: [Rung; 7] = [
    Rung {
        kbps: 2_500,
        fps: 30,
        height: 720,
    },
    Rung {
        kbps: 1_500,
        fps: 30,
        height: 720,
    },
    Rung {
        kbps: 900,
        fps: 30,
        height: 720,
    },
    Rung {
        kbps: 600,
        fps: 15,
        height: 720,
    },
    Rung {
        kbps: 400,
        fps: 15,
        height: 480,
    },
    Rung {
        kbps: 250,
        fps: 10,
        height: 360,
    },
    Rung {
        kbps: 120,
        fps: 5,
        height: 240,
    },
];

/// Where a stream starts: below the top, so a weak uplink is not flooded first.
const START_RUNG: usize = 1;
/// Steps down at most this often: the last step needs time to show.
const STEP_DOWN_EVERY: Duration = Duration::from_secs(2);
/// Climbs one rung only after this long with nothing wrong.
const HOLD_UP: Duration = Duration::from_secs(20);
/// Round trips the baseline is the least of.
const RTT_SAMPLES: usize = 30;
/// A round trip this much over the baseline, and at least twice it, is a
/// queue on the uplink.
const RTT_RISE_MS: u64 = 150;

/// The uplink as the live rendition sees it: shared by every camera, since
/// they share one uplink with telemetry and commands.
#[derive(Debug)]
pub struct Uplink {
    rung: usize,
    rtts: VecDeque<u64>,
    changed_at: Option<Instant>,
    clean_since: Option<Instant>,
}

impl Default for Uplink {
    fn default() -> Self {
        Self {
            rung: START_RUNG,
            rtts: VecDeque::new(),
            changed_at: None,
            clean_since: None,
        }
    }
}

impl Uplink {
    #[must_use]
    pub const fn rung(&self) -> usize {
        self.rung
    }

    /// A round trip to the relay. True when the rung changed.
    pub fn round_trip(&mut self, ms: u64, now: Instant) -> bool {
        let baseline = self.rtts.iter().copied().min();
        self.rtts.push_back(ms);
        if self.rtts.len() > RTT_SAMPLES {
            self.rtts.pop_front();
        }
        match baseline {
            Some(base) if ms > base + RTT_RISE_MS && ms > base * 2 => self.congested(now),
            _ => self.clean(now),
        }
    }

    /// The publisher fell 200 ms behind and pictures were dropped.
    pub fn fell_behind(&mut self, now: Instant) -> bool {
        self.congested(now)
    }

    /// A command from away went to a vehicle: video does not climb now.
    pub fn command(&mut self, now: Instant) {
        self.clean_since = Some(now);
    }

    fn congested(&mut self, now: Instant) -> bool {
        self.clean_since = None;
        let rested = self
            .changed_at
            .is_none_or(|at| now.duration_since(at) >= STEP_DOWN_EVERY);
        if self.rung + 1 < LADDER.len() && rested {
            self.rung += 1;
            self.changed_at = Some(now);
            return true;
        }
        false
    }

    fn clean(&mut self, now: Instant) -> bool {
        let since = *self.clean_since.get_or_insert(now);
        let held = |at: Instant| now.duration_since(at) >= HOLD_UP;
        if self.rung > 0 && held(since) && self.changed_at.is_none_or(held) {
            self.rung -= 1;
            self.changed_at = Some(now);
            self.clean_since = Some(now);
            return true;
        }
        false
    }
}

/// The camera's codec, as `ffmpeg` reads it from a pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    H264,
    H265,
}

impl Input {
    /// From an access unit's codec string, such as `avc1.64001f`.
    #[must_use]
    pub fn of(codec: &str) -> Option<Self> {
        if codec.starts_with("avc") {
            Some(Self::H264)
        } else if codec.starts_with("hvc") || codec.starts_with("hev") {
            Some(Self::H265)
        } else {
            None
        }
    }
}

/// Where the rendition goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub protocol: Contribution,
    pub url: String,
    pub grant: String,
}

/// SRT's window for retransmitting lost packets, in microseconds as
/// `ffmpeg` takes it: under the one-second target with room to spare.
const SRT_LATENCY_US: u32 = 200_000;

/// `ffmpeg`'s arguments: Annex B pictures on its stdin, timed by their
/// arrival; out as H.264 without B-frames, a keyframe every two seconds.
#[must_use]
pub fn ffmpeg_args(input: Input, rung: Rung, encoder: &str, target: &Target) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-fflags",
        "nobuffer",
        "-flags",
        "low_delay",
        "-probesize",
        "32768",
        "-analyzeduration",
        "0",
        "-use_wallclock_as_timestamps",
        "1",
        "-f",
        match input {
            Input::H264 => "h264",
            Input::H265 => "hevc",
        },
        "-i",
        "pipe:0",
        "-an",
        "-vf",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(format!(
        "fps={},scale=-2:'min({},ih)'",
        rung.fps, rung.height
    ));
    args.extend(["-c:v".into(), encoder.to_owned()]);
    if encoder == "libx264" {
        args.extend(
            [
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
            ]
            .map(str::to_owned),
        );
    }
    args.extend([
        "-bf".into(),
        "0".into(),
        "-g".into(),
        (rung.fps * 2).to_string(),
        "-b:v".into(),
        format!("{}k", rung.kbps),
        "-maxrate".into(),
        format!("{}k", rung.kbps),
        "-bufsize".into(),
        format!("{}k", rung.kbps / 2),
    ]);
    match target.protocol {
        Contribution::Whip => {
            args.extend([
                "-f".into(),
                "whip".into(),
                "-authorization".into(),
                target.grant.clone(),
                target.url.clone(),
            ]);
        }
        Contribution::Srt => {
            let joiner = if target.url.contains('?') { '&' } else { '?' };
            args.extend([
                "-f".into(),
                "mpegts".into(),
                format!("{}{joiner}latency={SRT_LATENCY_US}", target.url),
            ]);
        }
    }
    args
}

/// What this computer's `ffmpeg` can send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publisher {
    pub program: String,
    pub whip: bool,
    pub srt: bool,
}

/// Reads `ffmpeg -muxers` and `ffmpeg -protocols`: WHIP is a muxer
/// (`FFmpeg` 8), SRT a protocol (built with `libsrt`).
#[must_use]
pub fn capabilities(muxers: &str, protocols: &str) -> (bool, bool) {
    let whip = muxers
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some("whip"));
    let srt = protocols.lines().any(|l| l.trim() == "srt");
    (whip, srt)
}

/// The protocol to use: the coordinator's, else the other.
#[must_use]
pub fn choose(preferred: Contribution, publisher: &Publisher) -> Option<Contribution> {
    let has = |p: Contribution| match p {
        Contribution::Whip => publisher.whip,
        Contribution::Srt => publisher.srt,
    };
    let other = match preferred {
        Contribution::Whip => Contribution::Srt,
        Contribution::Srt => Contribution::Whip,
    };
    [preferred, other].into_iter().find(|p| has(*p))
}

/// This computer's publisher, looked for once.
async fn publisher() -> Option<Publisher> {
    static FOUND: tokio::sync::OnceCell<Option<Publisher>> = tokio::sync::OnceCell::const_new();
    FOUND
        .get_or_init(|| async {
            let program = std::env::var("UXDUCK_LIVE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
            let run = |what: &'static str| {
                let program = program.clone();
                async move {
                    let out = tokio::process::Command::new(&program)
                        .args(["-hide_banner", what])
                        .stdin(std::process::Stdio::null())
                        .output()
                        .await
                        .ok()?;
                    Some(String::from_utf8_lossy(&out.stdout).into_owned())
                }
            };
            let muxers = run("-muxers").await?;
            let protocols = run("-protocols").await.unwrap_or_default();
            let (whip, srt) = capabilities(&muxers, &protocols);
            Some(Publisher { program, whip, srt })
        })
        .await
        .clone()
}

/// The encoder: `libx264` unless `UXDUCK_LIVE_ENCODER` names a hardware one,
/// such as `h264_v4l2m2m` or `h264_videotoolbox`.
fn encoder() -> String {
    std::env::var("UXDUCK_LIVE_ENCODER").unwrap_or_else(|_| "libx264".into())
}

/// What the publishing side shares: the cameras as viewers see them, and the uplink.
#[derive(Debug, Default)]
struct Shared {
    cameras: Mutex<BTreeMap<String, RemoteCamera>>,
    uplink: Mutex<Uplink>,
}

impl Shared {
    fn set(&self, camera: &str, live_on: Option<&str>, problem: Option<&str>) {
        let mut cameras = self
            .cameras
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cameras.insert(
            camera.to_owned(),
            RemoteCamera {
                id: camera.to_owned(),
                name: String::new(),
                live_on: live_on.map(str::to_owned),
                problem: problem.map(str::to_owned),
            },
        );
    }

    fn forget(&self, camera: &str) {
        self.cameras
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(camera);
    }

    fn uplink(&self) -> std::sync::MutexGuard<'_, Uplink> {
        self.uplink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// How one connection to the relay publishes: started with it, ended
/// with it, so nothing is sent while the computer is not connected.
#[derive(Debug)]
pub struct Live {
    engine: EngineHandle,
    shared: Arc<Shared>,
    wanted: watch::Sender<Vec<String>>,
    rung: watch::Sender<usize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Live {
    fn drop(&mut self) {
        // The cameras' tasks are in the manager's set, which goes with it.
        self.task.abort();
        self.engine.post(|e| e.video_remote_wanted(Vec::new()));
    }
}

/// What a camera's task needs.
#[derive(Clone, Debug)]
struct Ctx {
    api: String,
    token: String,
    device: String,
    engine: EngineHandle,
    shared: Arc<Shared>,
    rung: watch::Receiver<usize>,
    rung_tx: watch::Sender<usize>,
}

impl Live {
    #[must_use]
    pub fn start(api: &str, token: &str, device: &str, engine: &EngineHandle) -> Self {
        let shared = Arc::new(Shared::default());
        let (wanted, wanted_rx) = watch::channel(Vec::new());
        let (rung, rung_rx) = watch::channel(START_RUNG);
        let ctx = Ctx {
            api: api.to_owned(),
            token: token.to_owned(),
            device: device.to_owned(),
            engine: engine.clone(),
            shared: shared.clone(),
            rung: rung_rx,
            rung_tx: rung.clone(),
        };
        let task = tokio::spawn(manage(ctx, wanted_rx));
        Self {
            engine: engine.clone(),
            shared,
            wanted,
            rung,
            task,
        }
    }

    /// The cameras the relay says someone away watches.
    pub fn want(&self, cameras: Vec<String>) {
        let needed = cameras.clone();
        self.engine.post(move |e| e.video_remote_wanted(needed));
        self.wanted.send_if_modified(|was| {
            let changed = *was != cameras;
            *was = cameras;
            changed
        });
    }

    /// A round trip to the relay, on the uplink video shares.
    pub fn round_trip(&self, ms: u64) {
        let mut uplink = self.shared.uplink();
        if uplink.round_trip(ms, Instant::now()) {
            let _ = self.rung.send(uplink.rung());
        }
    }

    /// A command from away is going to a vehicle.
    pub fn command(&self) {
        self.shared.uplink().command(Instant::now());
    }

    /// Each camera wanted now, live on a node or with its problem.
    #[must_use]
    pub fn cameras(&self) -> BTreeMap<String, RemoteCamera> {
        self.shared
            .cameras
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether anything is being published or tried.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.wanted.borrow().is_empty() || !self.cameras().is_empty()
    }
}

async fn manage(ctx: Ctx, mut wanted: watch::Receiver<Vec<String>>) {
    let mut demand = Demand::default();
    // Tests wait less than the linger.
    if let Some(ms) = std::env::var("UXDUCK_LIVE_LINGER_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        demand.linger = Duration::from_millis(ms);
    }
    let mut tasks = tokio::task::JoinSet::new();
    let mut running: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tick.tick() => {}
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
        let now = Instant::now();
        let (start, stop) = demand.update(&wanted.borrow(), now);
        for camera in stop {
            if let Some(task) = running.remove(&camera) {
                task.abort();
            }
            ctx.shared.forget(&camera);
            tracing::info!(%camera, "live video: nobody watches it now; stopped");
        }
        for camera in start {
            tracing::info!(%camera, "live video: watched from away; publishing");
            ctx.shared.set(&camera, None, None);
            let handle = tasks.spawn(publish(ctx.clone(), camera.clone()));
            running.insert(camera, handle);
        }
    }
}

/// Asks the coordinator for nodes and a grant.
async fn grant(ctx: &Ctx, request: &MediaPublishRequest) -> Result<MediaPublishGrant, String> {
    let client = crate::online::client(Duration::from_secs(15)).ok_or("no web client here")?;
    let answer = client
        .post(format!("{}/v1/media/publish", ctx.api))
        .bearer_auth(&ctx.token)
        .json(request)
        .send()
        .await
        .map_err(|_| "UXDuck cannot reach its server for live video".to_owned())?;
    match answer.status() {
        s if s.is_success() => answer
            .json()
            .await
            .map_err(|_| "the server's answer was not understood".to_owned()),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            Err("Live video from away is not set up on UXDuck's server yet".into())
        }
        s => Err(format!("UXDuck's server refused live video ({s})")),
    }
}

/// How one run of the publisher ended.
enum Ended {
    /// The grant runs out soon: ask again, same node.
    Refresh,
    /// The node or the publisher failed: try the next node.
    Failed(String),
}

/// One camera, from its grant to its stop (an abort).
#[allow(
    clippy::too_many_lines,
    reason = "one camera's life, read top to bottom"
)]
async fn publish(ctx: Ctx, camera: String) {
    let mut attempt = 0u32;
    loop {
        let wait = crate::edge::backoff(attempt, crate::edge::unit());
        attempt = attempt.saturating_add(1);
        let found = {
            let id = camera.clone();
            ctx.engine.run(move |e| e.edge_live_source(&id)).await
        };
        let Ok(Some(source)) = found else {
            ctx.shared.set(
                &camera,
                None,
                Some("This camera cannot be watched from away now"),
            );
            tokio::time::sleep(wait).await;
            continue;
        };
        let Some(publisher) = publisher().await else {
            ctx.shared.set(
                &camera,
                None,
                Some("Live video from away needs ffmpeg on the computer with the camera"),
            );
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        };
        let Some(hub) = crate::video::hub() else {
            ctx.shared.set(
                &camera,
                None,
                Some("Video has not started on this computer"),
            );
            tokio::time::sleep(wait).await;
            continue;
        };
        let request = MediaPublishRequest {
            device_id: ctx.device.clone(),
            camera_id: camera.clone(),
            recording_id: source.recording_id.clone(),
            store: source.store,
        };
        let granted = match grant(&ctx, &request).await {
            Ok(granted) => granted,
            Err(problem) => {
                ctx.shared.set(&camera, None, Some(&problem));
                tokio::time::sleep(wait).await;
                continue;
            }
        };
        let Some(protocol) = choose(granted.protocol, &publisher) else {
            ctx.shared.set(
                &camera,
                None,
                Some("This computer's ffmpeg can send neither WHIP nor SRT"),
            );
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        };
        let mut problem = "No media node took the camera".to_owned();
        for node in &granted.nodes {
            let url = match protocol {
                Contribution::Whip => Some(node.whip_url.clone()),
                Contribution::Srt => node.srt_url.clone(),
            };
            let Some(url) = url else { continue };
            let target = Target {
                protocol,
                url,
                grant: granted.grant.clone(),
            };
            ctx.shared.set(&camera, Some(&node.id), None);
            let started = Instant::now();
            match run(
                &ctx,
                &hub,
                &publisher,
                &camera,
                &target,
                &request,
                granted.expires_at_ms,
            )
            .await
            {
                Ended::Refresh => {
                    attempt = 0;
                    problem.clear();
                    break;
                }
                Ended::Failed(why) => {
                    tracing::info!(%camera, node = %node.id, %why, "live video: the node did not take it");
                    if started.elapsed() > Duration::from_secs(30) {
                        attempt = 0;
                    }
                    problem = why;
                }
            }
        }
        if problem.is_empty() {
            continue;
        }
        ctx.shared.set(&camera, None, Some(&problem));
        tokio::time::sleep(wait).await;
    }
}

/// The publisher on one node until it fails or the grant needs renewing;
/// restarted at the new rung when the uplink changes.
async fn run(
    ctx: &Ctx,
    hub: &Arc<crate::video::hub::Hub>,
    publisher: &Publisher,
    camera: &str,
    target: &Target,
    request: &MediaPublishRequest,
    expires_at_ms: u64,
) -> Ended {
    let key = format!("live:{camera}");
    let watcher = hub.tap(&key, camera);
    let ended = pump(
        ctx,
        &watcher,
        publisher,
        camera,
        target,
        request,
        expires_at_ms,
    )
    .await;
    hub.close(&key);
    ended
}

/// How long before the grant runs out it is renewed.
const RENEW_BEFORE_MS: u64 = 10 * 60_000;
/// A write to the publisher that takes this long means the uplink is full.
const WRITE_STALL: Duration = Duration::from_secs(1);

#[allow(
    clippy::too_many_lines,
    reason = "one publisher's life, read top to bottom"
)]
async fn pump(
    ctx: &Ctx,
    watcher: &crate::video::hub::Watcher,
    publisher: &Publisher,
    camera: &str,
    target: &Target,
    request: &MediaPublishRequest,
    expires_at_ms: u64,
) -> Ended {
    let mut rung = ctx.rung.clone();
    let mut recording = request.recording_id.clone();
    let mut look = tokio::time::interval(Duration::from_secs(30));
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first picture says the codec; the watcher starts at a keyframe.
    let first = loop {
        match tokio::time::timeout(Duration::from_secs(20), watcher.next()).await {
            Err(_) => return Ended::Failed("No pictures are coming from the camera".into()),
            Ok(None) => return Ended::Failed("The camera stopped".into()),
            Ok(Some((Item::Unit(unit), _))) => break unit,
            Ok(Some(_)) => {}
        }
    };
    let Some(input) = Input::of(&first.sps.codec) else {
        return Ended::Failed(format!(
            "The camera's codec ({}) cannot be sent",
            first.sps.codec
        ));
    };
    let mut pending = Some(first);
    loop {
        if crate::edge::now_ms() + RENEW_BEFORE_MS >= expires_at_ms {
            return Ended::Refresh;
        }
        let at = LADDER[(*rung.borrow_and_update()).min(LADDER.len() - 1)];
        let mut child = match tokio::process::Command::new(&publisher.program)
            .args(ffmpeg_args(input, at, &encoder(), target))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => return Ended::Failed(format!("ffmpeg did not start: {error}")),
        };
        // Its last words, for the problem shown.
        let said = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let said = said.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    *said
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = line;
                }
            });
        }
        let Some(mut stdin) = child.stdin.take() else {
            return Ended::Failed("ffmpeg took no input".into());
        };
        let last_words = || {
            let line = said
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if line.is_empty() {
                "The media node closed the stream".to_owned()
            } else {
                format!("The media node closed the stream: {line}")
            }
        };
        tracing::debug!(
            camera,
            kbps = at.kbps,
            fps = at.fps,
            height = at.height,
            "live video: publishing"
        );
        let restart = loop {
            if let Some(unit) = pending.take() {
                match tokio::time::timeout(WRITE_STALL, stdin.write_all(&unit.data)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        let _ = child.wait().await;
                        return Ended::Failed(last_words());
                    }
                    Err(_) => {
                        // The uplink is full: yield, and start again at a keyframe.
                        watcher.wait_for_keyframe();
                        if ctx.shared.uplink().fell_behind(Instant::now()) {
                            let _ = ctx.rung_tx.send(ctx.shared.uplink().rung());
                        }
                    }
                }
            }
            tokio::select! {
                next = watcher.next() => match next {
                    None => return Ended::Failed("The camera stopped".into()),
                    Some((Item::Unit(unit), gap)) => {
                        if gap && ctx.shared.uplink().fell_behind(Instant::now()) {
                            let _ = ctx.rung_tx.send(ctx.shared.uplink().rung());
                        }
                        pending = Some(unit);
                    }
                    Some(_) => {}
                },
                changed = rung.changed() => {
                    if changed.is_err() {
                        return Ended::Failed("stopped".into());
                    }
                    break true;
                }
                _ = child.wait() => return Ended::Failed(last_words()),
                _ = look.tick() => {
                    // A recording that started since: the coordinator is told,
                    // so its original replaces the live recording (C13).
                    let id = camera.to_owned();
                    let now = ctx.engine.run(move |e| e.edge_live_source(&id)).await;
                    let latest = now.ok().flatten().and_then(|s| s.recording_id);
                    if latest.is_some() && latest != recording {
                        recording.clone_from(&latest);
                        let ctx = ctx.clone();
                        let request = MediaPublishRequest { recording_id: latest, ..request.clone() };
                        tokio::spawn(async move { let _ = grant(&ctx, &request).await; });
                    }
                    if crate::edge::now_ms() + RENEW_BEFORE_MS >= expires_at_ms {
                        break false;
                    }
                }
            }
        };
        let _ = child.kill().await;
        if !restart {
            return Ended::Refresh;
        }
        // Again at the new rung, from a keyframe.
        watcher.wait_for_keyframe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_remote_viewer_leaving_stops_publishing_immediately_by_default() {
        let now = Instant::now();
        let mut demand = Demand::default();
        assert_eq!(
            demand.update(&["camera".into()], now),
            (vec!["camera".into()], vec![])
        );
        assert_eq!(demand.update(&[], now), (vec![], vec!["camera".into()]));
        assert!(demand.running().is_empty());
    }

    #[test]
    fn an_explicit_linger_can_keep_a_stream_over_a_page_reload() {
        let start = Instant::now();
        let linger = Duration::from_secs(10);
        let mut demand = Demand {
            linger,
            ..Demand::default()
        };
        assert_eq!(
            demand.update(&["a".into(), "b".into()], start),
            (vec!["a".into(), "b".into()], vec![])
        );
        // Still watched: nothing new.
        assert_eq!(
            demand.update(&["a".into(), "b".into()], start),
            (vec![], vec![])
        );
        // B's viewer goes: b lingers, then stops.
        let soon = start + Duration::from_secs(3);
        assert_eq!(demand.update(&["a".into()], soon), (vec![], vec![]));
        assert_eq!(
            demand.update(&["a".into()], soon + linger),
            (vec![], vec!["b".into()])
        );
        // A reload: gone and back within the linger, never restarted.
        let later = soon + linger + Duration::from_secs(1);
        assert_eq!(demand.update(&[], later), (vec![], vec![]));
        assert_eq!(
            demand.update(&["a".into()], later + Duration::from_secs(2)),
            (vec![], vec![])
        );
        assert_eq!(
            demand.update(&[], later + Duration::from_secs(3) + linger),
            (vec![], vec![]),
            "the linger starts again when it is unwatched again"
        );
        assert_eq!(
            demand.update(&[], later + Duration::from_secs(4) + linger * 2),
            (vec![], vec!["a".into()])
        );
        assert!(demand.running().is_empty());
    }

    #[test]
    fn video_steps_down_when_the_uplink_queues_and_climbs_back_slowly() {
        let start = Instant::now();
        let mut uplink = Uplink::default();
        assert_eq!(LADDER[uplink.rung()].kbps, 1_500, "starts below the top");
        // A steady 60 ms: after 20 s clean, one rung up.
        for s in 0..20 {
            assert!(!uplink.round_trip(60, start + Duration::from_secs(s)));
        }
        assert!(uplink.round_trip(60, start + Duration::from_secs(20)));
        assert_eq!(uplink.rung(), 0);
        // The uplink queues: the relay's round trip triples. Down at once,
        // then at most every 2 s however bad it is.
        let busy = start + Duration::from_secs(30);
        assert!(uplink.round_trip(400, busy));
        assert_eq!(uplink.rung(), 1);
        assert!(!uplink.round_trip(450, busy + Duration::from_millis(500)));
        assert!(uplink.fell_behind(busy + Duration::from_secs(2)));
        assert_eq!(uplink.rung(), 2);
        // Bitrate went first, then frame rate, then resolution.
        let order: Vec<_> = LADDER.windows(2).map(|w| (w[0], w[1])).collect();
        for (a, b) in order {
            assert!(b.kbps <= a.kbps && b.fps <= a.fps && b.height <= a.height);
        }
        assert_eq!((LADDER[3].fps, LADDER[3].height), (15, 720));
        assert_eq!(LADDER[4].height, 480);
        // Never past the bottom.
        let mut t = busy + Duration::from_secs(4);
        for _ in 0..20 {
            uplink.fell_behind(t);
            t += Duration::from_secs(3);
        }
        assert_eq!(uplink.rung(), LADDER.len() - 1);
        // Commands from away hold it down: no climb while they come.
        let quiet = t + Duration::from_secs(60);
        uplink.round_trip(60, quiet);
        for s in 1..30 {
            if s % 10 == 0 {
                uplink.command(quiet + Duration::from_secs(s));
            }
            assert!(
                !uplink.round_trip(60, quiet + Duration::from_secs(s)),
                "climbed at {s} s with commands going"
            );
        }
    }

    fn target(protocol: Contribution, url: &str) -> Target {
        Target {
            protocol,
            url: url.into(),
            grant: "eyJ.grant".into(),
        }
    }

    #[test]
    fn ffmpeg_reads_the_camera_from_its_stdin_and_sends_h264_without_b_frames() {
        let whip = ffmpeg_args(
            Input::H265,
            LADDER[4],
            "libx264",
            &target(Contribution::Whip, "https://lon1.media.test/live/s1/whip"),
        );
        let line = whip.join(" ");
        assert!(
            line.contains("-use_wallclock_as_timestamps 1 -f hevc -i pipe:0"),
            "{line}"
        );
        assert!(line.contains("-vf fps=15,scale=-2:'min(480,ih)'"), "{line}");
        assert!(
            line.contains("-c:v libx264 -preset ultrafast -tune zerolatency"),
            "{line}"
        );
        assert!(
            line.contains("-bf 0 -g 30 -b:v 400k -maxrate 400k -bufsize 200k"),
            "{line}"
        );
        assert!(
            line.ends_with("-f whip -authorization eyJ.grant https://lon1.media.test/live/s1/whip"),
            "{line}"
        );
        let srt = ffmpeg_args(
            Input::H264,
            LADDER[0],
            "h264_v4l2m2m",
            &target(
                Contribution::Srt,
                "srt://lon1:8890?streamid=publish:live/s1:jwt:eyJ.grant",
            ),
        )
        .join(" ");
        assert!(srt.contains("-f h264 -i pipe:0"), "{srt}");
        assert!(
            !srt.contains("-preset"),
            "only libx264 takes a preset: {srt}"
        );
        assert!(
            srt.ends_with(
                "-f mpegts srt://lon1:8890?streamid=publish:live/s1:jwt:eyJ.grant&latency=200000"
            ),
            "{srt}"
        );
    }

    #[test]
    fn the_protocol_is_the_coordinators_unless_this_ffmpeg_lacks_it() {
        // Homebrew's FFmpeg 8.1: WHIP, no SRT.
        let muxers = " E  mpegts          MPEG-TS (MPEG-2 Transport Stream)\n E  whip            WHIP(WebRTC-HTTP ingestion protocol) muxer\n";
        let protocols = "Input:\n  rtmp\n  srtp\nOutput:\n  rtmp\n  srtp\n";
        assert_eq!(capabilities(muxers, protocols), (true, false));
        assert_eq!(
            capabilities(" E  mpegts  MPEG-TS\n", "Output:\n  srt\n"),
            (false, true)
        );
        let homebrew = Publisher {
            program: "ffmpeg".into(),
            whip: true,
            srt: false,
        };
        assert_eq!(
            choose(Contribution::Srt, &homebrew),
            Some(Contribution::Whip)
        );
        assert_eq!(
            choose(Contribution::Whip, &homebrew),
            Some(Contribution::Whip)
        );
        let neither = Publisher {
            whip: false,
            ..homebrew
        };
        assert_eq!(choose(Contribution::Whip, &neither), None);
        assert_eq!(Input::of("avc1.64001f"), Some(Input::H264));
        assert_eq!(Input::of("hvc1.1.6.L93.B0"), Some(Input::H265));
        assert_eq!(Input::of("mp4v"), None);
    }
}

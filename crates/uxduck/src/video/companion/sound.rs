//! Sound from the camera computer's microphones (ADR-0067 M2 to M5, M7):
//! one `gst-launch-1.0` per camera that has a microphone, which captures
//! it, converts it to 48 kHz mono and encodes it as Opus in RTP once per
//! live step and once for the recording, each to its own localhost UDP
//! port of the companion's choosing.
//!
//! The devices are read again every second (`microphones.rs`), so a
//! microphone plugged in starts its camera's process and one pulled out
//! stops it. A process that ends while its device is still there is
//! started again after a wait that doubles up to `RESTART_MOST`. A
//! camera's choice changed by a reload applies at the next read. The video
//! never waits for any of it.
//!
//! The four live encodes go to sockets this module holds, and their
//! packets go to the video sender, which sends each listening viewer one
//! of them (`sender/listening.rs`). Every payloader starts its timestamps at
//! 0 and every run of the process has its own SSRC, so the steps of one run
//! share one timeline and a new run is told from the last. The recording's
//! encode goes to a port the onboard recorder's joining process reads (M7).
//!
//! `GStreamer` is a Raspberry Pi OS package and only a camera computer with a
//! microphone needs it: `sudo apt-get install` the packages in `PACKAGES`.
//! When it cannot be run and a camera's `microphone` is not `none`, the
//! journal says so as an error, the camera's sound is `failed` with the
//! reason, and the recorder says it to the ground.
//!
//! For tests and the cloud check, three environment variables stand in for
//! the computer's own:
//!
//! - `UXDUCK_COMPANION_SOUND_ROOT`: a folder holding `proc/` and `sys/` trees
//!   read in place of `/proc/asound` and `/sys/class/sound`.
//! - `UXDUCK_COMPANION_SOUND_SOURCE`: a `GStreamer` source used in place of
//!   `alsasrc`, such as `audiotestsrc is-live=true wave=sine freq=440`.
//! - `UXDUCK_GST_LAUNCH`: the `gst-launch-1.0` to run, such as a fake that
//!   sends Opus RTP to the ports it is given.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::AsyncBufReadExt as _;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use super::microphones::{self, Choice, Microphone, Resolved};

/// The Debian and Raspberry Pi OS packages sound needs on the camera
/// computer: `gst-launch-1.0`, `alsasrc`, the converters, Opus and RTP, and
/// `rtspclientsink` for the recording (M4).
pub const PACKAGES: [&str; 5] = [
    "gstreamer1.0-tools",
    "gstreamer1.0-alsa",
    "gstreamer1.0-plugins-base",
    "gstreamer1.0-plugins-good",
    "gstreamer1.0-rtsp",
];

/// How often the devices are read and the processes checked (M2).
const EVERY: Duration = Duration::from_secs(1);
/// The first wait before a process that ended is started again.
const RESTART_FIRST: Duration = Duration::from_secs(1);
/// The longest wait, doubling from the first: a device that keeps refusing
/// is tried twice a minute, which keeps the journal readable and still
/// picks up a fix within half a minute.
const RESTART_MOST: Duration = Duration::from_secs(30);
/// A process that ran this long before it ended was working, so the next
/// wait starts again from the first.
const RAN_WELL: Duration = Duration::from_secs(10);
/// Bytes on the wire around each Opus packet: IPv4 20, UDP 8, RTP 12,
/// SRTP's tag 10 and the transport-wide sequence extension 8 (M5).
const HEADERS: u32 = 58;

/// An Opus encode: its bitrate, packet length and whether its band is full
/// (20 kHz) or the encoder's own choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Encode {
    /// bit/s.
    pub bitrate: u32,
    pub ms: u32,
    pub fullband: bool,
}

impl Encode {
    /// What it costs on the wire, sound and headers, bit/s.
    #[must_use]
    pub const fn wire(&self) -> u32 {
        self.bitrate + HEADERS * 8 * 1000 / self.ms
    }

    /// Its packets' length in 48 kHz samples.
    #[must_use]
    pub const fn samples(&self) -> u32 {
        self.ms * 48
    }
}

/// The live steps, highest first (M5): about 55, 47, 28 and 20 kbit/s on
/// the wire. Below 24 kbit/s the encoder chooses its own band (M1).
pub const STEPS: [Encode; 4] = [
    Encode {
        bitrate: 32_000,
        ms: 20,
        fullband: true,
    },
    Encode {
        bitrate: 24_000,
        ms: 20,
        fullband: true,
    },
    Encode {
        bitrate: 16_000,
        ms: 40,
        fullband: false,
    },
    Encode {
        bitrate: 12_000,
        ms: 60,
        fullband: false,
    },
];

/// The recording's fixed encode (M7, ADR-0066 R1's fixed quality).
pub const RECORDING: Encode = Encode {
    bitrate: 64_000,
    ms: 20,
    fullband: true,
};

/// What the sound processes stand on: `/`, `alsasrc` and the
/// `gst-launch-1.0` on `PATH`, unless the environment says otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub root: PathBuf,
    pub source: Option<String>,
    pub gst: PathBuf,
}

impl Environment {
    #[must_use]
    pub fn from_env() -> Self {
        let set = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        Self {
            root: set("UXDUCK_COMPANION_SOUND_ROOT")
                .map_or_else(|| PathBuf::from("/"), PathBuf::from),
            source: set("UXDUCK_COMPANION_SOUND_SOURCE").map(|s| s.to_string_lossy().into_owned()),
            gst: set("UXDUCK_GST_LAUNCH")
                .map_or_else(|| PathBuf::from("gst-launch-1.0"), PathBuf::from),
        }
    }
}

/// Where `gst` is: a path as given, else found on `PATH`. None when it is
/// not there to run.
#[must_use]
pub fn locate(gst: &Path) -> Option<PathBuf> {
    let runnable = |path: &Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    };
    if gst.components().count() > 1 {
        return runnable(gst).then(|| gst.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(gst))
        .find(|path| runnable(path))
}

/// Why there is no sound when `GStreamer` cannot be run, for people.
fn missing(gst: &Path) -> String {
    format!(
        "GStreamer is not installed on the camera computer ({} cannot be run). Install {}.",
        gst.display(),
        PACKAGES.join(", ")
    )
}

/// Whether a camera's sound can come (M9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    /// Its microphone is there and its process runs.
    Available,
    /// `auto` with no capture device.
    NoMicrophone,
    /// The chosen device is not plugged in.
    Unplugged,
    /// `microphone = "none"`.
    Off,
    /// `GStreamer` missing, the device refused or held by another camera.
    Failed,
}

/// One camera's sound now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraSound {
    pub state: State,
    /// The device its choice gives, when one is plugged in.
    pub microphone: Option<Microphone>,
    /// Why it cannot be heard, for people.
    pub reason: Option<String>,
    /// While its sound is wanted, its `microphone` not `none` and `GStreamer`
    /// there: the port its recording encode goes to (M7).
    #[serde(skip)]
    pub recording: Option<u16>,
}

/// The devices, and each camera's sound by its path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub microphones: Vec<Microphone>,
    pub cameras: BTreeMap<String, CameraSound>,
}

/// An Opus packet from one of a camera's live steps.
#[derive(Clone, Debug)]
pub struct Packet {
    pub path: String,
    /// Its index in `STEPS`.
    pub step: usize,
    /// The run of the process it came from.
    pub ssrc: u32,
    pub timestamp: u32,
    pub payload: Vec<u8>,
    /// When it arrived, which is when its sound ended, give or take the
    /// encoder.
    pub at: Instant,
}

/// What the sender takes: each camera's sound and its packets.
pub struct Feed {
    pub status: watch::Receiver<Status>,
    pub packets: mpsc::Receiver<Packet>,
}

/// The manager, running.
pub struct Running {
    pub feed: Feed,
    /// Each camera's sound, for the recorder.
    pub status: watch::Receiver<Status>,
}

/// Binds each camera's ports and starts reading the devices. `choices` is
/// each camera's `microphone` by its path, again after each reload.
///
/// # Errors
/// No localhost UDP port.
pub async fn start(
    environment: Environment,
    mut choices: watch::Receiver<BTreeMap<String, Choice>>,
) -> Result<Running, String> {
    let (packets_tx, packets) = mpsc::channel(256);
    let mut cameras = Vec::new();
    for (path, choice) in choices.borrow_and_update().iter() {
        let mut ports = [0; STEPS.len()];
        for (step, port) in ports.iter_mut().enumerate() {
            let socket = UdpSocket::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("cannot open a localhost UDP port for sound: {e}"))?;
            *port = socket.local_addr().map_err(|e| e.to_string())?.port();
            tokio::spawn(receive(socket, path.clone(), step, packets_tx.clone()));
        }
        // Chosen now and read by the recorder's joining process later.
        let recording = std::net::UdpSocket::bind("127.0.0.1:0")
            .and_then(|s| s.local_addr())
            .map_err(|e| format!("cannot find a localhost UDP port for sound: {e}"))?
            .port();
        cameras.push(Camera {
            path: path.clone(),
            choice: choice.clone(),
            ports,
            recording,
            run: None,
            backoff: Backoff::default(),
            failure: None,
            said: None,
            ssrc: 0,
        });
    }
    let (status_tx, status) = watch::channel(Status::default());
    let mut manager = Manager {
        environment,
        cameras,
        status: status_tx,
    };
    manager.tick();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(EVERY);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = timer.tick() => {}
                changed = choices.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let now = choices.borrow_and_update().clone();
                    for camera in &mut manager.cameras {
                        if let Some(choice) = now.get(&camera.path)
                            && *choice != camera.choice
                        {
                            tracing::info!(path = %camera.path, from = %camera.choice, to = %choice, "the camera's microphone changed");
                            camera.choice = choice.clone();
                        }
                    }
                }
            }
            if manager.status.is_closed() {
                return;
            }
            manager.tick();
        }
    });
    Ok(Running {
        feed: Feed {
            status: status.clone(),
            packets,
        },
        status,
    })
}

/// Reads one live step's RTP and hands its packets on.
async fn receive(socket: UdpSocket, path: String, step: usize, packets: mpsc::Sender<Packet>) {
    let mut buffer = vec![0_u8; 2048];
    loop {
        let Ok(length) = socket.recv(&mut buffer).await else {
            continue;
        };
        let at = Instant::now();
        let Some((ssrc, timestamp, payload)) = rtp(&buffer[..length]) else {
            continue;
        };
        let packet = Packet {
            path: path.clone(),
            step,
            ssrc,
            timestamp,
            payload: payload.to_vec(),
            at,
        };
        if packets.send(packet).await.is_err() {
            return;
        }
    }
}

/// An RTP packet's SSRC, timestamp and payload.
#[must_use]
pub fn rtp(packet: &[u8]) -> Option<(u32, u32, &[u8])> {
    let first = *packet.first()?;
    if first >> 6 != 2 || packet.len() < 12 {
        return None;
    }
    let word = |at: usize| Some(u32::from_be_bytes(packet.get(at..at + 4)?.try_into().ok()?));
    let mut start = 12 + 4 * usize::from(first & 0x0f);
    if first & 0x10 != 0 {
        let words = u16::from_be_bytes(packet.get(start + 2..start + 4)?.try_into().ok()?);
        start += 4 + 4 * usize::from(words);
    }
    let mut end = packet.len();
    if first & 0x20 != 0 {
        end = end.checked_sub(usize::from(*packet.last()?))?;
    }
    Some((word(8)?, word(4)?, packet.get(start..end)?))
}

/// A wait that doubles each time a process ends early.
#[derive(Debug)]
pub(in crate::video) struct Backoff {
    wait: Duration,
    until: Option<Instant>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            wait: RESTART_FIRST,
            until: None,
        }
    }
}

impl Backoff {
    /// Whether a process may start now.
    pub(in crate::video) fn due(&self, now: Instant) -> bool {
        self.until.is_none_or(|until| now >= until)
    }

    /// A process that ran for `ran` ended: the next may start after the
    /// wait, which doubles unless it ran well.
    pub(in crate::video) fn ended(&mut self, ran: Duration, now: Instant) {
        if ran >= RAN_WELL {
            self.wait = RESTART_FIRST;
        }
        self.until = Some(now + self.wait);
        self.wait = (self.wait * 2).min(RESTART_MOST);
    }

    pub(in crate::video) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A `gst-launch-1.0` running, and the last error it wrote.
pub(in crate::video) struct Helper {
    child: tokio::process::Child,
    started: Instant,
    error: Arc<Mutex<Option<String>>>,
}

impl Helper {
    /// Starts `gst` with a pipeline's words.
    pub(in crate::video) fn spawn(gst: &Path, words: &[String]) -> std::io::Result<Self> {
        let mut child = tokio::process::Command::new(gst)
            .args(words)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let error = Arc::new(Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            let error = error.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(said) = gst_error(&line)
                        && let Ok(mut error) = error.lock()
                    {
                        *error = Some(said);
                    }
                }
            });
        }
        Ok(Self {
            child,
            started: Instant::now(),
            error,
        })
    }

    /// When it ended: how long it ran and why, in its own words when it
    /// said them.
    pub(in crate::video) fn ended(&mut self) -> Option<(Duration, String)> {
        let status = self.child.try_wait().ok()??;
        let said = self.error.lock().ok().and_then(|e| e.clone());
        Some((
            self.started.elapsed(),
            said.unwrap_or_else(|| format!("GStreamer stopped ({status})")),
        ))
    }
}

/// The message of a `gst-launch-1.0` error line, such as "Could not open
/// audio device for recording. Device is being used by another
/// application." from `ERROR: from element /GstPipeline:pipeline0/GstAlsaSrc:alsasrc0: …`.
fn gst_error(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("ERROR:")?.trim();
    let message = match rest.strip_prefix("from element ") {
        Some(from) => from.split_once(": ").map_or(from, |(_, message)| message),
        None => rest,
    };
    Some(message.trim().to_owned())
}

/// The words of the capture process: the device or the source, 48 kHz
/// mono, and one Opus encode in RTP to each port, the live steps' then the
/// recording's (M1, M4).
#[must_use]
pub fn pipeline(source: &str, ports: [u16; STEPS.len()], recording: u16, ssrc: u32) -> Vec<String> {
    let mut words: Vec<String> = source.split_whitespace().map(str::to_owned).collect();
    for word in [
        "!",
        "audioconvert",
        "!",
        "audioresample",
        "!",
        "audio/x-raw,rate=48000,channels=1",
        "!",
        "tee",
        "name=t",
    ] {
        words.push(word.to_owned());
    }
    for (encode, port) in STEPS.iter().zip(ports).chain([(&RECORDING, recording)]) {
        words.extend([
            "t.".to_owned(),
            "!".to_owned(),
            "queue".to_owned(),
            "!".to_owned(),
            "opusenc".to_owned(),
            "audio-type=generic".to_owned(),
            format!("bitrate={}", encode.bitrate),
            format!("frame-size={}", encode.ms),
            format!(
                "bandwidth={}",
                if encode.fullband { "fullband" } else { "auto" }
            ),
            "!".to_owned(),
            "rtpopuspay".to_owned(),
            "pt=96".to_owned(),
            format!("ssrc={ssrc}"),
            "timestamp-offset=0".to_owned(),
            "!".to_owned(),
            "udpsink".to_owned(),
            "host=127.0.0.1".to_owned(),
            format!("port={port}"),
            "sync=false".to_owned(),
            "async=false".to_owned(),
        ]);
    }
    words
}

/// The words of the joining process (M7): the record path's video read back
/// from `MediaMTX`'s RTSP, the recording encode from its port, both
/// published to the sound path.
#[must_use]
pub fn joining(rtsp: &str, record_path: &str, sound_path: &str, port: u16) -> Vec<String> {
    let rtsp = rtsp.trim_end_matches('/');
    [
        "rtspclientsink".to_owned(),
        "name=s".to_owned(),
        format!("location={rtsp}/{sound_path}"),
        "protocols=tcp".to_owned(),
        "rtspsrc".to_owned(),
        format!("location={rtsp}/{record_path}"),
        "latency=0".to_owned(),
        "protocols=tcp".to_owned(),
        "!".to_owned(),
        "rtph264depay".to_owned(),
        "!".to_owned(),
        "s.".to_owned(),
        "udpsrc".to_owned(),
        "address=127.0.0.1".to_owned(),
        format!("port={port}"),
        "caps=application/x-rtp,media=audio,clock-rate=48000,encoding-name=OPUS,payload=96"
            .to_owned(),
        "!".to_owned(),
        "rtpjitterbuffer".to_owned(),
        "latency=60".to_owned(),
        "!".to_owned(),
        "rtpopusdepay".to_owned(),
        "!".to_owned(),
        "s.".to_owned(),
    ]
    .into()
}

struct Run {
    device: String,
    helper: Helper,
}

struct Camera {
    path: String,
    choice: Choice,
    ports: [u16; STEPS.len()],
    recording: u16,
    run: Option<Run>,
    backoff: Backoff,
    /// Why its last process ended or did not start, while it waits.
    failure: Option<String>,
    /// What the journal last heard about it, so each is said once.
    said: Option<String>,
    /// The last run's SSRC, one more for each.
    ssrc: u32,
}

struct Manager {
    environment: Environment,
    cameras: Vec<Camera>,
    status: watch::Sender<Status>,
}

impl Manager {
    /// Reads the devices, starts and stops the processes, and says each
    /// camera's sound.
    fn tick(&mut self) {
        let found = microphones::find(&self.environment.root);
        let gst = locate(&self.environment.gst);
        let now = Instant::now();
        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        let mut cameras = BTreeMap::new();
        for camera in &mut self.cameras {
            let resolved = camera.choice.resolve(&found);
            let wanted = camera.choice != Choice::None && gst.is_some();
            let (state, microphone, reason) = match (&resolved, &gst) {
                (Resolved::Device(device), Some(gst)) => {
                    let owner = owners
                        .entry(device.id.clone())
                        .or_insert_with(|| camera.path.clone());
                    if *owner == camera.path {
                        camera.keep(device, gst, self.environment.source.as_deref(), now);
                    } else {
                        camera.stop();
                    }
                    let reason = if *owner == camera.path {
                        camera
                            .run
                            .is_none()
                            .then(|| camera.failure.clone())
                            .flatten()
                    } else {
                        Some(format!(
                            "{} is used by {owner}, and one microphone serves one camera.",
                            device.name
                        ))
                    };
                    let state = if reason.is_some() {
                        State::Failed
                    } else {
                        State::Available
                    };
                    (state, Some((*device).clone()), reason)
                }
                (Resolved::Off, _) => {
                    camera.stop();
                    (State::Off, None, None)
                }
                (Resolved::NoMicrophone, _) => {
                    camera.stop();
                    (State::NoMicrophone, None, None)
                }
                (_, None) => {
                    camera.stop();
                    (State::Failed, None, Some(missing(&self.environment.gst)))
                }
                (Resolved::Unplugged(id), _) => {
                    camera.stop();
                    (
                        State::Unplugged,
                        None,
                        Some(format!("The microphone {id} is not plugged in.")),
                    )
                }
            };
            camera.say(state, microphone.as_ref(), reason.as_deref());
            cameras.insert(
                camera.path.clone(),
                CameraSound {
                    state,
                    microphone,
                    reason,
                    recording: wanted.then_some(camera.recording),
                },
            );
        }
        let status = Status {
            microphones: found,
            cameras,
        };
        self.status.send_if_modified(|now| {
            let changed = *now != status;
            *now = status;
            changed
        });
    }
}

impl Camera {
    /// Keeps a process running on `device`: one on another device stops,
    /// one that ended is noted and started again when its wait is over.
    fn keep(&mut self, device: &Microphone, gst: &Path, source: Option<&str>, now: Instant) {
        if self.run.as_ref().is_some_and(|run| run.device != device.id) {
            self.stop();
        }
        if let Some(run) = &mut self.run
            && let Some((ran, why)) = run.helper.ended()
        {
            self.run = None;
            self.backoff.ended(ran, now);
            self.failure = Some(why);
        }
        if self.run.is_some() || !self.backoff.due(now) {
            return;
        }
        self.ssrc = self.ssrc.wrapping_add(1);
        let source = source.map_or_else(
            || format!("alsasrc device=\"{}\"", device.alsa),
            str::to_owned,
        );
        let words = pipeline(&source, self.ports, self.recording, self.ssrc);
        match Helper::spawn(gst, &words) {
            Ok(helper) => {
                tracing::info!(path = %self.path, microphone = %device.id, name = %device.name, "starting the camera's sound");
                self.run = Some(Run {
                    device: device.id.clone(),
                    helper,
                });
            }
            Err(error) => {
                self.backoff.ended(Duration::ZERO, now);
                self.failure = Some(format!("{} cannot be run: {error}", gst.display()));
            }
        }
    }

    /// Stops its process, and forgets why the last one ended.
    fn stop(&mut self) {
        if let Some(run) = self.run.take() {
            tracing::info!(path = %self.path, microphone = %run.device, "stopping the camera's sound");
        }
        self.backoff.reset();
        self.failure = None;
    }

    /// Says a change in the journal, once.
    fn say(&mut self, state: State, microphone: Option<&Microphone>, reason: Option<&str>) {
        let said = format!("{state:?} {:?} {reason:?}", microphone.map(|m| &m.id));
        if self.said.as_ref() == Some(&said) {
            return;
        }
        self.said = Some(said);
        let path = &self.path;
        let microphone = microphone.map(|m| m.id.as_str());
        match (state, reason) {
            (State::Failed, Some(reason)) if reason.starts_with("GStreamer is not installed") => {
                tracing::error!(%path, %reason, "no sound for this camera");
            }
            (State::Failed, Some(reason)) => {
                tracing::warn!(%path, ?microphone, %reason, "no sound for this camera");
            }
            _ => tracing::info!(%path, ?state, ?microphone, ?reason, "the camera's sound"),
        }
    }
}

#[cfg(test)]
pub(in crate::video) mod tests;

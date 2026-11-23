//! Recording on the vehicle (ADR-0066): the camera computer records each
//! camera's second, full-quality encode to its own disk, through a
//! `MediaMTX` path whose `record` this turns on and off.
//!
//! What it should be is set per camera in `/etc/uxduck/companion.toml`,
//! `record` under `[cameras.<path>]`: `armed` (the default) records from when
//! the flight controller arms until it disarms, `always` records from the
//! start, and `off` waits for the ground. A reload (SIGHUP) applies a
//! changed mode without a restart. The flight controller's `HEARTBEAT` comes from the vehicle's
//! MAVLink router, so recording follows arming with no ground station there.
//!
//! Each path is also a MAVLink camera on the vehicle's system id, component
//! 100 for the first, so the ground can start and stop it and see its
//! storage (ADR-0066 R3). A ground Start records now, in any mode, unless
//! there is no room, and that recording lasts until the ground's Stop, a
//! disarm included. A ground Stop stops until the next arm in `armed` mode,
//! and until a Start or a restart otherwise. Start and Stop are heard only
//! on the camera's own component: the ground's automatic recording sends
//! them to every component, and never reaches this recorder.
//!
//! `MediaMTX` keeps an API change in memory only, so `record` is read once a
//! second and set again when it is not what the recorder wants, as after
//! a restart of `MediaMTX` (ADR-0064 W8).
//!
//! When the disk fills, the oldest finished segments of earlier recordings
//! are deleted to keep recording, never the current recording's, and the
//! ground hears how much went (`disk`).
//!
//! With sound (ADR-0067 M7): while a camera records and its `microphone` is
//! not `none` with `GStreamer` there, a joining `gst-launch-1.0` reads the
//! record path's video back from `MediaMTX`'s RTSP, takes the camera's
//! recording encode from its port, and publishes both to
//! `<record path>_sound`, a path added through the API with the record
//! path's `record*` settings, again after a restart of `MediaMTX`. Once
//! that path is ready, `record` is on there in place of the record path, so
//! each segment has H.264 and Opus. Until then, and whenever the joining
//! process stops, the record path records the picture alone, so a
//! recording is never lost to sound. A microphone pulled out mid-flight
//! leaves the joined path recording with a silent sound track. Both paths'
//! folders count as this camera's recordings when the disk fills.

mod disk;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::net::UdpSocket;
use uxduck_mavlink::framing::split;
use uxduck_mavlink::onboard_camera::{
    self, ACCEPTED, CAMERA, CAMERA_CAPTURE_STATUS, CAMERA_INFORMATION, CAMERAS, Command, DENIED,
    NOTICE, REQUEST_MESSAGE, STORAGE_INFORMATION, Space, TEMPORARILY_REJECTED, UNSUPPORTED,
    VIDEO_START_CAPTURE, VIDEO_STOP_CAPTURE, WARNING,
};

use super::companion::sound::{self, Backoff, Helper};
use super::sender::Api;

/// The vendor name the ground knows a recording camera by (ADR-0066).
const VENDOR: &str = "UXDuck companion";

/// How often `record` and the disk are checked, the heartbeat sent and,
/// while recording, the capture status (the camera protocol's once a
/// second).
const EVERY: Duration = Duration::from_secs(1);

/// The recorder, from `[mavlink]` and the cameras with a `record_path` in
/// `/etc/uxduck/companion.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The vehicle's MAVLink router, `udp:HOST:PORT`, where the flight
    /// controller's heartbeat is heard and the ground's commands arrive.
    pub mavlink: String,
    /// The vehicle's system id (`SYSID_THISMAV` on `ArduPilot`).
    pub system_id: u8,
    /// `MediaMTX`'s API, with `user:password@` when it asks for one.
    pub mediamtx_api: String,
    /// `MediaMTX`'s RTSP server, where the joining process reads a record
    /// path's video and publishes it with sound (ADR-0067 M7).
    pub mediamtx_rtsp: String,
    pub paths: Vec<PathConfig>,
}

/// What recording with sound stands on (ADR-0067 M7): each camera's sound,
/// whose `recording` port is there while it is wanted, and the
/// `gst-launch-1.0` to run.
pub struct Sounds {
    pub status: tokio::sync::watch::Receiver<sound::Status>,
    pub gst: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathConfig {
    /// The `MediaMTX` path that records, such as `front_record`.
    pub path: String,
    /// The camera it records, its live path, such as `front`: the name the
    /// ground knows it by and its MAVLink camera's model name.
    pub camera: String,
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Armed,
    Always,
    Off,
}

impl Config {
    /// # Errors
    /// What is wrong with it, in words a person can act on.
    pub fn check(&self) -> Result<(), String> {
        // MAVLink has six camera components, 100 to 105.
        if self.paths.len() > usize::from(CAMERAS) {
            return Err(format!(
                "{} cameras have a record_path, and MAVLink has camera components for {CAMERAS}",
                self.paths.len()
            ));
        }
        let mut seen = HashSet::new();
        for path in &self.paths {
            if path.path.is_empty() {
                return Err(format!(
                    "the camera {} has an empty record_path",
                    path.camera
                ));
            }
            if path.path == path.camera {
                return Err(format!(
                    "the camera {} records its own path: record_path names the second encode, such as {}_record",
                    path.camera, path.camera
                ));
            }
            if !seen.insert(&path.path) {
                return Err(format!("two cameras have the record_path {}", path.path));
            }
        }
        router(&self.mavlink)?;
        Api::new(&self.mediamtx_api)?;
        Ok(())
    }
}

/// `udp:HOST:PORT`.
fn router(spec: &str) -> Result<(String, u16), String> {
    spec.strip_prefix("udp:")
        .and_then(|rest| rest.rsplit_once(':'))
        .and_then(|(host, port)| Some((host.to_owned(), port.parse().ok()?)))
        .filter(|(host, _)| !host.is_empty())
        .ok_or_else(|| {
            format!(
                "[mavlink] router = \"{spec}\" is not udp:HOST:PORT, such as udp:127.0.0.1:15010"
            )
        })
}

/// One path's recording as the recorder wants it.
struct Unit {
    path: String,
    name: String,
    mode: Mode,
    camera: onboard_camera::Camera,
    /// Since when it should be recording, on the recorder's clock.
    since_ms: Option<u64>,
    /// The ground started this recording, so only its Stop ends it.
    by_ground: bool,
    space: Space,
    /// Whether there is room to record, deleting earlier recordings.
    room: bool,
}

/// The recordings' state and their MAVLink cameras, as plain state: frames
/// in, frames out, on a clock it is given.
pub struct Recorder {
    system: u8,
    units: Vec<Unit>,
    armed: Option<bool>,
}

impl Recorder {
    #[must_use]
    pub fn new(config: &Config, now_ms: u64) -> Self {
        let units = config
            .paths
            .iter()
            .zip(CAMERA..)
            .map(|(path, component)| Unit {
                path: path.path.clone(),
                name: path.camera.clone(),
                mode: path.mode,
                camera: onboard_camera::Camera::new(config.system_id, component),
                since_ms: (path.mode == Mode::Always).then_some(now_ms),
                by_ground: false,
                space: Space::default(),
                room: true,
            })
            .collect();
        Self {
            system: config.system_id,
            units,
            armed: None,
        }
    }

    /// Whether each path should be recording, in the configuration's order.
    pub fn wanted(&self) -> impl Iterator<Item = (&str, bool)> {
        self.units
            .iter()
            .map(|unit| (unit.path.as_str(), unit.since_ms.is_some()))
    }

    /// The space on a path's recording filesystem, and whether there is
    /// room to record.
    pub fn space(&mut self, index: usize, space: Space, room: bool) {
        if let Some(unit) = self.units.get_mut(index) {
            unit.space = space;
            unit.room = room;
        }
    }

    /// The modes of a reloaded configuration, for the paths it still records
    /// as before, and what to send. The rest of a reload needs a restart.
    pub fn reconfigure(&mut self, config: &Config, now_ms: u64) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for unit in &mut self.units {
            let Some(new) = config
                .paths
                .iter()
                .find(|p| p.path == unit.path && p.camera == unit.name)
            else {
                continue;
            };
            if new.mode == unit.mode {
                continue;
            }
            tracing::info!(path = %unit.path, from = ?unit.mode, to = ?new.mode, "the recording's mode changed");
            unit.mode = new.mode;
            // The ground's recording lasts until its Stop.
            if unit.by_ground {
                continue;
            }
            let on = match new.mode {
                Mode::Always => true,
                Mode::Armed => self.armed == Some(true),
                Mode::Off => false,
            };
            if on != unit.since_ms.is_some() {
                unit.since_ms = on.then_some(now_ms);
                frames.push(status(unit, now_ms));
            }
        }
        frames
    }

    /// A `STATUSTEXT` from a path's camera, in parts when it is long.
    pub fn say(&mut self, index: usize, severity: u8, said: &str) -> Vec<Vec<u8>> {
        self.units
            .get_mut(index)
            .map(|unit| unit.camera.status_text(severity, said))
            .unwrap_or_default()
    }

    /// Each camera's heartbeat and, while it records, its capture status.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for unit in &mut self.units {
            frames.push(unit.camera.heartbeat());
            if unit.since_ms.is_some() {
                frames.push(status(unit, now_ms));
            }
        }
        frames
    }

    /// A frame from the router: the flight controller's heartbeat, or a
    /// command for one of the cameras. What to send back.
    pub fn heard(&mut self, frame: &[u8], now_ms: u64) -> Vec<Vec<u8>> {
        if let Some(armed) = onboard_camera::armed(frame, self.system) {
            return self.arming(armed, now_ms);
        }
        let Some(command) = onboard_camera::command(frame) else {
            return Vec::new();
        };
        if command.to.0 != self.system && command.to.0 != 0 {
            return Vec::new();
        }
        let mut frames = Vec::new();
        for unit in &mut self.units {
            if command.to.1 == 0 || command.to.1 == unit.camera.component {
                frames.extend(obey(unit, &command, now_ms));
            }
        }
        frames
    }

    fn arming(&mut self, armed: bool, now_ms: u64) -> Vec<Vec<u8>> {
        let was = self.armed.replace(armed);
        if was == Some(armed) {
            return Vec::new();
        }
        let mut frames = Vec::new();
        for unit in self.units.iter_mut().filter(|u| u.mode == Mode::Armed) {
            let on = if armed {
                true
            } else if was == Some(true) && !unit.by_ground {
                false
            } else {
                continue;
            };
            if unit.since_ms.is_some() != on {
                unit.since_ms = on.then_some(now_ms);
                tracing::info!(
                    path = %unit.path,
                    "{} recording: the vehicle {}",
                    if on { "starting" } else { "stopping" },
                    if on { "armed" } else { "disarmed" }
                );
                frames.push(status(unit, now_ms));
            }
        }
        frames
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the camera protocol's milliseconds wrap after 49 days"
)]
fn status(unit: &mut Unit, now_ms: u64) -> Vec<u8> {
    let recording = unit
        .since_ms
        .map(|since| now_ms.saturating_sub(since) as u32);
    unit.camera
        .capture_status(now_ms as u32, recording, unit.space)
}

/// A command for this camera, or for every component: its
/// acknowledgement, then what it asked for or changed. A command for
/// every component that this camera does not have is left to the others,
/// and Start and Stop are this camera's only when they name it (R3).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a message id in a float, and milliseconds that wrap after 49 days"
)]
fn obey(unit: &mut Unit, command: &Command, now_ms: u64) -> Vec<Vec<u8>> {
    let mine = command.to.1 == unit.camera.component;
    let time_boot_ms = now_ms as u32;
    match command.command {
        REQUEST_MESSAGE => {
            let asked = command.params[0];
            let message = match asked.round() as u32 {
                CAMERA_INFORMATION => unit.camera.information(time_boot_ms, VENDOR, &unit.name),
                // Its one storage, 1, or every storage, 0.
                STORAGE_INFORMATION if matches!(command.params[1].round() as u32, 0 | 1) => {
                    unit.camera.storage(time_boot_ms, unit.space, &unit.name)
                }
                CAMERA_CAPTURE_STATUS => status(unit, now_ms),
                _ if mine => return vec![unit.camera.ack(command, DENIED)],
                _ => return Vec::new(),
            };
            vec![unit.camera.ack(command, ACCEPTED), message]
        }
        VIDEO_START_CAPTURE | VIDEO_STOP_CAPTURE if !mine => Vec::new(),
        VIDEO_START_CAPTURE => {
            if !unit.room {
                tracing::warn!(path = %unit.path, "the ground asked to record, and the disk is full");
                let said = format!("No room to record {}, the disk is full", unit.name);
                let mut frames = vec![unit.camera.ack(command, TEMPORARILY_REJECTED)];
                frames.extend(unit.camera.status_text(WARNING, &said));
                return frames;
            }
            if unit.since_ms.is_none() {
                tracing::info!(path = %unit.path, "starting recording: the ground asked");
                unit.since_ms = Some(now_ms);
            }
            unit.by_ground = true;
            vec![unit.camera.ack(command, ACCEPTED), status(unit, now_ms)]
        }
        VIDEO_STOP_CAPTURE => {
            if unit.since_ms.take().is_some() {
                tracing::info!(path = %unit.path, "stopping recording: the ground asked");
            }
            unit.by_ground = false;
            vec![unit.camera.ack(command, ACCEPTED), status(unit, now_ms)]
        }
        _ if mine => vec![unit.camera.ack(command, UNSUPPORTED)],
        _ => Vec::new(),
    }
}

/// What the keeper knows of one path in `MediaMTX`.
#[derive(Default)]
struct Kept {
    /// Whether it should record, as last applied.
    applied: Option<bool>,
    /// What it set the record path's own `record` to last.
    alone: Option<bool>,
    /// The segments there were when the current recording started, the
    /// ones it may delete. Told by name rather than by time, as the clock
    /// of a computer with no clock battery may be set mid-flight.
    earlier: Option<HashSet<PathBuf>>,
    /// The last problem said, so it is said once.
    said: Option<String>,
    /// The current recording has filled the disk alone, already said.
    full_said: bool,
    /// Its recording with sound.
    joined: Joined,
}

/// Where a camera's recording with sound comes from.
struct Joining<'a> {
    port: u16,
    rtsp: &'a str,
    gst: &'a Path,
}

/// One camera's joining process and its `<record path>_sound` path.
#[derive(Default)]
struct Joined {
    helper: Option<Helper>,
    backoff: Backoff,
    /// The sound path's `record` as last set or read, None when unknown.
    record: Option<bool>,
    ready: bool,
    /// Why joining stopped, said once.
    said: Option<String>,
}

impl Joined {
    /// Keeps the sound path and its joining process. Whether the sound path
    /// is ready and records in place of the record path.
    async fn keep(
        &mut self,
        api: &Api,
        path: &str,
        sound_path: &str,
        conf: &serde_json::Value,
        joining: &Joining<'_>,
    ) -> Result<bool, String> {
        let now = Instant::now();
        let record = if let Some(sound) = api.path_if_any(sound_path).await? {
            sound.get("record").and_then(serde_json::Value::as_bool)
        } else {
            let mut added: serde_json::Map<String, serde_json::Value> = conf
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, _)| key.starts_with("record") && *key != "record")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            added.insert("record".into(), false.into());
            api.add(sound_path, &serde_json::Value::Object(added))
                .await?;
            tracing::info!(%path, %sound_path, "added the path that records the camera with its sound");
            Some(false)
        };
        if let Some(helper) = &mut self.helper
            && let Some((ran, why)) = helper.ended()
        {
            self.helper = None;
            self.backoff.ended(ran, now);
            self.stopped(path, why);
        }
        if self.helper.is_none() && self.backoff.due(now) {
            let words = sound::joining(joining.rtsp, path, sound_path, joining.port);
            match Helper::spawn(joining.gst, &words) {
                Ok(helper) => {
                    tracing::info!(%path, %sound_path, "joining the camera's sound to its recording");
                    self.helper = Some(helper);
                }
                Err(error) => {
                    self.backoff.ended(Duration::ZERO, now);
                    self.stopped(
                        path,
                        format!("{} cannot be run: {error}", joining.gst.display()),
                    );
                }
            }
        }
        let ready = self.helper.is_some() && api.ready(sound_path).await?;
        if ready != self.ready {
            if ready {
                self.said = None;
            }
            tracing::info!(
                %path,
                %sound_path,
                "recording {}",
                if ready { "with sound" } else { "the picture alone" }
            );
            self.ready = ready;
        }
        if record != Some(ready) {
            api.patch(sound_path, &serde_json::json!({ "record": ready }))
                .await?;
            tracing::info!(path = %sound_path, record = ready, "set the path's recording");
        }
        self.record = Some(ready);
        Ok(ready)
    }

    fn stopped(&mut self, path: &str, why: String) {
        if self.said.as_ref() != Some(&why) {
            tracing::warn!(%path, %why, "cannot join the camera's sound to its recording, so its picture records alone until it can");
            self.said = Some(why);
        }
    }

    /// No recording with sound: the joining process stopped and the sound
    /// path's `record` off, where `MediaMTX` has the path.
    async fn stop(&mut self, api: &Api, sound_path: &str) -> Result<(), String> {
        if self.helper.take().is_some() {
            tracing::info!(%sound_path, "stopped joining the camera's sound to its recording");
        }
        self.backoff.reset();
        self.ready = false;
        self.said = None;
        if self.record == Some(false) {
            return Ok(());
        }
        let on = api
            .path_if_any(sound_path)
            .await?
            .and_then(|conf| conf.get("record").and_then(serde_json::Value::as_bool));
        if on == Some(true) {
            api.patch(sound_path, &serde_json::json!({ "record": false }))
                .await?;
            tracing::info!(path = %sound_path, record = false, "set the path's recording");
        }
        self.record = Some(false);
        Ok(())
    }
}

/// Runs until stopped. `reloaded` hears the configuration read again, whose
/// modes apply at once, and `recording` says the frame rate each camera
/// records at while it does, for the video sender.
///
/// # Errors
/// No UDP socket.
#[allow(
    clippy::too_many_lines,
    reason = "one loop over the router, the timer and reloads, and what each changes"
)]
pub async fn run(
    config: Config,
    mut reloaded: tokio::sync::watch::Receiver<Config>,
    recording: tokio::sync::watch::Sender<BTreeMap<String, f64>>,
    mut sounds: Sounds,
) -> Result<(), String> {
    let api = Api::new(&config.mediamtx_api)?;
    let (host, port) = router(&config.mavlink)?;
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("cannot open a UDP socket: {e}"))?;
    let started = Instant::now();
    let now_ms = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut recorder = Recorder::new(&config, now_ms());
    let mut kept: Vec<Kept> = config.paths.iter().map(|_| Kept::default()).collect();
    let mut other_system: Option<u8> = None;
    let mut unreachable = false;
    // Why each camera has no sound, as the ground last heard it.
    let mut no_sound: BTreeMap<String, String> = BTreeMap::new();
    tracing::info!(
        router = %config.mavlink,
        system = config.system_id,
        paths = ?config.paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        "recording on the vehicle"
    );
    let mut timer = tokio::time::interval(EVERY);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buffer = vec![0_u8; 65_536];
    loop {
        // Resolved each time: a Pi often starts before its network.
        let to = resolve(&host, port);
        let mut out = Vec::new();
        let mut check_disk = false;
        tokio::select! {
            _ = timer.tick() => {
                out.extend(recorder.tick(now_ms()));
                check_disk = true;
            }
            Ok(()) = reloaded.changed() => {
                let new = reloaded.borrow_and_update().clone();
                out.extend(recorder.reconfigure(&new, now_ms()));
            }
            Ok(()) = sounds.status.changed() => {
                let status = sounds.status.borrow_and_update().clone();
                out.extend(say_sound(&mut recorder, &config, &status, &mut no_sound));
            }
            received = socket.recv_from(&mut buffer) => {
                // An error is an earlier send that went nowhere.
                let Ok((length, _)) = received else { continue };
                for frame in split(&buffer[..length]).frames {
                    if other_system.is_none()
                        && let Some(system) = onboard_camera::autopilot(frame)
                        && system != config.system_id
                    {
                        tracing::warn!(heard = system, configured = config.system_id, "a flight controller with another system id is on the router: set system_id in [mavlink] when it is this vehicle's");
                        other_system = Some(system);
                    }
                    out.extend(recorder.heard(frame, now_ms()));
                }
            }
        }
        let wanted: Vec<(String, bool)> = recorder
            .wanted()
            .map(|(path, on)| (path.to_owned(), on))
            .collect();
        for (index, ((path, on), kept)) in wanted.iter().zip(&mut kept).enumerate() {
            if !check_disk && kept.applied == Some(*on) {
                continue;
            }
            let camera = &config.paths[index].camera;
            let port = sounds
                .status
                .borrow()
                .cameras
                .get(camera)
                .and_then(|sound| sound.recording);
            let joining = port.map(|port| Joining {
                port,
                rtsp: &config.mediamtx_rtsp,
                gst: &sounds.gst,
            });
            let said = keep(
                &api,
                path,
                *on,
                kept,
                check_disk,
                &disk::space,
                joining.as_ref(),
            )
            .await;
            if let Ok((fps, _)) = &said {
                recording.send_if_modified(|recording| match fps.filter(|_| *on) {
                    Some(fps) => recording.insert(camera.clone(), fps) != Some(fps),
                    None => recording.remove(camera).is_some(),
                });
            }
            if said.is_ok() && kept.said.take().is_some() {
                tracing::info!(%path, "keeping the recording again");
                out.extend(recorder.say(
                    index,
                    NOTICE,
                    &format!("Recording {camera} is under control again"),
                ));
            }
            match said {
                Ok((_, Some((space, room, notes)))) => {
                    recorder.space(index, space, room);
                    for (severity, note) in notes {
                        out.extend(recorder.say(index, severity, &note));
                    }
                }
                Ok((_, None)) => {}
                Err(error) => {
                    if kept.said.as_ref() != Some(&error) {
                        tracing::warn!(%path, %error, "cannot keep the recording");
                        out.extend(recorder.say(
                            index,
                            WARNING,
                            &format!("Recording {camera} is not under control, see the journal"),
                        ));
                        kept.said = Some(error);
                    }
                }
            }
        }
        let sent = match to {
            Ok(to) => send(&socket, to, &out).await,
            Err(error) => Err(error),
        };
        match sent {
            Ok(()) if unreachable => {
                tracing::info!(router = %config.mavlink, "reaching the MAVLink router again");
                unreachable = false;
            }
            Err(error) if !unreachable => {
                tracing::warn!(router = %config.mavlink, %error, "cannot reach the MAVLink router");
                unreachable = true;
            }
            _ => {}
        }
    }
}

/// Says to the ground, at warning severity, why a camera has no sound when
/// it cannot have any (ADR-0067 M4), once for each reason: from the camera's
/// own recording component, else the first.
fn say_sound(
    recorder: &mut Recorder,
    config: &Config,
    status: &sound::Status,
    said: &mut BTreeMap<String, String>,
) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    for (camera, sound) in &status.cameras {
        let (sound::State::Failed, Some(reason)) = (&sound.state, &sound.reason) else {
            said.remove(camera);
            continue;
        };
        if said.get(camera) == Some(reason) {
            continue;
        }
        said.insert(camera.clone(), reason.clone());
        let index = config
            .paths
            .iter()
            .position(|p| &p.camera == camera)
            .unwrap_or(0);
        frames.extend(recorder.say(index, WARNING, &format!("No sound from {camera}. {reason}")));
    }
    frames
}

/// Notes for the ground: each one's severity and words.
type Notes = Vec<(u8, String)>;

/// Sets `record` to `on` when `MediaMTX` has it otherwise, and with
/// `check_disk` reads the disk's `space` and makes room for the current
/// recording. The path's frame rate, and with `check_disk` the disk's space,
/// room and notes for the ground, warnings when it deleted earlier
/// recordings or the disk is nearly full.
async fn keep(
    api: &Api,
    path: &str,
    on: bool,
    kept: &mut Kept,
    check_disk: bool,
    space: &(dyn Fn(&Path) -> std::io::Result<Space> + Sync),
    joining: Option<&Joining<'_>>,
) -> Result<(Option<f64>, Option<(Space, bool, Notes)>), String> {
    let conf = api.path(path).await?;
    let fps = conf
        .get("rpiCameraFPS")
        .and_then(serde_json::Value::as_f64)
        .filter(|fps| *fps > 0.0);
    let sound_path = format!("{path}_sound");
    let with_sound = match joining {
        Some(joining) if on => kept
            .joined
            .keep(api, path, &sound_path, &conf, joining)
            .await
            .unwrap_or_else(|why| {
                kept.joined.stopped(path, why);
                false
            }),
        _ => {
            kept.joined.stop(api, &sound_path).await?;
            false
        }
    };
    let (active, other) = if with_sound {
        (sound_path.as_str(), path)
    } else {
        (path, sound_path.as_str())
    };
    // The segments of both, which are this camera's recordings.
    let store = disk::Store::of(active, &conf).map(|mut store| {
        if let Ok(other) = disk::Store::of(other, &conf) {
            store.also.push(other.dir);
        }
        store
    });
    if !on {
        kept.earlier = None;
    } else if kept.earlier.is_none() {
        // Listed before it records, so none of its own segments is there.
        let earlier = match &store {
            Ok(store) => store
                .earlier()
                .map_err(|e| format!("cannot read {}: {e}", store.dir.display()))?,
            Err(_) => HashSet::new(),
        };
        kept.earlier = Some(earlier);
        kept.full_said = false;
    }
    let record = conf.get("record").and_then(serde_json::Value::as_bool);
    let alone = on && !with_sound;
    if record != Some(alone) {
        if kept.alone == Some(alone) {
            tracing::info!(%path, "MediaMTX lost the recording's state, restarted or changed: setting it again");
        }
        api.patch(path, &serde_json::json!({ "record": alone }))
            .await?;
        tracing::info!(%path, record = alone, "set the path's recording");
    }
    kept.alone = Some(alone);
    kept.applied = Some(on);
    if !check_disk {
        return Ok((fps, None));
    }
    let store = store?;
    let mut notes = Vec::new();
    if let Some(earlier) = &kept.earlier {
        let made = store
            .make_room(earlier, space)
            .map_err(|e| format!("cannot make room in {}: {e}", store.dir.display()))?;
        if made.deleted > 0 {
            let said = format!(
                "Deleted {} of earlier recordings to keep recording, {} free",
                disk::size(made.deleted),
                disk::size(made.space.free)
            );
            tracing::warn!(%path, files = made.files, deleted = made.deleted, free = made.space.free, "{said}");
            notes.push((WARNING, said));
        }
        if made.short && !kept.full_said {
            let said = format!(
                "Disk nearly full, {} free and only this recording left",
                disk::size(made.space.free)
            );
            tracing::warn!(%path, free = made.space.free, reserve = store.reserve, "{said}. Recording goes on until the disk is full");
            notes.push((WARNING, said));
            kept.full_said = true;
        }
    }
    let space = space(&store.dir)
        .map_err(|e| format!("cannot read the free space of {}: {e}", store.dir.display()))?;
    let room = store
        .room(space, kept.earlier.as_ref())
        .map_err(|e| format!("cannot read {}: {e}", store.dir.display()))?;
    if !room && notes.is_empty() && kept.earlier.is_none() && !kept.full_said {
        tracing::warn!(%path, free = space.free, reserve = store.reserve, "no room to record");
        notes.push((
            WARNING,
            format!("No room to record, {} free", disk::size(space.free)),
        ));
        kept.full_said = true;
    }
    Ok((fps, Some((space, room, notes))))
}

fn resolve(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("cannot resolve {host}")))
}

async fn send(socket: &UdpSocket, to: SocketAddr, frames: &[Vec<u8>]) -> std::io::Result<()> {
    for frame in frames {
        socket.send_to(frame, to).await?;
    }
    Ok(())
}

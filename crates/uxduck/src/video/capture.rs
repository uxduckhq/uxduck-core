//! Local cameras belong to the daemon, including when every UI has closed.
//! The separately packaged `FFmpeg` runtime owns capture, decoding and encoding.
//! Only MPEG-TS comes back over a pipe; no raw pictures or GPU handles enter
//! the process that owns vehicle links. The runtime is built from pinned source.
//!
//! A camera can record a sound input chosen with it (S148): the helper
//! encodes it to AAC into the same MPEG-TS, which S147 records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uxduck_contracts::dto::video::{CaptureDevice, VideoDevicesResult};
use uxduck_core::domain::video::{CaptureInput, CaptureStandard};
use uxduck_media::source::Source;

use super::{hub::Hub, listener};
use crate::daemon::engine_task::EngineHandle;

const MAX_DIAGNOSTIC: usize = 8192;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const DEVICE_CACHE_AGE: Duration = Duration::from_secs(3);

/// Device enumeration is lazy and cached. The engine never waits for a driver.
#[derive(Debug, Default)]
pub struct Devices {
    state: Mutex<(VideoDevicesResult, Option<Instant>)>,
}

impl Devices {
    pub fn wants_sound(&self, input: &CaptureInput) -> bool {
        if input.no_sound {
            return false;
        }
        if input.record_sound {
            return true;
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .0
            .devices
            .iter()
            .find(|d| d.id == input.device_id)
            .and_then(|camera| own_microphone(camera, &state.0.sound_inputs))
            .is_some()
    }
    pub fn list(self: &Arc<Self>, runtime: &tokio::runtime::Handle) -> VideoDevicesResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.0.scanning && state.1.is_none_or(|at| at.elapsed() >= DEVICE_CACHE_AGE) {
            state.0.scanning = true;
            let shared = self.clone();
            runtime.spawn(async move {
                let result = discover().await;
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.0 = match result {
                    Ok(found) => VideoDevicesResult {
                        devices: found.cameras,
                        sound_inputs: found.sound_inputs,
                        scanning: false,
                        error: None,
                    },
                    Err(error) => VideoDevicesResult {
                        error: Some(error),
                        ..VideoDevicesResult::default()
                    },
                };
                state.1 = Some(Instant::now());
            });
        }
        state.0.clone()
    }

    /// The picker has often just enumerated these devices. Reuse a successful
    /// scan within its existing freshness window instead of asking the driver
    /// again on Connect. Expired/missing devices still take the discovery path.
    fn recent(&self, device_id: &str) -> Option<Discovered> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.0.error.is_some()
            || state.1.is_none_or(|at| at.elapsed() >= DEVICE_CACHE_AGE)
            || !state.0.devices.iter().any(|d| d.id == device_id)
        {
            return None;
        }
        Some(Discovered {
            cameras: state.0.devices.clone(),
            sound_inputs: state.0.sound_inputs.clone(),
        })
    }

    /// # Errors
    /// A missing helper, failed discovery or a device not in the catalogue.
    pub fn check(&self, input: &CaptureInput) -> Result<(), String> {
        program()?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.0.error {
            return Err(error.clone());
        }
        if !state.0.devices.iter().any(|d| d.id == input.device_id) {
            return Err(
                "This camera is not in the device list. Refresh the list and choose it again."
                    .into(),
            );
        }
        if let Some(sound) = input
            .sound_input
            .as_ref()
            .filter(|_| input.record_sound && !input.no_sound)
            && !state.0.sound_inputs.iter().any(|d| d.id == sound.device_id)
        {
            return Err(
                "This sound input is not in the device list. Refresh the list and choose it again."
                    .into(),
            );
        }
        Ok(())
    }
}

/// Packaged installs carry the runtime beside the daemon; source builds use
/// the same verified runtime in target/capture. Never silently use a PATH binary.
/// # Errors
/// A missing or invalid capture executable, with a repair command.
pub fn program() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Some(path) = std::env::var_os("UXDUCK_CAPTURE_FFMPEG") {
        let path = PathBuf::from(path);
        return path
            .is_file()
            .then_some(path)
            .ok_or_else(|| "UXDUCK_CAPTURE_FFMPEG does not point to a capture executable.".into());
    }
    let exe = crate::paths::executable().map_err(|e| e.to_string())?;
    if let Some(parent) = exe.parent() {
        let path = parent.join("capture").join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    if cfg!(target_os = "linux") && exe == Path::new("/usr/bin/uxduck") {
        let path = PathBuf::from("/usr/lib/uxduck/capture/ffmpeg");
        if path.is_file() {
            return Ok(path);
        }
    }
    // An installed build must never use a checkout left on the build machine.
    if cfg!(debug_assertions) {
        let target = format!(
            "{}-{}",
            std::env::consts::ARCH,
            if cfg!(target_os = "macos") {
                "apple-darwin"
            } else if cfg!(windows) {
                "pc-windows-msvc"
            } else {
                "unknown-linux-gnu"
            }
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/capture")
            .join(target)
            .join("capture")
            .join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("The local camera capture helper is missing. Reinstall UXDuck, or build it from source with pnpm capture:build.".into())
}

pub(super) fn command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.kill_on_drop(true).stdin(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // no console window
    command
}

/// Drain diagnostics even during capture, keeping bounded memory.
pub(super) async fn tail(mut reader: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut chunk = [0; 2048];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        kept.extend_from_slice(&chunk[..n]);
        if kept.len() > MAX_DIAGNOSTIC {
            kept.drain(..kept.len() - MAX_DIAGNOSTIC);
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

async fn output(program: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = command(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("The camera capture helper could not start: {e}. Rebuild or reinstall it.")
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or("The capture helper has no output pipe")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("The capture helper has no diagnostic pipe")?;
    let read = async { tokio::join!(tail(stdout), tail(stderr), child.wait()) };
    let (out, err, _) = tokio::time::timeout(DISCOVERY_TIMEOUT, read)
        .await
        .map_err(|_| {
            "Camera discovery timed out. Check camera permissions and the device's driver."
                .to_owned()
        })?;
    Ok(format!("{out}\n{err}"))
}

/// What discovery found: cameras, and sound inputs for them (S148).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub cameras: Vec<CaptureDevice>,
    pub sound_inputs: Vec<CaptureDevice>,
}

/// Deliberately omit screens and numeric enumeration indices; cameras and
/// sound inputs by their exact unique IDs.
fn mac_devices(text: &str) -> Result<Discovered, String> {
    let mut in_video = false;
    let mut in_audio = false;
    let mut legacy = false;
    let mut found = BTreeMap::new();
    let mut sound = BTreeMap::new();
    for line in text.lines() {
        if line.contains("AVFoundation video devices:") {
            in_video = true;
            continue;
        }
        if line.contains("AVFoundation audio devices:") {
            in_audio = true;
            continue;
        }
        if !in_video {
            continue;
        }
        let Some((name, uid)) = line.rsplit_once("  [uid:") else {
            legacy |= !in_audio && line.contains("] [") && !line.contains("Capture screen");
            continue;
        };
        let Some(uid) = uid.strip_suffix(']') else {
            continue;
        };
        let Some((_, name)) = name.rsplit_once("] ") else {
            continue;
        };
        let id = format!("avfoundation:{uid}");
        let device = CaptureDevice {
            id: id.clone(),
            name: name.trim().into(),
            persistent: true,
        };
        if in_audio {
            sound.insert(id, device);
        } else {
            found.insert(id, device);
        }
    }
    if !in_video {
        return Err(problem(
            text,
            "UXDuck could not list this computer's cameras",
        ));
    }
    if found.is_empty() && legacy {
        return Err("This FFmpeg cannot identify cameras safely. Use UXDuck's bundled capture helper, or run pnpm capture:build.".into());
    }
    Ok(Discovered {
        cameras: found.into_values().collect(),
        sound_inputs: sound.into_values().collect(),
    })
}

fn windows_devices(text: &str) -> Result<Discovered, String> {
    // The device being read: its name, and whether it is a sound input.
    let mut named: Option<(String, bool)> = None;
    let mut devices = Vec::new();
    let mut sound_inputs = Vec::new();
    for line in text.lines() {
        let line = line.split_once("] ").map_or(line, |(_, rest)| rest).trim();
        if let Some(name) = line
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix("\" (video)"))
        {
            named = Some((name.to_owned(), false));
        } else if let Some(name) = line
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix("\" (audio)"))
        {
            named = Some((name.to_owned(), true));
        } else if let Some(id) = line
            .strip_prefix("Alternative name \"")
            .and_then(|s| s.strip_suffix('"'))
            && let Some((name, sound)) = named.take()
        {
            let device = CaptureDevice {
                id: format!("dshow:{id}"),
                name,
                persistent: true,
            };
            if sound {
                sound_inputs.push(device);
            } else {
                devices.push(device);
            }
        }
    }
    if devices.is_empty()
        && !text.contains("Could not enumerate video devices")
        && !text.contains("(audio)")
    {
        return Err(problem(
            text,
            "UXDuck could not list this computer's cameras",
        ));
    }
    Ok(Discovered {
        cameras: devices,
        sound_inputs,
    })
}

fn linux_devices(text: &str, aliases: &BTreeMap<PathBuf, PathBuf>) -> Vec<CaptureDevice> {
    text.lines()
        .filter_map(|line| {
            let start = line.find("/dev/video")?;
            let rest = &line[start..];
            let (path, name) = rest.split_once(" [")?;
            let path = PathBuf::from(path.trim());
            let alias = aliases.get(&path).unwrap_or(&path);
            Some(CaptureDevice {
                id: format!("v4l2:{}", alias.display()),
                name: name.split(']').next().unwrap_or(name).to_owned(),
                persistent: alias != &path,
            })
        })
        .collect()
}

/// ALSA's configured sound servers and capture devices. Hardware uses card
/// names, which survive replugging, through `plughw` for format conversion.
/// The server routes share the microphone with other desktop applications.
fn alsa_devices(text: &str) -> Vec<CaptureDevice> {
    let mut devices: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches('*').trim();
            let (id, description) = line.split_once(" [")?;
            // FFmpeg appends a format such as `(none)` after the closing `]`.
            let name = description
                .rsplit_once(']')
                .map_or(description, |(name, _)| name)
                .trim();
            let (id, name) = match id {
                "pipewire" => (
                    "pipewire".to_owned(),
                    "System microphone (PipeWire)".to_owned(),
                ),
                "pulse" => (
                    "pulse".to_owned(),
                    "System microphone (PulseAudio)".to_owned(),
                ),
                "default" => ("default".to_owned(), "System default microphone".to_owned()),
                _ => {
                    let card = id.strip_prefix("hw:CARD=")?;
                    let name = if name.is_empty()
                        || name == "Direct hardware device without any conversions"
                    {
                        format!("ALSA microphone ({card})")
                    } else {
                        name.to_owned()
                    };
                    (format!("plughw:CARD={card}"), name)
                }
            };
            Some(CaptureDevice {
                id: format!("alsa:{id}"),
                name,
                persistent: true,
            })
        })
        .collect();
    // A named server route is clearer than a second alias to the same input.
    if devices
        .iter()
        .any(|d| matches!(d.id.as_str(), "alsa:pipewire" | "alsa:pulse"))
    {
        devices.retain(|d| d.id != "alsa:default");
    }
    devices
}

async fn discover() -> Result<Discovered, String> {
    let program = program()?;
    if cfg!(target_os = "macos") {
        mac_devices(
            &output(
                &program,
                &[
                    "-hide_banner",
                    "-f",
                    "avfoundation",
                    "-list_devices",
                    "true",
                    "-i",
                    "",
                ],
            )
            .await?,
        )
    } else if cfg!(windows) {
        windows_devices(
            &output(
                &program,
                &[
                    "-hide_banner",
                    "-f",
                    "dshow",
                    "-list_devices",
                    "true",
                    "-i",
                    "dummy",
                ],
            )
            .await?,
        )
    } else {
        let text = output(&program, &["-hide_banner", "-sources", "video4linux2"]).await?;
        if !text.contains("video4linux2") {
            return Err(problem(
                &text,
                "The capture helper has no Video4Linux support",
            ));
        }
        let mut aliases = BTreeMap::new();
        // Prefer serial identity, then physical port, to /dev/videoN.
        for dir in ["/dev/v4l/by-path", "/dev/v4l/by-id"] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
                entries.sort();
                for alias in entries {
                    if let Ok(path) = alias.canonicalize() {
                        aliases.insert(path, alias);
                    }
                }
            }
        }
        // A computer without ALSA's devices still has its cameras.
        let sound = output(&program, &["-hide_banner", "-sources", "alsa"])
            .await
            .unwrap_or_default();
        Ok(Discovered {
            cameras: linux_devices(&text, &aliases),
            sound_inputs: alsa_devices(&sound),
        })
    }
}

fn problem(text: &str, fallback: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("not a video capture device") {
        return "This Linux device does not send pictures. It may be the webcam's metadata device. Remove this entry in Camera settings, then add the camera again and choose its video device.".into();
    }
    if lower.contains("permission") || lower.contains("not authorized") || lower.contains("denied")
    {
        return "Camera access was denied. Allow UXDuck in your system's camera privacy settings (on Linux, check device permissions), then resume capture.".into();
    }
    if lower.contains("busy") || lower.contains("already in use") {
        return "The camera is in use. Close the other program using it, then resume capture."
            .into();
    }
    let details = text
        .lines()
        .rev()
        .filter(|l| {
            !l.trim().is_empty() && !l.contains("Exiting with exit code") && !l.starts_with("[out#")
        })
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{fallback}. {}",
        details.chars().take(600).collect::<String>()
    )
}

fn disconnected(device_id: &str) -> &'static str {
    if device_id.starts_with("v4l2:") {
        "This Linux camera is unavailable. Reconnect it, or remove this entry in Camera settings and add the camera again. An older entry may point to the webcam's metadata device, which does not send pictures."
    } else {
        "Camera disconnected. Reconnect the selected camera. UXDuck will resume capture automatically."
    }
}

/// When the helper fails with a sound input, what to say about it, if it
/// was the sound input's fault (S148).
fn sound_problem(text: &str, sound: &CaptureDevice) -> Option<String> {
    let lower = text.to_lowercase();
    if lower.contains("sound input is not connected") {
        return Some(format!(
            "The sound input {} is not connected. Reconnect it, or choose No sound under Sound.",
            sound.name
        ));
    }
    let about = lower.contains("audio") || lower.contains(&sound.name.to_lowercase());
    let refused = [
        "permission",
        "not authorized",
        "denied",
        "failed to create",
        "cannot use",
    ]
    .iter()
    .any(|w| lower.contains(w));
    (about && refused).then(|| {
        format!(
            "UXDuck couldn't open the sound input {}. Allow UXDuck in your system's microphone privacy settings (on Linux, check the device's permissions), or choose No sound under Sound.",
            sound.name
        )
    })
}

/// The helper's arguments: the camera, and with `sound` its sound input,
/// encoded to AAC beside the pictures (S148).
fn args(
    input: &CaptureInput,
    device: &CaptureDevice,
    sound: Option<&CaptureDevice>,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-nostdin",
    ]
    .map(Into::into)
    .to_vec();
    // Two pictures at most wait for the encoder; sound's small packets
    // share the queue, so it is a little longer with them.
    let queue = if sound.is_some() { "8" } else { "2" };
    args.extend(["-thread_queue_size".into(), queue.into()]);
    let sound_id = sound
        .map(|s| s.id.split_once(':').ok_or("Invalid sound input identity"))
        .transpose()?;
    let (size, rate) = match input.standard {
        CaptureStandard::Pal => (Some((720, 576)), Some("25".into())),
        CaptureStandard::Ntsc => (Some((720, 480)), Some("30000/1001".into())),
        CaptureStandard::Auto => (
            input.width.zip(input.height),
            input.frame_rate.map(|r| r.to_string()),
        ),
    };
    let (backend, id) = device.id.split_once(':').ok_or("Invalid camera identity")?;
    args.extend(["-f".into(), backend.into()]);
    if let Some((w, h)) = size {
        args.extend(["-video_size".into(), format!("{w}x{h}")]);
    }
    if let Some(rate) = rate {
        args.extend(["-framerate".into(), rate]);
    }
    if backend == "avfoundation"
        && input.standard == CaptureStandard::Auto
        && input.frame_rate.is_none()
    {
        args.extend(["-framerate".into(), "0".into()]);
    }
    let other = || Err("This sound input belongs to another operating system.".to_owned());
    match backend {
        "avfoundation" => {
            args.extend(["-video_device_id".into(), id.into()]);
            match sound_id {
                Some(("avfoundation", sound)) => {
                    args.extend(["-audio_device_id".into(), sound.into()]);
                }
                Some(_) => return other(),
                None => {}
            }
            args.extend([
                "-pixel_format".into(),
                "nv12".into(),
                "-i".into(),
                "none:none".into(),
            ]);
        }
        "dshow" => match sound_id {
            Some(("dshow", sound)) => {
                args.extend(["-i".into(), format!("video={id}:audio={sound}")]);
            }
            Some(_) => return other(),
            None => args.extend(["-i".into(), format!("video={id}")]),
        },
        "v4l2" => {
            if input.standard != CaptureStandard::Auto {
                args.extend([
                    "-standard".into(),
                    if input.standard == CaptureStandard::Pal {
                        "pal"
                    } else {
                        "ntsc"
                    }
                    .into(),
                ]);
            }
            args.extend(["-i".into(), id.into()]);
            // ALSA's sound is a second input, with a longer queue of its own.
            match sound_id {
                Some(("alsa", sound)) => args.extend(
                    ["-thread_queue_size", "64", "-f", "alsa", "-i", sound].map(Into::into),
                ),
                Some(_) => return other(),
                None => {}
            }
        }
        _ => return Err("This camera belongs to another operating system.".into()),
    }
    args.extend(outputs(input, sound_id.map(|(backend, _)| backend)));
    args.extend(encoding_args());
    Ok(args)
}

/// What goes out: the picture, mapped and filtered, and the sound, from
/// the input its backend puts it in, as AAC (S148), or none.
fn outputs(input: &CaptureInput, sound: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["-map".into(), "0:v:0".into()];
    match sound {
        Some("alsa") => args.extend(["-map".into(), "1:a:0".into()]),
        Some(_) => args.extend(["-map".into(), "0:a:0".into()]),
        None => args.push("-an".into()),
    }
    args.extend(["-sn", "-dn"].map(Into::into));
    let mut filters = Vec::new();
    if input.deinterlace {
        filters.push("bwdif=mode=send_field:parity=auto:deint=all");
    }
    match input.standard {
        CaptureStandard::Pal => filters.push("scale=768:576,setsar=1"),
        CaptureStandard::Ntsc => filters.push("scale=640:480,setsar=1"),
        CaptureStandard::Auto => filters.push("scale=trunc(iw*sar/2)*2:trunc(ih/2)*2,setsar=1"),
    }
    args.extend(["-vf".into(), filters.join(",")]);
    if sound.is_some() {
        // AAC in the MPEG-TS, at 48 kHz, with the input's own channels.
        args.extend(["-c:a", "aac", "-b:a", "128k", "-ar", "48000"].map(Into::into));
    }
    args
}

pub(super) fn encoding_args() -> Vec<String> {
    let mut args = Vec::new();
    args.extend(
        [
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-threads",
            "2",
        ]
        .map(Into::into),
    );
    args.extend(
        [
            "-pix_fmt",
            "yuv420p",
            "-b:v",
            "6000k",
            "-bf",
            "0",
            "-g",
            "30",
            "-flush_packets",
            "1",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-mpegts_flags",
            "+resend_headers",
            "-f",
            "mpegts",
            "pipe:1",
        ]
        .map(Into::into),
    );
    args
}

#[allow(
    clippy::too_many_arguments,
    reason = "one capture's source, its hub, engine, permission and devices"
)]
pub fn spawn(
    runtime: &tokio::runtime::Handle,
    id: String,
    input: CaptureInput,
    epoch: u64,
    hub: Arc<Hub>,
    engine: EngineHandle,
    selection: Option<super::permission::Receiver>,
    devices: Arc<Devices>,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        tracing::info!(source = %id, "starting local camera capture");
        let mut permission_started = Instant::now();
        let mut backoff = 1u64;
        // Why the camera's own microphone, picked by itself, couldn't be
        // opened: then the pictures go on without it (S161).
        let mut own_refused: Option<String> = None;
        let mut first = if let Some(mut answer) = selection {
            let report_id = id.clone();
            let config = input.clone();
            engine.post(move |e| e.capture_stopped(&report_id, &config, epoch,
                "Waiting for macOS permission. Respond to the camera or microphone question before video starts.".into()));
            Some(super::permission::selected(&mut answer).await)
        } else { None };
        loop {
            // Reconnects, recording policy and restored sources only check access.
            let access = match first.take() {
                Some(answer) => answer,
                None => super::permission::query(false, false).await,
            };
            let access = match access.and_then(|access| {
                access.camera_error().map_or(Ok(access), Err)
            }) {
                Ok(access) => access,
                Err(why) => {
                    tracing::info!(source = %id, permission_ms = permission_started.elapsed().as_millis(), "local camera permission check failed");
                    let report_id = id.clone();
                    let config = input.clone();
                    engine.post(move |e| {
                        e.capture_permission_issue(&report_id, &config, epoch);
                        e.capture_stopped(&report_id, &config, epoch, why);
                    });
                    return;
                }
            };
            tracing::info!(source = %id, permission_ms = permission_started.elapsed().as_millis(), "local camera permission ready");
            let (result, refused) =
                run(&id, &input, epoch, &hub, &engine, own_refused.as_deref(), &access, &devices).await;
            if refused.is_some() {
                own_refused = refused;
                backoff = 0;
            }
            let report_id = id.clone();
            let config = input.clone();
            engine.post(move |e| e.capture_stopped(&report_id, &config, epoch, result));
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).clamp(1, 15);
            permission_started = Instant::now();
        }
    })
}

/// Words that say what a device is rather than which one it is.
const KIND_WORDS: &[&str] = &[
    "camera",
    "webcam",
    "video",
    "microphone",
    "mic",
    "audio",
    "input",
    "capture",
    "usb",
    "hd",
    "built-in",
    "integrated",
    "internal",
];

fn naming_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty() && !KIND_WORDS.contains(w))
        .map(str::to_owned)
        .collect()
}

/// The camera's own microphone (S161): the sound input whose name starts
/// with the same words as the camera's, once what each is ("Camera",
/// "Microphone", "USB"…) is set aside, so "`MacBook` Pro Microphone" for
/// "`MacBook` Pro Camera" and "Storm Microphone" for "Storm Camera": every
/// word of the shorter name, in order, so a "Logitech G Pro" headset is not a
/// "Logitech BRIO"'s. The one sharing the most. Most cameras have none, so
/// most get no sound; matching by the USB device behind both would find more.
#[must_use]
pub fn own_microphone<'a>(
    camera: &CaptureDevice,
    inputs: &'a [CaptureDevice],
) -> Option<&'a CaptureDevice> {
    let theirs = naming_words(&camera.name);
    let shared = |input: &CaptureDevice| {
        let ours = naming_words(&input.name);
        let n = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
        // All of the shorter name, or no match.
        if n == ours.len().min(theirs.len()) {
            n
        } else {
            0
        }
    };
    // The first listed of those sharing the most.
    inputs
        .iter()
        .map(|input| (shared(input), input))
        .filter(|(n, _)| *n > 0)
        .fold(
            None,
            |best: Option<(usize, &CaptureDevice)>, (n, input)| match best {
                Some((most, _)) if most >= n => best,
                _ => Some((n, input)),
            },
        )
        .map(|(_, input)| input)
}

/// Why a run stopped, and why the camera's own microphone couldn't be used
/// when that was the reason.
struct Stopped(String, Option<String>);

impl From<String> for Stopped {
    fn from(why: String) -> Self {
        Self(why, None)
    }
}

impl From<&str> for Stopped {
    fn from(why: &str) -> Self {
        Self(why.to_owned(), None)
    }
}

/// One run of the capture helper until it stops: why it stopped, and, when
/// the camera's own microphone picked by itself was what failed, why, so the
/// next run goes on without it.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one run of the helper, start to stop"
)]
async fn run(
    id: &str,
    input: &CaptureInput,
    epoch: u64,
    hub: &Hub,
    engine: &EngineHandle,
    own_refused: Option<&str>,
    access: &super::permission::Access,
    devices: &Devices,
) -> (String, Option<String>) {
    let started = Instant::now();
    let automatic = !input.no_sound && !input.record_sound;
    let result = async {
        let program = program()?;
        let cached = devices.recent(&input.device_id);
        let discovery_cached = cached.is_some();
        let found = match cached {
            Some(found) => found,
            None => discover().await?,
        };
        let discovery_ms = started.elapsed().as_millis();
        let device = found.cameras.into_iter().find(|d| d.id == input.device_id).ok_or_else(|| disconnected(&input.device_id))?;
        // A sound input that is missing leaves the pictures playing, and says so.
        let (mut sound, mut note) = match input.sound_input.as_ref().filter(|_| input.record_sound && !input.no_sound) {
            // Not chosen: the camera's own microphone, when it has one (S161).
            None if !automatic => (None, None),
            None => match own_refused {
                Some(why) => (None, Some(why.to_owned())),
                None => (own_microphone(&device, &found.sound_inputs).cloned(), None),
            },
            Some(chosen) => match found.sound_inputs.into_iter().find(|d| d.id == chosen.device_id) {
                Some(device) => (Some(device), None),
                None => (None, Some(format!("The sound input {} is not connected, so recordings have no sound. The video plays.", chosen.name))),
            },
        };
        if sound.is_some() && let Some(why) = access.microphone_note() {
            sound = None;
            note = Some(why);
            let report_id = id.to_owned();
            let config = input.clone();
            engine.post(move |e| e.capture_permission_issue(&report_id, &config, epoch));
        }
        let mut child = command(&program).args(args(input, &device, sound.as_ref())?).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("The camera capture helper could not start: {e}"))?;
        let helper_started = Instant::now();
        tracing::info!(source = id, discovery_ms, discovery_cached, "local camera capture helper started");
        let mut stdout = child.stdout.take().ok_or("The capture helper has no output pipe")?;
        let stderr = child.stderr.take().ok_or("The capture helper has no diagnostic pipe")?;
        let diagnostics = tokio::spawn(tail(stderr));
        let mut source = Source::new();
        let mut buffer = vec![0; 188 * 128];
        let mut pending = Vec::new();
        let mut reported = Instant::now();
        let mut before = (0u64, 0u64);
        let mut startup = true;
        let mut last_picture = Instant::now();
        let mut pictures = 0;
        let mut first_bytes_ms = None;
        let failure = loop {
            let limit = if startup { Duration::from_secs(15) } else { Duration::from_secs(5) };
            let timeout = limit.saturating_sub(last_picture.elapsed());
            let n = match tokio::time::timeout(timeout, stdout.read(&mut buffer)).await {
                Ok(Ok(0)) => break "The camera stopped sending video",
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break "The camera capture pipe closed",
                Err(_) => break "No video arrived from the camera. Check its connection, driver and selected video mode",
            };
            first_bytes_ms.get_or_insert_with(|| helper_started.elapsed().as_millis());
            pending.extend_from_slice(&buffer[..n]);
            let whole = pending.len() / 188 * 188;
            if whole == 0 { continue; }
            hub.publish_ts(id, &pending[..whole]);
            for output in source.push(&pending[..whole]) { hub.publish(id, &listener::item(output)); }
            pending.drain(..whole);
            let status = source.status();
            if status.counters.pictures > pictures {
                last_picture = Instant::now();
                pictures = status.counters.pictures;
            }
            if status.counters.pictures > 0 && (startup || reported.elapsed() >= Duration::from_secs(1)) {
                if startup {
                    tracing::info!(source = id, discovery_ms, first_bytes_ms, first_picture_ms = helper_started.elapsed().as_millis(), startup_ms = started.elapsed().as_millis(), "local camera received its first picture");
                }
                let mut report = listener::report(0, &std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &status, reported.elapsed().as_secs_f64().max(0.001), before.0, before.1);
                report.id = Some(id.to_owned());
                report.sender = "local capture".into();
                report.counters.hold_us = hub.hold_us(id);
                report.note.clone_from(&note);
                before = (status.counters.bytes, status.counters.pictures);
                reported = Instant::now();
                startup = false;
                let config = input.clone();
                engine.post(move |e| e.capture_observed(epoch, &config, report));
            }
        };
        let _ = child.kill().await;
        let _ = child.wait().await;
        let diagnostics = diagnostics.await.unwrap_or_default();
        Err::<(), Stopped>(
            match sound.as_ref().and_then(|s| sound_problem(&diagnostics, s)) {
                // Picked by itself: go on without it, and say so.
                Some(why) if automatic => {
                    let note = format!(
                        "{} Recordings have no sound. The video plays.",
                        why.split(" Allow UXDuck").next().unwrap_or(&why)
                    );
                    Stopped(why, Some(note))
                }
                Some(why) => Stopped(why, None),
                None => Stopped(problem(&diagnostics, failure), None),
            },
        )
    }.await;
    match result {
        Err(Stopped(why, refused)) => {
            tracing::info!(source = id, elapsed_ms = started.elapsed().as_millis(), reason = %why, "local camera capture stopped");
            (why, refused)
        }
        Ok(()) => ("Camera capture ended".into(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_reuses_recent_discovery_but_rescans_missing_expired_or_failed_devices() {
        let camera = CaptureDevice {
            id: "avfoundation:camera".into(),
            name: "Camera".into(),
            persistent: true,
        };
        let devices = Devices {
            state: Mutex::new((
                VideoDevicesResult {
                    devices: vec![camera.clone()],
                    sound_inputs: vec![CaptureDevice {
                        id: "avfoundation:mic".into(),
                        name: "Microphone".into(),
                        persistent: true,
                    }],
                    ..VideoDevicesResult::default()
                },
                Some(Instant::now()),
            )),
        };
        let found = devices.recent(&camera.id).unwrap();
        assert_eq!(found.cameras, vec![camera.clone()]);
        assert_eq!(found.sound_inputs[0].id, "avfoundation:mic");
        assert!(devices.recent("avfoundation:newly-connected").is_none());
        devices.state.lock().unwrap().1 = Instant::now().checked_sub(DEVICE_CACHE_AGE);
        assert!(devices.recent(&camera.id).is_none());
        devices.state.lock().unwrap().1 = None;
        assert!(devices.recent(&camera.id).is_none());
        {
            let mut state = devices.state.lock().unwrap();
            state.1 = Some(Instant::now());
            state.0.error = Some("The driver could not enumerate devices".into());
        }
        assert!(devices.recent(&camera.id).is_none());
    }

    #[test]
    fn identity_ignores_indices_names_screens_and_audio() {
        let text = "[AVFoundation indev @ a] AVFoundation video devices:\n[AVFoundation indev @ a] [0] USB Video  [uid:usb-A]\n[AVFoundation indev @ a] [1] USB Video  [uid:usb-B]\n[AVFoundation indev @ a] [2] Capture screen 0\n[AVFoundation indev @ a] AVFoundation audio devices:\n[AVFoundation indev @ a] [0] Microphone  [uid:mic]";
        let found = mac_devices(text).unwrap();
        let devices = &found.cameras;
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "avfoundation:usb-A");
        assert_eq!(devices[0].name, devices[1].name);
        // Microphones are sound inputs, never cameras (S148).
        assert_eq!(found.sound_inputs.len(), 1);
        assert_eq!(found.sound_inputs[0].id, "avfoundation:mic");
        assert_eq!(found.sound_inputs[0].name, "Microphone");
        assert!(mac_devices("[AVFoundation indev @ a] AVFoundation video devices:\n[AVFoundation indev @ a] [0] Old name-only camera\n[AVFoundation indev @ a] AVFoundation audio devices:").is_err());
        let win = windows_devices("[dshow @ a] \"USB Video\" (video)\n[dshow @ a]   Alternative name \"@device_pnp_camera-A\"\n[dshow @ a] \"USB Audio\" (audio)\n[dshow @ a]   Alternative name \"@device_audio\"").unwrap();
        assert_eq!(win.cameras.len(), 1);
        assert_eq!(win.cameras[0].id, "dshow:@device_pnp_camera-A");
        assert_eq!(win.sound_inputs.len(), 1);
        assert_eq!(win.sound_inputs[0].id, "dshow:@device_audio");
        assert_eq!(win.sound_inputs[0].name, "USB Audio");
    }

    #[test]
    fn linux_prefers_a_serial_path_to_a_device_number() {
        let aliases = BTreeMap::from([(
            PathBuf::from("/dev/video2"),
            PathBuf::from("/dev/v4l/by-id/usb-camera"),
        )]);
        let devices = linux_devices(
            "Auto-detected sources for video4linux2:\n /dev/video2 [USB Camera] (none)\n /dev/video0 [Built in] (none)",
            &aliases,
        );
        assert_eq!(devices.len(), 2);
        assert!(devices[0].persistent);
        assert!(!devices[1].persistent);
        assert_eq!(devices[0].id, "v4l2:/dev/v4l/by-id/usb-camera");
    }

    #[test]
    fn a_saved_metadata_node_explains_how_to_choose_the_video_device() {
        let error = problem(
            "[video4linux2,v4l2 @ fixture] Not a video capture device.\nError opening input: No such device",
            "The camera stopped sending video",
        );
        assert!(error.contains("metadata device"), "{error}");
        assert!(
            error.contains("Remove this entry in Camera settings"),
            "{error}"
        );
        assert!(error.contains("add the camera again"), "{error}");
        // The fixed helper omits a saved metadata node before capture starts.
        let missing = disconnected("v4l2:/dev/v4l/by-id/usb-camera-video-index1");
        assert!(missing.contains("metadata device"), "{missing}");
        assert!(
            missing.contains("remove this entry in Camera settings"),
            "{missing}"
        );
        assert!(missing.contains("add the camera again"), "{missing}");
        assert!(!disconnected("avfoundation:camera").contains("metadata"));
    }

    #[test]
    fn analog_modes_keep_field_times_and_correct_picture_shape_without_audio() {
        let mut input = CaptureInput {
            device_id: "avfoundation:usb-A".into(),
            name: "USB Video".into(),
            enabled: true,
            standard: CaptureStandard::Pal,
            width: None,
            height: None,
            frame_rate: None,
            deinterlace: true,
            record_sound: false,
            sound_input: None,
            no_sound: false,
        };
        let device = CaptureDevice {
            id: input.device_id.clone(),
            name: input.name.clone(),
            persistent: true,
        };
        let line = args(&input, &device, None).unwrap().join(" ");
        assert!(line.contains("-video_device_id usb-A"));
        assert!(line.contains("-video_size 720x576 -framerate 25"));
        assert!(
            line.contains("bwdif=mode=send_field:parity=auto:deint=all,scale=768:576,setsar=1")
        );
        assert!(line.contains("-an"));
        assert!(line.contains("-bf 0"));
        assert!(line.ends_with("-f mpegts pipe:1"));
        input.standard = CaptureStandard::Ntsc;
        let line = args(&input, &device, None).unwrap().join(" ");
        assert!(line.contains("720x480 -framerate 30000/1001"));
        assert!(line.contains("scale=640:480,setsar=1"));
    }

    #[test]
    fn alsa_inputs_are_named_by_card_through_plughw() {
        let text = "Auto-detected sources for alsa:\n* default [Default ALSA Output]\n  hw:CARD=C920,DEV=0 [HD Pro Webcam C920, USB Audio]\n  sysdefault:CARD=C920 [HD Pro Webcam C920]\n  hw:CARD=PCH,DEV=0 [HDA Intel PCH, ALC3246 Analog]";
        let inputs = alsa_devices(text);
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].id, "alsa:default");
        assert_eq!(inputs[1].id, "alsa:plughw:CARD=C920,DEV=0");
        assert_eq!(inputs[1].name, "HD Pro Webcam C920, USB Audio");
        assert!(inputs.iter().all(|i| i.persistent));
    }

    #[test]
    fn linux_desktop_microphone_is_available_without_claiming_raw_hardware() {
        // Real FFmpeg shape on the Framework Ryzen laptop. Its first raw
        // device is busy while PipeWire owns it; the pipewire route works.
        let inputs = alsa_devices(
            "Auto-detected sources for alsa:\n  null [Discard all samples (playback) or generate zero samples (capture)] (none)\n  pipewire [PipeWire Sound Server] (none)\n* default [Default ALSA Output (currently PipeWire Media Server)] (none)\n  hw:CARD=Generic_1,DEV=0 [Direct hardware device without any conversions] (none)\n  plughw:CARD=Generic_1,DEV=0 [Hardware device with all software conversions] (none)\n  hw:CARD=acppdmmach,DEV=0 [Direct hardware device without any conversions] (none)",
        );
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0].id, "alsa:pipewire");
        assert_eq!(inputs[0].name, "System microphone (PipeWire)");
        assert_eq!(inputs[1].id, "alsa:plughw:CARD=Generic_1,DEV=0");
        assert_eq!(inputs[1].name, "ALSA microphone (Generic_1,DEV=0)");
        assert_eq!(inputs[2].name, "ALSA microphone (acppdmmach,DEV=0)");
        assert!(
            own_microphone(
                &CaptureDevice {
                    id: "v4l2:/dev/video0".into(),
                    name: "Laptop Webcam Module (2nd Gen)".into(),
                    persistent: false,
                },
                &inputs
            )
            .is_none()
        );
    }

    #[test]
    fn alsa_lists_pulse_and_preserves_informative_names_without_ffmpeg_suffixes() {
        let inputs = alsa_devices(
            "  pulse [PulseAudio Sound Server] (none)\n* default [Default ALSA Output] (none)\n  hw:CARD=C920,DEV=0 [HD Pro Webcam C920, USB Audio] (none)",
        );
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].id, "alsa:pulse");
        assert_eq!(inputs[0].name, "System microphone (PulseAudio)");
        assert_eq!(inputs[1].name, "HD Pro Webcam C920, USB Audio");
        assert!(
            alsa_devices("Auto-detected sources for alsa:\n  null [Discard samples] (none)")
                .is_empty()
        );
    }

    #[test]
    fn a_sound_input_is_captured_with_its_camera_on_each_platform() {
        let input = CaptureInput {
            device_id: "avfoundation:usb-A".into(),
            name: "USB Video".into(),
            enabled: true,
            standard: CaptureStandard::Auto,
            width: None,
            height: None,
            frame_rate: None,
            deinterlace: false,
            record_sound: true,
            sound_input: None,
            no_sound: false,
        };
        let device = |id: &str| CaptureDevice {
            id: id.into(),
            name: "Device".into(),
            persistent: true,
        };
        // macOS: one AVFoundation input with both exact identities.
        let mac = args(
            &input,
            &device("avfoundation:usb-A"),
            Some(&device("avfoundation:mic-1")),
        )
        .unwrap()
        .join(" ");
        assert!(mac.contains("-thread_queue_size 8"));
        assert!(mac.contains("-video_device_id usb-A -audio_device_id mic-1"));
        assert!(mac.contains("-i none:none"));
        assert!(mac.contains("-map 0:v:0 -map 0:a:0"));
        assert!(!mac.contains("-an"));
        assert!(mac.contains("-c:a aac -b:a 128k -ar 48000"));
        assert!(mac.ends_with("-f mpegts pipe:1"));
        // Windows: video and audio in one DirectShow input.
        let win = args(
            &input,
            &device("dshow:@device_pnp_cam"),
            Some(&device("dshow:@device_cm_mic")),
        )
        .unwrap()
        .join(" ");
        assert!(win.contains("-i video=@device_pnp_cam:audio=@device_cm_mic"));
        assert!(win.contains("-map 0:a:0"));
        // Linux: ALSA as a second input.
        let linux = args(
            &input,
            &device("v4l2:/dev/v4l/by-id/usb-cam"),
            Some(&device("alsa:plughw:CARD=C920,DEV=0")),
        )
        .unwrap()
        .join(" ");
        assert!(linux.contains(
            "-i /dev/v4l/by-id/usb-cam -thread_queue_size 64 -f alsa -i plughw:CARD=C920,DEV=0"
        ));
        assert!(linux.contains("-map 0:v:0 -map 1:a:0"));
        for route in ["pipewire", "pulse", "default"] {
            let line = args(
                &input,
                &device("v4l2:/dev/video0"),
                Some(&device(&format!("alsa:{route}"))),
            )
            .unwrap()
            .join(" ");
            assert!(line.contains(&format!("-f alsa -i {route}")));
            assert!(line.contains("-map 0:v:0 -map 1:a:0"));
        }
        // A sound input from another system is refused; none means no sound.
        assert!(
            args(
                &input,
                &device("v4l2:/dev/video0"),
                Some(&device("avfoundation:mic"))
            )
            .is_err()
        );
        let silent = args(&input, &device("avfoundation:usb-A"), None)
            .unwrap()
            .join(" ");
        assert!(silent.contains("-thread_queue_size 2") && silent.contains("-an"));
        assert!(!silent.contains("aac"));
    }

    #[test]
    fn a_camera_records_its_own_microphone_and_no_one_elses() {
        let device = |id: &str, name: &str| CaptureDevice {
            id: id.into(),
            name: name.into(),
            persistent: true,
        };
        // As listed on a Mac with BlackHole, Camo, Teams and a Storm webcam.
        let inputs = [
            device("bh", "BlackHole 2ch"),
            device("mac", "MacBook Pro Microphone"),
            device("storm", "Storm Microphone"),
            device("camo", "Camo Microphone"),
            device("teams", "Microsoft Teams Audio"),
        ];
        let own =
            |camera: &str| own_microphone(&device("cam", camera), &inputs).map(|d| d.id.as_str());
        assert_eq!(own("MacBook Pro Camera"), Some("mac"));
        assert_eq!(own("MacBook Pro Desk View Camera"), Some("mac"));
        assert_eq!(own("Storm Camera"), Some("storm"));
        assert_eq!(own("Camo Camera"), Some("camo"));
        // Most cameras have none: no sound, never another microphone.
        assert_eq!(own("Iriun Camera"), None);
        assert_eq!(own("OBS Virtual Camera"), None);
        assert_eq!(own("USB Video"), None);
        // A shared brand is not enough: a headset is not the webcam's.
        let brand = [device("headset", "Logitech G Pro Microphone")];
        assert_eq!(
            own_microphone(&device("brio", "Logitech BRIO"), &brand),
            None
        );
        // A capture adapter whose audio has its name, as DirectShow and ALSA list them.
        let adapter = [
            device("dshow:a", "Cam Link 4K"),
            device("dshow:b", "Microphone (Realtek)"),
        ];
        assert_eq!(
            own_microphone(&device("dshow:v", "Cam Link 4K"), &adapter).map(|d| d.id.as_str()),
            Some("dshow:a")
        );
    }

    #[test]
    fn a_sound_input_that_fails_says_so_in_words() {
        let mic = CaptureDevice {
            id: "avfoundation:mic".into(),
            name: "USB Microphone".into(),
            persistent: true,
        };
        let refused = sound_problem(
            "[AVFoundation indev @ x] Failed to create AV capture input device: Cannot use USB Microphone",
            &mic,
        )
        .unwrap();
        assert!(refused.contains("microphone privacy settings"), "{refused}");
        assert!(refused.contains("choose No sound under Sound"));
        let gone = sound_problem(
            "[AVFoundation indev @ x] The selected sound input is not connected",
            &mic,
        )
        .unwrap();
        assert!(gone.starts_with("The sound input USB Microphone is not connected"));
        // A camera's own failure is not the sound input's.
        assert_eq!(
            sound_problem("Permission denied opening camera", &mic),
            None
        );
    }

    #[tokio::test]
    async fn stderr_is_drained_with_bounded_memory() {
        let bytes = vec![b'x'; MAX_DIAGNOSTIC * 4];
        assert_eq!(tail(bytes.as_slice()).await.len(), MAX_DIAGNOSTIC);
    }
}

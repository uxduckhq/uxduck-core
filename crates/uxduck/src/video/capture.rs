//! Local cameras belong to the daemon, including when every UI has closed.
//! The separately packaged `FFmpeg` runtime owns capture, decoding and encoding.
//! Only MPEG-TS comes back over a pipe; no raw pictures or GPU handles enter
//! the process that owns vehicle links. The runtime is built from pinned source.

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

/// Device enumeration is lazy and cached. The engine never waits for a driver.
#[derive(Debug, Default)]
pub struct Devices {
    state: Mutex<(VideoDevicesResult, Option<Instant>)>,
}

impl Devices {
    pub fn list(self: &Arc<Self>, runtime: &tokio::runtime::Handle) -> VideoDevicesResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.0.scanning
            && state
                .1
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(3))
        {
            state.0.scanning = true;
            let shared = self.clone();
            runtime.spawn(async move {
                let result = discover().await;
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.0 = match result {
                    Ok(devices) => VideoDevicesResult {
                        devices,
                        scanning: false,
                        error: None,
                    },
                    Err(error) => VideoDevicesResult {
                        devices: Vec::new(),
                        scanning: false,
                        error: Some(error),
                    },
                };
                state.1 = Some(Instant::now());
            });
        }
        state.0.clone()
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
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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

fn command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.kill_on_drop(true).stdin(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // no console window
    command
}

/// Drain diagnostics even during capture, keeping bounded memory.
async fn tail(mut reader: impl AsyncRead + Unpin) -> String {
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

/// Deliberately omit screens, microphones and numeric enumeration indices.
fn mac_devices(text: &str) -> Result<Vec<CaptureDevice>, String> {
    let mut in_video = false;
    let mut legacy = false;
    let mut found = BTreeMap::new();
    for line in text.lines() {
        if line.contains("AVFoundation video devices:") {
            in_video = true;
            continue;
        }
        if line.contains("AVFoundation audio devices:") {
            break;
        }
        if !in_video {
            continue;
        }
        let Some((name, uid)) = line.rsplit_once("  [uid:") else {
            legacy |= line.contains("] [") && !line.contains("Capture screen");
            continue;
        };
        let Some(uid) = uid.strip_suffix(']') else {
            continue;
        };
        let Some((_, name)) = name.rsplit_once("] ") else {
            continue;
        };
        let id = format!("avfoundation:{uid}");
        found.insert(
            id.clone(),
            CaptureDevice {
                id,
                name: name.trim().into(),
                persistent: true,
            },
        );
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
    Ok(found.into_values().collect())
}

fn windows_devices(text: &str) -> Result<Vec<CaptureDevice>, String> {
    let mut name = None;
    let mut devices = Vec::new();
    for line in text.lines() {
        let line = line.split_once("] ").map_or(line, |(_, rest)| rest).trim();
        if let Some(named) = line
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix("\" (video)"))
        {
            name = Some(named.to_owned());
        } else if let Some(id) = line
            .strip_prefix("Alternative name \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            if let Some(name) = name.take() {
                devices.push(CaptureDevice {
                    id: format!("dshow:{id}"),
                    name,
                    persistent: true,
                });
            }
        } else if line.ends_with("(audio)") {
            name = None;
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
    Ok(devices)
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

async fn discover() -> Result<Vec<CaptureDevice>, String> {
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
        Ok(linux_devices(&text, &aliases))
    }
}

fn problem(text: &str, fallback: &str) -> String {
    let lower = text.to_lowercase();
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

fn args(input: &CaptureInput, device: &CaptureDevice) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-nostdin",
        "-thread_queue_size",
        "2",
    ]
    .map(Into::into)
    .to_vec();
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
    match backend {
        "avfoundation" => args.extend([
            "-video_device_id".into(),
            id.into(),
            "-pixel_format".into(),
            "nv12".into(),
            "-i".into(),
            "none:none".into(),
        ]),
        "dshow" => args.extend(["-i".into(), format!("video={id}")]),
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
        }
        _ => return Err("This camera belongs to another operating system.".into()),
    }
    args.extend(["-map", "0:v:0", "-an", "-sn", "-dn"].map(Into::into));
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
    args.extend(encoding_args());
    Ok(args)
}

fn encoding_args() -> Vec<String> {
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

pub fn spawn(
    runtime: &tokio::runtime::Handle,
    id: String,
    input: CaptureInput,
    epoch: u64,
    hub: Arc<Hub>,
    engine: EngineHandle,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let mut backoff = 1u64;
        loop {
            let result = run(&id, &input, epoch, &hub, &engine).await;
            let report_id = id.clone();
            let config = input.clone();
            engine.post(move |e| e.capture_stopped(&report_id, &config, epoch, result));
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(15);
        }
    })
}

async fn run(
    id: &str,
    input: &CaptureInput,
    epoch: u64,
    hub: &Hub,
    engine: &EngineHandle,
) -> String {
    let result = async {
        let program = program()?;
        let devices = discover().await?;
        let device = devices.into_iter().find(|d| d.id == input.device_id).ok_or("Camera disconnected. Reconnect the selected camera. UXDuck will resume capture automatically.")?;
        let mut child = command(&program).args(args(input, &device)?).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("The camera capture helper could not start: {e}"))?;
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
        let failure = loop {
            let limit = if startup { Duration::from_secs(15) } else { Duration::from_secs(5) };
            let timeout = limit.saturating_sub(last_picture.elapsed());
            let n = match tokio::time::timeout(timeout, stdout.read(&mut buffer)).await {
                Ok(Ok(0)) => break "The camera stopped sending video",
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break "The camera capture pipe closed",
                Err(_) => break "No video arrived from the camera. Check its connection, driver and selected video mode",
            };
            pending.extend_from_slice(&buffer[..n]);
            let whole = pending.len() / 188 * 188;
            if whole == 0 { continue; }
            for output in source.push(&pending[..whole]) { hub.publish(id, &listener::item(output)); }
            pending.drain(..whole);
            let status = source.status();
            if status.counters.pictures > pictures {
                last_picture = Instant::now();
                pictures = status.counters.pictures;
            }
            if status.counters.pictures > 0 && (startup || reported.elapsed() >= Duration::from_secs(1)) {
                let mut report = listener::report(0, &std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &status, reported.elapsed().as_secs_f64().max(0.001), before.0, before.1);
                report.id = Some(id.to_owned());
                report.sender = "local capture".into();
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
        Err::<(), String>(problem(&diagnostics, failure))
    }.await;
    result
        .err()
        .unwrap_or_else(|| "Camera capture ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ignores_indices_names_screens_and_audio() {
        let text = "[AVFoundation indev @ a] AVFoundation video devices:\n[AVFoundation indev @ a] [0] USB Video  [uid:usb-A]\n[AVFoundation indev @ a] [1] USB Video  [uid:usb-B]\n[AVFoundation indev @ a] [2] Capture screen 0\n[AVFoundation indev @ a] AVFoundation audio devices:\n[AVFoundation indev @ a] [0] Microphone  [uid:mic]";
        let devices = mac_devices(text).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "avfoundation:usb-A");
        assert_eq!(devices[0].name, devices[1].name);
        assert!(mac_devices("[AVFoundation indev @ a] AVFoundation video devices:\n[AVFoundation indev @ a] [0] Old name-only camera\n[AVFoundation indev @ a] AVFoundation audio devices:").is_err());
        let win = windows_devices("[dshow @ a] \"USB Video\" (video)\n[dshow @ a]   Alternative name \"@device_pnp_camera-A\"\n[dshow @ a] \"USB Audio\" (audio)\n[dshow @ a]   Alternative name \"@device_audio\"").unwrap();
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].id, "dshow:@device_pnp_camera-A");
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
        };
        let device = CaptureDevice {
            id: input.device_id.clone(),
            name: input.name.clone(),
            persistent: true,
        };
        let line = args(&input, &device).unwrap().join(" ");
        assert!(line.contains("-video_device_id usb-A"));
        assert!(line.contains("-video_size 720x576 -framerate 25"));
        assert!(
            line.contains("bwdif=mode=send_field:parity=auto:deint=all,scale=768:576,setsar=1")
        );
        assert!(line.contains("-an"));
        assert!(line.contains("-bf 0"));
        assert!(line.ends_with("-f mpegts pipe:1"));
        input.standard = CaptureStandard::Ntsc;
        let line = args(&input, &device).unwrap().join(" ");
        assert!(line.contains("720x480 -framerate 30000/1001"));
        assert!(line.contains("scale=640:480,setsar=1"));
    }

    #[tokio::test]
    async fn stderr_is_drained_with_bounded_memory() {
        let bytes = vec![b'x'; MAX_DIAGNOSTIC * 4];
        assert_eq!(tail(bytes.as_slice()).await.len(), MAX_DIAGNOSTIC);
    }
}

//! Live view of a camera with periodic intra refresh (ADR-0017): it sends
//! no IDR after it starts and `WebCodecs` starts only at one, so while a
//! browser waits on it, the capture helper re-encodes it with an IDR every
//! second, for that camera's browsers only. Only then: it costs CPU.
//! Recordings and the live publisher keep the camera's own pictures. It
//! stops as soon as no browser watches. The encoder is the computer's
//! hardware one when the helper has it (`VideoToolbox`, Media Foundation),
//! else x264, as Settings › Cameras says: hardware takes a 1080p camera from
//! about 63% of a core to about 20% (the rest is decoding, which stays in
//! software: the `VideoToolbox` decoder refuses to start at a recovery point).
//! The picture keeps the camera's size: scaling it down in software costs
//! more than the smaller encode saves.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedReceiver;
use uxduck_media::nal::Codec;
use uxduck_media::source::{Output, Source};
use uxduck_media::ts::PACKET_SIZE;
use uxduck_media::tsmux::TsWriter;

use uxduck_core::domain::video::Reencoder;

use super::hub::{Hub, Item, Watcher};
use crate::daemon::engine_task::EngineHandle;

/// A helper that takes this long to accept a picture has stalled.
const WRITE_STALL: Duration = Duration::from_secs(5);

/// x264, which every helper has.
pub const SOFTWARE: &str = "libx264";

/// This platform's hardware H.264 encoder, when the helper is built with it.
#[must_use]
pub const fn hardware() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("h264_videotoolbox")
    } else if cfg!(windows) {
        Some("h264_mf")
    } else {
        None
    }
}

/// A bitrate for a hardware encoder, from the picture's height: they need
/// one, and x264's constant quality has no counterpart there.
fn bitrate(height: Option<u32>) -> &'static str {
    match height.unwrap_or(1080) {
        0..=480 => "2M",
        481..=720 => "5M",
        721..=1080 => "10M",
        _ => "20M",
    }
}

/// The capture helper's arguments: the camera as MPEG-TS on its stdin (the
/// helper reads no raw H.264), out through `encoder` as H.264 without
/// B-frames, an IDR a second, as MPEG-TS, with as little delay as it allows.
#[must_use]
pub fn args(encoder: &str, height: Option<u32>) -> Vec<String> {
    let mut args: Vec<&str> = vec![
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
        "-f",
        "mpegts",
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
        "-an",
        "-c:v",
        encoder,
    ];
    match encoder {
        "h264_videotoolbox" => args.extend([
            "-realtime",
            "1",
            // Never VideoToolbox's own software fallback: that is x264's job.
            "-allow_sw",
            "0",
            "-b:v",
            bitrate(height),
            "-pix_fmt",
            "nv12",
        ]),
        "h264_mf" => args.extend([
            "-hw_encoding",
            "1",
            "-b:v",
            bitrate(height),
            "-pix_fmt",
            "nv12",
        ]),
        _ => args.extend([
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
        ]),
    }
    args.extend([
        "-bf",
        "0",
        "-force_key_frames",
        "expr:gte(t,n_forced)",
        "-flush_packets",
        "1",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "-f",
        "mpegts",
        "pipe:1",
    ]);
    args.into_iter().map(str::to_owned).collect()
}

/// The encoders to try, in order, for a setting and what the helper has.
/// # Errors
/// Hardware asked for, and this computer's helper has none.
pub fn encoders(
    choice: Reencoder,
    hardware: Option<&'static str>,
) -> Result<Vec<&'static str>, String> {
    match (choice, hardware) {
        (Reencoder::Automatic, Some(hardware)) => Ok(vec![hardware, SOFTWARE]),
        (Reencoder::Automatic | Reencoder::Software, _) => Ok(vec![SOFTWARE]),
        (Reencoder::Hardware, Some(hardware)) => Ok(vec![hardware]),
        (Reencoder::Hardware, None) => Err("This computer has no hardware H.264 encoder \
            UXDuck can use. Choose Automatic or Software in Settings › Cameras."
            .into()),
    }
}

/// Whether the helper was built with `encoder`, asked once.
async fn helper_has(program: &Path, encoder: &str) -> bool {
    static ENCODERS: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    let listed = ENCODERS
        .get_or_init(|| async {
            super::capture::command(program)
                .args(["-hide_banner", "-encoders"])
                .output()
                .await
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default()
        })
        .await;
    listed
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}

/// Re-encodes each source the hub asks for, one task each.
pub async fn manage(hub: Arc<Hub>, engine: EngineHandle, mut wanted: UnboundedReceiver<String>) {
    while let Some(source_id) = wanted.recv().await {
        let (hub, engine) = (hub.clone(), engine.clone());
        tokio::spawn(async move {
            tracing::info!(source = %source_id, "re-encoding a camera that sends no IDR");
            let result = run(&hub, &engine, &source_id).await;
            hub.reencode_ended(&source_id, result.is_err());
            let id = source_id.clone();
            engine.post(move |e| e.video_reencoding(&id, None));
            match result {
                Ok(pictures) => {
                    tracing::info!(source = %source_id, pictures, "re-encoding stopped");
                }
                Err(why) => {
                    tracing::warn!(source = %source_id, %why, "re-encoding failed");
                    engine.post(move |e| e.video_reencode_failed(&source_id, why));
                }
            }
        });
    }
}

/// Each encoder the setting allows in turn, the next when one fails while
/// browsers still watch: Automatic falls back from hardware to software.
async fn run(hub: &Arc<Hub>, engine: &EngineHandle, source_id: &str) -> Result<u64, String> {
    let program = super::capture::program()?;
    let choice = engine
        .run(|e| e.video_reencoder())
        .await
        .unwrap_or_default();
    let hardware = match hardware() {
        Some(name) if helper_has(&program, name).await => Some(name),
        _ => None,
    };
    let order = encoders(choice, hardware)?;
    let mut result = Ok(0);
    for (i, encoder) in order.iter().enumerate() {
        let (id, named) = (source_id.to_owned(), (*encoder).to_owned());
        engine.post(move |e| e.video_reencoding(&id, Some(named)));
        let key = format!("reencode:{source_id}");
        let watcher = hub.tap(&key, source_id);
        result = pump(hub, source_id, &program, encoder, &watcher).await;
        hub.close(&key);
        match &result {
            Err(why) if i + 1 < order.len() && hub.browsers(source_id) > 0 => {
                tracing::warn!(source = %source_id, encoder, %why, "falling back to software");
            }
            _ => break,
        }
    }
    result
}

/// The camera's first picture for the helper, or none once no browser
/// watches or the watch ends.
async fn first_picture(
    hub: &Arc<Hub>,
    source_id: &str,
    watcher: &Watcher,
) -> Option<Arc<uxduck_media::au::AccessUnit>> {
    let mut look = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            next = watcher.next() => match next {
                None => return None,
                Some((Item::Unit(unit), _)) => return Some(unit),
                Some(_) => {}
            },
            _ = look.tick() => {
                if hub.browsers(source_id) == 0 {
                    return None;
                }
            }
        }
    }
}

/// The camera's pictures into the helper and its pictures out to the
/// browsers, until none watch; how many it made.
async fn pump(
    hub: &Arc<Hub>,
    source_id: &str,
    program: &Path,
    encoder: &str,
    watcher: &Watcher,
) -> Result<u64, String> {
    // The first picture says the camera's size, for a hardware bitrate.
    let Some(first) = first_picture(hub, source_id, watcher).await else {
        return Ok(0);
    };
    let mut look = tokio::time::interval(Duration::from_secs(1));
    let mut child = super::capture::command(program)
        .args(args(encoder, first.sps.coded_height))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("The capture helper did not start: {e}"))?;
    let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("The capture helper has no pipes".into());
    };
    let diagnostics = tokio::spawn(super::capture::tail(stderr));
    let reader = {
        let (hub, source_id) = (hub.clone(), source_id.to_owned());
        tokio::spawn(async move {
            let mut source = Source::new();
            let (mut pending, mut chunk) = (Vec::new(), vec![0; PACKET_SIZE * 64]);
            let mut pictures = 0u64;
            while let Ok(n @ 1..) = stdout.read(&mut chunk).await {
                pending.extend_from_slice(&chunk[..n]);
                let whole = pending.len() / PACKET_SIZE * PACKET_SIZE;
                for output in source.push(&pending[..whole]) {
                    if let Output::Unit(unit) = output {
                        pictures += 1;
                        hub.publish_reencoded(&source_id, unit);
                    }
                }
                pending.drain(..whole);
            }
            pictures
        })
    };
    let mut writer: Option<TsWriter> = None;
    let mut stalled = false;
    let mut pending = Some(first);
    loop {
        tokio::select! {
            next = async {
                match pending.take() {
                    Some(unit) => Some((Item::Unit(unit), false)),
                    None => watcher.next().await,
                }
            } => {
                let unit = match next {
                    None => break,
                    Some((Item::Unit(unit), _)) => unit,
                    Some(_) => continue,
                };
                let codec = if unit.sps.codec.starts_with("hvc") || unit.sps.codec.starts_with("hev") {
                    Codec::H265
                } else {
                    Codec::H264
                };
                let ts = writer
                    .get_or_insert_with(|| TsWriter::new(codec, false))
                    .picture(&unit.data, unit.timestamp_us * 9 / 100, unit.start());
                if !matches!(tokio::time::timeout(WRITE_STALL, stdin.write_all(&ts)).await, Ok(Ok(()))) {
                    stalled = true;
                    break;
                }
            }
            _ = look.tick() => {
                if hub.browsers(source_id) == 0 {
                    break;
                }
            }
        }
    }
    drop(stdin);
    let exited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    if !matches!(exited, Ok(Ok(_))) {
        let _ = child.kill().await;
    }
    let pictures = reader.await.unwrap_or(0);
    let said = diagnostics.await.unwrap_or_default();
    // Only browsers leaving end a re-encode well; anything else while one
    // still watches is a failure, however many pictures came first.
    if stalled || hub.browsers(source_id) > 0 {
        let said = said.trim();
        let what = if pictures == 0 {
            "The capture helper made no pictures from this camera"
        } else {
            "The capture helper stopped re-encoding this camera"
        };
        return Err(if said.is_empty() {
            what.into()
        } else {
            format!("{what}: {said}")
        });
    }
    Ok(pictures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_reads_mpeg_ts_and_writes_h264_with_an_idr_a_second() {
        for encoder in [SOFTWARE, "h264_videotoolbox", "h264_mf"] {
            let args = args(encoder, Some(1080)).join(" ");
            assert!(args.contains("-f mpegts -i pipe:0"));
            assert!(args.contains(&format!("-c:v {encoder}")));
            assert!(args.contains("-bf 0"));
            assert!(args.contains("-force_key_frames expr:gte(t,n_forced)"));
            assert!(args.ends_with("-f mpegts pipe:1"));
        }
    }

    #[test]
    fn hardware_gets_a_bitrate_for_the_picture_and_never_its_own_software() {
        let small = args("h264_videotoolbox", Some(360)).join(" ");
        assert!(small.contains("-b:v 2M") && small.contains("-allow_sw 0"));
        assert!(
            args("h264_videotoolbox", Some(1080))
                .join(" ")
                .contains("-b:v 10M")
        );
        assert!(
            args("h264_mf", Some(2160))
                .join(" ")
                .contains("-hw_encoding 1 -b:v 20M")
        );
        // x264 keeps constant quality, no bitrate.
        assert!(!args(SOFTWARE, Some(1080)).join(" ").contains("-b:v"));
    }

    #[test]
    fn automatic_tries_hardware_then_software_and_the_others_insist() {
        let vt = Some("h264_videotoolbox");
        assert_eq!(
            encoders(Reencoder::Automatic, vt).unwrap(),
            ["h264_videotoolbox", SOFTWARE]
        );
        assert_eq!(encoders(Reencoder::Automatic, None).unwrap(), [SOFTWARE]);
        assert_eq!(
            encoders(Reencoder::Hardware, vt).unwrap(),
            ["h264_videotoolbox"]
        );
        assert!(encoders(Reencoder::Hardware, None).is_err());
        assert_eq!(encoders(Reencoder::Software, vt).unwrap(), [SOFTWARE]);
    }
}

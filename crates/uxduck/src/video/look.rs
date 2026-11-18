//! The assistant's look through a camera (FB-244, ADR-0017 V28): the
//! camera's pictures from the hub, as MPEG-TS, into the packaged capture
//! helper, which decodes them, samples a clip with `fps`, scales them and
//! writes JPEGs. Only JPEGs come back: the daemon never decodes (V1).

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uxduck_engine::looks::{LookOrder, Looked, LookedFrame, look_size};
use uxduck_media::nal::Codec;
use uxduck_media::tsmux::TsWriter;

use super::hub::{Hub, Item, Watcher};
use crate::daemon::engine_task::EngineHandle;

/// How long a look waits for the camera's first picture. A camera nobody
/// had open connects first, and a decoder starts only at a keyframe or a
/// recovery point, which some cameras send every few seconds.
const FIRST_PICTURE: Duration = Duration::from_secs(15);

/// How long past a clip's length the helper may take to write its frames.
const SLACK: Duration = Duration::from_secs(5);

/// How long the helper may take to hand over its last frames once it is
/// told the clip is over.
const LAST_FRAMES: Duration = Duration::from_secs(2);

/// Past every wait above, how long a look may take to stop the helper and
/// answer, before it is stopped. The engine holds a slot for each look
/// until it answers, so a look always answers.
const STOPPING: Duration = Duration::from_secs(10);

/// The helper's JPEG quality, `FFmpeg`'s `-q:v` (2 best to 31): about 80 KB
/// for a 1024 px frame of camera footage.
const QUALITY: &str = "5";

/// The capture helper's arguments: the camera as MPEG-TS on its stdin, out
/// as `frames` JPEGs of `width` × `height` on its stdout, a clip's evenly
/// spaced over `seconds` from its first picture.
#[must_use]
pub fn args(frames: u32, seconds: f64, width: u32, height: u32) -> Vec<String> {
    let scale = format!("scale={width}:{height}:out_range=full,format=yuvj420p");
    let filter = if frames > 1 {
        format!("fps={}/{seconds},{scale}", frames - 1)
    } else {
        scale
    };
    [
        "-hide_banner",
        "-loglevel",
        "error",
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
        "-vf",
        &filter,
        "-frames:v",
        &frames.to_string(),
        "-q:v",
        QUALITY,
        "-c:v",
        "mjpeg",
        "-f",
        "mjpeg",
        "pipe:1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Where the JPEG at the start of `data` ends, once it has all arrived:
/// its segments are walked, and its scan data read to the next marker.
#[must_use]
pub fn jpeg_end(data: &[u8]) -> Option<usize> {
    if data.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    loop {
        if *data.get(at)? != 0xFF {
            return None;
        }
        let marker = *data.get(at + 1)?;
        match marker {
            0xD9 => return Some(at + 2),
            0xFF => at += 1,
            0x01 | 0xD0..=0xD7 => at += 2,
            _ => {
                let length =
                    usize::from(u16::from_be_bytes([*data.get(at + 2)?, *data.get(at + 3)?]));
                at += 2 + length;
                if marker == 0xDA {
                    // Scan data: an 0xFF in it is followed by 0x00 or a restart.
                    loop {
                        if *data.get(at)? == 0xFF {
                            match *data.get(at + 1)? {
                                0x00 | 0xD0..=0xD7 => at += 2,
                                _ => break,
                            }
                        } else {
                            at += 1;
                        }
                    }
                }
            }
        }
    }
}

/// Takes every whole JPEG from the start of `pending`.
pub fn take_jpegs(pending: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut taken = Vec::new();
    while let Some(end) = jpeg_end(pending) {
        taken.push(pending.drain(..end).collect());
    }
    taken
}

/// A JPEG's size, from its frame header.
fn size(jpeg: &[u8]) -> (u32, u32) {
    uxduck_media::photo::jpeg_size(jpeg).unwrap_or_default()
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// A clip's length as a wait. However long a clip is asked for, the
/// longest wait there is.
fn clip(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX)
}

/// When a wait of `length` from now ends: far ahead for a clip too long
/// for the clock to count.
fn after(length: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(length)
        .unwrap_or_else(|| now + Duration::from_hours(100 * 365 * 24))
}

/// Looks, and tells the engine what it saw. Aborting the handle stops the
/// look and its helper, and nothing is told.
pub fn start(
    hub: Arc<Hub>,
    engine: EngineHandle,
    program: std::path::PathBuf,
    order: LookOrder,
) -> tokio::task::AbortHandle {
    let key = format!("look:{}", order.id);
    let watcher = hub.tap(&key, &order.source_id);
    let looking = {
        let order = order.clone();
        tokio::spawn(async move { look(&program, &watcher, &order).await })
    };
    let stop = looking.abort_handle();
    let handle = stop.clone();
    tokio::spawn(async move {
        let limit = FIRST_PICTURE
            .saturating_add(clip(order.seconds))
            .saturating_add(SLACK + LAST_FRAMES + STOPPING);
        let result = match tokio::time::timeout_at(after(limit).into(), looking).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "The look through {} stopped before it finished.",
                order.camera
            )),
            Err(_) => {
                stop.abort();
                Err(format!(
                    "The look through {} took longer than {} s, so UXDuck stopped it.",
                    order.camera,
                    limit.as_secs()
                ))
            }
        };
        hub.close(&key);
        if let Err(why) = &result {
            tracing::warn!(source = %order.source_id, %why, "the assistant's look failed");
        }
        let id = order.id;
        engine.post(move |e| e.video_looked(&id, result));
    });
    handle
}

#[allow(
    clippy::too_many_lines,
    reason = "one look, from the first picture to the last frame"
)]
async fn look(program: &Path, watcher: &Watcher, order: &LookOrder) -> Result<Looked, String> {
    let first = tokio::time::timeout(FIRST_PICTURE, async {
        loop {
            match watcher.next().await {
                None => return None,
                Some((Item::Unit(unit), _)) => return Some(unit),
                Some(_) => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| {
        format!(
            "{} sent no picture UXDuck could start from in {} s. Check that it is on and \
             connected.",
            order.camera,
            FIRST_PICTURE.as_secs()
        )
    })?;
    let seen_at_us = now_us();
    let camera_width = first.sps.display_width.or(first.sps.coded_width);
    let camera_height = first.sps.display_height.or(first.sps.coded_height);
    let (width, height) = look_size(
        camera_width.unwrap_or(1920),
        camera_height.unwrap_or(1080),
        order.frames,
        order.max_bytes,
    )?;
    let mut child = super::capture::command(program)
        .args(args(order.frames, order.seconds, width, height))
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
    let wanted = order.frames as usize;
    let room = order.max_bytes.unwrap_or(u64::MAX);
    // Frames that pass the room stop the look at once: a long clip that
    // comes out larger than estimated is not made to its end for nothing.
    let mut reader = tokio::spawn(async move {
        let (mut pending, mut chunk, mut jpegs) = (Vec::new(), vec![0; 64 * 1024], Vec::new());
        let mut bytes = 0u64;
        while jpegs.len() < wanted && bytes <= room {
            match stdout.read(&mut chunk).await {
                Ok(n @ 1..) => {
                    pending.extend_from_slice(&chunk[..n]);
                    for jpeg in take_jpegs(&mut pending) {
                        bytes += jpeg.len() as u64;
                        jpegs.push(jpeg);
                        // The frame that passes the room is the last one
                        // counted, however many more one read brought.
                        if bytes > room {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        jpegs
    });
    let deadline = after(clip(order.seconds).saturating_add(SLACK));
    let codec = if first.sps.codec.starts_with("hvc") || first.sps.codec.starts_with("hev") {
        Codec::H265
    } else {
        Codec::H264
    };
    let mut writer = TsWriter::new(codec, false);
    let mut pending = Some(first);
    let jpegs = loop {
        tokio::select! {
            jpegs = &mut reader => {
                drop(stdin);
                break jpegs.unwrap_or_default();
            }
            () = tokio::time::sleep_until(deadline.into()) => {
                drop(stdin);
                break last_frames(&mut reader, Instant::now() + LAST_FRAMES).await;
            }
            next = async {
                match pending.take() {
                    Some(unit) => Some((Item::Unit(unit), false)),
                    None => watcher.next().await,
                }
            } => {
                let Some((item, _)) = next else {
                    drop(stdin);
                    break last_frames(&mut reader, deadline + LAST_FRAMES).await;
                };
                let Item::Unit(unit) = item else { continue };
                let ts = writer.picture(&unit.data, unit.timestamp_us * 9 / 100, unit.start());
                // A helper that stops reading holds the write, so it has
                // the clip's deadline too.
                let written = tokio::time::timeout_at(deadline.into(), stdin.write_all(&ts)).await;
                if !matches!(written, Ok(Ok(()))) {
                    break last_frames(&mut reader, deadline + LAST_FRAMES).await;
                }
            }
        }
    };
    let _ = child.kill().await;
    let said = diagnostics.await.unwrap_or_default();
    if jpegs.is_empty() {
        let said = said.trim();
        return Err(if said.is_empty() {
            format!("The capture helper made no picture from {}.", order.camera)
        } else {
            format!(
                "The capture helper made no picture from {}: {said}",
                order.camera
            )
        });
    }
    let bytes: u64 = jpegs.iter().map(|jpeg| jpeg.len() as u64).sum();
    if bytes > room {
        return Err(format!(
            "The pictures from {} passed the {} KB this look has room for after {} of {wanted} \
             frames, so UXDuck stopped looking. Ask for fewer frames or a shorter clip.",
            order.camera,
            room / 1024,
            jpegs.len()
        ));
    }
    if jpegs.len() < wanted {
        return Err(format!(
            "{} sent pictures for only part of the clip: {} of {wanted} frames in {} s.",
            order.camera,
            jpegs.len(),
            order.seconds
        ));
    }
    let interval_us = if order.frames > 1 {
        order.seconds * 1e6 / f64::from(order.frames - 1)
    } else {
        0.0
    };
    let frames = jpegs
        .into_iter()
        .enumerate()
        .map(|(k, jpeg)| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "microseconds into a clip, saturating"
            )]
            let offset_us = (interval_us * k as f64) as u64;
            let (width, height) = size(&jpeg);
            LookedFrame {
                offset_us,
                at_us: seen_at_us + offset_us,
                width,
                height,
                jpeg_bytes: jpeg.len() as u64,
                // Encoded here, on the look's task, so the engine's thread,
                // which controllers and commands share, only moves it.
                jpeg_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &jpeg,
                ),
            }
        })
        .collect();
    Ok(Looked {
        camera_width,
        camera_height,
        frames,
    })
}

/// The frames the helper has written by `until`.
async fn last_frames(
    reader: &mut tokio::task::JoinHandle<Vec<Vec<u8>>>,
    until: Instant,
) -> Vec<Vec<u8>> {
    tokio::time::timeout_at(until.into(), reader)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helper_reads_mpeg_ts_and_writes_scaled_jpegs_sampled_over_the_clip() {
        let one = args(1, 0.0, 1024, 576).join(" ");
        assert!(one.contains("-f mpegts -i pipe:0"));
        assert!(one.contains("-vf scale=1024:576:out_range=full,format=yuvj420p -frames:v 1"));
        assert!(one.ends_with("-c:v mjpeg -f mjpeg pipe:1"));
        let clip = args(8, 10.0, 640, 360).join(" ");
        assert!(clip.contains("-vf fps=7/10,scale=640:360"), "{clip}");
        assert!(clip.contains("-frames:v 8"));
    }

    /// A JPEG whose header and scan data hold bytes that look like markers.
    fn jpeg(scan: &[u8]) -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8];
        // A comment holding 0xFF 0xD9, which is no end.
        data.extend([0xFF, 0xFE, 0x00, 0x06, 0xFF, 0xD9, 0xFF, 0xD8]);
        data.extend([0xFF, 0xDA, 0x00, 0x04, 0x01, 0x02]);
        data.extend_from_slice(scan);
        data.extend([0xFF, 0xD9]);
        data
    }

    #[test]
    fn jpegs_are_split_where_each_ends_whatever_their_bytes_hold() {
        let a = jpeg(&[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56]);
        let b = jpeg(&[0xFF, 0x00, 0xFF, 0x00]);
        let mut pending = [a.clone(), b.clone(), b[..9].to_vec()].concat();
        assert_eq!(take_jpegs(&mut pending), vec![a, b.clone()]);
        // The third has not all arrived.
        assert_eq!(pending, b[..9].to_vec());
        pending.extend_from_slice(&b[9..]);
        assert_eq!(take_jpegs(&mut pending), vec![b]);
        assert!(pending.is_empty());
        assert_eq!(jpeg_end(b"not a jpeg"), None);
    }

    /// A camera's pictures: 3 s of H.264 at 25 fps, 1280 × 720, made by
    /// the helper, as access units.
    fn camera(program: &Path) -> Vec<Arc<uxduck_media::au::AccessUnit>> {
        let ts = std::process::Command::new(program)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=25:duration=3",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "25",
                "-bf",
                "0",
                "-f",
                "mpegts",
                "pipe:1",
            ])
            .output()
            .expect("the helper runs")
            .stdout;
        let mut source = uxduck_media::source::Source::new();
        let whole = ts.len() / 188 * 188;
        source
            .push(&ts[..whole])
            .into_iter()
            .filter_map(|output| match output {
                uxduck_media::source::Output::Unit(unit) => Some(unit),
                _ => None,
            })
            .collect()
    }

    async fn looks(frames: u32, seconds: f64) -> Result<Looked, String> {
        let program = super::super::capture::program().expect("pnpm capture:build first");
        let units = camera(&program);
        assert!(units.len() > 60, "{} pictures", units.len());
        let hub = Arc::new(Hub::new(std::env::temp_dir()));
        let watcher = hub.tap("look:test", "cam");
        let publisher = {
            let hub = hub.clone();
            tokio::spawn(async move {
                for unit in units {
                    hub.publish("cam", &Item::Unit(unit));
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
            })
        };
        let order = LookOrder {
            id: "lk1".into(),
            source_id: "cam".into(),
            camera: "Front".into(),
            frames,
            seconds,
            max_bytes: None,
        };
        let looked = look(&program, &watcher, &order).await;
        publisher.abort();
        looked
    }

    #[tokio::test]
    #[ignore = "needs the capture helper: pnpm capture:build, then cargo test -- --ignored"]
    async fn the_helper_makes_the_picture_now_and_a_clip_from_a_cameras_stream() {
        let one = looks(1, 0.0).await.unwrap();
        assert_eq!(
            (one.camera_width, one.camera_height),
            (Some(1280), Some(720))
        );
        assert_eq!(one.frames.len(), 1);
        let frame = &one.frames[0];
        assert_eq!((frame.width, frame.height), (1024, 576));
        assert!(frame.jpeg_base64.starts_with("/9j/"));
        assert_eq!(frame.offset_us, 0);

        let clip = looks(3, 2.0).await.unwrap();
        let offsets: Vec<u64> = clip.frames.iter().map(|f| f.offset_us).collect();
        assert_eq!(offsets, [0, 1_000_000, 2_000_000]);
        assert!(
            clip.frames
                .iter()
                .all(|f| (f.width, f.height) == (1024, 576))
        );
        // Frames a second apart of a moving test picture differ.
        assert_ne!(clip.frames[0].jpeg_base64, clip.frames[1].jpeg_base64);
    }

    /// A picture of `bytes`, more than a pipe holds.
    #[cfg(unix)]
    fn large(timestamp_us: u64) -> Arc<uxduck_media::au::AccessUnit> {
        Arc::new(uxduck_media::au::AccessUnit {
            arrived: None,
            data: [vec![0, 0, 0, 1, 0x65], vec![0x88; 256 * 1024]].concat(),
            key: true,
            recovery: false,
            timestamp_us,
            composition_us: 0,
            sps: uxduck_media::nal::SpsInfo {
                id: 0,
                codec: "avc1.64001f".into(),
                coded_width: Some(1280),
                coded_height: Some(720),
                display_width: Some(1280),
                display_height: Some(720),
            },
        })
    }

    /// A helper that is running but never reads its pictures cannot hold a
    /// look, and the slot the engine keeps for it, past the clip's deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_helper_that_stops_reading_ends_the_look_at_the_clips_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("ffmpeg");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hub = Arc::new(Hub::new(dir.path().to_path_buf()));
        let watcher = hub.tap("look:test", "cam");
        for k in 0..4 {
            hub.publish("cam", &Item::Unit(large(k * 40_000)));
        }
        let order = LookOrder {
            id: "lk1".into(),
            source_id: "cam".into(),
            camera: "Front".into(),
            frames: 1,
            seconds: 0.0,
            max_bytes: None,
        };
        let began = Instant::now();
        let looked = tokio::time::timeout(
            SLACK + LAST_FRAMES + Duration::from_secs(5),
            look(&program, &watcher, &order),
        )
        .await
        .expect("the look ends without the helper reading");
        assert_eq!(
            looked.unwrap_err(),
            "The capture helper made no picture from Front."
        );
        assert!(began.elapsed() >= SLACK);
    }

    /// A long clip whose frames pass its room is stopped as soon as they
    /// do, not made to its end.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_clip_that_passes_its_room_is_stopped_early_in_words() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("ffmpeg");
        // A JPEG of about 20 KB: a comment segment between its markers.
        let comment = vec![b'x'; 20_000];
        let length = u16::try_from(comment.len() + 2).unwrap().to_be_bytes();
        let jpeg = [
            &[0xFF, 0xD8, 0xFF, 0xFE][..],
            &length,
            &comment,
            &[0xFF, 0xD9],
        ]
        .concat();
        std::fs::write(dir.path().join("frame.jpg"), &jpeg).unwrap();
        // It reads the camera's stream and writes JPEGs until it is stopped.
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n(while :; do cat '{}' || exit 0; done) &\nexec cat >/dev/null\n",
                dir.path().join("frame.jpg").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hub = Arc::new(Hub::new(dir.path().to_path_buf()));
        let watcher = hub.tap("look:test", "cam");
        for k in 0..4 {
            hub.publish("cam", &Item::Unit(large(k * 40_000)));
        }
        let order = LookOrder {
            id: "lk1".into(),
            source_id: "cam".into(),
            camera: "Front".into(),
            frames: 20,
            seconds: 3600.0,
            max_bytes: Some(300_000),
        };
        let began = Instant::now();
        let looked = tokio::time::timeout(Duration::from_secs(5), look(&program, &watcher, &order))
            .await
            .expect("stopped long before the clip's end");
        let why = looked.unwrap_err();
        assert!(
            why.starts_with(
                "The pictures from Front passed the 292 KB this look has room for after 15 of 20 frames"
            ),
            "{why}"
        );
        assert!(
            why.ends_with(", so UXDuck stopped looking. Ask for fewer frames or a shorter clip."),
            "{why}"
        );
        assert!(began.elapsed() < Duration::from_secs(5));
    }
}

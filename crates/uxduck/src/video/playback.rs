//! Playing a recording kept as MPEG-TS (S151, ADR-0017 V23): browsers play
//! MP4, not MPEG-TS, so each recording is converted once, with the code
//! that wrote recordings before (about 1 GB/s, nothing decoded), into the
//! app folder's `cache/playback`, as it ends or when it is first opened.
//! The cache is UXDuck's to throw away: a copy unused for two weeks goes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use uxduck_media::record::Recorder;
use uxduck_media::source::{Output, Source};
use uxduck_media::ts::PACKET_SIZE;

/// How long a converted recording is kept without being played.
pub const UNUSED_FOR: Duration = Duration::from_hours(14 * 24);

/// The playback cache beside the media folder: `<app folder>/cache/playback`.
#[must_use]
pub fn cache_dir(media: &Path) -> PathBuf {
    media
        .parent()
        .unwrap_or(media)
        .join("cache")
        .join("playback")
}

/// Where a recording's MP4 for playback is kept.
#[must_use]
pub fn cached(cache: &Path, recording_id: &str) -> PathBuf {
    cache.join(format!("{recording_id}.mp4"))
}

/// Converts `ts` into `mp4`, unless a copy newer than it is there: the
/// pictures and sound into MP4 without re-encoding, written beside and
/// renamed into place, so a half-made copy is never played.
///
/// # Errors
/// The recording cannot be read or the copy written.
pub fn convert(ts: &Path, mp4: &Path) -> std::io::Result<()> {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(copy), Some(original)) = (modified(mp4), modified(ts))
        && copy >= original
    {
        return Ok(());
    }
    if let Some(dir) = mp4.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut input = std::fs::File::open(ts)?;
    let mut source = Source::new();
    let mut recorder = Recorder::with_sound();
    let mut out = Vec::new();
    let mut buffer = vec![0u8; PACKET_SIZE * 7 * 512];
    let mut pending = Vec::new();
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..n]);
        let whole = pending.len() / PACKET_SIZE * PACKET_SIZE;
        for chunk in pending[..whole].chunks(PACKET_SIZE * 7) {
            for output in source.push(chunk) {
                match output {
                    Output::Unit(unit) => out.extend(recorder.push(unit)),
                    Output::Audio(unit) => out.extend(recorder.push_audio(unit)),
                    Output::Klv { .. } => {}
                }
            }
        }
        pending.drain(..whole);
    }
    out.extend(recorder.finish());
    if out.is_empty() {
        return Err(std::io::Error::other(
            "the recording has no picture to play",
        ));
    }
    let partial = mp4.with_extension("mp4.part");
    std::fs::write(&partial, &out)?;
    std::fs::rename(&partial, mp4)
}

/// A clip of a recording kept as MPEG-TS, as MP4, and where it begins and
/// ends in the recording's media.
pub struct Clip {
    pub mp4: Vec<u8>,
    /// Where its first picture is: the last one a decoder can start from at
    /// or before the start asked for, so up to one keyframe interval early.
    pub from_us: u64,
    /// Where its last picture ends: that picture starts before the end asked
    /// for, so this may be a frame past it, or earlier when the recording ends first.
    pub to_us: u64,
}

/// Cuts `from_us` to `to_us` of a recording kept as MPEG-TS into MP4
/// without re-encoding, its times counted from its first picture as the
/// playable copy counts them. Nothing is decoded, so it starts at the last
/// picture a decoder can start from at or before `from_us`.
///
/// # Errors
/// The recording cannot be read, or has no picture in the range.
pub fn clip(ts: &Path, from_us: u64, to_us: u64) -> std::io::Result<Clip> {
    let mut input = std::fs::File::open(ts)?;
    let mut source = Source::new();
    let mut recorder = Recorder::with_sound();
    let mut out = Vec::new();
    let mut first_us = None;
    let (mut start_us, mut last_us) = (None, 0);
    let mut buffer = vec![0u8; PACKET_SIZE * 7 * 512];
    let mut pending = Vec::new();
    'read: loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..n]);
        let whole = pending.len() / PACKET_SIZE * PACKET_SIZE;
        for chunk in pending[..whole].chunks(PACKET_SIZE * 7) {
            for output in source.push(chunk) {
                match output {
                    Output::Unit(unit) => {
                        if first_us.is_none() && unit.start() {
                            first_us = Some(unit.timestamp_us);
                        }
                        let Some(first) = first_us else { continue };
                        let at = unit.timestamp_us.saturating_sub(first);
                        if at >= to_us {
                            break 'read;
                        }
                        // Each place to start before the clip's start begins it again.
                        if unit.start() && at <= from_us {
                            recorder = Recorder::with_sound();
                            out.clear();
                            start_us = Some(at);
                        }
                        if start_us.is_some() {
                            last_us = at;
                            out.extend(recorder.push(unit));
                        }
                    }
                    Output::Audio(unit) => out.extend(recorder.push_audio(unit)),
                    Output::Klv { .. } => {}
                }
            }
        }
        pending.drain(..whole);
    }
    out.extend(recorder.finish());
    match start_us {
        Some(start) if !out.is_empty() && last_us >= from_us => Ok(Clip {
            to_us: start + uxduck_media::mp4::duration_us(&out),
            from_us: start,
            mp4: out,
        }),
        _ => Err(std::io::Error::other(
            "the recording has no picture in this part",
        )),
    }
}

/// Where a video added from elsewhere (S208) keeps its playable copy:
/// named after the file's path, length and time, so a changed file is
/// made again.
#[must_use]
pub fn added_copy(cache: &Path, path: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(meta) = std::fs::metadata(path) {
        meta.len().hash(&mut hasher);
        meta.modified().ok().hash(&mut hasher);
    }
    cache.join(format!("added-{:016x}.mp4", hasher.finish()))
}

/// One copy made at a time, for the HTTP route and for adding alike.
static PREPARING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Makes the playable copy `mp4` of a video added from elsewhere, unless
/// one is there: its pictures and sound put into MP4, from 192-byte
/// packets too, or its pictures encoded again as H.264 by the media helper
/// when a browser cannot play them, without the sound.
///
/// # Errors
/// The file cannot be read, the media helper is missing or fails, or the
/// copy cannot be written.
pub fn prepare(path: &Path, play: super::created::Play, mp4: &Path) -> std::io::Result<()> {
    use super::created::Play;
    let _one_at_a_time = PREPARING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if mp4.is_file() {
        return Ok(());
    }
    if let Some(dir) = mp4.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let ts = mp4.with_extension("ts.part");
    let made = match play {
        Play::Direct => return Ok(()),
        Play::Remux => return convert(path, mp4),
        Play::Remux192 => {
            let mut input = std::fs::File::open(path)?;
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes)?;
            let start = (0..192.min(bytes.len()))
                .find(|&i| bytes.get(i + 4) == Some(&0x47))
                .unwrap_or(0);
            let stripped: Vec<u8> = bytes[start..]
                .as_chunks::<192>()
                .0
                .iter()
                .flat_map(|p| p[4..].iter().copied())
                .collect();
            std::fs::write(&ts, stripped)
        }
        Play::Transcode => {
            let helper = super::capture::program().map_err(std::io::Error::other)?;
            let output = std::process::Command::new(helper)
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
                .arg(path)
                .args([
                    "-map", "0:v:0", "-an", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20",
                    "-pix_fmt", "yuv420p", "-bf", "0", "-g", "50", "-f", "mpegts",
                ])
                .arg(&ts)
                .stdin(std::process::Stdio::null())
                .output()?;
            if output.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "the media helper could not convert the video: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )))
            }
        }
    };
    let converted = made.and_then(|()| convert(&ts, mp4));
    let _ = std::fs::remove_file(&ts);
    converted
}

/// Starts making a playable copy now, on a thread of its own: said in the
/// log if it fails, and tried again when the video is first played.
pub fn prepare_soon(cache: &Path, path: &Path, play: super::created::Play) {
    let (mp4, path) = (added_copy(cache, path), path.to_owned());
    std::thread::spawn(move || {
        if let Err(e) = prepare(&path, play, &mp4) {
            tracing::warn!(path = %path.display(), "added video not prepared: {e}");
        }
    });
}

/// Marks a copy as played now, so the cache keeps it.
pub fn mark_used(mp4: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(mp4) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Removes copies unused for `unused_for`, and half-made ones: what it freed.
pub fn clean(cache: &Path, unused_for: Duration) -> u64 {
    let now = SystemTime::now();
    let mut freed = 0;
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age > unused_for);
        let partial = entry.path().extension().is_some_and(|e| e == "part");
        if (old || partial) && std::fs::remove_file(entry.path()).is_ok() {
            freed += meta.len();
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three seconds of H.264 at 25 pictures a second, a keyframe each
    /// second, as UXDuck's own MPEG-TS writer makes it: the fixtures are
    /// private validation files, not part of the exported source.
    fn three_seconds() -> Vec<u8> {
        use uxduck_media::nal::Codec;
        use uxduck_media::tsmux::TsWriter;
        let sps = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10,
        ];
        let key = [
            &[0, 0, 0, 1][..],
            &sps,
            &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
            &[0, 0, 0, 1, 0x65],
            &[7u8; 900],
        ]
        .concat();
        let delta = [&[0, 0, 0, 1, 0x41][..], &[3u8; 300]].concat();
        let mut writer = TsWriter::new(Codec::H264, false);
        (0..=75u64)
            .flat_map(|n| {
                let key_frame = n % 25 == 0;
                writer.picture(
                    if key_frame { &key } else { &delta },
                    90_000 + n * 3_600,
                    key_frame,
                )
            })
            .collect()
    }

    #[test]
    fn a_ts_recording_becomes_a_playable_mp4_once() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("media/r.ts");
        std::fs::create_dir_all(ts.parent().unwrap()).unwrap();
        std::fs::write(&ts, three_seconds()).unwrap();
        let cache = cache_dir(&dir.path().join("media"));
        assert_eq!(cache, dir.path().join("cache/playback"));
        let mp4 = cached(&cache, "vr1");
        convert(&ts, &mp4).unwrap();
        let data = std::fs::read(&mp4).unwrap();
        assert_eq!(uxduck_media::mp4::complete_length(&data), data.len());
        assert!(uxduck_media::mp4::duration_us(&data) >= 2_900_000);
        // A copy newer than the recording is not made again.
        let made = std::fs::metadata(&mp4).unwrap().modified().unwrap();
        convert(&ts, &mp4).unwrap();
        assert_eq!(std::fs::metadata(&mp4).unwrap().modified().unwrap(), made);
    }

    #[test]
    fn a_clip_starts_at_the_keyframe_before_it_and_ends_where_asked() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("r.ts");
        std::fs::write(&ts, three_seconds()).unwrap();
        // 1.4 s to 2.2 s: the keyframe at 1 s is the place to start.
        let cut = clip(&ts, 1_400_000, 2_200_000).unwrap();
        assert_eq!(cut.from_us, 1_000_000);
        assert_eq!(uxduck_media::mp4::complete_length(&cut.mp4), cut.mp4.len());
        let length = uxduck_media::mp4::duration_us(&cut.mp4);
        assert!((1_150_000..=1_250_000).contains(&length), "{length}");
        assert_eq!(cut.to_us, cut.from_us + length);
        // Past the recording's end, it ends with the recording.
        let tail = clip(&ts, 2_500_000, 60_000_000).unwrap();
        assert_eq!(tail.from_us, 2_000_000);
        assert!(tail.to_us <= 3_100_000, "{}", tail.to_us);
        // After its end there is nothing to cut.
        assert!(clip(&ts, 10_000_000, 20_000_000).is_err());
    }

    #[test]
    fn a_ts_with_no_picture_says_so_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("empty.ts");
        std::fs::write(&ts, [0x47u8; 188]).unwrap();
        let mp4 = dir.path().join("cache/e.mp4");
        assert!(convert(&ts, &mp4).is_err());
        assert!(!mp4.exists());
    }

    #[test]
    fn copies_unused_for_long_and_half_made_ones_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (old, fresh, part) = (
            dir.path().join("old.mp4"),
            dir.path().join("fresh.mp4"),
            dir.path().join("x.mp4.part"),
        );
        for p in [&old, &fresh, &part] {
            std::fs::write(p, b"12345").unwrap();
        }
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_hours(20 * 24))
            .unwrap();
        assert_eq!(clean(dir.path(), UNUSED_FOR), 10);
        assert!(!old.exists() && !part.exists() && fresh.exists());
    }
}

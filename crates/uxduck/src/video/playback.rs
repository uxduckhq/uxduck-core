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

/// How long a token's copy of a recording still being written is kept
/// unasked for while a newer token of the same recording has been asked
/// for since (FB-243). Each Camera tab opened gets a token, and on a
/// recording still being written each token holds a whole copy, so the
/// copies of tabs since closed would fill the disk over a long armed
/// session. The newest token's copy stays for as long as the token lasts.
pub const PIN_IDLE: Duration = Duration::from_mins(5);

/// How often tokens' copies are looked over (FB-243).
pub const PIN_SWEEP_EVERY: Duration = Duration::from_mins(1);

/// The name a playback token plays a recording's copy by (FB-243): a
/// second name for the copy it was first given. A recording still being
/// written is copied again as it grows, and a `<video>` reading its ranges
/// from two different copies cannot decode them.
#[must_use]
pub fn pinned(cache: &Path, recording_id: &str, token: &str) -> PathBuf {
    cache.join(format!("{recording_id}.{token}.mp4"))
}

/// The recording and token a file in the cache is a token's copy for:
/// exactly `<id>.vp<32 hex>.mp4`, as [`pinned`] names it.
fn pin_of(name: &str) -> Option<(&str, &str)> {
    let (id, token) = name.strip_suffix(".mp4")?.split_once('.')?;
    let hex = token.strip_prefix("vp")?;
    let exact = !id.is_empty()
        && !id.contains('.')
        && hex.len() == 32
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    exact.then_some((id, token))
}

/// The copy of `ts` a playback token plays, by its name `pinned`: made
/// as [`convert`] makes it on the token's first request, then the same
/// bytes for every later one, however much the recording has grown since.
/// A second name for the shared copy, or a copy of it on a disk that
/// cannot give a file a second name, such as exFAT, said once in the log.
///
/// `had_copy`: the token was given its copy before. When that copy is
/// gone, swept as unneeded, `None`: never other bytes under the same
/// token, which a `<video>` could not play on from, so the player asks
/// for the recording again. Never at once with [`sweep_pins`].
///
/// # Errors
/// The recording cannot be read or the copy written, or the disk refused
/// the second name for another reason than having none, such as being full.
pub fn pin(
    ts: &Path,
    mp4: &Path,
    pinned: &Path,
    had_copy: bool,
) -> std::io::Result<Option<PathBuf>> {
    let _one_at_a_time = PINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Not marked used again: the copy's time says when it was made, and a
    // token's later requests must not make it look newer than the
    // recording it was made from.
    if pinned.is_file() {
        return Ok(Some(pinned.to_owned()));
    }
    if had_copy {
        return Ok(None);
    }
    convert(ts, mp4)?;
    if let Err(e) = std::fs::hard_link(mp4, pinned) {
        if !copied_instead(&e) {
            return Err(e);
        }
        COPIED_SAID.call_once(|| {
            tracing::warn!(
                cache = %pinned.parent().unwrap_or(pinned).display(),
                "this disk cannot give a playback copy a second name, so a recording still being written is copied for each player instead: {e}"
            );
        });
        let partial = partial(pinned);
        if let Err(e) = std::fs::copy(mp4, &partial).and_then(|_| std::fs::rename(&partial, pinned))
        {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    }
    mark_used(pinned);
    Ok(Some(pinned.to_owned()))
}

/// Tokens' copies given or swept one at a time (FB-243), so a sweep never
/// removes a copy between a request finding it and serving it.
static PINS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Said once in the log: tokens' copies are copied, not linked.
static COPIED_SAID: std::sync::Once = std::sync::Once::new();

/// Whether a disk that refused a second name for a copy has none to give:
/// exFAT and FAT say so as not supported (`ENOTSUP`, errno 45 on macOS,
/// or `EOPNOTSUPP`, which the standard library calls uncategorized, and
/// `ERROR_INVALID_FUNCTION` or `ERROR_NOT_SUPPORTED` on Windows), another
/// volume as crossing devices, and some file systems as not permitted. A
/// full disk or any other error is passed on.
fn copied_instead(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{CrossesDevices, PermissionDenied, Unsupported};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let none = [libc::ENOTSUP, libc::EOPNOTSUPP];
    #[cfg(windows)]
    let none = [1, 50];
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let none: [i32; 0] = [];
    matches!(e.kind(), Unsupported | CrossesDevices | PermissionDenied)
        || e.raw_os_error().is_some_and(|code| none.contains(&code))
}

/// A name beside `path` to write a copy under before it is renamed into
/// place: its own for each writer, so two copies made at once, by the
/// recorder as a recording ends, a player and the uploader, never write
/// into one file. `.part`, so one left half-made is removed.
fn partial(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}-{n}.part", std::process::id()));
    PathBuf::from(name)
}

/// Removes tokens' copies that are not needed (FB-243): a token's that
/// `asked` no longer knows, and one unasked for over `idle` while a newer
/// token of the same recording has been asked for since. What it removed.
pub fn sweep_pins(
    cache: &Path,
    asked: impl Fn(&str) -> Option<std::time::Instant>,
    idle: Duration,
) -> usize {
    let _one_at_a_time = PINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut kept: Vec<(String, PathBuf, std::time::Instant)> = Vec::new();
    let mut removed = 0;
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some((id, token)) = name.to_str().and_then(pin_of) else {
            continue;
        };
        match asked(token) {
            Some(at) => kept.push((id.to_owned(), entry.path(), at)),
            None => removed += usize::from(std::fs::remove_file(entry.path()).is_ok()),
        }
    }
    for (id, path, at) in &kept {
        let newer = kept.iter().any(|(other, _, then)| other == id && then > at);
        if newer && at.elapsed() > idle {
            removed += usize::from(std::fs::remove_file(path).is_ok());
        }
    }
    removed
}

/// Removes a recording's copies: its own and its tokens' (FB-243).
pub fn remove_copies(cache: &Path, recording_id: &str) {
    let _ = std::fs::remove_file(cached(cache, recording_id));
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(pin_of)
            .is_some_and(|(id, _)| id == recording_id)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
    if let Some((at, why)) = NOT_IN_PLACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(mp4)
        .filter(|(at, _)| at.elapsed() < PUT_IN_PLACE_AGAIN_AFTER)
    {
        return Err(std::io::Error::other(format!(
            "the playback copy could not be put in place {} s ago: {why}",
            at.elapsed().as_secs()
        )));
    }
    if let Some(dir) = mp4.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = partial(mp4);
    let written = std::fs::File::create(&partial).and_then(|file| {
        let mut out = std::io::BufWriter::new(file);
        convert_into(ts, &mut out)?;
        out.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let mut failed = NOT_IN_PLACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(e) = std::fs::rename(&partial, mp4) {
        let _ = std::fs::remove_file(&partial);
        tracing::warn!(copy = %mp4.display(), "a playback copy could not be put in place: {e}");
        failed.insert(mp4.to_owned(), (std::time::Instant::now(), e.to_string()));
        return Err(e);
    }
    failed.remove(mp4);
    Ok(())
}

/// How long a copy that could not be renamed into place, as on Windows
/// while a player still has the old one open, is not made again: each
/// try writes a whole copy, gigabytes for a recording of hours, and
/// players ask for one on every range they read.
pub const PUT_IN_PLACE_AGAIN_AFTER: Duration = Duration::from_secs(30);

/// Copies that could not be put in place lately: when, and why.
static NOT_IN_PLACE: std::sync::Mutex<
    std::collections::BTreeMap<PathBuf, (std::time::Instant, String)>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Converts `ts` into MP4 written to `out` as it is made, so none of it
/// is held in memory: a flight's file (ADR-0062) takes it straight into
/// itself, with no copy on this computer's disk. Until 2026-11-17 the
/// whole copy was held in memory, so a recording of hours took that much,
/// and a file's copies were made in the playback cache first. The bytes
/// written.
///
/// # Errors
/// The recording cannot be read, has no picture, or `out` refused it.
pub fn convert_into(ts: &Path, out: &mut dyn std::io::Write) -> std::io::Result<u64> {
    let mut input = std::fs::File::open(ts)?;
    let mut source = Source::new();
    let mut recorder = Recorder::with_sound();
    let mut written = 0u64;
    let mut put = |bytes: Vec<u8>| -> std::io::Result<()> {
        written += bytes.len() as u64;
        out.write_all(&bytes)
    };
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
                    Output::Unit(unit) => put(recorder.push(unit))?,
                    Output::Audio(unit) => put(recorder.push_audio(unit))?,
                    Output::Klv { .. } => {}
                }
            }
        }
        pending.drain(..whole);
    }
    put(recorder.finish())?;
    if written == 0 {
        return Err(std::io::Error::other(
            "the recording has no picture to play",
        ));
    }
    Ok(written)
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
pub(crate) mod tests {
    use super::*;

    /// Three seconds of H.264 at 25 pictures a second, a keyframe each
    /// second, as UXDuck's own MPEG-TS writer makes it: the fixtures are
    /// private validation files, not part of the exported source.
    pub(crate) fn three_seconds() -> Vec<u8> {
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

    /// A playback token as the engine makes one: `vp` and 32 hex digits.
    fn token(digit: char) -> String {
        format!("vp{}", digit.to_string().repeat(32))
    }

    #[test]
    fn a_token_plays_the_copy_it_was_first_given_while_the_recording_grows() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("media/r.ts");
        std::fs::create_dir_all(ts.parent().unwrap()).unwrap();
        let whole = three_seconds();
        let packets = whole.len() / PACKET_SIZE;
        std::fs::write(&ts, &whole[..packets / 2 * PACKET_SIZE]).unwrap();
        let cache = cache_dir(&dir.path().join("media"));
        let mp4 = cached(&cache, "vr1");
        let (a, b) = (token('a'), token('b'));
        let first = pin(&ts, &mp4, &pinned(&cache, "vr1", &a), false)
            .unwrap()
            .unwrap();
        let given = std::fs::read(&first).unwrap();
        // Still being written: the recording grows past its copy.
        std::fs::write(&ts, &whole).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&ts)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap();
        let again = pin(&ts, &mp4, &pinned(&cache, "vr1", &a), true)
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&again).unwrap(), given);
        let second = pin(&ts, &mp4, &pinned(&cache, "vr1", &b), false)
            .unwrap()
            .unwrap();
        let grown = std::fs::read(&second).unwrap();
        assert!(grown.len() > given.len(), "{} {}", grown.len(), given.len());
        // The first token's bytes stay as they were.
        assert_eq!(std::fs::read(&first).unwrap(), given);
        // Swept: the token is not given other bytes, so its player asks again.
        let kept = std::fs::read(&second).unwrap();
        std::fs::remove_file(&second).unwrap();
        assert_eq!(
            pin(&ts, &mp4, &pinned(&cache, "vr1", &b), true).unwrap(),
            None
        );
        assert!(!second.exists());
        std::fs::write(&second, kept).unwrap();
        // Deleted: the recording's copy and its tokens' go together.
        let other = pinned(&cache, "vr2", &a);
        std::fs::write(&other, b"x").unwrap();
        remove_copies(&cache, "vr1");
        assert!(!mp4.exists() && !first.exists() && !second.exists());
        assert!(other.exists());
    }

    #[test]
    fn tokens_copies_go_once_their_token_is_gone_or_a_newer_one_plays_on() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let now = std::time::Instant::now();
        let ago = |minutes: u64| now.checked_sub(Duration::from_mins(minutes)).unwrap();
        let (gone, closed, open, paused, newest) =
            (token('0'), token('1'), token('2'), token('3'), token('4'));
        let path = |id: &str, t: &str| pinned(cache, id, t);
        for (id, t) in [
            ("vr1", &gone),
            ("vr1", &closed),
            ("vr1", &open),
            ("vr1", &newest),
            ("vr2", &paused),
        ] {
            std::fs::write(path(id, t), b"x").unwrap();
        }
        // Not tokens' copies: left alone, as clean leaves them.
        let others = [
            cache.join("vr1.mp4"),
            cache.join("added-0123.mp4"),
            cache.join("vr1.vpzz.mp4"),
            cache.join("a.b.mp4"),
        ];
        for other in &others {
            std::fs::write(other, b"x").unwrap();
        }
        let asked = |t: &str| {
            [
                (closed.as_str(), ago(10)),
                (open.as_str(), ago(1)),
                (newest.as_str(), ago(0)),
                (paused.as_str(), ago(30)),
            ]
            .into_iter()
            .find_map(|(k, at)| (k == t).then_some(at))
        };
        assert_eq!(sweep_pins(cache, asked, PIN_IDLE), 2);
        // An expired token's, and a closed tab's once a newer one plays on.
        assert!(!path("vr1", &gone).exists() && !path("vr1", &closed).exists());
        // Asked for lately, the newest, and the only one of its recording.
        assert!(path("vr1", &open).exists() && path("vr1", &newest).exists());
        assert!(path("vr2", &paused).exists());
        assert!(others.iter().all(|o| o.exists()));
        assert_eq!(clean(cache, UNUSED_FOR), 0);
        assert!(others.iter().all(|o| o.exists()) && path("vr2", &paused).exists());
    }

    #[test]
    fn copies_made_at_once_never_write_into_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("r.ts");
        std::fs::write(&ts, three_seconds()).unwrap();
        let alone = dir.path().join("alone.mp4");
        convert(&ts, &alone).unwrap();
        let want = std::fs::read(&alone).unwrap();
        let mp4 = dir.path().join("cache/vr1.mp4");
        for round in 0..10 {
            // Newer than any copy, so each writer makes it again.
            std::fs::File::options()
                .write(true)
                .open(&ts)
                .unwrap()
                .set_modified(SystemTime::now() + Duration::from_hours(round + 1))
                .unwrap();
            let writers: Vec<_> = (0..4)
                .map(|_| {
                    let (ts, mp4) = (ts.clone(), mp4.clone());
                    std::thread::spawn(move || convert(&ts, &mp4))
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
            assert!(std::fs::read(&mp4).unwrap() == want, "round {round}");
        }
        let left: Vec<_> = std::fs::read_dir(dir.path().join("cache"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["vr1.mp4"]);
    }

    #[test]
    fn a_copy_that_cannot_be_put_in_place_leaves_nothing_and_waits_before_trying_again() {
        let dir = tempfile::tempdir().unwrap();
        let ts = dir.path().join("r.ts");
        std::fs::write(&ts, three_seconds()).unwrap();
        // A folder where the copy goes, with a file in it: renaming onto it
        // fails, as renaming over a copy a player holds open does on Windows.
        let mp4 = dir.path().join("cache/vr1.mp4");
        std::fs::create_dir_all(&mp4).unwrap();
        std::fs::write(mp4.join("held"), b"x").unwrap();
        // The recording newer than what is in the way, so a copy is made.
        std::fs::File::options()
            .write(true)
            .open(&ts)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_hours(1))
            .unwrap();
        let first = convert(&ts, &mp4).unwrap_err();
        let left = || {
            std::fs::read_dir(dir.path().join("cache"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        };
        assert_eq!(left(), ["vr1.mp4"], "{first}");
        // Asked again at once: no second copy is written.
        let again = convert(&ts, &mp4).unwrap_err();
        assert!(
            again.to_string().contains("could not be put in place"),
            "{again}"
        );
        assert_eq!(left(), ["vr1.mp4"]);
        // Once it can be, it is.
        std::fs::remove_dir_all(&mp4).unwrap();
        NOT_IN_PLACE.lock().unwrap().remove(&mp4);
        convert(&ts, &mp4).unwrap();
        assert!(mp4.is_file());
    }

    #[test]
    fn only_a_disk_with_no_second_names_gets_a_copy_instead() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::Unsupported,
            ErrorKind::CrossesDevices,
            ErrorKind::PermissionDenied,
        ] {
            assert!(copied_instead(&Error::from(kind)), "{kind:?}");
        }
        for kind in [
            ErrorKind::StorageFull,
            ErrorKind::NotFound,
            ErrorKind::AlreadyExists,
        ] {
            assert!(!copied_instead(&Error::from(kind)), "{kind:?}");
        }
        // exFAT on macOS.
        #[cfg(target_os = "macos")]
        assert!(copied_instead(&Error::from_raw_os_error(45)));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(copied_instead(&Error::from_raw_os_error(libc::EOPNOTSUPP)));
            assert!(!copied_instead(&Error::from_raw_os_error(libc::ENOSPC)));
        }
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

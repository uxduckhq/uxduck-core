//! The daemon's side of a replay (ADR-0039): the tlog records it plays,
//! read from the links' tlogs or a file, and the timer that runs its clock
//! while one plays and sleeps while none does.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use uxduck_engine::runtime::{Clock, LinkLog, ReplayRead, ReplayWork};

use crate::adapters::system::SystemClock;
use crate::daemon::engine_task::EngineHandle;

/// The most a replay holds, some ten hours of a busy link. The whole of what
/// a replay plays is in memory, where the engine goes through it and seeks
/// in it, so a longer log could take the computer's memory and UXDuck with
/// it. A longer one is refused in [`too_long`]'s words, with its size,
/// never cut short. A flight's file (ADR-0062) is not held in memory, so
/// [`stream`] writes its whole log however long it is.
const MAX_BYTES: u64 = 1 << 30;

#[cfg(test)]
thread_local! {
    /// A smaller bound for a test, so a log over it needs no gigabyte.
    static TEST_MOST: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The most a replay holds: [`MAX_BYTES`], or a test's own bound.
fn most() -> u64 {
    #[cfg(test)]
    if let Some(most) = TEST_MOST.with(std::cell::Cell::get) {
        return most;
    }
    MAX_BYTES
}

/// Runs `f` with a replay holding `most` bytes at most.
#[cfg(test)]
pub fn with_most<T>(most: u64, f: impl FnOnce() -> T) -> T {
    TEST_MOST.with(|m| m.set(Some(most)));
    let out = f();
    TEST_MOST.with(|m| m.set(None));
    out
}

/// A size in the words the bound is named in: gigabytes of 1,024 MB, so
/// [`MAX_BYTES`] is "1 GB".
fn gigabytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a size in words")]
    let gb = bytes as f64 / (1u64 << 30) as f64;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 0.1 {
        let tenths = format!("{gb:.1}");
        format!("{} GB", tenths.strip_suffix(".0").unwrap_or(&tenths))
    } else {
        format!("{} MB", bytes.div_ceil(1 << 20))
    }
}

/// What a person is told of a log longer than a replay holds, with its
/// measured size. `what` is "flight's log" or "file".
fn too_long(what: &str, bytes: u64) -> String {
    format!(
        "This {what} is {}. A replay is held in memory and takes {} at most.",
        gigabytes(bytes),
        gigabytes(most())
    )
}
/// Why a log gave nothing: it has no record in the span asked for. One of
/// several connections a flight was heard on (`ReplayRead::Merged`) may
/// have heard nothing then, and adds nothing.
const NOT_HERE: &str = "This flight's log is not on this computer any more.";
/// Read before a flight's start offset for its lead-in: ten seconds of a
/// busy link with room to spare.
const LEAD_BYTES: u64 = 8 << 20;
/// How often the clock steps while a replay plays.
const STEP: Duration = Duration::from_millis(50);

static PLAYING: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);

/// The engine says whether a replay plays.
pub fn running(on: bool) {
    PLAYING.send_replace(on);
}

/// Steps the replay's clock while one plays; waits otherwise.
pub async fn clock(engine: EngineHandle) {
    let mut playing = PLAYING.subscribe();
    loop {
        if !*playing.borrow_and_update() {
            if playing.changed().await.is_err() {
                return;
            }
            continue;
        }
        let mut step = tokio::time::interval(STEP);
        step.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while *playing.borrow() {
            step.tick().await;
            crate::wakes::count("replay: clock (20 Hz while playing)");
            let now = SystemClock.now_us();
            if engine.run(move |e| e.replay_tick(now)).await.is_err() {
                return;
            }
        }
    }
}

/// Does the engine's work on replays' logs off its task (ADR-0039, long logs):
/// each on a thread of its own, since one that gave way stops by itself
/// and a log's values are read while another opens. What it made goes back
/// to the engine in order with everything else.
pub async fn work(mut works: mpsc::UnboundedReceiver<ReplayWork>, engine: EngineHandle) {
    while let Some(work) = works.recv().await {
        let engine = engine.clone();
        tokio::task::spawn_blocking(move || {
            let worked = work.run();
            engine.post(move |e| e.replay_worked(worked));
        });
    }
}

/// How long a log joined from the account is kept unused (R12).
const KEEP_UNUSED: Duration = Duration::from_hours(14 * 24);

/// A flight log in the account joined on this computer (R12): its chunks
/// into `cache/replay/<id>.tlog`, once, kept while it is used in the last
/// 14 days; others unused that long go.
///
/// # Errors
/// Not signed in, no such log, or a chunk that does not check out.
pub async fn account_log(
    data_dir: &Path,
    device_id: &str,
    id: &str,
) -> Result<PathBuf, uxduck_contracts::errors::ApiError> {
    use uxduck_contracts::errors::{ApiError, ErrorKind};
    let dir = data_dir.join("cache").join("replay");
    let _ = std::fs::create_dir_all(&dir);
    // Unused for 14 days: gone.
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > KEEP_UNUSED);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let safe: String = format!("{device_id}-{id}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.tlog"));
    if path.is_file() {
        // Used now: kept another 14 days.
        let _ = File::options()
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return Ok(path);
    }
    let part = path.with_extension("part");
    crate::uploader::download(
        data_dir,
        crate::uploader::Stored::Flights,
        device_id,
        id,
        &part,
    )
    .await
    .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
    std::fs::rename(&part, &path).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
    Ok(path)
}

/// A replay's records from the tlogs folder or a file.
///
/// # Errors
/// Why there are none, in words.
pub fn read(tlogs: &Path, what: &ReplayRead) -> Result<Vec<u8>, String> {
    match what {
        ReplayRead::VehicleFile { local } => {
            let relative = Path::new(local);
            if !relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err("Not a vehicle's file.".into());
            }
            let data_dir = tlogs.parent().unwrap_or(tlogs);
            let path = crate::vehicle_files::dir(data_dir).join(relative);
            if !path.is_file() {
                return Err("Download the file in Files first, then replay it.".into());
            }
            read(
                tlogs,
                &ReplayRead::File {
                    path: path.display().to_string(),
                },
            )
        }
        ReplayRead::LinkBefore {
            link_id,
            file,
            offset,
            bytes,
        } => {
            own(link_id, file)?;
            before(&tlogs.join(file), *offset, (*bytes).min(most()))
                .map_err(|e| format!("Cannot read this flight's log: {e}"))
        }
        ReplayRead::Merged { reads } => read_merged(tlogs, reads),
        ReplayRead::LinkFile {
            link_id,
            file,
            from_us,
            to_us,
        } => {
            own(link_id, file)?;
            let mut out = Vec::new();
            append_range(
                &tlogs.join(file),
                0,
                *from_us,
                *to_us,
                &mut Kept::new(&mut out, most()),
            )
            .map_err(|e| unread(tlogs, what, &e))?;
            if out.is_empty() {
                return Err(NOT_HERE.into());
            }
            Ok(out)
        }
        ReplayRead::File { path } => {
            let path = Path::new(path);
            let size = std::fs::metadata(path)
                .map_err(|e| format!("Cannot read {}: {e}", path.display()))?
                .len();
            if size > most() {
                return Err(too_long("file", size));
            }
            std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))
        }
        ReplayRead::Packed { path, entry } => {
            let file = Path::new(path);
            let entries = crate::flight_file::entries(file)
                .map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
            let found = crate::flight_file::find(&entries, entry)
                .ok_or_else(|| format!("{} has no {entry}.", file.display()))?;
            if found.size > most() {
                return Err(too_long("flight's log", found.size));
            }
            let mut out = Vec::with_capacity(usize::try_from(found.size).unwrap_or(0));
            crate::flight_file::reader(file, found)
                .and_then(|mut r| r.read_to_end(&mut out))
                .map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
            Ok(out)
        }
        ReplayRead::Link {
            link_id,
            from_us,
            to_us,
            start,
        } => {
            let mut bytes = Vec::new();
            link_records(
                tlogs,
                link_id,
                (*from_us, *to_us),
                start.as_ref(),
                &mut Kept::new(&mut bytes, most()),
            )
            .map_err(|e| unread(tlogs, what, &e))?;
            if bytes.is_empty() {
                return Err(NOT_HERE.into());
            }
            Ok(bytes)
        }
    }
}

/// What several connections' logs of a flight hold, merged (ADR-0039 R10,
/// 2026-11-22). One with nothing in the span adds nothing.
fn read_merged(tlogs: &Path, reads: &[ReplayRead]) -> Result<Vec<u8>, String> {
    let mut logs = Vec::new();
    for one in reads {
        match read(tlogs, one) {
            Ok(bytes) => logs.push(bytes),
            Err(why) if why == NOT_HERE => {}
            Err(why) => return Err(why),
        }
    }
    if logs.len() < 2 {
        return logs.pop().ok_or_else(|| NOT_HERE.into());
    }
    let logs: Vec<&[u8]> = logs.iter().map(Vec::as_slice).collect();
    let merged = uxduck_mavlink::tlog::merge(&logs);
    if merged.len() as u64 > most() {
        return Err(too_long("flight's log", merged.len() as u64));
    }
    Ok(merged)
}

/// Writes a flight's log to `out` as it is read, however long it is, for
/// its `.uxflight` (ADR-0062): nothing of it is held in memory, so the
/// bound on a replay does not apply. The bytes written.
///
/// # Errors
/// Why there is none, in words, or `out` refused it.
pub fn stream(tlogs: &Path, what: &ReplayRead, out: &mut dyn Write) -> Result<u64, String> {
    let written = match what {
        ReplayRead::Link {
            link_id,
            from_us,
            to_us,
            start,
        } => {
            let mut kept = Kept::new(out, u64::MAX);
            link_records(
                tlogs,
                link_id,
                (*from_us, *to_us),
                start.as_ref(),
                &mut kept,
            )
            .map_err(|e| format!("Cannot read this flight's log: {e}"))?;
            kept.bytes
        }
        ReplayRead::LinkFile {
            link_id,
            file,
            from_us,
            to_us,
        } => {
            own(link_id, file)?;
            let mut kept = Kept::new(out, u64::MAX);
            append_range(&tlogs.join(file), 0, *from_us, *to_us, &mut kept)
                .map_err(|e| format!("Cannot read this flight's log: {e}"))?;
            kept.bytes
        }
        ReplayRead::Packed { path, entry } => {
            let file = Path::new(path);
            let entries = crate::flight_file::entries(file)
                .map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
            let found = crate::flight_file::find(&entries, entry)
                .ok_or_else(|| format!("{} has no {entry}.", file.display()))?;
            crate::flight_file::reader(file, found)
                .and_then(|mut r| std::io::copy(&mut r, out))
                .map_err(|e| format!("Cannot read {}: {e}", file.display()))?
        }
        ReplayRead::File { path } => File::open(path)
            .and_then(|mut f| std::io::copy(&mut f, out))
            .map_err(|e| format!("Cannot read {path}: {e}"))?,
        ReplayRead::Merged { reads } => {
            let mut logs = Vec::new();
            for one in reads {
                let mut file = tempfile::tempfile()
                    .map_err(|e| format!("Cannot merge this flight's logs: {e}"))?;
                let mut writer = std::io::BufWriter::new(&mut file);
                let streamed = stream(tlogs, one, &mut writer);
                writer
                    .flush()
                    .map_err(|e| format!("Cannot merge this flight's logs: {e}"))?;
                drop(writer);
                match streamed {
                    Ok(_) => {}
                    Err(why) if why == NOT_HERE => continue,
                    Err(why) => return Err(why),
                }
                file.seek(SeekFrom::Start(0))
                    .map_err(|e| format!("Cannot merge this flight's logs: {e}"))?;
                logs.push(std::io::BufReader::new(file));
            }
            merge_streams(logs, out).map_err(|e| format!("Cannot merge this flight's logs: {e}"))?
        }
        ReplayRead::VehicleFile { .. } | ReplayRead::LinkBefore { .. } => {
            let bytes = read(tlogs, what)?;
            out.write_all(&bytes).map_err(|e| e.to_string())?;
            bytes.len() as u64
        }
    };
    if written == 0 {
        return Err(NOT_HERE.into());
    }
    Ok(written)
}

/// Why a flight's log gave no replay, in words: it is longer than a replay
/// holds, measured to say by how much, or it could not be read.
fn unread(tlogs: &Path, what: &ReplayRead, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::FileTooLarge {
        let measured = stream(tlogs, what, &mut std::io::sink()).unwrap_or(most() + 1);
        too_long("flight's log", measured)
    } else {
        format!("Cannot read this flight's log: {error}")
    }
}

/// Where a log's records go, and how many bytes of them may: a replay's
/// are held in memory and come to [`most`] at most, a file's are written
/// as they are read and have no bound.
struct Kept<'a> {
    out: &'a mut dyn Write,
    bytes: u64,
    most: u64,
}

impl<'a> Kept<'a> {
    fn new(out: &'a mut dyn Write, most: u64) -> Self {
        Self {
            out,
            bytes: 0,
            most,
        }
    }

    /// Adds one record.
    ///
    /// # Errors
    /// Past `most`, as [`std::io::ErrorKind::FileTooLarge`], or `out`
    /// refused it.
    fn keep(&mut self, record: &[u8]) -> std::io::Result<()> {
        self.bytes += record.len() as u64;
        if self.bytes > self.most {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        self.out.write_all(record)
    }
}

/// One of this link's own files, by name only.
fn own(link_id: &str, file: &str) -> Result<(), String> {
    if file.starts_with(&format!("{link_id}-")) && Path::new(file).components().count() == 1 {
        Ok(())
    } else {
        Err("Not this connection's log.".into())
    }
}

/// The whole records of the file at `path` in the `bytes` before
/// `offset`.
fn before(path: &Path, offset: u64, bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let end = offset.min(file.metadata()?.len());
    let from = end.saturating_sub(bytes);
    file.seek(SeekFrom::Start(from))?;
    let mut out = Vec::new();
    file.take(end - from).read_to_end(&mut out)?;
    let begin = if from == 0 {
        0
    } else {
        uxduck_mavlink::tlog::resync(&out).unwrap_or(out.len())
    };
    out.drain(..begin);
    Ok(out)
}

/// A link's recordings in the tlogs folder, oldest first (S348).
#[must_use]
pub fn link_logs(tlogs: &Path, link_id: &str) -> Vec<LinkLog> {
    link_files(tlogs, link_id)
        .into_iter()
        .filter_map(|(secs, path)| {
            Some(LinkLog {
                name: path.file_name()?.to_string_lossy().into_owned(),
                start_us: secs.saturating_mul(1_000_000),
                bytes: std::fs::metadata(&path).map_or(0, |m| m.len()),
            })
        })
        .collect()
}

/// Deletes one of a link's recordings, by name (S348). One already gone is
/// deleted.
///
/// # Errors
/// Not this link's file, or the file could not be deleted.
pub fn remove_link_log(tlogs: &Path, link_id: &str, name: &str) -> Result<(), String> {
    own(link_id, name)?;
    match std::fs::remove_file(tlogs.join(name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("Cannot delete {name}: {e}."))
        }
        _ => Ok(()),
    }
}

/// Merges logs read whole into files (`ReplayRead::Merged`), a record at
/// a time from each, as [`uxduck_mavlink::tlog::merge`] does in memory, so
/// a flight's file is written however long its logs are. The bytes written.
fn merge_streams(mut logs: Vec<impl Read>, out: &mut dyn Write) -> std::io::Result<u64> {
    let mut next: Vec<Option<(u64, Vec<u8>)>> = Vec::with_capacity(logs.len());
    for log in &mut logs {
        next.push(next_record(log)?);
    }
    let mut merge = uxduck_mavlink::tlog::Merge::default();
    let mut written = 0u64;
    loop {
        let Some(i) = (0..next.len())
            .filter_map(|i| next[i].as_ref().map(|(stamp, _)| (*stamp, i)))
            .min()
            .map(|(_, i)| i)
        else {
            return Ok(written);
        };
        if let Some((stamp, frame)) = next[i].take()
            && merge.keeps(i, stamp, &frame)
        {
            out.write_all(&stamp.to_be_bytes())?;
            out.write_all(&frame)?;
            written += 8 + frame.len() as u64;
        }
        next[i] = next_record(&mut logs[i])?;
    }
}

/// The next whole record of a log written by [`stream`], or none at its end.
fn next_record(log: &mut impl Read) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    let mut stamp = [0u8; 8];
    match log.read_exact(&mut stamp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut frame = vec![0u8; 3];
    log.read_exact(&mut frame)?;
    let len = uxduck_mavlink::framing::frame_len(&frame).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not a MAVLink frame")
    })?;
    frame.resize(len, 0);
    log.read_exact(&mut frame[3..])?;
    Ok(Some((u64::from_be_bytes(stamp), frame)))
}

/// A link's files, oldest first, by the start time in their names
/// (`<link id>-<unix seconds>.tlog`).
fn link_files(tlogs: &Path, link_id: &str) -> Vec<(u64, PathBuf)> {
    let mut files: Vec<(u64, PathBuf)> = std::fs::read_dir(tlogs)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "tlog"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            let (link, secs) = stem.rsplit_once('-')?;
            (link == link_id).then_some((secs.parse::<u64>().ok()?, p))
        })
        .collect();
    files.sort();
    files
}

/// What a flight's telemetry takes in its file, at least and at most, from
/// the sizes of the files it is read from, without reading them: a file's
/// or a `.uxflight` entry's exactly, a link's at most what its files hold
/// from where the flight's records are read, as later flights may follow
/// in them. None when a file cannot be measured.
pub fn telemetry_bytes(tlogs: &Path, what: &ReplayRead) -> Option<(u64, u64)> {
    let len = |path: &Path| std::fs::metadata(path).ok().map(|m| m.len());
    match what {
        ReplayRead::Link {
            link_id,
            from_us,
            to_us,
            start,
        } => {
            let files = link_files(tlogs, link_id);
            let first = files
                .iter()
                .rposition(|(secs, _)| secs.saturating_mul(1_000_000) <= *from_us)
                .unwrap_or(0);
            let mut most = 0u64;
            for (secs, path) in files.iter().skip(first) {
                if secs.saturating_mul(1_000_000) > *to_us {
                    break;
                }
                let from = start
                    .as_ref()
                    .filter(|(file, _)| {
                        path.file_name()
                            .is_some_and(|n| n.to_string_lossy() == *file)
                    })
                    .map(|(_, offset)| offset.saturating_sub(LEAD_BYTES))
                    .filter(|from| first_stamp(path, *from).is_some_and(|stamp| stamp < *from_us))
                    .unwrap_or(0);
                most = most.saturating_add(len(path)?.saturating_sub(from));
            }
            Some((0, most))
        }
        ReplayRead::Merged { reads } => {
            reads.iter().try_fold((0u64, 0u64), |(least, most), one| {
                let (l, m) = telemetry_bytes(tlogs, one)?;
                Some((least.max(l), most.saturating_add(m)))
            })
        }
        ReplayRead::LinkFile { file, .. } => Some((0, len(&tlogs.join(file))?)),
        ReplayRead::LinkBefore { bytes, .. } => Some((0, *bytes)),
        ReplayRead::File { path } => len(Path::new(path)).map(|n| (n, n)),
        ReplayRead::Packed { path, entry } => {
            let entries = crate::flight_file::entries(Path::new(path)).ok()?;
            crate::flight_file::find(&entries, entry).map(|found| (found.size, found.size))
        }
        ReplayRead::VehicleFile { .. } => None,
    }
}

/// The records stamped `from_us..=to_us` in a link's files. A link writes
/// one file at a time, so only the last file begun by `from_us` and those
/// after it can hold them; with the flight's start, its file is read from
/// a little before the offset.
fn link_records(
    tlogs: &Path,
    link_id: &str,
    (from_us, to_us): (u64, u64),
    start: Option<&(String, u64)>,
    out: &mut Kept,
) -> std::io::Result<()> {
    let files = link_files(tlogs, link_id);
    let first = files
        .iter()
        .rposition(|(secs, _)| secs.saturating_mul(1_000_000) <= from_us)
        .unwrap_or(0);
    for (secs, path) in files.iter().skip(first) {
        if secs.saturating_mul(1_000_000) > to_us {
            break;
        }
        let hinted = start
            .filter(|(file, _)| {
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy() == *file)
            })
            .map(|(_, offset)| offset.saturating_sub(LEAD_BYTES))
            // A link busier than the lead-in allows for: what is read
            // from there already belongs to the flight, so earlier records
            // may too, and the file is read from its start, never cut.
            .filter(|from| first_stamp(path, *from).is_some_and(|stamp| stamp < from_us));
        let past = append_range(path, hinted.unwrap_or(0), from_us, to_us, out)?;
        if past {
            break;
        }
    }
    Ok(())
}

/// The stamp of the first whole record at or after `offset` in the file.
fn first_stamp(path: &Path, offset: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(CHUNK).read_to_end(&mut bytes).ok()?;
    let at = if offset == 0 {
        0
    } else {
        uxduck_mavlink::tlog::resync(&bytes)?
    };
    uxduck_mavlink::tlog::records(&bytes[at..])
        .next()
        .map(|(stamp, _)| stamp)
}

/// How much of a log is read at a time: a flight early in a long file is
/// read to its own end, not the file's (FB-192).
const CHUNK: u64 = 8 << 20;
/// The longest record: a stamp and the longest MAVLink frame.
const MAX_RECORD: usize = 8 + 10 + 255 + 2 + 13;

/// Appends a file's records in the range, from `offset` (resynchronised
/// when it is not the start). True once a record past the range was seen.
/// The file is read through to the range however far into it the range
/// is: until 2026-11-14 only its first 1 GB from `offset` was read, and a
/// flight further in was cut short there, or not found, without a word.
///
/// # Errors
/// When the file cannot be read, or `out` takes no more, as [`Kept::keep`].
fn append_range(
    path: &Path,
    offset: u64,
    from_us: u64,
    to_us: u64,
    out: &mut Kept,
) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    append_records(file, offset != 0, (from_us, to_us), out)
}

/// Appends the records in `from_us..=to_us` that `log` holds, read a
/// chunk at a time and no further than the first record past the range.
/// `resync` when `log` begins in the middle of a record. Records end at
/// the first that is not whole, as [`uxduck_mavlink::tlog::records`] do.
/// Only what is appended counts against `out`'s bound: what comes before
/// the range is read past, a chunk at a time.
///
/// # Errors
/// When `log` cannot be read, or `out` takes no more, as [`Kept::keep`].
fn append_records(
    mut log: impl Read,
    resync: bool,
    (from_us, to_us): (u64, u64),
    out: &mut Kept,
) -> std::io::Result<bool> {
    let mut bytes = Vec::new();
    let mut synced = !resync;
    loop {
        let read = (&mut log).take(CHUNK).read_to_end(&mut bytes)?;
        let end = read == 0;
        if !synced {
            match uxduck_mavlink::tlog::resync(&bytes) {
                Some(at) if end || at + 3 * MAX_RECORD <= bytes.len() => {
                    bytes.drain(..at);
                    synced = true;
                }
                _ if end => return Ok(false),
                // No place before the last three records' worth can
                // become one, so the scan resumes there.
                _ => {
                    bytes.drain(..bytes.len().saturating_sub(3 * MAX_RECORD));
                    continue;
                }
            }
        }
        let base = bytes.as_ptr() as usize;
        let mut whole = 0;
        for (stamp, frame) in uxduck_mavlink::tlog::records(&bytes) {
            if stamp > to_us {
                return Ok(true);
            }
            let at = frame.as_ptr() as usize - base - 8;
            whole = at + 8 + frame.len();
            if stamp >= from_us {
                out.keep(&bytes[at..whole])?;
            }
        }
        // What is left is a record cut by the chunk, or not a record.
        if end || bytes.len() - whole >= MAX_RECORD {
            return Ok(false);
        }
        bytes.drain(..whole);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_mavlink::sim;

    const T0: u64 = 1_760_000_000;

    fn write(dir: &Path, name: &str, from_us: u64, n: u64) {
        let mut log = Vec::new();
        for i in 0..n {
            let frame = sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, from_us + i * 1_000_000, &frame).unwrap();
        }
        std::fs::write(dir.join(name), log).unwrap();
    }

    /// ADR-0039 R10, 2026-11-22: a plane heard on a radio and on 4G, whose
    /// radio dropped out, replays and is written to its file from both
    /// connections' logs, each position once, and a connection that heard
    /// nothing of it adds nothing.
    #[test]
    fn a_flight_heard_on_two_connections_is_read_and_written_from_both() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = T0 * 1_000_000;
        let (mut radio, mut cell) = (Vec::new(), Vec::new());
        for i in 0..10u8 {
            let ms = u32::from(i) * 1_000;
            let at = t0 + u64::from(ms) * 1_000;
            let frame = sim::global_position_at_frame((1, i), (0, 0), (0, 0), ms);
            if !(3..=6).contains(&i) {
                uxduck_mavlink::tlog::record(&mut radio, at, &frame).unwrap();
            }
            uxduck_mavlink::tlog::record(&mut cell, at + 150_000, &frame).unwrap();
        }
        std::fs::write(dir.path().join(format!("li1-{T0}.tlog")), radio).unwrap();
        std::fs::write(dir.path().join(format!("li2-{T0}.tlog")), cell).unwrap();
        let link = |id: &str| ReplayRead::Link {
            link_id: id.into(),
            from_us: t0,
            to_us: t0 + 20_000_000,
            start: None,
        };
        let what = ReplayRead::Merged {
            reads: vec![link("li1"), link("li2"), link("li3")],
        };
        let merged = read(dir.path(), &what).unwrap();
        let sequences: Vec<u8> = uxduck_mavlink::tlog::read(&merged)
            .iter()
            .filter_map(|(_, f)| uxduck_mavlink::framing::header(f).map(|h| h.sequence))
            .collect();
        assert_eq!(sequences, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut written = Vec::new();
        let n = stream(dir.path(), &what, &mut written).unwrap();
        assert_eq!(written, merged);
        assert_eq!(n, merged.len() as u64);
        // None of them has anything then.
        let none = ReplayRead::Merged {
            reads: vec![link("li3"), link("li4")],
        };
        assert_eq!(read(dir.path(), &none), Err(NOT_HERE.to_owned()));
    }

    /// Counts what is read of a log.
    struct Counted<R>(R, std::rc::Rc<std::cell::Cell<u64>>);

    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.set(self.1.get() + n as u64);
            Ok(n)
        }
    }

    /// FB-192: a short flight early in a long link file is read to its own
    /// end, not the file's, and gives the records a whole read does, also
    /// from the middle of a record and across chunks.
    #[test]
    fn a_short_flight_early_in_a_long_file_is_read_no_further_than_it() {
        let mut log = Vec::new();
        let mut i = 0u64;
        while (log.len() as u64) < 3 * CHUNK {
            let frame = sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, T0 * 1_000_000 + i * 10_000, &frame).unwrap();
            i += 1;
        }
        let (from_us, to_us) = (T0 * 1_000_000 + 100_000_000, T0 * 1_000_000 + 400_000_000);
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut out = Vec::new();
        let past = append_records(
            Counted(&log[3..], std::rc::Rc::clone(&read)),
            true,
            (from_us, to_us),
            &mut Kept::new(&mut out, u64::MAX),
        )
        .unwrap();
        assert!(past);
        assert!(read.get() <= CHUNK, "read {} of {}", read.get(), log.len());
        let whole: Vec<u8> = uxduck_mavlink::tlog::read(&log)
            .into_iter()
            .filter(|(stamp, _)| (from_us..=to_us).contains(stamp))
            .flat_map(|(stamp, frame)| {
                let mut record = stamp.to_be_bytes().to_vec();
                record.extend_from_slice(frame);
                record
            })
            .collect();
        assert_eq!(out, whole);
        // A range that ends in the last chunk: the records across chunks.
        let (from_us, to_us) = (from_us, T0 * 1_000_000 + (i - 10) * 10_000);
        let mut out = Vec::new();
        assert!(
            append_records(
                &log[..],
                false,
                (from_us, to_us),
                &mut Kept::new(&mut out, u64::MAX)
            )
            .unwrap()
        );
        let count = uxduck_mavlink::tlog::read(&out).len() as u64;
        assert_eq!(count, i - 10 - 10_000 + 1);
        // To the end of the file.
        let mut out = Vec::new();
        assert!(
            !append_records(
                &log[..],
                false,
                (from_us, u64::MAX),
                &mut Kept::new(&mut out, u64::MAX)
            )
            .unwrap()
        );
        assert_eq!(uxduck_mavlink::tlog::read(&out).len() as u64, i - 10_000);
    }

    /// A flight further into its file than a replay holds is read whole:
    /// only what is kept counts against the most a replay holds. Until
    /// 2026-11-14 the file's first 1 GB was all that was read, so such a
    /// flight was cut short or not found, without a word. One that is itself
    /// longer than a replay holds is refused, never cut.
    #[test]
    fn a_flight_far_into_a_long_file_is_read_whole_and_one_too_long_is_refused() {
        let mut log = Vec::new();
        for i in 0..10_000u64 {
            let frame = sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, T0 * 1_000_000 + i * 10_000, &frame).unwrap();
        }
        let record = log.len() as u64 / 10_000;
        // The last hundred records, with room for two hundred: the 9,900
        // before them are fifty times what may be kept.
        let most = 200 * record;
        let from_us = T0 * 1_000_000 + 9_900 * 10_000;
        let mut out = Vec::new();
        assert!(
            !append_records(
                &log[..],
                false,
                (from_us, u64::MAX),
                &mut Kept::new(&mut out, most)
            )
            .unwrap()
        );
        assert_eq!(uxduck_mavlink::tlog::read(&out).len(), 100);
        // Three hundred records are more than two hundred: refused, in the
        // words a person reads.
        let from_us = T0 * 1_000_000 + 9_700 * 10_000;
        let mut out = Vec::new();
        let error = append_records(
            &log[..],
            false,
            (from_us, u64::MAX),
            &mut Kept::new(&mut out, most),
        )
        .expect_err("more than a replay holds");
        assert_eq!(error.kind(), std::io::ErrorKind::FileTooLarge);
        let nowhere = ReplayRead::File {
            path: "/nowhere.tlog".into(),
        };
        assert_eq!(
            unread(
                Path::new("/nowhere"),
                &nowhere,
                &std::io::Error::other("the disk went away")
            ),
            "Cannot read this flight's log: the disk went away"
        );
        // Its words name the size measured (2026-11-17).
        assert_eq!(
            too_long("flight's log", 1_395_864_371),
            "This flight's log is 1.3 GB. A replay is held in memory and takes 1 GB at most."
        );
        assert_eq!(
            too_long("flight's log", 13 << 30),
            "This flight's log is 13 GB. A replay is held in memory and takes 1 GB at most."
        );
    }

    /// A log that begins with more than a chunk of bytes that are no
    /// record is resynchronised past them, and gives every record after.
    #[test]
    fn a_log_is_resynchronised_past_more_than_a_chunk_of_noise() {
        let mut log = vec![0u8; usize::try_from(CHUNK).unwrap() + 1_000];
        for i in 0..100u64 {
            let frame = sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, T0 * 1_000_000 + i * 1_000, &frame).unwrap();
        }
        let mut out = Vec::new();
        assert!(
            !append_records(
                &log[..],
                true,
                (0, u64::MAX),
                &mut Kept::new(&mut out, u64::MAX)
            )
            .unwrap()
        );
        assert_eq!(uxduck_mavlink::tlog::read(&out).len(), 100);
    }

    /// A flight's lead-in is read from a little before its offset in its
    /// file, room for ten seconds of a busy link. On a link busier than
    /// that, what is read there is already the flight's, so the file is
    /// read from its start. Until 2026-11-17 the lead-in was cut there,
    /// without a word.
    #[test]
    fn a_lead_in_longer_than_its_window_is_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("li1-{T0}.tlog");
        let mut log = Vec::new();
        let mut n = 0u64;
        while (log.len() as u64) < LEAD_BYTES + LEAD_BYTES / 4 {
            let frame = sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, T0 * 1_000_000 + n * 1_000, &frame).unwrap();
            n += 1;
        }
        std::fs::write(dir.path().join(&name), &log).unwrap();
        // Armed at its last record, its lead-in from its first.
        let what = ReplayRead::Link {
            link_id: "li1".into(),
            from_us: T0 * 1_000_000,
            to_us: u64::MAX,
            start: Some((name, log.len() as u64)),
        };
        let bytes = read(dir.path(), &what).unwrap();
        assert_eq!(uxduck_mavlink::tlog::read(&bytes).len() as u64, n);
        let mut streamed = Vec::new();
        assert_eq!(
            stream(dir.path(), &what, &mut streamed).unwrap(),
            log.len() as u64
        );
        assert_eq!(streamed, log);
    }

    #[test]
    fn a_flights_telemetry_is_measured_at_most_what_its_files_hold() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &format!("li1-{T0}.tlog"), T0 * 1_000_000, 100);
        let second = format!("li1-{}.tlog", T0 + 200);
        write(dir.path(), &second, (T0 + 200) * 1_000_000, 100);
        write(dir.path(), &format!("li2-{T0}.tlog"), T0 * 1_000_000, 500);
        let size = 8 + sim::heartbeat_frame(1, 1, 0).len() as u64;
        let what = ReplayRead::Link {
            link_id: "li1".into(),
            from_us: (T0 + 250) * 1_000_000,
            to_us: (T0 + 260) * 1_000_000,
            start: Some((second.clone(), 50 * size)),
        };
        let streamed = stream(dir.path(), &what, &mut std::io::sink()).unwrap();
        let (least, most) = telemetry_bytes(dir.path(), &what).unwrap();
        assert_eq!(least, 0);
        assert!(most >= streamed, "{most} < {streamed}");
        assert_eq!(
            most,
            100 * size,
            "the flight's file alone, not the link's others"
        );
        let file = ReplayRead::File {
            path: dir.path().join(&second).display().to_string(),
        };
        assert_eq!(
            telemetry_bytes(dir.path(), &file),
            Some((100 * size, 100 * size))
        );
    }

    #[test]
    fn a_flight_is_read_across_the_files_it_spans_and_no_others() {
        let dir = tempfile::tempdir().unwrap();
        // Three runs of the link, and another link's.
        write(dir.path(), &format!("li1-{T0}.tlog"), T0 * 1_000_000, 100);
        write(
            dir.path(),
            &format!("li1-{}.tlog", T0 + 200),
            (T0 + 200) * 1_000_000,
            100,
        );
        write(
            dir.path(),
            &format!("li1-{}.tlog", T0 + 400),
            (T0 + 400) * 1_000_000,
            100,
        );
        write(dir.path(), &format!("li2-{T0}.tlog"), T0 * 1_000_000, 500);
        let what = ReplayRead::Link {
            link_id: "li1".into(),
            from_us: (T0 + 250) * 1_000_000,
            to_us: (T0 + 450) * 1_000_000,
            start: None,
        };
        let bytes = read(dir.path(), &what).unwrap();
        let stamps: Vec<u64> = uxduck_mavlink::tlog::read(&bytes)
            .iter()
            .map(|r| r.0 / 1_000_000 - T0)
            .collect();
        assert_eq!(stamps.first(), Some(&250));
        assert_eq!(stamps.last(), Some(&450));
        assert_eq!(stamps.len(), 50 + 51);
        // With the flight's offset: the same, read from near it.
        let size = 8 + sim::heartbeat_frame(1, 1, 0).len() as u64;
        let what = ReplayRead::Link {
            link_id: "li1".into(),
            from_us: (T0 + 250) * 1_000_000,
            to_us: (T0 + 260) * 1_000_000,
            start: Some((format!("li1-{}.tlog", T0 + 200), 60 * size)),
        };
        let bytes = read(dir.path(), &what).unwrap();
        assert_eq!(uxduck_mavlink::tlog::read(&bytes).len(), 11);
        // With its disarmed time: that file alone, to its end.
        let what = ReplayRead::LinkFile {
            link_id: "li1".into(),
            file: format!("li1-{}.tlog", T0 + 200),
            from_us: (T0 + 250) * 1_000_000,
            to_us: u64::MAX,
        };
        let bytes = read(dir.path(), &what).unwrap();
        let stamps: Vec<u64> = uxduck_mavlink::tlog::read(&bytes)
            .iter()
            .map(|r| r.0 / 1_000_000 - T0)
            .collect();
        assert_eq!((stamps.first(), stamps.last()), (Some(&250), Some(&299)));
        let what = ReplayRead::LinkFile {
            link_id: "li1".into(),
            file: format!("li2-{T0}.tlog"),
            from_us: 0,
            to_us: u64::MAX,
        };
        assert!(read(dir.path(), &what).is_err());
        // Nothing there.
        let what = ReplayRead::Link {
            link_id: "li9".into(),
            from_us: 0,
            to_us: u64::MAX,
            start: None,
        };
        assert!(read(dir.path(), &what).is_err());
    }
}

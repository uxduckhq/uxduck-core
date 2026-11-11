//! The daemon's side of a replay (ADR-0039): the tlog records it plays,
//! read from the links' tlogs or a file, and the timer that runs its clock
//! while one plays and sleeps while none does.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use tokio::sync::watch;
use uxduck_engine::runtime::{Clock, LinkLog, ReplayRead};

use crate::adapters::system::SystemClock;
use crate::daemon::engine_task::EngineHandle;

/// The most a replay holds in memory: some ten hours of a busy link.
const MAX_BYTES: u64 = 1 << 30;
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
            before(&tlogs.join(file), *offset, (*bytes).min(MAX_BYTES))
                .map_err(|e| format!("Cannot read this flight's log: {e}"))
        }
        ReplayRead::LinkFile {
            link_id,
            file,
            from_us,
            to_us,
        } => {
            own(link_id, file)?;
            let mut out = Vec::new();
            append_range(&tlogs.join(file), 0, *from_us, *to_us, &mut out)
                .map_err(|e| format!("Cannot read this flight's log: {e}"))?;
            if out.is_empty() {
                return Err("This flight's log is not on this computer any more.".into());
            }
            Ok(out)
        }
        ReplayRead::File { path } => {
            let path = Path::new(path);
            let size = std::fs::metadata(path)
                .map_err(|e| format!("Cannot read {}: {e}", path.display()))?
                .len();
            if size > MAX_BYTES {
                return Err("This file is too large to replay (over 1 GB).".into());
            }
            std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))
        }
        ReplayRead::Link {
            link_id,
            from_us,
            to_us,
            start,
        } => {
            let bytes = link_records(tlogs, link_id, *from_us, *to_us, start.as_ref())
                .map_err(|e| format!("Cannot read this flight's log: {e}"))?;
            if bytes.is_empty() {
                return Err("This flight's log is not on this computer any more.".into());
            }
            Ok(bytes)
        }
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

/// The records stamped `from_us..=to_us` in a link's files. A link writes
/// one file at a time, so only the last file begun by `from_us` and those
/// after it can hold them; with the flight's start, its file is read from
/// a little before the offset.
fn link_records(
    tlogs: &Path,
    link_id: &str,
    from_us: u64,
    to_us: u64,
    start: Option<&(String, u64)>,
) -> std::io::Result<Vec<u8>> {
    let files = link_files(tlogs, link_id);
    let first = files
        .iter()
        .rposition(|(secs, _)| secs.saturating_mul(1_000_000) <= from_us)
        .unwrap_or(0);
    let mut out = Vec::new();
    for (secs, path) in files.iter().skip(first) {
        if secs.saturating_mul(1_000_000) > to_us {
            break;
        }
        let hinted = start
            .filter(|(file, _)| {
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy() == *file)
            })
            .map(|(_, offset)| offset.saturating_sub(LEAD_BYTES));
        let past = append_range(path, hinted.unwrap_or(0), from_us, to_us, &mut out)?;
        if out.len() as u64 > MAX_BYTES {
            return Err(std::io::Error::other("the flight's log is over 1 GB"));
        }
        if past {
            break;
        }
    }
    Ok(out)
}

/// Appends a file's records in the range, from `offset` (resynchronised
/// when it is not the start). True once a record past the range was seen.
fn append_range(
    path: &Path,
    offset: u64,
    from_us: u64,
    to_us: u64,
    out: &mut Vec<u8>,
) -> std::io::Result<bool> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(usize::try_from(length - offset.min(length)).unwrap_or(0));
    file.take(MAX_BYTES).read_to_end(&mut bytes)?;
    let begin = if offset == 0 {
        0
    } else {
        uxduck_mavlink::tlog::resync(&bytes).unwrap_or(bytes.len())
    };
    let base = bytes.as_ptr() as usize;
    for (stamp, frame) in uxduck_mavlink::tlog::read(&bytes[begin..]) {
        if stamp > to_us {
            return Ok(true);
        }
        if stamp >= from_us {
            let at = frame.as_ptr() as usize - base - 8;
            out.extend_from_slice(&bytes[at..at + 8 + frame.len()]);
        }
    }
    Ok(false)
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

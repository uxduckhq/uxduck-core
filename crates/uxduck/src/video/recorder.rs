//! A recording on disk (S40, S151): one thread per recording writes the
//! camera's MPEG-TS as it arrived, or its pictures and KLV muxed into
//! MPEG-TS when it sends none (ADR-0017 V23), with a keyframe index
//! beside it, syncing about once a second, until the hub drops its end;
//! then the engine hears how it ended, and the recording is converted for
//! playback while nobody waits for it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use uxduck_media::mp4::{complete_length, duration_us};
use uxduck_media::tsmux::{index_entry, read_index};
use uxduck_media::tsrecord::{TsRecorder, Written, whole_length};

use super::hub::Item;
use crate::daemon::engine_task::EngineHandle;

const PROGRESS_EVERY: Duration = Duration::from_secs(2);
/// How often what was written is made durable: a power cut loses at most this.
const SYNC_EVERY: Duration = Duration::from_secs(1);

/// A recording's keyframe index beside it: the same name, `.idx`.
#[must_use]
pub fn index_path(dir: &Path, file: &str) -> PathBuf {
    dir.join(Path::new(file).with_extension("idx"))
}

/// Makes the file for a recording named `<stem>.ts`, or `<stem> 2.ts` and
/// on when a file has that name, never replacing one (S150, S151): its
/// name and the file.
///
/// # Errors
/// The folder or the file cannot be made.
pub fn reserve(dir: &Path, stem: &str) -> std::io::Result<(String, File)> {
    std::fs::create_dir_all(dir)?;
    for n in 1..1000 {
        let name = if n == 1 {
            format!("{stem}.ts")
        } else {
            format!("{stem} {n}.ts")
        };
        match File::create_new(dir.join(&name)) {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for the recording"))
}

/// Writes into `file` (named in `dir`) until `items` closes, then converts
/// it for playback into `cache`; stops early if the file is deleted or
/// moved away.
pub fn spawn(
    dir: PathBuf,
    cache: PathBuf,
    recording_id: String,
    file: std::io::Result<(String, File)>,
    items: Receiver<Item>,
    engine: EngineHandle,
) {
    let spawned = std::thread::Builder::new()
        .name("video-record".into())
        .spawn(move || {
            let mut recorder = TsRecorder::new();
            let name = file.as_ref().ok().map(|(name, _)| name.clone());
            let written = match file {
                Ok((name, file)) => write(
                    &dir,
                    &recording_id,
                    (&name, file),
                    &items,
                    &mut recorder,
                    &engine,
                ),
                Err(e) => Err((0, 0, e.to_string())),
            };
            let (bytes, duration, error) = match written {
                Ok(Some((bytes, duration))) => (bytes, duration, None),
                // Deleted or moved away while it was written (S151): what
                // was written is wherever it went, and it leaves the list.
                Ok(None) => {
                    if let Some(name) = &name {
                        let _ = std::fs::remove_file(index_path(&dir, name));
                    }
                    engine.post(move |e| e.video_recording_removed(&recording_id));
                    return;
                }
                Err((bytes, duration, error)) => (bytes, duration, Some(error)),
            };
            let sound = recorder.sound;
            let converts = bytes > 0 && error.is_none();
            {
                let recording_id = recording_id.clone();
                engine.post(move |e| {
                    e.video_recording_ended(&recording_id, bytes, duration, error, sound);
                });
            }
            // Ready to play before anyone asks, most of the time.
            if converts && let Some(name) = name {
                let to = super::playback::cached(&cache, &recording_id);
                if let Err(error) = super::playback::convert(&dir.join(name), &to) {
                    tracing::warn!(%error, "a recording could not be converted for playback");
                }
            }
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "a recording could not start");
    }
}

type Failure = (u64, u64, String);

/// Whether the file a recording writes is still at its place in the media
/// folder: Move to Trash and deleting both leave an open file writable.
fn still_there(path: &Path, file: &File) -> bool {
    let at_path = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file.metadata()
            .is_ok_and(|open| open.dev() == at_path.dev() && open.ino() == at_path.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, at_path);
        true
    }
}

fn write(
    dir: &Path,
    recording_id: &str,
    (name, mut file): (&str, File),
    items: &Receiver<Item>,
    recorder: &mut TsRecorder,
    engine: &EngineHandle,
) -> Result<Option<(u64, u64)>, Failure> {
    let failed = |recorder: &TsRecorder, e: std::io::Error| {
        (recorder.bytes(), recorder.duration_us, e.to_string())
    };
    let mut index: Option<File> = None;
    let mut reported = Instant::now();
    let mut synced = Instant::now();
    let mut checked = Instant::now();
    let mut unsynced = false;
    let path = dir.join(name);
    loop {
        let item = match items.recv_timeout(PROGRESS_EVERY) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let written = match item {
            Some(Item::Ts(bytes)) => recorder.push_ts(bytes),
            Some(Item::Unit(unit)) => recorder.push_unit(&unit),
            Some(Item::Klv { timestamp_us, data }) => recorder.push_klv(timestamp_us, &data),
            Some(Item::Audio(unit)) => recorder.push_audio(&unit),
            None => Written::default(),
        };
        if !written.bytes.is_empty() {
            file.write_all(&written.bytes)
                .map_err(|e| failed(recorder, e))?;
            unsynced = true;
        }
        if !written.index.is_empty() {
            if index.is_none() {
                index = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(index_path(dir, name))
                    .ok();
            }
            if let Some(index) = index.as_mut() {
                let _ = index.write_all(&written.index);
            }
        }
        if unsynced && synced.elapsed() >= SYNC_EVERY {
            file.sync_data().map_err(|e| failed(recorder, e))?;
            synced = Instant::now();
            unsynced = false;
        }
        if checked.elapsed() >= SYNC_EVERY {
            checked = Instant::now();
            if !still_there(&path, &file) {
                return Ok(None);
            }
        }
        if reported.elapsed() >= PROGRESS_EVERY {
            reported = Instant::now();
            let (id, bytes, duration) = (
                recording_id.to_owned(),
                recorder.bytes(),
                recorder.duration_us,
            );
            engine.post(move |e| e.video_recording_progress(&id, bytes, duration));
        }
    }
    file.sync_all().map_err(|e| failed(recorder, e))?;
    if let Some(index) = index {
        let _ = index.sync_all();
    }
    Ok(Some((recorder.bytes(), recorder.duration_us)))
}

/// Cuts an interrupted recording, its file named, to its whole packets
/// (MPEG-TS) or whole fragments (an MP4 made before S151): its size and
/// length, or `None` if no picture was saved.
#[must_use]
pub fn recover(dir: &Path, file: &str) -> Option<(u64, u64)> {
    let path = dir.join(file);
    if Path::new(file).extension().is_some_and(|e| e == "ts") {
        return recover_ts(dir, file, &path);
    }
    let data = std::fs::read(&path).ok()?;
    let whole = complete_length(&data);
    if whole < data.len() {
        let file = OpenOptions::new().write(true).open(&path).ok()?;
        file.set_len(whole as u64).ok()?;
        let _ = file.sync_all();
    }
    let duration = duration_us(&data[..whole]);
    (duration > 0).then_some((whole as u64, duration))
}

/// An MPEG-TS recording cut to its last whole packet, its index to the
/// keyframes inside it; its length is its last keyframe's time, or at
/// least a moment when it has one.
fn recover_ts(dir: &Path, file: &str, path: &Path) -> Option<(u64, u64)> {
    let len = std::fs::metadata(path).ok()?.len();
    let whole = whole_length(len);
    if whole < len {
        let cut = OpenOptions::new().write(true).open(path).ok()?;
        cut.set_len(whole).ok()?;
        let _ = cut.sync_all();
    }
    let index = std::fs::read(index_path(dir, file)).unwrap_or_default();
    let entries: Vec<_> = read_index(&index)
        .into_iter()
        .filter(|(_, at)| *at < whole)
        .collect();
    if entries.len() * 16 < index.len() {
        let kept: Vec<u8> = entries
            .iter()
            .flat_map(|(us, at)| index_entry(*us, *at))
            .collect();
        let _ = std::fs::write(index_path(dir, file), kept);
    }
    let last = entries.last()?;
    Some((whole, last.0.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_is_named_ts_and_never_replaces_another() {
        let dir = tempfile::tempdir().unwrap();
        let (one, _) = reserve(dir.path(), "2026-10-18 10.42.03 Alpha").unwrap();
        let (two, _) = reserve(dir.path(), "2026-10-18 10.42.03 Alpha").unwrap();
        assert_eq!(one, "2026-10-18 10.42.03 Alpha.ts");
        assert_eq!(two, "2026-10-18 10.42.03 Alpha 2.ts");
    }

    #[test]
    fn a_cut_ts_keeps_its_whole_packets_and_the_keyframes_inside_them() {
        let dir = tempfile::tempdir().unwrap();
        let name = "cut.ts";
        std::fs::write(dir.path().join(name), vec![0x47; 188 * 10 + 50]).unwrap();
        let index = [
            index_entry(0, 0),
            index_entry(1_000_000, 188 * 5),
            index_entry(2_000_000, 188 * 20),
        ]
        .concat();
        std::fs::write(index_path(dir.path(), name), index).unwrap();
        assert_eq!(recover(dir.path(), name), Some((188 * 10, 1_000_000)));
        assert_eq!(
            std::fs::metadata(dir.path().join(name)).unwrap().len(),
            188 * 10
        );
        assert_eq!(
            read_index(&std::fs::read(index_path(dir.path(), name)).unwrap()).len(),
            2
        );
    }
}

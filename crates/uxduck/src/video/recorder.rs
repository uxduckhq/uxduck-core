//! A recording on disk (S40): one thread per recording writes the camera's
//! pictures as fragmented MP4 and its KLV beside it, syncing each fragment,
//! until the hub drops its end; then the engine hears how it ended.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use uxduck_media::mp4::{complete_length, duration_us};
use uxduck_media::record::{Recorder, klv_record};

use super::hub::Item;
use crate::daemon::engine_task::EngineHandle;

const PROGRESS_EVERY: Duration = Duration::from_secs(2);

#[must_use]
pub fn mp4_path(dir: &Path, recording_id: &str) -> PathBuf {
    dir.join(format!("{recording_id}.mp4"))
}

fn klv_path(dir: &Path, recording_id: &str) -> PathBuf {
    dir.join(format!("{recording_id}.klv"))
}

/// Writes until `items` closes.
pub fn spawn(dir: PathBuf, recording_id: String, items: Receiver<Item>, engine: EngineHandle) {
    let spawned = std::thread::Builder::new()
        .name("video-record".into())
        .spawn(move || {
            let (bytes, duration, error) = match write(&dir, &recording_id, &items, &engine) {
                Ok((bytes, duration)) => (bytes, duration, None),
                Err((bytes, duration, error)) => (bytes, duration, Some(error)),
            };
            engine.post(move |e| e.video_recording_ended(&recording_id, bytes, duration, error));
        });
    if let Err(error) = spawned {
        tracing::error!(%error, "a recording could not start");
    }
}

type Failure = (u64, u64, String);

fn write(
    dir: &Path,
    recording_id: &str,
    items: &Receiver<Item>,
    engine: &EngineHandle,
) -> Result<(u64, u64), Failure> {
    let failed = |bytes, duration, e: std::io::Error| (bytes, duration, e.to_string());
    std::fs::create_dir_all(dir).map_err(|e| failed(0, 0, e))?;
    let mut file = File::create_new(mp4_path(dir, recording_id)).map_err(|e| failed(0, 0, e))?;
    let mut klv: Option<File> = None;
    let mut recorder = Recorder::new();
    let mut first_us = None;
    let mut bytes = 0u64;
    let mut reported = Instant::now();
    loop {
        let item = match items.recv_timeout(PROGRESS_EVERY) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match item {
            Some(Item::Unit(unit)) => {
                let timestamp = unit.timestamp_us;
                let out = recorder.push(unit);
                if recorder.started() && first_us.is_none() {
                    first_us = Some(timestamp);
                }
                if !out.is_empty() {
                    file.write_all(&out)
                        .and_then(|()| file.sync_data())
                        .map_err(|e| failed(bytes, recorder.duration_us, e))?;
                    bytes += out.len() as u64;
                }
            }
            Some(Item::Klv { timestamp_us, data }) => {
                if let Some(first) = first_us {
                    if klv.is_none() {
                        klv = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(klv_path(dir, recording_id))
                            .ok();
                    }
                    if let Some(klv) = klv.as_mut() {
                        let _ =
                            klv.write_all(&klv_record(timestamp_us.saturating_sub(first), &data));
                    }
                }
            }
            None => {}
        }
        if reported.elapsed() >= PROGRESS_EVERY {
            reported = Instant::now();
            let (id, duration) = (recording_id.to_owned(), recorder.duration_us);
            engine.post(move |e| e.video_recording_progress(&id, bytes, duration));
        }
    }
    let last = recorder.finish();
    file.write_all(&last)
        .and_then(|()| file.sync_all())
        .map_err(|e| failed(bytes, recorder.duration_us, e))?;
    Ok((bytes + last.len() as u64, recorder.duration_us))
}

/// Cuts an interrupted recording to its last whole fragment: its size and
/// length, or `None` if no picture was saved.
#[must_use]
pub fn recover(dir: &Path, recording_id: &str) -> Option<(u64, u64)> {
    let path = mp4_path(dir, recording_id);
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

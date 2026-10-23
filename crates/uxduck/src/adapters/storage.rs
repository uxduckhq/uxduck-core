//! Disk usage by category, and deleting old files (ADR-0007 storage
//! policy). Categories are places in the data directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use uxduck_core::domain::settings::StorageCategory;
use uxduck_engine::runtime::{Deleted, StorageReport};

/// The files and directories of a category.
fn places(data_dir: &Path, category: StorageCategory) -> Vec<PathBuf> {
    let file_prefixed = |prefix: &str| -> Vec<PathBuf> {
        std::fs::read_dir(data_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with(prefix))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    match category {
        StorageCategory::Tlogs => vec![data_dir.join("tlogs")],
        StorageCategory::EventLog => file_prefixed("events.ndjson"),
        StorageCategory::Database => {
            [file_prefixed("uxduck.db"), file_prefixed("analytics.db")].concat()
        }
        StorageCategory::Backups => vec![data_dir.join("backups")],
        StorageCategory::Logs => vec![data_dir.join("logs")],
        StorageCategory::Plugins => vec![data_dir.join("plugins")],
        StorageCategory::Video => vec![
            crate::paths::media_dir(data_dir),
            crate::video::playback::cache_dir(&crate::paths::media_dir(data_dir)),
        ],
        StorageCategory::Simulators
        | StorageCategory::SimulatorLogs
        | StorageCategory::SimulatorData => vec![data_dir.join("simulators")],
        StorageCategory::Parameters => vec![data_dir.join("parameters")],
        StorageCategory::DemoVideos => vec![data_dir.join("cache/demo-videos")],
        StorageCategory::VehicleFiles => vec![crate::vehicle_files::dir(data_dir)],
    }
}

/// Every file under `path`, or `path` itself.
fn files(path: &Path, out: &mut Vec<(PathBuf, std::fs::Metadata)>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            files(&entry.path(), out);
        }
    } else if meta.is_file() {
        out.push((path.to_owned(), meta));
    }
}

#[must_use]
pub fn report(data_dir: &Path) -> StorageReport {
    let mut simulator_files = Vec::new();
    let simulator_root = data_dir.join("simulators");
    files(&simulator_root, &mut simulator_files);
    let mut simulator_totals = std::collections::BTreeMap::<StorageCategory, (u64, u32)>::new();
    for (path, meta) in simulator_files {
        let total = simulator_totals
            .entry(simulator_category(&simulator_root, &path))
            .or_default();
        total.0 += meta.len();
        total.1 = total.1.saturating_add(1);
    }
    let categories = StorageCategory::ALL
        .iter()
        .map(|category| {
            if matches!(
                category,
                StorageCategory::Simulators
                    | StorageCategory::SimulatorLogs
                    | StorageCategory::SimulatorData
            ) {
                let (bytes, files) = simulator_totals.get(category).copied().unwrap_or_default();
                return (*category, bytes, files);
            }
            let mut found = Vec::new();
            for place in places(data_dir, *category) {
                files(&place, &mut found);
            }
            let bytes = found.iter().map(|(_, m)| m.len()).sum();
            (
                *category,
                bytes,
                u32::try_from(found.len()).unwrap_or(u32::MAX),
            )
        })
        .collect();
    StorageReport {
        data_dir: data_dir.display().to_string(),
        categories,
        free_bytes: fs4::available_space(data_dir).ok(),
        total_bytes: fs4::total_space(data_dir).ok(),
    }
}

/// Deletes a deletable category's files last modified before the cutoff.
/// Files still being written (a tlog of this run) are newer than any
/// cutoff a day or more back, so they stay.
pub fn delete_older(data_dir: &Path, category: StorageCategory, cutoff_us: u64) -> Deleted {
    let mut deleted = Deleted::default();
    if !category.deletable() || category == StorageCategory::SimulatorLogs {
        return deleted;
    }
    let cutoff = UNIX_EPOCH + Duration::from_micros(cutoff_us);
    let mut found = Vec::new();
    for place in places(data_dir, category) {
        files(&place, &mut found);
    }
    for (path, meta) in found {
        let old = meta.modified().is_ok_and(|modified| modified < cutoff);
        if old && std::fs::remove_file(&path).is_ok() {
            deleted.files += 1;
            deleted.bytes += meta.len();
            tracing::info!(file = %path.display(), "deleted by the storage policy");
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_cache_counts_separately_and_recording_retention_leaves_it_available() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache/demo-videos");
        std::fs::create_dir_all(&cache).unwrap();
        let video = cache.join("demo.mp4");
        std::fs::write(&video, b"video").unwrap();
        let usage = report(dir.path());
        let demo = usage
            .categories
            .iter()
            .find(|(kind, _, _)| *kind == StorageCategory::DemoVideos)
            .unwrap();
        assert_eq!(demo.1, 5);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let cutoff = u64::try_from(now).unwrap() + 1_000_000;
        assert_eq!(
            delete_older(dir.path(), StorageCategory::Video, cutoff).files,
            0
        );
        assert_eq!(
            delete_older(dir.path(), StorageCategory::DemoVideos, cutoff).files,
            0
        );
        assert_eq!(std::fs::read(video).unwrap(), b"video");
    }

    #[test]
    fn usage_is_counted_by_category_and_only_old_deletable_files_go() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tlogs")).unwrap();
        std::fs::write(dir.path().join("tlogs/a.tlog"), [0u8; 100]).unwrap();
        std::fs::write(dir.path().join("events.ndjson"), [0u8; 40]).unwrap();
        let report = report(dir.path());
        let of = |c| {
            report
                .categories
                .iter()
                .find(|(k, _, _)| *k == c)
                .unwrap()
                .1
        };
        assert_eq!(of(StorageCategory::Tlogs), 100);
        assert_eq!(of(StorageCategory::EventLog), 40);
        assert!(report.free_bytes.is_some());

        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let now = u64::try_from(now).unwrap();
        let kept = delete_older(dir.path(), StorageCategory::Tlogs, now - 60_000_000);
        assert_eq!(kept.files, 0, "written just now");
        let never = delete_older(dir.path(), StorageCategory::EventLog, now + 60_000_000);
        assert_eq!(never.files, 0, "the event log is never deleted");
        let gone = delete_older(dir.path(), StorageCategory::Tlogs, now + 60_000_000);
        assert_eq!((gone.files, gone.bytes), (1, 100));
    }
}

/// Build trees may contain folders called logs too: classify their root first.
fn simulator_category(root: &Path, file: &Path) -> StorageCategory {
    let parts: Vec<_> = file.components().skip(root.components().count()).collect();
    if parts
        .first()
        .is_some_and(|p| p.as_os_str() == "bin" || p.as_os_str() == "catalogue")
    {
        StorageCategory::Simulators
    } else if parts
        .get(1)
        .is_some_and(|p| matches!(p.as_os_str().to_str(), Some("logs" | "log" | "sitl.log")))
    {
        StorageCategory::SimulatorLogs
    } else {
        StorageCategory::SimulatorData
    }
}

/// Read actual log files only, never links or parameters. Missing log roots
/// are normal for a simulation that has not written that kind of log.
fn log_files(path: &Path, out: &mut Vec<(PathBuf, std::fs::Metadata)>) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            log_files(&entry?.path(), out)?;
        }
    } else if meta.is_file() {
        out.push((path.to_owned(), meta));
    }
    Ok(())
}

fn simulation_dirs(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(data_dir.join("simulators")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name() != "bin"
            && entry.file_name() != "catalogue"
        {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn logs_in(dir: &Path) -> std::io::Result<Vec<(PathBuf, std::fs::Metadata)>> {
    let mut found = Vec::new();
    for name in ["logs", "log", "sitl.log"] {
        log_files(&dir.join(name), &mut found)?;
    }
    Ok(found)
}

/// Log usage by simulation, including the manager's active-process status.
///
/// # Errors
/// When simulation directories or log metadata cannot be read.
pub fn simulator_logs(
    data_dir: &Path,
    active: &HashSet<String>,
) -> Result<Vec<uxduck_contracts::dto::settings::SimulatorLogUsage>, String> {
    let read = || -> std::io::Result<Vec<uxduck_contracts::dto::settings::SimulatorLogUsage>> {
        let mut usage = Vec::new();
        for dir in simulation_dirs(data_dir)? {
            let found = logs_in(&dir)?;
            if found.is_empty() {
                continue;
            }
            let id = dir
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| std::io::Error::other("A simulation folder has an invalid name"))?
                .to_owned();
            usage.push(uxduck_contracts::dto::settings::SimulatorLogUsage {
                running: active.contains(&id),
                simulation_id: id,
                name: None,
                bytes: found.iter().map(|(_, m)| m.len()).sum(),
                files: u32::try_from(found.len()).unwrap_or(u32::MAX),
            });
        }
        Ok(usage)
    };
    read().map_err(|e| format!("Could not read simulator log sizes: {e}"))
}

/// Called with the simulation manager's lock held, so a launch cannot race
/// cleanup. None clears all stopped logs; a cutoff applies retention.
///
/// # Errors
/// When a directory cannot be read or a log cannot be removed.
pub fn delete_simulator_logs(
    data_dir: &Path,
    active: &HashSet<String>,
    cutoff: Option<u64>,
) -> Result<Deleted, String> {
    let delete = || -> std::io::Result<Deleted> {
        let mut deleted = Deleted::default();
        for dir in simulation_dirs(data_dir)? {
            if dir
                .file_name()
                .and_then(|s| s.to_str())
                .is_none_or(|id| active.contains(id))
            {
                continue;
            }
            for (path, meta) in logs_in(&dir)? {
                if let Some(cutoff) = cutoff
                    && meta.modified()? >= UNIX_EPOCH + Duration::from_micros(cutoff)
                {
                    continue;
                }
                std::fs::remove_file(&path)?;
                deleted.files = deleted.files.saturating_add(1);
                deleted.bytes += meta.len();
            }
        }
        Ok(deleted)
    };
    delete().map_err(|error| format!("Could not clear all stopped simulator logs: {error}. Some files may already have been cleared; refresh Storage and try again."))
}

#[cfg(test)]
mod simulator_tests {
    use super::*;

    fn write(root: &Path, name: &str, size: usize) -> PathBuf {
        let path = root.join("simulators").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; size]).unwrap();
        path
    }

    #[test]
    fn simulator_storage_separates_builds_logs_and_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bin/arducopter", 100);
        write(dir.path(), "catalogue/build/log/build-output.txt", 20);
        write(dir.path(), "stopped/logs/00000001.BIN", 10);
        write(dir.path(), "stopped/sitl.log", 5);
        write(dir.path(), "px4/log/day/flight.ulg", 30);
        write(dir.path(), "px4/parameters.bson", 7);
        write(dir.path(), "stopped/eeprom.bin", 3);
        let report = report(dir.path());
        let bytes = |cat| {
            report
                .categories
                .iter()
                .find(|(c, _, _)| *c == cat)
                .unwrap()
                .1
        };
        assert_eq!(bytes(StorageCategory::Simulators), 120);
        assert_eq!(bytes(StorageCategory::SimulatorLogs), 45);
        assert_eq!(bytes(StorageCategory::SimulatorData), 10);
        assert_eq!(
            simulator_logs(dir.path(), &HashSet::from(["px4".into()]))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn size_limit_removes_oldest_stopped_logs_and_never_running_logs_or_builds() {
        let dir = tempfile::tempdir().unwrap();
        let build = write(dir.path(), "bin/arducopter", 100);
        let state = write(dir.path(), "old/eeprom.bin", 20);
        let oldest = write(dir.path(), "old/logs/old.BIN", 60);
        let recent = write(dir.path(), "new/log/day/recent.ulg", 50);
        let running = write(dir.path(), "running/sitl.log", 30);
        let before = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&oldest)
            .unwrap()
            .set_modified(before)
            .unwrap();
        let active = HashSet::from(["running".into()]);
        assert_eq!(
            limit_simulator_logs(dir.path(), &active, 0).unwrap().files,
            0
        );
        let removed = limit_simulator_logs(dir.path(), &active, 100).unwrap();
        assert_eq!((removed.files, removed.bytes), (1, 60));
        assert!(!oldest.exists() && recent.exists() && running.exists());
        let removed = limit_simulator_logs(dir.path(), &active, 10).unwrap();
        assert_eq!((removed.files, removed.bytes), (1, 50));
        assert!(
            running.exists() && build.exists() && state.exists(),
            "running logs may exceed the limit"
        );
        assert_eq!(
            limit_simulator_logs(dir.path(), &active, 10).unwrap().files,
            0
        );
    }

    #[test]
    fn clear_and_retention_preserve_running_logs_builds_state_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let build = write(dir.path(), "bin/arducopter", 100);
        let build_log = write(dir.path(), "catalogue/build/log/test.ulg", 20);
        let old = write(dir.path(), "stopped/logs/old.BIN", 10);
        let recent = write(dir.path(), "stopped/logs/new.BIN", 11);
        let stdout = write(dir.path(), "stopped/sitl.log", 12);
        let running = write(dir.path(), "running/logs/old.BIN", 13);
        let px4 = write(dir.path(), "px4/log/day/old.ulg", 14);
        let state = write(dir.path(), "stopped/eeprom.bin", 15);
        let last_number = write(dir.path(), "stopped/logs/LASTLOG.TXT", 2);
        let now = std::time::SystemTime::now();
        let before = now - Duration::from_hours(8 * 24);
        for path in [&old, &stdout, &running, &px4] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(before)
                .unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                &build,
                dir.path().join("simulators/stopped/logs/build-link"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("simulators/bin"),
                dir.path().join("simulators/stopped/logs/build-dir"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("simulators/stopped"),
                dir.path().join("simulators/linked"),
            )
            .unwrap();
        }
        let active = HashSet::from(["running".into()]);
        let cutoff = u64::try_from(
            (now - Duration::from_hours(7 * 24))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros(),
        )
        .unwrap();
        let deleted = delete_simulator_logs(dir.path(), &active, Some(cutoff)).unwrap();
        assert_eq!((deleted.files, deleted.bytes), (3, 36));
        assert!(!old.exists() && !stdout.exists() && !px4.exists());
        assert!(
            recent.exists()
                && running.exists()
                && state.exists()
                && build.exists()
                && build_log.exists()
        );
        let deleted = delete_simulator_logs(dir.path(), &active, None).unwrap();
        assert_eq!((deleted.files, deleted.bytes), (2, 13));
        assert!(!recent.exists() && !last_number.exists());
        assert!(running.exists() && state.exists() && build.exists() && build_log.exists());
        assert_eq!(
            delete_simulator_logs(dir.path(), &active, None)
                .unwrap()
                .files,
            0
        );
    }
}

/// Enforces the total log budget, oldest stopped files first. Active log
/// bytes count towards the budget but active files are never candidates.
/// Zero disables the size limit.
///
/// # Errors
/// When a log cannot be read or removed; earlier deletions may have succeeded.
pub fn limit_simulator_logs(
    data_dir: &Path,
    active: &HashSet<String>,
    max_bytes: u64,
) -> Result<Deleted, String> {
    if max_bytes == 0 {
        return Ok(Deleted::default());
    }
    let limit = || -> std::io::Result<Deleted> {
        let mut total = 0u64;
        let mut candidates = Vec::new();
        for dir in simulation_dirs(data_dir)? {
            let running = dir
                .file_name()
                .and_then(|s| s.to_str())
                .is_none_or(|id| active.contains(id));
            for (path, meta) in logs_in(&dir)? {
                total = total.saturating_add(meta.len());
                if !running {
                    candidates.push((meta.modified()?, path, meta.len()));
                }
            }
        }
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut deleted = Deleted::default();
        for (_, path, bytes) in candidates {
            if total <= max_bytes {
                break;
            }
            std::fs::remove_file(path)?;
            total = total.saturating_sub(bytes);
            deleted.files = deleted.files.saturating_add(1);
            deleted.bytes = deleted.bytes.saturating_add(bytes);
        }
        Ok(deleted)
    };
    limit().map_err(|error| format!("Could not enforce the simulator log size limit: {error}. Some stopped logs may already have been removed."))
}

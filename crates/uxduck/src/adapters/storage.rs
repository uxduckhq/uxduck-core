//! Disk usage by category, and deleting old files (ADR-0007 storage
//! policy). Categories are places in the data directory.

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
        StorageCategory::Video => vec![crate::paths::recordings_dir(data_dir)],
        StorageCategory::Simulators => vec![data_dir.join("simulators")],
        StorageCategory::Parameters => vec![data_dir.join("parameters")],
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
    let categories = StorageCategory::ALL
        .iter()
        .map(|category| {
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
    if !category.deletable() {
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

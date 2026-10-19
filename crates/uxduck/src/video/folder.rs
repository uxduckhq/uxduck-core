//! The media folder as people leave it (S151): photos and recordings
//! deleted or moved away in Finder or Explorer leave the list, with what
//! UXDuck kept for them, a recording's keyframe index and playback copy.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon::engine_task::EngineHandle;

/// How often the folder is read: a file's name each, nothing more.
const EVERY: Duration = Duration::from_secs(3);

/// The folder's file names now, and the keyframe indexes whose recording
/// is gone; `None` when it cannot be read for another reason than not
/// being there, so nothing leaves the list on a passing error.
pub fn read(dir: &Path) -> Option<(HashSet<String>, Vec<PathBuf>)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Default::default()),
        Err(_) => return None,
    };
    let mut names = HashSet::new();
    for entry in entries {
        let entry = entry.ok()?;
        if let Ok(name) = entry.file_name().into_string() {
            names.insert(name);
        }
    }
    let orphans = names
        .iter()
        .filter_map(|name| name.strip_suffix(".idx"))
        .filter(|stem| !names.contains(&format!("{stem}.ts")))
        .map(|stem| dir.join(format!("{stem}.idx")))
        .collect();
    Some((names, orphans))
}

/// Reads the folder every few seconds for as long as the daemon runs.
pub fn watch(dir: PathBuf, cache: PathBuf, engine: EngineHandle) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let at = dir.clone();
            let Ok(Some((names, orphans))) = tokio::task::spawn_blocking(move || read(&at)).await
            else {
                continue;
            };
            let Ok(gone) = engine.run(move |e| e.forget_missing_media(&names)).await else {
                return;
            };
            let cache = cache.clone();
            tokio::task::spawn_blocking(move || {
                for path in orphans {
                    let _ = std::fs::remove_file(path);
                }
                for id in &gone {
                    let _ = std::fs::remove_file(super::playback::cached(&cache, id));
                }
                if !gone.is_empty() {
                    tracing::info!(recordings = gone.len(), "recordings deleted outside UXDuck");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_lists_its_files_and_the_indexes_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.ts", "a.idx", "b.idx", "p.jpg"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let (names, orphans) = read(dir.path()).unwrap();
        assert_eq!(names.len(), 4);
        assert!(names.contains("p.jpg"));
        assert_eq!(orphans, [dir.path().join("b.idx")]);
    }

    #[test]
    fn a_missing_folder_has_nothing_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let (names, orphans) = read(&dir.path().join("media")).unwrap();
        assert!(names.is_empty() && orphans.is_empty());
    }
}

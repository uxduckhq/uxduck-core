//! The vehicle's SD card plugged into this computer (ADR-0039 R15, S212):
//! noticed by its `APM/LOGS` folder, and its `.BIN` logs copied into this
//! computer's logs (`<data>/logs-from-cards/<card>-<date>/`), where Flights
//! lists them to replay. Nothing is fetched from a vehicle: people pull the
//! card, since nothing reads a log over MAVLink quickly (maintainer,
//! 2026-10-28).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use uxduck_contracts::dto::replay::{CopiedLog, LogCard, LogCardsResult};

/// Where the copies go.
#[must_use]
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs-from-cards")
}

/// The places a card is mounted on this platform.
#[allow(clippy::vec_init_then_push, reason = "each platform adds its own")]
fn mount_roots() -> Vec<PathBuf> {
    #[allow(unused_mut, reason = "each platform adds its own")]
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    roots.push(PathBuf::from("/Volumes"));
    #[cfg(target_os = "linux")]
    {
        if let Ok(user) = std::env::var("USER") {
            roots.push(PathBuf::from("/media").join(&user));
            roots.push(PathBuf::from("/run/media").join(&user));
        }
        roots.push(PathBuf::from("/media"));
        roots.push(PathBuf::from("/mnt"));
    }
    #[allow(unused_mut, reason = "Windows adds its drives")]
    let mut volumes: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| std::fs::read_dir(root).into_iter().flatten().flatten())
        .map(|e| e.path())
        .collect();
    #[cfg(windows)]
    volumes.extend((b'D'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))));
    volumes
}

/// A card's logs folder, whatever its letters' case.
fn logs_folder(volume: &Path) -> Option<PathBuf> {
    let find = |dir: &Path, name: &str| {
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)))
    };
    find(&find(volume, "APM")?, "LOGS")
}

fn bin_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")))
        .collect();
    files.sort();
    files
}

/// Cards plugged in with logs on them, from `volumes` (the mount points).
#[must_use]
pub fn cards_in(volumes: &[PathBuf]) -> Vec<LogCard> {
    volumes
        .iter()
        .filter_map(|volume| {
            let folder = logs_folder(volume)?;
            let files = bin_files(&folder);
            (!files.is_empty()).then(|| LogCard {
                path: volume.display().to_string(),
                name: volume.file_name().map_or_else(
                    || volume.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                ),
                logs: u32::try_from(files.len()).unwrap_or(u32::MAX),
                bytes: files
                    .iter()
                    .filter_map(|f| std::fs::metadata(f).ok())
                    .map(|m| m.len())
                    .sum(),
            })
        })
        .collect()
}

/// The logs copied off cards before, newest first.
#[must_use]
pub fn copied(data_dir: &Path) -> Vec<CopiedLog> {
    let mut logs: Vec<(std::time::SystemTime, CopiedLog)> = std::fs::read_dir(dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|copy| {
            let card = copy.file_name().to_string_lossy().into_owned();
            bin_files(&copy.path())
                .into_iter()
                .map(move |file| (card.clone(), file))
        })
        .filter_map(|(card, file)| {
            let meta = std::fs::metadata(&file).ok()?;
            Some((
                meta.modified().ok()?,
                CopiedLog {
                    path: file.display().to_string(),
                    name: file.file_name()?.to_string_lossy().into_owned(),
                    card,
                    bytes: meta.len(),
                },
            ))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
    logs.into_iter().map(|(_, log)| log).collect()
}

/// The card being copied, how many done of how many, and why it stopped.
type Progress = (String, u32, u32, Option<String>);

/// Copies a card's logs on a thread of its own; how it goes is in the
/// shared state that `logs/cards` reads.
#[derive(Clone, Debug, Default)]
pub struct Copying(Arc<Mutex<Option<Progress>>>);

impl Copying {
    #[must_use]
    pub fn now(&self) -> Option<Progress> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts copying the logs of the card at `card` (a mount point).
    ///
    /// # Errors
    /// When it is not a card with logs, or one is being copied.
    pub fn start(&self, data_dir: &Path, card: &str, today: &str) -> Result<(), String> {
        let volume = PathBuf::from(card);
        let folder = logs_folder(&volume).ok_or("This card has no APM/LOGS folder.")?;
        let files = bin_files(&folder);
        {
            let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if now
                .as_ref()
                .is_some_and(|(_, done, of, why)| done < of && why.is_none())
            {
                return Err("A card is being copied already.".into());
            }
            *now = Some((
                card.to_owned(),
                0,
                u32::try_from(files.len()).unwrap_or(u32::MAX),
                None,
            ));
        }
        let name = volume
            .file_name()
            .map_or_else(|| "card".to_owned(), |n| n.to_string_lossy().into_owned());
        let into = dir(data_dir).join(format!("{name}-{today}"));
        let state = self.0.clone();
        std::thread::spawn(move || {
            let copy = || -> Result<(), String> {
                std::fs::create_dir_all(&into).map_err(|e| e.to_string())?;
                for (done, file) in files.iter().enumerate() {
                    let Some(file_name) = file.file_name() else {
                        continue;
                    };
                    let target = into.join(file_name);
                    let same = std::fs::metadata(&target)
                        .ok()
                        .zip(std::fs::metadata(file).ok())
                        .is_some_and(|(a, b)| a.len() == b.len());
                    if !same {
                        let part = target.with_extension("part");
                        std::fs::copy(file, &part)
                            .and_then(|_| std::fs::rename(&part, &target))
                            .map_err(|e| format!("{}: {e}", file.display()))?;
                    }
                    if let Some(now) = state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_mut()
                    {
                        now.1 = u32::try_from(done + 1).unwrap_or(u32::MAX);
                    }
                }
                Ok(())
            };
            if let Err(why) = copy()
                && let Some(now) = state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
            {
                now.3 = Some(why);
            }
        });
        Ok(())
    }
}

/// What `logs/cards` answers.
#[must_use]
pub fn status(data_dir: &Path, copying: &Copying) -> LogCardsResult {
    let now = copying.now();
    LogCardsResult {
        cards: cards_in(&mount_roots()),
        copied: copied(data_dir),
        copying: now.as_ref().map(|(card, _, _, _)| card.clone()),
        done: now.as_ref().map_or(0, |n| n.1),
        of: now.as_ref().map_or(0, |n| n.2),
        problem: now.and_then(|n| n.3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_is_known_by_its_logs_folder_and_its_logs_are_copied() {
        let volumes = tempfile::tempdir().unwrap();
        let card = volumes.path().join("NO NAME");
        std::fs::create_dir_all(card.join("APM").join("LOGS")).unwrap();
        std::fs::write(card.join("APM/LOGS/00000001.BIN"), b"one").unwrap();
        std::fs::write(card.join("APM/LOGS/00000002.bin"), b"two!").unwrap();
        std::fs::write(card.join("APM/LOGS/LASTLOG.TXT"), b"2").unwrap();
        // A stick with no logs folder is no card.
        std::fs::create_dir_all(volumes.path().join("PHOTOS/DCIM")).unwrap();
        let cards = cards_in(&[card.clone(), volumes.path().join("PHOTOS")]);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            (cards[0].name.as_str(), cards[0].logs, cards[0].bytes),
            ("NO NAME", 2, 7)
        );

        let data = tempfile::tempdir().unwrap();
        let copying = Copying::default();
        copying
            .start(data.path(), &card.display().to_string(), "2026-10-28")
            .unwrap();
        for _ in 0..200 {
            if copying.now().is_some_and(|n| n.1 == 2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let logs = copied(data.path());
        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|l| l.card == "NO NAME-2026-10-28"));
    }
}

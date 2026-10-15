//! "Vehicle files" (S121, ADR-0027 F5): the copies UXDuck makes of
//! vehicles' files, kept in the data directory by vehicle and by the path on
//! the vehicle, `vehicle-files/<vehicle id>/APM/LOGS/00000012.BIN`. A copy
//! being made ends in `.part` until it is whole. The browser saves a whole
//! copy from `/api/vehicle-files/<name>?token=…`, an address `files/save` grants
//! once, for a minute.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uxduck_engine::vehicle_files::FilesPort;

/// How long a save address works.
const GRANT_FOR: Duration = Duration::from_secs(60);

/// Where the copies are kept.
#[must_use]
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("vehicle-files")
}

#[derive(Debug)]
struct Grant {
    path: PathBuf,
    name: String,
    until: Instant,
}

/// The copies, and the save addresses granted.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    grants: Arc<Mutex<HashMap<String, Grant>>>,
}

impl Store {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: dir(data_dir),
            grants: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A copy's path, refusing anything that climbs out of the folder.
    fn whole(&self, local: &str) -> Option<PathBuf> {
        let relative = Path::new(local);
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            .then(|| self.dir.join(relative))
    }

    fn part(&self, local: &str) -> Option<PathBuf> {
        self.whole(local).map(|whole| {
            let mut name = whole.into_os_string();
            name.push(".part");
            PathBuf::from(name)
        })
    }

    /// The copy a save address names, once: the address is spent.
    #[must_use]
    pub fn take(&self, token: &str) -> Option<(PathBuf, String)> {
        let mut grants = self.grants.lock().ok()?;
        grants.retain(|_, grant| grant.until > Instant::now());
        grants.remove(token).map(|grant| (grant.path, grant.name))
    }
}

fn size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

impl FilesPort for Store {
    fn copied(&self, local: &str) -> u64 {
        self.part(local)
            .and_then(|part| size(&part))
            .or_else(|| self.whole(local).and_then(|whole| size(&whole)))
            .unwrap_or(0)
    }

    fn write(&mut self, local: &str, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let part = self
            .part(local)
            .ok_or_else(|| format!("{local} is not a place for a copy"))?;
        if let Some(parent) = part.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part)
            .map_err(|e| e.to_string())?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        if length < offset {
            return Err(format!(
                "the copy has {length} bytes, not the {offset} these follow"
            ));
        }
        file.set_len(offset).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())
    }

    fn finish(&mut self, local: &str) -> Result<(), String> {
        let (Some(part), Some(whole)) = (self.part(local), self.whole(local)) else {
            return Err(format!("{local} is not a place for a copy"));
        };
        if !part.exists() {
            // Empty on the vehicle: nothing was written.
            if let Some(parent) = whole.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            if !whole.exists() {
                std::fs::write(&whole, []).map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        std::fs::rename(&part, &whole).map_err(|e| e.to_string())
    }

    fn discard(&mut self, local: &str) {
        for path in [self.part(local), self.whole(local)].into_iter().flatten() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn grant(&mut self, local: &str, name: &str) -> Option<String> {
        let whole = self.whole(local).filter(|w| w.is_file())?;
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).ok()?;
        let token = bytes.iter().fold(String::new(), |mut token, b| {
            use std::fmt::Write as _;
            let _ = write!(token, "{b:02x}");
            token
        });
        let mut grants = self.grants.lock().ok()?;
        grants.retain(|_, grant| grant.until > Instant::now());
        grants.insert(
            token.clone(),
            Grant {
                path: whole,
                name: name.to_owned(),
                until: Instant::now() + GRANT_FOR,
            },
        );
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Some(format!("/api/vehicle-files/{safe}?token={token}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_grows_in_order_resumes_and_is_saved_once() {
        let data = tempfile::tempdir().unwrap();
        let mut store = Store::new(data.path());
        let local = "v1/APM/LOGS/00000001.BIN";
        assert_eq!(store.copied(local), 0);
        store.write(local, 0, b"hello ").unwrap();
        store.write(local, 6, b"wor").unwrap();
        assert_eq!(store.copied(local), 9);
        // Resumed from 6: what was past it is cut.
        store.write(local, 6, b"world").unwrap();
        assert!(store.write(local, 20, b"!").is_err(), "a gap");
        assert!(
            store.grant(local, "00000001.BIN").is_none(),
            "not whole yet"
        );
        store.finish(local).unwrap();
        let whole = dir(data.path()).join(local);
        assert_eq!(std::fs::read(&whole).unwrap(), b"hello world");
        assert_eq!(store.copied(local), 11);
        let url = store.grant(local, "00000001.BIN").unwrap();
        let token = url.split("token=").nth(1).unwrap();
        assert!(url.starts_with("/api/vehicle-files/00000001.BIN?token="));
        assert_eq!(store.take(token).unwrap().0, whole);
        assert!(store.take(token).is_none(), "once");
        store.discard(local);
        assert!(!whole.exists());
        assert!(store.write("v1/../../etc/x", 0, b"x").is_err());
    }
}

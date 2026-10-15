//! Interfaces on disk, in `<data dir>/interface/` (interface-updates.md):
//!
//! ```text
//! interface/
//!   current                 the version being served, one line
//!   followed.json           the last manifest accepted: publishedAt, entry chosen
//!   bad.json                versions marked bad, and why
//!   settings.json           `interface.updates`: on or off
//!   <version>/              an unpacked, verified interface
//!   <version>.json          its manifest entry; a folder without one is not complete
//!   <version>.partial/      one being unpacked; deleted on start
//!   <version>.download      its archive being downloaded; deleted on start
//! ```
//!
//! An interface is complete once its folder and its entry are both there:
//! the folder is renamed into place only after every file checked out, and
//! the entry is written after it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::interface::{InterfaceEntry, MAX_ARCHIVE_BYTES};

/// The most an archive may unpack to, against a compression bomb.
const MAX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the version {0:?} is not one UXDuck can keep")]
    BadVersion(String),
    #[error("the archive is {got} bytes, not {want}")]
    Size { got: u64, want: u64 },
    #[error("the archive is larger than UXDuck downloads ({0} bytes)")]
    TooLarge(u64),
    #[error("the archive's checksum does not match")]
    ArchiveHash,
    #[error("{0} in the archive does not match its checksum")]
    FileHash(String),
    #[error("{0} in the archive is not in the manifest")]
    Unlisted(String),
    #[error("{0} is in the manifest but not in the archive")]
    Missing(String),
    #[error("{0:?} in the archive would leave its folder")]
    Escapes(String),
    #[error("{0:?} in the archive is not a file or a folder")]
    NotAFile(String),
    #[error("the archive unpacks to more than {MAX_UNPACKED_BYTES} bytes")]
    Unpacked,
}

/// The last manifest UXDuck accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Followed {
    pub published_at: String,
    /// The entry it chose from it, if any listed this UXDuck's protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A version marked bad on this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bad {
    pub reason: String,
    /// When, RFC 3339.
    pub at: String,
    /// The `publishedAt` of the manifest followed then. Only a newer
    /// manifest that names the version again brings it back.
    #[serde(default)]
    pub manifest_published_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// `on` (the default) or `off`, which pins the current interface.
    #[serde(default = "on")]
    pub updates: String,
}

fn on() -> String {
    "on".into()
}

/// A complete interface on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub entry: InterfaceEntry,
    pub dir: PathBuf,
}

/// Whether a version is safe as a folder name: it comes from a signed
/// manifest, but a folder name is checked anyway.
#[must_use]
pub fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 100
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        && version
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && !version.ends_with(".partial")
        && !version.ends_with(".download")
}

#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens the store, deleting what an interrupted install left: partial
    /// folders and downloads, folders without their entry and entries
    /// without their folder.
    ///
    /// # Errors
    /// When the folder cannot be made or read.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let store = Self { dir };
        store.clean()?;
        Ok(store)
    }

    fn clean(&self) -> std::io::Result<()> {
        let mut folders = BTreeSet::new();
        let mut entries = BTreeSet::new();
        for item in std::fs::read_dir(&self.dir)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            if name.ends_with(".partial") || name.ends_with(".download") {
                remove(&path);
            } else if path.is_dir() {
                folders.insert(name);
            } else if let Some(version) = name.strip_suffix(".json")
                && valid_version(version)
                && !matches!(version, "followed" | "bad" | "settings")
            {
                entries.insert(version.to_owned());
            }
        }
        for folder in folders.difference(&entries) {
            remove(&self.dir.join(folder));
        }
        for entry in entries.difference(&folders) {
            remove(&self.dir.join(format!("{entry}.json")));
        }
        Ok(())
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
        let bytes = std::fs::read(self.dir.join(name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_atomic(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{name}.tmp"));
        {
            let mut file = File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, self.dir.join(name))
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
        self.write_atomic(name, &bytes)
    }

    /// The version being served, as last written.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join("current")).ok()?;
        let version = text.trim();
        (!version.is_empty()).then(|| version.to_owned())
    }

    /// # Errors
    /// When it cannot be written.
    pub fn set_current(&self, version: &str) -> std::io::Result<()> {
        self.write_atomic("current", format!("{version}\n").as_bytes())
    }

    #[must_use]
    pub fn followed(&self) -> Option<Followed> {
        self.read_json("followed.json")
    }

    /// # Errors
    /// When it cannot be written.
    pub fn set_followed(&self, followed: &Followed) -> std::io::Result<()> {
        self.write_json("followed.json", followed)
    }

    #[must_use]
    pub fn bad(&self) -> BTreeMap<String, Bad> {
        self.read_json("bad.json").unwrap_or_default()
    }

    /// # Errors
    /// When it cannot be written.
    pub fn mark_bad(&self, version: &str, bad: Bad) -> std::io::Result<()> {
        let mut all = self.bad();
        all.insert(version.to_owned(), bad);
        self.write_json("bad.json", &all)
    }

    #[must_use]
    pub fn settings(&self) -> Settings {
        self.read_json("settings.json")
            .unwrap_or(Settings { updates: on() })
    }

    /// # Errors
    /// When it cannot be written.
    pub fn set_settings(&self, settings: &Settings) -> std::io::Result<()> {
        self.write_json("settings.json", settings)
    }

    /// Every complete interface, newest first.
    #[must_use]
    pub fn kept(&self) -> Vec<Kept> {
        let Ok(items) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut kept: Vec<Kept> = items
            .filter_map(Result::ok)
            .filter(|item| item.path().is_dir())
            .filter_map(|item| {
                let version = item.file_name().to_string_lossy().into_owned();
                if !valid_version(&version) {
                    return None;
                }
                let entry: InterfaceEntry = self.read_json(&format!("{version}.json"))?;
                (entry.version == version && item.path().join("index.html").is_file()).then(|| {
                    Kept {
                        entry,
                        dir: item.path(),
                    }
                })
            })
            .collect();
        kept.sort_by(|a, b| b.entry.published_at.cmp(&a.entry.published_at));
        kept
    }

    /// Where an entry's archive is downloaded to.
    #[must_use]
    pub fn download_path(&self, version: &str) -> PathBuf {
        self.dir.join(format!("{version}.download"))
    }

    /// Installs an entry from its downloaded archive: checks the archive's
    /// size and checksum, unpacks it into `<version>.partial/`, checks
    /// every file against the entry and that none leaves the folder, syncs
    /// and renames it into place, then writes the entry. Any failure
    /// deletes the partial work; the archive is deleted either way.
    ///
    /// # Errors
    /// Any of [`InstallError`].
    pub fn install(&self, entry: &InterfaceEntry, archive: &Path) -> Result<Kept, InstallError> {
        let result = self.install_inner(entry, archive);
        remove(archive);
        if result.is_err() {
            remove(&self.dir.join(format!("{}.partial", entry.version)));
        }
        result
    }

    fn install_inner(&self, entry: &InterfaceEntry, archive: &Path) -> Result<Kept, InstallError> {
        let version = &entry.version;
        if !valid_version(version) {
            return Err(InstallError::BadVersion(version.clone()));
        }
        if entry.archive.size > MAX_ARCHIVE_BYTES {
            return Err(InstallError::TooLarge(entry.archive.size));
        }
        let size = std::fs::metadata(archive)?.len();
        if size != entry.archive.size {
            return Err(InstallError::Size {
                got: size,
                want: entry.archive.size,
            });
        }
        if sha256_file(archive)? != entry.archive.sha256.to_ascii_lowercase() {
            return Err(InstallError::ArchiveHash);
        }
        let partial = self.dir.join(format!("{version}.partial"));
        remove(&partial);
        std::fs::create_dir_all(&partial)?;
        unpack(File::open(archive)?, &partial, &entry.files)?;
        sync_tree(&partial)?;
        let target = self.dir.join(version);
        remove(&target);
        std::fs::rename(&partial, &target)?;
        self.write_json(&format!("{version}.json"), entry)?;
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(Kept {
            entry: entry.clone(),
            dir: target,
        })
    }

    /// Deletes every kept interface but these.
    pub fn keep_only(&self, keep: &BTreeSet<String>) {
        for kept in self.kept() {
            if !keep.contains(&kept.entry.version) {
                remove(&self.dir.join(format!("{}.json", kept.entry.version)));
                remove(&kept.dir);
            }
        }
    }
}

/// Unpacks a `tar.zst` into `into`, checking every file against `files`.
fn unpack(
    archive: impl Read,
    into: &Path,
    files: &BTreeMap<String, String>,
) -> Result<(), InstallError> {
    let decoder = zstd::stream::read::Decoder::new(archive)?;
    let mut tar = tar::Archive::new(decoder);
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for item in tar.entries()? {
        let mut item = item?;
        let raw = item.path()?.to_string_lossy().into_owned();
        let relative = safe_relative(&raw).ok_or_else(|| InstallError::Escapes(raw.clone()))?;
        let kind = item.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(into.join(&relative))?;
            continue;
        }
        if !kind.is_file() {
            return Err(InstallError::NotAFile(raw));
        }
        let name = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let Some(want) = files.get(&name) else {
            return Err(InstallError::Unlisted(name));
        };
        let path = into.join(&relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&path)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = item.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > MAX_UNPACKED_BYTES {
                return Err(InstallError::Unpacked);
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read])?;
        }
        if hex(&hasher.finalize()) != want.to_ascii_lowercase() {
            return Err(InstallError::FileHash(name));
        }
        seen.insert(name);
    }
    if let Some(missing) = files.keys().find(|name| !seen.contains(*name)) {
        return Err(InstallError::Missing(missing.clone()));
    }
    Ok(())
}

/// A path inside the folder: relative, with no `..`, root or drive.
fn safe_relative(raw: &str) -> Option<PathBuf> {
    if raw.contains('\\') {
        return None;
    }
    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn sync_tree(dir: &Path) -> std::io::Result<()> {
    for item in std::fs::read_dir(dir)? {
        let path = item?.path();
        if path.is_dir() {
            sync_tree(&path)?;
        } else {
            File::open(&path)?.sync_all()?;
        }
    }
    // Directories cannot be opened for syncing on Windows.
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// # Errors
/// When the file cannot be read.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn remove(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

//! Per-file app updates (S164). A Mac release lists every item of the signed
//! `UXDuck.app` in its `pkg` ([`BundleFiles`], named by the signed manifest),
//! so an update downloads only the files this computer's app does not already
//! have. The new bundle is built next to the installed one from unchanged
//! installed files (cloned, on APFS) and downloaded ones, and must then match
//! the list exactly: every path, kind, permission, size, checksum and link,
//! and nothing else. Only then does the platform validate and swap it in.
//! Nothing here is specific to macOS, so it is tested everywhere.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uxduck_contracts::dto::release::{BundleEntry, BundleFiles, ReleaseArtifact};

/// The most a list may weigh; the Mac app's is about 150 kB.
pub(super) const MAX_LIST: u64 = 16 << 20;
/// The most a bundle may hold unpacked; the Mac app holds about 330 MB.
pub(super) const MAX_BUNDLE: u64 = 4 << 30;
/// In the update's folder: the checked list, written once its files are here
/// and the candidate bundle passed, which makes it the staged update.
pub(super) const LIST: &str = "files.json";

/// The list the signed manifest names, checked against it and itself.
///
/// # Errors
/// A size or checksum other than the manifest's, a list for another version
/// or target, an unsafe entry, or an unexpected blob location.
pub(super) fn read(
    bytes: &[u8],
    artifact: &ReleaseArtifact,
    version: &str,
    target: &str,
    allowed: impl Fn(&str) -> bool,
) -> Result<BundleFiles, String> {
    if bytes.len() as u64 != artifact.size
        || format!("{:x}", Sha256::digest(bytes)) != artifact.sha256.to_ascii_lowercase()
    {
        return Err("The update's file list does not match the signed release.".into());
    }
    let list: BundleFiles = serde_json::from_slice(bytes)
        .map_err(|e| format!("The update's file list is invalid: {e}"))?;
    list.check()?;
    if list.version != version || list.target != target {
        return Err("The update's file list is for another release or computer.".into());
    }
    if !list.blobs.ends_with('/') || !allowed(&list.blobs) {
        return Err("The update's files must be downloaded over HTTPS.".into());
    }
    let total = list
        .entries
        .iter()
        .map(|entry| match entry {
            BundleEntry::File { size, .. } => *size,
            _ => 0,
        })
        .try_fold(0u64, u64::checked_add);
    if total.is_none_or(|total| total > MAX_BUNDLE) {
        return Err("The update is larger than UXDuck accepts.".into());
    }
    Ok(list)
}

/// The SHA-256 of `path`'s contents.
pub(super) fn sha256(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// The bundle folder's mode and every item under it, sorted by path, as
/// the release lists them (`packed` is 0: it says nothing about a file on
/// disk). Links are read, never followed.
///
/// # Errors
/// An unreadable item, a name that is not UTF-8, or a device, socket or pipe.
pub(super) fn scan(root: &Path) -> Result<(u32, Vec<BundleEntry>), String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let mut entries = Vec::new();
    walk(root, "", &mut entries)?;
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    Ok((meta.permissions().mode() & 0o7777, entries))
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<BundleEntry>) -> Result<(), String> {
    for item in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let item = item.map_err(|e| e.to_string())?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|name| format!("{} in {} is not UTF-8", name.display(), dir.display()))?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let full = item.path();
        let meta = std::fs::symlink_metadata(&full).map_err(|e| format!("{path}: {e}"))?;
        let mode = meta.permissions().mode() & 0o7777;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&full)
                .map_err(|e| format!("{path}: {e}"))?
                .into_os_string()
                .into_string()
                .map_err(|_| format!("{path} links to a name that is not UTF-8"))?;
            out.push(BundleEntry::Symlink { path, target });
        } else if meta.is_dir() {
            out.push(BundleEntry::Dir {
                path: path.clone(),
                mode,
            });
            walk(&full, &path, out)?;
        } else if meta.is_file() {
            out.push(BundleEntry::File {
                sha256: sha256(&full)?,
                size: meta.len(),
                path,
                mode,
                packed: 0,
            });
        } else {
            return Err(format!("{path} is not a file, folder or link"));
        }
    }
    Ok(())
}

/// The installed files the new bundle can reuse: for each regular file whose
/// size some listed file has, its checksum and where it is. Links are not
/// followed, so nothing outside `root` is read.
///
/// # Errors
/// When `root` or an item in it cannot be read.
pub(super) fn index(root: &Path, list: &BundleFiles) -> Result<HashMap<String, PathBuf>, String> {
    let sizes: HashSet<u64> = list
        .entries
        .iter()
        .filter_map(|entry| match entry {
            BundleEntry::File { size, .. } => Some(*size),
            _ => None,
        })
        .collect();
    let mut found = HashMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for item in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let item = item.map_err(|e| e.to_string())?;
            let meta = std::fs::symlink_metadata(item.path()).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                pending.push(item.path());
            } else if meta.is_file() && sizes.contains(&meta.len()) {
                found
                    .entry(sha256(&item.path())?)
                    .or_insert_with(|| item.path());
            }
        }
    }
    Ok(found)
}

/// One download: a file's contents the computer does not have yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Missing {
    pub sha256: String,
    pub size: u64,
    pub packed: u64,
}

/// Each distinct file contents `have` (given its checksum and size) says is
/// not on this computer, once.
pub(super) fn missing(list: &BundleFiles, have: impl Fn(&str, u64) -> bool) -> Vec<Missing> {
    let mut seen = HashSet::new();
    list.entries
        .iter()
        .filter_map(|entry| match entry {
            BundleEntry::File {
                sha256,
                size,
                packed,
                ..
            } if seen.insert(sha256.as_str()) && !have(sha256, *size) => Some(Missing {
                sha256: sha256.clone(),
                size: *size,
                packed: *packed,
            }),
            _ => None,
        })
        .collect()
}

/// Where a downloaded file's checked contents are kept in an update's folder.
pub(super) fn blob(dir: &Path, sha256: &str) -> PathBuf {
    dir.join("blobs").join(sha256)
}

/// The most a compressed file may weigh: zstd never grows data by more.
pub(super) fn packed_limit(size: u64) -> u64 {
    size + size / 128 + (64 << 10)
}

/// Decompresses a downloaded file into its blob, refusing anything but
/// exactly `size` bytes with this checksum. Never leaves a partial blob.
///
/// # Errors
/// A corrupt stream, another size or checksum, or a write failure.
pub(super) fn unpack(compressed: &Path, dir: &Path, sha256: &str, size: u64) -> Result<(), String> {
    let out = blob(dir, sha256);
    let partial = out.with_extension("partial");
    let result = (|| {
        std::fs::create_dir_all(dir.join("blobs")).map_err(|e| e.to_string())?;
        let input = std::fs::File::open(compressed).map_err(|e| e.to_string())?;
        let decoder = zstd::stream::read::Decoder::new(input).map_err(|e| e.to_string())?;
        let mut limited = decoder.take(size + 1);
        let mut file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        let mut written = 0u64;
        loop {
            let read = limited
                .read(&mut buffer)
                .map_err(|e| format!("An update file is corrupt: {e}"))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            hash.update(&buffer[..read]);
            file.write_all(&buffer[..read]).map_err(|e| e.to_string())?;
        }
        if written != size || format!("{:x}", hash.finalize()) != sha256 {
            return Err("An update file does not match the signed release.".to_owned());
        }
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&partial, &out).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

/// Whether a kept blob is still exactly what the list says.
pub(super) fn have_blob(dir: &Path, sha: &str, size: u64) -> bool {
    let path = blob(dir, sha);
    std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size)
        && sha256(&path).is_ok_and(|found| found == sha)
}

/// Builds the bundle at `dest`, which must not exist yet, exactly as listed:
/// each file copied from `source` (a clone on APFS), each folder, link and
/// permission as listed, then checked against the list in full.
///
/// # Errors
/// A file `source` has no contents for, a write failure, or a result that is
/// not exactly the list.
pub(super) fn assemble(
    list: &BundleFiles,
    dest: &Path,
    source: impl Fn(&str) -> Option<PathBuf>,
) -> Result<(), String> {
    std::fs::create_dir(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut dirs = Vec::new();
    for entry in &list.entries {
        let at = dest.join(entry.path());
        match entry {
            BundleEntry::Dir { path, mode } => {
                std::fs::create_dir(&at).map_err(|e| format!("{path}: {e}"))?;
                dirs.push((at, *mode));
            }
            BundleEntry::File {
                path, mode, sha256, ..
            } => {
                let from = source(sha256)
                    .ok_or_else(|| format!("The update is missing the contents of {path}."))?;
                std::fs::copy(&from, &at).map_err(|e| format!("{path}: {e}"))?;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(*mode))
                    .map_err(|e| format!("{path}: {e}"))?;
            }
            BundleEntry::Symlink { path, target } => {
                std::os::unix::fs::symlink(target, &at).map_err(|e| format!("{path}: {e}"))?;
            }
        }
    }
    // Folders last, deepest first, so a read-only folder is filled first.
    for (at, mode) in dirs.iter().rev() {
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(*mode))
            .map_err(|e| e.to_string())?;
    }
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(list.mode))
        .map_err(|e| e.to_string())?;
    verify(list, dest)
}

/// Whether `root` holds exactly the list and nothing more.
///
/// # Errors
/// The first path that is missing, extra or different.
pub(super) fn verify(list: &BundleFiles, root: &Path) -> Result<(), String> {
    let (mode, found) = scan(root)?;
    if mode != list.mode {
        return Err("The updated app's folder has the wrong permissions.".into());
    }
    let expected = list.entries.iter().map(|entry| match entry {
        BundleEntry::File {
            path,
            mode,
            size,
            sha256,
            ..
        } => BundleEntry::File {
            path: path.clone(),
            mode: *mode,
            size: *size,
            sha256: sha256.clone(),
            packed: 0,
        },
        other => other.clone(),
    });
    let mut found = found.into_iter();
    for wanted in expected {
        match found.next() {
            Some(have) if have == wanted => {}
            Some(have) if have.path() == wanted.path() => {
                return Err(format!(
                    "The updated app's {} is not what the release lists.",
                    have.path()
                ));
            }
            Some(have) if have.path() < wanted.path() => {
                return Err(format!(
                    "The updated app has {}, which the release does not list.",
                    have.path()
                ));
            }
            _ => {
                return Err(format!("The updated app is missing {}.", wanted.path()));
            }
        }
    }
    match found.next() {
        Some(extra) => Err(format!(
            "The updated app has {}, which the release does not list.",
            extra.path()
        )),
        None => Ok(()),
    }
}

/// The new bundle at `dest` from the installed app at `installed` and the
/// update folder's downloaded files, preferring installed files, which APFS
/// clones instead of copying.
///
/// # Errors
/// From [`index`] and [`assemble`].
pub(super) fn build(
    dir: &Path,
    list: &BundleFiles,
    installed: &Path,
    dest: &Path,
) -> Result<(), String> {
    let have = index(installed, list)?;
    assemble(list, dest, |sha| {
        have.get(sha).cloned().or_else(|| {
            let kept = blob(dir, sha);
            kept.is_file().then_some(kept)
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "temporary bundle fixtures")]
mod tests {
    use super::*;
    use uxduck_contracts::dto::release::BUNDLE_FILES_SCHEMA;

    const BLOBS: &str = "https://app.uxduck.com/releases/files/";

    fn file(root: &Path, path: &str, bytes: &[u8], mode: u32) {
        let at = root.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, bytes).unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A small app shaped like the Mac one: nested framework links, an
    /// executable, an empty folder and a read-only folder.
    fn app(root: &Path, version: &[u8]) {
        std::fs::create_dir_all(root).unwrap();
        file(root, "Contents/Info.plist", version, 0o644);
        file(root, "Contents/MacOS/uxduck", b"daemon", 0o755);
        file(root, "Contents/Resources/same.txt", b"unchanged", 0o644);
        file(root, "Contents/Resources/copy.txt", b"unchanged", 0o600);
        let framework = "Contents/Frameworks/Electron Framework.framework";
        file(
            root,
            &format!("{framework}/Versions/A/Electron Framework"),
            b"electron",
            0o755,
        );
        std::os::unix::fs::symlink("A", root.join(framework).join("Versions/Current")).unwrap();
        std::os::unix::fs::symlink(
            "Versions/Current/Electron Framework",
            root.join(framework).join("Electron Framework"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("Contents/Empty")).unwrap();
        file(root, "Contents/Locked/readme", b"locked", 0o444);
        std::fs::set_permissions(
            root.join("Contents/Locked"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
    }

    fn list_of(root: &Path) -> BundleFiles {
        let (mode, mut entries) = scan(root).unwrap();
        for entry in &mut entries {
            if let BundleEntry::File { size, packed, .. } = entry {
                *packed = *size;
            }
        }
        let list = BundleFiles {
            schema: BUNDLE_FILES_SCHEMA,
            version: "0.3.0".into(),
            target: "aarch64-apple-darwin".into(),
            blobs: BLOBS.into(),
            mode,
            entries,
        };
        list.check().unwrap();
        list
    }

    /// Lets a fixture with a read-only folder be deleted.
    fn unlock(root: &Path) {
        if root.join("Contents/Locked").exists() {
            std::fs::set_permissions(
                root.join("Contents/Locked"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }

    fn store(dir: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        std::fs::write(blob(dir, &format!("{:x}", Sha256::digest(bytes))), bytes).unwrap();
    }

    fn artifact(bytes: &[u8]) -> ReleaseArtifact {
        ReleaseArtifact {
            target: "aarch64-apple-darwin".into(),
            kind: "files".into(),
            file: "uxduck-0.3.0-aarch64-apple-darwin.files.json".into(),
            url: "https://app.uxduck.com/releases/v0.3.0/x.files.json".into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn the_new_bundle_reuses_installed_files_and_downloads_only_changed_ones() {
        let root = tempfile::tempdir().unwrap();
        let (old, new) = (root.path().join("old.app"), root.path().join("new.app"));
        app(&old, b"0.2.0");
        app(&new, b"0.3.0");
        file(&new, "Contents/MacOS/added", b"new helper", 0o755);
        let list = list_of(&new);
        let have = index(&old, &list).unwrap();
        let wanted = missing(&list, |sha, _| have.contains_key(sha));
        let sizes: Vec<u64> = wanted.iter().map(|m| m.size).collect();
        assert_eq!(sizes, [5, 10], "only Info.plist and the added helper");
        // `unchanged` is listed twice but is on the computer, so never fetched.
        assert!(missing(&list, |_, _| false).len() < list.entries.len());

        let update = root.path().join("update");
        store(&update, b"0.3.0");
        store(&update, b"new helper");
        let candidate = root.path().join("candidate.app");
        build(&update, &list, &old, &candidate).unwrap();
        verify(&list, &candidate).unwrap();
        assert_eq!(
            std::fs::read(candidate.join("Contents/Info.plist")).unwrap(),
            b"0.3.0"
        );
        let framework = candidate.join("Contents/Frameworks/Electron Framework.framework");
        assert_eq!(
            std::fs::read_link(framework.join("Versions/Current")).unwrap(),
            Path::new("A")
        );
        assert_eq!(
            std::fs::read(framework.join("Electron Framework")).unwrap(),
            b"electron",
            "the link resolves inside the new bundle"
        );
        let mode = |path: &str| {
            std::fs::symlink_metadata(candidate.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("Contents/MacOS/added"), 0o755);
        assert_eq!(mode("Contents/Resources/copy.txt"), 0o600);
        assert_eq!(mode("Contents/Locked"), 0o555);
        assert_eq!(mode("Contents/Locked/readme"), 0o444);
        assert!(candidate.join("Contents/Empty").is_dir());
        for app in [&old, &new, &candidate] {
            unlock(app);
        }
    }

    #[test]
    fn a_missing_file_stops_the_build_without_touching_the_installed_app() {
        let root = tempfile::tempdir().unwrap();
        let (old, new) = (root.path().join("old.app"), root.path().join("new.app"));
        app(&old, b"0.2.0");
        app(&new, b"0.3.0");
        let list = list_of(&new);
        let candidate = root.path().join("candidate.app");
        let error = build(&root.path().join("update"), &list, &old, &candidate).unwrap_err();
        assert!(
            error.contains("missing the contents of Contents/Info.plist"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(old.join("Contents/Info.plist")).unwrap(),
            b"0.2.0"
        );
        verify(&list_of(&old), &old).unwrap();
        for app in [&old, &new, &candidate] {
            unlock(app);
        }
    }

    #[test]
    fn verification_finds_missing_extra_tampered_and_changed_items() {
        let root = tempfile::tempdir().unwrap();
        let app_dir = root.path().join("app");
        app(&app_dir, b"0.3.0");
        let list = list_of(&app_dir);
        verify(&list, &app_dir).unwrap();
        let fails = |change: &dyn Fn(&Path), expect: &str| {
            let copy = root.path().join("copy");
            let _ = std::fs::remove_dir_all(&copy);
            assemble(&list, &copy, |sha| {
                index(&app_dir, &list).unwrap().get(sha).cloned()
            })
            .unwrap();
            unlock(&copy);
            change(&copy);
            let mut adjusted = list.clone();
            for entry in &mut adjusted.entries {
                if let BundleEntry::Dir { path, mode } = entry
                    && path == "Contents/Locked"
                {
                    *mode = 0o755;
                }
            }
            let error = verify(&adjusted, &copy).unwrap_err();
            assert!(error.contains(expect), "{error} lacks {expect}");
        };
        fails(
            &|copy| std::fs::write(copy.join("Contents/Info.plist"), b"0.3.1").unwrap(),
            "Contents/Info.plist is not",
        );
        fails(
            &|copy| std::fs::remove_file(copy.join("Contents/MacOS/uxduck")).unwrap(),
            "missing Contents/MacOS/uxduck",
        );
        fails(
            &|copy| std::fs::write(copy.join("Contents/MacOS/extra"), b"x").unwrap(),
            "has Contents/MacOS/extra",
        );
        fails(
            &|copy| std::fs::write(copy.join("Contents/zz-extra"), b"x").unwrap(),
            "has Contents/zz-extra",
        );
        fails(
            &|copy| {
                std::fs::set_permissions(
                    copy.join("Contents/MacOS/uxduck"),
                    std::fs::Permissions::from_mode(0o777),
                )
                .unwrap();
            },
            "Contents/MacOS/uxduck is not",
        );
        fails(
            &|copy| {
                let link =
                    copy.join("Contents/Frameworks/Electron Framework.framework/Versions/Current");
                std::fs::remove_file(&link).unwrap();
                std::os::unix::fs::symlink("B", &link).unwrap();
            },
            "Versions/Current is not",
        );
        fails(
            &|copy| {
                std::fs::remove_dir(copy.join("Contents/Empty")).unwrap();
                std::fs::write(copy.join("Contents/Empty"), b"").unwrap();
            },
            "Contents/Empty is not",
        );
        unlock(&app_dir);
    }

    #[test]
    fn the_list_must_match_the_signed_manifest_and_this_release() {
        let root = tempfile::tempdir().unwrap();
        app(root.path(), b"0.3.0");
        let list = list_of(root.path());
        let bytes = serde_json::to_vec(&list).unwrap();
        let https = |url: &str| url.starts_with("https://");
        let read_as =
            |bytes: &[u8], version, target| read(bytes, &artifact(bytes), version, target, https);
        assert_eq!(
            read_as(&bytes, "0.3.0", "aarch64-apple-darwin").unwrap(),
            list
        );
        // Another checksum or size than the manifest's.
        let mut tampered = bytes.clone();
        tampered[10] ^= 1;
        assert!(
            read(
                &tampered,
                &artifact(&bytes),
                "0.3.0",
                "aarch64-apple-darwin",
                https
            )
            .is_err()
        );
        // Another release or computer.
        assert!(read_as(&bytes, "0.3.1", "aarch64-apple-darwin").is_err());
        assert!(read_as(&bytes, "0.3.0", "x86_64-apple-darwin").is_err());
        // Plain HTTP files, or a list that escapes the bundle.
        let mut plain = list.clone();
        plain.blobs = "http://app.uxduck.com/releases/files/".into();
        let plain = serde_json::to_vec(&plain).unwrap();
        assert!(read_as(&plain, "0.3.0", "aarch64-apple-darwin").is_err());
        let mut escape = list.clone();
        escape.entries.push(BundleEntry::Symlink {
            path: "Contents/z".into(),
            target: "../../../etc".into(),
        });
        let escape = serde_json::to_vec(&escape).unwrap();
        assert!(read_as(&escape, "0.3.0", "aarch64-apple-darwin").is_err());
        // Larger than any app UXDuck accepts.
        let mut huge = list;
        if let Some(BundleEntry::File { size, .. }) = huge
            .entries
            .iter_mut()
            .find(|e| matches!(e, BundleEntry::File { .. }))
        {
            *size = MAX_BUNDLE + 1;
        }
        let huge = serde_json::to_vec(&huge).unwrap();
        assert!(read_as(&huge, "0.3.0", "aarch64-apple-darwin").is_err());
        unlock(root.path());
    }

    #[test]
    fn a_downloaded_file_is_kept_only_when_it_is_exactly_the_listed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let contents = b"the new daemon".repeat(100);
        let sha = format!("{:x}", Sha256::digest(&contents));
        let size = contents.len() as u64;
        let compressed = dir.path().join("download.zst");
        std::fs::write(&compressed, zstd::encode_all(&contents[..], 3).unwrap()).unwrap();
        assert!(compressed.metadata().unwrap().len() <= packed_limit(size));
        // Another size or checksum: nothing is kept.
        assert!(unpack(&compressed, dir.path(), &sha, size - 1).is_err());
        assert!(unpack(&compressed, dir.path(), &"0".repeat(64), size).is_err());
        assert!(!have_blob(dir.path(), &sha, size));
        std::fs::write(&compressed, b"not zstd").unwrap();
        assert!(unpack(&compressed, dir.path(), &sha, size).is_err());
        assert!(!blob(dir.path(), &sha).with_extension("partial").exists());
        std::fs::write(&compressed, zstd::encode_all(&contents[..], 3).unwrap()).unwrap();
        unpack(&compressed, dir.path(), &sha, size).unwrap();
        assert!(have_blob(dir.path(), &sha, size));
        std::fs::write(blob(dir.path(), &sha), b"changed on disk").unwrap();
        assert!(!have_blob(dir.path(), &sha, size));
    }

    #[test]
    fn the_installed_index_never_follows_links_out_of_the_app() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("UXDuck.app");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(root.path().join("secret"), b"0.3.0").unwrap();
        std::os::unix::fs::symlink(root.path().join("secret"), installed.join("link")).unwrap();
        std::os::unix::fs::symlink(root.path(), installed.join("parent")).unwrap();
        let new = root.path().join("new.app");
        app(&new, b"0.3.0");
        let list = list_of(&new);
        assert!(index(&installed, &list).unwrap().is_empty());
        unlock(&new);
    }
}

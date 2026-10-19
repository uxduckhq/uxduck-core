//! The release manifest (ADR-0008 distribution): what a channel currently
//! recommends, with every artifact's checksum. It is signed with minisign;
//! `uxduck update check` verifies the signature before trusting any of it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A channel's pointer, such as `stable.json`, published at each release and
/// re-published, newer and signed again, to roll back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    /// Bumped only for incompatible changes to this format.
    pub schema: u32,
    /// Such as `stable`.
    pub channel: String,
    /// The version the channel recommends: the newest, or an older one after
    /// a rollback.
    pub version: String,
    /// When this pointer was published, RFC 3339. Clients refuse a manifest
    /// older than the newest they have seen, so an old signed manifest
    /// cannot be replayed; a rollback is a new, later manifest.
    pub published_at: String,
    /// Set when `version` is older than a version released before it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rollback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    pub artifacts: Vec<ReleaseArtifact>,
    /// The desktop app this release goes with (S110, ADR-0025 D6): its own
    /// version, which changes only when Electron or its code does, so a new
    /// UXDuck downloads it again only when this names another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_app: Option<DesktopAppRelease>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppRelease {
    pub version: String,
    pub artifacts: Vec<DesktopAppArtifact>,
    /// SHA-256 of what this version was built from (S164). A release whose
    /// desktop app has the same version and inputs reuses these signed
    /// archives instead of building and signing them again, so the
    /// desktop app's bytes stay the same and a per-file update skips them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<String>,
}

/// One platform's desktop app: an archive, checked by size and SHA-256
/// against this signed manifest before it is unpacked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppArtifact {
    /// The Rust target triple it runs on.
    pub target: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the archive.
    pub sha256: String,
    /// The archive's size, what is downloaded.
    pub size: u64,
    /// What it takes on disk once unpacked.
    pub unpacked_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseArtifact {
    /// The Rust target triple (`universal-apple-darwin` for macOS before 0.1.1).
    pub target: String,
    /// `pkg`, `msi`, `deb`, `tar.gz` or `zip`, or `files`: the [`BundleFiles`]
    /// of the app inside that target's `pkg` (S164).
    pub kind: String,
    pub file: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    pub size: u64,
}

/// The only schema this build understands.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The artifact kind of a [`BundleFiles`] list.
pub const BUNDLE_FILES_KIND: &str = "files";

/// The only [`BundleFiles`] schema this build understands.
pub const BUNDLE_FILES_SCHEMA: u32 = 1;

/// Every file, folder and link in the signed `UXDuck.app` inside one Mac
/// `pkg` (S164), so an update downloads only the files that changed. The
/// signed manifest names this list's SHA-256 as a [`ReleaseArtifact`] of kind
/// [`BUNDLE_FILES_KIND`]; each file's contents are at
/// `<blobs><sha256>.zst`, zstd-compressed and named by the SHA-256 of the
/// uncompressed bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleFiles {
    pub schema: u32,
    /// The release this bundle is.
    pub version: String,
    /// The Rust target triple of the `pkg` it came from.
    pub target: String,
    /// The URL every file's compressed contents start with, ending in `/`.
    pub blobs: String,
    /// The bundle folder's own permission bits.
    pub mode: u32,
    /// Sorted by path, so a folder comes before what it holds.
    pub entries: Vec<BundleEntry>,
}

/// One item in the bundle. Paths are relative to the bundle and use `/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BundleEntry {
    Dir {
        path: String,
        /// Permission bits, such as `0o755` (493).
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        size: u64,
        /// Lowercase hex SHA-256 of the uncompressed contents.
        sha256: String,
        /// The compressed size: what downloading it costs.
        packed: u64,
    },
    Symlink {
        path: String,
        /// Relative, and inside the bundle, as Electron's framework links are.
        target: String,
    },
}

impl BundleEntry {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Dir { path, .. } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

/// The most entries a list may hold; the Mac app has about 600.
pub const BUNDLE_FILES_MAX_ENTRIES: usize = 100_000;

impl BundleFiles {
    /// Rejects a list that could write outside the bundle or is ambiguous:
    /// absolute, empty or `..` paths, unsorted or repeated paths, an item
    /// whose folder is not listed before it, a link leaving the bundle, a
    /// bad checksum, or permission bits beyond `0o777`.
    ///
    /// # Errors
    /// What is wrong with the first bad entry.
    pub fn check(&self) -> Result<(), String> {
        if self.schema != BUNDLE_FILES_SCHEMA {
            return Err(format!("file list schema {} is not 1", self.schema));
        }
        if self.entries.len() > BUNDLE_FILES_MAX_ENTRIES {
            return Err("the file list has too many entries".into());
        }
        if self.mode & !0o777 != 0 {
            return Err("the bundle has unexpected permission bits".into());
        }
        let mut dirs = std::collections::HashSet::new();
        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            let path = entry.path();
            check_path(path)?;
            if previous.is_some_and(|p| p >= path) {
                return Err(format!("{path} is out of order or repeated"));
            }
            previous = Some(path);
            if let Some((parent, _)) = path.rsplit_once('/')
                && !dirs.contains(parent)
            {
                return Err(format!("{path} is in a folder the list does not have"));
            }
            match entry {
                BundleEntry::Dir { mode, .. } => {
                    if mode & !0o777 != 0 {
                        return Err(format!("{path} has unexpected permission bits"));
                    }
                    dirs.insert(path);
                }
                BundleEntry::File { mode, sha256, .. } => {
                    if mode & !0o777 != 0 {
                        return Err(format!("{path} has unexpected permission bits"));
                    }
                    if sha256.len() != 64
                        || !sha256
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return Err(format!("{path} has no SHA-256"));
                    }
                }
                BundleEntry::Symlink { target, .. } => check_link(path, target)?,
            }
        }
        Ok(())
    }
}

fn check_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("{path:?} is not a path inside the bundle"));
    }
    Ok(())
}

/// A link's target, resolved from the link's folder, must stay in the bundle.
fn check_link(path: &str, target: &str) -> Result<(), String> {
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return Err(format!("{path} links outside the bundle"));
    }
    let mut depth: Vec<&str> = path.split('/').collect();
    depth.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if depth.pop().is_none() {
                    return Err(format!("{path} links outside the bundle"));
                }
            }
            other => depth.push(other),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn file(path: &str) -> BundleEntry {
        BundleEntry::File {
            path: path.into(),
            mode: 0o644,
            size: 1,
            sha256: "a".repeat(64),
            packed: 10,
        }
    }

    fn dir(path: &str) -> BundleEntry {
        BundleEntry::Dir {
            path: path.into(),
            mode: 0o755,
        }
    }

    fn link(path: &str, target: &str) -> BundleEntry {
        BundleEntry::Symlink {
            path: path.into(),
            target: target.into(),
        }
    }

    fn list(entries: Vec<BundleEntry>) -> BundleFiles {
        BundleFiles {
            schema: 1,
            version: "0.3.0".into(),
            target: "aarch64-apple-darwin".into(),
            blobs: "https://app.uxduck.com/releases/files/".into(),
            mode: 0o755,
            entries,
        }
    }

    #[test]
    fn a_manifest_without_files_or_inputs_still_reads_and_round_trips() {
        let old = r#"{"schema":1,"channel":"stable","version":"0.2.0","publishedAt":"2026-10-17T00:00:00Z","artifacts":[{"target":"aarch64-apple-darwin","kind":"pkg","file":"a.pkg","url":"https://x/a.pkg","sha256":"00","size":1}],"desktopApp":{"version":"0.2.0","artifacts":[]}}"#;
        let manifest: ReleaseManifest = serde_json::from_str(old).unwrap();
        assert_eq!(manifest.desktop_app.as_ref().unwrap().inputs, None);
        let text = serde_json::to_string(&manifest).unwrap();
        assert!(!text.contains("inputs"));
        assert_eq!(
            serde_json::from_str::<ReleaseManifest>(&text).unwrap(),
            manifest
        );
        let mut new = manifest;
        new.desktop_app.as_mut().unwrap().inputs = Some("ab".into());
        new.artifacts.push(ReleaseArtifact {
            target: "aarch64-apple-darwin".into(),
            kind: BUNDLE_FILES_KIND.into(),
            file: "uxduck-0.2.0-aarch64-apple-darwin.files.json".into(),
            url: "https://x/files.json".into(),
            sha256: "00".into(),
            size: 2,
        });
        let text = serde_json::to_string(&new).unwrap();
        assert!(text.contains(r#""inputs":"ab""#));
        assert_eq!(serde_json::from_str::<ReleaseManifest>(&text).unwrap(), new);
    }

    #[test]
    fn bundle_entries_are_tagged_by_kind_and_round_trip() {
        let files = list(vec![
            dir("Contents"),
            link("Contents/Current", "Info.plist"),
            file("Contents/Info.plist"),
        ]);
        let text = serde_json::to_string(&files).unwrap();
        assert!(text.contains(r#"{"kind":"dir","path":"Contents","mode":493}"#));
        assert!(
            text.contains(r#""kind":"symlink","path":"Contents/Current","target":"Info.plist""#)
        );
        assert_eq!(serde_json::from_str::<BundleFiles>(&text).unwrap(), files);
        assert!(files.check().is_ok());
    }

    #[test]
    fn a_list_that_could_escape_the_bundle_or_is_ambiguous_is_refused() {
        let good = || vec![dir("Contents"), file("Contents/a")];
        assert!(list(good()).check().is_ok());
        for bad in [
            vec![file("/etc/passwd")],
            vec![file("../outside")],
            vec![dir("Contents"), file("Contents/../../x")],
            vec![dir("Contents"), file("Contents//a")],
            vec![dir("Contents"), file("Contents/./a")],
            vec![file("")],
            vec![file("Contents/a")],
            vec![dir("Contents"), file("Contents/b"), file("Contents/a")],
            vec![dir("Contents"), file("Contents/a"), file("Contents/a")],
            vec![file("Contents"), file("Contents/a")],
            vec![dir("Contents"), link("Contents/l", "/Applications")],
            vec![dir("Contents"), link("Contents/l", "../../x")],
            vec![dir("Contents"), link("Contents/l", "")],
        ] {
            assert!(list(bad.clone()).check().is_err(), "accepted {bad:?}");
        }
        let mut setuid = good();
        setuid[1] = BundleEntry::File {
            path: "Contents/a".into(),
            mode: 0o4755,
            size: 1,
            sha256: "a".repeat(64),
            packed: 1,
        };
        assert!(list(setuid).check().is_err());
        let mut hash = good();
        hash[1] = BundleEntry::File {
            path: "Contents/a".into(),
            mode: 0o644,
            size: 1,
            sha256: "A".repeat(64),
            packed: 1,
        };
        assert!(list(hash).check().is_err());
        assert!(
            list(vec![dir("Contents"), link("Contents/l", "../Contents/./a")])
                .check()
                .is_ok()
        );
        let mut schema = list(good());
        schema.schema = 2;
        assert!(schema.check().is_err());
    }
}

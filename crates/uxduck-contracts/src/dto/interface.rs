//! The interface (ADR-0025 D3, S109): the pages people use UXDuck through,
//! published by every web deploy and kept on disk by UXDuck, so an installed
//! UXDuck serves the newest compatible one online and the last one it
//! followed offline (`docs/plan/interface-updates.md`).
//!
//! The manifest is not part of the RPC protocol: the web deploy writes it
//! (`cargo xtask interface-manifest`) and UXDuck reads it. `InterfaceStatus`
//! is: `interface/status` answers it and `interface/updated` pushes it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The only manifest schema this build reads.
pub const INTERFACE_MANIFEST_SCHEMA: u32 = 1;

/// The largest archive UXDuck downloads.
pub const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

/// `interface/manifest.json` on app.uxduck.com, signed with the release key
/// (`manifest.json.minisig`): the newest entry for each protocol version
/// still in the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceManifest {
    /// Bumped only for incompatible changes to this format.
    pub schema: u32,
    /// `production` or `staging`.
    pub channel: String,
    /// When this manifest was published, RFC 3339 in UTC. UXDuck refuses
    /// one older than the last it followed; a rollback is a newer manifest
    /// naming an older entry.
    pub published_at: String,
    pub entries: Vec<InterfaceEntry>,
}

/// One published interface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceEntry {
    /// The build's id, as its `/version.json` reports it:
    /// `0.1.0+e05d65f4.20261011T1200`.
    pub version: String,
    /// When it was built and published, RFC 3339 in UTC.
    pub published_at: String,
    /// The protocol versions it talks, newest first.
    pub protocols: Vec<u32>,
    pub archive: InterfaceArchive,
    /// Every file in the archive by its path, with its lowercase hex
    /// SHA-256.
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceArchive {
    /// A `tar.zst` of the build.
    pub url: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the archive.
    pub sha256: String,
}

impl InterfaceManifest {
    /// The newest entry first, by `publishedAt` (RFC 3339 in UTC compares
    /// as text).
    #[must_use]
    pub fn newest_first(&self) -> Vec<&InterfaceEntry> {
        let mut entries: Vec<&InterfaceEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| b.published_at.cmp(&a.published_at));
        entries
    }
}

/// Which interface UXDuck serves, for the next page load, and how its
/// updates are going. Answers `interface/status`; pushed as
/// `interface/updated` whenever it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceStatus {
    /// The interface a page loaded now gets: its build id, as its
    /// `/version.json` says. A page whose own id differs is older or newer
    /// and offers a reload.
    pub version: String,
    /// When it was built, RFC 3339; none when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// The copy built into UXDuck, not one downloaded.
    #[serde(default)]
    pub built_in: bool,
    /// Whether UXDuck follows the published interface: off with
    /// `uxduck config set interface.updates off`, in development and in
    /// builds without the release key.
    #[serde(default)]
    pub updates: bool,
    /// The newest published interface needs a newer UXDuck: "Update UXDuck
    /// to get the latest version".
    #[serde(default)]
    pub update_uxduck: bool,
    /// When the last check finished, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub checked_at_ms: Option<u64>,
    /// Why the last check or install failed, in words for the person:
    /// "Couldn't check for a new interface: no connection".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

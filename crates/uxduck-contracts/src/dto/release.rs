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
    /// `pkg`, `msi`, `deb`, `tar.gz` or `zip`.
    pub kind: String,
    pub file: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    pub size: u64,
}

/// The only schema this build understands.
pub const MANIFEST_SCHEMA: u32 = 1;

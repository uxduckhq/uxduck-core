//! The self-update check (ADR-0008): fetch a channel's release manifest,
//! verify its minisign signature against the key built into this binary,
//! refuse one older than the newest already seen (so an old signed manifest
//! cannot be replayed), and compare versions. It never downloads or installs
//! anything by itself.
//!
//! Its user agent says the version, platform and architecture, and two
//! markers with no identifier: `first` on the first check this data
//! directory ever makes, and `today` on the first of each UTC day. The app
//! Worker counts them as new installs and installs in use, whatever the
//! analytics switch says (ADR-0016 O7).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uxduck_contracts::dto::release::{MANIFEST_SCHEMA, ReleaseArtifact, ReleaseManifest};

/// Where channels are published: the production app's static site.
pub const DEFAULT_URL: &str = match option_env!("UXDUCK_UPDATE_TEST_URL") {
    Some(url) => url,
    None => "https://app.uxduck.com/releases/stable.json",
};

/// Isolated installer tests bake their feed and a separate signing key into
/// both versions. The full-app updater in ordinary releases ignores runtime
/// feed changes.
pub const APP_UPDATE_PUBLIC_KEY: Option<&str> = match option_env!("UXDUCK_UPDATE_TEST_KEY") {
    Some(key) => Some(key),
    None => PUBLIC_KEY,
};

pub const IS_UPDATE_TEST: bool = option_env!("UXDUCK_UPDATE_TEST_URL").is_some();

/// The release public key (the base64 line of `minisign.pub`), set by the
/// release workflow at build time. Development builds have none and do not
/// check.
pub const PUBLIC_KEY: Option<&str> = option_env!("UXDUCK_RELEASE_PUBLIC_KEY");

/// The release key for signed data (terrain) in a development daemon: the
/// one built in, else the checkout's copy that `cargo xtask release-key`
/// keeps. Only debug builds read the copy.
#[must_use]
pub fn development_key() -> Option<String> {
    PUBLIC_KEY.map(str::to_owned).or_else(|| {
        let kept = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.cache/release-public-key"
        );
        cfg!(debug_assertions)
            .then(|| std::fs::read_to_string(kept).ok())
            .flatten()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
    })
}

/// A `UXDUCK_*_URL` override of where signed data comes from: `none` turns
/// it off in any build; another address only a development build takes, so
/// a release downloads its simulators, terrain, definitions and interface
/// from production as everyone's does.
#[must_use]
pub fn url_override(name: &str) -> Option<String> {
    let url = std::env::var(name).ok()?;
    if url == "none" || cfg!(debug_assertions) {
        return Some(url);
    }
    tracing::warn!("{name} is for development builds; this release ignores it");
    None
}

pub const TARGET: &str = env!("UXDUCK_TARGET");

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("this build has no release key, so it cannot check for updates")]
    NoKey,
    #[error("cannot fetch {url}: {reason}")]
    Fetch { url: String, reason: String },
    #[error("the manifest's signature does not verify: {0}")]
    Signature(String),
    #[error("the manifest is not valid: {0}")]
    Invalid(String),
    #[error(
        "the manifest was published at {offered}, before one already seen ({seen}); it may be replayed, so it is ignored"
    )]
    Replayed { offered: String, seen: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    UpToDate,
    /// A newer version, and this platform's download if the release has one.
    Available(Option<ReleaseArtifact>),
    /// The channel recommends an older version: a rollback.
    RolledBack(Option<ReleaseArtifact>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub current: String,
    pub manifest: ReleaseManifest,
    pub verdict: Verdict,
    /// Exact signed bytes retained for the installer worker to verify again.
    pub signed_bytes: Vec<u8>,
    pub signature: String,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let download = |artifact: &Option<ReleaseArtifact>| {
            artifact.as_ref().map_or_else(
                || format!(" There is no build for {TARGET} in it."),
                |a| format!("\n  {} ({} bytes, sha256 {})", a.url, a.size, a.sha256),
            )
        };
        match &self.verdict {
            Verdict::UpToDate => write!(f, "UXDuck {} is up to date.", self.current),
            Verdict::Available(artifact) => write!(
                f,
                "UXDuck {} is available (this is {}).{}",
                self.manifest.version,
                self.current,
                download(artifact)
            ),
            Verdict::RolledBack(artifact) => write!(
                f,
                "The {} channel was rolled back to {} (this is {}). Consider installing it.{}",
                self.manifest.channel,
                self.manifest.version,
                self.current,
                download(artifact)
            ),
        }
    }
}

/// What the last good check saw, kept in the data directory.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Seen {
    #[serde(default)]
    published_at: String,
    /// The UTC day of the last check that reached the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_check_day: Option<String>,
}

/// The update check's user agent: `uxduck/0.4.1 (macos; aarch64; first; today)`.
#[must_use]
pub fn user_agent(version: &str, first: bool, today: bool) -> String {
    let mut parts = vec![crate::analytics::platform(), crate::analytics::arch()];
    if first {
        parts.push("first");
    }
    if today {
        parts.push("today");
    }
    format!("uxduck/{version} ({})", parts.join("; "))
}

/// The markers for a check now: whether none has ever reached the server
/// from this data directory, and whether none has today.
fn markers(seen: Option<&Seen>, today: &str) -> (bool, bool) {
    let first = seen.is_none();
    let checked_today = seen
        .and_then(|s| s.last_check_day.as_deref())
        .is_some_and(|day| day == today);
    (first, !checked_today)
}

/// The artifact for this platform: this target's, preferring an installer.
/// Releases before 0.1.1 had one universal macOS build, which any Mac takes.
#[must_use]
pub fn artifact_for(manifest: &ReleaseManifest, target: &str) -> Option<ReleaseArtifact> {
    let rank = |kind: &str| match kind {
        "pkg" | "msi" | "deb" => 0,
        _ => 1,
    };
    let best = |wanted: &str| {
        manifest
            .artifacts
            .iter()
            .filter(|a| a.target == wanted)
            .min_by_key(|a| rank(&a.kind))
            .cloned()
    };
    best(target).or_else(|| {
        target
            .ends_with("apple-darwin")
            .then(|| best("universal-apple-darwin"))
            .flatten()
    })
}

/// Whether `bytes` carry a good minisign `signature` by `public_key` (the
/// base64 line). Also the interface manifest's check (S109).
///
/// # Errors
/// A bad key or signature.
pub fn verify_signature(
    bytes: &[u8],
    signature: &str,
    public_key: &str,
) -> Result<(), UpdateError> {
    let key = minisign_verify::PublicKey::from_base64(public_key.trim())
        .map_err(|e| UpdateError::Signature(format!("the public key: {e}")))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| UpdateError::Signature(e.to_string()))?;
    key.verify(bytes, &signature, false)
        .map_err(|e| UpdateError::Signature(e.to_string()))
}

/// Verifies and reads a manifest.
///
/// # Errors
/// A bad key, signature or document.
pub fn verify(
    bytes: &[u8],
    signature: &str,
    public_key: &str,
) -> Result<ReleaseManifest, UpdateError> {
    verify_signature(bytes, signature, public_key)?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(bytes).map_err(|e| UpdateError::Invalid(e.to_string()))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(UpdateError::Invalid(format!(
            "schema {} is not one this build reads ({MANIFEST_SCHEMA})",
            manifest.schema
        )));
    }
    Ok(manifest)
}

/// Compares this build with a verified manifest.
///
/// # Errors
/// When a version does not parse.
pub fn compare(
    current: &str,
    manifest: ReleaseManifest,
    target: &str,
) -> Result<Report, UpdateError> {
    let parse = |v: &str| {
        semver::Version::parse(v).map_err(|e| UpdateError::Invalid(format!("version {v}: {e}")))
    };
    let (have, offered) = (parse(current)?, parse(&manifest.version)?);
    let artifact = artifact_for(&manifest, target);
    let verdict = match offered.cmp(&have) {
        std::cmp::Ordering::Greater => Verdict::Available(artifact),
        std::cmp::Ordering::Less if manifest.rollback => Verdict::RolledBack(artifact),
        // The same version, or an older one not marked as a rollback: this
        // build is ahead of the channel (a pre-release, or a newer channel).
        std::cmp::Ordering::Equal | std::cmp::Ordering::Less => Verdict::UpToDate,
    };
    Ok(Report {
        current: current.to_owned(),
        manifest,
        verdict,
        signed_bytes: Vec::new(),
        signature: String::new(),
    })
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, UpdateError> {
    let failed = |reason: String| UpdateError::Fetch {
        url: url.to_owned(),
        reason,
    };
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(response.status().to_string()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| failed(e.to_string()))? {
        if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
            return Err(failed("The signed release metadata is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Fetches, verifies and compares, remembering what it saw in `data_dir`.
///
/// # Errors
/// Any of [`UpdateError`].
pub async fn check(
    url: &str,
    public_key: Option<&str>,
    data_dir: &Path,
) -> Result<Report, UpdateError> {
    let public_key = public_key.ok_or(UpdateError::NoKey)?;
    let seen_path = data_dir.join("update.json");
    let read: Option<Seen> = std::fs::read(&seen_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let today = crate::analytics::utc_day(now_us());
    let (first, new_today) = markers(read.as_ref(), &today);
    let mut seen = read.unwrap_or_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(user_agent(env!("CARGO_PKG_VERSION"), first, new_today))
        .build()
        .map_err(|e| UpdateError::Fetch {
            url: url.to_owned(),
            reason: e.to_string(),
        })?;
    let bytes = fetch(&client, url).await?;
    // It reached the server, so it was counted: the markers are spent.
    seen.last_check_day = Some(today);
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&seen_path, serde_json::to_vec(&seen).unwrap_or_default());
    let signature = fetch(&client, &format!("{url}.minisig")).await?;
    let manifest = verify(&bytes, &String::from_utf8_lossy(&signature), public_key)?;
    // RFC 3339 in UTC compares as text.
    if manifest.published_at < seen.published_at {
        return Err(UpdateError::Replayed {
            offered: manifest.published_at,
            seen: seen.published_at,
        });
    }
    seen.published_at.clone_from(&manifest.published_at);
    let _ = std::fs::write(&seen_path, serde_json::to_vec(&seen).unwrap_or_default());
    let mut report = compare(env!("CARGO_PKG_VERSION"), manifest, TARGET)?;
    report.signed_bytes = bytes;
    report.signature = String::from_utf8_lossy(&signature).into_owned();
    Ok(report)
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// How long until the next check: just after the next UTC midnight, plus
/// `spread` so installs do not all ask in the same second. A running daemon
/// therefore checks on every UTC day, which a plain 24-hour sleep does not
/// promise.
#[must_use]
pub fn until_next_day(now_us: u64, spread: Duration) -> Duration {
    const DAY_US: u64 = 86_400_000_000;
    let into_day = now_us % DAY_US;
    Duration::from_micros(DAY_US - into_day) + spread
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, rollback: bool) -> ReleaseManifest {
        let artifact = |target: &str, kind: &str| ReleaseArtifact {
            target: target.into(),
            kind: kind.into(),
            file: format!("uxduck-{version}-{target}.{kind}"),
            url: format!("https://example.test/uxduck-{version}-{target}.{kind}"),
            sha256: "00".into(),
            size: 1,
        };
        ReleaseManifest {
            schema: 1,
            channel: "stable".into(),
            version: version.into(),
            published_at: "2026-10-01T00:00:00Z".into(),
            rollback,
            notes_url: None,
            desktop_app: None,
            artifacts: vec![
                artifact("universal-apple-darwin", "tar.gz"),
                artifact("universal-apple-darwin", "pkg"),
                artifact("x86_64-unknown-linux-gnu", "deb"),
            ],
        }
    }

    #[test]
    fn the_markers_count_a_new_install_once_and_each_day_once() {
        assert_eq!(markers(None, "2026-09-30"), (true, true));
        let seen = Seen {
            published_at: String::new(),
            last_check_day: Some("2026-09-30".into()),
        };
        assert_eq!(markers(Some(&seen), "2026-09-30"), (false, false));
        assert_eq!(markers(Some(&seen), "2026-10-01"), (false, true));
        // A data directory checked before the markers existed is not new.
        let old = Seen::default();
        assert_eq!(markers(Some(&old), "2026-10-01"), (false, true));

        let agent = user_agent("0.4.1", true, true);
        assert!(agent.starts_with("uxduck/0.4.1 ("));
        assert!(agent.ends_with("; first; today)"));
        assert!(!user_agent("0.4.1", false, false).contains("today"));
    }

    #[test]
    fn the_next_check_is_just_after_the_next_utc_midnight() {
        let day = 86_400_000_000u64;
        let spread = Duration::from_secs(30);
        // 23:00 UTC: one hour, then the spread.
        assert_eq!(
            until_next_day(10 * day + 23 * 3_600_000_000, spread),
            Duration::from_secs(3600 + 30)
        );
        assert_eq!(
            until_next_day(10 * day, Duration::ZERO),
            Duration::from_hours(24)
        );
    }

    #[test]
    fn versions_compare_and_a_platform_gets_its_installer() {
        let newer = compare("0.1.0", manifest("0.2.0", false), "aarch64-apple-darwin").unwrap();
        let Verdict::Available(Some(artifact)) = &newer.verdict else {
            panic!("{newer:?}");
        };
        assert_eq!(artifact.kind, "pkg", "the installer before the archive");
        assert_eq!(
            compare("0.2.0", manifest("0.2.0", false), "x")
                .unwrap()
                .verdict,
            Verdict::UpToDate
        );
        assert!(matches!(
            compare("0.2.0", manifest("0.1.0", true), "x86_64-unknown-linux-gnu")
                .unwrap()
                .verdict,
            Verdict::RolledBack(Some(_))
        ));
        assert_eq!(
            compare("0.3.0-beta.1", manifest("0.2.0", false), "x")
                .unwrap()
                .verdict,
            Verdict::UpToDate,
            "ahead of the channel"
        );
        let windows = compare("0.1.0", manifest("0.2.0", false), "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(windows.verdict, Verdict::Available(None));
        assert!(windows.to_string().contains("no build for"));
    }

    #[test]
    fn a_mac_takes_its_own_architecture_or_an_older_universal_build() {
        let mut split = manifest("0.2.0", false);
        for (artifact, target) in split
            .artifacts
            .iter_mut()
            .zip(["aarch64-apple-darwin", "x86_64-apple-darwin"])
        {
            artifact.target = target.into();
            artifact.kind = "pkg".into();
            artifact.file = format!("uxduck-0.2.0-{target}.pkg");
        }
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
            let found = artifact_for(&split, target).unwrap();
            assert_eq!(found.target, target);
        }
        let universal = manifest("0.2.0", false);
        assert_eq!(
            artifact_for(&universal, "x86_64-apple-darwin")
                .unwrap()
                .target,
            "universal-apple-darwin"
        );
        let arm_only = ReleaseManifest {
            artifacts: split.artifacts[..1].to_vec(),
            ..split
        };
        assert_eq!(
            artifact_for(&arm_only, "x86_64-apple-darwin"),
            None,
            "never the other architecture"
        );
    }
}

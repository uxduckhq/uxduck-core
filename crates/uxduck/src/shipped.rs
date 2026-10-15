//! Telling someone their suggestion shipped (S73): suggestions sent from
//! this install with "tell me when it ships" are kept in `suggestions.json`
//! in the data directory, and `docs/releases/suggestions.json`, built in,
//! lists the issues each release ships. When this install runs a version
//! that ships one it has not told about, it says so once. A suggestion
//! credited to an account is told by email instead.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The issues each release ships, by version, as the maintainer lists them
/// when releasing.
const RELEASES: &str = include_str!("../../../docs/releases/suggestions.json");

#[derive(Debug, Default, Deserialize)]
struct Releases {
    #[serde(default)]
    releases: std::collections::BTreeMap<String, Vec<String>>,
}

/// One suggestion sent from here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    /// The Linear issue, such as `FB-12`.
    pub issue: String,
    pub title: String,
    /// Credited to a signed-in account, which is emailed instead.
    pub by_email: bool,
    /// Told already, in this version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub told_in: Option<String>,
}

fn read(path: &Path) -> Vec<Sent> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write(path: &Path, sent: &[Sent]) {
    if let Ok(json) = serde_json::to_vec_pretty(sent) {
        let _ = std::fs::write(path, json);
    }
}

/// Keeps a suggestion that was sent, once.
pub fn remember(path: &Path, issue: &str, title: &str, by_email: bool) {
    let mut sent = read(path);
    if sent.iter().any(|s| s.issue == issue) {
        return;
    }
    sent.push(Sent {
        issue: issue.to_owned(),
        title: title.chars().take(120).collect(),
        by_email,
        told_in: None,
    });
    write(path, &sent);
}

/// What to say now, as (issue, title, version): suggestions kept here that
/// a release up to `version` ships, not credited to an account and not yet
/// told. Marks them told.
#[must_use]
pub fn due(path: &Path, version: &str) -> Vec<(String, String, String)> {
    due_in(path, version, RELEASES)
}

fn due_in(path: &Path, version: &str, releases: &str) -> Vec<(String, String, String)> {
    let releases: Releases = serde_json::from_str(releases).unwrap_or_default();
    let mut sent = read(path);
    let mut due = Vec::new();
    for suggestion in sent
        .iter_mut()
        .filter(|s| !s.by_email && s.told_in.is_none())
    {
        let shipped_in = releases
            .releases
            .iter()
            .filter(|(release, _)| newer_or_same(version, release))
            .find(|(_, issues)| issues.contains(&suggestion.issue))
            .map(|(release, _)| release.clone());
        if let Some(release) = shipped_in {
            suggestion.told_in = Some(version.to_owned());
            due.push((suggestion.issue.clone(), suggestion.title.clone(), release));
        }
    }
    if !due.is_empty() {
        write(path, &sent);
    }
    due
}

/// Whether `version` is `release` or later, comparing `major.minor.patch`.
fn newer_or_same(version: &str, release: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(version) >= parts(release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_is_told_once_when_a_release_up_to_this_version_ships_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suggestions.json");
        remember(&path, "FB-12", "Airspeed for rovers", false);
        remember(&path, "FB-12", "Airspeed for rovers", false);
        remember(&path, "FB-13", "Credited", true);
        remember(&path, "FB-14", "Not yet", false);
        let releases = r#"{ "releases": { "0.2.0": ["FB-12", "FB-13"], "0.3.0": ["FB-14"] } }"#;

        assert!(
            due_in(&path, "0.1.9", releases).is_empty(),
            "not shipped yet"
        );
        assert_eq!(
            due_in(&path, "0.2.1", releases),
            [(
                "FB-12".to_owned(),
                "Airspeed for rovers".to_owned(),
                "0.2.0".to_owned()
            )],
            "once, and not the one told by email"
        );
        assert!(due_in(&path, "0.2.1", releases).is_empty(), "told already");
        assert_eq!(due_in(&path, "0.10.0", releases)[0].0, "FB-14");
        assert!(newer_or_same("0.10.0", "0.9.1"));
        assert!(!newer_or_same("0.2.0-beta", "0.3.0"));
    }

    #[test]
    fn the_built_in_release_list_reads() {
        let releases: Releases = serde_json::from_str(RELEASES).unwrap();
        assert!(releases.releases.values().all(|issues| !issues.is_empty()));
    }
}

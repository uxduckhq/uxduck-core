//! Which published entry to follow and which interface to serve
//! (interface-updates.md, "What it accepts" and "Choosing what to serve").
//! Pure, so every rule has a test.

use std::collections::BTreeMap;

use uxduck_contracts::dto::interface::{InterfaceEntry, InterfaceManifest};

use super::store::{Bad, Kept};

/// What a manifest offers this UXDuck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    /// The newest entry that lists this protocol and is not bad.
    pub entry: Option<InterfaceEntry>,
    /// The newest entry needs a newer UXDuck.
    pub update_uxduck: bool,
}

/// Whether a version marked bad stays out: always, unless a manifest newer
/// than the one followed when it was marked names it again.
fn excluded(bad: &BTreeMap<String, Bad>, version: &str, manifest_published_at: &str) -> bool {
    bad.get(version)
        .is_some_and(|bad| manifest_published_at <= bad.manifest_published_at.as_str())
}

/// The entry to follow for `protocol`: the newest that lists it and is not
/// bad (points 3 and 4).
#[must_use]
pub fn pick(manifest: &InterfaceManifest, protocol: u32, bad: &BTreeMap<String, Bad>) -> Pick {
    let entries = manifest.newest_first();
    let update_uxduck = entries.first().is_some_and(|newest| {
        !newest.protocols.contains(&protocol) && newest.protocols.iter().any(|p| *p > protocol)
    });
    let entry = entries
        .into_iter()
        .find(|entry| {
            entry.protocols.contains(&protocol)
                && !excluded(bad, &entry.version, &manifest.published_at)
        })
        .cloned();
    Pick {
        entry,
        update_uxduck,
    }
}

/// The copy built into UXDuck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltIn {
    pub version: String,
    /// Its build time, RFC 3339; empty when unknown, which is older than
    /// anything.
    pub published_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    /// A kept interface, by version.
    Kept(String),
    /// Nothing to serve: no built-in copy and none kept.
    Nothing,
}

/// What the store and this run know when choosing.
#[derive(Debug)]
pub struct Known<'a> {
    pub protocol: u32,
    /// The entry an accepted manifest named this run, and that manifest's
    /// `publishedAt`; none offline or before the first good check.
    pub followed: Option<(&'a InterfaceEntry, &'a str)>,
    pub current: Option<&'a str>,
    /// Newest first.
    pub kept: &'a [Kept],
    pub built_in: Option<&'a BuiltIn>,
    pub bad: &'a BTreeMap<String, Bad>,
}

/// The interface to serve.
#[must_use]
pub fn choose(known: &Known<'_>) -> Source {
    let usable = |kept: &&Kept| {
        kept.entry.protocols.contains(&known.protocol)
            && !known.bad.contains_key(&kept.entry.version)
    };
    if let Some((entry, manifest_published_at)) = known.followed {
        // The built-in copy is that entry, or newer than a manifest
        // published before it was built, which cannot be a rollback away
        // from it.
        if let Some(built_in) = known.built_in
            && (built_in.version == entry.version
                || (built_in.published_at.as_str() > entry.published_at.as_str()
                    && built_in.published_at.as_str() > manifest_published_at))
        {
            return Source::BuiltIn;
        }
        if known.kept.iter().any(|k| k.entry.version == entry.version) {
            return Source::Kept(entry.version.clone());
        }
        // Not installed (a failed download): as offline.
    }
    // Kept is newest first.
    let newest_kept = known.kept.iter().find(usable);
    let preferred = known
        .current
        .and_then(|current| {
            known
                .kept
                .iter()
                .filter(usable)
                .find(|k| k.entry.version == current)
        })
        .or(newest_kept);
    let built_in_newer = known.built_in.is_some_and(|built_in| {
        newest_kept.is_some_and(|newest| built_in.published_at > newest.entry.published_at)
    });
    match (preferred, known.built_in) {
        (Some(_), Some(_)) if built_in_newer => Source::BuiltIn,
        (Some(kept), _) => Source::Kept(kept.entry.version.clone()),
        (None, Some(_)) => Source::BuiltIn,
        (None, None) => Source::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uxduck_contracts::dto::interface::InterfaceArchive;

    use super::*;

    fn entry(version: &str, at: &str, protocols: &[u32]) -> InterfaceEntry {
        InterfaceEntry {
            version: version.into(),
            published_at: at.into(),
            protocols: protocols.to_vec(),
            archive: InterfaceArchive {
                url: format!("https://example.test/{version}.tar.zst"),
                size: 1,
                sha256: "00".into(),
            },
            files: BTreeMap::new(),
        }
    }

    fn kept(version: &str, at: &str) -> Kept {
        Kept {
            entry: entry(version, at, &[1]),
            dir: PathBuf::from(version),
        }
    }

    fn manifest(at: &str, entries: Vec<InterfaceEntry>) -> InterfaceManifest {
        InterfaceManifest {
            schema: 1,
            channel: "production".into(),
            published_at: at.into(),
            entries,
        }
    }

    fn bad(manifest_at: &str) -> Bad {
        Bad {
            reason: "it did not start".into(),
            at: manifest_at.into(),
            manifest_published_at: manifest_at.into(),
        }
    }

    #[test]
    fn an_entry_without_this_protocol_is_skipped_and_the_newest_compatible_taken() {
        let m = manifest(
            "2026-10-11T12:00:00Z",
            vec![
                entry("b", "2026-10-11T10:00:00Z", &[2, 1]),
                entry("c", "2026-10-11T12:00:00Z", &[3, 2]),
                entry("a", "2026-10-10T10:00:00Z", &[1]),
            ],
        );
        let none = BTreeMap::new();
        let one = pick(&m, 1, &none);
        assert_eq!(one.entry.unwrap().version, "b");
        assert!(one.update_uxduck, "the newest needs a newer UXDuck");
        let three = pick(&m, 3, &none);
        assert_eq!(three.entry.unwrap().version, "c");
        assert!(!three.update_uxduck);
        // An older UXDuck than any entry: nothing, and not told to update
        // when nothing newer exists for it either.
        assert_eq!(pick(&m, 0, &none).entry, None);
    }

    #[test]
    fn a_bad_entry_is_skipped_until_a_newer_manifest_names_it_again() {
        let m = manifest(
            "2026-10-11T12:00:00Z",
            vec![
                entry("new", "2026-10-11T12:00:00Z", &[1]),
                entry("old", "2026-10-10T12:00:00Z", &[1]),
            ],
        );
        let mut marked = BTreeMap::new();
        marked.insert("new".to_owned(), bad("2026-10-11T12:00:00Z"));
        assert_eq!(pick(&m, 1, &marked).entry.unwrap().version, "old");
        let later = manifest("2026-10-12T12:00:00Z", m.entries.clone());
        assert_eq!(pick(&later, 1, &marked).entry.unwrap().version, "new");
    }

    #[test]
    fn online_the_followed_entry_is_served_even_an_older_one_after_a_rollback() {
        let kept = [
            kept("new", "2026-10-11T12:00:00Z"),
            kept("old", "2026-10-10T12:00:00Z"),
        ];
        let old = entry("old", "2026-10-10T12:00:00Z", &[1]);
        let built_in = BuiltIn {
            version: "shipped".into(),
            published_at: "2026-10-09T00:00:00Z".into(),
        };
        let none = BTreeMap::new();
        let known = Known {
            protocol: 1,
            followed: Some((&old, "2026-10-12T00:00:00Z")),
            current: Some("new"),
            kept: &kept,
            built_in: Some(&built_in),
            bad: &none,
        };
        assert_eq!(choose(&known), Source::Kept("old".into()));
    }

    #[test]
    fn online_the_built_in_copy_is_served_when_it_is_the_entry_or_newer_than_the_manifest() {
        let same = entry("shipped", "2026-10-11T12:00:00Z", &[1]);
        let built_in = BuiltIn {
            version: "shipped".into(),
            published_at: "2026-10-11T12:00:00Z".into(),
        };
        let none = BTreeMap::new();
        let mut known = Known {
            protocol: 1,
            followed: Some((&same, "2026-10-11T12:00:00Z")),
            current: None,
            kept: &[],
            built_in: Some(&built_in),
            bad: &none,
        };
        assert_eq!(choose(&known), Source::BuiltIn, "no download for the same");
        let older = entry("older", "2026-10-10T12:00:00Z", &[1]);
        let kept = [kept("older", "2026-10-10T12:00:00Z")];
        known.kept = &kept;
        known.followed = Some((&older, "2026-10-10T12:00:00Z"));
        assert_eq!(
            choose(&known),
            Source::BuiltIn,
            "a manifest from before the build"
        );
        known.followed = Some((&older, "2026-10-12T00:00:00Z"));
        assert_eq!(
            choose(&known),
            Source::Kept("older".into()),
            "a rollback published after the build"
        );
    }

    #[test]
    fn offline_current_then_the_newest_kept_then_the_built_in_copy() {
        let kept = [
            kept("new", "2026-10-11T12:00:00Z"),
            kept("old", "2026-10-10T12:00:00Z"),
        ];
        let built_in = BuiltIn {
            version: "shipped".into(),
            published_at: "2026-10-01T00:00:00Z".into(),
        };
        let none = BTreeMap::new();
        let mut one_bad = BTreeMap::new();
        one_bad.insert("new".to_owned(), bad("2026-10-11T12:00:00Z"));
        let mut both_bad = one_bad.clone();
        both_bad.insert("old".to_owned(), bad("2026-10-11T12:00:00Z"));
        let known = |current: Option<&'static str>, bad: &'static str| {
            let marked = match bad {
                "one" => &one_bad,
                "both" => &both_bad,
                _ => &none,
            };
            choose(&Known {
                protocol: 1,
                followed: None,
                current,
                kept: &kept,
                built_in: Some(&built_in),
                bad: marked,
            })
        };
        assert_eq!(known(Some("old"), ""), Source::Kept("old".into()));
        assert_eq!(known(None, ""), Source::Kept("new".into()));
        assert_eq!(known(Some("gone"), ""), Source::Kept("new".into()));
        assert_eq!(
            known(Some("new"), "one"),
            Source::Kept("old".into()),
            "a bad one is never served"
        );
        assert_eq!(known(Some("new"), "both"), Source::BuiltIn);
        let nothing = Known {
            protocol: 1,
            followed: None,
            current: None,
            kept: &[],
            built_in: None,
            bad: &both_bad,
        };
        assert_eq!(choose(&nothing), Source::Nothing);
    }

    #[test]
    fn offline_the_built_in_copy_wins_when_it_is_newer_than_every_kept_one() {
        let kept = [kept("new", "2026-10-11T12:00:00Z")];
        let built_in = BuiltIn {
            version: "shipped".into(),
            published_at: "2026-10-12T00:00:00Z".into(),
        };
        let none = BTreeMap::new();
        let known = Known {
            protocol: 1,
            followed: None,
            current: Some("new"),
            kept: &kept,
            built_in: Some(&built_in),
            bad: &none,
        };
        assert_eq!(choose(&known), Source::BuiltIn);
    }

    #[test]
    fn a_kept_interface_for_another_protocol_is_not_served() {
        let mut other = kept("other", "2026-10-12T00:00:00Z");
        other.entry.protocols = vec![2];
        let kept = [other, kept("mine", "2026-10-10T00:00:00Z")];
        let none = BTreeMap::new();
        let known = Known {
            protocol: 1,
            followed: None,
            current: Some("other"),
            kept: &kept,
            built_in: None,
            bad: &none,
        };
        assert_eq!(choose(&known), Source::Kept("mine".into()));
    }
}

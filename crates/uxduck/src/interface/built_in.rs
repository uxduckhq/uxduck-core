//! The interface built into `uxduck` (`--features embed-ui`, release
//! builds), or the one `--ui-dir` names: the floor, served when nothing
//! downloaded is better.

use std::path::PathBuf;

use serde_json::Value;

use super::choose::BuiltIn;

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../apps/web/dist"]
#[exclude = "*.map"]
struct Embedded;

/// A file of the built-in interface.
#[derive(Debug)]
pub enum Found {
    #[cfg_attr(not(feature = "embed-ui"), allow(dead_code))]
    Bytes {
        body: Vec<u8>,
        mime: String,
    },
    Disk(PathBuf),
}

#[derive(Clone, Debug)]
pub enum Files {
    /// Compiled into this executable.
    Embedded,
    /// A built `apps/web` on disk (`--ui-dir`).
    Dir(PathBuf),
    None,
}

impl Files {
    /// `--ui-dir` when given, else the embedded copy when this build has
    /// one.
    #[must_use]
    pub fn new(ui_dir: Option<PathBuf>) -> Self {
        match ui_dir {
            Some(dir) => Self::Dir(dir),
            None if cfg!(feature = "embed-ui") => Self::Embedded,
            None => Self::None,
        }
    }

    /// A file by its path inside the build, such as `assets/index-Ab12.js`.
    #[must_use]
    pub fn find(&self, path: &str) -> Option<Found> {
        match self {
            Self::Embedded => embedded(path),
            Self::Dir(dir) => {
                let file = dir.join(path);
                file.is_file().then_some(Found::Disk(file))
            }
            Self::None => None,
        }
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        match self.find(path)? {
            Found::Bytes { body, .. } => Some(body),
            Found::Disk(file) => std::fs::read(file).ok(),
        }
    }

    /// Its version and build time, from its `version.json`; a build
    /// without one is "built-in", older than anything published.
    #[must_use]
    pub fn info(&self) -> Option<BuiltIn> {
        self.find("index.html")?;
        let parsed = self
            .read("version.json")
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        Some(parsed.as_ref().and_then(build_id).unwrap_or(BuiltIn {
            version: "built-in".into(),
            published_at: String::new(),
        }))
    }
}

#[cfg(feature = "embed-ui")]
fn embedded(path: &str) -> Option<Found> {
    Embedded::get(path).map(|file| Found::Bytes {
        body: file.data.into_owned(),
        mime: file.metadata.mimetype().to_owned(),
    })
}

#[cfg(not(feature = "embed-ui"))]
fn embedded(_: &str) -> Option<Found> {
    None
}

/// A build's id and time from its `version.json`: the `id` it carries, or
/// for a build from before it had one, the same made from its version,
/// commit and build time (`apps/web/scripts/version-file.ts`).
#[must_use]
pub fn build_id(json: &Value) -> Option<BuiltIn> {
    let built_at = json["builtAt"].as_str().unwrap_or_default().to_owned();
    if let Some(id) = json["id"].as_str() {
        return Some(BuiltIn {
            version: id.to_owned(),
            published_at: built_at,
        });
    }
    let version = json["version"].as_str()?;
    let compact: String = built_at
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches('Z')
        .chars()
        .filter(|c| !matches!(c, '-' | ':'))
        .collect();
    let mut id = format!("{version}+");
    if let Some(commit) = json["commit"].as_str() {
        id.push_str(&commit[..commit.len().min(8)]);
        id.push('.');
    }
    id.push_str(&compact);
    id.push('Z');
    Some(BuiltIn {
        version: id,
        published_at: built_at,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_build_id_is_its_own_or_made_as_the_web_build_makes_it() {
        let made = build_id(&json!({
            "version": "0.1.0",
            "commit": "53cf3ca7fef2927a681c5ecc7a8a0c12b2b1af94",
            "builtAt": "2026-10-11T18:43:36.945Z"
        }))
        .unwrap();
        assert_eq!(made.version, "0.1.0+53cf3ca7.20261011T184336Z");
        assert_eq!(made.published_at, "2026-10-11T18:43:36.945Z");
        let own = build_id(&json!({ "id": "x", "builtAt": "2026-10-11T00:00:00Z" })).unwrap();
        assert_eq!(own.version, "x");
    }
}

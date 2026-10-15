//! A plugin's files as one bundle, for the account (S59, ADR-0006 X13):
//! `plugin.json`, the module or script it names and its dialect file if it
//! has one, as JSON with the bytes in base64. The same files always make the same bytes, so its SHA-256
//! says whether the account already has it.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FORMAT: &str = "uxduck.plugin/1";

#[derive(Serialize, Deserialize)]
struct Bundle {
    format: String,
    /// File name to base64 bytes.
    files: BTreeMap<String, String>,
}

/// A file name beside `plugin.json`: no folders, nothing hidden.
fn plain(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\']) && name.len() <= 128
}

/// The hex SHA-256 of a bundle.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Bundles the plugin in `dir` whose module or script is `module`.
///
/// # Errors
/// When a file cannot be read or the module is not beside `plugin.json`.
pub fn pack(dir: &Path, module: &str) -> Result<Vec<u8>, String> {
    if !plain(module) {
        return Err(format!("{module}: not a file beside plugin.json"));
    }
    let mut files = BTreeMap::new();
    let manifest =
        std::fs::read(dir.join("plugin.json")).map_err(|e| format!("plugin.json: {e}"))?;
    // Its vendor's dialect travels with it (S60).
    let dialect = serde_json::from_slice::<serde_json::Value>(&manifest)
        .ok()
        .and_then(|m| m["dialect"].as_str().map(ToOwned::to_owned))
        .filter(|name| plain(name));
    files.insert("plugin.json".to_owned(), STANDARD.encode(&manifest));
    for name in std::iter::once(module).chain(dialect.as_deref()) {
        let bytes = std::fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
        files.insert(name.to_owned(), STANDARD.encode(bytes));
    }
    serde_json::to_vec(&Bundle {
        format: FORMAT.into(),
        files,
    })
    .map_err(|e| e.to_string())
}

/// Writes a bundle's files into `dir`, replacing what was there.
///
/// # Errors
/// When it is not a bundle, names a file outside `dir`, or cannot be
/// written.
pub fn unpack(bytes: &[u8], dir: &Path) -> Result<(), String> {
    let bundle: Bundle =
        serde_json::from_slice(bytes).map_err(|_| "not a UXDuck plugin bundle".to_owned())?;
    if bundle.format != FORMAT || !bundle.files.contains_key("plugin.json") {
        return Err("not a UXDuck plugin bundle".into());
    }
    if let Some(bad) = bundle.files.keys().find(|name| !plain(name)) {
        return Err(format!("{bad}: not a plain file name"));
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for (name, data) in bundle.files {
        let data = STANDARD
            .decode(data)
            .map_err(|_| format!("{name}: not base64"))?;
        std::fs::write(dir.join(&name), data).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_files_make_the_same_bundle_and_unpack_back() {
        let from = tempfile::tempdir().unwrap();
        std::fs::write(
            from.path().join("plugin.json"),
            r#"{"id":"x","dialect":"acme.xml"}"#,
        )
        .unwrap();
        std::fs::write(from.path().join("acme.xml"), "<mavlink/>").unwrap();
        std::fs::write(from.path().join("index.ts"), "export {}").unwrap();
        std::fs::write(from.path().join("notes.txt"), "left out").unwrap();
        let one = pack(from.path(), "index.ts").unwrap();
        assert_eq!(
            digest(&one),
            digest(&pack(from.path(), "index.ts").unwrap())
        );

        let to = tempfile::tempdir().unwrap();
        let dir = to.path().join("x");
        unpack(&one, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("index.ts")).unwrap(),
            "export {}"
        );
        assert!(!dir.join("notes.txt").exists());
        assert!(dir.join("acme.xml").exists(), "the dialect travels too");
    }

    #[test]
    fn a_bundle_cannot_write_outside_its_folder() {
        let files = BTreeMap::from([
            ("plugin.json".to_owned(), STANDARD.encode("{}")),
            ("../escape".to_owned(), STANDARD.encode("x")),
        ]);
        let bytes = serde_json::to_vec(&Bundle {
            format: FORMAT.into(),
            files,
        })
        .unwrap();
        let to = tempfile::tempdir().unwrap();
        assert!(unpack(&bytes, &to.path().join("x")).is_err());
        assert!(pack(to.path(), "../plugin.json").is_err());
    }
}

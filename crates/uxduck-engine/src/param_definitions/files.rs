//! The catalogue's files, checked the same way by the daemon and the
//! browser (S67, ADR-0021 P2, P4): a manifest signed with the release key,
//! and gzipped sets that must match its SHA-256. No I/O here; the host
//! fetches and keeps the bytes.

use std::io::Read as _;

use sha2::{Digest, Sha256};
use uxduck_contracts::dto::param_definitions::{
    CatalogueSet, DEFINITION_CATALOGUE_SCHEMA, DefinitionCatalogue,
};
use uxduck_core::domain::param_definitions::DefinitionSet;

/// Where the catalogue is published, beside the simulator catalogue.
pub const DEFAULT_URL: &str = "https://simulators.uxduck.com/parameters/catalogue.json";

/// Missing public trust configuration is separate from a publisher's
/// secret signing key. A person's own definitions do not need either.
pub const NO_CATALOGUE_KEY: &str = "this build has no trusted public verification key for the parameter catalogue; add descriptions from a matching firmware checkout or use a build with the publisher's public key";

/// A set is a few hundred kilobytes gzipped; anything far larger is not one.
pub const MAX_SET_BYTES: u64 = 16 * 1024 * 1024;

/// Whether `bytes` were signed with `public_key` (minisign, base64).
///
/// # Errors
/// A bad key or signature.
pub fn check_signature(bytes: &[u8], signature: &str, public_key: &str) -> Result<(), String> {
    let key = minisign_verify::PublicKey::from_base64(public_key.trim())
        .map_err(|e| format!("the public key: {e}"))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| format!("the catalogue's signature: {e}"))?;
    key.verify(bytes, &signature, false)
        .map_err(|e| format!("the catalogue's signature does not verify: {e}"))
}

/// Verifies and reads a manifest.
///
/// # Errors
/// A bad key, signature or document.
pub fn verify(bytes: &[u8], signature: &str, key: &str) -> Result<DefinitionCatalogue, String> {
    check_signature(bytes, signature, key)?;
    let catalogue: DefinitionCatalogue =
        serde_json::from_slice(bytes).map_err(|e| format!("the catalogue is not valid: {e}"))?;
    if catalogue.schema != DEFINITION_CATALOGUE_SCHEMA {
        return Err(format!(
            "catalogue schema {} is not one this build reads ({DEFINITION_CATALOGUE_SCHEMA})",
            catalogue.schema
        ));
    }
    Ok(catalogue)
}

/// The sets a host may fetch: a plain file name, not too large, over HTTPS
/// (or plain HTTP to this machine, for tests).
#[must_use]
pub fn usable(catalogue: DefinitionCatalogue) -> Vec<CatalogueSet> {
    catalogue
        .sets
        .into_iter()
        .filter(|s| {
            let name = &s.file.name;
            let plain = !name.is_empty()
                && !name.starts_with('.')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
            let url = &s.file.url;
            plain
                && s.file.size <= MAX_SET_BYTES
                && (url.starts_with("https://")
                    || url.starts_with("http://127.0.0.1")
                    || url.starts_with("http://localhost"))
        })
        .collect()
}

/// Lowercase hex SHA-256.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

/// A catalogue set's bytes, checked against its checksum and read.
///
/// # Errors
/// When they do not match, or are not a gzipped set.
pub fn read_set(set: &CatalogueSet, bytes: &[u8]) -> Result<DefinitionSet, String> {
    if sha256(bytes) != set.file.sha256 {
        return Err(format!(
            "{} does not match the catalogue's checksum",
            set.file.name
        ));
    }
    gunzip_set(bytes)
}

/// A gzipped set, as the catalogue serves it and the daemon keeps one.
///
/// # Errors
/// When it is not one.
pub fn gunzip_set(bytes: &[u8]) -> Result<DefinitionSet, String> {
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_SET_BYTES * 16)
        .read_to_end(&mut json)
        .map_err(|e| format!("not a gzipped set: {e}"))?;
    serde_json::from_slice(&json).map_err(|e| format!("not a definition set: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_contracts::dto::param_definitions::Firmware;
    use uxduck_contracts::dto::sim::CatalogueFile;

    fn entry(name: &str, url: &str) -> CatalogueSet {
        CatalogueSet {
            id: "arducopter-4.5.7".into(),
            firmware: Firmware::ArduCopter,
            version: "4.5.7".into(),
            commit: "2a3dc4b7".into(),
            file: CatalogueFile {
                name: name.into(),
                url: url.into(),
                size: 150_000,
                sha256: "0".repeat(64),
                encoding: None,
            },
            source_url: String::new(),
        }
    }

    #[test]
    fn only_plain_names_over_https_are_fetched() {
        let sets = usable(DefinitionCatalogue {
            schema: 1,
            published_at: String::new(),
            sets: vec![
                entry("a.json.gz", "https://x/a.json.gz"),
                entry("../a.json.gz", "https://x/a.json.gz"),
                entry("b.json.gz", "http://elsewhere/b.json.gz"),
                entry("c.json.gz", "http://127.0.0.1:9/c.json.gz"),
            ],
        });
        let names: Vec<&str> = sets.iter().map(|s| s.file.name.as_str()).collect();
        assert_eq!(names, ["a.json.gz", "c.json.gz"]);
    }

    #[test]
    fn a_set_must_match_its_checksum() {
        let set = entry("a.json.gz", "https://x/a.json.gz");
        assert!(
            read_set(&set, b"changed")
                .unwrap_err()
                .contains("does not match the catalogue's checksum")
        );
        assert_eq!(sha256(b"").len(), 64);
    }
}

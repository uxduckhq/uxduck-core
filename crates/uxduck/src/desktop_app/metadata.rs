//! Read the bundled app's own version, rather than an installer sidecar.
//!
//! Electron ASAR is two Chromium Pickles (size, then a JSON header), followed
//! by file data. See electron/asar's src/disk.ts. Only the packed package.json
//! is read here; no paths are extracted and no Electron process is started.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

const MAX_HEADER: u32 = 1024 * 1024;
const MAX_PACKAGE: u64 = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    version: String,
    main: String,
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// The version comes from the same package Electron loads on every platform.
pub(super) fn version(archive: &Path) -> Result<String, String> {
    read_version(archive).map_err(|error| {
        format!(
            "UXDuck's desktop installation is incomplete or damaged: {}: {error}. Reinstall UXDuck, or run `uxduck open --browser`.",
            archive.display()
        )
    })
}

fn read_version(archive: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(archive)?;
    let length = file.metadata()?.len();
    let mut prefix = [0_u8; 16];
    file.read_exact(&mut prefix)?;
    let word = |offset| {
        u32::from_le_bytes([
            prefix[offset],
            prefix[offset + 1],
            prefix[offset + 2],
            prefix[offset + 3],
        ])
    };
    let header_size = word(4);
    let json_size = word(12);
    if word(0) != 4
        || !(8..=MAX_HEADER).contains(&header_size)
        || word(8) != header_size - 4
        || json_size > header_size - 8
        || u64::from(header_size) + 8 > length
    {
        return Err(invalid("invalid ASAR header size"));
    }
    let mut json = vec![0; json_size as usize];
    file.read_exact(&mut json)?;
    let header: serde_json::Value = serde_json::from_slice(&json)?;
    let entry = &header["files"]["package.json"];
    if entry.get("link").is_some() || entry["unpacked"].as_bool() == Some(true) {
        return Err(invalid("package.json must be packed in app.asar"));
    }
    let offset = entry["offset"]
        .as_str()
        .and_then(|offset| offset.parse::<u64>().ok())
        .ok_or_else(|| invalid("invalid package.json offset"))?;
    let size = entry["size"]
        .as_u64()
        .filter(|size| (1..=MAX_PACKAGE).contains(size))
        .ok_or_else(|| invalid("invalid package.json size"))?;
    let start = (u64::from(header_size) + 8)
        .checked_add(offset)
        .ok_or_else(|| invalid("package.json offset overflow"))?;
    if start.checked_add(size).is_none_or(|end| end > length) {
        return Err(invalid("package.json extends beyond app.asar"));
    }
    file.seek(SeekFrom::Start(start))?;
    let mut package =
        vec![0; usize::try_from(size).map_err(|_| invalid("package.json too large"))?];
    file.read_exact(&mut package)?;
    let package: Package = serde_json::from_slice(&package)?;
    if package.name != "@uxduck/desktop" || package.main != "dist/main.js" {
        return Err(invalid("app.asar is not UXDuck's desktop app"));
    }
    semver::Version::parse(&package.version)
        .map_err(|_| invalid("invalid desktop package version"))?;
    Ok(package.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCHIVE: &[u8] = include_bytes!("../../tests/fixtures/desktop-app.asar");

    #[test]
    fn reads_a_real_electron_archive_without_starting_electron() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        std::fs::write(&path, ARCHIVE).unwrap();
        assert_eq!(version(&path).unwrap(), "0.2.0");
    }

    #[test]
    fn corrupt_truncated_and_unbounded_metadata_fail_with_the_path_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        let mut oversized = ARCHIVE.to_vec();
        oversized[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut wrong_app = ARCHIVE.to_vec();
        let name = wrong_app
            .windows(15)
            .position(|s| s == b"@uxduck/desktop")
            .unwrap();
        wrong_app[name] = b'x';
        for bytes in [vec![0; 20], ARCHIVE[..12].to_vec(), oversized, wrong_app] {
            std::fs::write(&path, bytes).unwrap();
            let error = version(&path).unwrap_err();
            assert!(error.contains(&path.display().to_string()), "{error}");
            assert!(error.contains("Reinstall UXDuck"), "{error}");
            assert!(error.contains("open --browser"), "{error}");
        }
    }
}

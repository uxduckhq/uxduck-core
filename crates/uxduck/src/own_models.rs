//! Vehicles' own 3D models (ADR-0035 M5), kept under the data folder as
//! `vehicle-models/<sha-256>`: written whole once the engine has checked
//! one, read back in pieces by the pages that draw it, and removed when no
//! vehicle is drawn with it.

use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

/// A model's file; the engine names it by its hash, checked here as well,
/// since it ends up in a path.
fn path(root: &Path, file: &str) -> Result<PathBuf, String> {
    if file.len() != 64 || !file.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("No model {file}."));
    }
    Ok(root.join(file))
}

pub fn keep(root: &Path, file: &str, bytes: &[u8]) -> Result<(), String> {
    let path = path(root, file)?;
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(root).map_err(|e| format!("Cannot keep the model: {e}"))?;
    // Whole or not at all: written beside it, then named.
    let partial = root.join(format!(".{file}"));
    fs::write(&partial, bytes)
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|e| format!("Cannot keep the model: {e}"))
}

pub fn read(root: &Path, file: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    let path = path(root, file)?;
    let mut opened = fs::File::open(&path).map_err(|_| {
        "This model is no longer kept on this computer. Choose its file again.".to_owned()
    })?;
    opened
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("Cannot read the model: {e}"))?;
    let mut out = Vec::new();
    opened
        .take(u64::try_from(len).unwrap_or(u64::MAX))
        .read_to_end(&mut out)
        .map_err(|e| format!("Cannot read the model: {e}"))?;
    Ok(out)
}

pub fn remove(root: &Path, file: &str) -> Result<(), String> {
    match fs::remove_file(path(root, file)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("Cannot remove the model: {e}"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    #[test]
    fn a_model_is_kept_once_read_in_pieces_and_removed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("vehicle-models");
        let file = "ab".repeat(32);
        super::keep(&root, &file, b"glTF model").unwrap();
        super::keep(&root, &file, b"glTF model").unwrap();
        assert_eq!(super::read(&root, &file, 0, 100).unwrap(), b"glTF model");
        assert_eq!(super::read(&root, &file, 5, 3).unwrap(), b"mod");
        assert!(super::read(&root, "../etc/passwd", 0, 1).is_err());
        super::remove(&root, &file).unwrap();
        super::remove(&root, &file).unwrap();
        assert!(
            super::read(&root, &file, 0, 1)
                .unwrap_err()
                .contains("Choose its file again")
        );
    }
}

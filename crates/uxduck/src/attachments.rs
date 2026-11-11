//! Files attached to chat messages (S246), kept under the data folder as
//! `attachments/<id>/<name>`: the bytes arrive in order into a hidden
//! `.upload` beside it, renamed to the file's own name once whole, so a
//! video keeps its extension for whatever plays it.

use std::fs::{self, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

const PARTIAL: &str = ".upload";

/// An attachment's folder; the engine makes the ids, but an id is checked
/// here as well, since it ends up in a path.
fn folder(root: &Path, id: &str) -> Result<PathBuf, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("No attached file {id}."));
    }
    Ok(root.join(id))
}

/// The whole file in an attachment's folder: the one not hidden.
fn whole(root: &Path, id: &str) -> Result<PathBuf, String> {
    let dir = folder(root, id)?;
    let gone = || format!("The attached file {id} is no longer kept. Attach it again.");
    fs::read_dir(&dir)
        .map_err(|_| gone())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .ok_or_else(gone)
}

pub fn write(root: &Path, id: &str, offset: u64, bytes: &[u8]) -> Result<(), String> {
    let dir = folder(root, id)?;
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot keep the file: {e}"))?;
    let path = dir.join(PARTIAL);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&path)
        .map_err(|e| format!("Cannot keep the file: {e}"))?;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.write_all(bytes))
        .map_err(|e| format!("Cannot keep the file: {e}"))
}

pub fn finish(root: &Path, id: &str, name: &str, keep: bool) -> Result<(), String> {
    let dir = folder(root, id)?;
    if !keep {
        return match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("Cannot remove the file: {e}"))
            }
            _ => Ok(()),
        };
    }
    // The engine cleaned the name; a folder in it is still refused here.
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("Cannot keep a file named {name}."));
    }
    fs::rename(dir.join(PARTIAL), dir.join(name)).map_err(|e| format!("Cannot keep the file: {e}"))
}

pub fn read(root: &Path, id: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    let path = whole(root, id)?;
    let mut file = fs::File::open(&path).map_err(|e| format!("Cannot read the file: {e}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("Cannot read the file: {e}"))?;
    let mut out = Vec::new();
    file.take(u64::try_from(len).unwrap_or(u64::MAX))
        .read_to_end(&mut out)
        .map_err(|e| format!("Cannot read the file: {e}"))?;
    Ok(out)
}

pub fn path(root: &Path, id: &str) -> Result<String, String> {
    whole(root, id).map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    #[test]
    fn a_file_arrives_in_pieces_and_keeps_its_name() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        super::write(root, "at1", 0, b"").unwrap();
        super::write(root, "at1", 0, b"hello ").unwrap();
        // Not whole yet: not readable.
        assert!(super::read(root, "at1", 0, 100).is_err());
        super::write(root, "at1", 6, b"world").unwrap();
        super::finish(root, "at1", "greeting.txt", true).unwrap();
        assert_eq!(super::read(root, "at1", 0, 100).unwrap(), b"hello world");
        assert_eq!(super::read(root, "at1", 6, 3).unwrap(), b"wor");
        assert!(super::path(root, "at1").unwrap().ends_with("greeting.txt"));

        super::write(root, "at2", 0, b"half").unwrap();
        super::finish(root, "at2", "x.tlog", false).unwrap();
        assert!(!root.join("at2").exists());
        assert!(super::read(root, "../at1", 0, 1).is_err());
        assert!(super::finish(root, "at3", "../evil", true).is_err());
    }
}

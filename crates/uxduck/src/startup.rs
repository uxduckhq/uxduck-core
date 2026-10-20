//! One private, bounded error handoff from a background daemon to its launcher.
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

pub const ERROR_FILE: &str = "UXDUCK_STARTUP_ERROR_FILE";
const LIMIT: usize = 8192;

/// Only a launcher-created file is written; normal CLI use needs no handoff.
pub fn record(message: &str) {
    let Some(path) = std::env::var_os(ERROR_FILE) else {
        return;
    };
    if let Err(error) = write(Path::new(&path), message) {
        eprintln!("uxduck: cannot save startup error: {error}");
    }
}

fn write(path: &Path, message: &str) -> std::io::Result<()> {
    // Scrub before bounding so truncation cannot leave part of a secret behind.
    let text = uxduck_engine::feedback::scrub_text(&message.replace("127.0.0.1:", "localhost:"));
    let mut start = text.len().saturating_sub(LIMIT);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(&text.as_bytes()[start..])
}

pub fn read(file: &mut File) -> std::io::Result<String> {
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(LIMIT as u64)))?;
    let mut bytes = Vec::new();
    file.take(LIMIT as u64).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    if text.trim().is_empty() {
        Ok("No startup diagnostic was written; run `uxduck daemon` to see why.".into())
    } else {
        Ok(text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test fixtures and assertions")]

    use super::*;

    #[test]
    fn handoff_is_bounded_redacted_utf8_and_preserves_local_port_errors() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let message = format!(
            "{} cannot listen on 127.0.0.1:45000 token=ghp_startupRegressionSecret1234 user@example.com",
            "ü".repeat(LIMIT)
        );
        write(file.path(), &message).unwrap();
        let text = read(file.as_file_mut()).unwrap();
        assert!(file.as_file().metadata().unwrap().len() <= LIMIT as u64);
        assert!(!text.contains('�'));
        assert!(!text.contains("ghp_startupRegressionSecret1234"));
        assert!(!text.contains("user@example.com"));
        assert!(text.contains("localhost:45000"), "{text}");
    }

    #[test]
    fn only_an_existing_handoff_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(write(&missing, "failure").is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn empty_handoff_explains_how_to_get_more_diagnostics() {
        let mut file = tempfile::tempfile().unwrap();
        assert!(read(&mut file).unwrap().contains("run `uxduck daemon`"));
    }
}

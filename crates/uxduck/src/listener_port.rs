//! Last successful loopback port, shared with the installer launcher so an
//! upgrade from a version without this cache can retain its browser origin.
use std::io;
use std::path::Path;

const FILE: &str = "listener-port";

pub fn read(directory: &Path) -> io::Result<Option<u16>> {
    let text = match std::fs::read_to_string(directory.join(FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    text.trim()
        .parse::<std::num::NonZeroU16>()
        .map(|port| Some(port.get()))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid listener-port cache. Remove that file to choose a new loopback port",
            )
        })
}

pub fn write(directory: &Path, port: u16) -> io::Result<()> {
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot remember port zero",
        ));
    }
    let temporary = directory.join("listener-port.tmp");
    std::fs::write(&temporary, format!("{port}\n"))?;
    std::fs::rename(temporary, directory.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_profiles_are_independent_and_a_successful_port_is_replaced_atomically() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        assert_eq!(read(one.path()).unwrap(), None);
        write(one.path(), 50123).unwrap();
        write(one.path(), 50124).unwrap();
        assert_eq!(read(one.path()).unwrap(), Some(50124));
        assert_eq!(read(two.path()).unwrap(), None);
        assert!(!one.path().join("listener-port.tmp").exists());
    }

    #[test]
    fn invalid_or_unreadable_cache_is_reported_and_never_means_port_zero() {
        let directory = tempfile::tempdir().unwrap();
        for value in ["0", "65536", "-1", "not a port"] {
            std::fs::write(directory.path().join(FILE), value).unwrap();
            assert_eq!(
                read(directory.path()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(write(directory.path(), 0).is_err());
        std::fs::remove_file(directory.path().join(FILE)).unwrap();
        std::fs::create_dir(directory.path().join(FILE)).unwrap();
        assert!(read(directory.path()).is_err());
    }
}

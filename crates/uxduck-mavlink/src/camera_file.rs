//! A camera's definition file read over MAVLink FTP (ADR-0063 Y5, S411):
//! the file a camera names with an `mftp://` or `mavlinkftp://` address in
//! `CAMERA_INFORMATION`, read from the component that keeps it. The
//! autopilot's own files go through the vehicle's one FTP client. Any other
//! component, such as the camera itself, has a client of its own here, so
//! a definition is read beside a parameter download and not in its way.
//!
//! The camera and its file are not trusted: the read stops at
//! [`MAX_BYTES`] and after [`MAX_US`], and what was read is only bytes
//! until the engine has checked it.

use std::collections::BTreeMap;

use crate::camera::Key;
use crate::files::{self, FtpClient};
use crate::transfers::{Failure, Timing};

/// The id of these reads in an FTP client, which never reach the engine as
/// a vehicle's files.
pub const READ_ID: &str = "uxduck:camera-definition";
/// The most a definition file may hold as it is sent, packed or not.
pub const MAX_BYTES: usize = 1 << 20;
/// The longest a read may take.
pub const MAX_US: u64 = 60_000_000;

/// How a MAVLink FTP address begins. The guide and the cameras that follow
/// it write `mftp://`. `common.xml` writes `mavlinkftp://` where it says what
/// `cam_definition_uri` may hold, and that a ground station must read it.
/// What follows either is read the same way.
const SCHEMES: [&str; 2] = ["mftp://", "mavlinkftp://"];

/// What follows the scheme of a MAVLink FTP address. None for any other
/// address.
fn after_scheme(uri: &str) -> Option<&str> {
    SCHEMES.iter().find_map(|scheme| {
        uri.get(..scheme.len())
            .filter(|start| start.eq_ignore_ascii_case(scheme))
            .and_then(|_| uri.get(scheme.len()..))
    })
}

/// Whether an address is a MAVLink FTP one, readable here or not.
#[must_use]
pub fn is_ftp(uri: &str) -> bool {
    after_scheme(uri).is_some()
}

/// The path on a component, and the component when the address names one,
/// of a MAVLink FTP address: `mftp://[comp=<id>:][@<alias>/]<path>` as the
/// guide writes it, or `mftp://[;comp=<id>]<path>` as older cameras do, and
/// the same after `mavlinkftp://`. None for any other address.
#[must_use]
pub fn ftp_address(uri: &str) -> Option<(Option<u8>, String)> {
    let rest = after_scheme(uri)?;
    let (component, path) = if let Some(rest) = rest.strip_prefix("[;comp=") {
        let (id, path) = rest.split_once(']')?;
        (Some(id.parse().ok()?), path)
    } else if let Some(rest) = rest.strip_prefix("comp=") {
        let (id, path) = rest.split_once(':')?;
        (Some(id.parse().ok()?), path)
    } else {
        (None, rest)
    };
    let path = path.trim();
    let plain = path.chars().all(|c| !c.is_control() && c != '\\')
        && !path.split('/').any(|part| part == "..");
    (!path.is_empty() && path.len() <= 200 && plain).then(|| (component, path.to_owned()))
}

#[derive(Clone, Debug)]
struct Read {
    /// None when the vehicle's own client reads it, from its autopilot.
    client: Option<FtpClient>,
    /// The camera's own component, which the file is for.
    camera: u8,
    bytes: Vec<u8>,
    started_us: u64,
}

/// What a read came to, for the session to act on.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Nothing yet.
    Going,
    /// Stop it: the file is too large or took too long.
    Stop(String),
    /// It ended: the file's bytes, or why there are none.
    Done(Result<Vec<u8>, String>),
}

/// The definition files being read, by the component each is read from.
#[derive(Clone, Debug, Default)]
pub struct Reads {
    by: BTreeMap<Key, Read>,
}

impl Reads {
    /// Whether any file is being read.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.by.is_empty()
    }

    /// Whether a file is being read from this component.
    #[must_use]
    pub fn reading(&self, key: &Key) -> bool {
        self.by.contains_key(key)
    }

    /// Whether a component's file is read by a client of its own here.
    #[must_use]
    pub fn own(&self, key: &Key) -> bool {
        self.by.get(key).is_some_and(|read| read.client.is_some())
    }

    /// The camera a component's file is read for, by its own component.
    #[must_use]
    pub fn camera(&self, key: &Key) -> Option<u8> {
        self.by.get(key).map(|read| read.camera)
    }

    /// Starts a read through the vehicle's own client, which the session
    /// asks itself.
    pub fn start_shared(&mut self, key: &Key, camera: u8, now_us: u64) {
        self.by.insert(
            key.clone(),
            Read {
                client: None,
                camera,
                bytes: Vec::new(),
                started_us: now_us,
            },
        );
    }

    /// Starts a read from a component with a client of its own.
    pub fn start(&mut self, key: &Key, camera: u8, path: &str, now_us: u64) -> files::Output {
        let mut client = FtpClient::new();
        let output = client.file(
            READ_ID,
            files::Op::Read {
                path: path.to_owned(),
                from: 0,
            },
            now_us,
        );
        self.by.insert(
            key.clone(),
            Read {
                client: Some(client),
                camera,
                bytes: Vec::new(),
                started_us: now_us,
            },
        );
        output
    }

    /// A component's answer, for its own client.
    pub fn answer(
        &mut self,
        key: &Key,
        answer: &crate::transfers::ftp::Answer,
        received_us: u64,
        now_us: u64,
    ) -> files::Output {
        self.by
            .get_mut(key)
            .and_then(|read| read.client.as_mut())
            .map(|client| client.answer(answer, received_us, now_us))
            .unwrap_or_default()
    }

    /// What an FTP client reported of one of these reads.
    pub fn event(&mut self, key: &Key, event: files::Event) -> Step {
        let Some(read) = self.by.get_mut(key) else {
            return Step::Going;
        };
        match event {
            files::Event::Opened { size, .. } if size > MAX_BYTES as u64 => Step::Stop(too_large()),
            files::Event::Data { bytes, .. } => {
                read.bytes.extend_from_slice(&bytes);
                if read.bytes.len() > MAX_BYTES {
                    Step::Stop(too_large())
                } else {
                    Step::Going
                }
            }
            files::Event::Done { result, .. } => {
                let read = self.by.remove(key);
                // The FTP client speaks of the vehicle. A file read from
                // the camera's own component went unanswered by the camera.
                let own = read.as_ref().is_some_and(|read| read.client.is_some());
                Step::Done(match result {
                    Ok(()) => Ok(read.map(|read| read.bytes).unwrap_or_default()),
                    Err(Failure::Timeout) => Err(NO_ANSWER.into()),
                    Err(failure) => {
                        let why = failure.to_string();
                        Err(if own && why.starts_with("the vehicle did not answer") {
                            NO_ANSWER.into()
                        } else {
                            sentence(&why)
                        })
                    }
                })
            }
            _ => Step::Going,
        }
    }

    /// Stops a read: what to send to close it, and that it is no longer
    /// read.
    pub fn stop(&mut self, key: &Key, now_us: u64) -> files::Output {
        self.by
            .remove(key)
            .and_then(|mut read| {
                read.client
                    .as_mut()
                    .map(|client| client.cancel(READ_ID, now_us))
            })
            .unwrap_or_default()
    }

    /// Each tick: what the own clients send again, and the reads that
    /// took too long.
    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> (Vec<(Key, files::Output)>, Vec<Key>) {
        let mut outputs = Vec::new();
        let mut late = Vec::new();
        for (key, read) in &mut self.by {
            if now_us.saturating_sub(read.started_us) > MAX_US {
                late.push(key.clone());
            } else if let Some(client) = &mut read.client {
                outputs.push((key.clone(), client.tick(now_us, timing)));
            }
        }
        (outputs, late)
    }

    /// Forgets the reads at the addresses that match.
    pub fn forget(&mut self, gone: impl Fn(&Key) -> bool) {
        self.by.retain(|key, _| !gone(key));
    }
}

/// Whether an FTP event is for one of these reads.
#[must_use]
pub fn ours(event: &files::Event) -> bool {
    match event {
        files::Event::Listed { id, .. }
        | files::Event::Opened { id, .. }
        | files::Event::Data { id, .. }
        | files::Event::Sent { id, .. }
        | files::Event::Crc { id, .. }
        | files::Event::Done { id, .. } => id == READ_ID,
        files::Event::Parameters(_) | files::Event::ParamsWritten { .. } => false,
    }
}

const NO_ANSWER: &str = "The camera did not answer.";

fn too_large() -> String {
    "The file is larger than 1 MB, the most UXDuck reads for a camera's definition.".into()
}

/// What took too long, in words.
#[must_use]
pub fn too_slow() -> String {
    "The file took longer than a minute to read.".into()
}

/// A reason as a sentence: a capital first, a full stop last.
fn sentence(why: &str) -> String {
    let mut chars = why.trim().trim_end_matches('.').chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => "The file could not be read.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ftp_address_names_a_path_and_sometimes_a_component() {
        let path = |uri| ftp_address(uri);
        assert_eq!(
            path("mftp:///camera.xml"),
            Some((None, "/camera.xml".into()))
        );
        assert_eq!(
            path("mftp://info/camera.xml.xz"),
            Some((None, "info/camera.xml.xz".into()))
        );
        assert_eq!(path("MFTP://camera.xml"), Some((None, "camera.xml".into())));
        assert_eq!(
            path("mftp://comp=100:/camera.xml"),
            Some((Some(100), "/camera.xml".into()))
        );
        assert_eq!(
            path("mftp://[;comp=100]camera.xml"),
            Some((Some(100), "camera.xml".into()))
        );
        assert_eq!(
            path("mftp://comp=12:@MAV_LOG/camera.xml"),
            Some((Some(12), "@MAV_LOG/camera.xml".into()))
        );
        // `common.xml`'s name for the scheme, read the same way after it.
        for (uri, component, file) in [
            ("mavlinkftp:///camera.xml", None, "/camera.xml"),
            (
                "mavlinkftp://info/camera.xml.xz",
                None,
                "info/camera.xml.xz",
            ),
            ("MavlinkFTP://camera.xml", None, "camera.xml"),
            (
                "mavlinkftp://comp=100:/camera.xml",
                Some(100),
                "/camera.xml",
            ),
            (
                "mavlinkftp://[;comp=100]camera.xml",
                Some(100),
                "camera.xml",
            ),
            (
                "mavlinkftp://comp=12:@MAV_LOG/camera.xml",
                Some(12),
                "@MAV_LOG/camera.xml",
            ),
        ] {
            assert_eq!(path(uri), Some((component, file.into())), "{uri}");
            assert!(is_ftp(uri), "{uri}");
        }
        assert!(is_ftp("mftp:///camera.xml") && is_ftp("MFTP://camera.xml"));
        for refused in [
            "http://192.168.144.25/camera.xml",
            "file:///etc/passwd",
            "mftp://",
            "mftp://comp=999:/camera.xml",
            "mftp://comp=x:/camera.xml",
            "mftp://../../etc/passwd",
            "mftp://a/../b.xml",
            "mftp://a\nb.xml",
            "ftp://camera.xml",
            "",
            "mavlinkftp://",
            "mavlinkftp://comp=999:/camera.xml",
            "mavlinkftp://comp=x:/camera.xml",
            "mavlinkftp://../../etc/passwd",
            "mavlinkftp://a/../b.xml",
            "mavlinkftp://a\nb.xml",
            "mavlinkftp:/camera.xml",
            "mavlinkftp:camera.xml",
            "mavlinkftps://camera.xml",
            "mavlink://camera.xml",
            "mavftp://camera.xml",
            "xmftp://camera.xml",
            "mftp",
            "mavlinkftp:/",
            "mftp:/\u{e9}camera.xml",
        ] {
            assert_eq!(path(refused), None, "{refused}");
        }
        // An address of the kind that cannot be read is still of the kind,
        // so it is said to be unreadable and not some other address.
        assert!(is_ftp("mavlinkftp://../../etc/passwd") && is_ftp("mftp://"));
        for other in [
            "http://a/camera.xml",
            "ftp://camera.xml",
            "mavlinkftp:/x",
            "",
        ] {
            assert!(!is_ftp(other), "{other}");
        }
    }

    #[test]
    fn a_read_stops_when_the_file_is_too_large() {
        let key: Key = ("link".into(), "peer".into(), 1, 100);
        let mut reads = Reads::default();
        let started = reads.start(&key, 100, "/camera.xml", 0);
        assert_eq!(started.send.len(), 1);
        assert!(reads.own(&key));
        assert_eq!(reads.camera(&key), Some(100));
        let opened = files::Event::Opened {
            id: READ_ID.into(),
            size: MAX_BYTES as u64 + 1,
        };
        assert!(matches!(reads.event(&key, opened), Step::Stop(_)));
        let data = |len| files::Event::Data {
            id: READ_ID.into(),
            offset: 0,
            bytes: vec![0; len],
        };
        assert_eq!(reads.event(&key, data(MAX_BYTES)), Step::Going);
        assert!(matches!(reads.event(&key, data(1)), Step::Stop(_)));
        reads.stop(&key, 1);
        assert!(!reads.reading(&key));
    }

    #[test]
    fn a_read_ends_with_its_bytes_or_why_not() {
        let key: Key = ("link".into(), "peer".into(), 1, 100);
        let mut reads = Reads::default();
        reads.start(&key, 100, "/camera.xml", 0);
        reads.event(
            &key,
            files::Event::Data {
                id: READ_ID.into(),
                offset: 0,
                bytes: b"<mavlinkcamera/>".to_vec(),
            },
        );
        let done = |result| files::Event::Done {
            id: READ_ID.into(),
            result,
        };
        assert_eq!(
            reads.event(&key, done(Ok(()))),
            Step::Done(Ok(b"<mavlinkcamera/>".to_vec()))
        );
        reads.start(&key, 100, "/camera.xml", 0);
        assert_eq!(
            reads.event(&key, done(Err(Failure::Timeout))),
            Step::Done(Err("The camera did not answer.".into()))
        );
        reads.start(&key, 100, "/camera.xml", 0);
        assert_eq!(
            reads.event(
                &key,
                done(Err(Failure::Rejected(
                    "there is no such file or folder".into()
                )))
            ),
            Step::Done(Err("There is no such file or folder.".into()))
        );
    }

    #[test]
    fn a_read_that_takes_too_long_is_told() {
        let key: Key = ("link".into(), "peer".into(), 1, 100);
        let mut reads = Reads::default();
        reads.start_shared(&key, 100, 0);
        assert!(!reads.own(&key));
        let (_, late) = reads.tick(MAX_US, &Timing::default());
        assert!(late.is_empty());
        let (_, late) = reads.tick(MAX_US + 1, &Timing::default());
        assert_eq!(late, vec![key]);
    }
}

//! Where a PX4 vehicle keeps its own parameter descriptions (ADR-0021 P12).
//! Asked by `MAV_CMD_REQUEST_MESSAGE`, `COMPONENT_METADATA` names its
//! general metadata, a file on the vehicle that names the others, its
//! parameters' `parameters.json` among them. Both are read over MAVLink FTP
//! and are usually xz-compressed.

use serde_json::Value;

use crate::files;

/// `COMPONENT_METADATA`.
pub const COMPONENT_METADATA: u32 = 397;
/// `COMP_METADATA_TYPE_PARAMETER`.
const PARAMETER: u64 = 1;
/// The id of the session's own FTP reads, which never reach the engine as
/// files.
pub const READ_ID: &str = "uxduck:px4-metadata";
/// The most a file may hold, read or unpacked: PX4 1.17's parameters are
/// 68 KB compressed and 717 KB unpacked.
const MAX_BYTES: usize = 16 * 1024 * 1024;
const ASK_EVERY_US: u64 = 5_000_000;
const ASKS: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Locating,
    General,
    Parameters,
    Done,
}

/// One vehicle's metadata, from asking where it is to its parameters'.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    stage: Stage,
    asked: u8,
    asked_us: u64,
    bytes: Vec<u8>,
}

/// What the session does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Ask for `COMPONENT_METADATA`.
    Ask,
    /// Read this file over FTP, as [`READ_ID`].
    Read(String),
    /// The parameters' metadata as JSON text, or why there is none.
    Said(Result<String, String>),
    Nothing,
}

impl Metadata {
    /// Each tick: ask where the metadata is, again while it is unanswered,
    /// and give up after a few asks.
    pub fn tick(&mut self, now_us: u64) -> Next {
        if self.stage != Stage::Locating
            || (self.asked > 0 && now_us < self.asked_us.saturating_add(ASK_EVERY_US))
        {
            return Next::Nothing;
        }
        if self.asked >= ASKS {
            self.stage = Stage::Done;
            return Next::Said(Err("it didn't say where they are".into()));
        }
        self.asked += 1;
        self.asked_us = now_us;
        Next::Ask
    }

    /// `COMPONENT_METADATA` said where the general metadata is.
    pub fn located(&mut self, uri: &str) -> Next {
        if self.stage != Stage::Locating {
            return Next::Nothing;
        }
        self.read(uri, Stage::General)
    }

    fn read(&mut self, uri: &str, stage: Stage) -> Next {
        self.bytes.clear();
        let Some(path) = ftp_path(uri) else {
            self.stage = Stage::Done;
            return Next::Said(Err(format!(
                "it keeps them at {uri}, which UXDuck can't read from the vehicle"
            )));
        };
        self.stage = stage;
        Next::Read(path)
    }

    /// The next bytes of the file being read. False once it is too large
    /// to be metadata, and the read is to be given up.
    pub fn data(&mut self, bytes: &[u8]) -> bool {
        self.bytes.extend_from_slice(bytes);
        self.bytes.len() <= MAX_BYTES
    }

    /// The file being read ended.
    pub fn done(&mut self, result: Result<(), String>) -> Next {
        let stage = self.stage;
        if !matches!(stage, Stage::General | Stage::Parameters) {
            return Next::Nothing;
        }
        let bytes = std::mem::take(&mut self.bytes);
        self.stage = Stage::Done;
        let text = match result.and_then(|()| unpack(&bytes)) {
            Ok(text) => text,
            Err(why) => return Next::Said(Err(why)),
        };
        if stage == Stage::Parameters {
            return Next::Said(Ok(text));
        }
        match parameters_uri(&text) {
            Ok(uri) => self.read(&uri, Stage::Parameters),
            Err(why) => Next::Said(Err(why)),
        }
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

/// The path on the vehicle of an `mftp://` URI, such as
/// `mftp://[;comp=1]etc/extras/parameters.json.xz`, from the root of its
/// files. None for any other URI.
fn ftp_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("mftp://")?;
    let rest = match rest.strip_prefix("[;comp=") {
        Some(rest) => rest.split_once(']')?.1,
        None => rest,
    };
    let path = rest.trim_start_matches('/');
    (!path.is_empty()).then(|| format!("/{path}"))
}

/// A file as text, unpacked when it is xz-compressed.
fn unpack(bytes: &[u8]) -> Result<String, String> {
    let text = if bytes.starts_with(b"\xFD7zXZ\0") {
        let mut out = Limited(Vec::new());
        lzma_rs::xz_decompress(&mut &bytes[..], &mut out)
            .map_err(|e| format!("they couldn't be unpacked: {e}"))?;
        out.0
    } else {
        bytes.to_vec()
    };
    String::from_utf8(text).map_err(|_| "they aren't text".to_owned())
}

/// Unpacked bytes, refused past [`MAX_BYTES`].
struct Limited(Vec<u8>);

impl std::io::Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + bytes.len() > MAX_BYTES {
            return Err(std::io::Error::other("they are too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Where the general metadata says the parameters' metadata is.
fn parameters_uri(general: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(general).map_err(|_| "its list of them isn't JSON".to_owned())?;
    value
        .get("metadataTypes")
        .and_then(Value::as_array)
        .and_then(|types| {
            types
                .iter()
                .find(|t| t.get("type").and_then(Value::as_u64) == Some(PARAMETER))
        })
        .and_then(|t| t.get("uri")?.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "its list of them doesn't name its parameters'".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xz(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut text.as_bytes(), &mut out).unwrap();
        out
    }

    const GENERAL: &str = r#"{"metadataTypes": [{"fileCrc": 1, "type": 4, "uri": "mftp://etc/extras/all_events.json.xz"}, {"fileCrc": 2, "type": 1, "uri": "mftp://[;comp=1]etc/extras/parameters.json.xz", "uriFallback": "https://px4-travis.s3.amazonaws.com/Firmware/main/px4_sitl_default/parameters.json.xz"}], "version": 1}"#;

    #[test]
    fn the_general_metadata_leads_to_the_parameters_by_ftp() {
        let mut metadata = Metadata::default();
        assert_eq!(metadata.tick(1), Next::Ask);
        assert_eq!(metadata.tick(2), Next::Nothing, "waits for an answer");
        assert_eq!(
            metadata.located("mftp://etc/extras/component_general.json.xz"),
            Next::Read("/etc/extras/component_general.json.xz".into())
        );
        assert_eq!(metadata.tick(10_000_000), Next::Nothing, "answered");
        let general = xz(GENERAL);
        let (first, rest) = general.split_at(10);
        assert!(metadata.data(first) && metadata.data(rest));
        assert_eq!(
            metadata.done(Ok(())),
            Next::Read("/etc/extras/parameters.json.xz".into())
        );
        assert!(metadata.data(br#"{"parameters": []}"#), "plain JSON too");
        assert_eq!(
            metadata.done(Ok(())),
            Next::Said(Ok(r#"{"parameters": []}"#.into()))
        );
        assert_eq!(metadata.located("mftp://again"), Next::Nothing, "read once");
    }

    #[test]
    fn unanswered_unreadable_or_elsewhere_says_why() {
        let mut metadata = Metadata::default();
        let mut asks = 0;
        for second in 0..20 {
            match metadata.tick(second * 1_000_000) {
                Next::Ask => asks += 1,
                Next::Said(Err(why)) => {
                    assert_eq!(why, "it didn't say where they are");
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(asks, 3);

        let mut metadata = Metadata::default();
        assert!(matches!(
            metadata.located("https://example.com/general.json"),
            Next::Said(Err(why)) if why.contains("https://example.com/general.json")
        ));

        let mut metadata = Metadata::default();
        metadata.located("mftp://etc/extras/component_general.json.xz");
        assert_eq!(
            metadata.done(Err("the vehicle did not answer".into())),
            Next::Said(Err("the vehicle did not answer".into()))
        );

        let mut metadata = Metadata::default();
        metadata.located("mftp://etc/extras/component_general.json.xz");
        let mut damaged = xz(GENERAL);
        let middle = damaged.len() / 2;
        damaged[middle] ^= 0xFF;
        metadata.data(&damaged);
        assert!(matches!(metadata.done(Ok(())), Next::Said(Err(_))));

        let mut metadata = Metadata::default();
        metadata.located("mftp://etc/extras/component_general.json.xz");
        metadata.data(br#"{"metadataTypes": [{"type": 4, "uri": "mftp://x"}]}"#);
        assert_eq!(
            metadata.done(Ok(())),
            Next::Said(Err("its list of them doesn't name its parameters'".into()))
        );
    }

    #[test]
    fn an_mftp_path_is_from_the_vehicles_root() {
        assert_eq!(ftp_path("mftp://etc/x.json"), Some("/etc/x.json".into()));
        assert_eq!(ftp_path("mftp:///etc/x.json"), Some("/etc/x.json".into()));
        assert_eq!(
            ftp_path("mftp://[;comp=1]etc/x.json"),
            Some("/etc/x.json".into())
        );
        assert_eq!(ftp_path("mftp://"), None);
        assert_eq!(ftp_path("https://example.com/x.json"), None);
    }
}

//! Files attached to chat messages (S246): uploaded in order over the API,
//! kept by the host under their id, then read by the agent or handed to a
//! replay or a flight's videos by that id. Uploads in progress live here;
//! a sent file is found again through the message that carries it.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::Value;
use uxduck_contracts::dto::thread::{
    ATTACHMENT_CHUNK, ATTACHMENT_MAX, AttachedFile, AttachmentAppendParams, AttachmentAppendResult,
    AttachmentBeginParams, AttachmentBeginResult, AttachmentFinishParams, AttachmentImageParams,
    AttachmentImageResult, AttachmentReadParams, AttachmentReadResult, PICTURE_LONG_EDGE,
};
use uxduck_contracts::dto::video::LOOK_REPLY_BYTES;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::thread::{self, Attachment, ItemKind, ItemStatus};

use crate::rpc::handlers::to_value;
use crate::runtime::{Picture, PictureOrder};
use crate::{ConnectionId, Engine};

mod json_rpc {
    use serde::Serialize;
    use serde_json::Value;
    use uxduck_contracts::dto::thread::AttachmentImageResult;

    #[derive(Serialize)]
    pub(super) struct Reply<'a> {
        pub(super) jsonrpc: &'static str,
        pub(super) id: &'a Value,
        pub(super) result: &'a AttachmentImageResult,
    }
}

/// The most `attachment/read` returns at once.
const READ_MAX: usize = 64 * 1024;

/// Uploads begun and not yet sent with a message, and pictures being made
/// for the AI model (FB-248).
#[derive(Debug, Default)]
pub(crate) struct Uploads {
    open: BTreeMap<String, Upload>,
    pictures: BTreeMap<String, Picturing>,
}

#[derive(Debug)]
struct Upload {
    connection: ConnectionId,
    name: String,
    bytes: u64,
    received: u64,
    done: bool,
    map: Option<thread::MapFrame>,
}

/// An `attachment/image` waiting for the host to make its picture.
#[derive(Debug)]
struct Picturing {
    connection: ConnectionId,
    request_id: Value,
    id: String,
    found: Found,
    /// The format the file's bytes say it is, in words.
    format: &'static str,
    /// Why the picture was given less room than asked for, for the note.
    held: Option<String>,
}

/// A file as the agent and the features that take one know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub name: String,
    pub bytes: u64,
    /// Where on the ground a picture of UXDuck's map shows (FB-248).
    pub map: Option<thread::MapFrame>,
}

/// What a picture's first bytes say it is: PNG, JPEG or WebP, the
/// pictures UXDuck shows the AI model (FB-248).
fn picture_format(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("PNG")
    } else if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("JPEG")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("WebP")
    } else {
        None
    }
}

/// What the AI model is told of a picture that is not of UXDuck's map,
/// before it takes positions from it (FB-248).
pub const PLACE_IT_FIRST: &str = "This picture carries no map coordinates from UXDuck. Before \
    proposing waypoints or a plan from an outline in it, ask the person to place or confirm the \
    outline on the map, such as by drawing it as an area marker or by moving the corners of an \
    area marker you add as your guess, then use the marker's corners. Never guess its position \
    or scale from the picture alone.";

/// A file's name with no folders and nothing a file system minds.
fn file_name(name: &str) -> Result<String, ApiError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let clean = clean.trim().trim_start_matches('.').trim().to_owned();
    if clean.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the file has no name",
        ));
    }
    if clean.len() > thread::MAX_FILE_NAME {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("a file's name is at most {} bytes", thread::MAX_FILE_NAME),
        ));
    }
    Ok(clean)
}

/// Not text: a log, a video, a picture or an archive, by its name or a NUL.
fn binary(name: &str, head: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    let by_name = [
        ".tlog", ".bin", ".mp4", ".mov", ".m4v", ".ts", ".mkv", ".avi", ".jpg", ".jpeg", ".png",
        ".gif", ".webp", ".zip", ".gz", ".zst", ".apj", ".px4", ".pdf",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    by_name || head.iter().take(8192).any(|b| *b == 0)
}

fn gone(id: &str) -> ApiError {
    ApiError::new(
        ErrorKind::NotFound,
        format!("No attached file {id}. Ask the operator to attach it to a message again."),
    )
}

impl Engine {
    /// A file attached to a message this connection may see, or one it
    /// uploaded and has not sent yet; with no connection, any, once its
    /// caller has checked.
    pub(crate) fn attached_file(
        &self,
        connection: Option<ConnectionId>,
        id: &str,
    ) -> Result<Found, ApiError> {
        if let Some(upload) = self.uploads.open.get(id) {
            return if upload.done && connection.is_none_or(|c| c == upload.connection) {
                Ok(Found {
                    name: upload.name.clone(),
                    bytes: upload.bytes,
                    map: upload.map.clone(),
                })
            } else {
                Err(gone(id))
            };
        }
        self.state
            .items
            .values()
            .filter(|item| connection.is_none_or(|c| self.sees_files_of(c, &item.thread_id)))
            .flat_map(|item| item.attachments.iter())
            .find_map(|a| {
                let file = a.file.as_ref().filter(|f| f.id == id)?;
                Some(Found {
                    name: a.title.clone(),
                    bytes: file.bytes,
                    map: file.map.clone(),
                })
            })
            .ok_or_else(|| gone(id))
    }

    /// `replay/open` for a connection: a file attached to a chat it can
    /// see, or one it uploaded, and nobody else's.
    pub(crate) fn open_replay_for(
        &mut self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::replay::ReplayOpenParams,
    ) -> Result<(), ApiError> {
        if let Some(id) = &params.attachment {
            self.attached_file(Some(connection), id)?;
        }
        // A flight's own file (ADR-0062): its flight, in the Logbook by
        // reference, replayed as one.
        if let Some(path) = params.path.as_deref().filter(|p| {
            std::path::Path::new(p)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("uxflight"))
        }) {
            let flight_id = self.adopt_flight_file(path)?;
            return self.open_replay(&uxduck_contracts::dto::replay::ReplayOpenParams {
                flight_id: Some(flight_id),
                path: None,
                ..params.clone()
            });
        }
        self.open_replay(params)
    }

    /// Whether a connection may use the files sent in a chat: one that can
    /// see the chat, or UXDuck's own agent host, which answers every
    /// operator's chats from one connection inside UXDuck.
    fn sees_files_of(&self, connection: ConnectionId, thread_id: &str) -> bool {
        let inside = self
            .connections
            .get(&connection)
            .is_some_and(|c| c.transport == uxduck_contracts::dto::trace::Transport::Internal);
        inside || crate::rpc::threads::sees_thread(self, connection, thread_id)
    }

    /// Checks a message's files before it is sent: each uploaded whole by
    /// this connection, as big as it says, or sent before in this chat with
    /// a message taken back or the one this `replaces`. They are the
    /// message's from now.
    pub(crate) fn take_attached_files(
        &mut self,
        connection: ConnectionId,
        thread_id: &str,
        replaces: Option<&str>,
        attachments: &mut [Attachment],
    ) -> Result<(), ApiError> {
        let sent_before: Vec<Attachment> = self
            .state
            .items_of(thread_id)
            .into_iter()
            .filter(|i| {
                i.kind == ItemKind::UserMessage
                    && (i.status == ItemStatus::Superseded || replaces == Some(i.id.as_str()))
            })
            .flat_map(|i| i.attachments.iter().cloned())
            .collect();
        for attachment in attachments.iter_mut() {
            let Some(file) = &attachment.file else {
                continue;
            };
            // Sent before with a message taken back: it goes again as it was.
            if let Some(sent) = sent_before
                .iter()
                .find(|a| a.file.as_ref().is_some_and(|f| f.id == file.id))
            {
                attachment.clone_from(sent);
                continue;
            }
            let upload = self
                .uploads
                .open
                .get(&file.id)
                .filter(|u| u.connection == connection && u.done)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::InvalidValue,
                        format!("{} has not finished uploading", attachment.title),
                    )
                })?;
            // The name and size are UXDuck's, whatever the message says.
            attachment.title.clone_from(&upload.name);
            attachment.file = Some(thread::AttachedFile {
                id: file.id.clone(),
                bytes: upload.bytes,
                map: upload.map.clone(),
            });
        }
        for attachment in attachments.iter() {
            if let Some(file) = &attachment.file {
                self.uploads.open.remove(&file.id);
            }
        }
        Ok(())
    }

    /// An upload used for something other than a message, such as a
    /// vehicle's own model, once that kept its own copy: it goes, as one
    /// taken out of the input does. A file sent with a message stays.
    pub(crate) fn used_upload(&mut self, connection: ConnectionId, id: &str) {
        if self
            .uploads
            .open
            .get(id)
            .is_none_or(|u| u.connection != connection)
        {
            return;
        }
        if let Some(upload) = self.uploads.open.remove(id) {
            let _ = self.system.attachment_finish(id, &upload.name, false);
        }
    }

    /// A connection went: what it was still uploading goes too.
    pub(crate) fn attachments_disconnected(&mut self, connection: ConnectionId) {
        let unsent: Vec<(String, String)> = self
            .uploads
            .open
            .iter()
            .filter(|(_, u)| u.connection == connection)
            .map(|(id, u)| (id.clone(), u.name.clone()))
            .collect();
        for (id, name) in unsent {
            self.uploads.open.remove(&id);
            let _ = self.system.attachment_finish(&id, &name, false);
        }
        self.uploads
            .pictures
            .retain(|_, p| p.connection != connection);
    }

    /// The host's picture for an `attachment/image` (FB-248), answered to
    /// the request that asked, serialised once from its base64. The trace
    /// and MCP's analytics see it with the JPEG left out.
    pub fn attachment_pictured(&mut self, key: &str, made: Result<Picture, String>) {
        let Some(waiting) = self.uploads.pictures.remove(key) else {
            return;
        };
        let picture = match made {
            Ok(picture) => picture,
            Err(why) => {
                let frame = crate::rpc::router::error(
                    &waiting.request_id,
                    &ApiError::new(ErrorKind::InvalidValue, why),
                );
                self.send(waiting.connection, &frame);
                return;
            }
        };
        let scaled =
            (picture.source_width, picture.source_height) != (picture.width, picture.height);
        let misshapen = waiting.found.map.as_ref().filter(|map| {
            let framed = f64::from(map.width) * f64::from(picture.source_height);
            let seen = f64::from(map.height) * f64::from(picture.source_width);
            (framed - seen).abs() > framed.max(seen) / 100.0
        });
        let map = waiting
            .found
            .map
            .as_ref()
            .filter(|_| misshapen.is_none())
            .map(|map| map.describe(picture.width, picture.height));
        let note = [
            (waiting.format != "JPEG").then(|| format!("Made a JPEG of the {}.", waiting.format)),
            scaled.then(|| {
                format!(
                    "Scaled from the picture's {} × {} to {} × {}.",
                    picture.source_width, picture.source_height, picture.width, picture.height
                )
            }),
            waiting.held,
            misshapen.map(|map| {
                format!(
                    "Its map coordinates were left out. They are for a {} × {} picture, and this \
                     one is {} × {}, another shape, so they don't say where it is.",
                    map.width, map.height, picture.source_width, picture.source_height
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let result = AttachmentImageResult {
            id: waiting.id,
            name: waiting.found.name,
            bytes: waiting.found.bytes,
            source_width: picture.source_width,
            source_height: picture.source_height,
            width: picture.width,
            height: picture.height,
            placement: map.is_none().then(|| PLACE_IT_FIRST.to_owned()),
            map,
            note: (!note.is_empty()).then(|| note.join(" ")),
            jpeg_base64: picture.jpeg_base64,
        };
        let text = serde_json::to_string(&json_rpc::Reply {
            jsonrpc: "2.0",
            id: &waiting.request_id,
            result: &result,
        })
        .unwrap_or_default();
        let mut traced = serde_json::json!({
            "jsonrpc": "2.0",
            "id": waiting.request_id,
            "result": to_value(&AttachmentImageResult { jpeg_base64: String::new(), ..result.clone() }).unwrap_or_default(),
        });
        traced["result"]["jpegBase64"] = Value::String(format!(
            "({} characters of JPEG left out)",
            result.jpeg_base64.len()
        ));
        self.send_text(waiting.connection, text, &traced);
    }

    /// Lets go of an upload not sent with a message, and the file it kept:
    /// a log once it is imported (S457).
    pub(crate) fn forget_upload(&mut self, id: &str) {
        if let Some(upload) = self.uploads.open.remove(id) {
            let _ = self.system.attachment_finish(id, &upload.name, false);
        }
    }

    /// The whole of an attached file, for a replay.
    pub(crate) fn attached_bytes(
        &mut self,
        connection: Option<ConnectionId>,
        id: &str,
    ) -> Result<(Found, Vec<u8>), ApiError> {
        let found = self.attached_file(connection, id)?;
        let len = usize::try_from(found.bytes).unwrap_or(usize::MAX);
        let bytes = self
            .system
            .attachment_read(id, 0, len)
            .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
        Ok((found, bytes))
    }

    /// Where an attached file is on this computer, for a flight's video.
    pub(crate) fn attached_path(
        &self,
        connection: ConnectionId,
        id: &str,
    ) -> Result<(Found, String), ApiError> {
        let found = self.attached_file(Some(connection), id)?;
        let path = self
            .system
            .attachment_path(id)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        Ok((found, path))
    }
}

/// `attachment/*`: uploading needs `control`, as sending a message does;
/// reading one needs `read`.
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let needs = if matches!(
        request,
        ClientRequest::AttachmentRead(_) | ClientRequest::AttachmentImage(_)
    ) {
        Capability::Read
    } else {
        Capability::Control
    };
    if !granted.contains(&needs) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            needs.denied_message(),
        ));
    }
    match request {
        ClientRequest::AttachmentBegin(params) => begin(engine, connection, &params),
        ClientRequest::AttachmentAppend(params) => append(engine, connection, &params),
        ClientRequest::AttachmentFinish(params) => finish(engine, connection, params),
        ClientRequest::AttachmentRead(params) => read(engine, connection, &params),
        ClientRequest::AttachmentImage(params) => image(engine, connection, &params),
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not an attachment method",
        )),
    }
}

fn begin(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &AttachmentBeginParams,
) -> Result<Value, ApiError> {
    let name = file_name(&params.name)?;
    let map = params.map.as_ref().map(thread::MapFrame::from);
    if let Some(map) = &map {
        map.check()
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    }
    if params.bytes > ATTACHMENT_MAX {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A file attached to a message is at most 8 GB.",
        ));
    }
    // However many files are open at once (maintainer, 2026-11-15): one costs
    // its bytes on disk, and a full disk is said in the computer's own words.
    let id = format!("at{}", &engine.random.token()[..16]);
    // An empty file is whole as it begins.
    engine
        .system
        .attachment_write(&id, &name, 0, &[])
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    engine.uploads.open.insert(
        id.clone(),
        Upload {
            connection,
            name,
            bytes: params.bytes,
            received: 0,
            done: false,
            map,
        },
    );
    to_value(&AttachmentBeginResult { id })
}

fn append(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &AttachmentAppendParams,
) -> Result<Value, ApiError> {
    let upload = engine
        .uploads
        .open
        .get(&params.id)
        .filter(|u| u.connection == connection && !u.done)
        .ok_or_else(|| gone(&params.id))?;
    if params.offset != upload.received {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "expected the bytes from {}, not {}",
                upload.received, params.offset
            ),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(params.content_base64.as_bytes())
        .map_err(|_| ApiError::new(ErrorKind::InvalidValue, "not base64"))?;
    let len = bytes.len() as u64;
    if len > ATTACHMENT_CHUNK || upload.received + len > upload.bytes {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "more bytes than the file has, or than one append takes",
        ));
    }
    let name = upload.name.clone();
    engine
        .system
        .attachment_write(&params.id, &name, params.offset, &bytes)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    let upload = engine
        .uploads
        .open
        .get_mut(&params.id)
        .ok_or_else(|| gone(&params.id))?;
    upload.received += len;
    to_value(&AttachmentAppendResult {
        received: upload.received,
    })
}

fn finish(
    engine: &mut Engine,
    connection: ConnectionId,
    params: AttachmentFinishParams,
) -> Result<Value, ApiError> {
    let upload = engine
        .uploads
        .open
        .get(&params.id)
        .filter(|u| u.connection == connection)
        .ok_or_else(|| gone(&params.id))?;
    let name = upload.name.clone();
    let bytes = upload.bytes;
    if params.discard {
        engine.uploads.open.remove(&params.id);
        engine
            .system
            .attachment_finish(&params.id, &name, false)
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        return to_value(&AttachedFile {
            id: params.id,
            bytes,
            map: None,
        });
    }
    if upload.received != bytes {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("{} of {bytes} bytes came", upload.received),
        ));
    }
    engine
        .system
        .attachment_finish(&params.id, &name, true)
        .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
    if let Some(upload) = engine.uploads.open.get_mut(&params.id) {
        upload.done = true;
    }
    to_value(&AttachedFile {
        id: params.id,
        bytes,
        map: None,
    })
}

fn read(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &AttachmentReadParams,
) -> Result<Value, ApiError> {
    let found = engine.attached_file(Some(connection), &params.id)?;
    let offset = params.offset.unwrap_or(0).min(found.bytes);
    let head = engine
        .system
        .attachment_read(&params.id, 0, 8192)
        .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
    if binary(&found.name, &head) {
        return to_value(&AttachmentReadResult {
            name: found.name,
            bytes: found.bytes,
            text: String::new(),
            truncated: false,
            binary: true,
        });
    }
    let read = engine
        .system
        .attachment_read(&params.id, offset, READ_MAX)
        .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
    to_value(&AttachmentReadResult {
        name: found.name,
        bytes: found.bytes,
        text: String::from_utf8_lossy(&read).into_owned(),
        truncated: offset + (read.len() as u64) < found.bytes,
        binary: false,
    })
}

/// `attachment/image` (FB-248): checked here, answered when the host has
/// made the picture.
fn image(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &AttachmentImageParams,
) -> Result<Value, ApiError> {
    let found = engine.attached_file(Some(connection), &params.id)?;
    let head = engine
        .system
        .attachment_read(&params.id, 0, 16)
        .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
    let Some(format) = picture_format(&head) else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{} is not a PNG, JPEG or WebP picture, so UXDuck can't show it to the AI model. \
                 Read it with attachment_read if it is text.",
                found.name
            ),
        ));
    };
    let Some((_, request_id)) = engine.replying_to.take() else {
        return Err(ApiError::new(
            ErrorKind::InvalidRequest,
            "attachment/image is answered as a request",
        ));
    };
    let key = format!("pc{}", &engine.random.token()[..16]);
    let max_bytes = params.max_bytes.unwrap_or(LOOK_REPLY_BYTES);
    let order = PictureOrder {
        key: key.clone(),
        attachment: params.id.clone(),
        long_edge: PICTURE_LONG_EDGE,
        max_bytes: max_bytes.min(LOOK_REPLY_BYTES),
    };
    if let Err(why) = engine.system.attachment_picture(order) {
        engine.replying_to = Some((connection, request_id));
        return Err(ApiError::new(ErrorKind::InvalidValue, why));
    }
    engine.uploads.pictures.insert(
        key,
        Picturing {
            connection,
            request_id,
            id: params.id.clone(),
            found,
            format,
            held: kept_to(max_bytes),
        },
    );
    Ok(Value::Null)
}

/// What the note says when a picture was asked for with more room than
/// UXDuck holds, with what was asked in whole MB or, if it isn't, in bytes.
fn kept_to(max_bytes: u64) -> Option<String> {
    const MB: u64 = 1024 * 1024;
    (max_bytes > LOOK_REPLY_BYTES).then(|| {
        let asked = if max_bytes.is_multiple_of(MB) {
            format!("{} MB", max_bytes / MB)
        } else {
            format!("{max_bytes} bytes")
        };
        format!(
            "Kept to {} MB of picture, not the {asked} asked for: UXDuck holds a picture in memory \
             while it answers.",
            LOOK_REPLY_BYTES / MB
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::{binary, file_name, kept_to};

    #[test]
    fn a_picture_held_to_8_mb_says_exactly_what_was_asked() {
        assert_eq!(kept_to(8 * 1024 * 1024), None);
        assert_eq!(
            kept_to(100 * 1024 * 1024).unwrap(),
            "Kept to 8 MB of picture, not the 100 MB asked for: UXDuck holds a picture in memory \
             while it answers."
        );
        assert!(
            kept_to(8 * 1024 * 1024 + 1)
                .unwrap()
                .contains("not the 8388609 bytes asked for"),
        );
    }

    #[test]
    fn a_name_keeps_no_folders_or_hidden_dot() {
        assert_eq!(file_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(file_name("C:\\logs\\flight.tlog").unwrap(), "flight.tlog");
        assert_eq!(file_name(".hidden.tlog").unwrap(), "hidden.tlog");
        assert_eq!(file_name("a\u{0}b?.mp4").unwrap(), "ab.mp4");
        assert!(file_name("..").is_err());
        assert!(file_name(&"x".repeat(300)).is_err());
    }

    #[test]
    fn logs_and_videos_are_not_text() {
        assert!(binary("2026-10-30.tlog", b"abc"));
        assert!(binary("00000012.BIN", b"abc"));
        assert!(binary("flight.MP4", b"abc"));
        assert!(binary("odd.txt", b"ab\0c"));
        assert!(!binary("copter.param", b"ARMING_CHECK,1\n"));
    }
}

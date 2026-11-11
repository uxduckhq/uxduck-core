//! Files attached to chat messages (S246): uploaded in order over the API,
//! kept by the host under their id, then read by the agent or handed to a
//! replay or a flight's videos by that id. Uploads in progress live here;
//! a sent file is found again through the message that carries it.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::Value;
use uxduck_contracts::dto::thread::{
    ATTACHMENT_CHUNK, ATTACHMENT_MAX, AttachedFile, AttachmentAppendParams, AttachmentAppendResult,
    AttachmentBeginParams, AttachmentBeginResult, AttachmentFinishParams, AttachmentReadParams,
    AttachmentReadResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::thread::{self, Attachment, ItemKind, ItemStatus};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The most `attachment/read` returns at once.
const READ_MAX: usize = 64 * 1024;

/// At most this many uploads open on one connection.
const OPEN_MAX: usize = thread::MAX_ATTACHMENTS;

/// Uploads begun and not yet sent with a message.
#[derive(Debug, Default)]
pub(crate) struct Uploads {
    open: BTreeMap<String, Upload>,
}

#[derive(Debug)]
struct Upload {
    connection: ConnectionId,
    name: String,
    bytes: u64,
    received: u64,
    done: bool,
}

/// A file as the agent and the features that take one know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub name: String,
    pub bytes: u64,
}

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
            });
        }
        for attachment in attachments.iter() {
            if let Some(file) = &attachment.file {
                self.uploads.open.remove(&file.id);
            }
        }
        Ok(())
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
    let needs = if matches!(request, ClientRequest::AttachmentRead(_)) {
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
    if params.bytes > ATTACHMENT_MAX {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A file attached to a message is at most 8 GB.",
        ));
    }
    let open = engine
        .uploads
        .open
        .values()
        .filter(|u| u.connection == connection)
        .count();
    if open >= OPEN_MAX {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("At most {OPEN_MAX} files at once. Send or remove one first."),
        ));
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::{binary, file_name};

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

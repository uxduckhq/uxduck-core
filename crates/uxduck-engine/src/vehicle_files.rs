//! A vehicle's files over MAVLink FTP (S121, ADR-0027 F1 to F7). The data
//! plane runs the protocol; the engine answers listings and reads, keeps
//! transfers as journaled entities that go on whoever watches them (F4),
//! writes a download's bytes through the files port into "Vehicle files"
//! (F5), and resumes what waited when the vehicle is back.
//!
//! A download's copy is always the start of the file: the data plane
//! reports bytes in order, so a resumed download asks for the file from its
//! copy's length, after a dropped link or a restart of UXDuck.

use std::collections::{BTreeMap, VecDeque};

use base64::Engine as _;
use serde_json::{Value, json};
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::files::{
    FileEntry, FileSaveResult, FileTransfer as FileTransferDto, FileTransferParams,
    FilesDownloadParams, FilesListResult, FilesPathParams, FilesReadResult, FilesRemoveParams,
    FilesRenameParams, FilesUploadParams, READ_MAX, UPLOAD_MAX,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::files::{FileTransfer, TransferDirection, TransferState};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::runtime::{FileOp, TransferOrder};
use crate::transfers::Failure;
use crate::{ConnectionId, Engine, upsert};

/// Progress is journaled, and so pushed, at most this often while a
/// transfer runs.
const JOURNAL_EVERY_US: u64 = 1_000_000;
/// The rate is measured over this long (F12: the last five seconds).
const RATE_WINDOW_US: u64 = 5_000_000;
/// A waiting transfer is tried again this long after it stopped.
const RETRY_AFTER_US: u64 = 5_000_000;

/// Keeps a download's copy (S121, ADR-0027 F5). Paths are relative to
/// "Vehicle files": the vehicle's id, then the path on the vehicle.
pub trait FilesPort: Send {
    /// Bytes of a copy so far, partial or whole; 0 when there is none.
    fn copied(&self, local: &str) -> u64;
    /// Writes bytes at `offset` into the partial copy, cutting off anything
    /// past it first.
    ///
    /// # Errors
    /// When the disk refuses, in words.
    fn write(&mut self, local: &str, offset: u64, bytes: &[u8]) -> Result<(), String>;
    /// The copy is whole: it takes the file's own name.
    ///
    /// # Errors
    /// When the disk refuses, in words.
    fn finish(&mut self, local: &str) -> Result<(), String>;
    /// Removes a copy, partial or whole.
    fn discard(&mut self, local: &str);
    /// A one-time address the browser saves a whole copy from.
    fn grant(&mut self, local: &str, name: &str) -> Option<String>;
}

/// What the data plane reports about a file job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileReport {
    /// A listing, whole: name and size, `None` for a folder.
    Listed {
        id: String,
        entries: Vec<(String, Option<u64>)>,
    },
    Opened {
        id: String,
        size: u64,
    },
    Data {
        id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    Sent {
        id: String,
        done: u64,
        total: u64,
    },
    Done {
        id: String,
        result: Result<(), Failure>,
    },
}

/// A request answered when the vehicle answers.
#[derive(Debug)]
struct Waiting {
    reply: Option<(ConnectionId, Value)>,
    vehicle_id: String,
    path: String,
    what: Asked,
}

#[derive(Debug)]
enum Asked {
    List,
    /// A small file read whole, up to `READ_MAX`.
    Read(Vec<u8>),
    /// Removed, made, renamed: answered with nothing.
    Change,
}

/// A running transfer's live parts, not journaled.
#[derive(Debug, Default)]
struct Live {
    /// `(time, bytes done)` over the last few seconds, for the rate.
    samples: VecDeque<(u64, u64)>,
    journaled_us: u64,
}

/// Requests waiting for vehicles, transfers' live parts, and the port.
#[derive(Default)]
pub(crate) struct Files {
    port: Option<Box<dyn FilesPort>>,
    waiting: BTreeMap<String, Waiting>,
    live: BTreeMap<String, Live>,
    /// An upload's bytes until it is done; lost on a restart of UXDuck.
    uploads: BTreeMap<String, Vec<u8>>,
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files")
            .field("port", &self.port.is_some())
            .field("waiting", &self.waiting)
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

fn actor() -> Actor {
    Actor {
        client_id: "files".into(),
        operator_id: None,
        identity: None,
    }
}

/// A path as the vehicle takes it: no trailing slash, `/` for the root,
/// and ArduPilot's own folders without a leading slash (`@SYS/uarts.txt`),
/// as its root listing names them `/@SYS`.
fn clean(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".into();
    }
    let trimmed = trimmed.trim_end_matches('/');
    trimmed
        .strip_prefix('/')
        .filter(|rest| rest.starts_with('@'))
        .unwrap_or(trimmed)
        .to_owned()
}

/// Why the vehicle refused a file, in words. ArduPilot lists every one of
/// its own files but has nothing for some on some boards (a simulation has
/// no `dma.txt`), and then says there is no such file.
fn refused(vehicle: &str, path: &str, failure: &Failure) -> String {
    let detail = failure.detail(0);
    if read_only(path) && detail == "there is no such file or folder" {
        return format!(
            "{} is empty on {vehicle}: ArduPilot has nothing to put in it here",
            name_of(path)
        );
    }
    format!("{vehicle}: {detail}")
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// Where a download's copy goes: the vehicle's id, then the path, with no
/// part that climbs out of the folder.
fn local_for(vehicle_id: &str, path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    format!("{vehicle_id}/{}", parts.join("/"))
}

/// The ArduPilot folders that are the autopilot's own and read-only (F7).
fn read_only(path: &str) -> bool {
    let path = path.trim_start_matches('/');
    path.starts_with("@SYS") || path.starts_with("@ROMFS")
}

fn denied(capability: Capability) -> ApiError {
    ApiError::new(ErrorKind::CapabilityDenied, capability.denied_message())
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(denied(capability))
        }
    };
    match request {
        ClientRequest::FilesList(params) => {
            require(Capability::Read)?;
            engine.ask(&params, Asked::List)
        }
        ClientRequest::FilesRead(params) => {
            require(Capability::Read)?;
            engine.ask(&params, Asked::Read(Vec::new()))
        }
        ClientRequest::FilesRemove(FilesRemoveParams {
            vehicle_id,
            path,
            dir,
        }) => {
            require(Capability::Config)?;
            refuse_read_only(&path)?;
            let op = if dir {
                FileOp::RemoveDir { path: clean(&path) }
            } else {
                FileOp::Remove { path: clean(&path) }
            };
            engine.change(&vehicle_id, &path, op)
        }
        ClientRequest::FilesMkdir(FilesPathParams { vehicle_id, path }) => {
            require(Capability::Config)?;
            refuse_read_only(&path)?;
            engine.change(&vehicle_id, &path, FileOp::MakeDir { path: clean(&path) })
        }
        ClientRequest::FilesRename(FilesRenameParams {
            vehicle_id,
            from,
            to,
        }) => {
            require(Capability::Config)?;
            refuse_read_only(&from)?;
            refuse_read_only(&to)?;
            let op = FileOp::Rename {
                from: clean(&from),
                to: clean(&to),
            };
            engine.change(&vehicle_id, &from, op)
        }
        ClientRequest::FilesDownload(params) => {
            require(Capability::Read)?;
            engine.download(connection, &params)
        }
        ClientRequest::FilesUpload(params) => {
            require(Capability::Config)?;
            engine.upload(connection, &params)
        }
        ClientRequest::FilesPause(FileTransferParams { id }) => {
            require(Capability::Read)?;
            engine.pause(&id)
        }
        ClientRequest::FilesResume(FileTransferParams { id }) => {
            require(Capability::Read)?;
            engine.resume(&id)
        }
        ClientRequest::FilesCancel(FileTransferParams { id }) => {
            require(Capability::Read)?;
            engine.cancel_transfer(&id)?;
            to_value(&Empty::default())
        }
        ClientRequest::FilesSave(FileTransferParams { id }) => {
            require(Capability::Read)?;
            engine.save(&id)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a files method")),
    }
}

fn refuse_read_only(path: &str) -> Result<(), ApiError> {
    if read_only(path) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the autopilot's own files are read-only",
        ));
    }
    Ok(())
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::FileTransferUpserted { transfer } => vec![EntityChange {
            file_transfer: Some(transfer.into()),
            ..upsert(EntityType::FileTransfer, &transfer.id)
        }],
        Change::FileTransferRemoved { id } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::FileTransfer, id)
        }],
        _ => Vec::new(),
    }
}

impl Engine {
    /// Gives the engine its files port (the daemon's "Vehicle files"), and
    /// sets transfers UXDuck was running when it stopped to wait for their
    /// vehicles. An upload's bytes did not survive: it failed.
    pub fn set_files(&mut self, port: Box<dyn FilesPort>) {
        self.files.port = Some(port);
        let stopped: Vec<FileTransfer> = self
            .state
            .file_transfers
            .values()
            .filter(|t| t.state.active())
            .cloned()
            .collect();
        for mut transfer in stopped {
            transfer.rate_bps = None;
            match transfer.direction {
                TransferDirection::Download => {
                    transfer.state = TransferState::Waiting;
                    transfer.done = transfer
                        .local
                        .as_deref()
                        .and_then(|local| self.files.port.as_ref().map(|p| p.copied(local)))
                        .unwrap_or(0);
                    transfer.detail = Some("UXDuck restarted: it goes on with the vehicle".into());
                }
                TransferDirection::Upload => {
                    transfer.state = TransferState::Failed;
                    transfer.detail =
                        Some("UXDuck restarted before it finished: send it again".into());
                }
            }
            transfer.updated_at_us = self.clock.now_us();
            self.commit_as(actor(), Change::FileTransferUpserted { transfer });
        }
    }

    fn file_vehicle(&self, vehicle_id: &str) -> Result<String, ApiError> {
        let vehicle = self.vehicle(vehicle_id).ok_or_else(|| {
            ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}"))
        })?;
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            return Err(ApiError::new(ErrorKind::InvalidValue, why));
        }
        Ok(vehicle.id.clone())
    }

    fn file_id(&mut self, prefix: &str) -> String {
        format!("{prefix}{}", &self.random.token()[..16])
    }

    /// A listing or a read, answered when the vehicle answers.
    fn ask(&mut self, params: &FilesPathParams, what: Asked) -> Result<Value, ApiError> {
        let vehicle_id = self.file_vehicle(&params.vehicle_id)?;
        let path = clean(&params.path);
        let id = self.file_id("fo");
        let op = match what {
            Asked::List => FileOp::List { path: path.clone() },
            Asked::Read(_) | Asked::Change => FileOp::Read {
                path: path.clone(),
                from: 0,
            },
        };
        self.files.waiting.insert(
            id.clone(),
            Waiting {
                reply: self.replying_to.take(),
                vehicle_id: vehicle_id.clone(),
                path,
                what,
            },
        );
        self.links
            .transfer(TransferOrder::File { vehicle_id, id, op });
        Ok(Value::Null)
    }

    /// A removal, a new folder or a rename, answered when the vehicle says.
    fn change(&mut self, vehicle_id: &str, path: &str, op: FileOp) -> Result<Value, ApiError> {
        let vehicle_id = self.file_vehicle(vehicle_id)?;
        let id = self.file_id("fo");
        self.files.waiting.insert(
            id.clone(),
            Waiting {
                reply: self.replying_to.take(),
                vehicle_id: vehicle_id.clone(),
                path: clean(path),
                what: Asked::Change,
            },
        );
        self.links
            .transfer(TransferOrder::File { vehicle_id, id, op });
        Ok(Value::Null)
    }

    fn download(
        &mut self,
        _connection: ConnectionId,
        params: &FilesDownloadParams,
    ) -> Result<Value, ApiError> {
        let vehicle_id = self.file_vehicle(&params.vehicle_id)?;
        let Some(port) = self.files.port.as_mut() else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "downloading a vehicle's files needs UXDuck Desktop",
            ));
        };
        let path = clean(&params.path);
        let local = local_for(&vehicle_id, &path);
        // A fresh download starts again: an older copy of it goes.
        port.discard(&local);
        let stale: Vec<String> = self
            .state
            .file_transfers
            .values()
            .filter(|t| t.local.as_deref() == Some(&local))
            .map(|t| t.id.clone())
            .collect();
        for id in stale {
            let _ = self.cancel_transfer(&id);
        }
        let now = self.clock.now_us();
        let transfer = FileTransfer {
            id: self.file_id("fd"),
            vehicle_id,
            direction: TransferDirection::Download,
            name: name_of(&path),
            path,
            local: Some(local),
            size: params.size,
            done: 0,
            state: TransferState::Queued,
            detail: None,
            rate_bps: None,
            started_at_us: now,
            updated_at_us: now,
        };
        self.commit_as(
            actor(),
            Change::FileTransferUpserted {
                transfer: transfer.clone(),
            },
        );
        self.start_transfer(&transfer);
        to_value(&FileTransferDto::from(&transfer))
    }

    fn upload(
        &mut self,
        _connection: ConnectionId,
        params: &FilesUploadParams,
    ) -> Result<Value, ApiError> {
        let vehicle_id = self.file_vehicle(&params.vehicle_id)?;
        let path = clean(&params.path);
        refuse_read_only(&path)?;
        if path == "/" || name_of(&path).is_empty() {
            return Err(ApiError::new(ErrorKind::InvalidValue, "name the file"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(params.content_base64.trim())
            .map_err(|e| ApiError::new(ErrorKind::InvalidValue, format!("not base64: {e}")))?;
        if bytes.len() as u64 > UPLOAD_MAX {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "files over 4 MB are too slow to send over MAVLink",
            ));
        }
        let now = self.clock.now_us();
        let transfer = FileTransfer {
            id: self.file_id("fu"),
            vehicle_id,
            direction: TransferDirection::Upload,
            name: name_of(&path),
            path,
            local: None,
            size: Some(bytes.len() as u64),
            done: 0,
            state: TransferState::Queued,
            detail: None,
            rate_bps: None,
            started_at_us: now,
            updated_at_us: now,
        };
        self.files.uploads.insert(transfer.id.clone(), bytes);
        self.commit_as(
            actor(),
            Change::FileTransferUpserted {
                transfer: transfer.clone(),
            },
        );
        self.start_transfer(&transfer);
        to_value(&FileTransferDto::from(&transfer))
    }

    /// Hands a queued transfer to the data plane: a download from its copy's
    /// length, an upload from the start.
    fn start_transfer(&mut self, transfer: &FileTransfer) {
        let op = match transfer.direction {
            TransferDirection::Download => {
                let from = transfer
                    .local
                    .as_deref()
                    .and_then(|local| self.files.port.as_ref().map(|p| p.copied(local)))
                    .unwrap_or(0);
                FileOp::Read {
                    path: transfer.path.clone(),
                    from,
                }
            }
            TransferDirection::Upload => {
                let Some(bytes) = self.files.uploads.get(&transfer.id).cloned() else {
                    return;
                };
                FileOp::Write {
                    path: transfer.path.clone(),
                    bytes,
                }
            }
        };
        self.files.live.insert(transfer.id.clone(), Live::default());
        self.links.transfer(TransferOrder::File {
            vehicle_id: transfer.vehicle_id.clone(),
            id: transfer.id.clone(),
            op,
        });
    }

    fn transfer(&self, id: &str) -> Result<FileTransfer, ApiError> {
        self.state
            .file_transfers
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no transfer {id}")))
    }

    fn pause(&mut self, id: &str) -> Result<Value, ApiError> {
        let mut transfer = self.transfer(id)?;
        if !transfer.state.active() {
            return to_value(&FileTransferDto::from(&transfer));
        }
        self.links.transfer(TransferOrder::CancelFile {
            vehicle_id: transfer.vehicle_id.clone(),
            id: id.to_owned(),
        });
        self.files.live.remove(id);
        // Progress is journaled once a second: the copy says how far it got.
        if let (Some(local), Some(port)) = (transfer.local.as_deref(), self.files.port.as_ref()) {
            transfer.done = port.copied(local);
        }
        transfer.state = TransferState::Paused;
        transfer.rate_bps = None;
        transfer.detail = None;
        transfer.updated_at_us = self.clock.now_us();
        self.commit_as(
            actor(),
            Change::FileTransferUpserted {
                transfer: transfer.clone(),
            },
        );
        to_value(&FileTransferDto::from(&transfer))
    }

    fn resume(&mut self, id: &str) -> Result<Value, ApiError> {
        let transfer = self.transfer(id)?;
        if matches!(
            transfer.state,
            TransferState::Queued | TransferState::Running | TransferState::Done
        ) {
            return to_value(&FileTransferDto::from(&transfer));
        }
        if transfer.direction == TransferDirection::Upload && !self.files.uploads.contains_key(id) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "UXDuck no longer has this file: send it again",
            ));
        }
        self.file_vehicle(&transfer.vehicle_id)?;
        let transfer = self.requeue(transfer);
        to_value(&FileTransferDto::from(&transfer))
    }

    fn requeue(&mut self, mut transfer: FileTransfer) -> FileTransfer {
        if transfer.direction == TransferDirection::Download {
            transfer.done = transfer
                .local
                .as_deref()
                .and_then(|local| self.files.port.as_ref().map(|p| p.copied(local)))
                .unwrap_or(0);
        } else {
            transfer.done = 0;
        }
        transfer.state = TransferState::Queued;
        transfer.detail = None;
        transfer.rate_bps = None;
        transfer.updated_at_us = self.clock.now_us();
        self.commit_as(
            actor(),
            Change::FileTransferUpserted {
                transfer: transfer.clone(),
            },
        );
        self.start_transfer(&transfer);
        transfer
    }

    /// Cancels a transfer, or puts a finished one away; either way its
    /// copy goes.
    fn cancel_transfer(&mut self, id: &str) -> Result<(), ApiError> {
        let transfer = self.transfer(id)?;
        if transfer.state.active() {
            self.links.transfer(TransferOrder::CancelFile {
                vehicle_id: transfer.vehicle_id.clone(),
                id: id.to_owned(),
            });
        }
        if let (Some(local), Some(port)) = (transfer.local.as_deref(), self.files.port.as_mut()) {
            port.discard(local);
        }
        self.files.live.remove(id);
        self.files.uploads.remove(id);
        self.commit_as(actor(), Change::FileTransferRemoved { id: id.to_owned() });
        Ok(())
    }

    fn save(&mut self, id: &str) -> Result<Value, ApiError> {
        let transfer = self.transfer(id)?;
        if transfer.state != TransferState::Done {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the download has not finished",
            ));
        }
        let url = transfer
            .local
            .as_deref()
            .zip(self.files.port.as_mut())
            .and_then(|(local, port)| port.grant(local, &transfer.name))
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "the copy is gone"))?;
        to_value(&FileSaveResult {
            url,
            name: transfer.name,
        })
    }

    /// What the data plane reports about a file job.
    pub fn file_reported(&mut self, report: FileReport) {
        let id = match &report {
            FileReport::Listed { id, .. }
            | FileReport::Opened { id, .. }
            | FileReport::Data { id, .. }
            | FileReport::Sent { id, .. }
            | FileReport::Done { id, .. } => id.clone(),
        };
        if self.files.waiting.contains_key(&id) {
            self.answer_waiting(&id, report);
        } else if self.state.file_transfers.contains_key(&id) {
            self.transfer_reported(&id, report);
        }
    }

    fn answer_waiting(&mut self, id: &str, report: FileReport) {
        let Some(mut waiting) = self.files.waiting.remove(id) else {
            return;
        };
        let result: Option<Result<Value, ApiError>> = match (report, &mut waiting.what) {
            (FileReport::Listed { entries, .. }, Asked::List) => {
                let mut entries: Vec<FileEntry> = entries
                    .into_iter()
                    .map(|(name, size)| FileEntry {
                        path: join(&waiting.path, &name),
                        name,
                        dir: size.is_none(),
                        size,
                    })
                    .collect();
                entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
                // Answered here; the job's `Done` that follows finds nothing.
                Some(to_value(&FilesListResult {
                    vehicle_id: waiting.vehicle_id.clone(),
                    path: waiting.path.clone(),
                    entries,
                }))
            }
            (FileReport::Data { bytes, .. }, Asked::Read(read)) => {
                read.extend_from_slice(&bytes);
                if read.len() as u64 >= READ_MAX {
                    // Enough: stop the vehicle sending the rest.
                    self.links.transfer(TransferOrder::CancelFile {
                        vehicle_id: waiting.vehicle_id.clone(),
                        id: id.to_owned(),
                    });
                    let read = std::mem::take(read);
                    Some(to_value(&read_result(&waiting, &read, true)))
                } else {
                    None
                }
            }
            (FileReport::Done { result: Ok(()), .. }, Asked::Read(read)) => {
                let read = std::mem::take(read);
                Some(to_value(&read_result(&waiting, &read, false)))
            }
            (FileReport::Done { result: Ok(()), .. }, Asked::Change) => {
                Some(to_value(&Empty::default()))
            }
            (
                FileReport::Done {
                    result: Err(failure),
                    ..
                },
                _,
            ) => Some(Err(ApiError::new(
                ErrorKind::InvalidValue,
                refused(
                    &self.vehicle_name(&waiting.vehicle_id),
                    &waiting.path,
                    &failure,
                ),
            ))),
            _ => None,
        };
        let Some(result) = result else {
            self.files.waiting.insert(id.to_owned(), waiting);
            return;
        };
        if let Some((connection, request_id)) = waiting.reply {
            let frame = match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": request_id, "result": value }),
                Err(error) => crate::rpc::router::error(&request_id, &error),
            };
            self.send(connection, &frame);
        }
    }

    fn transfer_reported(&mut self, id: &str, report: FileReport) {
        let Some(mut transfer) = self.state.file_transfers.get(id).cloned() else {
            return;
        };
        // A paused or cancelled job's last words.
        if !transfer.state.active() {
            return;
        }
        let now = self.clock.now_us();
        let mut journal = false;
        match report {
            FileReport::Listed { .. } => return,
            FileReport::Opened { size, .. } => {
                if size > 0 {
                    transfer.size = Some(size);
                }
                transfer.state = TransferState::Running;
                transfer.detail = None;
                journal = true;
            }
            FileReport::Data { offset, bytes, .. } => {
                let written = match (transfer.local.as_deref(), self.files.port.as_mut()) {
                    (Some(local), Some(port)) => port.write(local, offset, &bytes),
                    _ => Err("this UXDuck keeps no copies".into()),
                };
                if let Err(why) = written {
                    self.links.transfer(TransferOrder::CancelFile {
                        vehicle_id: transfer.vehicle_id.clone(),
                        id: id.to_owned(),
                    });
                    self.end_transfer(
                        transfer,
                        Err(format!("the copy could not be written: {why}")),
                    );
                    return;
                }
                transfer.state = TransferState::Running;
                transfer.done = offset + bytes.len() as u64;
            }
            FileReport::Sent { done, total, .. } => {
                transfer.state = TransferState::Running;
                transfer.done = done;
                transfer.size = Some(total);
            }
            FileReport::Done { result, .. } => {
                let result = match result {
                    Ok(()) => Ok(()),
                    Err(failure @ (Failure::Timeout | Failure::NotSent(_))) => {
                        return self.wait_for_vehicle(transfer, &failure.detail(0));
                    }
                    Err(Failure::Rejected(why)) if why == "the vehicle restarted" => {
                        return self.wait_for_vehicle(transfer, &why);
                    }
                    Err(failure) if read_only(&transfer.path) => Err(refused(
                        &self.vehicle_name(&transfer.vehicle_id),
                        &transfer.path,
                        &failure,
                    )),
                    Err(failure) => Err(failure.detail(0)),
                };
                self.end_transfer(transfer, result);
                return;
            }
        }
        let live = self.files.live.entry(id.to_owned()).or_default();
        live.samples.push_back((now, transfer.done));
        while live
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > RATE_WINDOW_US)
        {
            live.samples.pop_front();
        }
        if let (Some((first_at, first)), Some((last_at, last))) =
            (live.samples.front(), live.samples.back())
        {
            let span = last_at.saturating_sub(*first_at);
            if span >= 1_000_000 {
                transfer.rate_bps = Some(last.saturating_sub(*first) * 1_000_000 / span);
            }
        }
        if journal || now.saturating_sub(live.journaled_us) >= JOURNAL_EVERY_US {
            live.journaled_us = now;
            transfer.updated_at_us = now;
            self.commit_as(actor(), Change::FileTransferUpserted { transfer });
        }
    }

    fn wait_for_vehicle(&mut self, mut transfer: FileTransfer, why: &str) {
        self.files.live.remove(&transfer.id);
        transfer.state = TransferState::Waiting;
        transfer.rate_bps = None;
        transfer.detail = Some(format!(
            "{}: {why}",
            self.vehicle_name(&transfer.vehicle_id)
        ));
        if let (Some(local), Some(port)) = (transfer.local.as_deref(), self.files.port.as_ref()) {
            transfer.done = port.copied(local);
        }
        transfer.updated_at_us = self.clock.now_us();
        self.commit_as(actor(), Change::FileTransferUpserted { transfer });
    }

    fn end_transfer(&mut self, mut transfer: FileTransfer, result: Result<(), String>) {
        self.files.live.remove(&transfer.id);
        // Progress is journaled once a second, so the state can be behind:
        // the copy says how much came.
        if let (Some(local), Some(port)) = (transfer.local.as_deref(), self.files.port.as_ref()) {
            transfer.done = port.copied(local);
        }
        transfer.rate_bps = None;
        transfer.updated_at_us = self.clock.now_us();
        let result =
            result.and_then(
                |()| match (transfer.local.as_deref(), self.files.port.as_mut()) {
                    (Some(local), Some(port)) => port.finish(local),
                    _ => Ok(()),
                },
            );
        match result {
            Ok(()) => {
                transfer.state = TransferState::Done;
                transfer.detail = None;
                if transfer.direction == TransferDirection::Download {
                    transfer.size = Some(transfer.done);
                } else {
                    self.files.uploads.remove(&transfer.id);
                    transfer.done = transfer.size.unwrap_or(transfer.done);
                }
            }
            Err(why) => {
                transfer.state = TransferState::Failed;
                transfer.detail = Some(why);
            }
        }
        self.commit_as(actor(), Change::FileTransferUpserted { transfer });
    }

    /// Once a second: transfers waiting for a vehicle that is back go on,
    /// a few seconds after they stopped.
    pub fn resume_waiting_files(&mut self, now_us: u64) {
        let ready: Vec<FileTransfer> = self
            .state
            .file_transfers
            .values()
            .filter(|t| t.state == TransferState::Waiting)
            .filter(|t| now_us.saturating_sub(t.updated_at_us) >= RETRY_AFTER_US)
            .filter(|t| {
                self.vehicle(&t.vehicle_id).is_some_and(|v| {
                    v.presence == Presence::Online && self.not_writable(&v.link_id).is_none()
                })
            })
            .cloned()
            .collect();
        for transfer in ready {
            if transfer.direction == TransferDirection::Upload
                && !self.files.uploads.contains_key(&transfer.id)
            {
                continue;
            }
            self.requeue(transfer);
        }
    }
}

fn read_result(waiting: &Waiting, read: &[u8], truncated: bool) -> FilesReadResult {
    let text = String::from_utf8_lossy(read).into_owned();
    let odd = text
        .chars()
        .filter(|c| *c == char::REPLACEMENT_CHARACTER || (c.is_control() && !c.is_whitespace()))
        .count();
    FilesReadResult {
        vehicle_id: waiting.vehicle_id.clone(),
        path: waiting.path.clone(),
        bytes: read.len() as u64,
        truncated,
        binary: odd * 10 > text.chars().count(),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_stay_inside_their_vehicles_folder() {
        assert_eq!(local_for("v1", "/APM/LOGS/1.BIN"), "v1/APM/LOGS/1.BIN");
        assert_eq!(local_for("v1", "/../../etc/passwd"), "v1/etc/passwd");
        assert_eq!(local_for("v1", "@SYS/uarts.txt"), "v1/@SYS/uarts.txt");
        assert_eq!(join("/", "APM"), "/APM");
        assert_eq!(join("/APM", "LOGS"), "/APM/LOGS");
        assert_eq!(clean("/APM/LOGS/"), "/APM/LOGS");
        assert_eq!(clean(""), "/");
        assert_eq!(clean("/@SYS/uarts.txt"), "@SYS/uarts.txt");
        assert_eq!(
            refused(
                "Alpha",
                "@SYS/dma.txt",
                &Failure::Rejected("there is no such file or folder".into())
            ),
            "dma.txt is empty on Alpha: ArduPilot has nothing to put in it here"
        );
        assert!(read_only("@SYS/uarts.txt") && read_only("/@ROMFS/x") && !read_only("/APM"));
    }
}

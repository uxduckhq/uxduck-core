//! Recordings made on the vehicle (S429, ADR-0066 R12): listed from the
//! camera computer and downloaded to this computer over the computer's
//! pinned SSH, by the daemon's [`RecordingsPort`].
//!
//! A listing is answered when the computer answers and kept until the next
//! one, so a download names a recording by its id there. A download is a
//! [`FileTransfer`] with `from_computer`, so it is journaled, pushed,
//! paused, resumed and cancelled as a vehicle's other files are (ADR-0027
//! F4). The daemon copies each segment into a `.part` file, resumes from
//! what arrived after a dropped link, retries while the computer can be
//! reached, checks each segment's SHA-256 against the computer's, and joins
//! them into one file in the media folder. Only then is it catalogued as a
//! recording, marked as recorded on the vehicle (ADR-0017 V21).

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;
use uxduck_contracts::dto::companion_cameras::{
    CameraRecordings, ComputerDownloadParams, ComputerRecordingsParams, ComputerRecordingsResult,
    RecordingOnVehicle,
};
use uxduck_contracts::dto::files::FileTransfer as FileTransferDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::companion::CompanionComputer;
use uxduck_core::domain::companion_cameras::{VehicleRecordMode, address_names};
use uxduck_core::domain::files::{
    FileTransfer, FromComputer, RemoteSegment, TransferDirection, TransferState,
};
use uxduck_core::domain::video::{
    RecordedOnVehicle, RecordingState, RecordingTrigger, VideoContainer, VideoCounters,
    VideoRecording, VideoSource, VideoState,
};
use uxduck_core::events::{Actor, Change};

use crate::companions::through;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Progress is journaled, and so pushed, at most this often.
const JOURNAL_EVERY_US: u64 = 1_000_000;
/// The rate is measured over this long (ADR-0027 F12: the last five seconds).
const RATE_WINDOW_US: u64 = 5_000_000;

/// Where the daemon reaches camera computers for their recordings.
pub trait RecordingsPort: Send {
    fn request(&mut self, order: RecordingOrder);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordingOrder {
    /// List every recording camera's recordings on the computer.
    List {
        id: String,
        computer: Box<CompanionComputer>,
        through: Option<Box<CompanionComputer>>,
    },
    /// Copy a recording, going on from any `.part` files already there.
    Download {
        transfer_id: String,
        computer: Box<CompanionComputer>,
        through: Option<Box<CompanionComputer>>,
        from: FromComputer,
    },
    /// Stop a download, keeping what arrived unless `discard`.
    Stop { transfer_id: String, discard: bool },
}

/// One camera's recordings as the computer listed them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedCamera {
    pub camera: String,
    pub path: String,
    pub folder: Option<String>,
    pub mode: Option<VehicleRecordMode>,
    pub recordings: Vec<ListedRecording>,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedRecording {
    pub id: String,
    pub started_at_us: u64,
    pub duration_us: u64,
    pub segments: Vec<RemoteSegment>,
    /// Its last segment is still being written.
    pub recording: bool,
    pub not_downloadable: Option<String>,
}

/// What the daemon says about a download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadReport {
    /// Bytes of the whole recording that have arrived and been kept.
    Progress { done: u64 },
    /// It stopped for now and goes on by itself: why, and when, and
    /// whether it waits for an unmetered connection.
    Waiting {
        detail: String,
        done: u64,
        metered: bool,
    },
    /// It cannot go on.
    Failed { detail: String },
    /// Every segment matched and the recording is in the media folder.
    Kept(KeptRecording),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptRecording {
    pub file_name: String,
    pub bytes: u64,
    pub duration_us: Option<u64>,
    pub codec: Option<String>,
}

#[derive(Debug)]
struct Waiting {
    reply: Option<(ConnectionId, Value)>,
    vehicle_id: String,
    computer_id: String,
    /// Only this camera, when a picture was named.
    camera: Option<String>,
}

#[derive(Default)]
pub(crate) struct Recordings {
    port: Option<Box<dyn RecordingsPort>>,
    waiting: BTreeMap<String, Waiting>,
    /// Each computer's latest listing.
    listed: BTreeMap<String, Vec<ListedCamera>>,
    /// `(time, bytes done)` of each running download, and when it was
    /// last journaled.
    live: BTreeMap<String, (VecDeque<(u64, u64)>, u64)>,
}

impl std::fmt::Debug for Recordings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recordings")
            .field("port", &self.port.is_some())
            .field("waiting", &self.waiting)
            .field("listed", &self.listed)
            .finish_non_exhaustive()
    }
}

fn actor() -> Actor {
    Actor {
        client_id: "recordings".into(),
        operator_id: None,
        identity: None,
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn no_port() -> ApiError {
    invalid("Use UXDuck Desktop or the uxduck daemon for a camera computer's recordings.")
}

/// The camera a vehicle camera's downloads land in when no picture here
/// names its live path (ADR-0066 R12): one of its own, keyed by the vehicle
/// and the path, such as `vehicle:v:front`, with no address until someone
/// gives it one, so it stays the same camera when they do.
pub(crate) fn place_id(vehicle_id: &str, camera: &str) -> String {
    format!("vehicle:{vehicle_id}:{camera}")
}

/// The live path a camera with no address keeps the downloads of.
pub(crate) fn place_camera(source: &VideoSource) -> Option<&str> {
    if source.url.is_some() {
        return None;
    }
    let vehicle_id = source.vehicle_id.as_deref()?;
    source
        .id
        .strip_prefix("vehicle:")?
        .strip_prefix(vehicle_id)?
        .strip_prefix(':')
}

impl Engine {
    /// Gives the engine the daemon's way to camera computers, and sends on
    /// the downloads that were going when UXDuck stopped.
    pub fn set_recordings(&mut self, port: Box<dyn RecordingsPort>) {
        self.vehicle_recordings.port = Some(port);
        let going: Vec<FileTransfer> = self
            .state
            .file_transfers
            .values()
            .filter(|t| t.from_computer.is_some() && t.state.active())
            .cloned()
            .collect();
        for transfer in going {
            self.send_download(&transfer);
        }
    }

    /// The computer that records `source_id`'s camera, and that camera.
    fn computer_of_source(
        &self,
        vehicle_id: &str,
        source_id: &str,
    ) -> Result<(String, String), ApiError> {
        let source =
            self.state.video_sources.get(source_id).ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no camera {source_id}"))
            })?;
        let addresses: Vec<&str> = source
            .url
            .iter()
            .chain(&source.fallback_urls)
            .map(String::as_str)
            .collect();
        let vehicle = self.vehicle(vehicle_id).ok_or_else(|| {
            ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}"))
        })?;
        let place = place_camera(source);
        vehicle
            .companions
            .iter()
            .find_map(|computer| {
                let cameras = computer.check.as_ref()?.cameras.as_ref()?;
                let camera = match place {
                    Some(path) => cameras.paths.iter().find(|c| c.name == path)?,
                    None => cameras.named_by(addresses.iter().copied())?,
                };
                camera
                    .record
                    .as_ref()
                    .map(|_| (computer.id.clone(), camera.name.clone()))
            })
            .ok_or_else(|| {
                invalid(format!(
                    "No camera computer on {} records {} on the vehicle. Read the cameras on its vehicle page.",
                    vehicle.name, source.name
                ))
            })
    }

    /// The computer to reach, checked, with the one it is reached through.
    fn recording_computer(
        &self,
        vehicle_id: &str,
        computer_id: &str,
    ) -> Result<(Box<CompanionComputer>, Option<Box<CompanionComputer>>), ApiError> {
        let vehicle = self.vehicle(vehicle_id).ok_or_else(|| {
            ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}"))
        })?;
        let computer = vehicle
            .companions
            .iter()
            .find(|c| c.id == computer_id)
            .ok_or_else(|| invalid("Companion computer not found."))?;
        if computer
            .check
            .as_ref()
            .and_then(|c| c.host_key.as_ref())
            .is_none()
        {
            return Err(invalid(format!(
                "Check {} first, so UXDuck knows its SSH host key.",
                computer.name
            )));
        }
        Ok((Box::new(computer.clone()), through(vehicle, computer_id)))
    }

    /// `computer/recordings`.
    pub(crate) fn computer_recordings(
        &mut self,
        params: &ComputerRecordingsParams,
    ) -> Result<Value, ApiError> {
        if self.vehicle_recordings.port.is_none() {
            return Err(no_port());
        }
        let vehicle_id = self
            .vehicle(&params.vehicle_id)
            .map(|v| v.id.clone())
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no vehicle {}", params.vehicle_id),
                )
            })?;
        let (computer_id, camera) = match (&params.computer_id, &params.source_id) {
            (Some(computer_id), None) => (computer_id.clone(), None),
            (None, Some(source_id)) => {
                let (computer_id, camera) = self.computer_of_source(&vehicle_id, source_id)?;
                (computer_id, Some(camera))
            }
            _ => return Err(invalid("Name the computer or the camera, one of them.")),
        };
        let (computer, through) = self.recording_computer(&vehicle_id, &computer_id)?;
        let id = format!("rl{}", &self.random.token()[..16]);
        self.vehicle_recordings.waiting.insert(
            id.clone(),
            Waiting {
                reply: self.replying_to.take(),
                vehicle_id,
                computer_id,
                camera,
            },
        );
        if let Some(port) = self.vehicle_recordings.port.as_mut() {
            port.request(RecordingOrder::List {
                id,
                computer,
                through,
            });
        }
        Ok(Value::Null)
    }

    /// The daemon's answer to a listing.
    pub fn recordings_listed(&mut self, id: &str, result: Result<Vec<ListedCamera>, String>) {
        let Some(waiting) = self.vehicle_recordings.waiting.remove(id) else {
            return;
        };
        let (cameras, problem) = match result {
            Ok(cameras) => {
                self.vehicle_recordings
                    .listed
                    .insert(waiting.computer_id.clone(), cameras.clone());
                (cameras, None)
            }
            Err(why) => (Vec::new(), Some(why)),
        };
        let answer = ComputerRecordingsResult {
            vehicle_id: waiting.vehicle_id.clone(),
            computer_name: self
                .vehicle(&waiting.vehicle_id)
                .and_then(|v| v.companions.iter().find(|c| c.id == waiting.computer_id))
                .map(|c| c.name.clone())
                .unwrap_or_default(),
            cameras: cameras
                .iter()
                .filter(|c| waiting.camera.as_ref().is_none_or(|only| *only == c.camera))
                .map(|c| self.camera_recordings(&waiting.vehicle_id, &waiting.computer_id, c))
                .collect(),
            computer_id: waiting.computer_id,
            problem,
        };
        if let Some((connection, request_id)) = waiting.reply {
            let frame = match to_value(&answer) {
                Ok(value) => {
                    serde_json::json!({ "jsonrpc": "2.0", "id": request_id, "result": value })
                }
                Err(error) => crate::rpc::router::error(&request_id, &error),
            };
            self.send(connection, &frame);
        }
    }

    fn camera_recordings(
        &self,
        vehicle_id: &str,
        computer_id: &str,
        camera: &ListedCamera,
    ) -> CameraRecordings {
        let mut recordings: Vec<RecordingOnVehicle> = camera
            .recordings
            .iter()
            .map(|r| RecordingOnVehicle {
                id: r.id.clone(),
                started_at_us: r.started_at_us,
                duration_us: r.duration_us,
                bytes: r.segments.iter().map(|s| s.size).sum(),
                segments: u32::try_from(r.segments.len()).unwrap_or(u32::MAX),
                recording: r.recording,
                not_downloadable: r.not_downloadable.clone(),
                transfer_id: self.download_of(computer_id, &r.id).map(|t| t.id.clone()),
                recording_id: self.downloaded_as(computer_id, &r.id),
            })
            .collect();
        recordings.sort_by_key(|r| std::cmp::Reverse(r.started_at_us));
        let (source_id, problem) = match self.camera_of(vehicle_id, &camera.camera) {
            Ok(source) => (source.map(|s| s.id.clone()), camera.problem.clone()),
            Err(error) => (None, Some(error.message)),
        };
        CameraRecordings {
            camera: camera.camera.clone(),
            path: camera.path.clone(),
            folder: camera.folder.clone(),
            source_id,
            recordings,
            problem,
        }
    }

    /// The recording on this computer a recording on the vehicle was
    /// downloaded as, while it is here.
    fn downloaded_as(&self, computer_id: &str, recording: &str) -> Option<String> {
        self.state
            .video_recordings
            .values()
            .find(|v| {
                v.on_vehicle
                    .as_ref()
                    .is_some_and(|o| o.computer_id == computer_id && o.recording == recording)
            })
            .map(|v| v.id.clone())
    }

    /// The camera here whose recordings a vehicle camera's are: the
    /// vehicle's picture whose address names its live path (ADR-0066 R5),
    /// or the camera made for its downloads (R12), the same camera once it
    /// has an address. Two of them is for the person to settle, so it is
    /// said rather than guessed.
    fn camera_of(&self, vehicle_id: &str, camera: &str) -> Result<Option<&VideoSource>, ApiError> {
        let place = place_id(vehicle_id, camera);
        let mut matching = self.state.video_sources.values().filter(|s| {
            s.id == place
                || (s.vehicle_id.as_deref() == Some(vehicle_id)
                    && s.url
                        .iter()
                        .chain(&s.fallback_urls)
                        .any(|address| address_names(address, camera)))
        });
        let first = matching.next();
        let (Some(first), Some(second)) = (first, matching.next()) else {
            return Ok(first);
        };
        let vehicle = self
            .vehicle(vehicle_id)
            .map_or(vehicle_id, |v| v.name.as_str());
        Err(invalid(format!(
            "{} and {} are both {vehicle}'s {camera}, so UXDuck cannot tell whose recordings these are. Remove the address ending in /{camera} from one of them, or remove the camera you do not need.",
            first.name, second.name
        )))
    }

    /// A camera with no address whose live path `url` names, for a picture
    /// of the vehicle added by that address to be the same camera.
    pub(crate) fn place_named_by(
        &self,
        vehicle_id: &str,
        url: &str,
    ) -> Result<Option<VideoSource>, ApiError> {
        let mut matching = self.state.video_sources.values().filter(|s| {
            s.vehicle_id.as_deref() == Some(vehicle_id)
                && place_camera(s).is_some_and(|camera| address_names(url, camera))
        });
        let first = matching.next();
        let (Some(first), Some(second)) = (first, matching.next()) else {
            return Ok(first.cloned());
        };
        Err(invalid(format!(
            "This address names both {} and {}, kept for recordings from the vehicle. Give one of them this address in its Camera settings instead.",
            first.name, second.name
        )))
    }

    /// The newest download of a recording.
    fn download_of(&self, computer_id: &str, recording: &str) -> Option<&FileTransfer> {
        self.state
            .file_transfers
            .values()
            .filter(|t| {
                t.from_computer
                    .as_ref()
                    .is_some_and(|f| f.computer_id == computer_id && f.recording == recording)
            })
            .max_by_key(|t| t.started_at_us)
    }

    /// `computer/download`.
    #[allow(
        clippy::too_many_lines,
        reason = "a download's checks, reuse and new transfer in one place"
    )]
    pub(crate) fn computer_download(
        &mut self,
        params: &ComputerDownloadParams,
    ) -> Result<Value, ApiError> {
        if self.vehicle_recordings.port.is_none() {
            return Err(no_port());
        }
        let (computer, _) = self.recording_computer(&params.vehicle_id, &params.computer_id)?;
        let vehicle = self
            .vehicle(&params.vehicle_id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such vehicle"))?;
        let (camera, listed) = self
            .vehicle_recordings
            .listed
            .get(&params.computer_id)
            .and_then(|cameras| {
                cameras.iter().find_map(|camera| {
                    camera
                        .recordings
                        .iter()
                        .find(|r| r.id == params.recording)
                        .map(|r| (camera.clone(), r.clone()))
                })
            })
            .ok_or_else(|| {
                invalid(format!(
                    "List the recordings on {} again. UXDuck has not seen {} there.",
                    computer.name, params.recording
                ))
            })?;
        if listed.recording {
            return Err(invalid(format!(
                "{} is still recording this. It can be downloaded once the recording ends.",
                computer.name
            )));
        }
        if let Some(why) = &listed.not_downloadable {
            return Err(invalid(why.clone()));
        }
        if let Some(existing) = self.download_of(&params.computer_id, &listed.id).cloned() {
            let kept = existing.state == TransferState::Done
                && self
                    .downloaded_as(&params.computer_id, &listed.id)
                    .is_some();
            let mut existing = existing;
            if existing.state.active()
                && params.over_metered
                && let Some(from) = existing.from_computer.as_mut()
                && !from.over_metered
            {
                // A person chose to download now over the metered connection.
                from.over_metered = true;
                from.waits_for_unmetered = false;
                return self.resume_from_computer(existing);
            }
            if existing.state.active() || kept {
                return to_value(&FileTransferDto::from(&existing));
            }
            // Paused goes on from what it copied. Failed, or done and since
            // deleted here, starts again.
            if existing.state == TransferState::Paused {
                if let Some(from) = existing.from_computer.as_mut() {
                    from.over_metered |= params.over_metered;
                }
                return self.resume_from_computer(existing);
            }
            self.stop_from_computer(&existing.id, true);
            self.commit_as(actor(), Change::FileTransferRemoved { id: existing.id });
        }
        let now = self.clock.now_us();
        let name = self
            .camera_of(&vehicle.id, &camera.camera)?
            .map_or_else(|| camera.camera.clone(), |s| s.name.clone());
        let offset = self
            .video
            .port
            .as_ref()
            .map_or(0, |p| p.utc_offset_minutes(listed.started_at_us));
        let stem = format!(
            "{} {}",
            uxduck_media::photo::Civil::at(listed.started_at_us, offset).file_stem(),
            crate::photos::file_who(Some(&vehicle.name), &name)
        );
        let size: u64 = listed.segments.iter().map(|s| s.size).sum();
        let transfer = FileTransfer {
            id: format!("fd{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            direction: TransferDirection::Download,
            path: listed.id.clone(),
            name: format!("{stem}.mp4"),
            local: None,
            size: Some(size),
            done: 0,
            state: TransferState::Queued,
            detail: None,
            rate_bps: None,
            started_at_us: now,
            updated_at_us: now,
            from_computer: Some(FromComputer {
                computer_id: params.computer_id.clone(),
                camera: camera.camera.clone(),
                recording: listed.id.clone(),
                started_at_us: listed.started_at_us,
                duration_us: listed.duration_us,
                segments: listed.segments.clone(),
                stem,
                mode: camera.mode,
                over_metered: params.over_metered,
                waits_for_unmetered: false,
            }),
        };
        self.commit_as(
            actor(),
            Change::FileTransferUpserted {
                transfer: transfer.clone(),
            },
        );
        self.send_download(&transfer);
        to_value(&FileTransferDto::from(&transfer))
    }

    fn send_download(&mut self, transfer: &FileTransfer) {
        let Some(from) = transfer.from_computer.clone() else {
            return;
        };
        let Ok((computer, through)) =
            self.recording_computer(&transfer.vehicle_id, &from.computer_id)
        else {
            let mut transfer = transfer.clone();
            transfer.state = TransferState::Failed;
            transfer.detail = Some(
                "The camera computer is no longer on this vehicle, or has not been checked.".into(),
            );
            transfer.updated_at_us = self.clock.now_us();
            self.commit_as(actor(), Change::FileTransferUpserted { transfer });
            return;
        };
        self.vehicle_recordings.live.remove(&transfer.id);
        if let Some(port) = self.vehicle_recordings.port.as_mut() {
            port.request(RecordingOrder::Download {
                transfer_id: transfer.id.clone(),
                computer,
                through,
                from,
            });
        }
    }

    /// `files/pause` of a download from a camera computer.
    pub(crate) fn pause_from_computer(&mut self, mut transfer: FileTransfer) -> FileTransfer {
        self.stop_from_computer(&transfer.id, false);
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
        transfer
    }

    /// `files/resume` of a download from a camera computer.
    pub(crate) fn resume_from_computer(
        &mut self,
        mut transfer: FileTransfer,
    ) -> Result<Value, ApiError> {
        if self.vehicle_recordings.port.is_none() {
            return Err(no_port());
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
        self.send_download(&transfer);
        to_value(&FileTransferDto::from(&transfer))
    }

    /// Stops a download from a camera computer; with `discard` the daemon
    /// removes what it copied.
    pub(crate) fn stop_from_computer(&mut self, id: &str, discard: bool) {
        self.vehicle_recordings.live.remove(id);
        if let Some(port) = self.vehicle_recordings.port.as_mut() {
            port.request(RecordingOrder::Stop {
                transfer_id: id.to_owned(),
                discard,
            });
        }
    }

    /// What the daemon says about a download.
    pub fn recording_download_reported(&mut self, transfer_id: &str, report: DownloadReport) {
        let Some(mut transfer) = self.state.file_transfers.get(transfer_id).cloned() else {
            return;
        };
        // A paused or cancelled download's last words.
        if !transfer.state.active() {
            return;
        }
        let now = self.clock.now_us();
        match report {
            DownloadReport::Progress { done } => {
                let first = transfer.state != TransferState::Running;
                transfer.state = TransferState::Running;
                transfer.detail = None;
                transfer.done = done;
                if let Some(from) = transfer.from_computer.as_mut() {
                    from.waits_for_unmetered = false;
                }
                let (samples, journaled) = self
                    .vehicle_recordings
                    .live
                    .entry(transfer_id.to_owned())
                    .or_default();
                samples.push_back((now, done));
                while samples
                    .front()
                    .is_some_and(|(at, _)| now.saturating_sub(*at) > RATE_WINDOW_US)
                {
                    samples.pop_front();
                }
                if let (Some((first_at, first_done)), Some((last_at, last_done))) =
                    (samples.front(), samples.back())
                {
                    let span = last_at.saturating_sub(*first_at);
                    if span >= 1_000_000 {
                        transfer.rate_bps =
                            Some(last_done.saturating_sub(*first_done) * 1_000_000 / span);
                    }
                }
                if !first && now.saturating_sub(*journaled) < JOURNAL_EVERY_US {
                    return;
                }
                *journaled = now;
            }
            DownloadReport::Waiting {
                detail,
                done,
                metered,
            } => {
                self.vehicle_recordings.live.remove(transfer_id);
                if let Some(from) = transfer.from_computer.as_mut() {
                    from.waits_for_unmetered = metered;
                }
                transfer.state = TransferState::Waiting;
                transfer.detail = Some(detail);
                transfer.done = done;
                transfer.rate_bps = None;
            }
            DownloadReport::Failed { detail } => {
                self.vehicle_recordings.live.remove(transfer_id);
                transfer.state = TransferState::Failed;
                transfer.detail = Some(detail);
                transfer.rate_bps = None;
            }
            DownloadReport::Kept(kept) => {
                self.vehicle_recordings.live.remove(transfer_id);
                let file_name = kept.file_name.clone();
                transfer.rate_bps = None;
                if let Err(error) = self.catalogue(&transfer, kept) {
                    transfer.state = TransferState::Failed;
                    transfer.detail = Some(format!(
                        "{} The video is kept in the media folder as {file_name}.",
                        error.message
                    ));
                } else {
                    transfer.state = TransferState::Done;
                    transfer.detail = None;
                    transfer.done = transfer.size.unwrap_or(transfer.done);
                }
            }
        }
        transfer.updated_at_us = now;
        self.commit_as(actor(), Change::FileTransferUpserted { transfer });
    }

    /// A downloaded recording joins this computer's photos and videos as
    /// its camera's (ADR-0017 V21): the picture here that is that camera,
    /// or else a camera made for it, named as cameras are, such as "Plane
    /// front", which this first download creates.
    fn catalogue(&mut self, transfer: &FileTransfer, kept: KeptRecording) -> Result<(), ApiError> {
        let Some(from) = transfer.from_computer.as_ref() else {
            return Ok(());
        };
        let source_id = if let Some(source) = self.camera_of(&transfer.vehicle_id, &from.camera)? {
            source.id.clone()
        } else {
            let place = self.new_place(&transfer.vehicle_id, &from.camera, &from.computer_id);
            let id = place.id.clone();
            self.commit_as(actor(), Change::VideoSourceUpserted { source: place });
            id
        };
        let duration_us = kept.duration_us.unwrap_or(from.duration_us);
        let recording = VideoRecording {
            id: format!("vr{}", &self.random.token()[..16]),
            source_id,
            vehicle_id: Some(transfer.vehicle_id.clone()),
            started_at_us: from.started_at_us,
            ended_at_us: Some(from.started_at_us.saturating_add(duration_us)),
            duration_us,
            bytes: kept.bytes,
            codec: kept.codec,
            width: None,
            height: None,
            state: RecordingState::Done,
            detail: None,
            trigger: match from.mode {
                Some(VehicleRecordMode::Armed) => RecordingTrigger::Armed,
                Some(VehicleRecordMode::Always) => RecordingTrigger::Always,
                Some(VehicleRecordMode::Off) | None => RecordingTrigger::ByHand,
            },
            capture: None,
            file: None,
            camera_receipt_id: None,
            sound: false,
            file_name: Some(kept.file_name),
            flight_id: None,
            alignment: None,
            packed: None,
            on_vehicle: Some(RecordedOnVehicle {
                computer_id: from.computer_id.clone(),
                camera: from.camera.clone(),
                recording: from.recording.clone(),
            }),
            version: 1,
        };
        self.commit_as(actor(), Change::VideoRecordingUpserted { recording });
        Ok(())
    }

    /// The camera for a vehicle camera's downloads, with no address: named
    /// "<vehicle> <camera>" as cameras are, recorded on the vehicle only
    /// (ADR-0066 R6), kept on this computer and not in the account.
    fn new_place(&self, vehicle_id: &str, camera: &str, computer_id: &str) -> VideoSource {
        let now = self.clock.now_us();
        let vehicle = self.vehicle(vehicle_id);
        let vehicle_name = vehicle.map_or(vehicle_id, |v| v.name.as_str());
        let computer = vehicle
            .and_then(|v| v.companions.iter().find(|c| c.id == computer_id))
            .map_or("its camera computer", |c| c.name.as_str());
        let name = format!("{vehicle_name} {camera}");
        VideoSource {
            recording_requested: false,
            reencoding: None,
            id: place_id(vehicle_id, camera),
            state_detail: Some(format!(
                "Recordings downloaded from {computer}. Add {name}'s address in its Camera settings to watch it live."
            )),
            name,
            named_by_hand: true,
            port: 0,
            sender: String::new(),
            container: VideoContainer::Rtp,
            codec: None,
            width: None,
            height: None,
            frame_rate: None,
            state: VideoState::Stopped,
            picture_note: None,
            vehicle_id: Some(vehicle_id.to_owned()),
            assigned_by_hand: true,
            vehicle_camera: false,
            vehicle_camera_by_hand: false,
            vehicle_only: true,
            served_by: None,
            record_alone: false,
            klv: false,
            url: None,
            fallback_urls: Vec::new(),
            address_names: Vec::new(),
            internet: None,
            active_url: None,
            active_choice: None,
            added_by_hand: false,
            from_account: false,
            capture: None,
            file: None,
            tracks: Vec::new(),
            audio: None,
            sound: None,
            counters: VideoCounters::default(),
            first_seen_us: now,
            changed_at_us: now,
            version: 1,
        }
    }
}

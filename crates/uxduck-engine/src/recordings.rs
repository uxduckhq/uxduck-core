//! Recording cameras (ADR-0017 V16, S40): which camera records when, by the
//! policy and by hand, the recordings as entities, recovery after a crash,
//! and playback tokens. The daemon writes the files.

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{
    VideoPlaybackParams, VideoPlaybackResult, VideoRecordAllParams, VideoRecordParams,
    VideoRecordWithOthersParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::video::{
    RecordPolicy, RecordingState, RecordingTrigger, VideoRecording, VideoSource, VideoState,
};
use uxduck_core::events::{Actor, Change};

use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;
use crate::{Engine, upsert};

/// The MAVLink camera protocol's commands, as the camera plugin sends them
/// (S145, ADR-0017 V20): every camera (`param1` 0) the vehicle drives.
/// `MAV_CMD_IMAGE_START_CAPTURE`: no interval, one image.
pub(crate) const IMAGE_START_CAPTURE: u16 = 2000;
/// `MAV_CMD_VIDEO_START_CAPTURE`, every stream.
const VIDEO_START_CAPTURE: u16 = 2500;
/// `MAV_CMD_VIDEO_STOP_CAPTURE`, every stream.
const VIDEO_STOP_CAPTURE: u16 = 2501;

/// A recording the uploader may cut into segments for the account (S83).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolVideo {
    pub id: String,
    pub started_at_us: u64,
    /// When it ended; still recording without.
    pub ended_at_us: Option<u64>,
    /// Its file in the media folder (S150).
    pub file_name: String,
    /// Whether its segments go now: once it ended, or while it records
    /// with "Upload video while flying" on.
    pub cut: bool,
}

/// `m:ss` into a recording.
fn position(us: u64) -> String {
    let s = us / 1_000_000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// While a recording grows, its size and length are journaled this often.
const JOURNAL_PROGRESS_US: u64 = 10_000_000;
/// How old a photo or ended recording is before a media folder read
/// without its file removes it: one read just before it was saved does not.
const MEDIA_SETTLE_US: u64 = 5_000_000;

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
        identity: None,
    }
}

impl Engine {
    /// Sends one of the camera protocol's commands to a vehicle, as a
    /// receipt from UXDuck (S145): its id, or none while the vehicle is not
    /// heard or its link is read-only.
    pub(crate) fn tell_vehicle_camera(
        &mut self,
        vehicle_id: &str,
        command: u16,
        params: Vec<f32>,
    ) -> Option<String> {
        use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
        use uxduck_core::domain::link::LinkPolicy;
        use uxduck_core::domain::vehicle::Presence;

        let vehicle = self.state.vehicles.get(vehicle_id)?;
        if vehicle.presence != Presence::Online
            || self
                .state
                .links
                .get(&vehicle.link_id)
                .is_none_or(|l| l.policy == LinkPolicy::Read)
        {
            return None;
        }
        let command = Command::Long { command, params };
        let receipt = Receipt {
            id: format!("rc{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            sent_without_recent_heartbeat: false,
            forced: false,
            attempts: 0,
            client_id: actor().client_id,
            operator_id: None,
            remote: None,
            created_at_us: self.clock.now_us(),
            completed_at_us: None,
            version: 1,
        };
        let id = receipt.id.clone();
        let vehicle_id = vehicle.id.clone();
        self.commit_as(actor(), Change::ReceiptUpserted { receipt });
        self.links.command(CommandOrder {
            receipt_id: id.clone(),
            vehicle_id,
            command,
            force: false,
        });
        Some(id)
    }

    /// Starts and stops the vehicle's own camera recording with the intent
    /// to record (S145): by hand or while armed whether or not the picture
    /// arrives, since the camera records what the downlink loses; "always"
    /// while the camera sends, as UXDuck's recording does.
    fn review_vehicle_camera(&mut self, source: &VideoSource) {
        let wanted = source.vehicle_camera
            && match self.recording_intent(source) {
                Some(RecordingTrigger::Always) => source.state == VideoState::Receiving,
                Some(_) => true,
                None => false,
            };
        let running = self.video.camera_recording.get(&source.id).cloned();
        match (wanted, running, source.vehicle_id.clone()) {
            (true, None, Some(vehicle_id)) => {
                if let Some(receipt) =
                    self.tell_vehicle_camera(&vehicle_id, VIDEO_START_CAPTURE, vec![0.0, 0.0])
                {
                    self.video
                        .camera_recording
                        .insert(source.id.clone(), (vehicle_id, receipt));
                }
            }
            (false, Some((vehicle_id, _)), _) => {
                self.video.camera_recording.remove(&source.id);
                self.tell_vehicle_camera(&vehicle_id, VIDEO_STOP_CAPTURE, vec![0.0]);
            }
            _ => {}
        }
    }

    /// Why a camera should be recording now, if it should.
    pub(crate) fn recording_intent(&self, source: &VideoSource) -> Option<RecordingTrigger> {
        if let Some(on) = self.video.by_hand.get(&source.id) {
            return on.then_some(RecordingTrigger::ByHand);
        }
        // Left out of the others: only its own Record starts it (S153).
        if source.record_alone {
            return None;
        }
        match self.state.settings.video_recording {
            RecordPolicy::ByHand => None,
            RecordPolicy::Always => Some(RecordingTrigger::Always),
            RecordPolicy::WhileArmed => source
                .vehicle_id
                .as_ref()
                .and_then(|id| self.state.vehicles.get(id))
                .is_some_and(|v| v.armed)
                .then_some(RecordingTrigger::Armed),
        }
    }

    /// Starts and stops recordings to match the policy, what was asked by
    /// hand, and which cameras are sending.
    pub(crate) fn review_recordings(&mut self) {
        if self.video.port.is_none() {
            return;
        }
        self.review_video_demand();
        let sources: Vec<VideoSource> = self.state.video_sources.values().cloned().collect();
        for source in sources {
            self.review_vehicle_camera(&source);
            let wanted = (source.state == VideoState::Receiving)
                .then(|| self.recording_intent(&source))
                .flatten();
            let active = self.video.recording.get(&source.id).cloned();
            match (wanted, active) {
                (Some(trigger), None) => self.start_recording(&source, trigger),
                (None, Some(recording_id)) => {
                    self.video.recording.remove(&source.id);
                    if let Some(port) = self.video.port.as_mut() {
                        port.stop_recording(&recording_id);
                    }
                }
                _ => {}
            }
        }
    }

    fn start_recording(&mut self, source: &VideoSource, trigger: RecordingTrigger) {
        let mut recording = VideoRecording {
            id: format!("vr{}", &self.random.token()[..16]),
            source_id: source.id.clone(),
            vehicle_id: source.vehicle_id.clone(),
            started_at_us: self.clock.now_us(),
            ended_at_us: None,
            duration_us: 0,
            bytes: 0,
            codec: source.codec.clone(),
            width: source.width,
            height: source.height,
            state: RecordingState::Recording,
            detail: None,
            trigger,
            capture: source.capture.clone(),
            file: source.file.clone(),
            camera_receipt_id: self
                .video
                .camera_recording
                .get(&source.id)
                .map(|(_, receipt)| receipt.clone()),
            sound: false,
            file_name: None,
            version: 1,
        };
        self.video
            .recording
            .insert(source.id.clone(), recording.id.clone());
        let sound = source.audio.as_ref().is_some_and(|a| a.recorded);
        // Named as photos are: the local time, the vehicle and the camera (S150).
        let vehicle = source
            .vehicle_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .map(|v| v.name.clone());
        let who = crate::photos::file_who(vehicle.as_deref(), &source.name);
        if let Some(port) = self.video.port.as_mut() {
            let at = recording.started_at_us;
            let stem = format!(
                "{} {}",
                uxduck_media::photo::Civil::at(at, port.utc_offset_minutes(at)).file_stem(),
                who
            );
            recording.file_name =
                Some(port.start_recording(&recording.id, &source.id, sound, &stem));
        }
        self.video
            .progress_journaled
            .insert(recording.id.clone(), recording.started_at_us);
        self.commit_as(actor(), Change::VideoRecordingUpserted { recording });
    }

    /// How a recording is growing: journaled now and then, so a crash
    /// loses little of what the list says.
    pub fn video_recording_progress(&mut self, recording_id: &str, bytes: u64, duration_us: u64) {
        let now = self.clock.now_us();
        let due = self
            .video
            .progress_journaled
            .get(recording_id)
            .is_none_or(|at| now.saturating_sub(*at) >= JOURNAL_PROGRESS_US);
        let Some(mut recording) = self.state.video_recordings.get(recording_id).cloned() else {
            return;
        };
        if !due || recording.state != RecordingState::Recording {
            return;
        }
        recording.bytes = bytes;
        recording.duration_us = duration_us;
        recording.version += 1;
        self.video
            .progress_journaled
            .insert(recording_id.to_owned(), now);
        self.commit_as(actor(), Change::VideoRecordingUpserted { recording });
    }

    /// The daemon closed a recording's file; `sound` says it holds the
    /// camera's sound too (S147).
    pub fn video_recording_ended(
        &mut self,
        recording_id: &str,
        bytes: u64,
        duration_us: u64,
        error: Option<String>,
        sound: bool,
    ) {
        let Some(mut recording) = self.state.video_recordings.get(recording_id).cloned() else {
            return;
        };
        self.video.progress_journaled.remove(recording_id);
        self.video.recording.retain(|_, id| id != recording_id);
        recording.bytes = bytes;
        recording.duration_us = duration_us;
        recording.sound = sound;
        recording.ended_at_us = Some(self.clock.now_us());
        (recording.state, recording.detail) = match error {
            Some(error) => (RecordingState::Failed, Some(error)),
            None if duration_us == 0 => (
                RecordingState::Failed,
                Some("No keyframe arrived before it stopped".into()),
            ),
            None => (RecordingState::Done, None),
        };
        recording.version += 1;
        self.commit_as(actor(), Change::VideoRecordingUpserted { recording });
    }

    /// A recording's file went while it was being written: deleted or
    /// moved out of the media folder (S151). It leaves the list, and the
    /// camera stops recording as though Stop was pressed, so deleting a
    /// recording is never followed by a new one.
    pub fn video_recording_removed(&mut self, recording_id: &str) {
        let Some(recording) = self.state.video_recordings.get(recording_id).cloned() else {
            return;
        };
        self.video.progress_journaled.remove(recording_id);
        self.video.recording.retain(|_, id| id != recording_id);
        self.video
            .by_hand
            .insert(recording.source_id.clone(), false);
        if let Some(port) = self.video.port.as_mut() {
            port.stop_recording(recording_id);
        }
        self.forget(recording);
        self.review_recordings();
    }

    /// Photos and recordings whose files are no longer in the media
    /// folder, deleted or moved outside UXDuck (S151), leave the list.
    /// `present` is the folder's file names, read just now; rows made in
    /// the last few seconds, and recordings still being written, stay.
    /// Returns the recordings that left, for their playback copies.
    pub fn forget_missing_media(
        &mut self,
        present: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let now = self.clock.now_us();
        let settled = |at_us: u64| at_us.saturating_add(MEDIA_SETTLE_US) < now;
        let recordings: Vec<String> = self
            .state
            .video_recordings
            .values()
            .filter(|r| r.state != RecordingState::Recording)
            .filter(|r| settled(r.ended_at_us.unwrap_or(r.started_at_us)))
            .filter(|r| !present.contains(&r.file()))
            .map(|r| r.id.clone())
            .collect();
        let photos: Vec<String> = self
            .state
            .video_photos
            .values()
            .filter(|p| settled(p.taken_at_us) && !present.contains(&p.file_name))
            .map(|p| p.id.clone())
            .collect();
        for id in &recordings {
            if let Some(recording) = self.state.video_recordings.get(id).cloned() {
                self.forget(recording);
            }
        }
        for id in photos {
            self.commit_as(actor(), Change::VideoPhotoRemoved { id });
        }
        recordings
    }

    fn forget(&mut self, recording: VideoRecording) {
        self.commit_as(
            actor(),
            Change::VideoRecordingRemoved {
                id: recording.id.clone(),
            },
        );
        self.video.forgotten.insert(recording.id.clone(), recording);
    }

    /// The recordings to store in the account (S83, ADR-0003 C17): none
    /// with "Store flights and recordings in my account" off.
    #[must_use]
    pub fn video_for_spool(&self) -> Vec<SpoolVideo> {
        let settings = &self.state.settings;
        if !settings.store_flights {
            return Vec::new();
        }
        self.state
            .video_recordings
            .values()
            .filter(|r| r.bytes > 0 || r.state == RecordingState::Recording)
            .map(|r| {
                let ended = r.state != RecordingState::Recording;
                SpoolVideo {
                    id: r.id.clone(),
                    file_name: r.file(),
                    started_at_us: r.started_at_us,
                    ended_at_us: r.ended_at_us.filter(|_| ended),
                    cut: ended || settings.upload_video_while_flying,
                }
            })
            .collect()
    }

    /// Part of a recording went before it reached the account: journaled
    /// with its exact range, and told (ADR-0003 "Eviction").
    pub fn video_lost(&mut self, recording_id: String, from_us: u64, to_us: u64, evicted: bool) {
        let recording = self
            .state
            .video_recordings
            .get(&recording_id)
            .or_else(|| self.video.forgotten.get(&recording_id))
            .cloned();
        self.commit_as(
            actor(),
            Change::VideoLost {
                recording: recording_id,
                from_us,
                to_us,
                evicted,
            },
        );
        let Some(recording) = recording else {
            return;
        };
        let from = recording
            .vehicle_id
            .as_ref()
            .and_then(|id| self.state.vehicles.get(id))
            .map(|v| format!(" from {}", v.name))
            .unwrap_or_default();
        let range = format!(
            "{} to {}",
            position(from_us.saturating_sub(recording.started_at_us)),
            position(to_us.saturating_sub(recording.started_at_us))
        );
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::VideoSource {
                id: recording.source_id.clone(),
            },
            title: if evicted {
                format!("Part of a recording{from} wasn't uploaded")
            } else {
                format!("A recording{from} was deleted before {range} reached your account")
            },
            body: Some(if evicted {
                format!(
                    "UXDuck deleted it to free disk space before {range} reached your account. \
                     The rest of the recording, and the flight's log, are in your account."
                )
            } else {
                "That part is gone. The rest of the recording, and the flight's log, are in your account."
                    .into()
            }),
            actions: Vec::new(),
            condition: None,
            report: Some("recording-lost".into()),
        });
    }

    /// At start: recordings the daemon was writing when it stopped end at
    /// their last whole fragment.
    pub(crate) fn recover_recordings(&mut self) {
        let interrupted: Vec<VideoRecording> = self
            .state
            .video_recordings
            .values()
            .filter(|r| r.state == RecordingState::Recording)
            .cloned()
            .collect();
        for mut recording in interrupted {
            let file = recording.file();
            let recovered = self
                .video
                .port
                .as_mut()
                .and_then(|port| port.recover_recording(&file));
            match recovered {
                Some((bytes, duration_us)) if duration_us > 0 => {
                    recording.state = RecordingState::Recovered;
                    recording.detail = Some(
                        "UXDuck stopped while recording. It ends at the last whole second or two"
                            .into(),
                    );
                    recording.bytes = bytes;
                    recording.duration_us = duration_us;
                }
                _ => {
                    recording.state = RecordingState::Failed;
                    recording.detail =
                        Some("UXDuck stopped before a whole picture was saved".into());
                }
            }
            recording.ended_at_us = Some(recording.started_at_us + recording.duration_us);
            recording.version += 1;
            self.commit_as(actor(), Change::VideoRecordingUpserted { recording });
        }
    }

    fn video_record(&mut self, params: &VideoRecordParams) -> Result<(), ApiError> {
        let source = self
            .state
            .video_sources
            .get(&params.source_id)
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no video source {}", params.source_id),
                )
            })?;
        if self.video.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video needs UXDuck Desktop",
            ));
        }
        if params.on && source.capture.as_ref().is_some_and(|c| !c.enabled) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Resume this camera before recording.",
            ));
        }
        self.video
            .by_hand
            .insert(params.source_id.clone(), params.on);
        self.review_recordings();
        Ok(())
    }

    /// Record all and Stop all (S153). Record all asks each camera that
    /// records with the others to record, as its own Record would, unless
    /// it is paused or can't be played. One idle or away records once its
    /// video arrives, as after Record (UXDuck doesn't listen to a camera
    /// nobody watches, so can't tell whether it sends); one recording
    /// already goes on as it is. Stop all stops each camera recording or
    /// asked to, as its own Stop would.
    fn video_record_all(&mut self, params: &VideoRecordAllParams) -> Result<(), ApiError> {
        if self.video.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video needs UXDuck Desktop",
            ));
        }
        let chosen: Vec<String> = self
            .state
            .video_sources
            .values()
            .filter(|s| {
                params
                    .vehicle_id
                    .as_deref()
                    .is_none_or(|id| s.vehicle_id.as_deref() == Some(id))
            })
            .filter(|s| {
                if params.on {
                    s.state != VideoState::Unsupported
                        && !s.record_alone
                        && s.capture.as_ref().is_none_or(|c| c.enabled)
                } else {
                    s.recording_requested || self.video.recording.contains_key(&s.id)
                }
            })
            .map(|s| s.id.clone())
            .collect();
        if params.on && chosen.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "No camera records with the others.",
            ));
        }
        for id in chosen {
            self.video.by_hand.insert(id, params.on);
        }
        self.review_recordings();
        Ok(())
    }

    /// "Record with the others" (S153): set by hand, kept with the camera.
    fn video_record_with_others(
        &mut self,
        params: &VideoRecordWithOthersParams,
    ) -> Result<VideoSource, ApiError> {
        let mut source = self
            .state
            .video_sources
            .get(&params.source_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no video source {}", params.source_id),
                )
            })?;
        source.record_alone = !params.on;
        source.version += 1;
        self.commit_as(
            actor(),
            Change::VideoSourceUpserted {
                source: source.clone(),
            },
        );
        self.review_recordings();
        Ok(source)
    }

    /// A vehicle arms: a Stop pressed for its cameras lasts until now, so
    /// "while armed" and "always" record its next flight (S153). A Record
    /// pressed stays.
    pub(crate) fn video_vehicle_arming(&mut self, vehicle_id: &str) {
        let stopped: Vec<String> = self
            .state
            .video_sources
            .values()
            .filter(|s| s.vehicle_id.as_deref() == Some(vehicle_id))
            .filter(|s| self.video.by_hand.get(&s.id) == Some(&false))
            .map(|s| s.id.clone())
            .collect();
        for id in stopped {
            self.video.by_hand.remove(&id);
        }
    }

    fn video_playback(
        &mut self,
        params: &VideoPlaybackParams,
    ) -> Result<VideoPlaybackResult, ApiError> {
        let recording = self
            .state
            .video_recordings
            .get(&params.recording_id)
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no recording {}", params.recording_id),
                )
            })?;
        if recording.state == RecordingState::Failed {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "this recording failed. There is nothing to play",
            ));
        }
        let token = format!("vp{}", &self.random.token()[..32]);
        let Some(port) = self.video.port.as_mut() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "video needs UXDuck Desktop",
            ));
        };
        let file = recording.file();
        port.allow_playback(&token, &recording.id, &file);
        let kept_as_ts = std::path::Path::new(&file)
            .extension()
            .is_some_and(|e| e == "ts");
        Ok(VideoPlaybackResult {
            path: format!("/recordings/{}.mp4?token={token}", recording.id),
            original_path: kept_as_ts
                .then(|| format!("/recordings/{}.ts?token={token}", recording.id)),
        })
    }
}

pub(crate) fn record(engine: &mut Engine, params: &VideoRecordParams) -> Result<Value, ApiError> {
    engine.video_record(params)?;
    to_value(&Empty {})
}

pub(crate) fn record_all(
    engine: &mut Engine,
    params: &VideoRecordAllParams,
) -> Result<Value, ApiError> {
    engine.video_record_all(params)?;
    to_value(&Empty {})
}

pub(crate) fn record_with_others(
    engine: &mut Engine,
    params: &VideoRecordWithOthersParams,
) -> Result<VideoSource, ApiError> {
    engine.video_record_with_others(params)
}

pub(crate) fn playback(
    engine: &mut Engine,
    params: &VideoPlaybackParams,
) -> Result<Value, ApiError> {
    to_value(&engine.video_playback(params)?)
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::VideoRecordingUpserted { recording } => vec![EntityChange {
            video_recording: Some(recording.into()),
            ..upsert(EntityType::VideoRecording, &recording.id)
        }],
        Change::VideoRecordingRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::VideoRecording, id)
        }],
        _ => Vec::new(),
    }
}

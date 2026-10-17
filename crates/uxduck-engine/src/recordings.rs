//! Recording cameras (ADR-0017 V16, S40): which camera records when, by the
//! policy and by hand, the recordings as entities, recovery after a crash,
//! and playback tokens. The daemon writes the files.

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::dto::video::{VideoPlaybackParams, VideoPlaybackResult, VideoRecordParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::video::{
    RecordPolicy, RecordingState, RecordingTrigger, VideoRecording, VideoSource, VideoState,
};
use uxduck_core::events::{Actor, Change};

use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{Engine, upsert};

/// A recording the uploader may cut into segments for the account (S83).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolVideo {
    pub id: String,
    pub started_at_us: u64,
    /// When it ended; still recording without.
    pub ended_at_us: Option<u64>,
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

fn actor() -> Actor {
    Actor {
        client_id: "video".into(),
        operator_id: None,
    }
}

impl Engine {
    /// Why a camera should be recording now, if it should.
    fn wanted(&self, source: &VideoSource) -> Option<RecordingTrigger> {
        if source.state != VideoState::Receiving {
            return None;
        }
        if let Some(on) = self.video.by_hand.get(&source.id) {
            return on.then_some(RecordingTrigger::ByHand);
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
        let sources: Vec<VideoSource> = self.state.video_sources.values().cloned().collect();
        for source in sources {
            let wanted = self.wanted(&source);
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
        let recording = VideoRecording {
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
            version: 1,
        };
        self.video
            .recording
            .insert(source.id.clone(), recording.id.clone());
        if let Some(port) = self.video.port.as_mut() {
            port.start_recording(&recording.id, &source.id);
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

    /// The daemon closed a recording's file.
    pub fn video_recording_ended(
        &mut self,
        recording_id: &str,
        bytes: u64,
        duration_us: u64,
        error: Option<String>,
    ) {
        let Some(mut recording) = self.state.video_recordings.get(recording_id).cloned() else {
            return;
        };
        self.video.progress_journaled.remove(recording_id);
        self.video.recording.retain(|_, id| id != recording_id);
        recording.bytes = bytes;
        recording.duration_us = duration_us;
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
        let recording = self.state.video_recordings.get(&recording_id).cloned();
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
            let recovered = self
                .video
                .port
                .as_mut()
                .and_then(|port| port.recover_recording(&recording.id));
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
        if params.on && source.state != VideoState::Receiving {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the camera is not sending. Nothing to record",
            ));
        }
        self.video
            .by_hand
            .insert(params.source_id.clone(), params.on);
        self.review_recordings();
        Ok(())
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
        port.allow_playback(&token, &recording.id);
        Ok(VideoPlaybackResult {
            path: format!("/recordings/{}.mp4?token={token}", recording.id),
        })
    }
}

pub(crate) fn record(engine: &mut Engine, params: &VideoRecordParams) -> Result<Value, ApiError> {
    engine.video_record(params)?;
    to_value(&Empty {})
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
        _ => Vec::new(),
    }
}

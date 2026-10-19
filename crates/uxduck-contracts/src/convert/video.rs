use uxduck_core::domain::video as core;

use crate::dto::video as dto;

impl From<core::VideoContainer> for dto::VideoContainer {
    fn from(v: core::VideoContainer) -> Self {
        match v {
            core::VideoContainer::Ts => Self::Ts,
            core::VideoContainer::RtpTs => Self::RtpTs,
            core::VideoContainer::Rtp => Self::Rtp,
            core::VideoContainer::RtpH264 => Self::RtpH264,
            core::VideoContainer::RtpH265 => Self::RtpH265,
        }
    }
}

impl From<core::VideoState> for dto::VideoState {
    fn from(v: core::VideoState) -> Self {
        match v {
            core::VideoState::Receiving => Self::Receiving,
            core::VideoState::Stopped => Self::Stopped,
            core::VideoState::Unsupported => Self::Unsupported,
        }
    }
}

impl From<&core::VideoCounters> for dto::VideoCounters {
    fn from(v: &core::VideoCounters) -> Self {
        Self {
            bitrate_bps: v.bitrate_bps,
            datagrams: v.datagrams,
            pictures: v.pictures,
            keyframes: v.keyframes,
            lost: v.lost,
            damaged: v.damaged,
            pes_length_mismatches: v.pes_length_mismatches,
            pes_length_max_excess: v.pes_length_max_excess,
            nals_dropped: v.nals_dropped,
            klv: v.klv,
            klv_bad: v.klv_bad,
        }
    }
}

impl From<&core::VideoTrack> for dto::VideoTrack {
    fn from(v: &core::VideoTrack) -> Self {
        Self {
            number: v.number,
            codec: v.codec.clone(),
            width: v.width,
            height: v.height,
            frame_rate: v.frame_rate,
            playing: v.playing,
        }
    }
}

impl From<&core::VideoSource> for dto::VideoSource {
    fn from(v: &core::VideoSource) -> Self {
        Self {
            recording_requested: v.recording_requested,
            id: v.id.clone(),
            name: v.name.clone(),
            named_by_hand: v.named_by_hand,
            port: v.port,
            sender: v.sender.clone(),
            container: v.container.into(),
            codec: v.codec.clone(),
            width: v.width,
            height: v.height,
            frame_rate: v.frame_rate,
            state: v.state.into(),
            state_detail: v.state_detail.clone(),
            vehicle_id: v.vehicle_id.clone(),
            assigned_by_hand: v.assigned_by_hand,
            vehicle_camera: v.vehicle_camera,
            record_alone: v.record_alone,
            klv: v.klv,
            url: v.url.as_deref().map(core::redact),
            added_by_hand: v.added_by_hand,
            capture: v.capture.as_ref().map(Into::into),
            tracks: v.tracks.iter().map(Into::into).collect(),
            audio: v.audio.as_ref().map(|a| dto::VideoAudio {
                name: a.name.clone(),
                recorded: a.recorded,
            }),
            counters: (&v.counters).into(),
            first_seen_us: v.first_seen_us,
            changed_at_us: v.changed_at_us,
            version: v.version,
        }
    }
}

impl From<core::RecordPolicy> for dto::RecordPolicy {
    fn from(v: core::RecordPolicy) -> Self {
        match v {
            core::RecordPolicy::ByHand => Self::ByHand,
            core::RecordPolicy::WhileArmed => Self::WhileArmed,
            core::RecordPolicy::Always => Self::Always,
        }
    }
}

impl From<dto::RecordPolicy> for core::RecordPolicy {
    fn from(v: dto::RecordPolicy) -> Self {
        match v {
            dto::RecordPolicy::ByHand => Self::ByHand,
            dto::RecordPolicy::WhileArmed => Self::WhileArmed,
            dto::RecordPolicy::Always => Self::Always,
        }
    }
}

impl From<&core::VideoRecording> for dto::VideoRecording {
    fn from(v: &core::VideoRecording) -> Self {
        Self {
            id: v.id.clone(),
            source_id: v.source_id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            started_at_us: v.started_at_us,
            ended_at_us: v.ended_at_us,
            duration_us: v.duration_us,
            bytes: v.bytes,
            codec: v.codec.clone(),
            width: v.width,
            height: v.height,
            state: match v.state {
                core::RecordingState::Recording => dto::RecordingState::Recording,
                core::RecordingState::Done => dto::RecordingState::Done,
                core::RecordingState::Recovered => dto::RecordingState::Recovered,
                core::RecordingState::Failed => dto::RecordingState::Failed,
            },
            detail: v.detail.clone(),
            capture: v.capture.as_ref().map(Into::into),
            camera_receipt_id: v.camera_receipt_id.clone(),
            sound: v.sound,
            file_name: v.file_name.clone(),
            trigger: match v.trigger {
                core::RecordingTrigger::ByHand => dto::RecordingTrigger::ByHand,
                core::RecordingTrigger::Armed => dto::RecordingTrigger::Armed,
                core::RecordingTrigger::Always => dto::RecordingTrigger::Always,
            },
            version: v.version,
        }
    }
}

impl From<&core::VideoPhoto> for dto::VideoPhoto {
    fn from(v: &core::VideoPhoto) -> Self {
        Self {
            id: v.id.clone(),
            source_id: v.source_id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            taken_at_us: v.taken_at_us,
            file_name: v.file_name.clone(),
            bytes: v.bytes,
            width: v.width,
            height: v.height,
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            altitude_m: v.altitude_m,
            heading_deg: v.heading_deg,
            camera_receipt_id: v.camera_receipt_id.clone(),
            version: v.version,
        }
    }
}

impl From<&core::CaptureInput> for dto::CaptureInput {
    fn from(v: &core::CaptureInput) -> Self {
        Self {
            device_id: v.device_id.clone(),
            name: v.name.clone(),
            enabled: v.enabled,
            standard: match v.standard {
                core::CaptureStandard::Auto => dto::CaptureStandard::Auto,
                core::CaptureStandard::Pal => dto::CaptureStandard::Pal,
                core::CaptureStandard::Ntsc => dto::CaptureStandard::Ntsc,
            },
            width: v.width,
            height: v.height,
            frame_rate: v.frame_rate,
            deinterlace: v.deinterlace,
            record_sound: v.record_sound,
            sound_input: v.sound_input.as_ref().map(|s| dto::SoundInput {
                device_id: s.device_id.clone(),
                name: s.name.clone(),
            }),
        }
    }
}
impl From<dto::CaptureInput> for core::CaptureInput {
    fn from(v: dto::CaptureInput) -> Self {
        Self {
            device_id: v.device_id,
            name: v.name,
            enabled: v.enabled,
            standard: match v.standard {
                dto::CaptureStandard::Auto => core::CaptureStandard::Auto,
                dto::CaptureStandard::Pal => core::CaptureStandard::Pal,
                dto::CaptureStandard::Ntsc => core::CaptureStandard::Ntsc,
            },
            width: v.width,
            height: v.height,
            frame_rate: v.frame_rate,
            deinterlace: v.deinterlace,
            record_sound: v.record_sound,
            sound_input: v.sound_input.map(|s| core::SoundInput {
                device_id: s.device_id,
                name: s.name,
            }),
        }
    }
}

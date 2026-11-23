use uxduck_core::domain::companion_cameras as cc;

use crate::dto::companion_cameras as dto;

fn picture(v: &cc::PictureSettings) -> dto::PictureSettings {
    dto::PictureSettings {
        width: v.width,
        height: v.height,
        fps: v.fps,
        mode: v.mode.clone(),
        idr_period: v.idr_period,
        bitrate: v.bitrate,
    }
}

impl From<cc::PicturePreset> for dto::PicturePreset {
    fn from(v: cc::PicturePreset) -> Self {
        match v {
            cc::PicturePreset::LowLatency => Self::LowLatency,
            cc::PicturePreset::Detail => Self::Detail,
            cc::PicturePreset::Fastest => Self::Fastest,
            cc::PicturePreset::DataSaver => Self::DataSaver,
        }
    }
}

impl From<&dto::PictureSettings> for cc::PictureSettings {
    fn from(v: &dto::PictureSettings) -> Self {
        Self {
            width: v.width,
            height: v.height,
            fps: v.fps,
            mode: v.mode.clone(),
            idr_period: v.idr_period,
            bitrate: v.bitrate,
        }
    }
}

/// A path as clients see it: with the preset it matches, the presets it can
/// take and why it cannot be adjusted, worked out here so every client
/// says the same.
fn camera_path(v: &cc::CompanionCameraPath) -> dto::CompanionCameraPath {
    let refused = v.adjustable().err();
    dto::CompanionCameraPath {
        name: v.name.clone(),
        source: v.source.clone(),
        camera_id: v.camera_id,
        sensor: v.sensor.clone(),
        settings: v.settings.as_ref().map(picture),
        preset: v
            .settings
            .as_ref()
            .and_then(cc::PicturePreset::matching)
            .map(Into::into),
        presets: if refused.is_some() {
            Vec::new()
        } else {
            cc::PicturePreset::offered(v.sensor.as_deref())
                .into_iter()
                .map(|preset| dto::PresetSettings {
                    preset: preset.into(),
                    settings: picture(&preset.settings()),
                })
                .collect()
        },
        refused,
        set_by_uxduck: v.set_by_uxduck,
        on_demand: v.on_demand,
        ready: v.ready,
        readers: v.readers,
        bytes: v.bytes,
        record: v.record.as_ref().map(|r| dto::VehicleRecording {
            path: r.path.clone(),
            mode: r.mode.into(),
            settings: r.settings.as_ref().map(picture),
        }),
        microphone: v.microphone.as_ref().map(|m| dto::CompanionMicrophone {
            choice: m.choice.clone(),
            device: m.device.clone(),
        }),
    }
}

impl From<cc::VehicleRecordMode> for dto::VehicleRecordMode {
    fn from(v: cc::VehicleRecordMode) -> Self {
        match v {
            cc::VehicleRecordMode::Armed => Self::Armed,
            cc::VehicleRecordMode::Always => Self::Always,
            cc::VehicleRecordMode::Off => Self::Off,
        }
    }
}

impl From<&cc::CompanionCameras> for dto::CompanionCameras {
    fn from(v: &cc::CompanionCameras) -> Self {
        Self {
            unit: v.unit.clone(),
            config: v.config.clone(),
            api: v.api.clone(),
            drop_in: v.drop_in.clone(),
            linger: v.linger.as_ref().map(|l| dto::CameraLinger {
                user: l.user.clone(),
                lingering: l.lingering,
                remove_ipc: l.remove_ipc,
                at_risk: l.at_risk(),
            }),
            paths: v.paths.iter().map(camera_path).collect(),
            watchdog: dto::CameraWatchdog {
                installed: v.watchdog.installed,
                running: v.watchdog.running,
                gave_up: v.watchdog.gave_up,
                restarts: v
                    .watchdog
                    .restarts
                    .iter()
                    .map(|r| dto::WatchdogRestart {
                        at_us: r.at_us,
                        path: r.path.clone(),
                    })
                    .collect(),
            },
            applied: v.applied.as_ref().map(|a| dto::PictureApplied {
                path: a.path.clone(),
                outcome: match a.outcome {
                    cc::PictureOutcome::Applied => dto::PictureOutcome::Applied,
                    cc::PictureOutcome::RolledBack => dto::PictureOutcome::RolledBack,
                    cc::PictureOutcome::RollbackFailed => dto::PictureOutcome::RollbackFailed,
                },
                failed: a.failed.clone(),
                log: a.log.clone(),
            }),
            problem: v.problem.clone(),
            record_problem: v.record_problem.clone(),
            microphones: v
                .microphones
                .iter()
                .map(|m| dto::CompanionMicrophoneDevice {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    card: m.card.clone(),
                    usb: m.usb,
                })
                .collect(),
            sound_problem: v.sound_problem.clone(),
        }
    }
}

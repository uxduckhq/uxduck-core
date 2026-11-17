use uxduck_core::domain::camera as core;
use uxduck_core::domain::video::redact;

use crate::dto::camera as dto;

impl From<core::CameraCapability> for dto::CameraCapability {
    fn from(v: core::CameraCapability) -> Self {
        match v {
            core::CameraCapability::CaptureVideo => Self::CaptureVideo,
            core::CameraCapability::CaptureImage => Self::CaptureImage,
            core::CameraCapability::Modes => Self::Modes,
            core::CameraCapability::ImageInVideoMode => Self::ImageInVideoMode,
            core::CameraCapability::VideoInImageMode => Self::VideoInImageMode,
            core::CameraCapability::ImageSurveyMode => Self::ImageSurveyMode,
            core::CameraCapability::BasicZoom => Self::BasicZoom,
            core::CameraCapability::BasicFocus => Self::BasicFocus,
            core::CameraCapability::VideoStream => Self::VideoStream,
            core::CameraCapability::TrackingPoint => Self::TrackingPoint,
            core::CameraCapability::TrackingRectangle => Self::TrackingRectangle,
            core::CameraCapability::TrackingGeoStatus => Self::TrackingGeoStatus,
            core::CameraCapability::ThermalRange => Self::ThermalRange,
        }
    }
}

/// A value MAVLink carries as an `f32`, as the shortest number that is the
/// same `f32`: 13.2, not 13.199999809265137.
fn widen(value: f32) -> f64 {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(value))
}

impl From<&core::CameraStream> for dto::CameraStream {
    fn from(v: &core::CameraStream) -> Self {
        Self {
            stream_id: v.stream_id,
            name: Some(v.name.clone()).filter(|name| !name.is_empty()),
            uri: redact(v.uri.trim()),
            kind: match v.kind {
                0 => dto::CameraStreamKind::Rtsp,
                1 => dto::CameraStreamKind::RtpUdp,
                2 => dto::CameraStreamKind::MpegTsTcp,
                3 => dto::CameraStreamKind::MpegTsUdp,
                _ => dto::CameraStreamKind::Other,
            },
            encoding: match v.encoding {
                1 => Some(dto::CameraStreamEncoding::H264),
                2 => Some(dto::CameraStreamEncoding::H265),
                _ => None,
            },
            running: v.flags & 1 != 0,
            thermal: v.flags & 2 != 0,
            width: Some(v.width).filter(|px| *px > 0),
            height: Some(v.height).filter(|px| *px > 0),
            frame_rate_hz: Some(v.frame_rate_hz)
                .filter(|hz| hz.is_finite() && *hz > 0.0)
                .map(widen),
            bitrate_bps: Some(v.bitrate).filter(|bps| *bps > 0),
            rotation_deg: Some(v.rotation).filter(|deg| *deg > 0),
            hfov_deg: Some(v.hfov).filter(|deg| *deg > 0),
            source_id: v.source_id.clone(),
        }
    }
}

impl From<core::CameraMode> for dto::CameraMode {
    fn from(v: core::CameraMode) -> Self {
        match v {
            core::CameraMode::Photo => Self::Photo,
            core::CameraMode::Video => Self::Video,
            core::CameraMode::Survey => Self::Survey,
        }
    }
}

impl From<dto::CameraMode> for core::CameraMode {
    fn from(v: dto::CameraMode) -> Self {
        match v {
            dto::CameraMode::Photo => Self::Photo,
            dto::CameraMode::Video => Self::Video,
            dto::CameraMode::Survey => Self::Survey,
        }
    }
}

impl From<core::CameraSource> for dto::CameraSource {
    fn from(v: core::CameraSource) -> Self {
        match v {
            core::CameraSource::Daylight => Self::Daylight,
            core::CameraSource::Thermal => Self::Thermal,
        }
    }
}

impl From<dto::CameraSource> for core::CameraSource {
    fn from(v: dto::CameraSource) -> Self {
        match v {
            dto::CameraSource::Daylight => Self::Daylight,
            dto::CameraSource::Thermal => Self::Thermal,
        }
    }
}

impl From<core::CameraAction> for dto::CameraAction {
    fn from(v: core::CameraAction) -> Self {
        match v {
            core::CameraAction::SetMode { mode } => Self::SetMode { mode: mode.into() },
            core::CameraAction::TakePhoto => Self::TakePhoto,
            core::CameraAction::StartInterval { interval_s, count } => {
                Self::StartInterval { interval_s, count }
            }
            core::CameraAction::StopInterval => Self::StopInterval,
            core::CameraAction::StartRecording => Self::StartRecording,
            core::CameraAction::StopRecording => Self::StopRecording,
            core::CameraAction::ZoomStep { steps } => Self::ZoomStep { steps },
            core::CameraAction::ZoomTo { percent } => Self::ZoomTo { percent },
            core::CameraAction::FocusStep { steps } => Self::FocusStep { steps },
            core::CameraAction::FocusTo { percent } => Self::FocusTo { percent },
            core::CameraAction::FocusAuto => Self::FocusAuto,
            core::CameraAction::Format { storage_id } => Self::Format { storage_id },
            core::CameraAction::Reset => Self::Reset,
            // Its type is the definition file's to say, not the caller's.
            core::CameraAction::SetSetting { name, value, .. } => Self::SetSetting { name, value },
            core::CameraAction::SetSource { source } => Self::SetSource {
                source: source.into(),
            },
            core::CameraAction::TrackPoint { x, y } => Self::TrackPoint { x, y },
            core::CameraAction::TrackRectangle {
                left,
                top,
                right,
                bottom,
            } => Self::TrackRectangle {
                left,
                top,
                right,
                bottom,
            },
            core::CameraAction::StopTracking => Self::StopTracking,
        }
    }
}

impl From<dto::CameraAction> for core::CameraAction {
    fn from(v: dto::CameraAction) -> Self {
        match v {
            dto::CameraAction::SetMode { mode } => Self::SetMode { mode: mode.into() },
            dto::CameraAction::TakePhoto => Self::TakePhoto,
            dto::CameraAction::StartInterval { interval_s, count } => {
                Self::StartInterval { interval_s, count }
            }
            dto::CameraAction::StopInterval => Self::StopInterval,
            dto::CameraAction::StartRecording => Self::StartRecording,
            dto::CameraAction::StopRecording => Self::StopRecording,
            dto::CameraAction::ZoomStep { steps } => Self::ZoomStep { steps },
            dto::CameraAction::ZoomTo { percent } => Self::ZoomTo { percent },
            dto::CameraAction::FocusStep { steps } => Self::FocusStep { steps },
            dto::CameraAction::FocusTo { percent } => Self::FocusTo { percent },
            dto::CameraAction::FocusAuto => Self::FocusAuto,
            dto::CameraAction::Format { storage_id } => Self::Format { storage_id },
            dto::CameraAction::Reset => Self::Reset,
            dto::CameraAction::SetSetting { name, value } => Self::SetSetting {
                name,
                value,
                kind: None,
            },
            dto::CameraAction::SetSource { source } => Self::SetSource {
                source: source.into(),
            },
            dto::CameraAction::TrackPoint { x, y } => Self::TrackPoint { x, y },
            dto::CameraAction::TrackRectangle {
                left,
                top,
                right,
                bottom,
            } => Self::TrackRectangle {
                left,
                top,
                right,
                bottom,
            },
            dto::CameraAction::StopTracking => Self::StopTracking,
        }
    }
}

impl From<core::CameraSettingKind> for dto::CameraSettingKind {
    fn from(v: core::CameraSettingKind) -> Self {
        match v {
            core::CameraSettingKind::Bool => Self::Bool,
            core::CameraSettingKind::Uint8 => Self::Uint8,
            core::CameraSettingKind::Int8 => Self::Int8,
            core::CameraSettingKind::Uint16 => Self::Uint16,
            core::CameraSettingKind::Int16 => Self::Int16,
            core::CameraSettingKind::Uint32 => Self::Uint32,
            core::CameraSettingKind::Int32 => Self::Int32,
            core::CameraSettingKind::Uint64 => Self::Uint64,
            core::CameraSettingKind::Int64 => Self::Int64,
            core::CameraSettingKind::Float => Self::Float,
            core::CameraSettingKind::Double => Self::Double,
            core::CameraSettingKind::Custom => Self::Custom,
        }
    }
}

impl From<&core::CameraPhoto> for dto::CameraPhoto {
    fn from(v: &core::CameraPhoto) -> Self {
        Self {
            index: v.index,
            taken_at_us: v.taken_at_us,
            place: v.place.map(|place| dto::CameraPhotoPlace {
                latitude_deg: place.latitude_deg,
                longitude_deg: place.longitude_deg,
                altitude_amsl_m: place.altitude_amsl_m,
                relative_altitude_m: place.relative_altitude_m,
            }),
            ok: v.ok,
            file: Some(redact(v.file.trim())).filter(|file| !file.is_empty()),
        }
    }
}

impl From<&core::Camera> for dto::Camera {
    fn from(v: &core::Camera) -> Self {
        let said = v.information.as_ref();
        let words = |text: &String| Some(text.trim().to_owned()).filter(|text| !text.is_empty());
        Self {
            id: v.id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            component_id: v.component_id,
            device_id: v.device_id,
            present: v.present,
            identified: said.is_some(),
            vendor: said.and_then(|i| words(&i.vendor)),
            model: said.and_then(|i| words(&i.model)),
            firmware: said.and_then(core::CameraInformation::firmware),
            capabilities: said.map_or_else(Vec::new, |i| {
                core::CameraCapability::of(i.flags)
                    .into_iter()
                    .map(Into::into)
                    .collect()
            }),
            flags: said.map_or(0, |i| i.flags),
            video_only: said.is_some_and(core::CameraInformation::video_only),
            sensor: said
                .filter(|i| i.sensor_size_mm.is_some() || i.resolution_px.is_some())
                .map(|i| dto::CameraSensor {
                    width_mm: i.sensor_size_mm.map(|(width, _)| widen(width)),
                    height_mm: i.sensor_size_mm.map(|(_, height)| widen(height)),
                    width_px: i.resolution_px.map(|(width, _)| width),
                    height_px: i.resolution_px.map(|(_, height)| height),
                }),
            lens: said
                .filter(|i| i.focal_length_mm.is_some() || i.lens_id != 0)
                .map(|i| dto::CameraLens {
                    id: i.lens_id,
                    focal_length_mm: i.focal_length_mm.map(widen),
                }),
            definition: said
                .and_then(|i| words(&i.definition_uri).map(|uri| (uri, i.definition_version)))
                .map(|(uri, version)| dto::CameraDefinition {
                    uri: redact(&uri),
                    version,
                }),
            gimbal_device_id: said.map(|i| i.gimbal_device_id).filter(|id| *id != 0),
            streams: v.streams.iter().map(Into::into).collect(),
            settings: v.settings.map(|settings| dto::CameraSettings {
                mode: settings.mode.map(Into::into),
                zoom_pct: settings.zoom_pct.map(widen),
                focus_pct: settings.focus_pct.map(widen),
            }),
            capture: v.capture.map(|capture| dto::CameraCapture {
                interval_s: capture.interval_s.map(widen),
                recording: capture.recording,
                recording_since_us: capture.recording_since_us,
                photo_count: capture.photo_count,
                photos_lost: capture.photos_lost,
                available_mib: v.available_mib().map(widen),
                card_full: v.card_full(),
            }),
            storage: v.storage.as_ref().map(|storage| dto::CameraStorage {
                storage_id: storage.storage_id,
                count: storage.count,
                status: match storage.status {
                    core::CameraStorageStatus::Empty => dto::CameraStorageStatus::Empty,
                    core::CameraStorageStatus::Unformatted => dto::CameraStorageStatus::Unformatted,
                    core::CameraStorageStatus::Ready => dto::CameraStorageStatus::Ready,
                    core::CameraStorageStatus::NotSupported => {
                        dto::CameraStorageStatus::NotSupported
                    }
                },
                total_mib: widen(storage.total_mib),
                used_mib: widen(storage.used_mib),
                available_mib: widen(storage.available_mib),
                name: words(&storage.name),
            }),
            photos: v.photos.iter().map(Into::into).collect(),
            sources: v.sources.iter().copied().map(Into::into).collect(),
            version: v.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(information: Option<core::CameraInformation>) -> core::Camera {
        core::Camera {
            id: "v1/100".into(),
            vehicle_id: "v1".into(),
            component_id: 100,
            device_id: None,
            present: true,
            information,
            streams: Vec::new(),
            settings: None,
            capture: None,
            storage: None,
            photos: Vec::new(),
            sources: Vec::new(),
            version: 1,
        }
    }

    #[test]
    fn a_camera_not_yet_identified_says_nothing_it_does_not_know() {
        let wire = dto::Camera::from(&camera(None));
        assert!(!wire.identified);
        assert_eq!((wire.vendor, wire.model, wire.firmware), (None, None, None));
        assert!(wire.capabilities.is_empty());
        assert!(!wire.video_only);
        assert_eq!(
            (wire.sensor, wire.lens, wire.definition),
            (None, None, None)
        );
        assert_eq!(wire.gimbal_device_id, None);
    }

    #[test]
    fn an_identified_camera_names_what_it_can_do_and_hides_passwords() {
        let mut camera = camera(Some(core::CameraInformation {
            vendor: "UXDuck".into(),
            model: "SimCamera".into(),
            firmware_version: 0x0000_0001,
            focal_length_mm: Some(16.0),
            sensor_size_mm: Some((13.2, 8.8)),
            resolution_px: Some((5472, 3648)),
            lens_id: 0,
            flags: 1 | 2 | 256,
            definition_version: 3,
            definition_uri: "http://user:secret@10.0.0.2/camera.xml".into(),
            gimbal_device_id: 154,
        }));
        camera.streams.push(core::CameraStream {
            stream_id: 1,
            name: "Main".into(),
            uri: "rtsp://admin:secret@10.0.0.2:8554/main".into(),
            kind: 0,
            encoding: 1,
            flags: 1,
            frame_rate_hz: 30.0,
            width: 1280,
            height: 720,
            source_id: Some("cam-v1-100-1".into()),
            ..core::CameraStream::default()
        });
        let wire = dto::Camera::from(&camera);
        assert!(wire.identified);
        assert_eq!(wire.vendor.as_deref(), Some("UXDuck"));
        assert_eq!(wire.firmware.as_deref(), Some("1.0.0"));
        assert_eq!(
            wire.capabilities,
            [
                dto::CameraCapability::CaptureVideo,
                dto::CameraCapability::CaptureImage,
                dto::CameraCapability::VideoStream
            ]
        );
        assert_eq!(wire.gimbal_device_id, Some(154));
        assert_eq!(
            wire.sensor.as_ref().and_then(|sensor| sensor.width_mm),
            Some(13.2),
            "as the camera wrote it, not 13.199999809265137"
        );
        assert_eq!(
            wire.definition,
            Some(dto::CameraDefinition {
                uri: "http://user@10.0.0.2/camera.xml".into(),
                version: 3
            })
        );
        let stream = &wire.streams[0];
        assert_eq!(stream.uri, "rtsp://admin@10.0.0.2:8554/main");
        assert_eq!(stream.kind, dto::CameraStreamKind::Rtsp);
        assert_eq!(stream.encoding, Some(dto::CameraStreamEncoding::H264));
        assert!(stream.running);
        assert_eq!((stream.width, stream.height), (Some(1280), Some(720)));
        assert_eq!(stream.bitrate_bps, None, "0 is not said");
        assert_eq!(stream.source_id.as_deref(), Some("cam-v1-100-1"));
    }

    #[test]
    fn a_camera_that_only_sends_video_says_so() {
        let wire = dto::Camera::from(&camera(Some(core::CameraInformation {
            flags: 256,
            ..core::CameraInformation::default()
        })));
        assert!(wire.video_only);
        assert_eq!(wire.capabilities, [dto::CameraCapability::VideoStream]);
        assert_eq!(
            (wire.vendor, wire.model),
            (None, None),
            "empty words are not said"
        );
    }
}

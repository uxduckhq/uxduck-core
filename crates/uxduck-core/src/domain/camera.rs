//! A vehicle's cameras (ADR-0063 Y5, S404): each camera UXDuck knows a
//! vehicle has and what it can do, whether the camera says so itself as a
//! MAVLink component or its autopilot speaks for it.

use serde::{Deserialize, Serialize};

/// Something a camera can do (`CAMERA_CAP_FLAGS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraCapability {
    CaptureVideo,
    CaptureImage,
    Modes,
    ImageInVideoMode,
    VideoInImageMode,
    ImageSurveyMode,
    BasicZoom,
    BasicFocus,
    VideoStream,
    TrackingPoint,
    TrackingRectangle,
    TrackingGeoStatus,
    ThermalRange,
}

impl CameraCapability {
    /// Each capability with its bit of `CAMERA_CAP_FLAGS`.
    pub const ALL: [(u32, Self); 13] = [
        (1, Self::CaptureVideo),
        (2, Self::CaptureImage),
        (4, Self::Modes),
        (8, Self::ImageInVideoMode),
        (16, Self::VideoInImageMode),
        (32, Self::ImageSurveyMode),
        (64, Self::BasicZoom),
        (128, Self::BasicFocus),
        (256, Self::VideoStream),
        (512, Self::TrackingPoint),
        (1024, Self::TrackingRectangle),
        (2048, Self::TrackingGeoStatus),
        (4096, Self::ThermalRange),
    ];

    /// The capabilities a camera's flags name.
    #[must_use]
    pub fn of(flags: u32) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|(bit, _)| flags & bit != 0)
            .map(|(_, capability)| capability)
            .collect()
    }
}

/// What a camera said of itself (`CAMERA_INFORMATION`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInformation {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Dev, patch, minor and major from the top byte down. 0 when unknown.
    #[serde(default)]
    pub firmware_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<f32>,
    /// Across and down, in millimetres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_size_mm: Option<(f32, f32)>,
    /// Across and down, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_px: Option<(u16, u16)>,
    #[serde(default)]
    pub lens_id: u8,
    /// `CAMERA_CAP_FLAGS`.
    pub flags: u32,
    #[serde(default)]
    pub definition_version: u16,
    /// Where its camera definition file is, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub definition_uri: String,
    /// The gimbal it sits on: a gimbal device's component id, 1 to 6 for
    /// one its autopilot speaks for, or 0 for none.
    #[serde(default)]
    pub gimbal_device_id: u8,
}

impl CameraInformation {
    const CAPTURES: u32 = 1 | 2;
    const VIDEO_STREAM: u32 = 256;
    /// The thermal range is said alongside a video stream, and asks
    /// nothing of the camera.
    const THERMAL_RANGE: u32 = 4096;

    /// Whether it can take images or record video itself.
    #[must_use]
    pub fn captures(&self) -> bool {
        self.flags & Self::CAPTURES != 0
    }

    /// Whether it only sends video: it has a video stream and nothing to
    /// control, so it is shown its picture and no camera controls.
    #[must_use]
    pub fn video_only(&self) -> bool {
        self.flags & !Self::THERMAL_RANGE == Self::VIDEO_STREAM
    }

    /// Its firmware version as words, such as `1.2.3`, or none when it
    /// did not say.
    #[must_use]
    pub fn firmware(&self) -> Option<String> {
        let [major, minor, patch, dev] = self.firmware_version.to_le_bytes();
        match (self.firmware_version, dev) {
            (0, _) => None,
            (_, 0) => Some(format!("{major}.{minor}.{patch}")),
            _ => Some(format!("{major}.{minor}.{patch}.{dev}")),
        }
    }
}

/// One of a camera's video streams (`VIDEO_STREAM_INFORMATION`, brought up
/// to date by `VIDEO_STREAM_STATUS`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraStream {
    pub stream_id: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Where it is fetched or sent, as the camera said it.
    pub uri: String,
    /// `VIDEO_STREAM_TYPE`: 0 RTSP, 1 RTP over UDP, 2 MPEG-TS over TCP,
    /// 3 MPEG-TS over UDP.
    pub kind: u8,
    /// `VIDEO_STREAM_ENCODING`: 0 unknown, 1 H.264, 2 H.265.
    #[serde(default)]
    pub encoding: u8,
    /// `VIDEO_STREAM_STATUS_FLAGS`: 1 running, 2 thermal, 4 with a
    /// thermal range to ask for.
    #[serde(default)]
    pub flags: u16,
    #[serde(default)]
    pub frame_rate_hz: f32,
    #[serde(default)]
    pub width: u16,
    #[serde(default)]
    pub height: u16,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default)]
    pub rotation: u16,
    #[serde(default)]
    pub hfov: u16,
    /// The video source this stream arrives as, once it does (S38).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

impl CameraStream {
    /// The UDP port a stream sent to the ground station arrives on.
    #[must_use]
    pub fn udp_port(&self) -> Option<u16> {
        self.uri
            .trim()
            .strip_prefix("udp://")
            .and_then(|rest| rest.rsplit(':').next())
            .and_then(|port| port.trim_end_matches('/').parse().ok())
    }
}

/// A camera's mode (`CAMERA_MODE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraMode {
    Photo,
    Video,
    /// Photos for a survey (`CAMERA_MODE_IMAGE_SURVEY`).
    Survey,
}

impl CameraMode {
    /// The mode a `CAMERA_MODE` number names.
    #[must_use]
    pub fn of(mode_id: u8) -> Option<Self> {
        match mode_id {
            0 => Some(Self::Photo),
            1 => Some(Self::Video),
            2 => Some(Self::Survey),
            _ => None,
        }
    }

    /// Its `CAMERA_MODE` number.
    #[must_use]
    pub fn id(self) -> u8 {
        match self {
            Self::Photo => 0,
            Self::Video => 1,
            Self::Survey => 2,
        }
    }
}

/// The type of one of a camera's own settings (`MAV_PARAM_EXT_TYPE`), as
/// its definition file names it (S411).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraSettingKind {
    /// On or off, sent as a `uint8`.
    Bool,
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    Float,
    Double,
    /// Up to 128 bytes that only the camera's maker can read.
    Custom,
}

impl CameraSettingKind {
    /// The type a definition file's `type` attribute names.
    #[must_use]
    pub fn of(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "bool" => Self::Bool,
            "uint8" => Self::Uint8,
            "int8" => Self::Int8,
            "uint16" => Self::Uint16,
            "int16" => Self::Int16,
            "uint32" => Self::Uint32,
            "int32" => Self::Int32,
            "uint64" => Self::Uint64,
            "int64" => Self::Int64,
            "float" => Self::Float,
            "double" => Self::Double,
            "custom" => Self::Custom,
            _ => return None,
        })
    }

    /// Whether it holds whole numbers only.
    #[must_use]
    pub fn whole(self) -> bool {
        !matches!(self, Self::Float | Self::Double | Self::Custom)
    }

    /// The least and the most a whole type holds, as far as a number here
    /// can say them. None for the others.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "the ends of the 64-bit types, to the nearest number"
    )]
    pub fn bounds(self) -> Option<(f64, f64)> {
        Some(match self {
            Self::Bool => (0.0, 1.0),
            Self::Uint8 => (0.0, f64::from(u8::MAX)),
            Self::Int8 => (f64::from(i8::MIN), f64::from(i8::MAX)),
            Self::Uint16 => (0.0, f64::from(u16::MAX)),
            Self::Int16 => (f64::from(i16::MIN), f64::from(i16::MAX)),
            Self::Uint32 => (0.0, f64::from(u32::MAX)),
            Self::Int32 => (f64::from(i32::MIN), f64::from(i32::MAX)),
            Self::Uint64 => (0.0, u64::MAX as f64),
            Self::Int64 => (i64::MIN as f64, i64::MAX as f64),
            Self::Float | Self::Double | Self::Custom => return None,
        })
    }

    /// Why a number cannot be a value of this type, if it cannot.
    #[must_use]
    pub fn refuses(self, value: f64) -> Option<&'static str> {
        if self == Self::Custom {
            return Some("a setting of a custom type is read only by its maker's own software");
        }
        if !value.is_finite() {
            return Some("a setting's value is a number");
        }
        let (least, most) = self.bounds()?;
        if value.fract() != 0.0 {
            return Some("this setting takes whole numbers");
        }
        (value < least || value > most)
            .then_some("the value is outside what this setting's type holds")
    }
}

/// One of a camera's sensors, which its picture comes from
/// (`CAMERA_SOURCE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraSource {
    /// Its daylight sensor (`CAMERA_SOURCE_RGB`).
    Daylight,
    /// Its thermal sensor (`CAMERA_SOURCE_IR`).
    Thermal,
}

impl CameraSource {
    /// Its `CAMERA_SOURCE` number.
    #[must_use]
    pub fn id(self) -> u8 {
        match self {
            Self::Daylight => 1,
            Self::Thermal => 2,
        }
    }
}

/// What to have a camera do (ADR-0063 Y5, S405): each is one command of
/// the MAVLink camera protocol, which the camera acknowledges, or one of
/// its own settings written as an extended parameter (S411).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CameraAction {
    /// Photo or video mode (`MAV_CMD_SET_CAMERA_MODE`).
    SetMode { mode: CameraMode },
    /// One photo (`MAV_CMD_IMAGE_START_CAPTURE` for one image).
    TakePhoto,
    /// A photo every `interval_s` seconds: `count` of them, or until
    /// stopped (`MAV_CMD_IMAGE_START_CAPTURE`).
    StartInterval {
        interval_s: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    /// Ends an interval (`MAV_CMD_IMAGE_STOP_CAPTURE`).
    StopInterval,
    /// `MAV_CMD_VIDEO_START_CAPTURE`.
    StartRecording,
    /// `MAV_CMD_VIDEO_STOP_CAPTURE`.
    StopRecording,
    /// Zooms by steps, in when positive and out when negative
    /// (`MAV_CMD_SET_CAMERA_ZOOM`, `ZOOM_TYPE_STEP`).
    ZoomStep { steps: i8 },
    /// Zooms to a place in its range, from 0 (widest) to 100
    /// (`ZOOM_TYPE_RANGE`).
    ZoomTo { percent: f32 },
    /// Focuses by steps, farther when positive and nearer when negative
    /// (`MAV_CMD_SET_CAMERA_FOCUS`, `FOCUS_TYPE_STEP`).
    FocusStep { steps: i8 },
    /// Focuses at a place in its range, from 0 (nearest) to 100
    /// (`FOCUS_TYPE_RANGE`).
    FocusTo { percent: f32 },
    /// Lets the camera focus by itself (`FOCUS_TYPE_AUTO`).
    FocusAuto,
    /// Erases a card and its list of photos (`MAV_CMD_STORAGE_FORMAT`).
    /// The first card unless `storage_id` names another.
    Format {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_id: Option<u8>,
    },
    /// Puts its settings back as it was made
    /// (`MAV_CMD_RESET_CAMERA_SETTINGS`).
    Reset,
    /// Sets one of the settings its definition file names (S411), as an
    /// extended parameter (`PARAM_EXT_SET`), which the camera answers with
    /// `PARAM_EXT_ACK`.
    SetSetting {
        /// The setting's name in the definition file, such as `CAM_ISO`.
        name: String,
        value: f64,
        /// Its type, which UXDuck fills in from the camera's definition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<CameraSettingKind>,
    },
    /// Shows another of its sensors on its picture
    /// (`MAV_CMD_SET_CAMERA_SOURCE`).
    SetSource { source: CameraSource },
    /// Tracks what is at a point in its picture
    /// (`MAV_CMD_CAMERA_TRACK_POINT`): across and down it, from 0 at the
    /// left and the top to 1.
    TrackPoint { x: f32, y: f32 },
    /// Tracks what is in a box in its picture
    /// (`MAV_CMD_CAMERA_TRACK_RECTANGLE`), by its sides from 0 at the
    /// picture's left and top to 1.
    TrackRectangle {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    },
    /// `MAV_CMD_CAMERA_STOP_TRACKING`.
    StopTracking,
    /// Starts or stops recording, as the camera last said it was not or
    /// was (S416). Never sent as such: the engine resolves it to
    /// `StartRecording` or `StopRecording`, as `ArmToggle` is.
    ToggleRecording,
    /// Video mode from photo mode, and photo mode otherwise (S416),
    /// resolved to `SetMode`.
    ToggleMode,
    /// The daylight picture from the thermal one, and the thermal one
    /// otherwise (S416), resolved to `SetSource`.
    ToggleSource,
    /// Zooms on at a rate from -1 to 1 until told otherwise, 0 stopping it
    /// (`MAV_CMD_SET_CAMERA_ZOOM`, `ZOOM_TYPE_CONTINUOUS`, S416).
    ZoomRate { rate: f32 },
    /// Focuses on at a rate from -1 to 1 until told otherwise, 0 stopping
    /// it (`MAV_CMD_SET_CAMERA_FOCUS`, `FOCUS_TYPE_CONTINUOUS`, S416).
    FocusRate { rate: f32 },
}

impl CameraAction {
    /// For people: "Take a photo", "Zoom in".
    #[must_use]
    pub fn label(&self) -> String {
        match *self {
            Self::SetMode {
                mode: CameraMode::Photo,
            } => "Switch the camera to photo mode".into(),
            Self::SetMode {
                mode: CameraMode::Video,
            } => "Switch the camera to video mode".into(),
            Self::SetMode {
                mode: CameraMode::Survey,
            } => "Switch the camera to survey mode".into(),
            Self::TakePhoto => "Take a photo".into(),
            Self::StartInterval {
                interval_s,
                count: Some(count),
            } => format!("Take {count} photos, one every {interval_s} s"),
            Self::StartInterval {
                interval_s,
                count: None,
            } => format!("Take a photo every {interval_s} s"),
            Self::StopInterval => "Stop taking photos".into(),
            Self::StartRecording => "Start recording".into(),
            Self::StopRecording => "Stop recording".into(),
            Self::ZoomStep { steps } if steps < 0 => "Zoom out".into(),
            Self::ZoomStep { .. } => "Zoom in".into(),
            Self::ZoomTo { percent } => format!("Zoom to {percent}%"),
            Self::FocusStep { steps } if steps < 0 => "Focus nearer".into(),
            Self::FocusStep { .. } => "Focus farther".into(),
            Self::FocusTo { percent } => format!("Focus at {percent}%"),
            Self::FocusAuto => "Focus automatically".into(),
            Self::Format { .. } => "Format the camera's card".into(),
            Self::Reset => "Reset the camera's settings".into(),
            Self::SetSetting {
                ref name, value, ..
            } => format!("Set the camera's {name} to {value}"),
            Self::SetSource {
                source: CameraSource::Daylight,
            } => "Switch the camera to its daylight picture".into(),
            Self::SetSource {
                source: CameraSource::Thermal,
            } => "Switch the camera to its thermal picture".into(),
            Self::TrackPoint { .. } => "Track a point in the camera's picture".into(),
            Self::TrackRectangle { .. } => "Track a box in the camera's picture".into(),
            Self::StopTracking => "Stop the camera's tracking".into(),
            Self::ToggleRecording => "Record or stop".into(),
            Self::ToggleMode => "Swap photo and video".into(),
            Self::ToggleSource => "Swap Daylight and Thermal".into(),
            Self::ZoomRate { rate } if rate > 0.0 => "Zoom in".into(),
            Self::ZoomRate { rate } if rate < 0.0 => "Zoom out".into(),
            Self::ZoomRate { .. } => "Stop zooming".into(),
            Self::FocusRate { rate } if rate > 0.0 => "Focus farther".into(),
            Self::FocusRate { rate } if rate < 0.0 => "Focus nearer".into(),
            Self::FocusRate { .. } => "Stop focusing".into(),
        }
    }

    /// What a camera must say it can do for this to be offered. None for
    /// what every camera with a card or settings takes.
    #[must_use]
    pub fn needs(&self) -> Option<CameraCapability> {
        match self {
            Self::SetMode { .. } | Self::ToggleMode => Some(CameraCapability::Modes),
            Self::TakePhoto | Self::StartInterval { .. } | Self::StopInterval => {
                Some(CameraCapability::CaptureImage)
            }
            Self::StartRecording | Self::StopRecording | Self::ToggleRecording => {
                Some(CameraCapability::CaptureVideo)
            }
            Self::ZoomStep { .. } | Self::ZoomTo { .. } | Self::ZoomRate { .. } => {
                Some(CameraCapability::BasicZoom)
            }
            Self::FocusStep { .. }
            | Self::FocusTo { .. }
            | Self::FocusAuto
            | Self::FocusRate { .. } => Some(CameraCapability::BasicFocus),
            Self::TrackPoint { .. } => Some(CameraCapability::TrackingPoint),
            Self::TrackRectangle { .. } => Some(CameraCapability::TrackingRectangle),
            Self::Format { .. }
            | Self::Reset
            | Self::SetSetting { .. }
            | Self::SetSource { .. }
            | Self::ToggleSource
            | Self::StopTracking => None,
        }
    }

    /// The setting it writes, for one that writes a setting of the
    /// camera's definition file.
    #[must_use]
    pub fn setting(&self) -> Option<(&str, f64, Option<CameraSettingKind>)> {
        match self {
            Self::SetSetting { name, value, kind } => Some((name, *value, *kind)),
            _ => None,
        }
    }

    /// Whether it is a toggle, which goes as what the camera needs from
    /// what it last said (`Camera::toggled`).
    #[must_use]
    pub fn toggles(&self) -> bool {
        matches!(
            self,
            Self::ToggleRecording | Self::ToggleMode | Self::ToggleSource
        )
    }

    /// Whether it erases what the camera holds.
    #[must_use]
    pub fn erases(&self) -> bool {
        matches!(self, Self::Format { .. })
    }

    /// Whether it starts a capture, which a full card refuses.
    #[must_use]
    pub fn captures(&self) -> bool {
        matches!(
            self,
            Self::TakePhoto | Self::StartInterval { .. } | Self::StartRecording
        )
    }

    /// Why it cannot be sent as it is, if it cannot.
    #[must_use]
    pub fn invalid(&self) -> Option<&'static str> {
        let percent = |p: f32| !(0.0..=100.0).contains(&p);
        if let Self::SetSetting { name, value, kind } = self {
            // An extended parameter's name is at most 16 characters of
            // ASCII (`param_id`).
            if name.is_empty() || name.len() > 16 || !name.is_ascii() {
                return Some("a setting's name is 1 to 16 plain characters");
            }
            if !value.is_finite() {
                return Some("a setting's value is a number");
            }
            return kind.and_then(|kind| kind.refuses(*value));
        }
        match *self {
            Self::StartInterval { interval_s, .. }
                if !(interval_s > 0.0 && interval_s.is_finite()) =>
            {
                Some("an interval is a number of seconds above 0")
            }
            Self::StartInterval { count: Some(0), .. } => {
                Some("a count of photos is at least 1. Leave it out to go on until stopped")
            }
            Self::ZoomStep { steps: 0 } | Self::FocusStep { steps: 0 } => {
                Some("a step is in one direction: not 0")
            }
            Self::ZoomTo { percent: p } | Self::FocusTo { percent: p } if percent(p) => {
                Some("a place in the range is from 0 to 100")
            }
            Self::ZoomRate { rate } | Self::FocusRate { rate } if !(-1.0..=1.0).contains(&rate) => {
                Some("a rate is from -1 to 1, and 0 stops")
            }
            Self::Format {
                storage_id: Some(0),
            } => Some("cards are numbered from 1"),
            Self::TrackPoint { x, y } if !(share(x) && share(y)) => {
                Some("a point in the picture is from 0 to 1 across it and down it")
            }
            Self::TrackRectangle {
                left,
                top,
                right,
                bottom,
            } if !([left, top, right, bottom].into_iter().all(share)
                && left < right
                && top < bottom) =>
            {
                Some(
                    "a box in the picture has its sides from 0 to 1, its left before its right and its top before its bottom",
                )
            }
            _ => None,
        }
    }
}

/// Whether a number is a place across or down a picture.
fn share(value: f32) -> bool {
    (0.0..=1.0).contains(&value)
}

/// A camera's mode, zoom and focus as it last said them
/// (`CAMERA_SETTINGS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CameraMode>,
    /// From 0 (widest) to 100. None when the camera does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_pct: Option<f32>,
    /// From 0 (nearest) to 100. None when the camera does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_pct: Option<f32>,
}

/// What a camera is capturing, as it last said (`CAMERA_CAPTURE_STATUS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraCapture {
    /// Seconds between photos while it takes them at an interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<f32>,
    pub recording: bool,
    /// When the recording began by the camera's own count of it, in
    /// microseconds since 1970 on this computer's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_since_us: Option<u64>,
    /// The photos in its list, which a format empties.
    #[serde(default)]
    pub photo_count: u32,
    /// Photos it counted and did not describe, asked for again without
    /// an answer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_lost: u32,
    /// Room left on its card, in mebibytes, in coarse steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_mib: Option<f32>,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A card's state (`STORAGE_STATUS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraStorageStatus {
    /// No card.
    Empty,
    Unformatted,
    #[default]
    Ready,
    /// The camera does not say.
    NotSupported,
}

impl CameraStorageStatus {
    /// The status a `STORAGE_STATUS` number names.
    #[must_use]
    pub fn of(status: u8) -> Self {
        match status {
            0 => Self::Empty,
            1 => Self::Unformatted,
            2 => Self::Ready,
            _ => Self::NotSupported,
        }
    }
}

/// A camera's card as it last said (`STORAGE_INFORMATION`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraStorage {
    /// 1 for the first card.
    pub storage_id: u8,
    /// How many cards it has.
    #[serde(default)]
    pub count: u8,
    pub status: CameraStorageStatus,
    #[serde(default)]
    pub total_mib: f32,
    #[serde(default)]
    pub used_mib: f32,
    #[serde(default)]
    pub available_mib: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// Where a photo was taken.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPhotoPlace {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_amsl_m: f64,
    /// Its height above home.
    pub relative_altitude_m: f64,
}

/// A photo a camera said it took (`CAMERA_IMAGE_CAPTURED`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraPhoto {
    /// Its place in the camera's list, from 0.
    pub index: u32,
    /// When it was taken by the camera's clock when that is set, else when
    /// UXDuck heard of it: microseconds since 1970.
    pub taken_at_us: u64,
    /// None when the camera did not know where it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<CameraPhotoPlace>,
    /// False when the camera said the capture failed.
    pub ok: bool,
    /// The file's name or address on the camera, when it gave one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
}

/// Room under which a card counts as full: less than two photos of most
/// cameras.
pub const CARD_FULL_BELOW_MIB: f32 = 16.0;

/// A camera of a vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Camera {
    /// `<vehicle id>/<component id>`, and `/<device id>` after it for a
    /// camera its autopilot speaks for.
    pub id: String,
    pub vehicle_id: String,
    /// The camera's own MAVLink component, or the autopilot's that speaks
    /// for it.
    pub component_id: u8,
    /// 1 to 6 for a camera its autopilot speaks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<u8>,
    /// Whether its heartbeats, or its autopilot's, are heard. A camera that
    /// is gone is kept, and identified again when it returns.
    pub present: bool,
    /// None until it says what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub information: Option<CameraInformation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<CameraStream>,
    /// Its mode, zoom and focus, once it has said them (S405).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<CameraSettings>,
    /// What it is capturing, once it has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CameraCapture>,
    /// Its card, once it has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<CameraStorage>,
    /// The photos it said it took while UXDuck listened, oldest first:
    /// the last [`Camera::PHOTOS_KEPT`] of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub photos: Vec<CameraPhoto>,
    /// The sensors it has said it has, in what it can do and in its
    /// streams, since UXDuck first heard it (ADR-0063 Y5). A camera with
    /// one stream shows one at a time, so what it showed before is kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<CameraSource>,
    pub version: u64,
}

impl Camera {
    /// How many of a camera's photos are kept in its list. A camera says
    /// of each photo it takes on its MAVLink stream, as many as it likes,
    /// and the whole list goes to every window with the camera each time
    /// it changes, so the list is a ring of the newest.
    pub const PHOTOS_KEPT: usize = 20;

    /// Room left on its card, from what it last said of its captures or
    /// of the card.
    #[must_use]
    pub fn available_mib(&self) -> Option<f32> {
        self.capture
            .and_then(|capture| capture.available_mib)
            .or_else(|| self.storage.as_ref().map(|storage| storage.available_mib))
    }

    /// Whether its card is known to have no room for a capture.
    #[must_use]
    pub fn card_full(&self) -> bool {
        self.available_mib()
            .is_some_and(|left| left < CARD_FULL_BELOW_MIB)
    }

    /// Whether it says it can do what an action needs. A camera that has
    /// not said what it is may.
    #[must_use]
    pub fn allows(&self, action: &CameraAction) -> bool {
        let Some(information) = &self.information else {
            return true;
        };
        let can = CameraCapability::of(information.flags);
        match action {
            // No flag says a camera has more than one sensor. Its answer
            // says whether it has the one asked for.
            CameraAction::SetSource { .. } | CameraAction::ToggleSource => return true,
            CameraAction::StopTracking => {
                return can.contains(&CameraCapability::TrackingPoint)
                    || can.contains(&CameraCapability::TrackingRectangle);
            }
            _ => {}
        }
        match action.needs() {
            Some(needed) => can.contains(&needed),
            None => !information.video_only(),
        }
    }

    /// The sensors a camera says it has now, with those it said before
    /// while it was the same camera: thermal from a thermal range it can
    /// give or a stream that is thermal, and daylight from a stream that
    /// is not. A camera that has named no stream has said neither.
    #[must_use]
    pub fn sources_said(&self, before: Option<&Self>) -> Vec<CameraSource> {
        const THERMAL_RANGE: u32 = 4096;
        // `VIDEO_STREAM_STATUS_FLAGS`: thermal, and a thermal range to ask for.
        const THERMAL: u16 = 2 | 4;
        let mut sources: Vec<CameraSource> = before
            .filter(|before| {
                let same = |a: &CameraInformation, b: &CameraInformation| {
                    (&a.vendor, &a.model) == (&b.vendor, &b.model)
                };
                match (&self.information, &before.information) {
                    (Some(now), Some(then)) => same(now, then),
                    (None, _) => true,
                    (Some(_), None) => false,
                }
            })
            .map(|before| before.sources.clone())
            .unwrap_or_default();
        let thermal = self
            .information
            .as_ref()
            .is_some_and(|said| said.flags & THERMAL_RANGE != 0)
            || self
                .streams
                .iter()
                .any(|stream| stream.flags & THERMAL != 0);
        let daylight = self
            .streams
            .iter()
            .any(|stream| stream.flags & THERMAL == 0);
        if daylight {
            sources.push(CameraSource::Daylight);
        }
        if thermal {
            sources.push(CameraSource::Thermal);
        }
        sources.sort_unstable();
        sources.dedup();
        sources
    }

    /// What a toggle comes to for this camera, from what it last said
    /// (S416): the opposite of its recording, its mode or its picture. Any
    /// other action is itself. Why not, while it has not said, as words
    /// that follow "the camera".
    ///
    /// # Errors
    ///
    /// When the camera has not said what the toggle turns over.
    pub fn toggled(&self, action: CameraAction) -> Result<CameraAction, &'static str> {
        match action {
            CameraAction::ToggleRecording => match self.capture {
                Some(capture) if capture.recording => Ok(CameraAction::StopRecording),
                Some(_) => Ok(CameraAction::StartRecording),
                None => Err("has not said yet whether it is recording"),
            },
            CameraAction::ToggleMode => match self.settings.and_then(|s| s.mode) {
                Some(CameraMode::Photo) => Ok(CameraAction::SetMode {
                    mode: CameraMode::Video,
                }),
                Some(CameraMode::Video | CameraMode::Survey) => Ok(CameraAction::SetMode {
                    mode: CameraMode::Photo,
                }),
                None => Err("has not said yet whether it is in photo or video mode"),
            },
            CameraAction::ToggleSource => {
                // `VIDEO_STREAM_STATUS_FLAGS_THERMAL`.
                const THERMAL: u16 = 2;
                if self.streams.is_empty() {
                    return Err("has not said yet which picture it shows");
                }
                let thermal = self.streams.iter().any(|s| s.flags & THERMAL != 0);
                Ok(CameraAction::SetSource {
                    source: if thermal {
                        CameraSource::Daylight
                    } else {
                        CameraSource::Thermal
                    },
                })
            }
            other => Ok(other),
        }
    }

    /// The id of a vehicle's camera.
    #[must_use]
    pub fn id_of(vehicle_id: &str, component_id: u8, device_id: Option<u8>) -> String {
        match device_id {
            Some(device) => format!("{vehicle_id}/{component_id}/{device}"),
            None => format!("{vehicle_id}/{component_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn information(flags: u32) -> CameraInformation {
        CameraInformation {
            flags,
            ..CameraInformation::default()
        }
    }

    #[test]
    fn flags_name_what_a_camera_can_do() {
        assert_eq!(
            CameraCapability::of(1 | 2 | 256),
            [
                CameraCapability::CaptureVideo,
                CameraCapability::CaptureImage,
                CameraCapability::VideoStream
            ]
        );
        assert!(CameraCapability::of(0).is_empty());
        assert_eq!(CameraCapability::of(u32::MAX).len(), 13);
    }

    #[test]
    fn a_camera_that_only_sends_video_has_only_its_stream() {
        assert!(information(256).video_only());
        assert!(
            information(256 | 4096).video_only(),
            "a thermal range is no control"
        );
        assert!(!information(256 | 2).video_only());
        assert!(
            !information(0).video_only(),
            "nothing said is not video only"
        );
        assert!(information(2).captures());
        assert!(information(1).captures());
        assert!(!information(256 | 64).captures());
    }

    #[test]
    fn a_firmware_version_reads_major_first() {
        let version = |v: u32| {
            CameraInformation {
                firmware_version: v,
                ..information(0)
            }
            .firmware()
        };
        assert_eq!(version(0), None);
        assert_eq!(version(0x0003_0201).as_deref(), Some("1.2.3"));
        assert_eq!(version(0x0403_0201).as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn a_camera_is_named_by_its_vehicle_component_and_device() {
        assert_eq!(Camera::id_of("v1", 100, None), "v1/100");
        assert_eq!(Camera::id_of("v1", 1, Some(2)), "v1/1/2");
    }

    #[test]
    fn a_udp_stream_names_its_port() {
        let stream = |uri: &str| CameraStream {
            uri: uri.into(),
            ..CameraStream::default()
        };
        assert_eq!(stream("udp://0.0.0.0:5600").udp_port(), Some(5600));
        assert_eq!(stream(" udp://:5601/ ").udp_port(), Some(5601));
        assert_eq!(stream("rtsp://10.0.0.2:8554/main").udp_port(), None);
    }

    #[test]
    fn a_camera_kept_before_it_had_streams_still_reads() {
        let camera: Camera = serde_json::from_str(
            r#"{"id":"v1/100","vehicleId":"v1","componentId":100,"present":false,"version":1}"#,
        )
        .unwrap();
        assert!(camera.information.is_none());
        assert!(camera.streams.is_empty());
    }

    /// A camera that has said what it can do.
    fn camera(flags: u32) -> Camera {
        Camera {
            id: "v/100".into(),
            vehicle_id: "v".into(),
            component_id: 100,
            device_id: None,
            present: true,
            information: Some(information(flags)),
            streams: Vec::new(),
            settings: None,
            capture: None,
            storage: None,
            photos: Vec::new(),
            sources: Vec::new(),
            version: 1,
        }
    }

    // S416: a controller's toggle is what the camera last said, turned
    // over, and nothing while it has not said.
    #[test]
    fn a_toggle_is_the_opposite_of_what_the_camera_last_said() {
        let mut camera = camera(1 | 2 | 256);
        assert!(camera.toggled(CameraAction::ToggleRecording).is_err());
        assert!(camera.toggled(CameraAction::ToggleMode).is_err());
        assert!(camera.toggled(CameraAction::ToggleSource).is_err());
        assert_eq!(
            camera.toggled(CameraAction::TakePhoto),
            Ok(CameraAction::TakePhoto)
        );

        camera.capture = Some(CameraCapture::default());
        assert_eq!(
            camera.toggled(CameraAction::ToggleRecording),
            Ok(CameraAction::StartRecording)
        );
        camera.capture = Some(CameraCapture {
            recording: true,
            ..CameraCapture::default()
        });
        assert_eq!(
            camera.toggled(CameraAction::ToggleRecording),
            Ok(CameraAction::StopRecording)
        );

        camera.settings = Some(CameraSettings {
            mode: Some(CameraMode::Photo),
            ..CameraSettings::default()
        });
        assert_eq!(
            camera.toggled(CameraAction::ToggleMode),
            Ok(CameraAction::SetMode {
                mode: CameraMode::Video
            })
        );
        camera.settings = Some(CameraSettings {
            mode: Some(CameraMode::Survey),
            ..CameraSettings::default()
        });
        assert_eq!(
            camera.toggled(CameraAction::ToggleMode),
            Ok(CameraAction::SetMode {
                mode: CameraMode::Photo
            })
        );

        camera.streams = vec![CameraStream::default()];
        assert_eq!(
            camera.toggled(CameraAction::ToggleSource),
            Ok(CameraAction::SetSource {
                source: CameraSource::Thermal
            })
        );
        camera.streams[0].flags = 2;
        assert_eq!(
            camera.toggled(CameraAction::ToggleSource),
            Ok(CameraAction::SetSource {
                source: CameraSource::Daylight
            })
        );
    }

    // S405: the Camera tab, the agent and "Also save on the vehicle" all
    // ask this before offering or sending a control.
    #[test]
    fn a_camera_allows_only_what_its_flags_say() {
        let stills = camera(2);
        assert!(stills.allows(&CameraAction::TakePhoto));
        assert!(stills.allows(&CameraAction::StartInterval {
            interval_s: 2.0,
            count: None
        }));
        assert!(!stills.allows(&CameraAction::StartRecording));
        assert!(!stills.allows(&CameraAction::ZoomStep { steps: 1 }));
        assert!(!stills.allows(&CameraAction::FocusAuto));
        assert!(!stills.allows(&CameraAction::SetMode {
            mode: CameraMode::Video
        }));
        // A card and settings are any camera's but one that only sends video.
        assert!(stills.allows(&CameraAction::Format { storage_id: None }));
        assert!(stills.allows(&CameraAction::Reset));
        let encoder = camera(256);
        assert!(!encoder.allows(&CameraAction::Format { storage_id: None }));
        assert!(!encoder.allows(&CameraAction::Reset));
        assert!(!encoder.allows(&CameraAction::TakePhoto));
        let whole = camera(1 | 2 | 4 | 64 | 128 | 256);
        for action in [
            CameraAction::StartRecording,
            CameraAction::StopRecording,
            CameraAction::ZoomTo { percent: 50.0 },
            CameraAction::FocusStep { steps: -1 },
            CameraAction::SetMode {
                mode: CameraMode::Photo,
            },
        ] {
            assert!(whole.allows(&action), "{action:?}");
        }
        // One that has not said what it is may do anything: the camera answers.
        let unknown = Camera {
            information: None,
            ..camera(0)
        };
        assert!(unknown.allows(&CameraAction::StartRecording));
    }

    #[test]
    fn an_action_is_named_for_people_and_checked_before_it_is_sent() {
        for (action, label) in [
            (CameraAction::TakePhoto, "Take a photo"),
            (CameraAction::ZoomStep { steps: 2 }, "Zoom in"),
            (CameraAction::ZoomStep { steps: -1 }, "Zoom out"),
            (CameraAction::FocusStep { steps: -1 }, "Focus nearer"),
            (CameraAction::FocusStep { steps: 1 }, "Focus farther"),
            (CameraAction::StopInterval, "Stop taking photos"),
            (
                CameraAction::SetMode {
                    mode: CameraMode::Video,
                },
                "Switch the camera to video mode",
            ),
            (
                CameraAction::StartInterval {
                    interval_s: 2.0,
                    count: Some(10),
                },
                "Take 10 photos, one every 2 s",
            ),
            (
                CameraAction::Format { storage_id: None },
                "Format the camera's card",
            ),
        ] {
            assert_eq!(action.label(), label);
            assert_eq!(action.invalid(), None, "{label}");
        }
        for wrong in [
            CameraAction::StartInterval {
                interval_s: 0.0,
                count: None,
            },
            CameraAction::StartInterval {
                interval_s: f32::NAN,
                count: None,
            },
            CameraAction::StartInterval {
                interval_s: 2.0,
                count: Some(0),
            },
            CameraAction::ZoomStep { steps: 0 },
            CameraAction::FocusStep { steps: 0 },
            CameraAction::ZoomTo { percent: 101.0 },
            CameraAction::FocusTo { percent: -1.0 },
            CameraAction::Format {
                storage_id: Some(0),
            },
        ] {
            assert!(wrong.invalid().is_some(), "{wrong:?}");
        }
        // Only a format erases, and only what starts a capture needs room.
        assert!(CameraAction::Format { storage_id: None }.erases());
        assert!(!CameraAction::Reset.erases());
        assert!(CameraAction::StartRecording.captures());
        assert!(CameraAction::TakePhoto.captures());
        assert!(!CameraAction::StopRecording.captures());
        assert!(!CameraAction::ZoomStep { steps: 1 }.captures());
    }

    #[test]
    fn a_card_is_full_below_two_photos_of_room_by_what_the_camera_last_said() {
        let mut sim = camera(1 | 2);
        assert_eq!(sim.available_mib(), None);
        assert!(!sim.card_full(), "nothing said is not a full card");
        sim.storage = Some(CameraStorage {
            storage_id: 1,
            count: 1,
            status: CameraStorageStatus::Ready,
            total_mib: 32_000.0,
            used_mib: 31_995.0,
            available_mib: 5.0,
            name: String::new(),
        });
        assert!(sim.card_full());
        // Its capture status is said more often, so it is believed first.
        sim.capture = Some(CameraCapture {
            available_mib: Some(20_000.0),
            ..CameraCapture::default()
        });
        assert_eq!(sim.available_mib(), Some(20_000.0));
        assert!(!sim.card_full());
        sim.capture = Some(CameraCapture {
            available_mib: Some(CARD_FULL_BELOW_MIB),
            ..CameraCapture::default()
        });
        assert!(!sim.card_full(), "exactly the line is not below it");
    }

    #[test]
    fn modes_and_card_states_read_from_their_mavlink_numbers() {
        for (id, mode) in [
            (0, CameraMode::Photo),
            (1, CameraMode::Video),
            (2, CameraMode::Survey),
        ] {
            assert_eq!(CameraMode::of(id), Some(mode));
            assert_eq!(mode.id(), id);
        }
        assert_eq!(CameraMode::of(3), None);
        assert_eq!(CameraStorageStatus::of(0), CameraStorageStatus::Empty);
        assert_eq!(CameraStorageStatus::of(1), CameraStorageStatus::Unformatted);
        assert_eq!(CameraStorageStatus::of(2), CameraStorageStatus::Ready);
        assert_eq!(
            CameraStorageStatus::of(3),
            CameraStorageStatus::NotSupported
        );
    }

    // S412: a point or a box is tracked only by a camera whose flags say
    // it tracks that. Stopping is for a camera that tracks either, and
    // the sensor is any camera's to be asked for, since no flag says a
    // camera has two.
    #[test]
    fn tracking_is_for_a_camera_that_says_it_tracks_and_a_sensor_is_any_cameras() {
        let point = CameraAction::TrackPoint { x: 0.5, y: 0.5 };
        let rectangle = CameraAction::TrackRectangle {
            left: 0.4,
            top: 0.4,
            right: 0.6,
            bottom: 0.6,
        };
        let thermal = CameraAction::SetSource {
            source: CameraSource::Thermal,
        };
        let plain = camera(1 | 2);
        assert!(!plain.allows(&point));
        assert!(!plain.allows(&rectangle));
        assert!(!plain.allows(&CameraAction::StopTracking));
        assert!(plain.allows(&thermal));
        let points = camera(1 | 2 | 512);
        assert!(points.allows(&point));
        assert!(!points.allows(&rectangle));
        assert!(points.allows(&CameraAction::StopTracking));
        let boxes = camera(1 | 2 | 1024);
        assert!(!boxes.allows(&point));
        assert!(boxes.allows(&rectangle));
        assert!(boxes.allows(&CameraAction::StopTracking));
        // One that only sends video is still asked for its other sensor.
        assert!(camera(256).allows(&thermal));
    }

    #[test]
    fn a_point_and_a_box_to_track_are_inside_the_picture() {
        for (action, label) in [
            (
                CameraAction::TrackPoint { x: 0.0, y: 1.0 },
                "Track a point in the camera's picture",
            ),
            (
                CameraAction::TrackRectangle {
                    left: 0.0,
                    top: 0.25,
                    right: 1.0,
                    bottom: 0.75,
                },
                "Track a box in the camera's picture",
            ),
            (CameraAction::StopTracking, "Stop the camera's tracking"),
            (
                CameraAction::SetSource {
                    source: CameraSource::Thermal,
                },
                "Switch the camera to its thermal picture",
            ),
            (
                CameraAction::SetSource {
                    source: CameraSource::Daylight,
                },
                "Switch the camera to its daylight picture",
            ),
        ] {
            assert_eq!(action.label(), label);
            assert_eq!(action.invalid(), None, "{label}");
            assert!(!action.captures() && !action.erases(), "{label}");
        }
        for wrong in [
            CameraAction::TrackPoint { x: -0.01, y: 0.5 },
            CameraAction::TrackPoint { x: 0.5, y: 1.01 },
            CameraAction::TrackPoint {
                x: f32::NAN,
                y: 0.5,
            },
            CameraAction::TrackRectangle {
                left: 0.6,
                top: 0.4,
                right: 0.4,
                bottom: 0.6,
            },
            CameraAction::TrackRectangle {
                left: 0.4,
                top: 0.6,
                right: 0.6,
                bottom: 0.6,
            },
            CameraAction::TrackRectangle {
                left: 0.4,
                top: 0.4,
                right: 1.2,
                bottom: 0.6,
            },
            CameraAction::TrackRectangle {
                left: 0.4,
                top: f32::NAN,
                right: 0.6,
                bottom: 0.6,
            },
        ] {
            assert!(wrong.invalid().is_some(), "{wrong:?}");
        }
        assert_eq!(CameraSource::Daylight.id(), 1);
        assert_eq!(CameraSource::Thermal.id(), 2);
    }

    #[test]
    fn a_camera_keeps_the_sensors_it_has_said_while_it_is_the_same_camera() {
        let stream = |flags: u16| CameraStream {
            stream_id: 1,
            flags,
            ..CameraStream::default()
        };
        // Nothing said of a stream is neither sensor.
        assert!(camera(1 | 2).sources_said(None).is_empty());
        // One daylight stream is a camera with no choice to offer.
        let mut daylight = camera(1 | 2);
        daylight.streams = vec![stream(1)];
        assert_eq!(daylight.sources_said(None), [CameraSource::Daylight]);
        // A thermal range it can give says thermal before it shows it.
        let mut both = camera(1 | 2 | 4096);
        both.streams = vec![stream(1)];
        assert_eq!(
            both.sources_said(None),
            [CameraSource::Daylight, CameraSource::Thermal]
        );
        // A camera with no such flag that shows thermal now, having shown
        // daylight before, has both.
        let mut then = camera(1 | 2);
        then.streams = vec![stream(1)];
        then.sources = then.sources_said(None);
        let mut now = camera(1 | 2);
        now.streams = vec![stream(1 | 2)];
        assert_eq!(now.sources_said(None), [CameraSource::Thermal]);
        assert_eq!(
            now.sources_said(Some(&then)),
            [CameraSource::Daylight, CameraSource::Thermal]
        );
        // Gone quiet, it keeps what it said.
        let quiet = Camera {
            information: None,
            ..camera(0)
        };
        then.sources = vec![CameraSource::Daylight, CameraSource::Thermal];
        assert_eq!(
            quiet.sources_said(Some(&then)),
            [CameraSource::Daylight, CameraSource::Thermal]
        );
        // Another camera at the same component starts again.
        let mut other = camera(1 | 2);
        other.information.as_mut().unwrap().model = "Another".into();
        other.streams = vec![stream(1)];
        assert_eq!(other.sources_said(Some(&then)), [CameraSource::Daylight]);
    }

    #[test]
    fn a_camera_stored_before_it_said_its_sensors_still_reads() {
        let old: Camera = serde_json::from_str(
            r#"{"id":"v/100","vehicleId":"v","componentId":100,"present":true,"version":3}"#,
        )
        .expect("an older camera");
        assert!(old.sources.is_empty());
        assert!(!serde_json::to_string(&old).unwrap().contains("sources"));
        let wire = serde_json::to_value(CameraAction::SetSource {
            source: CameraSource::Thermal,
        })
        .unwrap();
        assert_eq!(wire["type"], "set-source");
        assert_eq!(wire["source"], "thermal");
    }

    // A camera kept before S405 has none of what it says back.
    #[test]
    fn a_camera_stored_before_it_said_anything_back_still_reads() {
        let old: Camera = serde_json::from_str(
            r#"{"id":"v/100","vehicleId":"v","componentId":100,"present":true,"version":3}"#,
        )
        .expect("an older camera");
        assert_eq!((old.settings, old.capture), (None, None));
        assert_eq!(old.storage, None);
        assert!(old.photos.is_empty());
        let json = serde_json::to_value(&old).expect("JSON");
        for absent in ["settings", "capture", "storage", "photos"] {
            assert!(json.get(absent).is_none(), "{absent}");
        }
    }
}

//! A vehicle's cameras on the wire (ADR-0063 Y5, S404): each camera UXDuck
//! knows a vehicle has and what it can do, whether the camera says so
//! itself as a MAVLink component or its autopilot speaks for it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Something a camera can do, as its `CAMERA_CAP_FLAGS` say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraCapability {
    /// It records video.
    CaptureVideo,
    /// It takes images.
    CaptureImage,
    /// It has a video mode and an image mode.
    Modes,
    /// It takes images while in video mode.
    ImageInVideoMode,
    /// It records video while in image mode.
    VideoInImageMode,
    /// It has a survey mode for images.
    ImageSurveyMode,
    /// It zooms.
    BasicZoom,
    /// It focuses.
    BasicFocus,
    /// It has video streams.
    VideoStream,
    /// It tracks a point.
    TrackingPoint,
    /// It tracks a rectangle.
    TrackingRectangle,
    /// It says where what it tracks is.
    TrackingGeoStatus,
    /// It says the thermal range of its picture.
    ThermalRange,
}

/// A camera's sensor. Each measure is absent when the camera did not say.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSensor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_px: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_px: Option<u16>,
}

/// A camera's lens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraLens {
    /// Which of its lenses, from 0.
    pub id: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<f64>,
}

/// Where a camera's definition file is: the settings it has beyond the
/// protocol's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraDefinition {
    /// An address, without its password. `mftp://` or `mavlinkftp://` is
    /// a file on the vehicle, read over MAVLink FTP.
    pub uri: String,
    pub version: u16,
}

/// How a camera's video stream is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraStreamKind {
    Rtsp,
    /// RTP sent to a UDP port of the ground station.
    RtpUdp,
    /// MPEG-TS fetched over TCP.
    MpegTsTcp,
    /// MPEG-TS sent to a UDP port of the ground station.
    MpegTsUdp,
    /// A kind UXDuck does not know.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraStreamEncoding {
    H264,
    H265,
}

/// One of a camera's video streams, as the camera describes it. What
/// arrives at UXDuck is the video source `sourceId` names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraStream {
    /// The camera's own number for it, from 1.
    pub stream_id: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where it is fetched or sent, without its password.
    pub uri: String,
    pub kind: CameraStreamKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<CameraStreamEncoding>,
    /// The camera says it is sending it now.
    pub running: bool,
    /// A thermal picture.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thermal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate_hz: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u32>,
    /// How far the picture is turned clockwise, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_deg: Option<u16>,
    /// Its horizontal field of view, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hfov_deg: Option<u16>,
    /// The video source it arrives as (`video/list`), once it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

/// A camera's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraMode {
    Photo,
    Video,
    /// Photos for a survey.
    Survey,
}

/// One of a camera's sensors, which its picture comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraSource {
    /// Its daylight sensor.
    Daylight,
    /// Its thermal sensor.
    Thermal,
}

/// What to have a camera do: one command of the MAVLink camera protocol,
/// which the camera acknowledges, or one of its own settings written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CameraAction {
    /// Photo or video mode. Needs the `modes` capability.
    SetMode {
        mode: CameraMode,
    },
    /// One photo. Needs `capture-image`.
    TakePhoto,
    /// A photo every `intervalS` seconds: `count` of them, or until
    /// `stop-interval` when left out.
    StartInterval {
        interval_s: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    StopInterval,
    /// Records on the camera's own card. Needs `capture-video`. A full
    /// card is refused.
    StartRecording,
    StopRecording,
    /// Zooms by steps: in when positive, out when negative. Needs
    /// `basic-zoom`.
    ZoomStep {
        steps: i8,
    },
    /// Zooms to a place in its range, from 0 (widest) to 100.
    ZoomTo {
        percent: f32,
    },
    /// Focuses by steps: farther when positive, nearer when negative.
    /// Needs `basic-focus`.
    FocusStep {
        steps: i8,
    },
    /// Focuses at a place in its range, from 0 (nearest) to 100.
    FocusTo {
        percent: f32,
    },
    /// Lets the camera focus by itself.
    FocusAuto,
    /// Erases a card and the camera's list of photos, for good. The first
    /// card unless `storageId` names another. Refused while it captures.
    Format {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_id: Option<u8>,
    },
    /// Puts the camera's settings back as it was made.
    Reset,
    /// Sets one of the camera's own settings, which its definition file
    /// names (`camera/settings`). `value` is one of the setting's
    /// `options`, or a number within its `min` and `max`. Refused before
    /// it is sent for a setting the file does not name, one that is read
    /// only, and a value its type cannot hold. The camera's own refusal
    /// comes back in its words.
    SetSetting {
        /// The setting's `name`, such as `CAM_ISO`.
        name: String,
        value: f64,
    },
    /// Shows another of the camera's sensors on its picture. Any camera
    /// may be asked, since none says in what it can do whether it has
    /// more than one sensor: a camera without the one asked for refuses
    /// in its own words. The camera's `sources` are the ones it has said
    /// it has. Which one its picture is now is each stream's `thermal`,
    /// read back from the camera after it accepts.
    SetSource {
        source: CameraSource,
    },
    /// Tracks what is at a point in the camera's picture: `x` across it
    /// and `y` down it, from 0 at its left and top to 1. Needs
    /// `tracking-point`. What the camera then tracks is in the vehicle's
    /// telemetry as the camera says it: `camera.tracking` (0 not
    /// tracking, 1 tracking, 2 in error such as a target lost) with
    /// `camera.tracking_x` and `camera.tracking_y`, and, from a camera
    /// with `tracking-geo-status`, `camera.target` with
    /// `camera.target_latitude_deg` and `camera.target_longitude_deg`.
    TrackPoint {
        x: f32,
        y: f32,
    },
    /// Tracks what is in a box in the camera's picture, by its sides from
    /// 0 at the picture's left and top to 1. Needs `tracking-rectangle`.
    /// The box comes back in the vehicle's telemetry as
    /// `camera.tracking_left`, `camera.tracking_top`,
    /// `camera.tracking_right` and `camera.tracking_bottom`.
    TrackRectangle {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    },
    /// Ends the camera's tracking. Needs `tracking-point` or
    /// `tracking-rectangle`.
    StopTracking,
    /// Starts recording when the camera last said it was not, and stops
    /// when it said it was: a controller's one button (S416). Sent and
    /// receipted as `start-recording` or `stop-recording`. Refused while
    /// the camera has not said whether it records. Needs `capture-video`.
    ToggleRecording,
    /// Video mode when the camera last said photo, and photo otherwise
    /// (S416). Sent and receipted as `set-mode`. Refused while the camera
    /// has not said its mode. Needs `modes`.
    ToggleMode,
    /// The daylight picture when the camera last showed its thermal one,
    /// and the thermal one otherwise (S416). Sent and receipted as
    /// `set-source`. Refused while the camera has named no stream.
    ToggleSource,
    /// Zooms on until told otherwise, at `rate` from -1 to 1: in when
    /// positive, out when negative, and 0 stops (S416). A controller's
    /// held zoom sends it. A camera that refuses it is zoomed by steps.
    /// Needs `basic-zoom`.
    ZoomRate {
        rate: f32,
    },
    /// Focuses on until told otherwise, at `rate` from -1 to 1: farther
    /// when positive, nearer when negative, and 0 stops (S416). Needs
    /// `basic-focus`.
    FocusRate {
        rate: f32,
    },
}

/// The type of one of a camera's own settings, as its definition file
/// names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraSettingKind {
    /// On (1) or off (0).
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
    /// Bytes only the camera's maker can read. It has no `value` and
    /// cannot be set.
    Custom,
}

/// One value a setting can take, with the name the definition file gives
/// it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettingOption {
    pub name: String,
    pub value: f64,
}

/// One of a camera's own settings: what its definition file says of it,
/// and the value the camera last gave.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSetting {
    /// Its name on the camera, such as `CAM_ISO`.
    pub name: String,
    /// What the file calls it, for people.
    pub description: String,
    pub kind: CameraSettingKind,
    /// The value the camera last gave. Absent until it has, and for a
    /// `custom` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The values it can take as the camera is set now: fewer than the
    /// file lists when another setting's value limits them. Empty for a
    /// setting that takes a number in a range, and for `bool`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<CameraSettingOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// The camera sets it itself, and it can only be read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// The file asks that it is given no control of its own, as it does
    /// for the camera's mode, which `set-mode` sets.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The setting whose present value rules this one out, by name: it
    /// does not apply as the camera is set now. Absent when it applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_by: Option<String>,
}

/// Where UXDuck is with a camera's definition file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraDefinitionStatus {
    /// UXDuck has no definition file for the camera: it names none, so it
    /// has no settings beyond its mode, zoom and focus, or `reason` says
    /// why the one it names is not read.
    None,
    /// The file is at an `http` or `https` address, which UXDuck fetches
    /// only once a person agrees (`camera/definition/fetch`).
    Ask,
    /// It is being fetched.
    Fetching,
    /// It could not be fetched. `reason` says why.
    Failed,
    /// It was fetched and UXDuck refused it. `reason` says why.
    Refused,
    /// It is read, and `settings` has what it names.
    Loaded,
}

/// `camera/settings`: which camera.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettingsParams {
    /// The camera's `id` from `camera/list`.
    pub camera_id: String,
    /// Asks the camera for every value again. The answer comes at once
    /// with the values it had, and `reading` says they are being read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

/// `camera/definition/fetch`: which camera's definition file to fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraDefinitionFetchParams {
    /// The camera's `id` from `camera/list`.
    pub camera_id: String,
    /// The address agreed to, as `camera/settings` gave it. Refused when
    /// the camera has named another since.
    pub uri: String,
}

/// `camera/settings`: a camera's own settings, from its definition file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettingsResult {
    pub camera_id: String,
    pub status: CameraDefinitionStatus,
    /// Where the camera says the file is, without its password. Absent
    /// when it names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Why it could not be fetched, was refused or is not read, as a
    /// sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The file is UXDuck's own copy, kept from when it was last fetched
    /// for this maker, model and version.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept: bool,
    /// The camera is being asked for its values now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reading: bool,
    /// Why the camera's values could not all be read, when they could not.
    /// A camera that lists more settings than the 1,024 names UXDuck
    /// keeps from one camera is said here too, with how many it read and
    /// how many it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_failure: Option<String>,
    /// How much of the file's text is shown cut, as a sentence, when any
    /// is. A name, a unit or a description longer than 256 characters is
    /// kept to that length and ends in `…`, and this says how many were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_cut: Option<String>,
    /// The settings the file names, in its order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<CameraSetting>,
}

/// A camera's mode, zoom and focus as it last said them. Each is absent
/// when the camera does not say.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CameraMode>,
    /// From 0 (widest) to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom_pct: Option<f64>,
    /// From 0 (nearest) to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_pct: Option<f64>,
}

/// What a camera is capturing, as it last said.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraCapture {
    /// Seconds between photos while it takes them at an interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_s: Option<f64>,
    pub recording: bool,
    /// When the recording began by the camera's own count of it, in
    /// microseconds since 1970 on this computer's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub recording_since_us: Option<u64>,
    /// The photos in the camera's list, which a format empties.
    pub photo_count: u32,
    /// Photos it counted and never described, though UXDuck asked for
    /// each of them three times.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_lost: u32,
    /// Photos it counted that UXDuck did not ask for. UXDuck asks for a
    /// photo the camera counts and does not describe, however many are
    /// missing at once, a few at a time. Once the camera has described
    /// none of 64 in a row, each asked for three times, it does not answer
    /// such requests. UXDuck then asks this camera for no more, and counts
    /// here the ones it did not ask for.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_not_asked: u32,
    /// Photos it counts and has not described yet, which UXDuck is asking
    /// for or will. They join `photos` as the camera describes them, or
    /// `photosLost` or `photosNotAsked`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_awaited: u32,
    /// Room left on its card, in mebibytes, in coarse steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_mib: Option<f64>,
    /// The card is known to have no room for another capture.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub card_full: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraStorageStatus {
    /// No card.
    Empty,
    Unformatted,
    Ready,
    /// The camera does not say.
    NotSupported,
}

/// A camera's card as it last said. Sizes are in mebibytes, and 0 unless
/// the card is `ready`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraStorage {
    /// 1 for the first card.
    pub storage_id: u8,
    /// How many cards it has.
    pub count: u8,
    pub status: CameraStorageStatus,
    pub total_mib: f64,
    pub used_mib: f64,
    pub available_mib: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where a photo was taken.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraPhotoPlace {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_amsl_m: f64,
    /// Its height above home.
    pub relative_altitude_m: f64,
}

/// A photo a camera said it took.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraPhoto {
    /// Its place in the camera's list, from 0.
    pub index: u32,
    /// When it was taken by the camera's clock when that is set, else when
    /// UXDuck heard of it: microseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub taken_at_us: u64,
    /// Absent when the camera did not know where it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<CameraPhotoPlace>,
    /// False when the camera said the capture failed.
    pub ok: bool,
    /// The file's name or address on the camera, without its password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// `camera/images`: which of a camera's photos.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraImagesParams {
    /// The camera's `id` from `camera/list`.
    pub camera_id: String,
    /// Only photos before this place in the camera's list: the `index` of
    /// the first photo already read, from `camera/list` or an earlier
    /// answer. None for the newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    /// How many to answer: 50 when omitted, at most 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `camera/images`: one page of the photos a camera said it took.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraImagesResult {
    pub camera_id: String,
    /// The page, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub photos: Vec<CameraPhoto>,
    /// How many photos UXDuck has of this camera, as `photosListed` says.
    pub total: u32,
    /// How many of them come before this page's first. Read on with that
    /// photo's `index` as `before` until this is 0.
    pub earlier: u32,
    /// How many older photos the camera described that are not listed, as
    /// `photosDropped` says. Absent when every photo is listed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped: u32,
    /// The same in words for a person, as `photosNotListed` says: how
    /// many photos are not listed, why, and that they stay on the
    /// camera's card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_listed: Option<String>,
}

/// `camera/command`: has a camera do something.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraCommandParams {
    /// The camera's `id` from `camera/list`.
    pub camera_id: String,
    pub action: CameraAction,
    /// Confirms the command for a vehicle that is not online.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// A camera of a vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Camera {
    /// `<vehicle id>/<component id>`, and `/<device id>` after it for a
    /// camera its autopilot speaks for.
    pub id: String,
    pub vehicle_id: String,
    /// The camera's own MAVLink component id, or its autopilot's when the
    /// autopilot speaks for it.
    pub component_id: u8,
    /// 1 to 6 for a camera its autopilot speaks for (`camera_device_id`).
    /// Absent for a camera that is a MAVLink component itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<u8>,
    /// Whether its heartbeats, or its autopilot's, are heard now. A camera
    /// that is gone is kept, and identified again when it returns.
    pub present: bool,
    /// Whether it has said what it is. One that has not is a camera
    /// component that sends heartbeats and has not answered yet, or does
    /// not answer.
    pub identified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Its firmware version, such as `1.2.3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// What it can do. Empty until it is identified.
    pub capabilities: Vec<CameraCapability>,
    /// Its `CAMERA_CAP_FLAGS` as sent, for a bit UXDuck has no name for.
    pub flags: u32,
    /// It only sends video: it has a video stream and nothing to control.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub video_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<CameraSensor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens: Option<CameraLens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<CameraDefinition>,
    /// The gimbal it sits on: a gimbal device's component id, or 1 to 6
    /// for one its autopilot speaks for. Absent for a camera fixed to the
    /// airframe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gimbal_device_id: Option<u8>,
    pub streams: Vec<CameraStream>,
    /// Its mode, zoom and focus, once it has said them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<CameraSettings>,
    /// What it is capturing, once it has said: an interval, a recording
    /// and since when, its count of photos and the room on its card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CameraCapture>,
    /// Its card, once it has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<CameraStorage>,
    /// The photos it said it took while UXDuck listened, oldest first: the
    /// newest 20 of them, each with where it was taken. `photosListed`
    /// says how many there are, and `camera/images` reads the ones before
    /// these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub photos: Vec<CameraPhoto>,
    /// How many photos UXDuck lists of it: the ones it described while
    /// UXDuck listened, since its list was last emptied by a format, and
    /// at most the newest 100,000 of them. UXDuck keeps them until it
    /// stops. Absent when there is none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_listed: u32,
    /// How many older photos it described that are not listed. UXDuck
    /// keeps at most the newest 100,000 photos of one camera, because a
    /// camera describes as many as it likes on its MAVLink stream and
    /// each is held in memory. Past that the photo with the lowest
    /// `index` leaves the list, and stays on the camera's card. Each is
    /// counted once: one first described among indexes already dropped is
    /// not counted, since nothing of a dropped photo is kept to tell it
    /// from one described twice. Absent when every photo is listed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub photos_dropped: u32,
    /// `photosDropped` in words for a person, when it is not 0: how many
    /// photos are not listed, why, and that they stay on the camera's
    /// card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photos_not_listed: Option<String>,
    /// The sensors it has said it has since UXDuck first heard it:
    /// `thermal` from a thermal range it can give or a stream that is
    /// thermal, and `daylight` from a stream that is not. A camera with
    /// both can be switched with `set-source`. Absent until it names a
    /// stream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<CameraSource>,
    /// The receipt of UXDuck's own Start recording that it did not send to
    /// this camera, because the camera is in photo mode and says it
    /// records in video mode only. Present for as long as UXDuck means the
    /// vehicle's camera to record along with its own recording ("Also save
    /// on the vehicle"). UXDuck does not switch the camera's mode: send
    /// `set-mode` and `start-recording` to have it record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_not_asked: Option<String>,
    /// The video source whose picture it records on the vehicle, for a
    /// camera that names no stream of its own: `uxduck companion`'s
    /// recording camera, whose model names the picture's path (ADR-0066
    /// R5). It starts and stops by the vehicle's own mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// The latest warning `uxduck companion`'s recording camera said, such
    /// as "Deleted 450 MB of earlier recordings to keep recording, 160 MB
    /// free" (ADR-0066 R11), until it next starts recording. The Record
    /// menu's vehicle line shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `camera/list`: every camera of every vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraListResult {
    pub cameras: Vec<Camera>,
}

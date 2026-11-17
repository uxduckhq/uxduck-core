//! The camera component (MAVLink camera protocol, version 2) as plain
//! state: commands in, frames out, and a step on a clock it is given.
//! Nothing here belongs to a simulator alone, so a component on a real link
//! that answers as a camera can speak with it.

use std::collections::BTreeMap;

use mavio::dialects::ardupilotmega::enums::{
    CameraCapFlags, CameraMode, CameraTrackingMode, CameraTrackingStatusFlags,
    CameraTrackingTargetData, MavAutopilot, MavModeFlag, MavState, MavType, StorageStatus,
    StorageType, StorageUsageFlag, VideoStreamEncoding, VideoStreamStatusFlags, VideoStreamType,
};
use mavio::dialects::ardupilotmega::messages::{
    CameraCaptureStatus, CameraFovStatus, CameraImageCaptured, CameraInformation, CameraSettings,
    CameraThermalRange, CameraTrackingGeoStatus, CameraTrackingImageStatus, Heartbeat,
    StorageInformation, VideoStreamInformation, VideoStreamStatus,
};

use super::faults::{Answers, Due, Met};
use super::wire::{self, ACCEPTED, Command, DENIED, FAILED, TEMPORARILY_REJECTED};
use super::{Every, Place, elapsed, refusal};

pub mod settings;
pub(super) mod tracking;

/// `MAV_COMP_ID_CAMERA`.
pub const COMPONENT: u8 = 100;

// The camera protocol's commands. The first six are the ones
// `MAV_CMD_REQUEST_MESSAGE` replaced, which the guide asks a camera to
// answer still.
const REQUEST_CAMERA_INFORMATION: u16 = 521;
const REQUEST_CAMERA_SETTINGS: u16 = 522;
const REQUEST_STORAGE_INFORMATION: u16 = 525;
const REQUEST_CAMERA_CAPTURE_STATUS: u16 = 527;
const REQUEST_VIDEO_STREAM_INFORMATION: u16 = 2504;
const REQUEST_VIDEO_STREAM_STATUS: u16 = 2505;
const STORAGE_FORMAT: u16 = 526;
const RESET_CAMERA_SETTINGS: u16 = 529;
const SET_CAMERA_MODE: u16 = 530;
const SET_CAMERA_ZOOM: u16 = 531;
const SET_CAMERA_FOCUS: u16 = 532;
const SET_CAMERA_SOURCE: u16 = 534;
const IMAGE_START_CAPTURE: u16 = 2000;
const IMAGE_STOP_CAPTURE: u16 = 2001;
const VIDEO_START_CAPTURE: u16 = 2500;
const VIDEO_STOP_CAPTURE: u16 = 2501;
const VIDEO_START_STREAMING: u16 = 2502;
const VIDEO_STOP_STREAMING: u16 = 2503;
const CAMERA_TRACK_POINT: u16 = 2004;
const CAMERA_TRACK_RECTANGLE: u16 = 2005;
const CAMERA_STOP_TRACKING: u16 = 2010;

const CAMERA_INFORMATION: u32 = 259;
const CAMERA_SETTINGS: u32 = 260;
const STORAGE_INFORMATION: u32 = 261;
const CAMERA_CAPTURE_STATUS: u32 = 262;
const CAMERA_IMAGE_CAPTURED: u32 = 263;
const VIDEO_STREAM_INFORMATION: u32 = 269;
const VIDEO_STREAM_STATUS: u32 = 270;
const CAMERA_FOV_STATUS: u32 = 271;
const CAMERA_TRACKING_IMAGE_STATUS: u32 = 275;
const CAMERA_TRACKING_GEO_STATUS: u32 = 276;
const CAMERA_THERMAL_RANGE: u32 = 277;

/// `VIDEO_STREAM_STATUS_FLAGS`: the picture is a thermal one, and its
/// range can be asked for. The second is newer than mavio's dialect.
const STREAM_THERMAL: u8 = 2;
const STREAM_THERMAL_RANGE_ENABLED: u8 = 4;
/// `CAMERA_SOURCE`: the camera's own choice, its daylight sensor, and its
/// thermal one.
const SOURCE_DEFAULT: u32 = 0;
const SOURCE_RGB: u32 = 1;
const SOURCE_IR: u32 = 2;
/// The two ends of the scale its thermal picture is drawn on, as
/// `CAMERA_THERMAL_RANGE` gives them in degrees Celsius. The picture is
/// the daylight one in false colour: nothing in it is a temperature, and
/// the scale's numbers are its darkest and its brightest and no more.
pub const THERMAL_SCALE_C: (f32, f32) = (0.0, 100.0);

/// The lens from its widest to its longest, in millimetres.
const FOCAL_MM: (f32, f32) = (16.0, 80.0);
/// The sensor, across and down, in millimetres and in pixels.
const SENSOR_MM: (f32, f32) = (13.2, 8.8);
const SENSOR_PX: (u16, u16) = (5472, 3648);
/// The card, in MiB. What an image and a second of video take of it is
/// set by the camera's own settings (`settings`): 8 and 1.5 as it starts.
const CARD_MIB: f32 = 32_000.0;
/// The shortest time between two images of a sequence, in seconds.
const SHORTEST_INTERVAL_S: f32 = 0.5;
/// A step of zoom or focus, and how fast they run while held, in percent.
const STEP_PCT: f32 = 10.0;
const RUN_PCT_PER_S: f32 = 25.0;
/// The nearest it focuses, in metres.
const NEAREST_M: f32 = 0.5;
/// The fastest it sends a message again and again.
const FASTEST_MS: u64 = 20;

/// A message the camera sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    Information,
    Settings,
    Storage,
    CaptureStatus,
    /// An image of its log, by index.
    Image(usize),
    StreamInformation,
    StreamStatus,
    Fov,
    TrackingImage,
    TrackingGeo,
    ThermalRange,
}

impl Report {
    /// The message with this id, of those asked for by the id alone.
    fn of(message: u32) -> Option<Self> {
        match message {
            CAMERA_INFORMATION => Some(Self::Information),
            CAMERA_SETTINGS => Some(Self::Settings),
            STORAGE_INFORMATION => Some(Self::Storage),
            CAMERA_CAPTURE_STATUS => Some(Self::CaptureStatus),
            VIDEO_STREAM_INFORMATION => Some(Self::StreamInformation),
            VIDEO_STREAM_STATUS => Some(Self::StreamStatus),
            CAMERA_FOV_STATUS => Some(Self::Fov),
            CAMERA_TRACKING_IMAGE_STATUS => Some(Self::TrackingImage),
            CAMERA_TRACKING_GEO_STATUS => Some(Self::TrackingGeo),
            CAMERA_THERMAL_RANGE => Some(Self::ThermalRange),
            _ => None,
        }
    }
}

/// What a camera's video shows, for whoever draws its picture (S408):
/// from where, which way, and how wide. The truth of the simulation, as
/// the camera itself has it, and not what telemetry says of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub place: Place,
    /// A quaternion (w, x, y, z) from north, east and down.
    pub attitude: [f32; 4],
    /// The field of view across the picture, in degrees.
    pub hfov_deg: f32,
    /// The picture is its thermal one (ADR-0063 Y5), drawn in false colour.
    pub thermal: bool,
}

/// What became of what it tracks, for a gimbal in the same unit to follow
/// or to leave.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Tracked {
    /// A place on the ground: degrees, and metres above sea level.
    Taken(f64, f64, f64),
    Dropped,
}

/// An image it took: when, where and looking which way.
#[derive(Clone, Copy, Debug)]
struct Image {
    time_boot_ms: u32,
    place: Option<Place>,
    attitude: [f32; 4],
}

/// Images at an interval, and how many are still to come. None counts
/// for ever.
#[derive(Clone, Copy, Debug)]
struct Interval {
    every: Every,
    left: Option<u32>,
}

/// A video being recorded: since when, and how often it says so.
#[derive(Clone, Copy, Debug)]
struct Recording {
    since_ms: u64,
    status: Option<Every>,
}

/// A camera that takes images, records video, zooms and focuses, and keeps
/// count of all of it.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one thing the camera is or is doing"
)]
pub struct Camera {
    system: u8,
    /// `MAV_COMP_ID_CAMERA` unless set.
    pub component: u8,
    /// The video stream it names when asked, if it has one.
    pub stream: Option<String>,
    /// The stream is RTP over UDP (`VIDEO_STREAM_TYPE_RTPUDP`), as the
    /// picture of a simulation's camera is sent (S408), and not MPEG-TS.
    pub stream_rtp: bool,
    /// It only sends video (S404): it has no shutter, modes, lens or card,
    /// says so in what it can do, and has none of their commands.
    pub video_only: bool,
    /// It takes photos only in photo mode and records only in video mode,
    /// and has no zoom or focus to drive (ADR-0063 Y16). It says so in what
    /// it can do.
    pub mode_bound: bool,
    /// It has a thermal sensor beside its daylight one (ADR-0063 Y5): one
    /// stream that shows either, chosen with `MAV_CMD_SET_CAMERA_SOURCE`,
    /// and the range of the thermal picture when asked.
    pub thermal_sensor: bool,
    /// It tracks a point or a box in its picture (ADR-0063 Y5), and says
    /// where that is in the picture and on the ground.
    pub tracks: bool,
    /// The component id of the gimbal device it sits on, 1 to 6 for a
    /// gimbal that is no component of its own, or 0 for a camera fixed to
    /// the airframe.
    pub gimbal_device_id: u8,
    /// Where it is, when that is known. Images carry it.
    pub place: Option<Place>,
    /// Which way it looks: a quaternion (w, x, y, z) from north, east and
    /// down.
    pub attitude: [f32; 4],
    /// Where it says its definition file is (S411): its own, which it
    /// serves over MAVLink FTP, unless set. None names no file, and the
    /// camera then has no settings beyond the protocol's own.
    pub definition_uri: Option<String>,
    /// The files it serves over MAVLink FTP: its definition file.
    files: crate::fake_files::FakeFiles,
    /// The settings its definition file names.
    settings: settings::Settings,
    /// The photos it took since it was switched on.
    shots: u32,
    /// The last MAVLink sequence number it sent.
    pub(super) sent: u8,
    now_ms: u64,
    /// The commands a fault has it refuse or take its time over.
    pub(super) answers: Answers,
    /// What was on its card before a fault filled it.
    before_full: Option<f32>,
    video_mode: bool,
    /// Zoom and focus as percentages of their range, and how fast each is
    /// running, percent a second.
    zoom: f32,
    zooming: f32,
    focus: f32,
    focusing: f32,
    interval: Option<Interval>,
    /// The capture last started: who asked, with which interval, count and
    /// sequence number.
    asked: Option<((u8, u8), [f32; 3])>,
    /// Its image log, which only a format or a reset empties.
    images: Vec<Image>,
    recording: Option<Recording>,
    used_mib: f32,
    /// The messages it sends again and again, by id.
    streamed: BTreeMap<u32, Every>,
    /// Its stream shows the thermal sensor's picture.
    thermal_shown: bool,
    /// What it tracks, and whether that is in its picture now.
    target: Option<tracking::Target>,
    target_seen: Option<tracking::Seen>,
    /// A target taken or dropped that a gimbal in its unit has not heard
    /// of yet.
    tracked: Option<Tracked>,
}

impl Camera {
    /// A camera of the vehicle with this system id.
    #[must_use]
    pub fn new(system: u8) -> Self {
        Self {
            system,
            component: COMPONENT,
            stream: None,
            stream_rtp: false,
            video_only: false,
            mode_bound: false,
            thermal_sensor: false,
            tracks: false,
            gimbal_device_id: 0,
            place: None,
            attitude: [1.0, 0.0, 0.0, 0.0],
            definition_uri: Some(settings::DEFINITION_URI.to_owned()),
            files: Self::own_files(settings::DEFINITION.as_bytes()),
            settings: settings::Settings::default(),
            shots: 0,
            sent: 0,
            now_ms: 0,
            answers: Answers::default(),
            before_full: None,
            video_mode: false,
            zoom: 0.0,
            zooming: 0.0,
            focus: 100.0,
            focusing: 0.0,
            interval: None,
            asked: None,
            images: Vec::new(),
            recording: None,
            used_mib: 0.0,
            streamed: BTreeMap::new(),
            thermal_shown: false,
            target: None,
            target_seen: None,
            tracked: None,
        }
    }

    /// Leaves this much room on its card, in mebibytes, to see what a card
    /// nearly or wholly full does (S405).
    pub fn leave_room(&mut self, mib: f32) {
        self.used_mib = (CARD_MIB - mib).clamp(0.0, CARD_MIB);
    }

    /// Fills the card as a fault (ADR-0063 Y16), or puts back what was on
    /// it before. A full card fails photos and recordings and ends the
    /// ones under way. Formatting it empties it, and the fault is over. A
    /// camera that only sends video has no card to fill.
    pub fn fill_card(&mut self, full: bool) {
        if !full {
            if let Some(before) = self.before_full.take() {
                self.used_mib = before;
            }
        } else if self.before_full.is_none() && !self.video_only {
            self.before_full = Some(self.used_mib);
            self.used_mib = CARD_MIB;
        }
    }

    /// Whether its card is full because a fault filled it.
    #[must_use]
    pub fn card_filled(&self) -> bool {
        self.before_full.is_some()
    }

    /// The frame to send about once a second.
    pub fn heartbeat(&mut self) -> Vec<u8> {
        self.send(&Heartbeat {
            type_: MavType::Camera,
            autopilot: MavAutopilot::Invalid,
            base_mode: MavModeFlag::empty(),
            custom_mode: 0,
            system_status: MavState::Active,
            mavlink_version: 3,
        })
    }

    /// Carries out a command that names this component or every component:
    /// its acknowledgement first, then the messages it asked for or caused.
    pub fn command(&mut self, command: &Command) -> Vec<Vec<u8>> {
        self.tried(command).unwrap_or_else(|| {
            refusal(command)
                .map(|result| self.acknowledge(command, result))
                .into_iter()
                .collect()
        })
    }

    /// The same, or none for a command the camera does not have, so that a
    /// unit with more than a camera on this component can try its other
    /// part.
    pub(super) fn tried(&mut self, command: &Command) -> Option<Vec<Vec<u8>>> {
        match self.faulted(command) {
            Some(answer) => Some(vec![answer]),
            None => self.carried_out(command),
        }
    }

    fn carried_out(&mut self, command: &Command) -> Option<Vec<Vec<u8>>> {
        let (result, reports) = self.obey(command)?;
        let mut frames = vec![self.acknowledge(command, result)];
        frames.extend(reports.into_iter().map(|report| self.report(report)));
        Some(frames)
    }

    /// A fault's answer to a command the camera has, in place of carrying
    /// it out (ADR-0063 Y16): refused, or taken and still being worked on.
    fn faulted(&mut self, command: &Command) -> Option<Vec<u8>> {
        // Whether it has the command is what a copy of it makes of it.
        if !self.answers.concerns(command.command) || self.clone().obey(command).is_none() {
            return None;
        }
        Some(match self.answers.meets(command, self.now_ms)? {
            Met::Denied => self.acknowledge(command, DENIED),
            Met::Busy => self.acknowledge(command, TEMPORARILY_REJECTED),
            Met::Working(percent) => self.progressed(command, percent),
        })
    }

    /// What a command a fault made slow owes now: how far along it is, or
    /// its end, where it is carried out and its true result follows.
    fn slowed(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        match self.answers.due(now_ms) {
            None => Vec::new(),
            Some(Due::Progress(command, percent)) => vec![self.progressed(&command, percent)],
            Some(Due::Done(command)) => self.carried_out(&command).unwrap_or_default(),
        }
    }

    fn progressed(&mut self, command: &Command, percent: u8) -> Vec<u8> {
        self.sent = self.sent.wrapping_add(1);
        wire::in_progress(
            (self.system, self.component, self.sent),
            command.from,
            command.command,
            percent,
        )
    }

    /// Moves the camera to `now_ms` on the clock it is given, milliseconds
    /// since the start: zoom and focus run, the card fills, and the images
    /// and messages that are due come out.
    pub fn step(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let seconds = elapsed(self.now_ms, now_ms);
        self.now_ms = now_ms;
        self.zoom = (self.zoom + self.zooming * seconds).clamp(0.0, 100.0);
        self.focus = (self.focus + self.focusing * seconds).clamp(0.0, 100.0);
        let mut reports = Vec::new();
        if let Some(recording) = &mut self.recording {
            self.used_mib =
                (self.used_mib + self.settings.video_mib_per_s() * seconds).min(CARD_MIB);
            if recording
                .status
                .as_mut()
                .is_some_and(|every| every.due(now_ms))
            {
                reports.push(Report::CaptureStatus);
            }
            // A full card ends the recording.
            if self.used_mib >= CARD_MIB {
                self.recording = None;
            }
        }
        let mut interval = self.interval.take();
        if let Some(sequence) = &mut interval
            && sequence.every.due(now_ms)
        {
            let taken = self.capture();
            reports.extend(taken);
            sequence.left = sequence.left.map(|left| left.saturating_sub(1));
            // A full card ends the sequence, and so does its last image.
            if taken.is_none() || sequence.left == Some(0) {
                interval = None;
            }
        }
        self.interval = interval;
        self.find_target();
        let due: Vec<u32> = self
            .streamed
            .iter_mut()
            .filter_map(|(message, every)| every.due(now_ms).then_some(*message))
            .collect();
        reports.extend(
            due.into_iter()
                .filter_map(Report::of)
                .filter(|report| self.says_now(*report)),
        );
        let mut frames = self.settings_step();
        frames.extend(reports.into_iter().map(|report| self.report(report)));
        frames.extend(self.slowed(now_ms));
        frames
    }

    /// What a command comes to and the messages it brings, or none for a
    /// command the camera does not have.
    fn obey(&mut self, command: &Command) -> Option<(u8, Vec<Report>)> {
        let [p1, p2, p3, ..] = command.params;
        let plain = |result: u8| Some((result, Vec::new()));
        // A change of settings is followed by the settings.
        let settings = |result: u8| {
            let reports = if result == ACCEPTED {
                vec![Report::Settings]
            } else {
                Vec::new()
            };
            Some((result, reports))
        };
        if self.video_only
            && !matches!(
                command.command,
                wire::REQUEST_MESSAGE
                    | REQUEST_CAMERA_INFORMATION
                    | REQUEST_VIDEO_STREAM_INFORMATION
                    | REQUEST_VIDEO_STREAM_STATUS
                    | wire::SET_MESSAGE_INTERVAL
                    | VIDEO_START_STREAMING
                    | VIDEO_STOP_STREAMING
            )
        {
            return None;
        }
        // One that captures only in the mode for it has no lens to drive,
        // and fails a capture asked for in the other mode. The mode must
        // change before the same command can work.
        if self.mode_bound {
            match command.command {
                SET_CAMERA_ZOOM | SET_CAMERA_FOCUS => return None,
                IMAGE_START_CAPTURE if self.video_mode => return plain(FAILED),
                VIDEO_START_CAPTURE if !self.video_mode => return plain(FAILED),
                _ => {}
            }
        }
        match command.command {
            wire::REQUEST_MESSAGE => self.request(wire::whole(p1), p2),
            REQUEST_CAMERA_INFORMATION => self.requested(p1, CAMERA_INFORMATION, 0.0),
            REQUEST_CAMERA_SETTINGS => self.requested(p1, CAMERA_SETTINGS, 0.0),
            REQUEST_STORAGE_INFORMATION => self.requested(p2, STORAGE_INFORMATION, p1),
            REQUEST_CAMERA_CAPTURE_STATUS => self.requested(p1, CAMERA_CAPTURE_STATUS, 0.0),
            REQUEST_VIDEO_STREAM_INFORMATION => self.request(VIDEO_STREAM_INFORMATION, p1),
            REQUEST_VIDEO_STREAM_STATUS => self.request(VIDEO_STREAM_STATUS, p1),
            wire::SET_MESSAGE_INTERVAL => {
                let report = Report::of(wire::whole(p1)).filter(|report| self.has(*report))?;
                plain(self.send_every(report, wire::whole(p1), p2))
            }
            // One sensor: the id of all of them or of the first.
            IMAGE_START_CAPTURE | IMAGE_STOP_CAPTURE if !first_or_all(p1) => plain(DENIED),
            IMAGE_START_CAPTURE => Some(self.start_images(command)),
            IMAGE_STOP_CAPTURE => {
                self.interval = None;
                plain(ACCEPTED)
            }
            VIDEO_START_CAPTURE => plain(self.start_video(p1, p2)),
            VIDEO_STOP_CAPTURE => {
                self.recording = None;
                plain(ACCEPTED)
            }
            // The stream is there to connect to whenever it is named. The
            // guide asks that these are acknowledged all the same.
            VIDEO_START_STREAMING | VIDEO_STOP_STREAMING => {
                self.stream.as_ref()?;
                plain(if first_or_all(p1) { ACCEPTED } else { DENIED })
            }
            SET_CAMERA_MODE => settings(self.set_mode(p2)),
            SET_CAMERA_ZOOM => settings(self.set_zoom(p1, p2)),
            SET_CAMERA_FOCUS => settings(self.set_focus(p1, p2)),
            SET_CAMERA_SOURCE if self.thermal_sensor => Some(self.set_source(p1, p2, p3)),
            CAMERA_TRACK_POINT if self.tracks => Some(self.track_point(command)),
            CAMERA_TRACK_RECTANGLE if self.tracks => Some(self.track_rectangle(command)),
            CAMERA_STOP_TRACKING if self.tracks => Some(self.stop_tracking(p1)),
            RESET_CAMERA_SETTINGS => match flag(p1) {
                Some(true) => {
                    self.video_mode = false;
                    (self.zoom, self.zooming) = (0.0, 0.0);
                    (self.focus, self.focusing) = (100.0, 0.0);
                    self.settings = settings::Settings::default();
                    settings(ACCEPTED)
                }
                Some(false) => plain(ACCEPTED),
                None => plain(DENIED),
            },
            STORAGE_FORMAT => Some(self.format(p1, flag(p2), flag(p3))),
            _ => None,
        }
    }

    /// `MAV_CMD_REQUEST_MESSAGE`: the message with this id and its own
    /// parameter, which is an image's index, a stream's id or the card's.
    fn request(&self, message: u32, param: f32) -> Option<(u8, Vec<Report>)> {
        let report = if message == CAMERA_IMAGE_CAPTURED {
            Report::Image(wire::whole(param) as usize)
        } else {
            Report::of(message)?
        };
        if !self.has(report) {
            return None;
        }
        let there = match report {
            Report::Image(index) => !param.is_nan() && index < self.images.len(),
            Report::Storage => first_or_all(param),
            Report::StreamInformation | Report::StreamStatus => {
                self.stream.is_some() && first_or_all(param)
            }
            // The range of the thermal picture, while that is the picture.
            Report::ThermalRange => self.says_now(report) && first_or_all(param),
            _ => true,
        };
        Some(if there {
            (ACCEPTED, vec![report])
        } else {
            (DENIED, Vec::new())
        })
    }

    /// Whether it has a message at all. One that only sends video has what
    /// says so and its stream's. Only one that tracks has a tracking
    /// status, and only one with a thermal sensor a thermal range.
    fn has(&self, report: Report) -> bool {
        match report {
            Report::TrackingImage | Report::TrackingGeo => self.tracks,
            Report::ThermalRange => self.thermal_sensor,
            Report::Information | Report::StreamInformation | Report::StreamStatus => true,
            _ => !self.video_only,
        }
    }

    /// Whether a message asked for again and again is sent now. The
    /// tracking status is sent while it tracks, and the thermal range
    /// while the picture is the thermal one, as the two messages say of
    /// themselves.
    fn says_now(&self, report: Report) -> bool {
        match report {
            Report::TrackingImage | Report::TrackingGeo => self.target.is_some(),
            Report::ThermalRange => self.thermal_shown && self.stream.is_some(),
            _ => true,
        }
    }

    /// `MAV_CMD_SET_CAMERA_SOURCE`: the daylight sensor or the thermal one
    /// on its one stream. It has no picture in picture, so a second source
    /// is one it cannot show. Its stream's status follows, which says
    /// which picture it is.
    fn set_source(&mut self, camera: f32, primary: f32, secondary: f32) -> (u8, Vec<Report>) {
        let thermal = match exact(primary) {
            Some(SOURCE_DEFAULT | SOURCE_RGB) => false,
            Some(SOURCE_IR) => true,
            _ => return (DENIED, Vec::new()),
        };
        let alone = secondary.is_nan() || exact(secondary) == Some(SOURCE_DEFAULT);
        if !alone || !self.named(camera) {
            return (DENIED, Vec::new());
        }
        self.thermal_shown = thermal;
        let reports = if self.stream.is_some() {
            vec![Report::StreamStatus]
        } else {
            Vec::new()
        };
        (ACCEPTED, reports)
    }

    /// `MAV_CMD_CAMERA_TRACK_POINT`: a point in the picture, from 0 at its
    /// left and top to 1, and a radius as a share of its width.
    fn track_point(&mut self, command: &Command) -> (u8, Vec<Report>) {
        let [x, y, radius, camera, ..] = command.params;
        let radius = if radius.is_nan() { 0.0 } else { radius };
        let within = |share: f32| (0.0..=1.0).contains(&share);
        if !(within(x) && within(y) && within(radius) && self.named(camera)) {
            return (DENIED, Vec::new());
        }
        self.take_target((x, y), radius, None)
    }

    /// `MAV_CMD_CAMERA_TRACK_RECTANGLE`: a box in the picture by its left
    /// and top, then its right and bottom. Its middle is followed.
    fn track_rectangle(&mut self, command: &Command) -> (u8, Vec<Report>) {
        let [left, top, right, bottom, camera, ..] = command.params;
        let within = |share: f32| (0.0..=1.0).contains(&share);
        let drawn =
            [left, top, right, bottom].into_iter().all(within) && left < right && top < bottom;
        if !(drawn && self.named(camera)) {
            return (DENIED, Vec::new());
        }
        let middle = (f32::midpoint(left, right), f32::midpoint(top, bottom));
        self.take_target(middle, 0.0, Some((right - left, bottom - top)))
    }

    /// Takes the ground under a place in the picture as what it tracks. It
    /// cannot before it knows where it is, and fails where the place is
    /// sky. A target it had is dropped for the new one.
    fn take_target(
        &mut self,
        at: (f32, f32),
        radius: f32,
        size: Option<(f32, f32)>,
    ) -> (u8, Vec<Report>) {
        let Some(view) = self.view() else {
            return (TEMPORARILY_REJECTED, Vec::new());
        };
        let Some(target) = tracking::Target::under(&view, at, radius, size) else {
            return (FAILED, Vec::new());
        };
        self.target = Some(target);
        self.tracked = Some(Tracked::Taken(target.lat, target.lon, target.alt_m));
        self.find_target();
        (ACCEPTED, Vec::new())
    }

    /// `MAV_CMD_CAMERA_STOP_TRACKING`. One last status says it tracks
    /// nothing, where the status was being sent.
    fn stop_tracking(&mut self, camera: f32) -> (u8, Vec<Report>) {
        if !self.named(camera) {
            return (DENIED, Vec::new());
        }
        let reports = if self.target.take().is_some() {
            self.tracked = Some(Tracked::Dropped);
            [
                (CAMERA_TRACKING_IMAGE_STATUS, Report::TrackingImage),
                (CAMERA_TRACKING_GEO_STATUS, Report::TrackingGeo),
            ]
            .into_iter()
            .filter_map(|(message, report)| self.streamed.contains_key(&message).then_some(report))
            .collect()
        } else {
            Vec::new()
        };
        self.target_seen = None;
        (ACCEPTED, reports)
    }

    /// Finds what it tracks in the picture as it is now.
    fn find_target(&mut self) {
        self.target_seen = match (self.target, self.view()) {
            (Some(target), Some(view)) => target.seen(&view),
            _ => None,
        };
    }

    /// A target taken or dropped since this was last asked, for a gimbal
    /// in the same unit.
    pub(super) fn take_tracked(&mut self) -> Option<Tracked> {
        self.tracked.take()
    }

    /// Whether a camera id in a command is this camera's: every camera's,
    /// its own component's, or one left unset.
    #[allow(clippy::float_cmp, reason = "small whole numbers sent as floats")]
    fn named(&self, camera: f32) -> bool {
        camera.is_nan() || camera == 0.0 || camera == f32::from(self.component)
    }

    /// One of the request commands from before `MAV_CMD_REQUEST_MESSAGE`:
    /// 1 asks, 0 asks nothing and anything else is not a value it takes.
    fn requested(&self, asks: f32, message: u32, param: f32) -> Option<(u8, Vec<Report>)> {
        match flag(asks) {
            Some(true) => self.request(message, param),
            Some(false) => Some((ACCEPTED, Vec::new())),
            None => Some((DENIED, Vec::new())),
        }
    }

    /// `MAV_CMD_SET_MESSAGE_INTERVAL`: an interval in microseconds. Below
    /// zero stops it, and so does zero, the default, since the camera sends
    /// nothing unasked but heartbeats and the images it takes.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive interval, and a huge one saturates"
    )]
    fn send_every(&mut self, report: Report, message: u32, interval_us: f32) -> u8 {
        let streams = matches!(report, Report::StreamInformation | Report::StreamStatus);
        if streams && self.stream.is_none() {
            return DENIED;
        }
        if interval_us > 0.0 {
            let every_ms = ((interval_us / 1000.0) as u64).max(FASTEST_MS);
            self.streamed
                .insert(message, Every::after(every_ms, self.now_ms));
        } else {
            self.streamed.remove(&message);
        }
        ACCEPTED
    }

    /// `MAV_CMD_IMAGE_START_CAPTURE`: one image, or a sequence at an
    /// interval that counts down or goes on until it is stopped. An image
    /// asked for in the middle of a sequence is taken and the sequence
    /// goes on.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        reason = "an interval already checked, and the same command sent again"
    )]
    fn start_images(&mut self, command: &Command) -> (u8, Vec<Report>) {
        let [_, interval_s, count, number, ..] = command.params;
        // The same command again, as its sender does when the
        // acknowledgement was lost. It says so with the same sequence
        // number or by counting its sends, and the image is taken once.
        let asked = (command.from, [interval_s, count, number]);
        if self.asked == Some(asked) && (number > 0.0 || command.confirmation > 0) {
            return (ACCEPTED, Vec::new());
        }
        let count = wire::whole(count);
        if count != 1 && (interval_s.is_nan() || interval_s < SHORTEST_INTERVAL_S) {
            return (DENIED, Vec::new());
        }
        let Some(first) = self.capture() else {
            return (FAILED, Vec::new());
        };
        self.asked = Some(asked);
        if count != 1 {
            self.interval = Some(Interval {
                every: Every::after((interval_s * 1000.0) as u64, self.now_ms),
                // A count of 0 is for ever.
                left: count.checked_sub(1),
            });
        }
        (ACCEPTED, vec![first])
    }

    /// Takes an image if the card has room for it.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the clock wraps as the field does"
    )]
    fn capture(&mut self) -> Option<Report> {
        let image_mib = self.settings.image_mib();
        if self.used_mib + image_mib > CARD_MIB {
            return None;
        }
        self.used_mib += image_mib;
        self.shots = self.shots.wrapping_add(1);
        self.images.push(Image {
            time_boot_ms: self.now_ms as u32,
            place: self.place,
            attitude: self.attitude,
        });
        Some(Report::Image(self.images.len() - 1))
    }

    /// `MAV_CMD_VIDEO_START_CAPTURE`: a stream's id, and how many times a
    /// second to send `CAMERA_CAPTURE_STATUS` while it records. A camera
    /// already recording keeps its recording.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive rate"
    )]
    fn start_video(&mut self, stream: f32, status_hz: f32) -> u8 {
        if !first_or_all(stream) {
            return DENIED;
        }
        if self.recording.is_some() {
            return ACCEPTED;
        }
        if self.used_mib >= CARD_MIB {
            return FAILED;
        }
        self.recording = Some(Recording {
            since_ms: self.now_ms,
            status: (status_hz > 0.0).then(|| {
                let interval_ms = ((1000.0 / status_hz) as u64).max(FASTEST_MS);
                Every::after(interval_ms, self.now_ms)
            }),
        });
        ACCEPTED
    }

    /// `MAV_CMD_SET_CAMERA_MODE`: image or video. It captures either in
    /// both, so the mode is only what it says it is in.
    fn set_mode(&mut self, mode: f32) -> u8 {
        let video = match exact(mode) {
            Some(0) => false,
            Some(1) => true,
            _ => return DENIED,
        };
        // One that captures only in the mode for it keeps its mode while
        // it captures.
        let capturing = self.recording.is_some() || self.interval.is_some();
        if self.mode_bound && capturing && video != self.video_mode {
            return TEMPORARILY_REJECTED;
        }
        self.video_mode = video;
        ACCEPTED
    }

    /// `MAV_CMD_SET_CAMERA_ZOOM`: by steps, running until stopped, to a
    /// percentage, to a focal length or to a field of view.
    fn set_zoom(&mut self, kind: f32, value: f32) -> u8 {
        let level = match exact(kind) {
            Some(0) => (self.zoom + value * STEP_PCT).clamp(0.0, 100.0),
            Some(1) if !value.is_nan() => {
                self.zooming = value.clamp(-1.0, 1.0) * RUN_PCT_PER_S;
                return ACCEPTED;
            }
            Some(2) => value,
            Some(3) => zoom_of(value),
            Some(4) => zoom_of(SENSOR_MM.0 / (2.0 * (value.to_radians() / 2.0).tan())),
            _ => return DENIED,
        };
        // A NaN is in no range.
        if !(-0.01..=100.01).contains(&level) {
            return DENIED;
        }
        (self.zoom, self.zooming) = (level.clamp(0.0, 100.0), 0.0);
        ACCEPTED
    }

    /// `MAV_CMD_SET_CAMERA_FOCUS`: by steps, running until stopped, to a
    /// percentage, to a distance, or on its own, which finds the ground
    /// far away.
    fn set_focus(&mut self, kind: f32, value: f32) -> u8 {
        let level = match exact(kind) {
            Some(0) => (self.focus + value * STEP_PCT).clamp(0.0, 100.0),
            Some(1) if !value.is_nan() => {
                self.focusing = value.clamp(-1.0, 1.0) * RUN_PCT_PER_S;
                return ACCEPTED;
            }
            Some(2) => value,
            Some(3) if value >= NEAREST_M => 100.0 * (1.0 - NEAREST_M / value),
            Some(4..=6) => 100.0,
            _ => return DENIED,
        };
        if !(0.0..=100.0).contains(&level) {
            return DENIED;
        }
        (self.focus, self.focusing) = (level, 0.0);
        ACCEPTED
    }

    /// `MAV_CMD_STORAGE_FORMAT`: empties the card and the image log, or
    /// the log alone. Not while it is capturing.
    fn format(&mut self, card: f32, format: Option<bool>, log: Option<bool>) -> (u8, Vec<Report>) {
        let (Some(format), Some(log), true) = (format, log, first_or_all(card)) else {
            return (DENIED, Vec::new());
        };
        if self.interval.is_some() || self.recording.is_some() {
            return (TEMPORARILY_REJECTED, Vec::new());
        }
        if format || log {
            self.images.clear();
            self.asked = None;
        }
        if !format {
            return (ACCEPTED, Vec::new());
        }
        // A card a fault filled is empty like any other, and the fault is
        // over.
        self.before_full = None;
        self.used_mib = 0.0;
        (ACCEPTED, vec![Report::Storage])
    }

    fn focal_mm(&self) -> f32 {
        FOCAL_MM.0 + (FOCAL_MM.1 - FOCAL_MM.0) * self.zoom / 100.0
    }

    /// The field of view across a side of the sensor, in degrees.
    fn view_deg(&self, side_mm: f32) -> f32 {
        (2.0 * (side_mm / (2.0 * self.focal_mm())).atan()).to_degrees()
    }

    /// What its video shows, once it knows where it is (S408): the
    /// picture is as wide as the sensor, so the zoom's field of view across
    /// it is the one `VIDEO_STREAM_INFORMATION` gives.
    #[must_use]
    pub fn view(&self) -> Option<View> {
        Some(View {
            place: self.place?,
            attitude: self.attitude,
            hfov_deg: self.view_deg(SENSOR_MM.0),
            thermal: self.thermal_shown,
        })
    }

    /// Where the middle of the image falls on flat ground at home's height:
    /// latitude and longitude times 1e7 and millimetres above sea level.
    /// None when it does not look at the ground.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "degrees times 1e7 and millimetres fit"
    )]
    fn looks_at(&self) -> Option<(i32, i32, i32)> {
        const METRES_PER_DEGREE: f64 = 111_320.0;
        let place = self.place?;
        // The lens looks along the camera's own x axis.
        let [w, x, y, z] = self.attitude.map(f64::from);
        let north = 1.0 - 2.0 * (y * y + z * z);
        let east = 2.0 * (x * y + w * z);
        let down = 2.0 * (x * z - w * y);
        let height = f64::from(place.relative_alt_mm) / 1000.0;
        if down < 0.01 || height <= 0.0 {
            return None;
        }
        let reach = height / down;
        let latitude = f64::from(place.lat) / 1e7;
        let lat = latitude + north * reach / METRES_PER_DEGREE;
        let lon = f64::from(place.lon) / 1e7
            + east * reach / (METRES_PER_DEGREE * latitude.to_radians().cos());
        Some((
            (lat * 1e7).round() as i32,
            (lon * 1e7).round() as i32,
            place.alt_mm - place.relative_alt_mm,
        ))
    }

    fn acknowledge(&mut self, command: &Command, result: u8) -> Vec<u8> {
        self.sent = self.sent.wrapping_add(1);
        crate::sim::command_ack_frame(
            self.system,
            self.component,
            command.from,
            self.sent,
            command.command,
            result,
        )
    }

    fn send(&mut self, message: &dyn mavio::Message) -> Vec<u8> {
        self.sent = self.sent.wrapping_add(1);
        crate::sim::frame(self.system, self.component, self.sent, message)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::too_many_lines,
        reason = "one message per arm, and values that fit their fields"
    )]
    fn report(&mut self, report: Report) -> Vec<u8> {
        let time_boot_ms = self.now_ms as u32;
        let hfov = self.view_deg(SENSOR_MM.0);
        match report {
            Report::Information => {
                let mut flags = if self.video_only {
                    CameraCapFlags::empty()
                } else if self.mode_bound {
                    CameraCapFlags::CAPTURE_VIDEO
                        | CameraCapFlags::CAPTURE_IMAGE
                        | CameraCapFlags::HAS_MODES
                } else {
                    CameraCapFlags::CAPTURE_VIDEO
                        | CameraCapFlags::CAPTURE_IMAGE
                        | CameraCapFlags::HAS_MODES
                        | CameraCapFlags::CAN_CAPTURE_IMAGE_IN_VIDEO_MODE
                        | CameraCapFlags::CAN_CAPTURE_VIDEO_IN_IMAGE_MODE
                        | CameraCapFlags::HAS_BASIC_ZOOM
                        | CameraCapFlags::HAS_BASIC_FOCUS
                };
                if self.stream.is_some() {
                    flags |= CameraCapFlags::HAS_VIDEO_STREAM;
                }
                if self.tracks {
                    flags |= CameraCapFlags::HAS_TRACKING_POINT
                        | CameraCapFlags::HAS_TRACKING_RECTANGLE
                        | CameraCapFlags::HAS_TRACKING_GEO_STATUS;
                }
                if self.thermal_sensor {
                    flags |= CameraCapFlags::HAS_THERMAL_RANGE;
                }
                // One that only sends video knows no lens or sensor: not a
                // number, and 0 pixels.
                let (focal, sensor, pixels) = if self.video_only {
                    (f32::NAN, (f32::NAN, f32::NAN), (0, 0))
                } else {
                    (FOCAL_MM.0, SENSOR_MM, SENSOR_PX)
                };
                // One that only sends video has no settings to define.
                let definition = self.definition_uri.clone().filter(|_| !self.video_only);
                self.send(&CameraInformation {
                    time_boot_ms,
                    vendor_name: wire::text("UXDuck"),
                    model_name: wire::text("SimCamera"),
                    firmware_version: wire::version(1, 0, 0),
                    focal_length: focal,
                    sensor_size_h: sensor.0,
                    sensor_size_v: sensor.1,
                    resolution_h: pixels.0,
                    resolution_v: pixels.1,
                    flags,
                    cam_definition_version: if definition.is_some() {
                        settings::DEFINITION_VERSION
                    } else {
                        0
                    },
                    cam_definition_uri: wire::text(definition.as_deref().unwrap_or_default()),
                    gimbal_device_id: self.gimbal_device_id,
                    ..CameraInformation::default()
                })
            }
            Report::Settings => self.send(&CameraSettings {
                time_boot_ms,
                mode_id: if self.video_mode {
                    CameraMode::Video
                } else {
                    CameraMode::Image
                },
                // Not a number where it has no lens to drive.
                zoom_level: if self.mode_bound { f32::NAN } else { self.zoom },
                focus_level: if self.mode_bound {
                    f32::NAN
                } else {
                    self.focus
                },
                camera_device_id: 0,
            }),
            Report::Storage => self.send(&StorageInformation {
                time_boot_ms,
                storage_id: 1,
                storage_count: 1,
                status: StorageStatus::Ready,
                total_capacity: CARD_MIB,
                used_capacity: self.used_mib,
                available_capacity: CARD_MIB - self.used_mib,
                read_speed: 90.0,
                write_speed: 60.0,
                type_: StorageType::Microsd,
                name: wire::text("SD"),
                storage_usage: StorageUsageFlag::SET
                    | StorageUsageFlag::PHOTO
                    | StorageUsageFlag::VIDEO,
            }),
            Report::CaptureStatus => {
                let interval_ms = self.interval.map_or(0, |i| i.every.interval_ms());
                self.send(&CameraCaptureStatus {
                    time_boot_ms,
                    // 2 is an interval set and no image being taken now.
                    image_status: if self.interval.is_some() { 2 } else { 0 },
                    video_status: u8::from(self.recording.is_some()),
                    image_interval: std::time::Duration::from_millis(interval_ms).as_secs_f32(),
                    recording_time_ms: self
                        .recording
                        .map_or(0, |r| self.now_ms.saturating_sub(r.since_ms) as u32),
                    available_capacity: CARD_MIB - self.used_mib,
                    image_count: self.images.len() as i32,
                    camera_device_id: 0,
                })
            }
            Report::Image(index) => {
                let image = self.images[index];
                let place = image.place.unwrap_or(Place {
                    lat: 0,
                    lon: 0,
                    alt_mm: 0,
                    relative_alt_mm: 0,
                });
                self.send(&CameraImageCaptured {
                    time_boot_ms: image.time_boot_ms,
                    time_utc: 0,
                    camera_id: 0,
                    lat: place.lat,
                    lon: place.lon,
                    alt: place.alt_mm,
                    relative_alt: place.relative_alt_mm,
                    q: image.attitude,
                    image_index: index as i32,
                    capture_result: 1,
                    file_url: wire::text(&format!("IMG_{:04}.JPG", index + 1)),
                })
            }
            Report::StreamInformation => {
                let uri = self.stream.clone().unwrap_or_default();
                self.send(&VideoStreamInformation {
                    stream_id: 1,
                    count: 1,
                    type_: if uri.starts_with("rtsp://") {
                        VideoStreamType::Rtsp
                    } else if self.stream_rtp {
                        VideoStreamType::Rtpudp
                    } else {
                        VideoStreamType::MpegTs
                    },
                    flags: self.stream_flags(),
                    framerate: 30.0,
                    resolution_h: 1280,
                    resolution_v: 720,
                    bitrate: 4_000_000,
                    hfov: hfov.round() as u16,
                    name: wire::text("Main"),
                    uri: wire::text(&uri),
                    encoding: VideoStreamEncoding::H264,
                    ..VideoStreamInformation::default()
                })
            }
            Report::StreamStatus => self.send(&VideoStreamStatus {
                stream_id: 1,
                flags: self.stream_flags(),
                framerate: 30.0,
                resolution_h: 1280,
                resolution_v: 720,
                bitrate: 4_000_000,
                rotation: 0,
                hfov: hfov.round() as u16,
                camera_device_id: 0,
            }),
            Report::Fov => {
                let place = self.place;
                let (lat_image, lon_image, alt_image) =
                    self.looks_at().unwrap_or((i32::MAX, i32::MAX, i32::MAX));
                self.send(&CameraFovStatus {
                    time_boot_ms,
                    lat_camera: place.map_or(i32::MAX, |p| p.lat),
                    lon_camera: place.map_or(i32::MAX, |p| p.lon),
                    alt_camera: place.map_or(i32::MAX, |p| p.alt_mm),
                    lat_image,
                    lon_image,
                    alt_image,
                    q: self.attitude,
                    hfov,
                    vfov: self.view_deg(SENSOR_MM.1),
                    camera_device_id: 0,
                })
            }
            Report::TrackingImage => {
                let drawn = self
                    .target
                    .zip(self.target_seen)
                    .map(|(target, seen)| target.drawn(&seen));
                let (point, rectangle) = drawn.map_or(([f32::NAN; 3], [f32::NAN; 4]), |drawn| {
                    (drawn.point, drawn.rectangle)
                });
                self.send(&CameraTrackingImageStatus {
                    tracking_status: self.tracking_status(),
                    tracking_mode: match self.target {
                        None => CameraTrackingMode::None,
                        Some(target) if target.boxed() => CameraTrackingMode::Rectangle,
                        Some(_) => CameraTrackingMode::Point,
                    },
                    // The point or the box is in this message, and not
                    // drawn on the picture.
                    target_data: if self.target.is_some() {
                        CameraTrackingTargetData::IN_STATUS
                    } else {
                        CameraTrackingTargetData::empty()
                    },
                    point_x: point[0],
                    point_y: point[1],
                    radius: point[2],
                    rec_top_x: rectangle[0],
                    rec_top_y: rectangle[1],
                    rec_bottom_x: rectangle[2],
                    rec_bottom_y: rectangle[3],
                    camera_device_id: 0,
                })
            }
            // The ground does not move, and how well the place is known is
            // not for a simulation to make up: not a number.
            Report::TrackingGeo => {
                let target = self.target;
                self.send(&CameraTrackingGeoStatus {
                    tracking_status: self.tracking_status(),
                    lat: target.map_or(0, |target| (target.lat * 1e7).round() as i32),
                    lon: target.map_or(0, |target| (target.lon * 1e7).round() as i32),
                    alt: target.map_or(f32::NAN, |target| target.alt_m as f32),
                    h_acc: f32::NAN,
                    v_acc: f32::NAN,
                    vel_n: if target.is_some() { 0.0 } else { f32::NAN },
                    vel_e: if target.is_some() { 0.0 } else { f32::NAN },
                    vel_d: if target.is_some() { 0.0 } else { f32::NAN },
                    vel_acc: f32::NAN,
                    dist: self
                        .target_seen
                        .map_or(f32::NAN, |seen| seen.range_m as f32),
                    hdg: f32::NAN,
                    hdg_acc: f32::NAN,
                    camera_device_id: 0,
                })
            }
            Report::ThermalRange => self.send(&CameraThermalRange {
                time_boot_ms,
                stream_id: 1,
                camera_device_id: 0,
                max: THERMAL_SCALE_C.1,
                max_point_x: f32::NAN,
                max_point_y: f32::NAN,
                min: THERMAL_SCALE_C.0,
                min_point_x: f32::NAN,
                min_point_y: f32::NAN,
            }),
        }
    }

    /// `VIDEO_STREAM_STATUS_FLAGS` of its one stream: running, and thermal
    /// with a range to ask for while it shows its thermal sensor.
    fn stream_flags(&self) -> VideoStreamStatusFlags {
        let thermal = if self.thermal_shown {
            STREAM_THERMAL | STREAM_THERMAL_RANGE_ENABLED
        } else {
            0
        };
        VideoStreamStatusFlags::from_bits_retain(VideoStreamStatusFlags::RUNNING.bits() | thermal)
    }

    /// `CAMERA_TRACKING_STATUS_FLAGS`: idle, tracking, or in error while
    /// what it tracks is outside its picture.
    fn tracking_status(&self) -> CameraTrackingStatusFlags {
        match (self.target, self.target_seen) {
            (None, _) => CameraTrackingStatusFlags::Idle,
            (Some(_), Some(_)) => CameraTrackingStatusFlags::Active,
            (Some(_), None) => CameraTrackingStatusFlags::Error,
        }
    }
}

/// The zoom, in percent of its range, that gives a focal length.
fn zoom_of(focal_mm: f32) -> f32 {
    (focal_mm - FOCAL_MM.0) / (FOCAL_MM.1 - FOCAL_MM.0) * 100.0
}

/// An id that is this camera's only stream, card or sensor: 0 is all of
/// them, 1 is the first, and a NaN was left unset.
fn first_or_all(id: f32) -> bool {
    id.is_nan() || matches!(exact(id), Some(0 | 1))
}

/// A parameter that must be a whole number and not below zero.
fn exact(param: f32) -> Option<u32> {
    (param >= 0.0 && param.fract() == 0.0).then(|| wire::whole(param))
}

/// A parameter that is 0 or 1 and nothing else.
fn flag(param: f32) -> Option<bool> {
    match exact(param) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

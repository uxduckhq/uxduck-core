//! The MAVLink camera protocol's controls (ADR-0063 Y5, S405): what a
//! camera says of itself once it is identified, and the commands that
//! work it.
//!
//! A camera that can do more than send video is asked for
//! `CAMERA_SETTINGS`, `STORAGE_INFORMATION` and `CAMERA_CAPTURE_STATUS`:
//! three times with `MAV_CMD_REQUEST_MESSAGE`, then three times with the
//! command that superseded. It is asked to send `CAMERA_CAPTURE_STATUS`
//! every second, and asked for it again whenever five seconds pass without
//! one. After a command it accepts, what the command changes is asked for
//! again, unless the camera says so first. Each photo it takes arrives as a
//! `CAMERA_IMAGE_CAPTURED`. A photo it counts and does not describe is
//! asked for by its index, three times, and then counted as lost.
//!
//! Nothing here is sent by itself: the session sends what is due when a
//! heartbeat or a message arrives.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::camera::CameraAction;

use crate::camera::{AUTOPILOT_CAMERAS, Camera, Cameras, Key, Source};

/// `CAMERA_SETTINGS`'s message id.
pub const CAMERA_SETTINGS: u32 = 260;
/// `STORAGE_INFORMATION`'s message id.
pub const STORAGE_INFORMATION: u32 = 261;
/// `CAMERA_CAPTURE_STATUS`'s message id.
pub const CAMERA_CAPTURE_STATUS: u32 = 262;
/// `CAMERA_IMAGE_CAPTURED`'s message id.
pub const CAMERA_IMAGE_CAPTURED: u32 = 263;
/// The commands `MAV_CMD_REQUEST_MESSAGE` superseded.
const REQUEST_CAMERA_SETTINGS: u16 = 522;
const REQUEST_STORAGE_INFORMATION: u16 = 525;
const REQUEST_CAMERA_CAPTURE_STATUS: u16 = 527;
/// `MAV_CMD_SET_MESSAGE_INTERVAL`.
pub const SET_MESSAGE_INTERVAL: u16 = 511;
/// `MAV_CMD_STORAGE_FORMAT`.
pub const STORAGE_FORMAT: u16 = 526;
/// `MAV_CMD_RESET_CAMERA_SETTINGS`.
pub const RESET_CAMERA_SETTINGS: u16 = 529;
/// `MAV_CMD_SET_CAMERA_MODE`.
pub const SET_CAMERA_MODE: u16 = 530;
/// `MAV_CMD_SET_CAMERA_ZOOM`.
pub const SET_CAMERA_ZOOM: u16 = 531;
/// `MAV_CMD_SET_CAMERA_FOCUS`.
pub const SET_CAMERA_FOCUS: u16 = 532;
/// `MAV_CMD_IMAGE_START_CAPTURE`.
pub const IMAGE_START_CAPTURE: u16 = 2000;
/// `MAV_CMD_IMAGE_STOP_CAPTURE`.
pub const IMAGE_STOP_CAPTURE: u16 = 2001;
/// `MAV_CMD_VIDEO_START_CAPTURE`.
pub const VIDEO_START_CAPTURE: u16 = 2500;
/// `MAV_CMD_VIDEO_STOP_CAPTURE`.
pub const VIDEO_STOP_CAPTURE: u16 = 2501;

/// The `CAMERA_CAP_FLAGS` that name something to work: video, images,
/// modes, zoom and focus.
const CONTROLS: u32 = 1 | 2 | 4 | 64 | 128;
const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 3_000_000;
/// A capture status is asked for when none came for this long.
const POLL_US: u64 = 5_000_000;
/// After a command, the camera has this long to say what changed before
/// it is asked.
const SETTLE_US: u64 = 300_000;
/// How often a camera is asked to send its capture status.
const STATUS_INTERVAL_US: u32 = 1_000_000;
/// A photo counted and not described is asked for after this long, and
/// again this far apart.
const IMAGE_WAIT_US: u64 = 1_000_000;
const IMAGE_RETRY_US: u64 = 2_000_000;
/// The most photos asked for when a heartbeat arrives, so that a camera
/// that missed many is asked a few at a time on a link it shares with
/// the vehicle.
const IMAGES_AT_ONCE: usize = 2;
/// The most photos waited for at a time. More than this missing at once
/// are counted as lost without asking. The count is the camera's to say
/// on its MAVLink stream, and one that leaps by millions must not make a
/// list that long.
const IMAGES_WAITED_FOR: usize = 64;
/// How many described photos are remembered, to tell a photo told twice:
/// a ring, since a camera describes as many as it likes.
const DESCRIBED_KEPT: usize = 512;
/// A recording's start moves when the camera's own count of it differs
/// from UXDuck's by more than this.
const RECORDING_DRIFT_US: u64 = 1_500_000;
/// Room under which a card counts as full.
const FULL_BELOW_MIB: f32 = uxduck_core::domain::camera::CARD_FULL_BELOW_MIB;

/// The `MAV_CMD` and parameters that have a camera do something. `device`
/// is 1 to 6 for a camera its autopilot speaks for, and 0 for a camera
/// component's own or for every camera. `sequence` numbers one photo, so
/// that the command sent again is not a second photo. 0 leaves it out.
/// None for one of the camera's own settings, which is no command: it goes
/// as an extended parameter (`camera_params`, S411), and for a toggle,
/// which the engine sends as what the camera needs (S416).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "counts and sequence numbers far below f32's 24 bits"
)]
pub fn encode(action: &CameraAction, device: u8, sequence: u32) -> Option<(u16, [f32; 7])> {
    let id = f32::from(device);
    let nan = f32::NAN;
    Some(match *action {
        CameraAction::SetSetting { .. }
        | CameraAction::ToggleRecording
        | CameraAction::ToggleMode
        | CameraAction::ToggleSource => return None,
        // The definition asks for NaN in what this command leaves unused.
        CameraAction::SetMode { mode } => (
            SET_CAMERA_MODE,
            [id, f32::from(mode.id()), nan, nan, nan, nan, nan],
        ),
        CameraAction::TakePhoto => (
            IMAGE_START_CAPTURE,
            [id, 0.0, 1.0, sequence as f32, 0.0, 0.0, 0.0],
        ),
        CameraAction::StartInterval { interval_s, count } => (
            IMAGE_START_CAPTURE,
            [
                id,
                interval_s,
                // 0 goes on until stopped.
                count.unwrap_or(0) as f32,
                0.0,
                0.0,
                0.0,
                0.0,
            ],
        ),
        CameraAction::StopInterval => (IMAGE_STOP_CAPTURE, [id, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        // Every stream, and its capture status once a second while it
        // records.
        CameraAction::StartRecording => (VIDEO_START_CAPTURE, [0.0, 1.0, id, 0.0, 0.0, 0.0, 0.0]),
        CameraAction::StopRecording => (VIDEO_STOP_CAPTURE, [0.0, id, 0.0, 0.0, 0.0, 0.0, 0.0]),
        CameraAction::ZoomStep { steps } => (
            SET_CAMERA_ZOOM,
            [0.0, f32::from(steps), id, 0.0, 0.0, 0.0, 0.0],
        ),
        CameraAction::ZoomTo { percent } => {
            (SET_CAMERA_ZOOM, [2.0, percent, id, 0.0, 0.0, 0.0, 0.0])
        }
        CameraAction::FocusStep { steps } => (
            SET_CAMERA_FOCUS,
            [0.0, f32::from(steps), id, 0.0, 0.0, 0.0, 0.0],
        ),
        CameraAction::FocusTo { percent } => {
            (SET_CAMERA_FOCUS, [2.0, percent, id, 0.0, 0.0, 0.0, 0.0])
        }
        CameraAction::FocusAuto => (SET_CAMERA_FOCUS, [4.0, 0.0, id, 0.0, 0.0, 0.0, 0.0]),
        // `ZOOM_TYPE_CONTINUOUS` and `FOCUS_TYPE_CONTINUOUS` (S416).
        CameraAction::ZoomRate { rate } => (SET_CAMERA_ZOOM, [1.0, rate, id, 0.0, 0.0, 0.0, 0.0]),
        CameraAction::FocusRate { rate } => (SET_CAMERA_FOCUS, [1.0, rate, id, 0.0, 0.0, 0.0, 0.0]),
        // The card and its list of photos.
        CameraAction::Format { storage_id } => (
            STORAGE_FORMAT,
            [
                f32::from(storage_id.unwrap_or(1)),
                1.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
            ],
        ),
        CameraAction::Reset => (RESET_CAMERA_SETTINGS, [1.0, id, 0.0, 0.0, 0.0, 0.0, 0.0]),
        // The one source, with no second one beside it in the picture.
        CameraAction::SetSource { source } => (
            crate::camera_tracking::SET_CAMERA_SOURCE,
            [id, f32::from(source.id()), 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
        // The point alone: a radius of 0 is one pixel.
        CameraAction::TrackPoint { x, y } => (
            crate::camera_tracking::CAMERA_TRACK_POINT,
            [x, y, 0.0, id, 0.0, 0.0, 0.0],
        ),
        CameraAction::TrackRectangle {
            left,
            top,
            right,
            bottom,
        } => (
            crate::camera_tracking::CAMERA_TRACK_RECTANGLE,
            [left, top, right, bottom, id, 0.0, 0.0],
        ),
        CameraAction::StopTracking => (
            crate::camera_tracking::CAMERA_STOP_TRACKING,
            [id, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    })
}

/// A `CAMERA_SETTINGS` as it arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SettingsSaid {
    /// `CAMERA_MODE`.
    pub mode: u8,
    /// Percent of the range, NaN when not known.
    pub zoom_level: f32,
    pub focus_level: f32,
    pub camera_device_id: u8,
}

/// A `STORAGE_INFORMATION` as it arrived.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StorageSaid {
    pub storage_id: u8,
    pub storage_count: u8,
    /// `STORAGE_STATUS`.
    pub status: u8,
    pub total_mib: f32,
    pub used_mib: f32,
    pub available_mib: f32,
    pub name: String,
}

/// A `CAMERA_CAPTURE_STATUS` as it arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CaptureSaid {
    /// 0 idle, 1 capturing, 2 an interval set, 3 an interval set and
    /// capturing.
    pub image_status: u8,
    pub video_status: u8,
    pub image_interval_s: f32,
    pub recording_time_ms: u32,
    pub available_mib: f32,
    pub image_count: i32,
    pub camera_device_id: u8,
}

/// A `CAMERA_IMAGE_CAPTURED` as it arrived.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageSaid {
    /// Microseconds since 1970, or 0 when the camera's clock is not set.
    pub time_utc_us: u64,
    pub camera_id: u8,
    /// Degrees times ten million.
    pub lat: i32,
    pub lon: i32,
    /// Millimetres above mean sea level, and above home.
    pub alt_mm: i32,
    pub relative_alt_mm: i32,
    pub image_index: i32,
    pub ok: bool,
    pub file_url: String,
}

/// A camera's mode, zoom and focus.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Settings {
    /// `CAMERA_MODE`.
    pub mode: u8,
    /// Percent of the range, to a tenth.
    pub zoom_pct: Option<f32>,
    pub focus_pct: Option<f32>,
}

/// A camera's card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Storage {
    pub storage_id: u8,
    pub storage_count: u8,
    /// `STORAGE_STATUS`.
    pub status: u8,
    pub total_mib: f32,
    pub used_mib: f32,
    pub available_mib: f32,
    pub name: String,
}

/// What a camera is capturing. What changes every second is kept coarse,
/// so that a camera at rest or recording is not told again and again.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Capture {
    /// Seconds between photos while an interval is set.
    pub interval_s: Option<f32>,
    pub recording: bool,
    /// When the recording began by the camera's own count of it, on the
    /// clock the messages are timed with.
    pub recording_since_us: Option<u64>,
    pub image_count: u32,
    /// Photos counted, asked for again and never described.
    pub images_lost: u32,
    /// Room left on the card, in steps of a hundredth of the card.
    pub available_mib: Option<f32>,
}

/// What a camera last said of itself (S405).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub settings: Option<Settings>,
    pub storage: Option<Storage>,
    pub capture: Option<Capture>,
}

impl State {
    fn is_empty(&self) -> bool {
        self.settings.is_none() && self.storage.is_none() && self.capture.is_none()
    }
}

/// A photo a camera said it took.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// Its place in the camera's list, from 0.
    pub index: u32,
    /// Microseconds since 1970 by the camera's clock, or 0.
    pub time_utc_us: u64,
    /// When it was heard of.
    pub heard_us: u64,
    /// Latitude and longitude in degrees, metres above mean sea level and
    /// above home. None when the camera said 0, 0.
    pub place: Option<(f64, f64, f64, f64)>,
    pub ok: bool,
    pub file_url: String,
}

/// What to send a camera component or an autopilot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Send {
    /// `MAV_CMD_REQUEST_MESSAGE` for a message id, with its own parameter.
    Message { id: u32, param2: f32 },
    /// A command that `MAV_CMD_REQUEST_MESSAGE` superseded.
    Superseded { command: u16, params: [f32; 2] },
    /// `MAV_CMD_SET_MESSAGE_INTERVAL`.
    Interval { message: u32, interval_us: u32 },
}

/// Something read back from a camera: asked for when it is wanted, again
/// until it is heard, and no more after its tries are spent.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Read {
    /// Asks made since it was last heard or wanted.
    attempts: u8,
    asked_us: u64,
    /// From when it is wanted. None while it is not.
    wanted_us: Option<u64>,
    /// Only the superseded command got an answer.
    superseded: bool,
}

impl Read {
    pub(crate) fn want(&mut self, at_us: u64) {
        self.wanted_us = Some(self.wanted_us.map_or(at_us, |before| before.min(at_us)));
        self.attempts = 0;
    }

    /// It arrived. `again_us` is how long it may then go unheard.
    pub(crate) fn heard(&mut self, now_us: u64, again_us: Option<u64>) {
        if self.attempts > 0 {
            self.superseded |= self.attempts > ATTEMPTS;
        }
        self.attempts = 0;
        self.wanted_us = again_us.map(|after| now_us + after);
    }

    /// Counts an ask if one is due and fewer than `limit` were made:
    /// whether it is the superseded command's turn.
    pub(crate) fn take(&mut self, now_us: u64, limit: u8) -> Option<bool> {
        let due = self.wanted_us.is_some_and(|at| now_us >= at);
        let waited = self.attempts == 0 || now_us.saturating_sub(self.asked_us) >= RETRY_US;
        if !due || !waited || self.attempts >= limit {
            return None;
        }
        self.attempts += 1;
        self.asked_us = now_us;
        Some(self.superseded || self.attempts > ATTEMPTS)
    }
}

/// What is read back from a camera component, or from an autopilot about
/// its cameras.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Reads {
    started: bool,
    settings: Read,
    storage: Read,
    capture: Read,
    /// The capture status asked for every second, once one has come.
    stream: Read,
    streaming: bool,
}

/// A photo counted and not described yet.
#[derive(Clone, Copy, Debug)]
struct Missing {
    noticed_us: u64,
    attempts: u8,
    asked_us: u64,
}

/// One camera's state, and the photos it took.
#[derive(Clone, Debug, Default)]
pub(crate) struct Control {
    state: State,
    /// The card's size, to keep the room left in steps.
    total_mib: Option<f32>,
    /// The camera's count of photos when it was first heard. Photos
    /// before it were taken before UXDuck listened, and are not asked for.
    baseline: Option<u32>,
    /// The photos looked for so far: every index below it.
    looked_to: u32,
    /// The photos described, by index: the latest of them.
    described: BTreeSet<u32>,
    missing: BTreeMap<u32, Missing>,
    /// The last sequence number given to one photo.
    sequence: u32,
}

impl Control {
    /// The list of photos starts again, as a format leaves it.
    fn emptied(&mut self, count: u32) {
        self.baseline = Some(count);
        self.looked_to = count;
        self.described.clear();
        self.missing.clear();
    }

    /// Notes the photos the camera counts and has not described. More
    /// than can be asked for one by one are counted as lost.
    fn find_missing(&mut self, count: u32, now_us: u64) -> u32 {
        let Some(baseline) = self.baseline else {
            return 0;
        };
        let mut lost = 0;
        for index in self.looked_to.max(baseline)..count {
            if self.described.contains(&index) {
                continue;
            }
            if self.missing.len() >= IMAGES_WAITED_FOR {
                lost += 1;
                continue;
            }
            self.missing.entry(index).or_insert(Missing {
                noticed_us: now_us,
                attempts: 0,
                asked_us: 0,
            });
        }
        self.looked_to = self.looked_to.max(count);
        lost
    }

    /// The photos to ask for now, and how many were asked for three times
    /// and are given up.
    fn due_images(&mut self, now_us: u64) -> (Vec<u32>, u32) {
        let mut due = Vec::new();
        let mut lost = Vec::new();
        for (index, missing) in &mut self.missing {
            let waited = if missing.attempts == 0 {
                now_us.saturating_sub(missing.noticed_us) >= IMAGE_WAIT_US
            } else {
                now_us.saturating_sub(missing.asked_us) >= IMAGE_RETRY_US
            };
            if !waited {
                continue;
            }
            if missing.attempts >= ATTEMPTS {
                lost.push(*index);
            } else if due.len() < IMAGES_AT_ONCE {
                missing.attempts += 1;
                missing.asked_us = now_us;
                due.push(*index);
            }
        }
        for index in &lost {
            self.missing.remove(index);
        }
        #[allow(clippy::cast_possible_truncation, reason = "at most 64 are waited for")]
        (due, lost.len() as u32)
    }

    fn described(&mut self, index: u32) -> bool {
        self.missing.remove(&index);
        let new = self.described.insert(index);
        while self.described.len() > DESCRIBED_KEPT {
            self.described.pop_first();
        }
        new
    }
}

/// Room left, kept in steps of a hundredth of the card, and told when it
/// crosses the line under which a card counts as full.
fn coarse(before: Option<f32>, now: f32, total: Option<f32>) -> f32 {
    let Some(before) = before else {
        return now;
    };
    let step = total.map_or(64.0, |total| (total / 100.0).max(1.0));
    let crossed = (before < FULL_BELOW_MIB) != (now < FULL_BELOW_MIB);
    if (now - before).abs() >= step || crossed || (now <= 0.0) != (before <= 0.0) {
        now
    } else {
        before
    }
}

fn percent(level: f32) -> Option<f32> {
    level
        .is_finite()
        .then(|| (level.clamp(0.0, 100.0) * 10.0).round() / 10.0)
}

/// The camera a message is about, with its device id: the one its device
/// id names, else the source's only camera. A camera component is its own.
fn camera_at(source: &mut Source, device_id: u8) -> Option<(u8, &mut Camera)> {
    let device = if !source.autopilot {
        0
    } else if (1..=AUTOPILOT_CAMERAS).contains(&device_id) {
        device_id
    } else if source.cameras.len() == 1 {
        *source.cameras.keys().next()?
    } else {
        return None;
    };
    if source.autopilot {
        source
            .cameras
            .get_mut(&device)
            .map(|camera| (device, camera))
    } else {
        Some((device, source.cameras.entry(device).or_default()))
    }
}

impl Cameras {
    /// A `CAMERA_SETTINGS` from a camera component or an autopilot.
    pub fn settings(&mut self, key: &Key, said: SettingsSaid, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        source.reads.settings.heard(now_us, None);
        let Some((_, camera)) = camera_at(source, said.camera_device_id) else {
            return;
        };
        let now = Some(Settings {
            mode: said.mode,
            zoom_pct: percent(said.zoom_level),
            focus_pct: percent(said.focus_level),
        });
        if camera.control.state.settings != now {
            camera.control.state.settings = now;
            self.control_changed = true;
        }
    }

    /// A `STORAGE_INFORMATION`. A camera's first card is the one kept.
    pub fn storage(&mut self, key: &Key, said: StorageSaid, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        source.reads.storage.heard(now_us, None);
        let Some((_, camera)) = camera_at(source, 0) else {
            return;
        };
        let control = &mut camera.control;
        if control
            .state
            .storage
            .as_ref()
            .is_some_and(|kept| kept.storage_id < said.storage_id)
        {
            return;
        }
        // 2 is ready: the sizes of a card in any other state mean nothing.
        let ready = said.status == 2;
        let size = |mib: f32| {
            if ready && mib.is_finite() {
                mib.max(0.0)
            } else {
                0.0
            }
        };
        let now = Storage {
            storage_id: said.storage_id,
            storage_count: said.storage_count,
            status: said.status,
            total_mib: size(said.total_mib),
            used_mib: size(said.used_mib),
            available_mib: size(said.available_mib),
            name: said.name,
        };
        control.total_mib = ready.then_some(now.total_mib).filter(|total| *total > 0.0);
        // The card's own word for the room left, such as after a format.
        if ready
            && let Some(capture) = &mut control.state.capture
            && capture.available_mib != Some(now.available_mib)
        {
            capture.available_mib = Some(now.available_mib);
            self.control_changed = true;
        }
        if control.state.storage.as_ref() != Some(&now) {
            control.state.storage = Some(now);
            self.control_changed = true;
        }
    }

    /// A `CAMERA_CAPTURE_STATUS`.
    pub fn capture_status(&mut self, key: &Key, said: CaptureSaid, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        source.reads.capture.heard(now_us, Some(POLL_US));
        if !source.reads.streaming {
            // It has a capture status: it is asked to keep sending it.
            source.reads.streaming = true;
            source.reads.stream.want(now_us);
        }
        let Some((_, camera)) = camera_at(source, said.camera_device_id) else {
            return;
        };
        let control = &mut camera.control;
        let before = control.state.capture;
        let count = u32::try_from(said.image_count).unwrap_or(0);
        // A count that went down is a list emptied by a format.
        let emptied = before.is_some_and(|before| count < before.image_count);
        if emptied {
            control.emptied(count);
        } else if control.baseline.is_none() {
            control.baseline = Some(count);
        }
        let lost = control.find_missing(count, now_us);
        let recording = said.video_status == 1;
        let counted = now_us.saturating_sub(u64::from(said.recording_time_ms) * 1000);
        let since = before.and_then(|before| before.recording_since_us);
        let recording_since_us = recording.then(|| match since {
            // No count of its own: UXDuck's, from when it was first seen.
            Some(since) if said.recording_time_ms == 0 => since,
            Some(since) if since.abs_diff(counted) <= RECORDING_DRIFT_US => since,
            _ => counted,
        });
        let now = Capture {
            interval_s: matches!(said.image_status, 2 | 3)
                .then_some(said.image_interval_s)
                .filter(|interval| interval.is_finite() && *interval > 0.0),
            recording,
            recording_since_us,
            image_count: count,
            images_lost: before
                .filter(|_| !emptied)
                .map_or(0, |before| before.images_lost)
                + lost,
            available_mib: said.available_mib.is_finite().then(|| {
                coarse(
                    before.and_then(|before| before.available_mib),
                    said.available_mib.max(0.0),
                    control.total_mib,
                )
            }),
        };
        if before != Some(now) {
            control.state.capture = Some(now);
            self.control_changed = true;
        }
    }

    /// A `CAMERA_IMAGE_CAPTURED`. A photo already described is not told
    /// twice.
    pub fn image(&mut self, key: &Key, said: ImageSaid, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        let Some((device, camera)) = camera_at(source, said.camera_id) else {
            return;
        };
        let Ok(index) = u32::try_from(said.image_index) else {
            return;
        };
        let control = &mut camera.control;
        if !control.described(index) {
            return;
        }
        // A photo that comes before any capture status is where the list
        // starts.
        control.baseline = Some(control.baseline.map_or(index, |from| from.min(index)));
        if let Some(capture) = &mut control.state.capture
            && capture.image_count <= index
        {
            capture.image_count = index + 1;
            self.control_changed = true;
        }
        let scaled = |value: i32, by: f64| f64::from(value) / by;
        self.images.push((
            key.clone(),
            device,
            Image {
                index,
                time_utc_us: said.time_utc_us,
                heard_us: now_us,
                place: (said.lat != 0 || said.lon != 0).then(|| {
                    (
                        scaled(said.lat, 1e7),
                        scaled(said.lon, 1e7),
                        scaled(said.alt_mm, 1e3),
                        scaled(said.relative_alt_mm, 1e3),
                    )
                }),
                ok: said.ok,
                file_url: said.file_url,
            },
        ));
    }

    /// A `COMMAND_ACK` from a camera component or an autopilot. A camera
    /// command it accepted changes what the camera says of itself, whoever
    /// sent the command: that is asked for again shortly, unless the
    /// camera says so first.
    pub fn acknowledged(&mut self, key: &Key, command: u16, result: u8, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        // Another source and tracking are any camera's, with a card and
        // settings to read back or without (ADR-0063 Y5).
        if result == 0 {
            source.watch.accepted(command, now_us);
        }
        let reads = &mut source.reads;
        if command == SET_MESSAGE_INTERVAL {
            // Taken or refused, it is not asked for again.
            if reads.streaming {
                reads.stream.heard(now_us, None);
            }
            return;
        }
        if result != 0 || !reads.started {
            return;
        }
        let soon = now_us + SETTLE_US;
        match command {
            SET_CAMERA_MODE | SET_CAMERA_ZOOM | SET_CAMERA_FOCUS | RESET_CAMERA_SETTINGS => {
                reads.settings.want(soon);
            }
            IMAGE_START_CAPTURE | IMAGE_STOP_CAPTURE | VIDEO_START_CAPTURE | VIDEO_STOP_CAPTURE => {
                reads.capture.want(soon);
            }
            STORAGE_FORMAT => {
                reads.storage.want(soon);
                reads.capture.want(soon);
            }
            _ => {}
        }
    }

    /// The id a command names a camera by: its device id, or 0, every
    /// camera, for an autopilot's only camera that it did not number. That
    /// camera is listed by its lens id, as ArduPilot numbers its plain
    /// cameras, but a camera on one of ArduPilot's mounts gives the lens it
    /// looks through there, so the number may be no camera's. 0 is the one
    /// id certain to reach it (S409).
    #[must_use]
    pub fn addressed(&self, key: &Key, device: u8) -> u8 {
        let alone_and_unnumbered = self.sources.get(key).is_some_and(|source| {
            source.autopilot
                && source.cameras.len() == 1
                && source
                    .cameras
                    .get(&device)
                    .and_then(|camera| camera.information.as_ref())
                    .is_some_and(|said| !(1..=AUTOPILOT_CAMERAS).contains(&said.camera_device_id))
        });
        if alone_and_unnumbered { 0 } else { device }
    }

    /// The sequence number for one photo of a camera: one more than the
    /// last, and than the photos it has taken, so that a number from
    /// before UXDuck started is not used again. 0, which leaves it out,
    /// for a camera not heard at this address.
    pub fn photo_sequence(&mut self, key: &Key, device: u8) -> u32 {
        let Some(control) = self
            .sources
            .get_mut(key)
            .and_then(|source| source.cameras.get_mut(&device))
            .map(|camera| &mut camera.control)
        else {
            return 0;
        };
        let taken = control
            .state
            .capture
            .map_or(0, |capture| capture.image_count);
        control.sequence = control.sequence.max(taken) + 1;
        control.sequence
    }

    /// What to send now to read the cameras back, among those whose
    /// vehicle is known and heard.
    pub fn control_requests(
        &mut self,
        now_us: u64,
        known: impl Fn(&Key) -> bool,
    ) -> Vec<(Key, Send)> {
        let mut out = Vec::new();
        for (key, source) in &mut self.sources {
            if source.gone || !known(key) {
                continue;
            }
            let works = source.cameras.values().any(|camera| {
                camera
                    .information
                    .as_ref()
                    .is_some_and(|said| said.flags & CONTROLS != 0)
            });
            if !works {
                continue;
            }
            let reads = &mut source.reads;
            if !reads.started {
                reads.started = true;
                reads.settings.want(now_us);
                reads.storage.want(now_us);
                reads.capture.want(now_us);
            }
            let asks = [
                (
                    &mut reads.settings,
                    CAMERA_SETTINGS,
                    (REQUEST_CAMERA_SETTINGS, [1.0, 0.0]),
                ),
                // Every card, of which the first is kept.
                (
                    &mut reads.storage,
                    STORAGE_INFORMATION,
                    (REQUEST_STORAGE_INFORMATION, [0.0, 1.0]),
                ),
                (
                    &mut reads.capture,
                    CAMERA_CAPTURE_STATUS,
                    (REQUEST_CAMERA_CAPTURE_STATUS, [1.0, 0.0]),
                ),
            ];
            for (read, id, (command, params)) in asks {
                match read.take(now_us, 2 * ATTEMPTS) {
                    Some(true) => out.push((key.clone(), Send::Superseded { command, params })),
                    Some(false) => out.push((key.clone(), Send::Message { id, param2: 0.0 })),
                    None => {}
                }
            }
            if reads.stream.take(now_us, ATTEMPTS).is_some() {
                out.push((
                    key.clone(),
                    Send::Interval {
                        message: CAMERA_CAPTURE_STATUS,
                        interval_us: STATUS_INTERVAL_US,
                    },
                ));
            }
            for camera in source.cameras.values_mut() {
                let (due, lost) = camera.control.due_images(now_us);
                if lost > 0
                    && let Some(capture) = &mut camera.control.state.capture
                {
                    capture.images_lost += lost;
                    self.control_changed = true;
                }
                #[allow(clippy::cast_precision_loss, reason = "an index asked for exactly")]
                out.extend(due.into_iter().map(|index| {
                    (
                        key.clone(),
                        Send::Message {
                            id: CAMERA_IMAGE_CAPTURED,
                            param2: index as f32,
                        },
                    )
                }));
            }
        }
        out
    }

    /// Whether what a camera said of itself changed since this was last
    /// asked.
    pub fn take_control_changed(&mut self) -> bool {
        std::mem::take(&mut self.control_changed)
    }

    /// The photos described since this was last asked: where each was
    /// heard and its camera's device id (0 for a camera component's own).
    pub fn take_images(&mut self) -> Vec<(Key, u8, Image)> {
        std::mem::take(&mut self.images)
    }

    /// What each camera last said of itself: where it was heard and its
    /// device id (0 for a camera component's own). A camera that has said
    /// nothing is left out.
    pub fn states(&self) -> impl Iterator<Item = (&Key, u8, &State)> {
        self.sources.iter().flat_map(|(key, source)| {
            source
                .cameras
                .iter()
                .filter(|(_, camera)| !camera.control.state.is_empty())
                .map(move |(device, camera)| (key, *device, &camera.control.state))
        })
    }
}

#[cfg(test)]
mod tests {
    use uxduck_core::domain::camera::{CameraAction, CameraMode};
    use uxduck_core::domain::command::Command;

    use super::*;
    use crate::decode::{Message, decode};
    use crate::payload::{Payload, Place};
    use crate::session::{CommandOrder, CommandOutcome, Config, Effect, Session};
    use crate::sim::FakeVehicle;

    const PEER: &str = "127.0.0.1:5760";
    const SECOND: u64 = 1_000_000;

    /// A session and a fake vehicle that carries the simulated camera,
    /// talking over a link that loses the messages named.
    struct Rig {
        session: Session,
        vehicle: FakeVehicle,
        now_us: u64,
        /// Message ids from the vehicle that do not arrive.
        lost: Vec<u32>,
        /// Commands from UXDuck that do not arrive.
        unheard: Vec<u16>,
        /// Every command UXDuck sent: its target component, number and
        /// parameters.
        sent: Vec<(u8, u16, [f32; 7])>,
        made: Vec<Effect>,
    }

    impl Rig {
        /// The camera identified and read back, three seconds in.
        fn new() -> Self {
            let mut vehicle = FakeVehicle::new(1, 0x1234);
            let mut payload = Payload::new(1).with_camera(None);
            payload.vehicle(
                [0.0; 3],
                Some(Place {
                    lat: 473_977_000,
                    lon: 85_456_000,
                    alt_mm: 538_000,
                    relative_alt_mm: 50_000,
                }),
            );
            vehicle.payload = Some(payload);
            let mut rig = Self {
                session: Session::new(Config::default()),
                vehicle,
                now_us: 0,
                lost: Vec::new(),
                unheard: Vec::new(),
                sent: Vec::new(),
                made: Vec::new(),
            };
            for _ in 0..3 {
                rig.second();
            }
            rig
        }

        fn id(&self) -> String {
            self.session
                .vehicle_at("l1", PEER, 1)
                .expect("the vehicle")
                .to_owned()
        }

        /// Hands the session's frames to the vehicle and its answers back,
        /// until neither says more.
        fn converse(&mut self, effects: Vec<Effect>) {
            let mut pending = effects;
            for _ in 0..1000 {
                if pending.is_empty() {
                    break;
                }
                let mut next = Vec::new();
                for effect in pending {
                    let Effect::Send { bytes, .. } = effect else {
                        self.made.push(effect);
                        continue;
                    };
                    if let Some(command) = crate::payload::wire::command(&bytes) {
                        self.sent
                            .push((command.to.1, command.command, command.params));
                        if self.unheard.contains(&command.command) {
                            continue;
                        }
                    }
                    for reply in self.vehicle.answer(&bytes) {
                        next.extend(self.hear(&reply));
                    }
                }
                pending = next;
            }
        }

        fn hear(&mut self, frame: &[u8]) -> Vec<Effect> {
            let id = crate::framing::header(frame).map(|header| header.message_id);
            if id.is_some_and(|id| self.lost.contains(&id)) {
                return Vec::new();
            }
            self.session.ingest("l1", PEER, frame, self.now_us)
        }

        /// A tenth of a second: the payload's clock moves, what it sends
        /// arrives, and time is told.
        fn step(&mut self) {
            self.now_us += SECOND / 10;
            #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
            let t_s = self.now_us as f64 / 1e6;
            let mut effects = Vec::new();
            if self.now_us.is_multiple_of(SECOND) {
                let beat = self.vehicle.heartbeat();
                effects.extend(self.hear(&beat));
            }
            for frame in self.vehicle.payload_step(t_s) {
                effects.extend(self.hear(&frame));
            }
            effects.extend(self.session.tick(self.now_us));
            self.converse(effects);
        }

        fn second(&mut self) {
            for _ in 0..10 {
                self.step();
            }
        }

        /// Has the camera do something, as the engine would.
        fn act(&mut self, receipt: &str, action: CameraAction) {
            let order = CommandOrder {
                receipt_id: receipt.into(),
                vehicle_id: self.id(),
                command: Command::Camera {
                    component_id: Some(100),
                    device_id: None,
                    action,
                },
                force: false,
            };
            let effects = self.session.command(order, self.now_us);
            self.converse(effects);
        }

        fn done(&self, receipt: &str) -> Option<(CommandOutcome, u8)> {
            self.made.iter().find_map(|effect| match effect {
                Effect::CommandDone {
                    receipt_id,
                    outcome,
                    attempts,
                } if receipt_id == receipt => Some((outcome.clone(), *attempts)),
                _ => None,
            })
        }

        /// What the camera last said of itself.
        fn state(&self) -> State {
            self.made
                .iter()
                .rev()
                .find_map(|effect| match effect {
                    Effect::CameraState {
                        component_id: 100,
                        device_id: None,
                        state,
                        ..
                    } => Some((**state).clone()),
                    _ => None,
                })
                .expect("the camera's state was told")
        }

        fn states_told(&self) -> usize {
            self.made
                .iter()
                .filter(|effect| matches!(effect, Effect::CameraState { .. }))
                .count()
        }

        fn images(&self) -> Vec<&Image> {
            self.made
                .iter()
                .filter_map(|effect| match effect {
                    Effect::CameraImage { image, .. } => Some(&**image),
                    _ => None,
                })
                .collect()
        }

        fn commands(&self, command: u16) -> Vec<(u8, [f32; 7])> {
            self.sent
                .iter()
                .filter(|(_, sent, _)| *sent == command)
                .map(|(component, _, params)| (*component, *params))
                .collect()
        }

        /// The messages asked for with `MAV_CMD_REQUEST_MESSAGE`.
        fn asked(&self, message: u32) -> usize {
            #[allow(clippy::cast_precision_loss, reason = "a small message id")]
            self.commands(512)
                .iter()
                .filter(|(_, params)| (params[0] - message as f32).abs() < 0.5)
                .count()
        }

        fn camera(&mut self) -> &mut crate::payload::Camera {
            self.vehicle
                .payload
                .as_mut()
                .and_then(|payload| payload.camera.as_mut())
                .expect("a camera")
        }
    }

    // The guide's "Camera identification" and "Storage status" steps: once
    // a camera says what it is, its settings, its card and its capture
    // status are asked for, and it is asked to keep sending the last.
    #[test]
    fn a_camera_identified_is_read_back_and_asked_to_send_its_capture_status() {
        let mut rig = Rig::new();
        let state = rig.state();
        assert_eq!(
            state.settings,
            Some(Settings {
                mode: 0,
                zoom_pct: Some(0.0),
                focus_pct: Some(100.0)
            })
        );
        let card = state.storage.expect("its card");
        assert_eq!((card.storage_id, card.status), (1, 2));
        assert!((card.total_mib - 32_000.0).abs() < 0.5);
        let capture = state.capture.expect("its capture status");
        assert_eq!(
            (capture.image_count, capture.recording, capture.interval_s),
            (0, false, None)
        );
        assert_eq!(capture.available_mib, Some(32_000.0));
        // Each was asked for once, of the camera itself, and answered.
        for message in [CAMERA_SETTINGS, STORAGE_INFORMATION, CAMERA_CAPTURE_STATUS] {
            assert_eq!(rig.asked(message), 1, "message {message}");
        }
        assert!(
            rig.commands(512)
                .iter()
                .filter(|(_, params)| (260.0..=262.0).contains(&params[0]))
                .all(|(component, _)| *component == 100)
        );
        // Asked once to send its capture status every second, which it took.
        let streams: Vec<_> = rig
            .commands(SET_MESSAGE_INTERVAL)
            .into_iter()
            .filter(|(component, _)| *component == 100)
            .collect();
        assert_eq!(streams.len(), 1);
        assert_eq!((streams[0].1[0], streams[0].1[1]), (262.0, 1e6));
        // It then says so every second, and nothing more is asked or told
        // while nothing changes.
        // But for the streams this camera names none of, which it is asked
        // for three times from its first heartbeat (S39), and for whether
        // it manages a gimbal, which every component is asked three times
        // (S406).
        let of_the_camera = |rig: &Rig| {
            rig.sent
                .iter()
                .filter(|(component, _, params)| {
                    *component == 100
                        && (params[0] - 269.0).abs() > 0.5
                        && (params[0] - 280.0).abs() > 0.5
                })
                .count()
        };
        let (asked, told) = (of_the_camera(&rig), rig.states_told());
        for _ in 0..10 {
            rig.second();
        }
        assert_eq!(of_the_camera(&rig), asked, "{:?}", rig.sent);
        assert_eq!(rig.states_told(), told);
    }

    // The guide's "Still image capture": one image, acknowledged, and its
    // CAMERA_IMAGE_CAPTURED with where it was taken. The count goes up.
    #[test]
    fn a_photo_is_acknowledged_counted_and_told_with_where_it_was_taken() {
        let mut rig = Rig::new();
        rig.act("photo", CameraAction::TakePhoto);
        assert_eq!(rig.done("photo"), Some((CommandOutcome::Accepted, 1)));
        // One image, to the camera itself, numbered 1.
        assert_eq!(
            rig.commands(IMAGE_START_CAPTURE),
            [(100, [0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0])]
        );
        let images = rig.images();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].index, images[0].ok), (0, true));
        assert_eq!(images[0].file_url, "IMG_0001.JPG");
        let (lat, lon, alt, above) = images[0].place.expect("where it was taken");
        assert!((lat - 47.3977).abs() < 1e-6 && (lon - 8.5456).abs() < 1e-6);
        assert!((alt - 538.0).abs() < 1e-6 && (above - 50.0).abs() < 1e-6);
        assert_eq!(rig.state().capture.map(|c| c.image_count), Some(1));
        // The next photo has the next number, so it is a photo of its own.
        rig.second();
        rig.act("second", CameraAction::TakePhoto);
        assert_eq!(rig.commands(IMAGE_START_CAPTURE)[1].1[3], 2.0);
        assert_eq!(rig.images().len(), 2);
        assert_eq!(rig.state().capture.map(|c| c.image_count), Some(2));
    }

    // A command whose acknowledgement is lost is sent again with the same
    // sequence number, and the camera takes one photo.
    #[test]
    fn a_photo_sent_twice_is_one_photo() {
        let mut rig = Rig::new();
        rig.lost.push(77);
        rig.act("photo", CameraAction::TakePhoto);
        assert_eq!(rig.done("photo"), None);
        rig.lost.clear();
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.done("photo"), Some((CommandOutcome::Accepted, 2)));
        let sent = rig.commands(IMAGE_START_CAPTURE);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1[3], sent[1].1[3], "the same number both times");
        assert_eq!(rig.images().len(), 1);
        assert_eq!(rig.state().capture.map(|c| c.image_count), Some(1));
    }

    // The guide's "Missing images": the camera counts a photo UXDuck was
    // not told of, and it is asked for by its index.
    #[test]
    fn a_photo_whose_report_is_lost_is_asked_for_again() {
        let mut rig = Rig::new();
        rig.lost.push(CAMERA_IMAGE_CAPTURED);
        rig.act("photo", CameraAction::TakePhoto);
        assert_eq!(rig.done("photo"), Some((CommandOutcome::Accepted, 1)));
        assert!(rig.images().is_empty());
        rig.second();
        assert_eq!(rig.state().capture.map(|c| c.image_count), Some(1));
        // Asked for once the count shows it is missing, and lost again.
        rig.second();
        rig.second();
        assert_eq!(rig.asked(CAMERA_IMAGE_CAPTURED), 1);
        assert!(rig.images().is_empty());
        // The link is whole again: the next ask gets it, once.
        rig.lost.clear();
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.asked(CAMERA_IMAGE_CAPTURED), 2);
        let asks = rig.commands(512);
        let (component, params) = asks
            .iter()
            .rfind(|(_, params)| (params[0] - 263.0).abs() < 0.5)
            .expect("the ask");
        assert_eq!(
            (*component, params[1]),
            (100, 0.0),
            "of the camera, for index 0"
        );
        assert_eq!(rig.images().len(), 1);
        assert_eq!(rig.images()[0].index, 0);
        assert_eq!(rig.state().capture.map(|c| c.images_lost), Some(0));
        for _ in 0..10 {
            rig.second();
        }
        assert_eq!(rig.asked(CAMERA_IMAGE_CAPTURED), 2, "not asked for again");
    }

    #[test]
    fn a_photo_never_described_is_asked_for_three_times_and_counted_as_lost() {
        let mut rig = Rig::new();
        rig.lost.push(CAMERA_IMAGE_CAPTURED);
        rig.act("photo", CameraAction::TakePhoto);
        for _ in 0..20 {
            rig.second();
        }
        assert_eq!(rig.asked(CAMERA_IMAGE_CAPTURED), 3);
        assert!(rig.images().is_empty());
        let capture = rig.state().capture.expect("its capture status");
        assert_eq!((capture.image_count, capture.images_lost), (1, 1));
    }

    // The guide's interval capture: images until it is stopped.
    #[test]
    fn an_interval_takes_photos_until_it_is_stopped() {
        let mut rig = Rig::new();
        rig.act(
            "start",
            CameraAction::StartInterval {
                interval_s: 1.0,
                count: None,
            },
        );
        assert_eq!(rig.done("start"), Some((CommandOutcome::Accepted, 1)));
        // An interval, for ever, with no sequence number.
        assert_eq!(
            rig.commands(IMAGE_START_CAPTURE),
            [(100, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.state().capture.and_then(|c| c.interval_s), Some(1.0));
        let taken = rig.images().len();
        assert!((3..=5).contains(&taken), "{taken} photos in three seconds");
        rig.act("stop", CameraAction::StopInterval);
        assert_eq!(rig.done("stop"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(rig.commands(IMAGE_STOP_CAPTURE), [(100, [0.0; 7])]);
        for _ in 0..3 {
            rig.second();
        }
        let capture = rig.state().capture.expect("its capture status");
        assert_eq!(capture.interval_s, None);
        assert_eq!(rig.images().len(), taken);
        assert_eq!(capture.image_count as usize, taken);
    }

    // The guide's "Video capture": the camera's own recording time, told
    // once and not every second.
    #[test]
    fn a_recording_is_told_with_the_cameras_own_start_and_not_every_second() {
        let mut rig = Rig::new();
        rig.act("record", CameraAction::StartRecording);
        assert_eq!(rig.done("record"), Some((CommandOutcome::Accepted, 1)));
        // Every stream, a status a second, for the camera itself.
        assert_eq!(
            rig.commands(VIDEO_START_CAPTURE),
            [(100, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        let started = rig.now_us;
        rig.second();
        let capture = rig.state().capture.expect("its capture status");
        assert!(capture.recording);
        let since = capture.recording_since_us.expect("when it began");
        assert!(
            since.abs_diff(started) <= 200_000,
            "{since} against {started}"
        );
        let told = rig.states_told();
        for _ in 0..20 {
            rig.second();
        }
        assert_eq!(
            rig.states_told(),
            told,
            "a recording is not told every second"
        );
        assert_eq!(
            rig.state().capture.and_then(|c| c.recording_since_us),
            Some(since)
        );
        rig.act("stop", CameraAction::StopRecording);
        assert_eq!(rig.done("stop"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(rig.commands(VIDEO_STOP_CAPTURE), [(100, [0.0; 7])]);
        rig.second();
        let capture = rig.state().capture.expect("its capture status");
        assert_eq!(
            (capture.recording, capture.recording_since_us),
            (false, None)
        );
    }

    // Mode, zoom, focus and reset: the settings are read back after each.
    #[test]
    fn settings_are_read_back_after_a_command_changes_them() {
        let mut rig = Rig::new();
        rig.act(
            "mode",
            CameraAction::SetMode {
                mode: CameraMode::Video,
            },
        );
        assert_eq!(rig.done("mode"), Some((CommandOutcome::Accepted, 1)));
        let (component, params) = rig.commands(SET_CAMERA_MODE)[0];
        assert_eq!((component, params[0], params[1]), (100, 0.0, 1.0));
        assert!(params[2..].iter().all(|reserved| reserved.is_nan()));
        assert_eq!(rig.state().settings.map(|s| s.mode), Some(1));

        rig.act("in", CameraAction::ZoomStep { steps: 1 });
        assert_eq!(rig.done("in"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.commands(SET_CAMERA_ZOOM),
            [(100, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        assert_eq!(rig.state().settings.and_then(|s| s.zoom_pct), Some(10.0));

        rig.act("near", CameraAction::FocusTo { percent: 40.0 });
        assert_eq!(
            rig.commands(SET_CAMERA_FOCUS),
            [(100, [2.0, 40.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        assert_eq!(rig.state().settings.and_then(|s| s.focus_pct), Some(40.0));

        rig.act("reset", CameraAction::Reset);
        assert_eq!(rig.done("reset"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.commands(RESET_CAMERA_SETTINGS),
            [(100, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        assert_eq!(
            rig.state().settings,
            Some(Settings {
                mode: 0,
                zoom_pct: Some(0.0),
                focus_pct: Some(100.0)
            })
        );
        // The camera said so itself each time: nothing was asked again.
        rig.second();
        assert_eq!(rig.asked(CAMERA_SETTINGS), 1);
    }

    // A held zoom or focus runs at a rate until a rate of 0 stops it, and
    // a toggle the engine did not resolve is never sent (S416).
    #[test]
    fn a_zoom_or_focus_at_a_rate_runs_until_stopped_and_a_toggle_is_not_sent() {
        let mut rig = Rig::new();
        rig.act("in", CameraAction::ZoomRate { rate: 1.0 });
        assert_eq!(rig.done("in"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.commands(SET_CAMERA_ZOOM),
            [(100, [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );
        rig.second();
        rig.act("stop", CameraAction::ZoomRate { rate: 0.0 });
        assert_eq!(rig.done("stop"), Some((CommandOutcome::Accepted, 1)));
        rig.second();
        let zoomed = rig
            .state()
            .settings
            .and_then(|s| s.zoom_pct)
            .expect("a zoom");
        assert!(
            (20.0..=30.0).contains(&zoomed),
            "about a second's run, {zoomed}"
        );
        rig.second();
        assert_eq!(rig.state().settings.and_then(|s| s.zoom_pct), Some(zoomed));

        rig.act("near", CameraAction::FocusRate { rate: -0.5 });
        assert_eq!(rig.done("near"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.commands(SET_CAMERA_FOCUS),
            [(100, [1.0, -0.5, 0.0, 0.0, 0.0, 0.0, 0.0])]
        );

        let sent = rig.sent.len();
        rig.act("toggle", CameraAction::ToggleRecording);
        assert!(!matches!(
            rig.done("toggle"),
            Some((CommandOutcome::Accepted, _))
        ));
        assert_eq!(rig.sent.len(), sent, "nothing went to the camera");
    }

    // A camera that does not say what a command changed is asked.
    #[test]
    fn settings_a_camera_does_not_send_after_a_command_are_asked_for() {
        let mut rig = Rig::new();
        rig.lost.push(CAMERA_SETTINGS);
        rig.act("in", CameraAction::ZoomStep { steps: 2 });
        assert_eq!(rig.done("in"), Some((CommandOutcome::Accepted, 1)));
        rig.lost.clear();
        rig.second();
        assert_eq!(rig.asked(CAMERA_SETTINGS), 2);
        assert_eq!(rig.state().settings.and_then(|s| s.zoom_pct), Some(20.0));
    }

    // A full card: the camera refuses to record, and what it said of its
    // card says why. A format empties the card and the list of photos.
    #[test]
    fn a_full_card_refuses_a_capture_and_a_format_empties_it() {
        let mut rig = Rig::new();
        rig.act("first", CameraAction::TakePhoto);
        rig.camera().leave_room(4.0);
        rig.second();
        assert_eq!(rig.state().capture.and_then(|c| c.available_mib), Some(4.0));
        rig.act("photo", CameraAction::TakePhoto);
        assert_eq!(
            rig.done("photo"),
            Some((CommandOutcome::Rejected("Failed".into()), 1))
        );
        rig.camera().leave_room(0.0);
        rig.act("record", CameraAction::StartRecording);
        assert_eq!(
            rig.done("record"),
            Some((CommandOutcome::Rejected("Failed".into()), 1))
        );
        rig.second();
        let capture = rig.state().capture.expect("its capture status");
        assert_eq!(
            (capture.recording, capture.available_mib),
            (false, Some(0.0))
        );

        rig.act("format", CameraAction::Format { storage_id: None });
        assert_eq!(rig.done("format"), Some((CommandOutcome::Accepted, 1)));
        assert_eq!(
            rig.commands(STORAGE_FORMAT),
            [(100, [1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0])]
        );
        rig.second();
        let state = rig.state();
        let capture = state.capture.expect("its capture status");
        assert_eq!(
            (capture.image_count, capture.available_mib),
            (0, Some(32_000.0))
        );
        assert_eq!(state.storage.map(|card| card.used_mib), Some(0.0));
        // The list starts again: the next photo is its first, and is told.
        rig.act("again", CameraAction::TakePhoto);
        assert_eq!(rig.done("again"), Some((CommandOutcome::Accepted, 1)));
        let images = rig.images();
        assert_eq!(images.len(), 2);
        assert_eq!(images[1].index, 0);
    }

    // A camera busy capturing refuses a format, and the refusal is its own.
    #[test]
    fn a_format_while_recording_is_refused_with_the_cameras_answer() {
        let mut rig = Rig::new();
        rig.act("record", CameraAction::StartRecording);
        rig.act("format", CameraAction::Format { storage_id: None });
        assert_eq!(
            rig.done("format"),
            Some((CommandOutcome::Rejected("Temporarily rejected".into()), 1))
        );
    }

    // What a command cannot say is refused before it is sent.
    #[test]
    fn a_command_that_makes_no_sense_is_not_sent() {
        let mut rig = Rig::new();
        rig.act(
            "interval",
            CameraAction::StartInterval {
                interval_s: 0.0,
                count: None,
            },
        );
        assert!(matches!(
            rig.done("interval"),
            Some((CommandOutcome::NotSent(why), 0)) if why.contains("above 0")
        ));
        assert!(rig.commands(IMAGE_START_CAPTURE).is_empty());
    }

    /// A `COMMAND_ACK` with how far the command is.
    fn ack(from: u8, command: u16, result: u8, progress: u8) -> Vec<u8> {
        let mut payload = command.to_le_bytes().to_vec();
        payload.extend_from_slice(&[result, progress, 0, 0, 0, 0, 255, 190]);
        crate::custom::frame_from((1, from, 0), 77, &payload, 143)
    }

    // MAV_RESULT_IN_PROGRESS: the camera is working on it. How far it is
    // is told, the command is not sent again, and its end is the camera's.
    #[test]
    fn a_command_in_progress_is_told_and_waits_for_its_end() {
        let mut rig = Rig::new();
        // A camera that takes its time: its own answers do not arrive.
        rig.unheard.push(STORAGE_FORMAT);
        rig.act("format", CameraAction::Format { storage_id: None });
        let progress = |rig: &Rig| -> Vec<Option<u8>> {
            rig.made
                .iter()
                .filter_map(|effect| match effect {
                    Effect::CommandProgress {
                        receipt_id,
                        progress,
                    } if receipt_id == "format" => Some(*progress),
                    _ => None,
                })
                .collect()
        };
        for (at, percent) in [(1, 20), (2, 60), (3, 255)] {
            rig.now_us += SECOND;
            let effects = rig.session.ingest(
                "l1",
                PEER,
                &ack(100, STORAGE_FORMAT, 5, percent),
                rig.now_us,
            );
            rig.converse(effects);
            let tick = rig.session.tick(rig.now_us);
            rig.converse(tick);
            assert_eq!(progress(&rig).len(), at);
        }
        assert_eq!(progress(&rig), [Some(20), Some(60), None]);
        assert_eq!(rig.done("format"), None);
        assert_eq!(rig.commands(STORAGE_FORMAT).len(), 1, "not sent again");
        // The autopilot's word on the same command is not the camera's.
        let other = rig
            .session
            .ingest("l1", PEER, &ack(1, STORAGE_FORMAT, 5, 90), rig.now_us);
        rig.converse(other);
        assert_eq!(progress(&rig).len(), 3);
        let end = rig
            .session
            .ingest("l1", PEER, &ack(100, STORAGE_FORMAT, 0, 0), rig.now_us);
        rig.converse(end);
        assert_eq!(rig.done("format"), Some((CommandOutcome::Accepted, 1)));
    }

    fn progress_told(made: &[Effect]) -> Vec<(String, Option<u8>)> {
        made.iter()
            .filter_map(|effect| match effect {
                Effect::CommandProgress {
                    receipt_id,
                    progress,
                } => Some((receipt_id.clone(), *progress)),
                _ => None,
            })
            .collect()
    }

    // UXD-248: a camera's own command waits on the camera it was sent to.
    // The autopilot's word on the same command, accepted, refused or in
    // progress, does not end it, is not told as how far it is, and does
    // not hold off sending it again.
    #[test]
    fn a_cameras_command_is_answered_by_the_camera_and_not_its_autopilot() {
        let mut rig = Rig::new();
        rig.unheard.push(STORAGE_FORMAT);
        rig.act("format", CameraAction::Format { storage_id: None });
        for result in [0, 4, 5] {
            let effects =
                rig.session
                    .ingest("l1", PEER, &ack(1, STORAGE_FORMAT, result, 50), rig.now_us);
            rig.converse(effects);
        }
        assert_eq!(rig.done("format"), None);
        assert_eq!(progress_told(&rig.made), []);
        rig.now_us += Config::default().command_timeout_us;
        let working =
            rig.session
                .ingest("l1", PEER, &ack(1, STORAGE_FORMAT, 5, 60), rig.now_us - 1);
        rig.converse(working);
        let tick = rig.session.tick(rig.now_us);
        rig.converse(tick);
        let sent_to: Vec<u8> = rig
            .commands(STORAGE_FORMAT)
            .iter()
            .map(|(component, _)| *component)
            .collect();
        assert_eq!(sent_to, [100, 100], "sent again, as when nothing answers");
        let end = rig
            .session
            .ingest("l1", PEER, &ack(100, STORAGE_FORMAT, 4, 0), rig.now_us);
        rig.converse(end);
        assert!(matches!(
            rig.done("format"),
            Some((CommandOutcome::Rejected(_), 2))
        ));
        assert_eq!(progress_told(&rig.made), []);
    }

    // UXD-248: a command for a camera its autopilot speaks for goes to the
    // autopilot and waits on the autopilot. A camera component's word on
    // the same command is not its answer or how far it is.
    #[test]
    fn an_autopilots_cameras_command_is_answered_by_the_autopilot_and_not_a_camera() {
        use mavio::dialects::ardupilotmega::enums::CameraCapFlags;
        use mavio::dialects::ardupilotmega::messages::CameraInformation;

        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 0), 0);
        let said = CameraInformation {
            flags: CameraCapFlags::CAPTURE_IMAGE,
            camera_device_id: 2,
            ..CameraInformation::default()
        };
        session.ingest("l1", PEER, &crate::sim::frame(1, 1, 1, &said), 10);
        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let sent = session.command(
            CommandOrder {
                receipt_id: "photo".into(),
                vehicle_id,
                command: Command::Camera {
                    component_id: Some(1),
                    device_id: Some(2),
                    action: CameraAction::TakePhoto,
                },
                force: false,
            },
            40,
        );
        let to: Vec<u8> = sent
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                _ => None,
            })
            .filter(|command| command.command == IMAGE_START_CAPTURE)
            .map(|command| command.to.1)
            .collect();
        assert_eq!(to, [1]);
        let done = |effects: &[Effect]| {
            effects.iter().find_map(|effect| match effect {
                Effect::CommandDone {
                    receipt_id,
                    outcome,
                    attempts,
                } if receipt_id == "photo" => Some((outcome.clone(), *attempts)),
                _ => None,
            })
        };
        for (at, result) in [(50, 0), (51, 4), (52, 5)] {
            let camera = session.ingest("l1", PEER, &ack(100, IMAGE_START_CAPTURE, result, 10), at);
            assert_eq!(done(&camera), None, "a camera said {result}");
            assert_eq!(progress_told(&camera), [], "a camera said {result}");
        }
        let working = session.ingest("l1", PEER, &ack(1, IMAGE_START_CAPTURE, 5, 30), 60);
        assert_eq!(done(&working), None);
        assert_eq!(progress_told(&working), [("photo".to_owned(), Some(30))]);
        let end = session.ingest("l1", PEER, &ack(1, IMAGE_START_CAPTURE, 0, 0), 70);
        assert_eq!(done(&end), Some((CommandOutcome::Accepted, 1)));
    }

    // S409: ArduPilot leaves `camera_device_id` out. Its plain cameras put
    // their number in `lens_id`, and a camera on one of its mounts puts
    // there the lens it looks through, as its Viewpro driver does with 1
    // for the daylight sensor. Listed by that as the autopilot's second
    // camera, which it does not have, its photo failed and its zoom was
    // acknowledged and did nothing. An autopilot's only camera, which it
    // did not number, is commanded as every camera it has.
    #[test]
    fn an_autopilots_only_camera_it_did_not_number_is_commanded_as_all_its_cameras() {
        use mavio::dialects::ardupilotmega::enums::CameraCapFlags;
        use mavio::dialects::ardupilotmega::messages::CameraInformation;

        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 0), 0);
        let on_a_mount = CameraInformation {
            flags: CameraCapFlags::CAPTURE_IMAGE | CameraCapFlags::HAS_BASIC_ZOOM,
            lens_id: 1,
            ..CameraInformation::default()
        };
        session.ingest("l1", PEER, &crate::sim::frame(1, 1, 1, &on_a_mount), 10);
        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let mut at = 40;
        let mut sent = |session: &mut Session, action: CameraAction, command: u16| {
            at += 10;
            let made = session.command(
                CommandOrder {
                    receipt_id: format!("r{at}"),
                    vehicle_id: vehicle_id.clone(),
                    command: Command::Camera {
                        component_id: Some(1),
                        // What UXDuck lists it under: its lens id and one.
                        device_id: Some(2),
                        action: action.clone(),
                    },
                    force: false,
                },
                at,
            );
            let params: Vec<[f32; 7]> = made
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                    _ => None,
                })
                .filter(|sent| sent.command == command && sent.to == (1, 1))
                .map(|sent| sent.params)
                .collect();
            session.ingest("l1", PEER, &ack(1, command, 0, 0), at + 1);
            assert_eq!(params.len(), 1, "{action:?}");
            params[0]
        };
        let photo = sent(&mut session, CameraAction::TakePhoto, IMAGE_START_CAPTURE);
        assert!(photo[0].abs() < f32::EPSILON, "{photo:?}");
        assert!(photo[3] >= 1.0, "one photo is still numbered: {photo:?}");
        let zoom = sent(
            &mut session,
            CameraAction::ZoomTo { percent: 50.0 },
            SET_CAMERA_ZOOM,
        );
        assert!(zoom[2].abs() < f32::EPSILON, "{zoom:?}");
        let record = sent(
            &mut session,
            CameraAction::StartRecording,
            VIDEO_START_CAPTURE,
        );
        assert!(record[2].abs() < f32::EPSILON, "{record:?}");

        // Told of another camera, each is named by its number as before.
        let plain = CameraInformation {
            flags: CameraCapFlags::CAPTURE_IMAGE,
            lens_id: 0,
            ..CameraInformation::default()
        };
        session.ingest("l1", PEER, &crate::sim::frame(1, 1, 2, &plain), 200);
        let photo = sent(&mut session, CameraAction::TakePhoto, IMAGE_START_CAPTURE);
        assert!((photo[0] - 2.0).abs() < f32::EPSILON, "{photo:?}");
    }

    // An autopilot that numbers its camera is taken at its word, alone or
    // not.
    #[test]
    fn an_autopilots_only_camera_it_numbered_is_commanded_by_its_number() {
        use mavio::dialects::ardupilotmega::enums::CameraCapFlags;
        use mavio::dialects::ardupilotmega::messages::CameraInformation;

        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 0), 0);
        let said = CameraInformation {
            flags: CameraCapFlags::CAPTURE_IMAGE,
            camera_device_id: 2,
            ..CameraInformation::default()
        };
        session.ingest("l1", PEER, &crate::sim::frame(1, 1, 1, &said), 10);
        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let made = session.command(
            CommandOrder {
                receipt_id: "photo".into(),
                vehicle_id,
                command: Command::Camera {
                    component_id: Some(1),
                    device_id: Some(2),
                    action: CameraAction::TakePhoto,
                },
                force: false,
            },
            40,
        );
        let named: Vec<f32> = made
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                _ => None,
            })
            .filter(|sent| sent.command == IMAGE_START_CAPTURE)
            .map(|sent| sent.params[0])
            .collect();
        assert_eq!(named.len(), 1);
        assert!((named[0] - 2.0).abs() < f32::EPSILON, "{named:?}");
    }

    // An answer of in progress heard before a command was sent is an
    // earlier command's. It is not told as how far this one is, as it does
    // not hold off sending it again (UXD-248).
    #[test]
    fn progress_heard_before_a_command_was_sent_is_not_told_as_its_own() {
        let mut rig = Rig::new();
        rig.unheard.push(STORAGE_FORMAT);
        rig.act("format", CameraAction::Format { storage_id: None });
        let earlier =
            rig.session
                .ingest("l1", PEER, &ack(100, STORAGE_FORMAT, 5, 20), rig.now_us - 1);
        rig.converse(earlier);
        assert_eq!(progress_told(&rig.made), []);
        let its_own = rig
            .session
            .ingest("l1", PEER, &ack(100, STORAGE_FORMAT, 5, 40), rig.now_us);
        rig.converse(its_own);
        assert_eq!(progress_told(&rig.made), [("format".to_owned(), Some(40))]);
    }

    #[test]
    fn progress_is_read_from_the_acknowledgement() {
        assert_eq!(crate::decode::ack_progress(&ack(100, 526, 5, 40)), Some(40));
        assert_eq!(crate::decode::ack_progress(&ack(100, 526, 5, 255)), None);
        // MAVLink 2 leaves trailing zeros out: 0 percent.
        assert_eq!(
            crate::decode::ack_progress(&crate::sim::command_ack_frame(1, 100, (0, 0), 0, 526, 5)),
            Some(0)
        );
        assert!(matches!(
            decode(&ack(100, 526, 5, 40)).map(|d| d.message),
            Some(Message::CommandAck {
                command: 526,
                result: 5,
                ..
            })
        ));
    }

    // A camera from before MAV_CMD_REQUEST_MESSAGE: asked three times with
    // it, then three times with the command it superseded, and then left
    // alone. Once the older command is what it answers, it is what is
    // sent.
    #[test]
    fn a_camera_is_asked_three_times_then_with_the_superseded_commands() {
        let key: Key = ("l1".into(), PEER.into(), 1, 100);
        let mut cameras = Cameras::default();
        cameras.heartbeat(&key, false, 0);
        cameras.information(
            &key,
            crate::camera::Information {
                flags: 1 | 2,
                ..Default::default()
            },
        );
        let mut asked = Vec::new();
        for second in 0..40 {
            for (_, send) in cameras.control_requests(second * SECOND, |_| true) {
                asked.push((second, send));
            }
        }
        let of = |message: u32, old: u16| -> Vec<(u64, bool)> {
            asked
                .iter()
                .filter_map(|(second, send)| match send {
                    Send::Message { id, .. } if *id == message => Some((*second, false)),
                    Send::Superseded { command, .. } if *command == old => Some((*second, true)),
                    _ => None,
                })
                .collect()
        };
        let expected = [
            (0, false),
            (3, false),
            (6, false),
            (9, true),
            (12, true),
            (15, true),
        ];
        assert_eq!(of(CAMERA_SETTINGS, 522), expected);
        assert_eq!(of(STORAGE_INFORMATION, 525), expected);
        assert_eq!(of(CAMERA_CAPTURE_STATUS, 527), expected);
        assert!(asked.iter().any(|(_, send)| *send
            == Send::Superseded {
                command: 525,
                params: [0.0, 1.0]
            }));
        assert!(
            !asked
                .iter()
                .any(|(_, send)| matches!(send, Send::Interval { .. })),
            "a camera with no capture status is not asked to send it"
        );
        // It answers the older command for its capture status: five
        // seconds without one, and that is what is sent.
        cameras.control_requests(40 * SECOND, |_| true);
        cameras.acknowledged(&key, IMAGE_START_CAPTURE, 0, 40 * SECOND);
        let mut later = Vec::new();
        for tenth in 400..420 {
            later.extend(cameras.control_requests(tenth * SECOND / 10, |_| true));
        }
        assert_eq!(later.len(), 1);
        // The first three tries of a new want are the newer command's.
        assert!(matches!(later[0].1, Send::Message { id: 262, .. }));
    }

    // A camera its autopilot speaks for: what the autopilot says with a
    // device id is that camera's, and its commands go to the autopilot
    // with the id in the command's own parameter.
    #[test]
    fn an_autopilots_camera_is_read_and_worked_through_its_autopilot() {
        use mavio::dialects::ardupilotmega::enums::CameraCapFlags;
        use mavio::dialects::ardupilotmega::messages::{CameraCaptureStatus, CameraInformation};

        let mut session = Session::new(Config::default());
        let mut made = session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 0), 0);
        let said = CameraInformation {
            flags: CameraCapFlags::CAPTURE_IMAGE | CameraCapFlags::CAPTURE_VIDEO,
            camera_device_id: 2,
            ..CameraInformation::default()
        };
        made.extend(session.ingest("l1", PEER, &crate::sim::frame(1, 1, 1, &said), 10));
        made.extend(session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 2), 20));
        let asked: Vec<(u8, f32)> = made
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                _ => None,
            })
            .filter(|command| command.command == 512)
            .map(|command| (command.to.1, command.params[0]))
            .collect();
        for message in [260.0, 261.0, 262.0] {
            assert!(asked.contains(&(1, message)), "{message} of the autopilot");
        }
        let status = CameraCaptureStatus {
            image_count: 7,
            camera_device_id: 2,
            available_capacity: 100.0,
            ..CameraCaptureStatus::default()
        };
        let told = session.ingest("l1", PEER, &crate::sim::frame(1, 1, 3, &status), 30);
        let state = told
            .iter()
            .find_map(|effect| match effect {
                Effect::CameraState {
                    component_id: 1,
                    device_id: Some(2),
                    state,
                    ..
                } => Some(state),
                _ => None,
            })
            .expect("the state of camera 2");
        assert_eq!(state.capture.map(|c| c.image_count), Some(7));

        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let sent = session.command(
            CommandOrder {
                receipt_id: "photo".into(),
                vehicle_id,
                command: Command::Camera {
                    component_id: Some(1),
                    device_id: Some(2),
                    action: CameraAction::TakePhoto,
                },
                force: false,
            },
            40,
        );
        let command = sent
            .iter()
            .find_map(|effect| match effect {
                Effect::Send { bytes, .. } => crate::payload::wire::command(bytes),
                _ => None,
            })
            .expect("the command");
        assert_eq!((command.to.1, command.command), (1, IMAGE_START_CAPTURE));
        // Camera 2, one image, numbered after the seven it has taken.
        assert_eq!(command.params[..4], [2.0, 0.0, 1.0, 8.0]);
    }

    // A camera with no address of its own: every camera, through the
    // autopilot, as "Also save on the vehicle" always sent it (V20).
    #[test]
    fn a_command_for_no_camera_in_particular_goes_to_the_autopilot_for_all() {
        let mut rig = Rig::new();
        let order = CommandOrder {
            receipt_id: "all".into(),
            vehicle_id: rig.id(),
            command: Command::Camera {
                component_id: None,
                device_id: None,
                action: CameraAction::TakePhoto,
            },
            force: false,
        };
        let effects = rig.session.command(order, rig.now_us);
        rig.converse(effects);
        assert_eq!(
            rig.commands(IMAGE_START_CAPTURE),
            [(1, [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0])]
        );
    }

    #[test]
    fn room_left_is_told_in_steps_and_when_the_card_fills() {
        // A hundredth of a 32 000 MiB card is 320 MiB.
        let total = Some(32_000.0);
        assert_eq!(coarse(None, 31_000.0, total), 31_000.0);
        assert_eq!(coarse(Some(31_000.0), 30_900.0, total), 31_000.0);
        assert_eq!(coarse(Some(31_000.0), 30_600.0, total), 30_600.0);
        // Under the line a card counts as full, and at nothing left.
        assert_eq!(coarse(Some(100.0), 15.0, total), 15.0);
        assert_eq!(coarse(Some(15.0), 4.0, total), 15.0);
        assert_eq!(coarse(Some(15.0), 0.0, total), 0.0);
        assert_eq!(coarse(Some(0.0), 20.0, total), 20.0);
    }
}

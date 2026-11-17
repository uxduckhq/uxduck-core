//! A simulated payload (ADR-0063): a camera on component 100 and a gimbal
//! on component 154 of a vehicle's own system, as plain state. Frames go
//! in, frames come out, and it moves on a clock it is given. It has no
//! sockets and no threads, so it runs in the daemon and in the browser's
//! worker alike.
//!
//! Two set-ups. With an autopilot that manages the gimbal (ArduPilot with
//! `MNT1_TYPE 6`), the gimbal is a device alone and takes its attitude
//! from component 1. With no such autopilot, as on the fake vehicle, the
//! gimbal's component is its own manager too, as the guide's integrated
//! camera and gimbal is.
//!
//! What ArduPilot needs of it was measured in S400
//! (`docs/plan/payload-over-one-link.md`).

pub mod camera;
pub mod faults;
pub mod gimbal;
mod link;
mod manager;
pub mod wire;

#[cfg(test)]
mod tests;

use mavio::dialects::ardupilotmega::Ardupilotmega;

pub use camera::{Camera, View};
pub use faults::Faults;
pub use gimbal::Gimbal;
pub use link::{Defined, Heard, OnLink};
use wire::{Command, DENIED, REQUEST_MESSAGE, SET_MESSAGE_INTERVAL, UNSUPPORTED};

/// The autopilot's component: the vehicle's clock and attitude come from
/// it, and it manages the gimbal unless the payload manages itself.
pub const AUTOPILOT: u8 = 1;

const COMMAND_INT: u32 = 75;
const COMMAND_LONG: u32 = 76;
const ATTITUDE: u32 = 30;
const GLOBAL_POSITION_INT: u32 = 33;
const GIMBAL_MANAGER_SET_ATTITUDE: u32 = 282;
const GIMBAL_DEVICE_SET_ATTITUDE: u32 = 284;
const AUTOPILOT_STATE_FOR_GIMBAL_DEVICE: u32 = 286;
const GIMBAL_MANAGER_SET_PITCHYAW: u32 = 287;
const GIMBAL_MANAGER_SET_MANUAL_CONTROL: u32 = 288;
// The camera's files and its own settings (S411).
const FILE_TRANSFER_PROTOCOL: u32 = 110;
const PARAM_EXT_REQUEST_READ: u32 = 320;
const PARAM_EXT_REQUEST_LIST: u32 = 321;
const PARAM_EXT_SET: u32 = 323;

/// A clock that ticks about once an interval is still on time when a tick
/// comes this much early.
const EARLY_MS: u64 = 10;

/// Something done every so often on the simulation's clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Every {
    interval_ms: u64,
    due_ms: u64,
}

impl Every {
    /// Due at the first step.
    pub(crate) fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms,
            due_ms: 0,
        }
    }

    /// First due one interval after `now_ms`.
    pub(crate) fn after(interval_ms: u64, now_ms: u64) -> Self {
        Self {
            interval_ms,
            due_ms: now_ms + interval_ms,
        }
    }

    pub(crate) fn interval_ms(self) -> u64 {
        self.interval_ms
    }

    /// Whether it is time. A little early keeps its place, so the rate
    /// holds. Late counts the next one from now, so a long gap is not made
    /// up with a burst. A clock that went back, as an autopilot's does when
    /// it restarts, is due at once.
    pub(crate) fn due(&mut self, now_ms: u64) -> bool {
        let went_back = self.due_ms > now_ms + self.interval_ms;
        if now_ms + EARLY_MS < self.due_ms && !went_back {
            return false;
        }
        self.due_ms = if now_ms < self.due_ms && !went_back {
            self.due_ms + self.interval_ms
        } else {
            now_ms + self.interval_ms
        };
        true
    }
}

/// Seconds from one step to the next. A clock that went back or jumped
/// ahead moves nothing far.
#[allow(clippy::cast_precision_loss, reason = "at most a thousand")]
pub(crate) fn elapsed(before_ms: u64, now_ms: u64) -> f32 {
    now_ms.saturating_sub(before_ms).min(1000) as f32 / 1000.0
}

/// What a component says to a command it cannot take. Nothing when the
/// command was for every component, since another may have it.
pub(crate) fn refusal(command: &Command) -> Option<u8> {
    if command.to.1 == 0 {
        return None;
    }
    // It has the two commands that ask for messages, though not that message.
    Some(
        if matches!(command.command, REQUEST_MESSAGE | SET_MESSAGE_INTERVAL) {
            DENIED
        } else {
            UNSUPPORTED
        },
    )
}

/// Where the vehicle is, as `GLOBAL_POSITION_INT` gives it: degrees times
/// 1e7, and millimetres above sea level and above home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub lat: i32,
    pub lon: i32,
    pub alt_mm: i32,
    pub relative_alt_mm: i32,
}

/// Who manages the gimbal device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedBy {
    /// The autopilot, component 1 of the same system: the device alone.
    Autopilot,
    /// The payload: the gimbal's component speaks as its manager too.
    Itself,
}

/// What kind of camera and gimbal a payload is (ADR-0063 Y16): each is a
/// kind of hardware, and answers as that kind does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// A camera that takes photos, records, zooms and focuses, and a
    /// gimbal device apart from it.
    #[default]
    Standard,
    /// A camera that only sends video: no shutter, modes, lens or card.
    StreamOnly,
    /// A camera that takes photos in its photo mode and records in its
    /// video mode, with no zoom or focus to drive.
    PhotoVideo,
    /// A camera and a gimbal that are one unit and one component, the
    /// camera's. The unit manages its own gimbal (the guide's standalone
    /// integrated camera and gimbal).
    Integrated,
    /// Such a unit with a thermal sensor beside its daylight one, which
    /// tracks a point or a box in its picture and keeps its gimbal on it
    /// (ADR-0063 Y5).
    ThermalTracking,
}

impl Profile {
    /// Whether the camera and the gimbal are one unit and one component.
    #[must_use]
    pub fn unit(self) -> bool {
        matches!(self, Self::Integrated | Self::ThermalTracking)
    }
}

impl From<Option<uxduck_core::domain::link::PayloadProfile>> for Profile {
    fn from(profile: Option<uxduck_core::domain::link::PayloadProfile>) -> Self {
        use uxduck_core::domain::link::PayloadProfile;
        match profile {
            None => Self::Standard,
            Some(PayloadProfile::StreamOnly) => Self::StreamOnly,
            Some(PayloadProfile::PhotoVideo) => Self::PhotoVideo,
            Some(PayloadProfile::Integrated) => Self::Integrated,
            Some(PayloadProfile::ThermalTracking) => Self::ThermalTracking,
        }
    }
}

/// A vehicle's camera, its gimbal, or both.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one thing that is so or is not"
)]
pub struct Payload {
    system: u8,
    pub camera: Option<Camera>,
    /// The camera has no power (S404): it sends no heartbeats and answers
    /// nothing, as one unplugged, and a frame that names it is still not
    /// the autopilot's. With power again it carries on as it was.
    pub camera_off: bool,
    pub gimbal: Option<Gimbal>,
    /// The gimbal has no power, as a fault (ADR-0063 Y16): no heartbeats,
    /// no attitude and no answers, and a frame that names it is still not
    /// the autopilot's.
    pub gimbal_off: bool,
    /// The camera and the gimbal are one unit on the camera's component.
    integrated: bool,
    /// The vehicle's roll, pitch and yaw, once known.
    attitude: Option<[f32; 3]>,
    place: Option<Place>,
    heartbeat: Every,
    /// A heartbeat went out on the simulation's clock since `announce`.
    beat: bool,
    /// One went out on the host's clock since `step` had one due.
    announced: bool,
}

impl Payload {
    /// A payload with nothing on it yet, for the vehicle with this system id.
    #[must_use]
    pub fn new(system: u8) -> Self {
        Self {
            system,
            camera: None,
            camera_off: false,
            gimbal: None,
            gimbal_off: false,
            integrated: false,
            attitude: None,
            place: None,
            heartbeat: Every::new(1000),
            beat: false,
            announced: false,
        }
    }

    /// With a camera, which names this video stream when it has one.
    #[must_use]
    pub fn with_camera(mut self, stream: Option<String>) -> Self {
        let mut camera = Camera::new(self.system);
        camera.stream = stream;
        self.camera = Some(camera);
        self.settle();
        self
    }

    /// What the camera's video shows now (S408), unless it has no power or
    /// does not know where it is.
    #[must_use]
    pub fn view(&self) -> Option<View> {
        self.camera
            .as_ref()
            .filter(|_| !self.camera_off)
            .and_then(Camera::view)
    }

    /// With a gimbal.
    #[must_use]
    pub fn with_gimbal(mut self, managed_by: ManagedBy) -> Self {
        self.gimbal = Some(Gimbal::new(self.system, managed_by));
        self.settle();
        self
    }

    /// With a camera and a gimbal that are one unit on the camera's
    /// component (ADR-0063 Y16): the unit manages its gimbal itself, calls
    /// it gimbal 1 as a manager does a gimbal that is no component of its
    /// own, and has one heartbeat, a camera's.
    #[must_use]
    pub fn with_unit(mut self, stream: Option<String>) -> Self {
        self = self.with_camera(stream);
        self.gimbal = Some(Gimbal::inside(self.system, camera::COMPONENT));
        self.integrated = true;
        self.settle();
        self
    }

    /// Switches faults on and off (ADR-0063 Y16): everything given is as
    /// it is from now, so a fault left out is switched off. One that does
    /// not fit what the payload carries does nothing.
    pub fn set_faults(&mut self, faults: Faults) {
        self.camera_off = faults.camera_silent;
        self.gimbal_off = faults.gimbal_silent && !self.integrated;
        if let Some(camera) = &mut self.camera {
            camera.fill_card(faults.card_full);
            (camera.answers.refused, camera.answers.slow) = (faults.refused, faults.slow);
        }
        if let Some(gimbal) = &mut self.gimbal {
            gimbal.failed = faults.gimbal_error;
            gimbal.hold_for_autopilot(faults.autopilot_holds);
            (gimbal.answers.refused, gimbal.answers.slow) = (faults.refused, faults.slow);
        }
    }

    /// The faults that are on now. A full card that was formatted and a
    /// gimbal that another took from the autopilot are over by themselves.
    #[must_use]
    pub fn faults(&self) -> Faults {
        let camera = self.camera.as_ref();
        let gimbal = self.gimbal.as_ref();
        Faults {
            camera_silent: self.camera_off,
            gimbal_silent: self.gimbal_off,
            card_full: camera.is_some_and(Camera::card_filled),
            refused: camera
                .map(|camera| camera.answers.refused)
                .or(gimbal.map(|gimbal| gimbal.answers.refused))
                .flatten(),
            slow: camera
                .map(|camera| camera.answers.slow)
                .or(gimbal.map(|gimbal| gimbal.answers.slow))
                .flatten(),
            gimbal_error: gimbal.is_some_and(|gimbal| gimbal.failed),
            autopilot_holds: gimbal.is_some_and(Gimbal::held_for_autopilot),
        }
    }

    /// The vehicle's roll, pitch and yaw in radians and where it is, from a
    /// host that knows them without frames, as the fake vehicle does.
    pub fn vehicle(&mut self, attitude: [f32; 3], place: Option<Place>) {
        self.attitude = Some(attitude);
        self.place = place;
    }

    /// Heartbeats on the host's own clock, from the link's first moment and
    /// then about once a second. The simulation's clock does not run until
    /// the autopilot is heard, and at 30 times real time a second of the
    /// host's is half a minute of the autopilot's, which looks for a
    /// payload only in its first minute (S400). Once `step` sends them,
    /// this adds none.
    pub fn announce(&mut self) -> Vec<Vec<u8>> {
        if std::mem::take(&mut self.beat) {
            return Vec::new();
        }
        self.announced = true;
        self.heartbeats()
    }

    /// Moves everything to `now_ms` on the simulation's clock, milliseconds
    /// since the vehicle started, and returns what is due.
    pub fn step(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        if self.heartbeat.due(now_ms) {
            // After one on the host's clock, this second has had its own.
            if !std::mem::take(&mut self.announced) {
                frames.extend(self.heartbeats());
            }
            self.beat = true;
        }
        self.settle();
        if !self.gimbal_silent() {
            frames.extend(self.as_unit(|gimbal| gimbal.step(now_ms)));
        }
        self.settle();
        if !self.camera_off
            && let Some(camera) = &mut self.camera
        {
            frames.extend(camera.step(now_ms));
        }
        frames
    }

    /// Whether the gimbal says and hears nothing: its own power is off, or
    /// the power of the unit it is inside.
    fn gimbal_silent(&self) -> bool {
        self.gimbal_off || (self.integrated && self.camera_off)
    }

    /// What the gimbal says to something. Inside a unit it speaks as the
    /// camera's component, so the two count their frames as one sender.
    fn as_unit(&mut self, say: impl FnOnce(&mut Gimbal) -> Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let Some(gimbal) = &mut self.gimbal else {
            return Vec::new();
        };
        let Some(camera) = self.camera.as_mut().filter(|_| self.integrated) else {
            return say(gimbal);
        };
        gimbal.sent = camera.sent;
        let frames = say(gimbal);
        camera.sent = gimbal.sent;
        frames
    }

    /// A command for a unit that is one component: its camera's part
    /// answers what it has, its gimbal's part what that has, and what
    /// neither has is refused once.
    fn unit_command(&mut self, command: &Command) -> Vec<Vec<u8>> {
        let Some(camera) = &mut self.camera else {
            return Vec::new();
        };
        if self.camera_off || !hears(camera.component, command.to.1) {
            return Vec::new();
        }
        if let Some(frames) = camera.tried(command) {
            return frames;
        }
        let mut had = true;
        let frames = self.as_unit(|gimbal| {
            gimbal.tried(command).unwrap_or_else(|| {
                had = false;
                Vec::new()
            })
        });
        if had {
            return frames;
        }
        let Some(camera) = &mut self.camera else {
            return Vec::new();
        };
        camera.command(command)
    }

    /// Whether a frame names one of the payload's components as the one it
    /// is for. Such a frame is the payload's to answer and is kept from the
    /// autopilot, which would act on it too (S400).
    #[must_use]
    pub fn takes(&self, frame: &[u8]) -> bool {
        addressee(frame)
            .is_some_and(|(system, component)| system == self.system && self.has(component))
    }

    /// One whole frame from the link, and what the payload says to it.
    pub fn receive(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let Some(header) = crate::framing::header(frame) else {
            return Vec::new();
        };
        match header.message_id {
            COMMAND_INT | COMMAND_LONG => self.command(frame),
            ATTITUDE
            | GLOBAL_POSITION_INT
            | GIMBAL_MANAGER_SET_ATTITUDE
            | GIMBAL_DEVICE_SET_ATTITUDE
            | AUTOPILOT_STATE_FOR_GIMBAL_DEVICE
            | GIMBAL_MANAGER_SET_PITCHYAW
            | GIMBAL_MANAGER_SET_MANUAL_CONTROL => self.message(frame),
            FILE_TRANSFER_PROTOCOL
            | PARAM_EXT_REQUEST_READ
            | PARAM_EXT_REQUEST_LIST
            | PARAM_EXT_SET => self.camera_settings(frame),
            _ => Vec::new(),
        }
    }

    /// A request for the camera's files or its own settings (S411). A file
    /// request is the camera's only when it names the camera, as each
    /// component has files of its own. A request about settings may be
    /// for every component.
    fn camera_settings(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let Some((from, message)) = wire::typed(frame) else {
            return Vec::new();
        };
        let Some((system, component)) = settings_addressee(&message) else {
            return Vec::new();
        };
        let files = matches!(message, Ardupilotmega::FileTransferProtocol(_));
        if self.camera_off || (system != self.system && (files || system != 0)) {
            return Vec::new();
        }
        match &mut self.camera {
            Some(camera) if component == camera.component || (!files && component == 0) => {
                camera.settings_message(from, &message)
            }
            _ => Vec::new(),
        }
    }

    fn command(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let Some(command) = wire::command(frame) else {
            return Vec::new();
        };
        if !matches!(command.to.0, 0) && command.to.0 != self.system {
            return Vec::new();
        }
        self.settle();
        if self.integrated {
            let frames = self.unit_command(&command);
            // A target its camera took or dropped is its gimbal's to know
            // at once.
            self.follow_tracked();
            return frames;
        }
        let mut frames = Vec::new();
        if !self.camera_off
            && let Some(camera) = &mut self.camera
            && hears(camera.component, command.to.1)
        {
            frames.extend(camera.command(&command));
        }
        if !self.gimbal_off
            && let Some(gimbal) = &mut self.gimbal
            && hears(gimbal.component, command.to.1)
        {
            frames.extend(gimbal.command(&command));
        }
        frames
    }

    fn message(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let Some((from, message)) = wire::typed(frame) else {
            return Vec::new();
        };
        let autopilot = from == (self.system, AUTOPILOT);
        match &message {
            // The autopilot's clock (S400): everything the payload paces
            // is counted on it, at any speed of the simulation.
            Ardupilotmega::Attitude(attitude) if autopilot => {
                self.attitude = Some([attitude.roll, attitude.pitch, attitude.yaw]);
                return self.step(u64::from(attitude.time_boot_ms));
            }
            // Latitude and longitude 0 is no position.
            Ardupilotmega::GlobalPositionInt(position) if autopilot => {
                self.place = (position.lat != 0 || position.lon != 0).then_some(Place {
                    lat: position.lat,
                    lon: position.lon,
                    alt_mm: position.alt,
                    relative_alt_mm: position.relative_alt,
                });
            }
            Ardupilotmega::AutopilotStateForGimbalDevice(state) if autopilot => {
                let gimbal = self.gimbal.as_ref().map(|gimbal| gimbal.component);
                if (state.target_system == 0 || state.target_system == self.system)
                    && gimbal.is_some_and(|gimbal| hears(gimbal, state.target_component))
                {
                    self.attitude = wire::angles(state.q).or(self.attitude);
                }
            }
            _ => {
                self.settle();
                if !self.gimbal_silent()
                    && let Some(gimbal) = &mut self.gimbal
                {
                    gimbal.message(from, &message);
                }
            }
        }
        Vec::new()
    }

    fn heartbeats(&mut self) -> Vec<Vec<u8>> {
        let off = self.camera_off;
        let camera = self.camera.as_mut().filter(|_| !off).map(Camera::heartbeat);
        // A gimbal inside a unit has the unit's heartbeat, the camera's.
        let apart = !self.integrated && !self.gimbal_off;
        let gimbal = self
            .gimbal
            .as_mut()
            .filter(|_| apart)
            .map(Gimbal::heartbeat);
        camera.into_iter().chain(gimbal).collect()
    }

    fn has(&self, component: u8) -> bool {
        self.camera.as_ref().map(|camera| camera.component) == Some(component)
            || self.gimbal.as_ref().map(|gimbal| gimbal.component) == Some(component)
    }

    /// Tells each part what it needs of the vehicle and of the other. A
    /// camera on a gimbal looks where the gimbal points and names it. A
    /// camera alone is fixed to the airframe.
    fn settle(&mut self) {
        let vehicle = self.attitude.unwrap_or([0.0; 3]);
        if let Some(gimbal) = &mut self.gimbal {
            // A heading may come as 0 to 2π. The gimbal keeps -π to π.
            gimbal.vehicle_yaw = self.attitude.map(|[_, _, yaw]| wire::wrap(yaw));
            gimbal.place = self.place;
        }
        if let Some(camera) = &mut self.camera {
            camera.place = self.place;
            // A gimbal inside the camera's unit is named by the number its
            // manager gives it, since it is no component of its own.
            camera.gimbal_device_id = self.gimbal.as_ref().map_or(0, |gimbal| {
                if self.integrated {
                    gimbal.device_id()
                } else {
                    gimbal.component
                }
            });
            camera.attitude = wire::quaternion(
                self.gimbal
                    .as_ref()
                    .map_or(vehicle, Gimbal::attitude_from_north),
            );
        }
        self.follow_tracked();
    }

    /// A unit keeps its own gimbal on what its camera tracks (ADR-0063
    /// Y5), from when the camera takes a target until it drops it. The
    /// gimbal aims from the vehicle's own height and the camera sees from
    /// its lens above that, so the place is given that much lower.
    fn follow_tracked(&mut self) {
        let (Some(camera), Some(gimbal), true) =
            (&mut self.camera, &mut self.gimbal, self.integrated)
        else {
            return;
        };
        match camera.take_tracked() {
            Some(camera::Tracked::Taken(lat, lon, alt_m)) => {
                gimbal.follow(lat, lon, alt_m - camera::tracking::LENS_ABOVE_M);
            }
            Some(camera::Tracked::Dropped) => gimbal.unfollow(),
            None => {}
        }
    }
}

/// The system and component a file request or a request about a
/// component's own settings is for.
fn settings_addressee(message: &Ardupilotmega) -> Option<(u8, u8)> {
    match message {
        Ardupilotmega::FileTransferProtocol(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::ParamExtRequestRead(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::ParamExtRequestList(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::ParamExtSet(m) => Some((m.target_system, m.target_component)),
        _ => None,
    }
}

/// A component hears what names it and what is for every component.
fn hears(component: u8, target: u8) -> bool {
    target == 0 || target == component
}

/// The system and component a frame is for, of the frames the payload
/// reads that name one.
fn addressee(frame: &[u8]) -> Option<(u8, u8)> {
    if let Some(command) = wire::command(frame) {
        return Some(command.to);
    }
    let named = matches!(
        crate::framing::header(frame)?.message_id,
        GIMBAL_MANAGER_SET_ATTITUDE
            | GIMBAL_DEVICE_SET_ATTITUDE
            | AUTOPILOT_STATE_FOR_GIMBAL_DEVICE
            | GIMBAL_MANAGER_SET_PITCHYAW
            | GIMBAL_MANAGER_SET_MANUAL_CONTROL
            | FILE_TRANSFER_PROTOCOL
            | PARAM_EXT_REQUEST_READ
            | PARAM_EXT_REQUEST_LIST
            | PARAM_EXT_SET
    );
    if !named {
        return None;
    }
    let message = wire::typed(frame)?.1;
    if let Some(named) = settings_addressee(&message) {
        return Some(named);
    }
    match message {
        Ardupilotmega::GimbalManagerSetAttitude(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::GimbalDeviceSetAttitude(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::AutopilotStateForGimbalDevice(m) => {
            Some((m.target_system, m.target_component))
        }
        Ardupilotmega::GimbalManagerSetPitchyaw(m) => Some((m.target_system, m.target_component)),
        Ardupilotmega::GimbalManagerSetManualControl(m) => {
            Some((m.target_system, m.target_component))
        }
        _ => None,
    }
}

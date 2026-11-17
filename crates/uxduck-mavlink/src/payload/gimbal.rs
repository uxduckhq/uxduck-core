//! The gimbal device (MAVLink gimbal protocol, version 2) as plain state:
//! a three-axis head that holds roll and pitch against the horizon, and a
//! yaw that follows the vehicle's nose or holds north. It goes where its
//! manager sends it, no faster than its top speed and no further than its
//! stops. `manager.rs` is the manager a payload brings when no autopilot
//! manages it.

use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::dialects::ardupilotmega::enums::{
    GimbalDeviceCapFlags, GimbalDeviceErrorFlags, GimbalDeviceFlags, MavAutopilot, MavModeFlag,
    MavState, MavType,
};
use mavio::dialects::ardupilotmega::messages::{
    GimbalDeviceAttitudeStatus, GimbalDeviceInformation, Heartbeat,
};

use super::faults::{Answers, Due, Met};
use super::manager::Manager;
use super::wire::{self, ACCEPTED, Command, DENIED, TEMPORARILY_REJECTED, wrap};
use super::{AUTOPILOT, Every, ManagedBy, Place, elapsed, refusal};

/// `MAV_COMP_ID_GIMBAL`.
pub const COMPONENT: u8 = 154;

/// `GIMBAL_DEVICE_FLAGS`, which the manager's flags share.
pub const RETRACT: u16 = 1;
pub const NEUTRAL: u16 = 2;
pub const ROLL_LOCK: u16 = 4;
pub const PITCH_LOCK: u16 = 8;
pub const YAW_LOCK: u16 = 16;
pub const YAW_IN_VEHICLE_FRAME: u16 = 32;
pub const YAW_IN_EARTH_FRAME: u16 = 64;

/// How far it turns in roll, pitch and yaw, from and to, in radians. Pitch
/// is from the horizon, down being negative, and yaw is from the nose.
/// Pitch stops straight down, since a quaternion cannot tell a pitch past
/// it from a roll of half a turn.
pub const LIMITS: [(f32, f32); 3] = [
    (-0.5, 0.5),
    (-std::f32::consts::FRAC_PI_2, 0.6),
    (-3.1, 3.1),
];
/// Its top speed on every axis, in radians a second.
pub const TOP_SPEED: f32 = 2.0;

pub(super) const GIMBAL_MANAGER_INFORMATION: u32 = 280;
pub(super) const GIMBAL_MANAGER_STATUS: u32 = 281;
const GIMBAL_DEVICE_INFORMATION: u32 = 283;
const GIMBAL_DEVICE_ATTITUDE_STATUS: u32 = 285;

/// `GIMBAL_DEVICE_ATTITUDE_STATUS` ten times a second of the simulation's
/// clock unless asked otherwise. ArduPilot calls the mount unhealthy
/// below two (S400).
const STATUS_MS: u64 = 100;
/// The fastest it sends a message again and again.
pub(super) const FASTEST_MS: u64 = 20;
/// How near a stop counts as at it, in radians.
const AT_STOP: f32 = 1e-4;

/// What a manager asks of the device, as `GIMBAL_DEVICE_SET_ATTITUDE`
/// carries it, an axis at a time: roll, pitch, yaw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Target {
    /// `GIMBAL_DEVICE_FLAGS`.
    pub flags: u16,
    /// The angle to go to, in radians. None leaves an axis where it is, or
    /// turning at its rate.
    pub angles: [Option<f32>; 3],
    /// In radians a second: how fast to go to the angle, or with no angle
    /// how fast to keep turning.
    pub rates: [Option<f32>; 3],
}

impl Target {
    /// A quaternion and angular velocities as the messages carry them, where
    /// NaN leaves a value unset.
    #[must_use]
    pub fn of(flags: u16, q: [f32; 4], rates: [f32; 3]) -> Self {
        Self {
            flags,
            angles: wire::angles(q).map_or([None; 3], |angles| angles.map(Some)),
            rates: rates.map(set),
        }
    }
}

/// A value that NaN leaves unset.
pub(super) fn set(value: f32) -> Option<f32> {
    (!value.is_nan()).then_some(value)
}

/// A message the gimbal's component sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Report {
    Information,
    AttitudeStatus,
    ManagerInformation,
    ManagerStatus,
}

/// A gimbal device, with or without a manager of its own.
#[derive(Clone, Debug)]
pub struct Gimbal {
    pub(super) system: u8,
    /// `MAV_COMP_ID_GIMBAL` unless set.
    pub component: u8,
    /// The only sender it takes `GIMBAL_DEVICE_SET_ATTITUDE` from.
    manager: (u8, u8),
    /// The manager the payload brings, when no autopilot manages it.
    pub(super) own_manager: Option<Manager>,
    /// It is inside a unit that is another component, such as a camera
    /// with its own gimbal (ADR-0063 Y16): it has no device interface of
    /// its own to be asked about.
    internal: bool,
    /// Its motors have failed, as a fault: it says so in its status and
    /// stays where it is.
    pub failed: bool,
    /// The commands a fault has it refuse or take its time over.
    pub(super) answers: Answers,
    /// Reports its yaw from north and not from the vehicle's nose.
    /// ArduPilot reads it from the nose. MAVSDK's gimbal device tester
    /// reads it from north.
    pub reports_from_north: bool,
    /// The vehicle's yaw from north, once known. Without it the nose is
    /// taken to point north.
    pub vehicle_yaw: Option<f32>,
    /// Where the vehicle is, once known, for a manager asked to look at a
    /// place.
    pub place: Option<Place>,
    /// Roll and pitch from the horizon, and the yaw joint from the nose.
    angles: [f32; 3],
    /// Where the lens points from north, which a locked yaw holds.
    heading: f32,
    /// How fast each of the three and the heading turned in the last step.
    spin: [f32; 3],
    heading_spin: f32,
    /// The axes that are at a stop and were sent past it.
    limited: [bool; 3],
    target: Target,
    status: Option<Every>,
    /// The last MAVLink sequence number it sent.
    pub(super) sent: u8,
    pub(super) now_ms: u64,
}

impl Gimbal {
    /// A gimbal of the vehicle with this system id.
    #[must_use]
    pub fn new(system: u8, managed_by: ManagedBy) -> Self {
        let own = managed_by == ManagedBy::Itself;
        Self {
            system,
            component: COMPONENT,
            manager: (system, if own { COMPONENT } else { AUTOPILOT }),
            own_manager: own.then(Manager::default),
            internal: false,
            failed: false,
            answers: Answers::default(),
            reports_from_north: false,
            vehicle_yaw: None,
            place: None,
            angles: [0.0; 3],
            heading: 0.0,
            spin: [0.0; 3],
            heading_spin: 0.0,
            limited: [false; 3],
            target: Target::default(),
            status: Some(Every::new(STATUS_MS)),
            sent: 0,
            now_ms: 0,
        }
    }

    /// The gimbal inside a unit that is the component `component`, such as
    /// an integrated camera and gimbal (the guide's standalone set-up,
    /// ADR-0063 Y16). The unit manages it and speaks for it, and its
    /// device interface is inside the unit: nothing asks it what device it
    /// is, and it has no heartbeat of its own.
    #[must_use]
    pub fn inside(system: u8, component: u8) -> Self {
        let mut gimbal = Self::new(system, ManagedBy::Itself);
        gimbal.component = component;
        gimbal.manager = (system, component);
        gimbal.internal = true;
        gimbal
    }

    /// The frame to send about once a second.
    pub fn heartbeat(&mut self) -> Vec<u8> {
        self.send(&Heartbeat {
            type_: MavType::Gimbal,
            autopilot: MavAutopilot::Invalid,
            base_mode: MavModeFlag::empty(),
            custom_mode: 0,
            system_status: MavState::Active,
            mavlink_version: 3,
        })
    }

    /// Roll and pitch from the horizon and yaw from north: where a camera
    /// on it looks.
    #[must_use]
    pub fn attitude_from_north(&self) -> [f32; 3] {
        [self.angles[0], self.angles[1], self.heading]
    }

    /// The id it gives itself in its messages: 1 when its manager is the
    /// same component, and 0 when the manager is elsewhere and knows it by
    /// its component id.
    pub(super) fn device_id(&self) -> u8 {
        u8::from(self.own_manager.is_some())
    }

    /// Takes a target, from its manager only. Says whether it did.
    pub fn aim(&mut self, from: (u8, u8), mut target: Target) -> bool {
        if from != self.manager {
            return false;
        }
        let nose = self.vehicle_yaw.unwrap_or(0.0);
        if target.flags & RETRACT != 0 {
            // Tucked away: level, forward and tilted up to its stop.
            target.angles = [Some(0.0), Some(LIMITS[1].1), Some(0.0)];
            target.rates = [None; 3];
            target.flags &= !YAW_LOCK;
        } else if target.flags & NEUTRAL != 0 {
            target.angles = [Some(0.0); 3];
            target.rates = [None; 3];
            target.flags &= !YAW_LOCK;
        } else if let Some(yaw) = &mut target.angles[2] {
            // Kept from north while locked and from the nose while
            // following, whichever of the two the flags gave it in.
            let locked = target.flags & YAW_LOCK != 0;
            let from_north = target.flags & YAW_IN_VEHICLE_FRAME == 0
                && (target.flags & YAW_IN_EARTH_FRAME != 0 || locked);
            if from_north && !locked {
                *yaw = wrap(*yaw - nose);
            } else if !from_north && locked {
                *yaw = wrap(*yaw + nose);
            }
        }
        self.target = target;
        true
    }

    /// A message that is not a command: a target from a manager, or for the
    /// payload's own manager a setpoint from whoever is in control.
    pub fn message(&mut self, from: (u8, u8), message: &Ardupilotmega) {
        let Ardupilotmega::GimbalDeviceSetAttitude(set) = message else {
            self.manage(from, message);
            return;
        };
        if self.named(set.target_system, set.target_component) {
            let rates = [
                set.angular_velocity_x,
                set.angular_velocity_y,
                set.angular_velocity_z,
            ];
            self.aim(from, Target::of(set.flags.bits(), set.q, rates));
        }
    }

    /// Carries out a command that names this component or every component:
    /// its acknowledgement first, then the message it asked for.
    pub fn command(&mut self, command: &Command) -> Vec<Vec<u8>> {
        self.tried(command).unwrap_or_else(|| {
            refusal(command)
                .map(|result| self.acknowledge(command, result))
                .into_iter()
                .collect()
        })
    }

    /// The same, or none for a command the gimbal does not have, so that a
    /// unit with more than a gimbal on this component can answer once.
    pub(super) fn tried(&mut self, command: &Command) -> Option<Vec<Vec<u8>>> {
        match self.faulted(command) {
            Some(answer) => Some(vec![answer]),
            None => self.carried_out(command),
        }
    }

    fn carried_out(&mut self, command: &Command) -> Option<Vec<Vec<u8>>> {
        let (result, report) = self.obey(command)?;
        let mut frames = vec![self.acknowledge(command, result)];
        frames.extend(report.map(|report| self.report(report)));
        Some(frames)
    }

    /// A fault's answer to a command the gimbal has, in place of carrying
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

    /// Moves the gimbal to `now_ms` on the clock it is given, milliseconds
    /// since the start, and returns the status messages that are due.
    pub fn step(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let seconds = elapsed(self.now_ms, now_ms);
        self.now_ms = now_ms;
        if self.failed {
            // Motors that have failed turn nothing.
            (self.spin, self.heading_spin) = ([0.0; 3], 0.0);
        } else {
            self.look_at_place();
            self.turn(seconds);
        }
        let mut frames = Vec::new();
        if self.status.as_mut().is_some_and(|every| every.due(now_ms)) {
            frames.push(self.report(Report::AttitudeStatus));
        }
        if self.manager_status_due(now_ms) {
            frames.push(self.report(Report::ManagerStatus));
        }
        frames.extend(self.slowed(now_ms));
        frames
    }

    pub(super) fn named(&self, system: u8, component: u8) -> bool {
        (system == 0 || system == self.system) && (component == 0 || component == self.component)
    }

    fn obey(&mut self, command: &Command) -> Option<(u8, Option<Report>)> {
        let [p1, p2, ..] = command.params;
        match command.command {
            wire::REQUEST_MESSAGE => Some((ACCEPTED, Some(self.report_of(wire::whole(p1))?))),
            wire::SET_MESSAGE_INTERVAL => {
                let report = self.report_of(wire::whole(p1))?;
                Some((self.send_every(report, p2)?, None))
            }
            _ => self.manage_command(command).map(|result| (result, None)),
        }
    }

    /// The message with this id, of those this component sends.
    fn report_of(&self, message: u32) -> Option<Report> {
        match message {
            // A device inside a unit is the unit's own business.
            GIMBAL_DEVICE_INFORMATION if !self.internal => Some(Report::Information),
            GIMBAL_DEVICE_ATTITUDE_STATUS => Some(Report::AttitudeStatus),
            GIMBAL_MANAGER_INFORMATION if self.own_manager.is_some() => {
                Some(Report::ManagerInformation)
            }
            GIMBAL_MANAGER_STATUS if self.own_manager.is_some() => Some(Report::ManagerStatus),
            _ => None,
        }
    }

    /// `MAV_CMD_SET_MESSAGE_INTERVAL` for one of the two status messages:
    /// an interval in microseconds, 0 for its usual rate and below zero to
    /// stop it. The two information messages are sent when asked for and
    /// not at an interval.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a positive interval, and a huge one saturates"
    )]
    fn send_every(&mut self, report: Report, interval_us: f32) -> Option<u8> {
        let usual_ms = match report {
            Report::AttitudeStatus => STATUS_MS,
            Report::ManagerStatus => super::manager::STATUS_MS,
            Report::Information | Report::ManagerInformation => return None,
        };
        let every = if interval_us > 0.0 {
            let every_ms = ((interval_us / 1000.0) as u64).max(FASTEST_MS);
            Some(Every::after(every_ms, self.now_ms))
        } else if interval_us < 0.0 {
            None
        } else {
            Some(Every::after(usual_ms, self.now_ms))
        };
        if report == Report::AttitudeStatus {
            self.status = every;
        } else if let Some(manager) = &mut self.own_manager {
            manager.status = every;
        }
        Some(ACCEPTED)
    }

    /// Turns each axis towards its target for this many seconds.
    fn turn(&mut self, seconds: f32) {
        let nose = self.vehicle_yaw.unwrap_or(0.0);
        let locked = self.target.flags & YAW_LOCK != 0;
        // Held to north, the yaw joint takes up the vehicle's turn at once.
        let joint = if locked {
            wrap(self.heading - nose)
        } else {
            self.angles[2]
        };
        let before = [self.angles[0], self.angles[1], joint];
        let mut after = before;
        for axis in 0..3 {
            let (low, high) = LIMITS[axis];
            let now = before[axis].clamp(low, high);
            let rate = self.target.rates[axis].filter(|rate| *rate != 0.0);
            // Where it gets to, and which way it is sent beyond that.
            let (to, further) = if let Some(angle) = self.target.angles[axis] {
                let angle = if axis == 2 && locked {
                    wrap(angle - nose)
                } else {
                    angle
                };
                // A rate sent with an angle is how fast to go there.
                let reach = rate.map_or(TOP_SPEED, |rate| rate.abs().min(TOP_SPEED)) * seconds;
                let stop = angle.clamp(low, high);
                (now + (stop - now).clamp(-reach, reach), angle - stop)
            } else {
                let rate = rate.unwrap_or(0.0).clamp(-TOP_SPEED, TOP_SPEED);
                ((now + rate * seconds).clamp(low, high), rate)
            };
            after[axis] = to;
            self.limited[axis] = (further < -AT_STOP && to <= low + AT_STOP)
                || (further > AT_STOP && to >= high - AT_STOP);
        }
        let heading = wrap(nose + after[2]);
        if seconds > 0.0 {
            self.spin = [0, 1, 2].map(|axis| (after[axis] - before[axis]) / seconds);
            self.heading_spin = wrap(heading - self.heading) / seconds;
        }
        self.angles = after;
        self.heading = heading;
    }

    pub(super) fn acknowledge(&mut self, command: &Command, result: u8) -> Vec<u8> {
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

    pub(super) fn send(&mut self, message: &dyn mavio::Message) -> Vec<u8> {
        self.sent = self.sent.wrapping_add(1);
        crate::sim::frame(self.system, self.component, self.sent, message)
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the clock wraps as the field does"
    )]
    fn report(&mut self, report: Report) -> Vec<u8> {
        let time_boot_ms = self.now_ms as u32;
        match report {
            // The dialect's message has no `cap_flags2`, where a device
            // would say it can point at a place. ArduPilot must not be
            // told that (S400), and the device cannot do it anyway.
            Report::Information => self.send(&GimbalDeviceInformation {
                time_boot_ms,
                vendor_name: wire::text("UXDuck"),
                model_name: wire::text("SimGimbal"),
                custom_name: [0; 32],
                firmware_version: wire::version(1, 0, 0),
                hardware_version: wire::version(1, 0, 0),
                uid: 0,
                cap_flags: capabilities(),
                custom_cap_flags: 0,
                roll_min: LIMITS[0].0,
                roll_max: LIMITS[0].1,
                pitch_min: LIMITS[1].0,
                pitch_max: LIMITS[1].1,
                yaw_min: LIMITS[2].0,
                yaw_max: LIMITS[2].1,
                gimbal_device_id: self.device_id(),
            }),
            Report::AttitudeStatus => {
                let [roll, pitch, joint] = self.angles;
                let (yaw, frame, spin) = if self.reports_from_north {
                    (self.heading, YAW_IN_EARTH_FRAME, self.heading_spin)
                } else {
                    (joint, YAW_IN_VEHICLE_FRAME, self.spin[2])
                };
                // Roll and pitch are always held against the horizon.
                let held = self.target.flags & (RETRACT | NEUTRAL | YAW_LOCK);
                // "Limited by" a stop: at it, and sent past it. Looking
                // straight down for a survey is not a failure.
                let mut at_stop = GimbalDeviceErrorFlags::empty();
                for (limited, flag) in self.limited.into_iter().zip([
                    GimbalDeviceErrorFlags::AT_ROLL_LIMIT,
                    GimbalDeviceErrorFlags::AT_PITCH_LIMIT,
                    GimbalDeviceErrorFlags::AT_YAW_LIMIT,
                ]) {
                    if limited {
                        at_stop |= flag;
                    }
                }
                if self.failed {
                    at_stop |= GimbalDeviceErrorFlags::MOTOR_ERROR;
                }
                self.send(&GimbalDeviceAttitudeStatus {
                    target_system: 0,
                    target_component: 0,
                    time_boot_ms,
                    flags: GimbalDeviceFlags::from_bits_retain(
                        held | ROLL_LOCK | PITCH_LOCK | frame,
                    ),
                    q: wire::quaternion([roll, pitch, yaw]),
                    angular_velocity_x: self.spin[0],
                    angular_velocity_y: self.spin[1],
                    angular_velocity_z: spin,
                    failure_flags: at_stop,
                    // The yaw between the two frames is the vehicle's own.
                    delta_yaw: self.vehicle_yaw.unwrap_or(f32::NAN),
                    delta_yaw_velocity: f32::NAN,
                    gimbal_device_id: self.device_id(),
                })
            }
            Report::ManagerInformation => self.manager_information(),
            Report::ManagerStatus => self.manager_status(),
        }
    }
}

/// What the device can do: every axis, held against the horizon, with a
/// yaw that follows or holds north and can be given from north.
pub(super) fn capabilities() -> GimbalDeviceCapFlags {
    GimbalDeviceCapFlags::HAS_RETRACT
        | GimbalDeviceCapFlags::HAS_NEUTRAL
        | GimbalDeviceCapFlags::HAS_ROLL_AXIS
        | GimbalDeviceCapFlags::HAS_ROLL_LOCK
        | GimbalDeviceCapFlags::HAS_PITCH_AXIS
        | GimbalDeviceCapFlags::HAS_PITCH_LOCK
        | GimbalDeviceCapFlags::HAS_YAW_AXIS
        | GimbalDeviceCapFlags::HAS_YAW_FOLLOW
        | GimbalDeviceCapFlags::HAS_YAW_LOCK
        | GimbalDeviceCapFlags::SUPPORTS_YAW_IN_EARTH_FRAME
}

//! A gimbal manager of the payload's own, on the gimbal's component, for a
//! vehicle whose autopilot does not manage it: the guide's integrated
//! camera and gimbal. It says who is in control, takes pitch and yaw by
//! angle and by rate from them, looks at a place, and passes every target
//! to the device the way a manager elsewhere would.

use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::dialects::ardupilotmega::enums::{GimbalManagerCapFlags, GimbalManagerFlags};
use mavio::dialects::ardupilotmega::messages::{GimbalManagerInformation, GimbalManagerStatus};

use super::Every;
use super::gimbal::{
    Gimbal, LIMITS, NEUTRAL, PITCH_LOCK, ROLL_LOCK, TOP_SPEED, Target, YAW_LOCK, capabilities, set,
};
use super::wire::{ACCEPTED, Command, DENIED, TEMPORARILY_REJECTED};

const DO_SET_ROI_LOCATION: u16 = 195;
const DO_SET_ROI_NONE: u16 = 197;
const DO_GIMBAL_MANAGER_PITCHYAW: u16 = 1000;
const DO_GIMBAL_MANAGER_CONFIGURE: u16 = 1001;

/// `GIMBAL_MANAGER_CAP_FLAGS_CAN_POINT_LOCATION_GLOBAL`. The manager says
/// it of itself. The device never does.
const CAN_POINT_LOCATION_GLOBAL: u32 = 131_072;

/// `GIMBAL_MANAGER_STATUS` five times a second, the guide's low rate.
pub(super) const STATUS_MS: u64 = 200;

const METRES_PER_DEGREE: f64 = 111_320.0;

/// A place to look at: degrees, and metres above sea level or above home.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Roi {
    lat: f64,
    lon: f64,
    alt_m: f64,
    above_home: bool,
}

/// Who is in control of the gimbal, and what they last asked for.
#[derive(Clone, Debug)]
pub struct Manager {
    /// The system and component in primary and in secondary control. Zeros
    /// are no one.
    primary: (u8, u8),
    secondary: (u8, u8),
    /// `GIMBAL_MANAGER_FLAGS` of the target in force.
    flags: u16,
    roi: Option<Roi>,
    pub(super) status: Option<Every>,
    /// The autopilot was put in primary control as a fault (ADR-0063 Y16).
    held_as_fault: bool,
    /// The place the unit's own camera had it look at, to keep on what
    /// the camera tracks (ADR-0063 Y5). It is followed while this is
    /// still the place it looks at.
    followed: Option<Roi>,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            primary: (0, 0),
            secondary: (0, 0),
            flags: 0,
            roi: None,
            status: Some(Every::new(STATUS_MS)),
            held_as_fault: false,
            followed: None,
        }
    }
}

impl Gimbal {
    /// A setpoint for the manager. It is taken only from a component that
    /// was put in control, as the guide asks.
    pub(super) fn manage(&mut self, from: (u8, u8), message: &Ardupilotmega) {
        let (to, device, flags, target) = match message {
            Ardupilotmega::GimbalManagerSetAttitude(m) => (
                (m.target_system, m.target_component),
                m.gimbal_device_id,
                m.flags.bits(),
                Target::of(
                    0,
                    m.q,
                    [
                        m.angular_velocity_x,
                        m.angular_velocity_y,
                        m.angular_velocity_z,
                    ],
                ),
            ),
            Ardupilotmega::GimbalManagerSetPitchyaw(m) => (
                (m.target_system, m.target_component),
                m.gimbal_device_id,
                m.flags.bits(),
                pitch_and_yaw([m.pitch, m.yaw], [m.pitch_rate, m.yaw_rate]),
            ),
            // Sticks from -1 to 1: an angle as a share of the way to the
            // stop on that side, and a rate as a share of the top speed.
            Ardupilotmega::GimbalManagerSetManualControl(m) => (
                (m.target_system, m.target_component),
                m.gimbal_device_id,
                m.flags.bits(),
                pitch_and_yaw(
                    [share(m.pitch, LIMITS[1]), share(m.yaw, LIMITS[2])],
                    [m.pitch_rate * TOP_SPEED, m.yaw_rate * TOP_SPEED],
                ),
            ),
            _ => return,
        };
        if self.named(to.0, to.1) && self.manages(device) && self.in_control(from) {
            self.set(flags, target);
        }
    }

    /// Puts a system and component in primary control from outside the
    /// protocol, as an autopilot flying a mission or another ground
    /// station would be: for a fake vehicle whose gimbal is held when
    /// UXDuck finds it (S406). A gimbal with no manager of its own has
    /// no one to hold it.
    pub fn held_by(&mut self, who: (u8, u8)) {
        if let Some(manager) = &mut self.own_manager {
            manager.primary = who;
        }
    }

    /// Puts the vehicle's autopilot in primary control as a fault
    /// (ADR-0063 Y16), as an autopilot that points the gimbal for a
    /// mission is, or ends that: the autopilot gives the gimbal up if it
    /// still has it. Another that takes control has it, and the fault is
    /// over. A gimbal with no manager of its own has no one to hold it.
    pub fn hold_for_autopilot(&mut self, holds: bool) {
        let autopilot = (self.system, super::AUTOPILOT);
        let held = self.held_for_autopilot();
        let Some(manager) = &mut self.own_manager else {
            return;
        };
        if holds && !held {
            manager.primary = autopilot;
        } else if !holds && held {
            manager.primary = (0, 0);
        }
        manager.held_as_fault = holds;
    }

    /// Whether the autopilot is in primary control because a fault put it
    /// there, and no one has taken the gimbal since.
    #[must_use]
    pub fn held_for_autopilot(&self) -> bool {
        self.own_manager.as_ref().is_some_and(|manager| {
            manager.held_as_fault && manager.primary == (self.system, super::AUTOPILOT)
        })
    }

    /// A command for the manager and what it comes to, or none for a
    /// command it does not have, which is all of them when the payload
    /// has no manager.
    pub(super) fn manage_command(&mut self, command: &Command) -> Option<u8> {
        let manager = self.own_manager.as_ref()?;
        let p = command.params;
        match command.command {
            DO_GIMBAL_MANAGER_CONFIGURE => {
                let seats = (
                    seat(manager.primary, [p[0], p[1]], command.from),
                    seat(manager.secondary, [p[2], p[3]], command.from),
                );
                let (Some(primary), Some(secondary), true) =
                    (seats.0, seats.1, self.manages_param(p[6]))
                else {
                    return Some(DENIED);
                };
                let manager = self.own_manager.as_mut()?;
                (manager.primary, manager.secondary) = (primary, secondary);
                Some(ACCEPTED)
            }
            DO_GIMBAL_MANAGER_PITCHYAW => {
                if !self.manages_param(p[6]) || !self.in_control(command.from) {
                    return Some(DENIED);
                }
                let radians = |degrees: f32| degrees.to_radians();
                self.set(
                    flags_of(p[4]),
                    pitch_and_yaw([p[0], p[1]].map(radians), [p[2], p[3]].map(radians)),
                );
                Some(ACCEPTED)
            }
            DO_SET_ROI_LOCATION => {
                if !self.manages_param(p[0]) || !self.in_control(command.from) {
                    return Some(DENIED);
                }
                // It cannot point at a place before it knows its own.
                if self.place.is_none() {
                    return Some(TEMPORARILY_REJECTED);
                }
                let Some(roi) = roi_of(command) else {
                    return Some(DENIED);
                };
                let manager = self.own_manager.as_mut()?;
                manager.roi = Some(roi);
                manager.flags = ROLL_LOCK | PITCH_LOCK | YAW_LOCK;
                self.look_at_place();
                Some(ACCEPTED)
            }
            DO_SET_ROI_NONE => {
                if !self.manages_param(p[0]) || !self.in_control(command.from) {
                    return Some(DENIED);
                }
                // With no sticks of its own to go back to, it looks ahead.
                self.set(NEUTRAL, Target::default());
                Some(ACCEPTED)
            }
            _ => None,
        }
    }

    /// Keeps looking at the place it was given as the vehicle moves.
    pub(super) fn look_at_place(&mut self) {
        let Some(roi) = self.own_manager.as_ref().and_then(|manager| manager.roi) else {
            return;
        };
        let Some(place) = self.place else {
            return;
        };
        let latitude = f64::from(place.lat) / 1e7;
        let north = (roi.lat - latitude) * METRES_PER_DEGREE;
        let east = (roi.lon - f64::from(place.lon) / 1e7)
            * METRES_PER_DEGREE
            * latitude.to_radians().cos();
        let own_mm = if roi.above_home {
            place.relative_alt_mm
        } else {
            place.alt_mm
        };
        let below = f64::from(own_mm) / 1000.0 - roi.alt_m;
        #[allow(clippy::cast_possible_truncation, reason = "angles")]
        let (pitch, yaw) = (
            -below.atan2(north.hypot(east)) as f32,
            east.atan2(north) as f32,
        );
        let target = Target {
            flags: ROLL_LOCK | PITCH_LOCK | YAW_LOCK,
            angles: [Some(0.0), Some(pitch), Some(yaw)],
            rates: [None; 3],
        };
        self.aim((self.system, self.component), target);
    }

    pub(super) fn manager_status_due(&mut self, now_ms: u64) -> bool {
        self.own_manager
            .as_mut()
            .and_then(|manager| manager.status.as_mut())
            .is_some_and(|every| every.due(now_ms))
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the clock wraps as the field does"
    )]
    pub(super) fn manager_information(&mut self) -> Vec<u8> {
        let cap_flags = u32::from(capabilities().bits()) | CAN_POINT_LOCATION_GLOBAL;
        self.send(&GimbalManagerInformation {
            time_boot_ms: self.now_ms as u32,
            cap_flags: GimbalManagerCapFlags::from_bits_retain(cap_flags),
            gimbal_device_id: self.device_id(),
            roll_min: LIMITS[0].0,
            roll_max: LIMITS[0].1,
            pitch_min: LIMITS[1].0,
            pitch_max: LIMITS[1].1,
            yaw_min: LIMITS[2].0,
            yaw_max: LIMITS[2].1,
        })
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the clock wraps as the field does"
    )]
    pub(super) fn manager_status(&mut self) -> Vec<u8> {
        let (primary, secondary, flags) = self
            .own_manager
            .as_ref()
            .map_or(((0, 0), (0, 0), 0), |m| (m.primary, m.secondary, m.flags));
        self.send(&GimbalManagerStatus {
            time_boot_ms: self.now_ms as u32,
            flags: GimbalManagerFlags::from_bits_retain(flags),
            gimbal_device_id: self.device_id(),
            primary_control_sysid: primary.0,
            primary_control_compid: primary.1,
            secondary_control_sysid: secondary.0,
            secondary_control_compid: secondary.1,
        })
    }

    /// Whether a device id in a message is this gimbal's: its own, or 0
    /// for every gimbal the manager has.
    fn manages(&self, device: u8) -> bool {
        device == 0 || device == self.device_id()
    }

    /// The same for a device id in a command's parameter, where NaN was
    /// left unset.
    #[allow(clippy::float_cmp, reason = "small whole numbers sent as floats")]
    fn manages_param(&self, device: f32) -> bool {
        device.is_nan() || device == 0.0 || device == f32::from(self.device_id())
    }

    fn in_control(&self, who: (u8, u8)) -> bool {
        self.own_manager
            .as_ref()
            .is_some_and(|m| who != (0, 0) && (who == m.primary || who == m.secondary))
    }

    /// A target from whoever is in control: it ends looking at a place,
    /// and goes to the device as the manager's.
    fn set(&mut self, flags: u16, target: Target) {
        if let Some(manager) = &mut self.own_manager {
            manager.flags = flags;
            manager.roi = None;
        }
        self.aim((self.system, self.component), Target { flags, ..target });
    }
}

impl Gimbal {
    /// Keeps the gimbal on a place its unit's camera tracks (ADR-0063
    /// Y5): degrees, and metres above sea level. The unit turns its own
    /// gimbal, whoever is in control of it. A target from whoever is in
    /// control, or another place to look at, ends it, and the camera goes
    /// on tracking in the picture it then has.
    pub(super) fn follow(&mut self, lat: f64, lon: f64, alt_m: f64) {
        let Some(manager) = &mut self.own_manager else {
            return;
        };
        let roi = Roi {
            lat,
            lon,
            alt_m,
            above_home: false,
        };
        manager.roi = Some(roi);
        manager.followed = Some(roi);
        manager.flags = ROLL_LOCK | PITCH_LOCK | YAW_LOCK;
        self.look_at_place();
    }

    /// Ends that, when the camera tracks no more. A gimbal still on the
    /// place stays as it points. One sent elsewhere since is left alone.
    pub(super) fn unfollow(&mut self) {
        let Some(manager) = &mut self.own_manager else {
            return;
        };
        if manager
            .followed
            .take()
            .is_some_and(|roi| manager.roi == Some(roi))
        {
            manager.roi = None;
        }
    }

    /// Whether it is kept on what its unit's camera tracks.
    #[must_use]
    pub fn follows(&self) -> bool {
        self.own_manager
            .as_ref()
            .is_some_and(|manager| manager.followed.is_some() && manager.roi == manager.followed)
    }
}

/// Pitch and yaw by angle, by rate or both, with a level roll. NaN leaves
/// a value unset.
fn pitch_and_yaw([pitch, yaw]: [f32; 2], [pitch_rate, yaw_rate]: [f32; 2]) -> Target {
    Target {
        flags: 0,
        angles: [Some(0.0), set(pitch), set(yaw)],
        rates: [None, set(pitch_rate), set(yaw_rate)],
    }
}

/// A stick from -1 to 1 as an angle between an axis's two stops, with 0
/// straight ahead.
fn share(stick: f32, (low, high): (f32, f32)) -> f32 {
    let stick = stick.clamp(-1.0, 1.0);
    if stick < 0.0 {
        -stick * low
    } else {
        stick * high
    }
}

/// `GIMBAL_MANAGER_FLAGS` from a command's parameter.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "flags, and NaN is none"
)]
fn flags_of(param: f32) -> u16 {
    param as u16
}

/// Who sits in a seat after `MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE`, from
/// its system and component parameters: an id, 0 for no one, -1 to leave
/// it, -2 for the sender and -3 for the sender to give it up. None when a
/// parameter is none of these.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    reason = "ids checked to be whole and in range"
)]
fn seat(now: (u8, u8), asked: [f32; 2], sender: (u8, u8)) -> Option<(u8, u8)> {
    let sits = now == sender;
    let one = |asked: f32, now: u8, sender: u8| {
        if asked.is_nan() || asked == -1.0 {
            Some(now)
        } else if asked == -2.0 {
            Some(sender)
        } else if asked == -3.0 {
            // Giving up a seat is for the one sitting in it.
            Some(if sits { 0 } else { now })
        } else if (0.0..=255.0).contains(&asked) && asked.fract() == 0.0 {
            Some(asked as u8)
        } else {
            None
        }
    };
    Some((
        one(asked[0], now.0, sender.0)?,
        one(asked[1], now.1, sender.1)?,
    ))
}

/// The place a `MAV_CMD_DO_SET_ROI_LOCATION` names. A `COMMAND_INT` gives
/// degrees times 1e7 and says what its height is above. A `COMMAND_LONG`
/// gives degrees and a height above sea level.
fn roi_of(command: &Command) -> Option<Roi> {
    let alt_m = f64::from(command.params[6]);
    let roi = match command.place {
        // MAV_FRAME_GLOBAL and its integer twin, then the two above home.
        Some((0 | 5, x, y)) => (f64::from(x) / 1e7, f64::from(y) / 1e7, false),
        Some((3 | 6, x, y)) => (f64::from(x) / 1e7, f64::from(y) / 1e7, true),
        Some(_) => return None,
        None => (
            f64::from(command.params[4]),
            f64::from(command.params[5]),
            false,
        ),
    };
    let on_earth = roi.0.abs() <= 90.0 && roi.1.abs() <= 180.0 && alt_m.is_finite();
    on_earth.then_some(Roi {
        lat: roi.0,
        lon: roi.1,
        alt_m,
        above_home: roi.2,
    })
}

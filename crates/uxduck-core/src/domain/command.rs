//! Commands and their receipts (ADR-0005 command path, ADR-0004
//! acknowledgements). A receipt is journaled when the command is sent and
//! again when it ends: the audit record of who asked what of which vehicle,
//! and what the vehicle answered.

use serde::{Deserialize, Serialize};

use crate::domain::telemetry::AltitudeFrame;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    Arm,
    Disarm,
    /// Arm when disarmed, disarm when armed: one controller button or
    /// switch for both. Never sent as such: [`Command::resolved`] turns it
    /// into the one the vehicle needs, so receipts say which.
    ArmToggle,
    /// An ArduPilot mode by name, such as `LOITER` or `GUIDED`.
    SetMode {
        mode: String,
    },
    /// Return to launch.
    Rtl,
    Land,
    /// Restarts the autopilot (refused by ArduPilot while armed).
    Reboot,
    /// Goes to a place and holds there, at a height in a frame: GUIDED on
    /// ArduPilot, Hold on PX4. Sent as `DO_REPOSITION` in a `COMMAND_INT`,
    /// whose integer position keeps the place to the centimetre, where a
    /// `COMMAND_LONG`'s floats would move it by a metre.
    GoTo {
        latitude_deg: f64,
        longitude_deg: f64,
        /// The height, in `altitude_frame`.
        altitude_m: f64,
        /// What the height is measured from: above home unless said.
        /// The ellipsoid is refused; `DO_REPOSITION` has no such frame.
        #[serde(default)]
        altitude_frame: AltitudeFrame,
        /// `ArduPlane` loiter radius in metres: positive clockwise, negative
        /// counterclockwise. Omitted: keep the autopilot's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loiter_radius_m: Option<f64>,
        /// Submarine heading. Omitted commands retain the existing default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<GoToHeading>,
    },
    /// Moves home, where the vehicle returns to and its heights are
    /// measured from: `DO_SET_HOME` in a `COMMAND_INT`. To a place, or,
    /// with none, to where the vehicle is now.
    SetHome {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        place: Option<PlaceAmsl>,
    },
    /// Sets the EKF origin, the point the vehicle's position estimate is
    /// measured from: `DO_SET_GLOBAL_ORIGIN` in a `COMMAND_INT`. ArduPilot
    /// takes it only while it has none, as when it flies without GPS.
    SetEkfOrigin {
        place: PlaceAmsl,
    },
    /// Any `MAV_CMD` by number, with up to seven parameters: what plugins
    /// use for protocols the core does not name, such as the camera's.
    Long {
        command: u16,
        params: Vec<f32>,
    },
}

/// A place with its height above mean sea level, as home and the EKF
/// origin are given.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceAmsl {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_amsl_m: f64,
}

/// How a submarine faces while going to a location. Bearings are degrees
/// clockwise from true north.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum GoToHeading {
    FaceDestination,
    KeepCurrent,
    Set { degrees: f64 },
}

/// What the daemon and the UI do with a command while the vehicle is not
/// `online` (ADR-0004): the same table in both, so they never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfflineBehaviour {
    /// Sent anyway: things you want to reach a vehicle on a one-way link.
    Allow,
    /// Sent after an explicit confirmation.
    Warn,
    /// Refused (`not-sent`) unless an operator forces it.
    Block,
}

/// How much the operator's own command can change (ADR-0020 A5): `none` is
/// sent at once; `confirm` asks first, and the operator may stop it asking;
/// `dangerous` always asks and says why (A7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    None,
    Confirm,
    Dangerous,
}

/// A command's risk and, when dangerous, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiskOf {
    pub level: Risk,
    pub reason: Option<&'static str>,
}

impl RiskOf {
    const NONE: Self = Self {
        level: Risk::None,
        reason: None,
    };
    const CONFIRM: Self = Self {
        level: Risk::Confirm,
        reason: None,
    };
    const fn dangerous(reason: &'static str) -> Self {
        Self {
            level: Risk::Dangerous,
            reason: Some(reason),
        }
    }
}

/// How high above home an armed vehicle counts as flying.
pub const FLYING_ABOVE_HOME_M: f64 = 2.0;

const FORCED_ARM: &str =
    "Forcing skips the pre-arm checks that stop a vehicle arming when something is wrong.";
// Shown whatever the vehicle is, so "falls" is only for one that flies.
// The in-flight texts below show only while the app thinks it is flying,
// which a rover, boat or submarine never is.
const FORCED_DISARM: &str =
    "Forcing disarms even while it moves: the motors stop at once, and in the air it falls.";
const DISARM_IN_FLIGHT: &str = "Disarming in flight stops the motors, and the vehicle falls.";
const REBOOT_IN_FLIGHT: &str =
    "Rebooting in flight stops the flight controller, and the vehicle falls.";
const TERMINATE: &str = "Flight termination stops the vehicle at once, and it falls.";
const SCRIPTS_IN_FLIGHT: &str =
    "Restarting scripts in flight stops whatever they control until they start again.";
const HOME_IN_FLIGHT: &str = "Moving home in flight changes where it returns to and lands, and what its height is measured from.";
const ORIGIN_IN_FLIGHT: &str =
    "Setting the EKF origin in flight moves every position it reports from then on.";
const RESTART_MISSION_IN_FLIGHT: &str = "Restarting the mission in flight sends it back to the first step, and the steps flown so far are flown again.";
/// `DO_SET_MISSION_CURRENT`: param 1 the item, param 2 1 to reset the
/// mission (its `DO_JUMP` repeats, and a finished mission runs again).
pub const MAV_CMD_DO_SET_MISSION_CURRENT: u16 = 224;
/// `DO_PAUSE_CONTINUE`: param 1 0 to pause the mission where it is, 1 to
/// continue it (S340). `ArduCopter` takes it in AUTO and GUIDED.
pub const MAV_CMD_DO_PAUSE_CONTINUE: u16 = 193;
/// `DO_LAND_START` as a command: fly to the mission's landing sequence, the
/// nearest of its `DO_LAND_START` items, and land along it in AUTO (S345).
pub const MAV_CMD_DO_LAND_START: u16 = 189;
/// ArduPilot's `MAV_CMD_SCRIPTING` (S259, ADR-0027 F16).
pub const MAV_CMD_SCRIPTING: u16 = 42701;
/// Its `SCRIPTING_CMD_STOP_AND_RESTART`.
const SCRIPTING_CMD_STOP_AND_RESTART: f32 = 3.0;
const MAV_CMD_DO_FLIGHTTERMINATION: u16 = 185;
const MAV_CMD_NAV_TAKEOFF: u16 = 22;
/// `param2` of `COMPONENT_ARM_DISARM` that forces it.
const FORCE_MAGIC: f32 = 21196.0;

/// Modes that make a vehicle safer on their own, on any ArduPilot firmware
/// and on PX4.
const SAFE_MODES: [&str; 14] = [
    "RTL",
    "LAND",
    "BRAKE",
    "LOITER",
    "SMART_RTL",
    "QLAND",
    "QRTL",
    "HOLD",
    "AUTO.RTL",
    "AUTO.LAND",
    "AUTO.LOITER",
    "Return",
    "Precision Land",
    "AUTO.PRECLAND",
];

/// How a command may be sent from away, through the cloud (ADR-0003
/// "Command classes", S80): a property of each command, like its offline
/// behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandClass {
    /// Makes the vehicle safer: hold, loiter, return, land.
    Safety,
    /// Changes what it does: modes, going somewhere, missions.
    Mission,
    /// Cameras and gimbals.
    Payload,
    /// Arming, taking off, disarming, rebooting, terminating, and anything
    /// the table does not know.
    Critical,
}

/// `MAV_CMD`s for cameras and gimbals: `DO_DIGICAM_CONTROL`,
/// `DO_MOUNT_CONTROL`, `SET_CAMERA_ZOOM`, `SET_CAMERA_FOCUS`,
/// `DO_GIMBAL_MANAGER_PITCHYAW`, `IMAGE_START_CAPTURE`,
/// `IMAGE_STOP_CAPTURE`, `VIDEO_START_CAPTURE`, `VIDEO_STOP_CAPTURE`.
const PAYLOAD_COMMANDS: [u16; 9] = [203, 205, 531, 532, 1000, 2000, 2001, 2500, 2501];
/// `MAV_CMD`s that send a vehicle somewhere now: `NAV_WAYPOINT`,
/// `DO_CHANGE_ALTITUDE`, `DO_REPOSITION`. They go stale fastest.
const TARGET_COMMANDS: [u16; 3] = [16, 186, 192];
/// `MISSION_START`: param 1 the first item to fly, param 2 the last.
/// ArduPilot refuses either unless 0, so the engine makes the first item
/// current before it (S341).
pub const MAV_CMD_MISSION_START: u16 = 300;
/// `DO_CHANGE_SPEED`: param 1 the kind (0 airspeed, 1 ground speed), param 2 m/s.
pub const MAV_CMD_DO_CHANGE_SPEED: u16 = 178;

/// Why a command was refused before it was sent, for clients and agents to
/// act on without parsing `detail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// From away, and older than its class allows when it reached the
    /// computer (ADR-0003 C10).
    Expired,
    /// It would arm the vehicle while a blocking preflight check does not
    /// pass (ADR-0038 K3); the receipt lists them.
    Preflight,
    /// The vehicle is a replay (ADR-0039 R3): nothing is sent to it.
    Replay,
    /// It would arm a real vehicle before the sender's operator agreed to
    /// the current Terms of Service (ADR-0047).
    Terms,
}

const MAV_CMD_DO_SET_MODE: u16 = 176;
const MAV_CMD_COMPONENT_ARM_DISARM: u16 = 400;

impl Command {
    #[must_use]
    pub fn offline_behaviour(&self) -> OfflineBehaviour {
        match self {
            Self::Disarm | Self::Rtl | Self::Land => OfflineBehaviour::Allow,
            Self::SetMode { mode } if SAFE_MODES.iter().any(|m| m.eq_ignore_ascii_case(mode)) => {
                OfflineBehaviour::Allow
            }
            Self::Long { .. } if self.pauses() || self.lands_via_the_mission() => {
                OfflineBehaviour::Allow
            }
            Self::SetMode { .. }
            | Self::GoTo { .. }
            | Self::SetHome { .. }
            | Self::SetEkfOrigin { .. }
            | Self::Long { .. } => OfflineBehaviour::Warn,
            Self::Arm | Self::ArmToggle | Self::Reboot => OfflineBehaviour::Block,
        }
    }

    /// The risk table (ADR-0020 A5, A7). Safety commands are `none`, so
    /// nothing stands between the operator and landing; `forced` is arming
    /// or disarming past the autopilot's checks.
    #[must_use]
    pub fn risk(&self, flying: bool, forced: bool) -> RiskOf {
        let arm = |forced: bool| {
            if forced {
                RiskOf::dangerous(FORCED_ARM)
            } else {
                RiskOf::CONFIRM
            }
        };
        let disarm = |forced: bool| {
            if forced {
                RiskOf::dangerous(FORCED_DISARM)
            } else if flying {
                RiskOf::dangerous(DISARM_IN_FLIGHT)
            } else {
                RiskOf::NONE
            }
        };
        match self {
            // A flying vehicle is armed, so a toggle would disarm it.
            Self::Disarm => disarm(forced),
            Self::ArmToggle if flying => disarm(forced),
            Self::Arm | Self::ArmToggle => arm(forced),
            Self::Rtl | Self::Land => RiskOf::NONE,
            Self::SetMode { mode } if SAFE_MODES.iter().any(|m| m.eq_ignore_ascii_case(mode)) => {
                RiskOf::NONE
            }
            Self::Reboot if flying => RiskOf::dangerous(REBOOT_IN_FLIGHT),
            Self::SetHome { .. } if flying => RiskOf::dangerous(HOME_IN_FLIGHT),
            Self::SetEkfOrigin { .. } if flying => RiskOf::dangerous(ORIGIN_IN_FLIGHT),
            Self::SetHome { .. }
            | Self::SetEkfOrigin { .. }
            | Self::SetMode { .. }
            | Self::GoTo { .. }
            | Self::Reboot => RiskOf::CONFIRM,
            Self::Long { command, params } => {
                let param = |i: usize| params.get(i).copied().unwrap_or(0.0);
                match *command {
                    MAV_CMD_DO_FLIGHTTERMINATION => RiskOf::dangerous(TERMINATE),
                    MAV_CMD_DO_PAUSE_CONTINUE if param(0) < 0.5 => RiskOf::NONE,
                    MAV_CMD_DO_LAND_START => RiskOf::NONE,
                    MAV_CMD_SCRIPTING if flying => RiskOf::dangerous(SCRIPTS_IN_FLIGHT),
                    MAV_CMD_DO_SET_MISSION_CURRENT if flying && param(1) >= 0.5 => {
                        RiskOf::dangerous(RESTART_MISSION_IN_FLIGHT)
                    }
                    MAV_CMD_COMPONENT_ARM_DISARM if param(0) >= 0.5 => {
                        arm(forced || (param(1) - FORCE_MAGIC).abs() < 0.5)
                    }
                    MAV_CMD_COMPONENT_ARM_DISARM => {
                        disarm(forced || (param(1) - FORCE_MAGIC).abs() < 0.5)
                    }
                    _ => RiskOf::CONFIRM,
                }
            }
        }
    }

    /// The command's class (ADR-0003, S80). Anything the table does not
    /// know is critical.
    #[must_use]
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Rtl | Self::Land => CommandClass::Safety,
            Self::Long { .. } if self.pauses() || self.lands_via_the_mission() => {
                CommandClass::Safety
            }
            Self::SetMode { mode } if SAFE_MODES.iter().any(|m| m.eq_ignore_ascii_case(mode)) => {
                CommandClass::Safety
            }
            Self::SetMode { .. }
            | Self::GoTo { .. }
            | Self::SetHome { .. }
            | Self::SetEkfOrigin { .. } => CommandClass::Mission,
            Self::Arm | Self::Disarm | Self::ArmToggle | Self::Reboot => CommandClass::Critical,
            Self::Long { command, .. } => match *command {
                c if PAYLOAD_COMMANDS.contains(&c) => CommandClass::Payload,
                c if TARGET_COMMANDS.contains(&c) => CommandClass::Mission,
                MAV_CMD_MISSION_START
                | MAV_CMD_DO_SET_MODE
                | MAV_CMD_DO_CHANGE_SPEED
                | MAV_CMD_DO_SET_MISSION_CURRENT
                | MAV_CMD_DO_PAUSE_CONTINUE => CommandClass::Mission,
                _ => CommandClass::Critical,
            },
        }
    }

    /// How old it may be when it reaches the computer from away, in
    /// milliseconds: a target chosen on a picture two seconds old should not
    /// fly (ADR-0003 C10).
    #[must_use]
    pub fn max_age_ms(&self) -> u64 {
        match (self, self.class()) {
            (Self::GoTo { .. }, _) => 2_000,
            (Self::Long { command, .. }, _) if TARGET_COMMANDS.contains(command) => 2_000,
            (_, CommandClass::Mission) => 10_000,
            (_, CommandClass::Payload) => 2_000,
            (_, CommandClass::Safety | CommandClass::Critical) => 5_000,
        }
    }

    /// The command to send to a vehicle that is `armed` or not: a toggle
    /// becomes the one it needs, anything else is itself.
    #[must_use]
    pub fn resolved(self, armed: bool) -> Self {
        match self {
            Self::ArmToggle if armed => Self::Disarm,
            Self::ArmToggle => Self::Arm,
            other => other,
        }
    }

    /// Take-off to `height_m` above home: `NAV_TAKEOFF`, its height in `param7`.
    #[must_use]
    pub fn take_off(height_m: f32) -> Self {
        Self::Long {
            command: MAV_CMD_NAV_TAKEOFF,
            params: vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, height_m],
        }
    }

    /// Restarts ArduPilot's Lua scripts, reading them again from its
    /// scripts folder (S259, ADR-0027 F16).
    #[must_use]
    pub fn restart_scripts() -> Self {
        Self::Long {
            command: MAV_CMD_SCRIPTING,
            params: vec![SCRIPTING_CMD_STOP_AND_RESTART],
        }
    }

    /// Starts the mission again from `first`, its first item (1 on
    /// ArduPilot, whose item 0 is home; 0 on PX4), with its `DO_JUMP`
    /// repeats reset: `DO_SET_MISSION_CURRENT` with param 2 at 1.
    #[must_use]
    pub fn restart_mission(first: u16) -> Self {
        Self::Long {
            command: MAV_CMD_DO_SET_MISSION_CURRENT,
            params: vec![f32::from(first), 1.0],
        }
    }

    /// Makes mission item `seq` the current one: in a mission mode the
    /// vehicle goes on to it now, otherwise it is where the mission goes
    /// on from. `DO_SET_MISSION_CURRENT` with param 2 at 0.
    #[must_use]
    pub fn go_to_mission_item(seq: u16) -> Self {
        Self::Long {
            command: MAV_CMD_DO_SET_MISSION_CURRENT,
            params: vec![f32::from(seq), 0.0],
        }
    }

    /// Flies the mission in its mission mode from item `first`, or with
    /// none from its current item (param 1 at -1, as PX4 reads it):
    /// `MISSION_START`. On the ground it arms the vehicle, which the engine
    /// does first (S341).
    #[must_use]
    pub fn start_mission(first: Option<u16>) -> Self {
        Self::Long {
            command: MAV_CMD_MISSION_START,
            params: vec![first.map_or(-1.0, f32::from), 0.0],
        }
    }

    /// The item a mission's start flies from, when it names one: its
    /// param 1, 0 or more. None for anything but `MISSION_START`.
    #[must_use]
    pub fn mission_start_item(&self) -> Option<u16> {
        match self {
            Self::Long {
                command: MAV_CMD_MISSION_START,
                params,
            } => {
                let first = params.first().copied().unwrap_or(0.0).round();
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "checked to be a mission item number"
                )]
                (first >= 0.0 && first <= f32::from(u16::MAX)).then_some(first as u16)
            }
            _ => None,
        }
    }

    /// Whether it starts the mission (`MISSION_START`).
    #[must_use]
    pub fn starts_mission(&self) -> bool {
        matches!(
            self,
            Self::Long {
                command: MAV_CMD_MISSION_START,
                ..
            }
        )
    }

    /// Lands via the mission: flies to its landing sequence and lands
    /// along it, in AUTO (`DO_LAND_START`). Safe like Land and RTL.
    #[must_use]
    pub fn land_via_the_mission() -> Self {
        Self::Long {
            command: MAV_CMD_DO_LAND_START,
            params: Vec::new(),
        }
    }

    /// Whether it lands via the mission (`DO_LAND_START`).
    #[must_use]
    pub fn lands_via_the_mission(&self) -> bool {
        matches!(
            self,
            Self::Long {
                command: MAV_CMD_DO_LAND_START,
                ..
            }
        )
    }

    /// Pauses the mission where the vehicle is: it holds there, on its
    /// current step (`DO_PAUSE_CONTINUE` 0).
    #[must_use]
    pub fn pause_mission() -> Self {
        Self::Long {
            command: MAV_CMD_DO_PAUSE_CONTINUE,
            params: vec![0.0],
        }
    }

    /// Continues a paused mission from where it paused
    /// (`DO_PAUSE_CONTINUE` 1).
    #[must_use]
    pub fn continue_mission() -> Self {
        Self::Long {
            command: MAV_CMD_DO_PAUSE_CONTINUE,
            params: vec![1.0],
        }
    }

    /// Whether it pauses: `DO_PAUSE_CONTINUE` with param 1 at 0, which
    /// only makes the vehicle hold, so it is safe like LOITER.
    #[must_use]
    pub fn pauses(&self) -> bool {
        matches!(
            self,
            Self::Long { command: MAV_CMD_DO_PAUSE_CONTINUE, params }
                if params.first().copied().unwrap_or(0.0) < 0.5
        )
    }

    /// Whether it arms a vehicle that is on the ground and disarmed
    /// (ADR-0038 K3): arming, forced or not; a take-off and a mission's
    /// start, which some autopilots arm for; and `DO_SET_MODE` with the
    /// armed flag in its base mode.
    #[must_use]
    pub fn arms_on_the_ground(&self) -> bool {
        const ARMED_FLAG: u32 = 128;
        match self {
            Self::Arm => true,
            Self::Long { command, params } => {
                let param = |i: usize| params.get(i).copied().unwrap_or(0.0);
                match *command {
                    MAV_CMD_COMPONENT_ARM_DISARM => param(0) >= 0.5,
                    MAV_CMD_NAV_TAKEOFF | MAV_CMD_MISSION_START => true,
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "a base mode's flags, 0 to 255"
                    )]
                    MAV_CMD_DO_SET_MODE => (param(0).clamp(0.0, 255.0) as u32) & ARMED_FLAG != 0,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Whether this is a take-off (`NAV_TAKEOFF`).
    #[must_use]
    pub fn is_take_off(&self) -> bool {
        matches!(
            self,
            Self::Long {
                command: MAV_CMD_NAV_TAKEOFF,
                ..
            }
        )
    }

    /// For people: "Arm", "Set mode LOITER".
    #[must_use]
    #[allow(clippy::too_many_lines, reason = "one arm per command")]
    pub fn label(&self) -> String {
        match self {
            Self::Arm => "Arm".into(),
            Self::Disarm => "Disarm".into(),
            Self::ArmToggle => "Arm or disarm".into(),
            Self::SetMode { mode } => format!("Set mode {mode}"),
            Self::Rtl => "Return to launch".into(),
            Self::Land => "Land".into(),
            Self::Reboot => "Reboot".into(),
            Self::GoTo { .. } => "Go to".into(),
            Self::SetHome { .. } => "Set home".into(),
            Self::SetEkfOrigin { .. } => "Set the EKF origin".into(),
            Self::Long {
                command: MAV_CMD_NAV_TAKEOFF,
                ..
            } => "Take off".into(),
            Self::Long {
                command: MAV_CMD_DO_FLIGHTTERMINATION,
                ..
            } => "Terminate the flight".into(),
            Self::Long { command: 186, .. } => "Change height".into(),
            Self::Long { .. } if self.pauses() => "Pause the mission".into(),
            Self::Long { .. } if self.lands_via_the_mission() => "Land via the mission".into(),
            Self::Long { .. } if self.starts_mission() => match self.mission_start_item() {
                Some(first) if first > 0 => format!("Fly the mission from item {first}"),
                _ => "Start the mission".into(),
            },
            Self::Long {
                command: MAV_CMD_DO_PAUSE_CONTINUE,
                ..
            } => "Continue the mission".into(),
            Self::Long {
                command: MAV_CMD_DO_SET_MISSION_CURRENT,
                params,
            } if params.get(1).is_some_and(|p| *p >= 0.5) => "Restart the mission".into(),
            Self::Long {
                command: MAV_CMD_DO_SET_MISSION_CURRENT,
                params,
            } => format!(
                "Go to mission item {}",
                params.first().copied().unwrap_or(0.0).round()
            ),
            Self::Long {
                command: MAV_CMD_DO_CHANGE_SPEED,
                params,
            } if params.first().copied().unwrap_or(0.0) < 0.5 => "Change airspeed".into(),
            Self::Long {
                command: MAV_CMD_DO_CHANGE_SPEED,
                ..
            } => "Change speed".into(),
            // The camera protocol, as the Camera tab sends it (S145).
            Self::Long { command: 2000, .. } => "Take a photo".into(),
            Self::Long { command: 2500, .. } => "Start recording".into(),
            Self::Long { command: 2501, .. } => "Stop recording".into(),
            Self::Long {
                command: MAV_CMD_SCRIPTING,
                params,
            } if params
                .first()
                .is_some_and(|p| (*p - SCRIPTING_CMD_STOP_AND_RESTART).abs() < 0.5) =>
            {
                "Restart scripts".into()
            }
            // The Setup tab's compass calibration (S184).
            Self::Long { command: 42424, .. } => "Calibrate the compasses".into(),
            Self::Long { command: 42426, .. } => "Cancel the compass calibration".into(),
            // From a known heading (S185).
            Self::Long { command: 42006, .. } => "Calibrate the compasses from a heading".into(),
            // The accelerometers and level (S186): 241's parameter 5.
            Self::Long {
                command: 241,
                params,
            } if params.get(4).is_some_and(|p| (*p - 2.0).abs() < 0.5) => "Level".into(),
            // The airspeed's zero (S190): `ArduPlane` zeroes its airspeed
            // sensors with the ground pressure (parameter 3).
            Self::Long {
                command: 241,
                params,
            } if params.get(2).is_some_and(|p| (*p - 1.0).abs() < 0.5) => {
                "Zero the airspeed sensor".into()
            }
            // PX4's calibrations (S191): the compasses (parameter 2), the
            // radio (4), the airspeed sensor (6), and all 0 to stop one.
            Self::Long {
                command: 241,
                params,
            } if params.get(1).is_some_and(|p| (*p - 1.0).abs() < 0.5) => {
                "Calibrate the compasses".into()
            }
            Self::Long {
                command: 241,
                params,
            } if params.get(3).is_some_and(|p| (*p - 1.0).abs() < 0.5) => {
                "Start the radio calibration".into()
            }
            Self::Long {
                command: 241,
                params,
            } if params.get(5).is_some_and(|p| (*p - 2.0).abs() < 0.5) => {
                "Calibrate the airspeed sensor".into()
            }
            Self::Long {
                command: 241,
                params,
            } if params.iter().all(|p| p.abs() < 0.5) => "Stop the calibration".into(),
            Self::Long { command: 241, .. } => "Calibrate the accelerometers".into(),
            Self::Long { command: 42429, .. } => "Confirm the vehicle's position".into(),
            // Installing firmware (S189): restart and stay in the bootloader.
            Self::Long {
                command: 246,
                params,
            } if params.first().is_some_and(|p| (*p - 3.0).abs() < 0.5) => {
                "Restart into the bootloader".into()
            }
            // The radio calibration's channels (S187): `RC_CHANNELS` at a
            // rate, or at their usual rate again.
            Self::Long {
                command: 511,
                params,
            } if params.first().is_some_and(|p| (*p - 65.0).abs() < 0.5) => {
                if params.get(1).is_some_and(|p| *p > 0.0) {
                    "Send the radio's channels".into()
                } else {
                    "Send the radio's channels at their usual rate".into()
                }
            }
            // The motor test (S188): one motor at a throttle, or stopped
            // with a throttle and time of 0.
            Self::Long {
                command: 209,
                params,
            } => {
                let at = |i: usize| params.get(i).copied().unwrap_or(0.0);
                if at(2) <= 0.0 && at(3) <= 0.0 {
                    "Stop the motor test".into()
                } else {
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "a motor's place in the test's order, 1 to 12"
                    )]
                    let order = at(0).round().clamp(0.0, 255.0) as u8;
                    format!(
                        "Spin motor {} at {}%",
                        crate::domain::frames::letter(order),
                        at(2).round()
                    )
                }
            }
            // PX4's motor test (S191): motor n (`ACTUATOR_OUTPUT_FUNCTION`
            // n) at a value from 0 to 1, or stopped with a time of 0.
            Self::Long {
                command: 310,
                params,
            } => {
                let at = |i: usize| params.get(i).copied().unwrap_or(0.0);
                if at(1) <= 0.0 {
                    "Stop the motor test".into()
                } else {
                    format!(
                        "Spin motor {} at {}%",
                        at(4).round(),
                        (at(0) * 100.0).round()
                    )
                }
            }
            Self::Long { command, .. } => format!("Command {command}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReceiptStatus {
    /// On its way; waiting for the vehicle's acknowledgement.
    Sent,
    Accepted,
    /// The vehicle said no; `detail` has its reason.
    Rejected,
    /// No acknowledgement after every retry.
    Timeout,
    /// Never sent; `detail` says why.
    NotSent,
    /// The daemon restarted before the vehicle answered: it may or may not
    /// have been carried out.
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub id: String,
    pub vehicle_id: String,
    pub command: Command,
    pub status: ReceiptStatus,
    /// The acknowledgement's result, or why it was not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Set when the daemon refused it for a reason a client can act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RefusalReason>,
    /// The blocking preflight checks that did not pass (ADR-0038): why it
    /// was refused (`reason` preflight), or what it was sent past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preflight: Vec<crate::domain::preflight::CheckRef>,
    /// Sent although blocking checks did not pass (K4): by whom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_override: Option<crate::domain::preflight::PreflightOverride>,
    /// Sent while the vehicle was stale or offline (ADR-0004).
    pub sent_without_recent_heartbeat: bool,
    /// An operator overrode the offline table.
    pub forced: bool,
    /// Transmissions, counting retries.
    pub attempts: u8,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// The person who sent it, as they were named then (ADR-0033): never
    /// looked up again from `operator_id`, so a rename or switch keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_by: Option<crate::events::ActorIdentity>,
    /// Sent from away, through the account's relay (S80): who, and the id
    /// their app gave it. Only on receipts from before remote control was
    /// removed (ADR-0048).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteBy>,
    pub created_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_us: Option<u64>,
    pub version: u64,
}

/// A person who sent a command from away (ADR-0003 C9): their account, as
/// the cloud's signed grant names them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBy {
    pub user_id: String,
    /// Their name or email, for people.
    pub name: String,
    /// The id their app gave the command, so it is carried out once.
    pub command_id: String,
}

impl ReceiptStatus {
    #[must_use]
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarting_scripts_is_named_and_dangerous_only_in_flight() {
        let restart = Command::restart_scripts();
        assert_eq!(restart.label(), "Restart scripts");
        assert_eq!(restart.risk(false, false).level, Risk::Confirm);
        let flying = restart.risk(true, false);
        assert_eq!(flying.level, Risk::Dangerous);
        assert!(
            flying
                .reason
                .unwrap()
                .contains("stops whatever they control")
        );
    }

    #[test]
    fn a_toggle_is_sent_as_what_the_vehicle_needs() {
        assert_eq!(Command::ArmToggle.resolved(false), Command::Arm);
        assert_eq!(Command::ArmToggle.resolved(true), Command::Disarm);
        assert_eq!(Command::Land.resolved(true), Command::Land);
        assert_eq!(Command::ArmToggle.risk(true, false).level, Risk::Dangerous);
    }

    #[test]
    fn the_risk_table() {
        let level =
            |command: &Command, flying: bool, forced: bool| command.risk(flying, forced).level;
        // Safety commands never stand in the way.
        for command in [
            Command::Land,
            Command::Rtl,
            Command::SetMode {
                mode: "LOITER".into(),
            },
            Command::SetMode {
                mode: "qrtl".into(),
            },
        ] {
            assert_eq!(level(&command, true, false), Risk::None, "{command:?}");
        }
        assert_eq!(level(&Command::Disarm, false, false), Risk::None);
        assert_eq!(level(&Command::Arm, false, false), Risk::Confirm);
        assert_eq!(level(&Command::take_off(10.0), false, false), Risk::Confirm);
        assert_eq!(
            level(
                &Command::SetMode {
                    mode: "AUTO".into()
                },
                false,
                false
            ),
            Risk::Confirm
        );
        assert_eq!(level(&Command::Reboot, false, false), Risk::Confirm);
        let go_to = Command::GoTo {
            latitude_deg: -35.36,
            longitude_deg: 149.16,
            altitude_m: 30.0,
            altitude_frame: AltitudeFrame::AboveHome,
            loiter_radius_m: None,
            heading: None,
        };
        assert_eq!(level(&go_to, false, false), Risk::Confirm);
        assert_eq!(level(&go_to, true, false), Risk::Confirm);
        assert_eq!(go_to.class(), CommandClass::Mission);
        assert_eq!(go_to.max_age_ms(), 2_000);
        assert_eq!(go_to.offline_behaviour(), OfflineBehaviour::Warn);
        assert_eq!(go_to.label(), "Go to");
        let height = Command::Long {
            command: 186,
            params: vec![40.0, 3.0],
        };
        assert_eq!(level(&height, true, false), Risk::Confirm);
        assert_eq!(height.class(), CommandClass::Mission);
        assert_eq!(height.max_age_ms(), 2_000);
        assert_eq!(height.offline_behaviour(), OfflineBehaviour::Warn);
        assert_eq!(height.label(), "Change height");
        // The dangerous ones, each with its reason.
        for (command, flying, forced) in [
            (Command::Arm, false, true),
            (Command::Disarm, true, false),
            (Command::Disarm, false, true),
            (Command::Reboot, true, false),
            (
                Command::Long {
                    command: 185,
                    params: vec![1.0],
                },
                false,
                false,
            ),
            (
                Command::Long {
                    command: 400,
                    params: vec![1.0, 21196.0],
                },
                false,
                false,
            ),
            (
                Command::Long {
                    command: 400,
                    params: vec![0.0],
                },
                true,
                false,
            ),
            (
                Command::Long {
                    command: 400,
                    params: vec![0.0, 21196.0],
                },
                false,
                false,
            ),
        ] {
            let risk = command.risk(flying, forced);
            assert_eq!(risk.level, Risk::Dangerous, "{command:?}");
            assert!(risk.reason.is_some(), "{command:?}");
        }
        assert_eq!(Command::take_off(10.0).label(), "Take off");
    }

    #[test]
    fn changing_speed_is_a_mission_command_named_for_its_kind() {
        let speed = |kind: f32| Command::Long {
            command: MAV_CMD_DO_CHANGE_SPEED,
            params: vec![kind, 18.0, -1.0],
        };
        assert_eq!(speed(0.0).risk(true, false).level, Risk::Confirm);
        assert_eq!(speed(0.0).class(), CommandClass::Mission);
        assert_eq!(speed(0.0).offline_behaviour(), OfflineBehaviour::Warn);
        assert_eq!(speed(0.0).label(), "Change airspeed");
        assert_eq!(speed(1.0).label(), "Change speed");
    }

    #[test]
    fn restarting_the_mission_is_held_in_flight_and_going_to_an_item_is_not() {
        let restart = Command::restart_mission(1);
        let item = Command::go_to_mission_item(4);
        assert_eq!(
            restart,
            Command::Long {
                command: 224,
                params: vec![1.0, 1.0]
            }
        );
        assert_eq!(
            item,
            Command::Long {
                command: 224,
                params: vec![4.0, 0.0]
            }
        );
        assert_eq!(restart.label(), "Restart the mission");
        assert_eq!(item.label(), "Go to mission item 4");
        assert_eq!(restart.risk(false, false).level, Risk::Confirm);
        let flying = restart.risk(true, false);
        assert_eq!(flying.level, Risk::Dangerous);
        assert!(flying.reason.is_some());
        assert_eq!(item.risk(true, false).level, Risk::Confirm);
        for command in [&restart, &item] {
            assert_eq!(command.class(), CommandClass::Mission);
            assert_eq!(command.offline_behaviour(), OfflineBehaviour::Warn);
            assert!(!command.arms_on_the_ground());
        }
    }

    #[test]
    fn starting_the_mission_arms_and_may_name_its_first_item() {
        let start = Command::start_mission(None);
        let from = Command::start_mission(Some(3));
        assert_eq!(
            from,
            Command::Long {
                command: 300,
                params: vec![3.0, 0.0]
            }
        );
        assert_eq!(
            start,
            Command::Long {
                command: 300,
                params: vec![-1.0, 0.0]
            }
        );
        assert_eq!(start.label(), "Start the mission");
        assert_eq!(Command::start_mission(Some(0)).label(), "Start the mission");
        assert_eq!(from.label(), "Fly the mission from item 3");
        assert_eq!(start.mission_start_item(), None);
        assert_eq!(from.mission_start_item(), Some(3));
        assert_eq!(Command::restart_mission(1).mission_start_item(), None);
        for command in [&start, &from] {
            assert!(command.starts_mission());
            assert!(command.arms_on_the_ground());
            assert_eq!(command.class(), CommandClass::Mission);
            assert_eq!(command.risk(true, false).level, Risk::Confirm);
        }
    }

    #[test]
    fn landing_via_the_mission_is_safe_like_land() {
        let land = Command::land_via_the_mission();
        assert!(land.lands_via_the_mission());
        assert_eq!(land.label(), "Land via the mission");
        assert_eq!(land.risk(true, false).level, Risk::None);
        assert_eq!(land.class(), CommandClass::Safety);
        assert_eq!(land.offline_behaviour(), OfflineBehaviour::Allow);
        assert!(!land.arms_on_the_ground());
    }

    #[test]
    fn pausing_the_mission_is_safe_and_continuing_it_is_confirmed() {
        let pause = Command::pause_mission();
        let resume = Command::continue_mission();
        assert_eq!(
            pause,
            Command::Long {
                command: 193,
                params: vec![0.0]
            }
        );
        assert_eq!(
            resume,
            Command::Long {
                command: 193,
                params: vec![1.0]
            }
        );
        assert_eq!(pause.label(), "Pause the mission");
        assert_eq!(resume.label(), "Continue the mission");
        assert!(pause.pauses());
        assert!(!resume.pauses());
        assert_eq!(pause.risk(true, false).level, Risk::None);
        assert_eq!(pause.class(), CommandClass::Safety);
        assert_eq!(pause.offline_behaviour(), OfflineBehaviour::Allow);
        assert_eq!(resume.risk(true, false).level, Risk::Confirm);
        assert_eq!(resume.class(), CommandClass::Mission);
        assert_eq!(resume.offline_behaviour(), OfflineBehaviour::Warn);
        assert!(!pause.arms_on_the_ground() && !resume.arms_on_the_ground());
    }

    #[test]
    fn moving_home_or_the_origin_is_held_in_flight() {
        let place = PlaceAmsl {
            latitude_deg: -35.36,
            longitude_deg: 149.16,
            altitude_amsl_m: 584.0,
        };
        let here = Command::SetHome { place: None };
        let there = Command::SetHome { place: Some(place) };
        let origin = Command::SetEkfOrigin { place };
        for command in [&here, &there, &origin] {
            assert_eq!(
                command.risk(false, false).level,
                Risk::Confirm,
                "{command:?}"
            );
            let flying = command.risk(true, false);
            assert_eq!(flying.level, Risk::Dangerous, "{command:?}");
            assert!(flying.reason.is_some(), "{command:?}");
            assert_eq!(command.class(), CommandClass::Mission);
            assert_eq!(command.offline_behaviour(), OfflineBehaviour::Warn);
            assert!(!command.arms_on_the_ground());
        }
        assert_eq!(there.label(), "Set home");
        assert_eq!(origin.label(), "Set the EKF origin");
    }

    #[test]
    fn what_arms_on_the_ground() {
        let long = |command: u16, params: Vec<f32>| Command::Long { command, params };
        for arms in [
            Command::Arm,
            Command::take_off(10.0),
            long(400, vec![1.0]),
            long(400, vec![1.0, 21196.0]),
            long(300, vec![0.0, 0.0]),
            long(176, vec![217.0, 4.0]),
        ] {
            assert!(arms.arms_on_the_ground(), "{arms:?}");
        }
        for not in [
            Command::Disarm,
            Command::Rtl,
            Command::Land,
            Command::Reboot,
            Command::SetMode {
                mode: "AUTO".into(),
            },
            long(400, vec![0.0]),
            long(176, vec![89.0, 4.0]),
            long(2000, vec![]),
        ] {
            assert!(!not.arms_on_the_ground(), "{not:?}");
        }
    }

    #[test]
    fn the_offline_table() {
        assert_eq!(Command::Arm.offline_behaviour(), OfflineBehaviour::Block);
        assert_eq!(Command::Rtl.offline_behaviour(), OfflineBehaviour::Allow);
        assert_eq!(
            Command::SetMode {
                mode: "land".into()
            }
            .offline_behaviour(),
            OfflineBehaviour::Allow
        );
        assert_eq!(
            Command::SetMode {
                mode: "GUIDED".into()
            }
            .offline_behaviour(),
            OfflineBehaviour::Warn
        );
    }
}

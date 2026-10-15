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
    },
    /// Any `MAV_CMD` by number, with up to seven parameters: what plugins
    /// use for protocols the core does not name, such as the camera's.
    Long {
        command: u16,
        params: Vec<f32>,
    },
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
const FORCED_DISARM: &str =
    "Forcing disarms even in the air: the motors stop, and the vehicle falls.";
const DISARM_IN_FLIGHT: &str = "Disarming in flight stops the motors, and the vehicle falls.";
const REBOOT_IN_FLIGHT: &str =
    "Rebooting in flight stops the flight controller, and the vehicle falls.";
const TERMINATE: &str = "Flight termination stops the vehicle at once, and it falls.";
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

impl CommandClass {
    /// Whether someone away needs the vehicle's control lease to send it.
    #[must_use]
    pub const fn needs_lease(self) -> bool {
        matches!(self, Self::Mission | Self::Critical)
    }
}

/// `MAV_CMD`s for cameras and gimbals: `DO_DIGICAM_CONTROL`,
/// `DO_MOUNT_CONTROL`, `SET_CAMERA_ZOOM`, `SET_CAMERA_FOCUS`,
/// `DO_GIMBAL_MANAGER_PITCHYAW`, `IMAGE_START_CAPTURE`,
/// `IMAGE_STOP_CAPTURE`, `VIDEO_START_CAPTURE`, `VIDEO_STOP_CAPTURE`.
const PAYLOAD_COMMANDS: [u16; 9] = [203, 205, 531, 532, 1000, 2000, 2001, 2500, 2501];
/// `MAV_CMD`s that send a vehicle somewhere now: `NAV_WAYPOINT`,
/// `DO_REPOSITION`. They go stale fastest.
const TARGET_COMMANDS: [u16; 2] = [16, 192];
/// `MISSION_START`.
const MAV_CMD_MISSION_START: u16 = 300;

/// Why a command was refused before it was sent, for clients and agents to
/// act on without parsing `detail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// From away, and older than its class allows when it reached the
    /// computer (ADR-0003 C10).
    Expired,
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
            Self::SetMode { .. } | Self::GoTo { .. } | Self::Long { .. } => OfflineBehaviour::Warn,
            Self::Arm | Self::Reboot => OfflineBehaviour::Block,
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
            Self::Arm => arm(forced),
            Self::Disarm => disarm(forced),
            Self::Rtl | Self::Land => RiskOf::NONE,
            Self::SetMode { mode } if SAFE_MODES.iter().any(|m| m.eq_ignore_ascii_case(mode)) => {
                RiskOf::NONE
            }
            Self::Reboot if flying => RiskOf::dangerous(REBOOT_IN_FLIGHT),
            Self::SetMode { .. } | Self::GoTo { .. } | Self::Reboot => RiskOf::CONFIRM,
            Self::Long { command, params } => {
                let param = |i: usize| params.get(i).copied().unwrap_or(0.0);
                match *command {
                    MAV_CMD_DO_FLIGHTTERMINATION => RiskOf::dangerous(TERMINATE),
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
            Self::SetMode { mode } if SAFE_MODES.iter().any(|m| m.eq_ignore_ascii_case(mode)) => {
                CommandClass::Safety
            }
            Self::SetMode { .. } | Self::GoTo { .. } => CommandClass::Mission,
            Self::Arm | Self::Disarm | Self::Reboot => CommandClass::Critical,
            Self::Long { command, .. } => match *command {
                c if PAYLOAD_COMMANDS.contains(&c) => CommandClass::Payload,
                c if TARGET_COMMANDS.contains(&c) => CommandClass::Mission,
                MAV_CMD_MISSION_START | MAV_CMD_DO_SET_MODE => CommandClass::Mission,
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

    /// Take-off to `height_m` above home: `NAV_TAKEOFF`, its height in `param7`.
    #[must_use]
    pub fn take_off(height_m: f32) -> Self {
        Self::Long {
            command: MAV_CMD_NAV_TAKEOFF,
            params: vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, height_m],
        }
    }

    /// For people: "Arm", "Set mode LOITER".
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Arm => "Arm".into(),
            Self::Disarm => "Disarm".into(),
            Self::SetMode { mode } => format!("Set mode {mode}"),
            Self::Rtl => "Return to launch".into(),
            Self::Land => "Land".into(),
            Self::Reboot => "Reboot".into(),
            Self::GoTo { .. } => "Go to".into(),
            Self::Long {
                command: MAV_CMD_NAV_TAKEOFF,
                ..
            } => "Take off".into(),
            Self::Long {
                command: MAV_CMD_DO_FLIGHTTERMINATION,
                ..
            } => "Terminate the flight".into(),
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
    /// Sent while the vehicle was stale or offline (ADR-0004).
    pub sent_without_recent_heartbeat: bool,
    /// An operator overrode the offline table.
    pub forced: bool,
    /// Transmissions, counting retries.
    pub attempts: u8,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// Sent from away, through the account's relay (S80): who, and the id
    /// their app gave it.
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
        };
        assert_eq!(level(&go_to, false, false), Risk::Confirm);
        assert_eq!(level(&go_to, true, false), Risk::Confirm);
        assert_eq!(go_to.class(), CommandClass::Mission);
        assert_eq!(go_to.max_age_ms(), 2_000);
        assert_eq!(go_to.offline_behaviour(), OfflineBehaviour::Warn);
        assert_eq!(go_to.label(), "Go to");
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

//! Vehicle setup (ADR-0037): procedures a person follows with the vehicle in
//! their hands, such as calibrating its compasses. A procedure belongs to
//! UXDuck, not to a window (V2): every window, the CLI and the agent see the
//! same one, closing a window does not stop it, and a vehicle has one at a
//! time. What the person should do, and what went wrong, are codes the
//! interface puts in words, so the app, the CLI and the agent say the same.

use serde::{Deserialize, Serialize};

use crate::domain::history::Author;

/// A row of the Setup tab (V1), one per thing a vehicle is set up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupRow {
    Firmware,
    Accelerometer,
    Compass,
    Radio,
    Airspeed,
    Motors,
}

impl SetupRow {
    /// Every row, in the tab's order.
    pub const ALL: [Self; 6] = [
        Self::Firmware,
        Self::Accelerometer,
        Self::Compass,
        Self::Radio,
        Self::Airspeed,
        Self::Motors,
    ];

    /// Its name on the wire and in ids: "compass".
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Firmware => "firmware",
            Self::Accelerometer => "accelerometer",
            Self::Compass => "compass",
            Self::Radio => "radio",
            Self::Airspeed => "airspeed",
            Self::Motors => "motors",
        }
    }

    /// The procedure UXDuck runs for this row, where it has one: the
    /// compass since S184, the accelerometer since S186, the radio since
    /// S187, the motors since S188, the firmware since S189, the airspeed
    /// since S190 (its zero first; the check and the ratio beside it).
    #[must_use]
    pub const fn procedure(self) -> Option<SetupKind> {
        match self {
            Self::Firmware => Some(SetupKind::Firmware),
            Self::Compass => Some(SetupKind::Compass),
            Self::Accelerometer => Some(SetupKind::Accelerometer),
            Self::Radio => Some(SetupKind::Radio),
            Self::Motors => Some(SetupKind::Motors),
            Self::Airspeed => Some(SetupKind::AirspeedZero),
        }
    }
}

/// A procedure UXDuck runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupKind {
    /// Every compass, by the vehicle's own calibration while it is turned
    /// round every axis (V5).
    Compass,
    /// Every compass at once from the vehicle's known true heading, for a
    /// vehicle too large to turn (V6, S185): one command, answered at once.
    CompassHeading,
    /// The accelerometers, by the vehicle's own calibration (V7, S186): it
    /// asks for six positions in turn, and the person answers each.
    Accelerometer,
    /// Level (V7, S186): the vehicle takes the attitude it rests in now as
    /// level. One command, answered at once.
    Level,
    /// The radio (V8, S187): done by UXDuck from the vehicle's
    /// `RC_CHANNELS`, and written as parameters.
    Radio,
    /// One motor spinning while a person holds its press (V9, S188): its
    /// test renewed while held, stopped when let go.
    Motors,
    /// Firmware installed through the board's bootloader over USB (V10,
    /// S189).
    Firmware,
    /// The airspeed sensor zeroed with its pitot covered (V15, S190): the
    /// vehicle's own calibration, one command answered once it is done.
    AirspeedZero,
    /// The pitot checked by blowing into it (V15, S190): UXDuck watches
    /// the reading rise, and tells the vehicle nothing.
    AirspeedCheck,
    /// The airspeed ratio found in flight (V15, S190): the vehicle's
    /// automatic calibration on while it circles, off once the ratio has
    /// settled, and the ratio kept.
    AirspeedRatio,
}

impl SetupKind {
    /// Its row in the Setup tab.
    #[must_use]
    pub const fn row(self) -> SetupRow {
        match self {
            Self::Compass | Self::CompassHeading => SetupRow::Compass,
            Self::Accelerometer | Self::Level => SetupRow::Accelerometer,
            Self::Radio => SetupRow::Radio,
            Self::Motors => SetupRow::Motors,
            Self::Firmware => SetupRow::Firmware,
            Self::AirspeedZero | Self::AirspeedCheck | Self::AirspeedRatio => SetupRow::Airspeed,
        }
    }

    /// For people: "Compass calibration".
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Compass => "Compass calibration",
            Self::CompassHeading => "Compass calibration from a heading",
            Self::Accelerometer => "Accelerometer calibration",
            Self::Level => "Level",
            Self::Radio => "Radio calibration",
            Self::Motors => "Motor test",
            Self::Firmware => "Firmware install",
            Self::AirspeedZero => "Airspeed zero",
            Self::AirspeedCheck => "Pitot check",
            Self::AirspeedRatio => "Airspeed ratio",
        }
    }
}

/// Where a known heading came from (V6): typed in degrees, or the bearing
/// from the vehicle to a marker or to a point on the map that its nose is
/// pointed at. Only for the history and the interface; the vehicle is sent
/// the degrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeadingFrom {
    Typed,
    Marker,
    Map,
}

/// A compass calibration from a known heading (V6): the vehicle's true
/// heading, and where it is when it has no GPS fix of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownHeading {
    /// Degrees clockwise from true north, 0 to under 360.
    pub degrees: f64,
    pub from: HeadingFrom,
    /// The marker's name, when the heading is the bearing to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// Where the vehicle is, given on the map; absent when the vehicle
    /// used its own GPS position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
}

/// A position the vehicle asks to be held in during its accelerometer
/// calibration (V7): `ACCELCAL_VEHICLE_POS` 1 to 6, in the order `ArduPilot`
/// asks for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VehiclePosition {
    Level,
    /// On its left side: rolled left, left side down.
    Left,
    Right,
    NoseDown,
    NoseUp,
    /// Upside down.
    Back,
}

impl VehiclePosition {
    /// Every position, in the vehicle's order.
    pub const ALL: [Self; 6] = [
        Self::Level,
        Self::Left,
        Self::Right,
        Self::NoseDown,
        Self::NoseUp,
        Self::Back,
    ];

    /// From `ACCELCAL_VEHICLE_POS`; `None` for success, failure or
    /// anything else.
    #[must_use]
    pub const fn from_mavlink(value: u32) -> Option<Self> {
        Some(match value {
            1 => Self::Level,
            2 => Self::Left,
            3 => Self::Right,
            4 => Self::NoseDown,
            5 => Self::NoseUp,
            6 => Self::Back,
            _ => return None,
        })
    }

    /// Its `ACCELCAL_VEHICLE_POS`.
    #[must_use]
    pub const fn mavlink(self) -> u32 {
        match self {
            Self::Level => 1,
            Self::Left => 2,
            Self::Right => 3,
            Self::NoseDown => 4,
            Self::NoseUp => 5,
            Self::Back => 6,
        }
    }

    /// From PX4's word for the side facing down (S191): `down` is level,
    /// `up` upside down, `front` the nose down, `back` the tail down.
    #[must_use]
    pub fn from_px4(side: &str) -> Option<Self> {
        Some(match side {
            "down" => Self::Level,
            "left" => Self::Left,
            "right" => Self::Right,
            "front" => Self::NoseDown,
            "back" => Self::NoseUp,
            "up" => Self::Back,
            _ => return None,
        })
    }

    /// For the history, the CLI and the agent: "on its left side".
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Level => "level",
            Self::Left => "on its left side",
            Self::Right => "on its right side",
            Self::NoseDown => "nose down",
            Self::NoseUp => "nose up",
            Self::Back => "on its back",
        }
    }
}

/// A stick the radio calibration asks for in turn (S187, V8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stick {
    Roll,
    Pitch,
    Throttle,
    Yaw,
}

impl Stick {
    /// Every stick, in the order they are asked for.
    pub const ALL: [Self; 4] = [Self::Roll, Self::Pitch, Self::Throttle, Self::Yaw];

    /// The parameter saying which channel is this stick's: `RCMAP_ROLL`.
    #[must_use]
    pub const fn rcmap(self) -> &'static str {
        match self {
            Self::Roll => "RCMAP_ROLL",
            Self::Pitch => "RCMAP_PITCH",
            Self::Throttle => "RCMAP_THROTTLE",
            Self::Yaw => "RCMAP_YAW",
        }
    }

    /// PX4's parameter for the same (S191): `RC_MAP_ROLL`; 0 there is
    /// no channel.
    #[must_use]
    pub const fn px4_map(self) -> &'static str {
        match self {
            Self::Roll => "RC_MAP_ROLL",
            Self::Pitch => "RC_MAP_PITCH",
            Self::Throttle => "RC_MAP_THROTTLE",
            Self::Yaw => "RC_MAP_YAW",
        }
    }

    /// Its channel when `RCMAP_*` does not say: `ArduPilot`'s defaults.
    #[must_use]
    pub const fn default_channel(self) -> u8 {
        match self {
            Self::Roll => 1,
            Self::Pitch => 2,
            Self::Throttle => 3,
            Self::Yaw => 4,
        }
    }

    /// Moved as asked, it lowers its channel's PWM on a radio that needs
    /// no reversing: pushing pitch forward does, as `ArduPilot` reads
    /// pitch; rolling right, throttle up and yawing right raise it.
    #[must_use]
    pub const fn asked_lowers(self) -> bool {
        matches!(self, Self::Pitch)
    }

    /// What is asked, for the history, the CLI and the agent: "roll right".
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Roll => "roll right",
            Self::Pitch => "pitch forward",
            Self::Throttle => "throttle up",
            Self::Yaw => "yaw right",
        }
    }

    /// Its name: "roll".
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Roll => "roll",
            Self::Pitch => "pitch",
            Self::Throttle => "throttle",
            Self::Yaw => "yaw",
        }
    }
}

/// Where the radio calibration is (S187, V8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RadioStep {
    /// The sticks centred: confirmed, it takes each channel's centre.
    Centre,
    /// Every stick and switch moved to its ends: confirmed, it keeps each
    /// channel's lowest and highest.
    Ends,
    /// Each stick in turn, as asked (`asked`): found by itself.
    Sticks,
    /// The ranges and any fix, to check and write.
    Review,
    /// Written, waiting for the vehicle to take each value.
    Writing,
}

/// One channel as the radio calibration sees it, in microseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioChannel {
    /// 1 to 18.
    pub channel: u8,
    /// The latest reading.
    pub value: u16,
    /// Its lowest and highest since the sticks were centred.
    pub min: u16,
    pub max: u16,
    /// Its reading when the sticks were centred.
    pub centre: u16,
}

impl RadioChannel {
    /// From its lowest to its highest.
    #[must_use]
    pub const fn span(&self) -> u16 {
        self.max.saturating_sub(self.min)
    }
}

/// A stick found on a channel: which, and which way it moved as asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StickFound {
    pub stick: Stick,
    pub channel: u8,
    /// Moved as asked, it lowered the channel's PWM.
    pub lowers: bool,
}

/// A parameter the radio calibration offers to change, beyond the ranges:
/// a stick's channel (`RCMAP_*`) or its reversal (`RCn_REVERSED`), written
/// only when the person accepts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioFix {
    pub name: String,
    pub stick: Stick,
    pub from: f64,
    pub to: f64,
}

/// The motor a motor test spins (S188, V9): which, and how hard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MotorTest {
    /// Its place in the test's order, from 1 (`MAV_CMD_DO_MOTOR_TEST`'s
    /// first parameter).
    pub order: u8,
    /// "A", or a rover's "Throttle".
    pub label: String,
    /// Percent of its range, 1 to 30.
    pub throttle: f32,
}

/// Where a firmware install is (S189, V10), in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareStage {
    /// The vehicle was asked to restart into its bootloader.
    Restarting,
    /// Waiting for its bootloader on USB: restarting, or replugged.
    Waiting,
    /// The release is being fetched from `ArduPilot`'s server.
    Downloading,
    /// From here it cannot be cancelled: the board has no firmware until
    /// it is written.
    Erasing,
    Writing,
    Verifying,
    /// The bootloader was told to start the new firmware.
    Starting,
    /// Waiting for the vehicle to be heard again, and say its version.
    Reconnecting,
}

impl FirmwareStage {
    /// Before erasing: it can still be cancelled, and the board keeps its
    /// firmware.
    #[must_use]
    pub const fn cancellable(self) -> bool {
        matches!(self, Self::Restarting | Self::Waiting | Self::Downloading)
    }

    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Restarting => "restarting into its bootloader",
            Self::Waiting => "waiting for its bootloader",
            Self::Downloading => "downloading",
            Self::Erasing => "erasing",
            Self::Writing => "writing",
            Self::Verifying => "verifying",
            Self::Starting => "restarting",
            Self::Reconnecting => "reconnecting",
        }
    }
}

/// What a firmware install writes (S189): a release from `ArduPilot`'s
/// manifest, chosen by vehicle type and release, or a person's file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum FirmwareSource {
    Release {
        /// "copter", "heli", "plane", "rover", "sub", "blimp", "tracker".
        vehicle_type: String,
        /// "stable", "beta", "dev", or an earlier stable's version.
        release: String,
        /// A variant of the board by its manifest name
        /// ("CubeOrange-bdshot"); absent for the board's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
    },
    File {
        name: String,
    },
}

/// A board as its bootloader describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareBoard {
    pub board_id: u32,
    pub board_rev: u32,
    pub bootloader: u32,
    pub flash_size: u32,
}

/// Why a firmware install did not finish, for the interface to put in
/// words; `Setup.detail` has UXDuck's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareFailure {
    /// The file or release is for another board: both ids are given.
    BoardMismatch,
    /// The image does not fit the board's flash.
    TooLarge,
    /// What was written is not what the file holds.
    Crc,
    /// No bootloader was found on USB in time.
    NoBootloader,
    /// The board is in its STM32 DFU mode, which UXDuck does not install
    /// through.
    Dfu,
    /// A bootloader UXDuck does not speak to.
    Unsupported,
    /// The bootloader stopped answering, or refused.
    Bootloader,
    /// No release of that kind for this board.
    NoRelease,
    /// The release could not be downloaded.
    Download,
    /// The file is not one UXDuck installs.
    BadFile,
    /// The USB port failed.
    Port,
    /// Installed, and the vehicle was not heard again.
    NotReconnected,
}

/// A firmware install (S189, V10): its stage and progress, what it writes,
/// the board, and what changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareInstall {
    pub stage: FirmwareStage,
    /// Bytes downloaded or written in this stage, of `total`.
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    pub source: FirmwareSource,
    /// The version installed before ("4.6.2"), and the vehicle's type
    /// then ("Quadrotor").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_type: Option<String>,
    /// The board, once its bootloader said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<FirmwareBoard>,
    /// The board the file or release is built for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_board_id: Option<u32>,
    /// The manifest's name for the board (`CubeOrange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The release's version ("4.7.2"), and where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The parameters saved before erasing: the file's name in the
    /// vehicle's Files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FirmwareFailure>,
    /// The version and type the vehicle says once it is back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_type: Option<String>,
}

/// How one value written went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteState {
    Writing,
    Written,
    /// The vehicle kept another value, or did not answer.
    Refused,
}

/// One parameter the radio calibration writes, with its value before, so
/// the change can be undone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioWrite {
    pub name: String,
    pub from: f64,
    pub to: f64,
    pub state: WriteState,
}

/// The radio calibration's own state (S187, V8): what it captured and
/// found, what it offers, and what it wrote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Radio {
    pub step: RadioStep,
    /// Every channel the receiver gives, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<RadioChannel>,
    /// The stick asked for now, in `Sticks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<Stick>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sticks: Vec<StickFound>,
    /// From `Review`: a stick on another channel than `RCMAP_*` says, or
    /// moving the other way than `ArduPilot` expects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<RadioFix>,
    /// From `Writing`: every value written, as one change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<RadioWrite>,
}

impl Radio {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            step: RadioStep::Centre,
            channels: Vec::new(),
            asked: None,
            sticks: Vec::new(),
            fixes: Vec::new(),
            writes: Vec::new(),
        }
    }
}

impl Default for Radio {
    fn default() -> Self {
        Self::new()
    }
}

/// The airspeed row's procedures (S190, V15): readings in m/s as the
/// vehicle reports them (`VFR_HUD`), and for the ratio, the parameters by
/// the names its firmware uses, their values, and what was written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AirspeedRun {
    /// The reading when it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_mps: Option<f64>,
    /// The latest reading, as last journaled: after a zero, what it reads
    /// with the pitot covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading_mps: Option<f64>,
    /// The check's highest reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_mps: Option<f64>,
    /// The check passes once the reading has risen this far above where
    /// it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rise_mps: Option<f64>,
    /// The check fails if it has not by then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_us: Option<u64>,
    /// The ratio's parameter (`ARSPD_RATIO`) and the automatic
    /// calibration's (`ARSPD_AUTOCAL`), as the firmware names them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio_param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocal_param: Option<String>,
    /// The ratio before, and the vehicle's latest estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio_before: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// How far the estimate moved over the last minute of circling, as a
    /// percentage of it; settled under 2%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_pct: Option<f64>,
    /// Seconds of estimates gathered, of the minute it is judged over.
    #[serde(default)]
    pub gathered_s: u32,
    /// The ratio settled: autocal is being turned off and the ratio kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,
    /// UXDuck turned the vehicle's automatic calibration on and has not
    /// yet seen it turned off: it is turned off whenever the vehicle can
    /// be told, after a cancel, a landing, a lost link or a restart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub autocal_on: bool,
    /// What the ratio wrote: autocal on and off, and the ratio kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<RadioWrite>,
}

/// The sides of a PX4 calibration (S191): PX4 finds which side is down
/// itself, and says which it still wants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sides {
    /// Every side it wants, in `VehiclePosition`'s order.
    pub needed: Vec<VehiclePosition>,
    /// Those done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<VehiclePosition>,
}

/// Where a procedure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupStep {
    /// Asked of the vehicle; waiting for its answer.
    Starting,
    /// The vehicle is calibrating.
    Running,
    /// Every sensor finished well.
    Succeeded,
    /// The vehicle refused it or did not answer, or a sensor failed.
    Failed,
    /// Someone cancelled it, or the vehicle restarted during it.
    Cancelled,
}

impl SetupStep {
    /// Still going: starting or running.
    #[must_use]
    pub const fn running(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// What the person should do now: a code, put in words by the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Instruction {
    /// Turn the vehicle slowly round every axis until each sensor is done.
    TurnAround,
    /// Restart the vehicle so it uses the new calibration.
    Restart,
    /// Try again, turning it more slowly and steadily.
    TurnSlower,
    /// A compass's orientation looks wrong: check how it is mounted.
    CheckOrientation,
    /// The field measured is wrong: move away from metal and magnets, or
    /// the compass is faulty.
    MoveAway,
    /// Start again.
    TryAgain,
    /// The vehicle has no position: give it on the map, then start again.
    GivePosition,
    /// Hold the vehicle still in the position drawn (`position`), then
    /// say it is done (S186).
    PlaceAsShown,
    /// Keep it still while the vehicle measures the position (S186).
    HoldStill,
    /// Set it on a level surface, or as it should fly level, and try again
    /// (S186).
    SetLevel,
    /// Centre the sticks, throttle down, then say so (S187).
    CentreSticks,
    /// Move every stick and switch to its ends, then say so (S187).
    MoveToEnds,
    /// Move the stick asked for (`radio.asked`) as asked, and hold it
    /// (S187).
    MoveStick,
    /// Check the ranges and any fix offered, then write them (S187).
    CheckRadio,
    /// Unplug the board's USB and plug it back in, so UXDuck catches its
    /// bootloader as it starts (S189).
    Replug,
    /// Keep it plugged in until the install ends (S189).
    KeepPlugged,
    /// Cover the pitot from the wind while the sensor is zeroed (S190).
    CoverPitot,
    /// Blow gently into the pitot (S190).
    BlowIntoPitot,
    /// The reading did not rise: check the pitot's tubes are clear and not
    /// swapped, and the sensor's wiring (S190).
    CheckPitot,
    /// Fly a few circles, in LOITER or a loiter in AUTO, until the ratio
    /// settles (S190).
    FlyCircles,
    /// PX4 (S191): hold it still on a side not done yet; PX4 finds which
    /// side is down itself.
    HoldOnASide,
    /// PX4's compass (S191): keep the side found (`position`) down and turn
    /// the vehicle slowly round.
    TurnOnThisSide,
}

/// Where one sensor of a procedure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SensorState {
    Waiting,
    Running,
    Succeeded,
    Failed,
}

/// Why a compass's calibration failed: `MAG_CAL_REPORT.cal_status` from 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompassFailure {
    /// 5: no good fit, usually turned too fast or not round every axis.
    Failed,
    /// 6: the fit says the compass is mounted another way than set.
    BadOrientation,
    /// 7: the field measured is far from the Earth's.
    BadRadius,
    /// 8: its offsets are too large.
    Offsets,
    /// 9: its scaling is out of range.
    Scaling,
    /// 10: what is left after the fit is too large.
    Residuals,
}

impl CompassFailure {
    /// From `cal_status`; `None` for one that is not a failure.
    #[must_use]
    pub const fn from_status(status: u8) -> Option<Self> {
        Some(match status {
            5 => Self::Failed,
            6 => Self::BadOrientation,
            7 => Self::BadRadius,
            8 => Self::Offsets,
            9 => Self::Scaling,
            10 => Self::Residuals,
            _ => return None,
        })
    }

    /// In a few words, for the history and the agent; the app's own words
    /// are in its `words.ts`.
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Failed => "no good fit",
            Self::BadOrientation => "its orientation looks wrong",
            Self::BadRadius => "the field it measured is wrong",
            Self::Offsets => "its offsets are too large",
            Self::Scaling => "its scaling is out of range",
            Self::Residuals => "too much is left after the fit",
        }
    }

    /// What to do about it.
    #[must_use]
    pub const fn instruction(self) -> Instruction {
        match self {
            Self::Failed | Self::Residuals => Instruction::TurnSlower,
            Self::BadOrientation => Instruction::CheckOrientation,
            Self::BadRadius | Self::Offsets | Self::Scaling => Instruction::MoveAway,
        }
    }
}

/// How good a compass's fit is, from its fitness: the root mean square of
/// what the fit leaves, in milligauss, lower being better. ArduPilot passes
/// a compass at `COMPASS_CAL_FIT` (16 by default) or less.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fitness {
    /// 8 or less: what ArduPilot's strict setting passes.
    Good,
    /// Up to 16: what its default passes.
    Acceptable,
    /// Over 16: passed only with a relaxed setting.
    Poor,
}

impl Fitness {
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Good => "Good",
            Self::Acceptable => "Acceptable",
            Self::Poor => "Poor",
        }
    }

    #[must_use]
    pub fn of(fitness: f32) -> Self {
        if fitness <= 8.0 {
            Self::Good
        } else if fitness <= 16.0 {
            Self::Acceptable
        } else {
            Self::Poor
        }
    }
}

/// One sensor of a procedure: a compass, by its index on the vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensorProgress {
    /// The vehicle's index for it, 0 first (`compass_id`).
    pub index: u8,
    pub state: SensorState,
    /// Percent done, 0 to 100.
    pub progress: u8,
    /// Which of the 80 sections of the sphere round it have been seen:
    /// `completion_mask`, section `i` being bit `i % 8` of byte `i / 8`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<u8>,
    /// Which way to turn the vehicle next, as the vehicle asks: a vector in
    /// its body frame (forward, right, down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<[f32; 3]>,
    /// The vehicle's attempt, from 1, when it retries by itself.
    #[serde(default)]
    pub attempt: u8,
    /// Milligauss; once it succeeded or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitness: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Fitness>,
    /// Its offsets, milligauss, once it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offsets: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CompassFailure>,
    /// The vehicle saved the result itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub autosaved: bool,
}

impl SensorProgress {
    #[must_use]
    pub const fn waiting(index: u8) -> Self {
        Self {
            index,
            state: SensorState::Waiting,
            progress: 0,
            sections: Vec::new(),
            direction: None,
            attempt: 0,
            fitness: None,
            quality: None,
            offsets: None,
            failure: None,
            autosaved: false,
        }
    }

    /// Done, well or not.
    #[must_use]
    pub const fn finished(&self) -> bool {
        matches!(self.state, SensorState::Succeeded | SensorState::Failed)
    }
}

/// Where a vehicle's last procedure of a row is kept: `<vehicle id>/<row>`
/// (S189). Each row keeps its own, so a motor test does not hide the
/// compass's "Needs a restart" or the radio's Undo; one runs at a time.
#[must_use]
pub fn setup_id(vehicle_id: &str, row: SetupRow) -> String {
    format!("{vehicle_id}/{}", row.slug())
}

/// A vehicle's procedure of one row: the one running, or the last one
/// (V2). Keyed by [`setup_id`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    pub vehicle_id: String,
    pub kind: SetupKind,
    pub step: SetupStep,
    /// Percent done over every sensor: the least of theirs.
    pub progress: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensors: Vec<SensorProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<Instruction>,
    /// Why it did not start, in the vehicle's or UXDuck's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// It succeeded, and the vehicle uses the result only once restarted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart_needed: bool,
    /// From a known heading: the heading given, and the position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<KnownHeading>,
    /// The accelerometer's: the position the vehicle asks for now (S186).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<VehiclePosition>,
    /// The person said the vehicle is in `position`, and the vehicle is
    /// measuring it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub placed: bool,
    /// A PX4 calibration's sides (S191); `position` is the side it found
    /// down and is measuring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sides: Option<Box<Sides>>,
    /// The radio's (S187).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio: Option<Radio>,
    /// The motor test's (S188): the motor spinning, or the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motor: Option<MotorTest>,
    /// The firmware install's (S189).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<FirmwareInstall>,
    /// The airspeed's (S190).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub airspeed: Option<Box<AirspeedRun>>,
    /// Who started it; none when the vehicle was found doing it, started
    /// by another ground station.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<Author>,
    /// The receipt of the command that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    pub started_at_us: u64,
    pub updated_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_us: Option<u64>,
    pub version: u64,
}

impl Setup {
    /// Its key: [`setup_id`] of its vehicle and row.
    #[must_use]
    pub fn id(&self) -> String {
        setup_id(&self.vehicle_id, self.kind.row())
    }

    /// The least progress of its sensors, or 0 with none yet.
    #[must_use]
    pub fn overall(&self) -> u8 {
        self.sensors.iter().map(|s| s.progress).min().unwrap_or(0)
    }
}

/// What a vehicle's message asks of the person (V12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fix {
    /// Calibrate it: the row's procedure.
    Calibrate,
    /// Restart the vehicle to use a calibration it has.
    Restart,
}

/// A message a vehicle says, matched to the row that fixes it: the Setup
/// tab's row, and whether calibrating or restarting fixes it. One table per
/// firmware (V12). A pre-arm prefix (`PreArm: `, `Arm: `, `Preflight Fail: `)
/// is ignored.
#[must_use]
pub fn fix_for(autopilot: &str, text: &str) -> Option<(SetupRow, Fix)> {
    let reason = ["PreArm: ", "Arm: ", "Preflight Fail: "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(text)
        .trim()
        .to_lowercase();
    if autopilot.eq_ignore_ascii_case("px4") {
        px4(&reason)
    } else {
        ardupilot(&reason)
    }
}

/// `ArduPilot`'s pre-arm checks (`AP_Arming`, 4.3 to 4.6): "Compass not
/// calibrated", "Compass 2 not calibrated", "Compass calibrated requires
/// reboot", "Compass offsets too high", "3D Accel calibration needed",
/// "Accels calibrated requires reboot", "RC not calibrated".
fn ardupilot(reason: &str) -> Option<(SetupRow, Fix)> {
    let compass = reason.starts_with("compass");
    if compass && reason.ends_with("requires reboot") {
        Some((SetupRow::Compass, Fix::Restart))
    } else if compass && (reason.ends_with("not calibrated") || reason.contains("offsets too high"))
    {
        Some((SetupRow::Compass, Fix::Calibrate))
    } else if reason.starts_with("accels calibrated requires reboot") {
        Some((SetupRow::Accelerometer, Fix::Restart))
    } else if reason.starts_with("3d accel calibration needed") {
        Some((SetupRow::Accelerometer, Fix::Calibrate))
    } else if reason.starts_with("rc not calibrated") {
        Some((SetupRow::Radio, Fix::Calibrate))
    } else {
        None
    }
}

/// PX4's arming checks (1.14 to 1.17): "Compass 0 uncalibrated", "Accel 1
/// uncalibrated", "Gyro 0 uncalibrated" (calibrated with the accelerometer
/// there), "Accel 0 inconsistent - check cal", and the radio's "RC ERROR:
/// `RC1_MIN` < 800"; "No CPU load information" and the like match nothing.
fn px4(reason: &str) -> Option<(SetupRow, Fix)> {
    if reason.starts_with("rc error:") {
        return Some((SetupRow::Radio, Fix::Calibrate));
    }
    if !reason.ends_with("uncalibrated") && !reason.ends_with("inconsistent - check cal") {
        return None;
    }
    if reason.starts_with("compass") || reason.starts_with("mag") {
        Some((SetupRow::Compass, Fix::Calibrate))
    } else if reason.starts_with("accel") || reason.starts_with("gyro") {
        Some((SetupRow::Accelerometer, Fix::Calibrate))
    } else {
        None
    }
}

/// Whether UXDuck offers a message's fix for a vehicle with this autopilot
/// now: the row has a procedure for its firmware, `ArduPilot`'s or, since
/// S191, PX4's.
#[must_use]
pub fn offered(autopilot: &str, row: SetupRow) -> bool {
    set_up(autopilot) && row.procedure().is_some()
}

/// Whether UXDuck sets up vehicles with this autopilot: `ArduPilot`, and
/// PX4 since S191.
#[must_use]
pub fn set_up(autopilot: &str) -> bool {
    autopilot == "Ardupilotmega" || is_px4(autopilot)
}

/// A PX4 vehicle, by its heartbeat's autopilot as decoded (`Px4`).
#[must_use]
pub fn is_px4(autopilot: &str) -> bool {
    autopilot.eq_ignore_ascii_case("px4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ardupilot_messages_lead_to_their_row() {
        let ap = |text| fix_for("Ardupilotmega", text);
        for (text, expected) in [
            (
                "PreArm: Compass not calibrated",
                Some((SetupRow::Compass, Fix::Calibrate)),
            ),
            (
                "Compass not calibrated",
                Some((SetupRow::Compass, Fix::Calibrate)),
            ),
            (
                "PreArm: Compass 2 not calibrated",
                Some((SetupRow::Compass, Fix::Calibrate)),
            ),
            (
                "PreArm: Compass offsets too high",
                Some((SetupRow::Compass, Fix::Calibrate)),
            ),
            (
                "PreArm: Compass calibrated requires reboot",
                Some((SetupRow::Compass, Fix::Restart)),
            ),
            (
                "PreArm: 3D Accel calibration needed",
                Some((SetupRow::Accelerometer, Fix::Calibrate)),
            ),
            (
                "PreArm: Accels calibrated requires reboot",
                Some((SetupRow::Accelerometer, Fix::Restart)),
            ),
            (
                "PreArm: RC not calibrated",
                Some((SetupRow::Radio, Fix::Calibrate)),
            ),
            ("PreArm: Throttle below failsafe", None),
            ("PreArm: GPS 1: not healthy", None),
            ("PreArm: Compass not healthy", None),
            ("Compass: calibration done", None),
        ] {
            assert_eq!(ap(text), expected, "{text}");
        }
    }

    #[test]
    fn px4_messages_have_their_own_table() {
        let px4 = |text| fix_for("Px4", text);
        assert_eq!(
            px4("Preflight Fail: Compass 0 uncalibrated"),
            Some((SetupRow::Compass, Fix::Calibrate))
        );
        assert_eq!(
            px4("Preflight Fail: Accel 1 uncalibrated"),
            Some((SetupRow::Accelerometer, Fix::Calibrate))
        );
        assert_eq!(
            px4("Preflight Fail: Accel 0 inconsistent - check cal"),
            Some((SetupRow::Accelerometer, Fix::Calibrate))
        );
        assert_eq!(
            px4("Preflight Fail: RC ERROR: RC1_MIN < 800"),
            Some((SetupRow::Radio, Fix::Calibrate))
        );
        assert_eq!(px4("Preflight Fail: Strong magnetic interference"), None);
        // ArduPilot's words mean nothing from PX4.
        assert_eq!(px4("PreArm: Compass not calibrated"), None);
        // And PX4's nothing from ArduPilot.
        assert_eq!(
            fix_for("Ardupilotmega", "Preflight Fail: Compass 0 uncalibrated"),
            None
        );
    }

    #[test]
    fn only_rows_with_a_procedure_are_offered() {
        assert!(offered("Ardupilotmega", SetupRow::Compass));
        assert!(offered("Ardupilotmega", SetupRow::Accelerometer));
        assert!(offered("Ardupilotmega", SetupRow::Radio));
        // The airspeed has its procedures since S190.
        assert!(offered("Ardupilotmega", SetupRow::Airspeed));
        // PX4's since S191.
        assert!(offered("Px4", SetupRow::Compass));
        assert!(offered("Px4", SetupRow::Radio));
        assert!(!offered("Generic", SetupRow::Compass));
    }

    #[test]
    fn positions_are_the_vehicles_numbers() {
        for (i, position) in VehiclePosition::ALL.iter().enumerate() {
            let number = u32::try_from(i).unwrap_or_default() + 1;
            assert_eq!(position.mavlink(), number);
            assert_eq!(VehiclePosition::from_mavlink(number), Some(*position));
        }
        // Success and failure are not positions.
        assert_eq!(VehiclePosition::from_mavlink(16_777_215), None);
        assert_eq!(VehiclePosition::from_mavlink(16_777_216), None);
        assert_eq!(VehiclePosition::from_mavlink(0), None);
    }

    #[test]
    fn px4_sides_are_the_positions() {
        assert_eq!(
            VehiclePosition::from_px4("down"),
            Some(VehiclePosition::Level)
        );
        assert_eq!(VehiclePosition::from_px4("up"), Some(VehiclePosition::Back));
        assert_eq!(
            VehiclePosition::from_px4("front"),
            Some(VehiclePosition::NoseDown)
        );
        assert_eq!(
            VehiclePosition::from_px4("back"),
            Some(VehiclePosition::NoseUp)
        );
        assert_eq!(
            VehiclePosition::from_px4("left"),
            Some(VehiclePosition::Left)
        );
        assert_eq!(VehiclePosition::from_px4("error"), None);
    }

    #[test]
    fn fitness_in_words() {
        assert_eq!(Fitness::of(3.2), Fitness::Good);
        assert_eq!(Fitness::of(8.0), Fitness::Good);
        assert_eq!(Fitness::of(12.5), Fitness::Acceptable);
        assert_eq!(Fitness::of(16.0), Fitness::Acceptable);
        assert_eq!(Fitness::of(22.0), Fitness::Poor);
    }

    #[test]
    fn failures_say_what_to_do() {
        assert_eq!(CompassFailure::from_status(4), None);
        assert_eq!(
            CompassFailure::from_status(6).map(CompassFailure::instruction),
            Some(Instruction::CheckOrientation)
        );
        assert_eq!(
            CompassFailure::from_status(5).map(CompassFailure::instruction),
            Some(Instruction::TurnSlower)
        );
        assert_eq!(
            CompassFailure::from_status(7).map(CompassFailure::instruction),
            Some(Instruction::MoveAway)
        );
    }
}

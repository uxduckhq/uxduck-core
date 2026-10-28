//! Vehicle setup on the wire (ADR-0037): the Setup tab's rows, and a
//! vehicle's procedure, such as calibrating its compasses. Instructions and
//! results are codes the interface puts in words.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::history::Author;

/// A row of the Setup tab, one per thing a vehicle is set up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SetupRow {
    Firmware,
    Accelerometer,
    Compass,
    Radio,
    Airspeed,
    Motors,
}

/// A procedure UXDuck runs on a vehicle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SetupKind {
    /// Every compass, by the vehicle's own calibration while it is turned
    /// round every axis.
    Compass,
    /// Every compass at once from the vehicle's known true heading, for a
    /// vehicle too large to turn: one command, answered at once. Needs
    /// `heading` in `setup/start`.
    CompassHeading,
    /// The accelerometers, by the vehicle's own calibration: it asks for
    /// six positions in turn (`position`), and the person answers each with
    /// `setup/confirm`.
    Accelerometer,
    /// Level: the vehicle takes the attitude it rests in now as level. One
    /// command, answered at once.
    Level,
    /// The radio, done by UXDuck from the vehicle's `RC_CHANNELS`: the
    /// sticks centred and confirmed, everything moved to its ends and
    /// confirmed, then each stick as asked (`radio`); the ranges and any
    /// fix accepted are written as parameters with `setup/confirm`.
    Radio,
    /// One motor spinning while a person holds its press: started, kept and
    /// stopped with `setup/spin`, never with `setup/start`.
    Motors,
    /// Firmware installed through the board's bootloader over USB: started
    /// with `setup/firmware`, never with `setup/start`.
    Firmware,
    /// The airspeed sensor zeroed with its pitot covered: the vehicle's own
    /// calibration, on the ground and disarmed. Planes and VTOLs with an
    /// airspeed sensor.
    AirspeedZero,
    /// The pitot checked by blowing into it: UXDuck watches the reading
    /// rise (`airspeed.peakMps`) and passes it, or fails it when it stays
    /// flat until `airspeed.untilUs`. Nothing is sent to the vehicle.
    AirspeedCheck,
    /// The airspeed ratio, in flight: the vehicle's automatic calibration
    /// turned on while it circles, then off once the ratio has settled
    /// (`airspeed.settled`), and the ratio kept as a parameter change.
    AirspeedRatio,
}

/// Which way a propeller turns, seen from above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Spin {
    Clockwise,
    CounterClockwise,
}

/// One motor or output the motor test drives, where it is drawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MotorPlace {
    /// Its place in the test's order, from 1: `setup/spin`'s `motor`.
    pub order: u8,
    /// "A", "B"…; a rover's "Throttle", "Steering", "Left", "Right".
    pub label: String,
    /// The motor's own number, from 1 (an output set to Motor 1); a
    /// rover's test instance.
    pub motor: u8,
    /// Its place, the nose up: right of the middle and ahead of it, the
    /// farthest motor 1 from the middle.
    pub right: f64,
    pub forward: f64,
    /// Absent for a rover's outputs, and frames that yaw without torque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spin: Option<Spin>,
    /// How many motors before it, in the test's order, share its place:
    /// the other of a coaxial pair is 1.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stack: u8,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// A vehicle's motors as the motor test draws them, from its
/// `FRAME_CLASS` and `FRAME_TYPE` (a VTOL's `Q_FRAME_*`), or a rover's
/// outputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MotorLayout {
    /// "Quad X", "Hexa +", "Y6B", "Rover", or "Frame class 7, type 1" for
    /// one UXDuck does not draw.
    pub frame: String,
    /// In the test's order; empty for a frame UXDuck does not draw.
    pub motors: Vec<MotorPlace>,
}

/// The motor a motor test spins: which, and how hard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MotorTest {
    /// Its place in the test's order, from 1.
    pub order: u8,
    /// "A", or a rover's "Throttle".
    pub label: String,
    /// Percent, 1 to 30.
    pub throttle: f32,
}

/// `setup/spin`: spins one motor while a person holds its press.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupSpinParams {
    pub vehicle_id: String,
    /// Its place in the test's order, from 1 (`MotorPlace.order`).
    pub motor: u8,
    /// Percent of its range, 1 to 30; 5 by default.
    #[serde(default = "five")]
    pub throttle: f32,
    /// Still held: true to start or keep it spinning, false to stop it.
    #[serde(default = "held")]
    pub held: bool,
    /// A keepalive: only keeps a motor this connection holds, and never
    /// starts one, so a hold that ended (refused, or let go for a lost
    /// connection) is not started again by the next keepalive.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// The person has confirmed the propellers are off (a switch on the
    /// Motors row, off again each time the row opens). Without it, nothing
    /// spins: the test is refused before anything is sent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub propellers_off: bool,
}

const fn five() -> f32 {
    5.0
}

const fn held() -> bool {
    true
}

/// A stick the radio calibration asks for in turn: roll right, pitch
/// forward, throttle up, yaw right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Stick {
    Roll,
    Pitch,
    Throttle,
    Yaw,
}

/// Where the radio calibration is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RadioStep {
    /// Centre the sticks, throttle down, and confirm: each channel's centre.
    Centre,
    /// Move every stick and switch to its ends, and confirm: each channel's
    /// lowest and highest.
    Ends,
    /// Each stick in turn as `asked`, found as it moves.
    Sticks,
    /// The ranges and any fixes, to confirm with the fixes accepted.
    Review,
    /// Written, waiting for the vehicle to take each value.
    Writing,
}

/// One channel as the radio calibration sees it, in microseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RadioChannel {
    /// 1 to 18.
    pub channel: u8,
    /// The latest reading, as of the last update (`telemetry` has it live
    /// as `rc.<channel>`).
    pub value: u16,
    /// Its lowest and highest since the sticks were centred.
    pub min: u16,
    pub max: u16,
    /// Its reading when the sticks were centred.
    pub centre: u16,
}

/// A stick found on a channel, and which way it moved as asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StickFound {
    pub stick: Stick,
    pub channel: u8,
    /// Moved as asked, it lowered the channel's PWM.
    pub lowers: bool,
}

/// A change offered beyond the ranges: a stick's channel (`RCMAP_*`) or
/// its reversal (`RCn_REVERSED`), written only when accepted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RadioFix {
    pub name: String,
    pub stick: Stick,
    pub from: f64,
    pub to: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WriteState {
    Writing,
    Written,
    /// The vehicle kept another value, or did not answer.
    Refused,
}

/// One parameter written, with its value before: setting each back to
/// `from` undoes the change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RadioWrite {
    pub name: String,
    pub from: f64,
    pub to: f64,
    pub state: WriteState,
}

/// The radio calibration's state: what it captured and found, what it
/// offers, and what it wrote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Radio {
    pub step: RadioStep,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<RadioChannel>,
    /// The stick asked for now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked: Option<Stick>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sticks: Vec<StickFound>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<RadioFix>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<RadioWrite>,
}

/// The airspeed row's procedures: readings in m/s as the vehicle reports
/// them (`telemetry` has the live one as `speed.air_mps`), and for the
/// ratio, its parameters by the names its firmware uses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AirspeedRun {
    /// The reading when it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_mps: Option<f64>,
    /// The latest reading as of the last update: after a zero, what it
    /// read with the pitot covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading_mps: Option<f64>,
    /// The check's highest reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_mps: Option<f64>,
    /// The check passes once the reading rises this far above its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rise_mps: Option<f64>,
    /// The check fails if it has not risen by then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub until_us: Option<u64>,
    /// The ratio's parameter and the automatic calibration's, as the
    /// vehicle's firmware names them (`ARSPD_RATIO`, `ARSPD_AUTOCAL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio_param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocal_param: Option<String>,
    /// The ratio before, and the vehicle's latest estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio_before: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// How far the estimate moved over the last minute, as a percentage of
    /// it: settled under 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_pct: Option<f64>,
    /// Seconds of estimates gathered, of the 60 it is judged over.
    pub gathered_s: u32,
    /// The ratio settled: the automatic calibration is turned off and the
    /// ratio kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,
    /// The vehicle's automatic calibration was turned on by UXDuck and not
    /// yet off: UXDuck turns it off as soon as it can tell the vehicle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub autocal_on: bool,
    /// What it wrote, each with its value before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<RadioWrite>,
}

/// A position the vehicle asks to be held in during its accelerometer
/// calibration, in the order it asks: level, on its left side (left side
/// down), on its right side, nose down, nose up, on its back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehiclePosition {
    Level,
    Left,
    Right,
    NoseDown,
    NoseUp,
    Back,
}

/// The sides of a PX4 calibration: PX4 finds which side is down itself,
/// and says which it still wants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sides {
    /// Every side it wants.
    pub needed: Vec<VehiclePosition>,
    /// Those done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<VehiclePosition>,
}

/// Where a known heading came from: typed in degrees, or the bearing from
/// the vehicle to a marker or to a point on the map its nose points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HeadingFrom {
    Typed,
    Marker,
    Map,
}

/// `setup/start`'s heading, for `compass-heading`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeadingParams {
    /// The vehicle's true heading: degrees clockwise from true north.
    pub degrees: f64,
    #[serde(default = "typed")]
    pub from: HeadingFrom,
    /// The marker it is the bearing to, with `from: marker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_id: Option<String>,
    /// Where the vehicle is, both or neither: needed when it has no 3D GPS
    /// fix; without them it uses its own position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
}

const fn typed() -> HeadingFrom {
    HeadingFrom::Typed
}

/// A calibration from a known heading, as it was asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnownHeading {
    /// Degrees clockwise from true north, 0 to under 360.
    pub degrees: f64,
    pub from: HeadingFrom,
    /// The marker's name, when the heading is the bearing to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// Where the vehicle is, as given on the map; absent when the vehicle
    /// used its own GPS position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
}

/// Where a procedure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SetupStep {
    /// Asked of the vehicle; waiting for its answer.
    Starting,
    /// The vehicle is calibrating.
    Running,
    /// Every sensor finished well.
    Succeeded,
    /// The vehicle refused it or did not answer (`detail` says), or a
    /// sensor failed (its `failure` says).
    Failed,
    /// Someone cancelled it, or the vehicle restarted during it.
    Cancelled,
}

/// What the person should do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// Hold the vehicle still in the position asked for (`position`), then
    /// confirm it with `setup/confirm`.
    PlaceAsShown,
    /// Keep it still while the vehicle measures the position.
    HoldStill,
    /// Set it on a level surface, or as it should fly level, and try again.
    SetLevel,
    /// Centre the sticks, throttle down, then confirm.
    CentreSticks,
    /// Move every stick and switch to its ends, then confirm.
    MoveToEnds,
    /// Move the stick in `radio.asked` as asked, and hold it.
    MoveStick,
    /// Check the ranges and any fix offered, then confirm to write them.
    CheckRadio,
    /// Unplug the board's USB and plug it back in, so UXDuck catches its
    /// bootloader as it starts.
    Replug,
    /// Keep it plugged in until the install ends.
    KeepPlugged,
    /// Cover the pitot from the wind while the sensor is zeroed.
    CoverPitot,
    /// Blow gently into the pitot.
    BlowIntoPitot,
    /// The reading did not rise: check the pitot's tubes are clear and not
    /// swapped, and the sensor's wiring.
    CheckPitot,
    /// Fly a few circles, in LOITER or a loiter in AUTO, until the ratio
    /// settles.
    FlyCircles,
    /// PX4: hold it still on a side not done yet (`sides`); PX4 finds which
    /// side is down itself.
    HoldOnASide,
    /// PX4's compass: keep the side found (`position`) down and turn the
    /// vehicle slowly round.
    TurnOnThisSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SensorState {
    Waiting,
    Running,
    Succeeded,
    Failed,
}

/// Why a compass's calibration failed (`MAG_CAL_REPORT.cal_status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CompassFailure {
    /// No good fit: usually turned too fast or not round every axis.
    Failed,
    /// The fit says it is mounted another way than set.
    BadOrientation,
    /// The field measured is far from the Earth's.
    BadRadius,
    Offsets,
    Scaling,
    Residuals,
}

/// How good a compass's fit is: `good` at a fitness of 8 or less,
/// `acceptable` to 16 (ArduPilot's default limit), `poor` above.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Fitness {
    Good,
    Acceptable,
    Poor,
}

/// Where a firmware install is, in order. Until `erasing` it can be
/// cancelled and the board keeps its firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareStage {
    /// The vehicle was asked to restart into its bootloader.
    Restarting,
    /// Waiting for its bootloader on USB.
    Waiting,
    /// The release is being fetched (`done` of `total` bytes).
    Downloading,
    Erasing,
    /// `done` of `total` bytes written.
    Writing,
    Verifying,
    /// The new firmware is starting.
    Starting,
    /// Waiting for the vehicle to be heard again and say its version.
    Reconnecting,
}

/// What a firmware install writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum FirmwareSource {
    /// From ArduPilot's firmware server, for the board its bootloader
    /// names.
    Release {
        /// `copter`, `heli`, `plane`, `rover`, `sub`, `blimp` or `tracker`.
        vehicle_type: String,
        /// `stable`, `beta`, `dev`, or an earlier stable's version.
        release: String,
        /// A variant of the board by the manifest's name for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
    },
    /// A person's `.apj` or `.px4` file.
    File { name: String },
}

/// A board as its bootloader describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareBoard {
    /// The id firmware files are built for: 140 for a Cube Orange.
    pub board_id: u32,
    pub board_rev: u32,
    /// The bootloader's protocol revision.
    pub bootloader: u32,
    /// Bytes of flash for the firmware.
    pub flash_size: u32,
}

/// Why a firmware install did not finish; `detail` says it in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareFailure {
    /// For another board: `fileBoardId` and `board.boardId` differ.
    BoardMismatch,
    TooLarge,
    Crc,
    NoBootloader,
    /// The board is in its STM32 DFU mode, which UXDuck does not install
    /// through.
    Dfu,
    Unsupported,
    Bootloader,
    NoRelease,
    Download,
    BadFile,
    Port,
    NotReconnected,
}

/// A firmware install: its stage and progress, what it writes, the board,
/// and what changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareInstall {
    pub stage: FirmwareStage,
    /// Bytes downloaded or written in this stage, of `total`.
    #[schemars(schema_with = "crate::safe::integer")]
    pub done: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub total: u64,
    pub source: FirmwareSource,
    /// The version and vehicle type before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<FirmwareBoard>,
    /// The board the file or release is built for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_board_id: Option<u32>,
    /// The firmware server's name for the board (`CubeOrange`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The release's version, and its address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The file the vehicle's parameters were saved to before erasing, in
    /// its Files.
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

/// `setup/firmware`'s file: a `.apj` or `.px4`, its bytes in base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareFileParams {
    pub name: String,
    /// The file's bytes, base64; at most 8 MB before encoding.
    pub contents: String,
}

/// `setup/firmware`: installs firmware on a vehicle whose board is on a USB
/// port of the computer running UXDuck Desktop. Either `release` (with
/// `vehicleType`) or `file`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupFirmwareParams {
    pub vehicle_id: String,
    /// `copter`, `heli`, `plane`, `rover`, `sub`, `blimp` or `tracker`;
    /// with `release`, the vehicle's own type by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_type: Option<String>,
    /// `stable`, `beta`, `dev`, or an earlier stable's version (`4.6.2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// A variant of the board by the firmware server's name for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FirmwareFileParams>,
}

/// `setup/firmwareReleases`: what can be installed on a vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupReleasesParams {
    pub vehicle_id: String,
    /// The vehicle's own type by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_type: Option<String>,
    /// A variant of the board by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Fetch the list again rather than use the one kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

/// Which line of releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseChannel {
    Stable,
    Beta,
    Dev,
    /// An earlier stable release.
    Older,
}

/// One release a board can take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareRelease {
    pub channel: ReleaseChannel,
    /// "4.7.2", "4.8.0-beta1", "4.8.0-dev".
    pub version: String,
    pub url: String,
    pub board_id: u32,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub image_size: Option<u64>,
}

/// A vehicle type `ArduPilot` builds firmware for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareVehicleType {
    /// `copter`.
    pub id: String,
    /// "Copter".
    pub name: String,
}

/// `setup/firmwareReleases`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupReleasesResult {
    pub vehicle_id: String,
    /// The type listed, and every type there is.
    pub vehicle_type: String,
    pub vehicle_types: Vec<FirmwareVehicleType>,
    /// The version the vehicle runs now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    /// The board, as far as UXDuck knows it before its bootloader says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The other variants of the board.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    /// Stable, beta and dev, then earlier stables, newest first.
    pub releases: Vec<FirmwareRelease>,
    /// When the list was fetched from the firmware server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub fetched_at_us: Option<u64>,
    /// Why nothing is listed, or that the list is an older one, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One sensor of a procedure: a compass, by its index on the vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SensorProgress {
    /// The vehicle's index for it, 0 first.
    pub index: u8,
    pub state: SensorState,
    /// Percent done, 0 to 100.
    pub progress: u8,
    /// Which of the 80 sections of the sphere round it have been seen:
    /// 10 bytes, section `i` being bit `i % 8` of byte `i / 8`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<u8>,
    /// Which way to turn the vehicle next, as the vehicle asks: forward,
    /// right, down in its body frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<[f32; 3]>,
    pub attempt: u8,
    /// Milligauss: what the fit leaves, lower being better.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitness: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Fitness>,
    /// Milligauss, once it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offsets: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<CompassFailure>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub autosaved: bool,
}

/// A vehicle's procedure of one row: the one running, or the row's last
/// one. One per vehicle and row, keyed `<vehicleId>/<row>`; one runs at a
/// time. Every window, the CLI and the agent see the same ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    pub vehicle_id: String,
    /// The row it is kept for.
    pub row: SetupRow,
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
    /// The accelerometer's: the position the vehicle asks for now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<VehiclePosition>,
    /// The person confirmed `position`, and the vehicle is measuring it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub placed: bool,
    /// A PX4 calibration's sides; `position` is then the side PX4 found
    /// down and is measuring, with nothing to confirm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sides: Option<Sides>,
    /// The radio calibration's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio: Option<Radio>,
    /// The motor test's: the motor spinning, or the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motor: Option<MotorTest>,
    /// The firmware install's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<FirmwareInstall>,
    /// The airspeed's: zero, check or ratio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub airspeed: Option<AirspeedRun>,
    /// Who started it; absent when the vehicle was found doing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub finished_at_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `setup/start`: starts a procedure on a disarmed vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupStartParams {
    pub vehicle_id: String,
    pub kind: SetupKind,
    /// For `compass-heading` only, and needed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<HeadingParams>,
}

/// `setup/confirm`: the person did what the running procedure asks, such
/// as holding the vehicle in the position asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupConfirmParams {
    pub vehicle_id: String,
    /// For `accelerometer`, and needed there: the position the vehicle is
    /// in. Refused when the vehicle asks for another, as when another
    /// window answered first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<VehiclePosition>,
    /// For `radio`, and needed there: the step the person finished
    /// (`centre`, `ends` or `review`). Refused when the calibration is at
    /// another, as when another window confirmed first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio_step: Option<RadioStep>,
    /// For `radio` at `review`: the fixes accepted, by parameter name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept: Vec<String>,
}

/// `setup/cancel` and `setup/state`: one vehicle's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupParams {
    pub vehicle_id: String,
}

/// How a row stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RowState {
    /// Its procedure is running.
    Running,
    /// The vehicle says it needs calibrating.
    NotCalibrated,
    /// Calibrated, and the vehicle uses it once restarted.
    NeedsRestart,
    /// Calibrated: `calibratedAtUs` says when, if UXDuck knows.
    Calibrated,
    /// Firmware: installed, its version in `detail`.
    Installed,
    /// Nothing says either way, such as before its parameters are read.
    Unknown,
}

/// One row of the Setup tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupRowState {
    pub row: SetupRow,
    pub state: RowState,
    /// When it was last calibrated in UXDuck, from the vehicle's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub calibrated_at_us: Option<u64>,
    /// The vehicle's own words for its state ("Compass not calibrated"),
    /// or the firmware's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The procedure UXDuck runs for it on this vehicle; absent when
    /// UXDuck has none for it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<SetupKind>,
}

/// One compass of a vehicle, from its `COMPASS_DEV_ID*`, `COMPASS_USE*`,
/// `COMPASS_EXTERN*` and `COMPASS_OFS*` parameters. Which are used is
/// changed in Parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompassInfo {
    /// The vehicle's index for it, 0 first, as a calibration reports it.
    pub index: u8,
    /// Its `COMPASS_DEV_ID`: the bus, address and sensor type.
    pub device_id: u32,
    /// Used for the heading.
    pub used: bool,
    /// Outside the flight controller, such as in a GPS module; absent when
    /// the vehicle does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    /// Its offsets, milligauss; all zero until it is calibrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offsets: Option<[f64; 3]>,
}

/// `setup/state`: a vehicle's rows, compasses and procedure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupStateResult {
    pub vehicle_id: String,
    /// The rows for what this vehicle has, in the tab's order.
    pub rows: Vec<SetupRowState>,
    pub compasses: Vec<CompassInfo>,
    /// Its motors as the motor test draws them; absent when it has none
    /// UXDuck tests, or before its parameters are read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motors: Option<MotorLayout>,
    /// The procedure running, or the one changed last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<Setup>,
    /// The last procedure of each row, in the tab's order: a motor test
    /// does not hide the compass's restart or the radio's undo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub procedures: Vec<Setup>,
}

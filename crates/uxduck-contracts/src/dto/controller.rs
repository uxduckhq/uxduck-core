//! Controllers and input mappings on the wire (ADR-0005).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::command::Command;
use crate::dto::gripper::GripperAction;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    /// One vehicle; the client's selection never moves it.
    Pinned,
    /// The claiming client's selected vehicle, switched automatically with
    /// the sticks centred.
    FollowsSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub client_id: String,
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
}

/// An input device the daemon owns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Controller {
    pub id: String,
    pub name: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_target: Option<TargetBinding>,
    /// The vehicle it drives while claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Where the selection moved; the switch waits for neutral sticks or
    /// another target admission check. `controller/setTarget` retries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_target: Option<String>,
    pub mapping_id: String,
    /// Why input is paused; the controller remains claimed and assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_pause: Option<String>,
    /// A button or position held for its command: the command goes when
    /// the hold is complete, and nothing goes if it is let go before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holding: Option<ControllerHold>,
    /// The last photo a camera said it took after this controller's Take
    /// photo (S416), while UXDuck has run. Absent until there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo: Option<ControllerPhoto>,
    /// Its charge, as the platform last said (S419). Unknown where the
    /// platform says nothing, unavailable where it has no way to.
    #[serde(default)]
    pub power: ControllerPower,
    /// How low its battery is, at the levels the daemon warns of: none
    /// while it is above them, charging or wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_level: Option<PowerLevel>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// A controller's power, as the platform reports it (S419).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum ControllerPower {
    /// The platform said nothing of its power.
    #[default]
    Unknown,
    /// The platform has no way to say, as in the browser.
    Unavailable,
    /// On a cable, with no battery to tell of.
    Wired,
    /// On its battery, with `percent` left.
    Discharging { percent: u8 },
    /// Charging, with `percent` so far.
    Charging { percent: u8 },
    /// Full, on its charger.
    Charged,
}

/// How low a controller's battery is (S419): `UPower`'s default levels,
/// 20 % and 5 % left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PowerLevel {
    Low,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetBinding {
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// A hold under way on a controller, for the app to show it filling from
/// when it hears of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerHold {
    pub command: Command,
    /// How long the hold takes in all.
    pub hold_ms: u32,
}

/// A photo a camera said it took after a controller's Take photo (S416),
/// for the Cockpit's row to say where the pilot is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerPhoto {
    pub vehicle_id: String,
    /// The camera's `id` from `camera/list`.
    pub camera_id: String,
    /// Its place in the camera's list, from 0, as `CameraPhoto::index`.
    pub index: u32,
    /// False when the camera said the capture failed.
    pub ok: bool,
    /// When UXDuck heard of it: microseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub heard_at_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GamepadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftZ,
    RightZ,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GamepadButton {
    South,
    East,
    North,
    West,
    /// The upper shoulder buttons (L1 and R1, LB and RB).
    LeftTrigger,
    RightTrigger,
    /// The lower shoulder triggers (L2 and R2, LT and RT), as buttons.
    #[serde(rename = "left-trigger-2")]
    LeftTrigger2,
    #[serde(rename = "right-trigger-2")]
    RightTrigger2,
    Select,
    Start,
    /// The centre button (Home, PS, Xbox).
    Mode,
    /// Pressing a stick in.
    LeftThumb,
    RightThumb,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AxisBinding {
    pub axis: GamepadAxis,
    /// The platform's own code for the input (`AxisValue::code`), for a
    /// device whose layout the platform does not know, such as a radio in
    /// joystick mode. When set it is read instead of `axis`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    pub inverted: bool,
    /// The axis's measured raw readings at its ends and at rest; a reading
    /// is scaled so they read -1, 1 and 0. Absent: read as it comes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<AxisCalibration>,
}

/// An axis's raw readings, -1 to 1: `min` < `centre` < `max`, at least 0.05
/// apart. A throttle's centre is the middle of its travel.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AxisCalibration {
    pub min: f64,
    pub centre: f64,
    pub max: f64,
}

/// An additional ArduPilot RC input, driven by an axis or a radio's switch.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RcAxisBinding {
    /// Explicit input channel, 5 through 16; never a servo output number.
    pub channel: u8,
    pub input: AxisBinding,
}

/// A gimbal's pitch or yaw, as an input turns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalAxis {
    Pitch,
    Yaw,
}

/// An axis that turns the target vehicle's gimbal: the further it is
/// pushed the faster the gimbal turns, up and right positive, and at its
/// centre the gimbal stops.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalAxisBinding {
    pub control: GimbalAxis,
    pub input: AxisBinding,
}

/// What an axis does to the target vehicle's camera (S416).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CameraAxis {
    Zoom,
}

/// An axis that zooms the target vehicle's camera (S416): pushed up or
/// squeezed it zooms in, the further the faster, down it zooms out, and at
/// its centre the zoom stops.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraAxisBinding {
    pub control: CameraAxis,
    pub input: AxisBinding,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ButtonBinding {
    pub button: GamepadButton,
    /// The platform's own code for the button, for a device whose layout
    /// the platform does not know. When set it is matched instead of
    /// `button`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    pub command: Command,
    /// How long it must be held before the command goes, 100 to 10000 ms;
    /// absent: on the press itself. With a hold it is the input's long
    /// press (S418).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
    /// A press, or two quick presses (S418). Absent: a press. One input
    /// may have a press, a long press and a double press on each layer.
    #[serde(default, skip_serializing_if = "ButtonGesture::is_press")]
    pub gesture: ButtonGesture,
    /// The Shift button held for it (S418): it goes only while that Shift
    /// is held, and a binding without one only while no Shift is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<ShiftButton>,
    /// Sent again every this many milliseconds while the button stays
    /// down, at least 40 (S418): only for a step, such as a zoom step, on
    /// a press that is its input's only command on its layer. Absent: once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_ms: Option<u32>,
}

/// How an input is pressed for its binding (S418). A long press is a
/// press with `holdMs`. A press on an input that also has a long or a
/// double press goes when it is let go, and with a double press only once
/// no second press came within 300 ms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ButtonGesture {
    #[default]
    Press,
    DoublePress,
}

impl ButtonGesture {
    #[must_use]
    pub fn is_press(&self) -> bool {
        *self == Self::Press
    }
}

/// A button bound as a Shift (`HeldAction::Shift`), as a binding on its
/// layer names it (S418).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShiftButton {
    pub button: GamepadButton,
    /// As for `ButtonBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
}

/// A button held for an action (S263, S21): it never sends a command. The
/// gripper acts while held, only if pressed after the controller's current
/// route began. A button is bound either to a command or to an action, not
/// both.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionBinding {
    pub button: GamepadButton,
    /// As for `ButtonBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    pub action: HeldAction,
    /// The Shift button held for it (S418), as for `ButtonBinding::shift`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<ShiftButton>,
}

/// What a held button does. `open` and `close` hold the gripper.
/// `gimbal-up`, `gimbal-down`, `gimbal-left` and `gimbal-right` turn the
/// target vehicle's gimbal while held. `zoom-in`, `zoom-out`,
/// `focus-near` and `focus-far` zoom or focus its camera while held, and
/// stop when let go (S416). `talk` is held to speak to the
/// agent in the window that claimed the controller, and let go to send
/// (S21), which that window reads from `controller/inputs`. The app's
/// actions (S420) are read the same way and never reach a vehicle:
/// `next-vehicle` and `previous-vehicle` move the window's selection,
/// `cockpit-tab`, `camera-tab` and `map-tab` show that tab,
/// `next-camera` shows the vehicle's next camera, `camera-full-window`
/// fills the window with the Camera tab or puts it back, `show-on-map`
/// centres the map on the vehicle, and `navigate` starts or ends moving
/// focus with the direction pad (`controller/navigate`).
/// `shift` sends nothing: while it is held, the other buttons do what
/// their bindings with this Shift say (S418). A Shift has no other binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HeldAction {
    Open,
    Close,
    Talk,
    GimbalUp,
    GimbalDown,
    GimbalLeft,
    GimbalRight,
    ZoomIn,
    ZoomOut,
    FocusNear,
    FocusFar,
    NextVehicle,
    PreviousVehicle,
    CockpitTab,
    CameraTab,
    MapTab,
    NextCamera,
    CameraFullWindow,
    ShowOnMap,
    Navigate,
    Shift,
}

/// A position of an axis, such as a switch's, that sends a command when the
/// axis moves into it (within 0.1 of `at`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PositionBinding {
    pub axis: GamepadAxis,
    /// As for `AxisBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    /// The axis's raw value at the position, -1 to 1.
    pub at: f64,
    pub command: Command,
    /// As for `ButtonBinding::hold_ms`: how long the axis must stay in the
    /// position first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
}

/// Which part of an axis provides the throttle's 0 to 100% input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ThrottleRange {
    #[default]
    FullTravel,
    CentreToTop,
}

impl ThrottleRange {
    #[must_use]
    pub fn is_full_travel(&self) -> bool {
        *self == Self::FullTravel
    }
}

/// Which stick carries which control, as pilots number the layouts:
/// Mode 2 has throttle and yaw on the left, Mode 1 throttle and roll on
/// the right; Modes 3 and 4 swap Mode 1's and Mode 2's sticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum StickMode {
    #[serde(rename = "mode-1")]
    Mode1,
    #[serde(rename = "mode-2")]
    Mode2,
    #[serde(rename = "mode-3")]
    Mode3,
    #[serde(rename = "mode-4")]
    Mode4,
}

/// A stick control a mapping may leave unused. Throttle is not one: centred
/// it is 50%, so it is always read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StickControl {
    Roll,
    Pitch,
    Yaw,
}

/// An axis whose feel a mapping sets (S418): the four sticks and the
/// gimbal's two axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FeelAxis {
    Roll,
    Pitch,
    Yaw,
    Throttle,
    GimbalPitch,
    GimbalYaw,
}

/// How an axis feels after its deadzone and inversion (S418): the value
/// sent is `((1 - expo) * x + expo * x³) * rate`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AxisFeel {
    pub axis: FeelAxis,
    /// 0 (linear) to 1 (softest around the centre). Absent: 0.
    #[serde(default)]
    pub expo: f64,
    /// What a full push sends, above 0 and up to 1: a fraction of full
    /// travel, or of the gimbal's full rate. The throttle's is 1. Absent: 1.
    #[serde(default = "full_rate")]
    pub rate: f64,
}

const fn full_rate() -> f64 {
    1.0
}

/// Controller axes to the four sticks, and buttons to commands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputMapping {
    pub id: String,
    pub name: String,
    pub roll: AxisBinding,
    pub pitch: AxisBinding,
    pub yaw: AxisBinding,
    pub throttle: AxisBinding,
    #[serde(default, skip_serializing_if = "ThrottleRange::is_full_travel")]
    pub throttle_range: ThrottleRange,
    /// The stick layout its pilot chose in setup: where the app draws each
    /// control and which stick setup asks for. It changes nothing sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stick_mode: Option<StickMode>,
    /// Around the centre, as a fraction of full travel, read as zero.
    pub deadzone: f64,
    pub buttons: Vec<ButtonBinding>,
    /// Commands sent when an axis moves into a position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<PositionBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rc_channels: Vec<RcAxisBinding>,
    /// Controls not used, such as a rover's pitch and yaw: they read centred
    /// and need no input. Their bindings are kept for when they are used
    /// again. Only for a daemon with `unusedControls`; an older one ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unused: Vec<StickControl>,
    /// Buttons held for the gripper (S263) or to turn a gimbal. Absent
    /// when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionBinding>,
    /// Axes that turn the target vehicle's gimbal: its pitch and its yaw,
    /// each at most once. Absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gimbal: Vec<GimbalAxisBinding>,
    /// Axes that work the target vehicle's camera (S416): its zoom, at most
    /// once. Absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub camera: Vec<CameraAxisBinding>,
    /// Expo and rate for the sticks and the gimbal's axes (S418), each
    /// axis at most once. An axis not listed is linear at full rate.
    /// Absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feel: Vec<AxisFeel>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Claims a controller. Its sticks must be centred. `pinned` needs
/// `vehicleId`; `follows-selection` starts on the client's selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerClaimParams {
    pub id: String,
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_id: Option<String>,
}

/// Changes what a claimed controller drives, retaining its admission checks.
/// Unclaimed, remembers only the next target binding without claiming or routing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerSetTargetParams {
    pub id: String,
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// Keeps on this client's operator the mapping to use with a controller on
/// a vehicle, or, without `vehicleId`, for a vehicle with none of its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerChooseMappingParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub mapping_id: String,
}

/// The vehicle this client has selected, for controllers that follow it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientSelectParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// Replaces a mapping edited from `expectedVersion`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MappingUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    pub name: String,
    pub roll: AxisBinding,
    pub pitch: AxisBinding,
    pub yaw: AxisBinding,
    pub throttle: AxisBinding,
    #[serde(default, skip_serializing_if = "ThrottleRange::is_full_travel")]
    pub throttle_range: ThrottleRange,
    /// The stick layout chosen in setup; absent keeps the mapping's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stick_mode: Option<StickMode>,
    pub deadzone: f64,
    pub buttons: Vec<ButtonBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<PositionBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rc_channels: Vec<RcAxisBinding>,
    /// As for `InputMapping::unused`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unused: Vec<StickControl>,
    /// As for `InputMapping::actions`. Absent keeps the mapping's own, so a
    /// client that knows nothing of them never drops them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<ActionBinding>>,
    /// As for `InputMapping::gimbal`. Absent keeps the mapping's own, as
    /// for `actions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gimbal: Option<Vec<GimbalAxisBinding>>,
    /// As for `InputMapping::camera`. Absent keeps the mapping's own, as
    /// for `actions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<Vec<CameraAxisBinding>>,
    /// As for `InputMapping::feel`. Absent keeps the mapping's own, as for
    /// `actions`. The sticks' feel changes only while no controller using
    /// the mapping is claimed, as their deadzone does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feel: Option<Vec<AxisFeel>>,
}

/// Makes a mapping of its own for a controller: a copy of `from` (Mode 2
/// when absent) named `name`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MappingCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Deletes the custom mapping the person reviewed. Concurrent edits are refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MappingRemoveParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
}

/// One axis as the device reports it, before any mapping: -1 to 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AxisValue {
    /// The platform's code for it, for binding it by `AxisBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    /// What the device calls it, such as `X` or `Rz` for a USB joystick.
    pub name: String,
    /// Its number on the device, where the platform's code gives one: a
    /// USB joystick's axes in their HID order (X 1, Y 2, Z 3, Rx 4, Ry 5,
    /// Rz 6, Slider 7, Dial 8, Wheel 9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// The gamepad axis it is, for a device whose layout the platform
    /// knows; never a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<GamepadAxis>,
    pub value: f64,
    /// What it has shown itself to be since it was plugged in, on a device
    /// whose layout is not known; absent until it has moved, and on a known
    /// gamepad, whose sticks are sticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<InputKind>,
}

/// What an axis is, judged from the places it has read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    /// A stick, slider or knob: it has read between places.
    Continuous,
    /// It has only jumped between places, as a switch or a button does, or
    /// a stick the radio sends as on and off: never one to fly with.
    Switch,
}

/// A button the device has, where the platform can list them (macOS), or
/// one pressed since it was plugged in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ButtonState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    /// What the device calls it, such as `Button 3`.
    pub name: String,
    /// Its number on the device, where the platform's code gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// The gamepad button it is, as for `AxisValue::axis`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<GamepadButton>,
    /// Held now.
    pub pressed: bool,
    /// Pressed at least once since it was plugged in, so a tap between two
    /// reads still shows.
    #[serde(default)]
    pub seen: bool,
    /// How many times it has gone down since it was plugged in, so a reader
    /// counts every tap between two reads (S21's Talk button).
    #[serde(default)]
    pub presses: u32,
}

/// What a plugged-in controller reports right now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerInputs {
    pub id: String,
    /// Whether the platform knows this device's layout, as it does a common
    /// gamepad. Without it, such as for a radio in joystick mode, inputs have
    /// only the device's names, numbers and codes, and are mapped by code.
    pub layout_known: bool,
    /// Every axis the device has, where the platform can list them
    /// (macOS), with its value now; elsewhere every axis that has moved.
    pub axes: Vec<AxisValue>,
    pub buttons: Vec<ButtonState>,
    /// The gripper action it asks its vehicle for now (S263): requested,
    /// not confirmed by the vehicle. Absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gripper: Option<GripperAction>,
    /// A window navigates with it (S420): its direction pad, South and East
    /// send nothing to its vehicle. Absent when not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub navigating: bool,
}

/// Starts, refreshes or ends Navigate (S420) for a controller this client
/// claimed: while on, its direction pad, South and East move focus in the
/// window and send nothing to its vehicle, and its sticks still fly. It
/// ends two seconds after the last refresh, so a window that goes quiet
/// gives those buttons back to the vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerNavigateParams {
    pub id: String,
    pub on: bool,
}

/// Every plugged-in controller's raw input, for testing a gamepad and a
/// mapping. Changes nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerInputsResult {
    pub controllers: Vec<ControllerInputs>,
}

/// Explicitly starts keyboard piloting of one vehicle, with neutral steering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardStartParams {
    pub vehicle_id: String,
    /// Initial throttle input, -1 (idle) to 1 (full); 0 is the middle.
    pub throttle: f64,
}

/// Why the page released its keyboard controller (S291). Absent: the person
/// released it, and nothing is said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum KeyboardStopReason {
    /// UXDuck stopped being the app in front: the keys went elsewhere.
    AppUnfocused,
}

/// Releases this connection's keyboard controller.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardStopParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<KeyboardStopReason>,
}

/// A complete keyboard input frame. Never starts or reclaims a controller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct KeyboardInputParams {
    /// Roll, pitch, yaw and throttle, each -1 to 1.
    pub axes: [f64; 4],
    /// The gimbal's pitch and yaw keys, each -1, 0 or 1 with up and right
    /// positive. Absent turns nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gimbal: Option<[f64; 2]>,
}

//! Controllers and input mappings (ADR-0005 controllers and multi-vehicle
//! safety). A controller is an input device the daemon owns. It drives a
//! vehicle only while an operator client has claimed it, and it changes
//! target only with roll, pitch and yaw centred. Following the selection
//! switches automatically at neutral; pinned targets change explicitly.
//! Throttle is left where the operator holds it: hover, idle or full are all
//! theirs to choose, so it never blocks a claim or a switch.

use serde::{Deserialize, Serialize};

use crate::domain::command::Command;
use crate::domain::gripper::GripperAction;
use crate::domain::modes::Family;

pub const CONTROLLER_UNIT_ID_PREFIX: &str = "unit-";

/// What a claimed controller drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    /// One vehicle; the client's selection never moves it.
    Pinned,
    /// The claiming client's selected vehicle, switched automatically at neutral.
    FollowsSelection,
}

/// Who holds a controller: the connection's client id and its operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    pub client_id: String,
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Controller {
    /// Stable across replugging where the platform gives a device id.
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
    /// The selection moved here, but the switch waits for neutral sticks
    /// or another target admission check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_target: Option<String>,
    pub mapping_id: String,
    /// Why input is paused while a simulation restarts or awaits safe sticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_pause: Option<String>,
    /// A button or position being held for its command, until it goes or
    /// is let go. Never kept over a restart.
    #[serde(skip)]
    pub holding: Option<Hold>,
    /// The last photo a camera said it took after this controller's Take
    /// photo (S416). Never kept over a restart.
    #[serde(skip)]
    pub photo: Option<ControllerPhoto>,
    /// Its charge, as the platform last said (S419). Never kept over a
    /// restart: it is read again.
    #[serde(skip)]
    pub power: ControllerPower,
    pub version: u64,
}

/// A photo a camera said it took after a controller's Take photo (S416).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerPhoto {
    pub vehicle_id: String,
    pub camera_id: String,
    /// Its place in the camera's list, from 0.
    pub index: u32,
    pub ok: bool,
    pub heard_at_us: u64,
}

/// A controller's power, as the platform reports it (S419, ADR-0005 A7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum ControllerPower {
    /// The platform said nothing of its power.
    #[default]
    Unknown,
    /// The platform has no way to say: the browser's Gamepad API.
    Unavailable,
    /// On a cable with no battery to tell of.
    Wired,
    /// On its battery, with this much left.
    Discharging { percent: u8 },
    /// Charging, with this much so far.
    Charging { percent: u8 },
    /// Full, on its charger.
    Charged,
}

/// How low a controller's battery is (S419). The levels are `UPower`'s
/// defaults, which Linux desktops warn of a peripheral's battery at
/// (`PercentageLow=20`, `PercentageCritical=5`, both inclusive), so a
/// controller is called low where its platform would call it low.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PowerLevel {
    Low,
    Critical,
}

/// `UPower`'s default `PercentageLow`.
pub const POWER_LOW_PERCENT: u8 = 20;
/// `UPower`'s default `PercentageCritical`.
pub const POWER_CRITICAL_PERCENT: u8 = 5;

impl ControllerPower {
    /// How low it is while on its battery; never while charging or wired.
    #[must_use]
    pub const fn level(self) -> Option<PowerLevel> {
        match self {
            Self::Discharging { percent } if percent <= POWER_CRITICAL_PERCENT => {
                Some(PowerLevel::Critical)
            }
            Self::Discharging { percent } if percent <= POWER_LOW_PERCENT => Some(PowerLevel::Low),
            _ => None,
        }
    }
}

/// A rumble that tells the pilot something without looking (S419,
/// ADR-0005 A7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rumble {
    /// A hold completed and its command went: one short pulse.
    Done,
    /// A command was held back or refused: two short pulses.
    Refused,
    /// The route paused or the vehicle's link was lost: one long pulse.
    Lost,
}

/// One pulse of a rumble: how long it buzzes, then how long it rests
/// before the next, and how strongly, 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pulse {
    pub on_ms: u32,
    pub off_ms: u32,
    pub strength: f32,
}

impl Rumble {
    /// Its pulses, the same on every platform. Short enough to tell apart
    /// by count and long enough to feel through a grip, in steps of 50 ms,
    /// the step gilrs plays force feedback in.
    #[must_use]
    pub const fn pulses(self) -> &'static [Pulse] {
        const SHORT: Pulse = Pulse {
            on_ms: 150,
            off_ms: 150,
            strength: 0.6,
        };
        match self {
            Self::Done => &[SHORT],
            Self::Refused => &[SHORT, SHORT],
            Self::Lost => &[Pulse {
                on_ms: 600,
                off_ms: 0,
                strength: 1.0,
            }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetBinding {
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// A hold under way: what it sends when complete, and how long it takes.
#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    pub command: Command,
    pub hold_ms: u32,
}

/// A controller's axes, named as the platform library names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GamepadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftZ,
    RightZ,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

impl GamepadButton {
    /// What the mapping editor calls it (`BUTTONS` in its `inputs.ts`),
    /// for a refusal that names it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::South => "A / Cross (bottom)",
            Self::East => "B / Circle (right)",
            Self::West => "X / Square (left)",
            Self::North => "Y / Triangle (top)",
            Self::LeftTrigger => "LB / L1",
            Self::LeftTrigger2 => "LT / L2",
            Self::RightTrigger => "RB / R1",
            Self::RightTrigger2 => "RT / R2",
            Self::DpadUp => "D-pad up",
            Self::DpadDown => "D-pad down",
            Self::DpadLeft => "D-pad left",
            Self::DpadRight => "D-pad right",
            Self::Select => "Back / Share",
            Self::Start => "Start / Options",
            Self::Mode => "Home",
            Self::LeftThumb => "Left stick press (L3)",
            Self::RightThumb => "Right stick press (R3)",
        }
    }
}

/// Which axis drives a stick, and which way round.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisBinding {
    pub axis: GamepadAxis,
    /// The platform's own code for the input, for a device whose layout the
    /// platform does not know, such as a radio in joystick mode. When set it
    /// is read instead of `axis`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    pub inverted: bool,
    /// Where the axis reads at its ends and at rest, measured on the
    /// device: a radio's stick rarely reaches -1 and 1 exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<AxisCalibration>,
}

/// An axis's raw readings, -1 to 1, at its ends and at rest. The centre of
/// a stick that does not spring back, such as a throttle, is the middle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AxisCalibration {
    pub min: f64,
    pub centre: f64,
    pub max: f64,
}

impl AxisCalibration {
    /// Whether it can scale a reading: in order, inside -1 to 1, with
    /// room on each side of the centre.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        const ROOM: f64 = 0.05;
        [self.min, self.centre, self.max]
            .iter()
            .all(|v| v.is_finite() && (-1.0..=1.0).contains(v))
            && self.centre - self.min >= ROOM
            && self.max - self.centre >= ROOM
    }
}

impl AxisBinding {
    /// Names and platform codes can identify the same physical input. A code
    /// takes precedence over its stored (possibly guessed) gamepad name.
    #[must_use]
    pub fn shares_input(
        &self,
        other: &Self,
        inputs: &[(Option<u32>, Option<GamepadAxis>)],
    ) -> bool {
        let matches = |binding: &Self, code, axis| match binding.code {
            Some(wanted) => code == Some(wanted),
            None => axis == Some(binding.axis),
        };
        match (self.code, other.code) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.axis == other.axis,
            _ => inputs
                .iter()
                .any(|&(code, axis)| matches(self, code, axis) && matches(other, code, axis)),
        }
    }

    /// A raw reading scaled by the calibration, so the measured ends read
    /// -1 and 1 and the rest reads 0; unchanged without one.
    #[must_use]
    pub fn calibrated(&self, raw: f64) -> f64 {
        let Some(c) = self.calibration.filter(AxisCalibration::is_valid) else {
            return raw;
        };
        let scaled = if raw >= c.centre {
            (raw - c.centre) / (c.max - c.centre)
        } else {
            (raw - c.centre) / (c.centre - c.min)
        };
        scaled.clamp(-1.0, 1.0)
    }
}

/// An additional ArduPilot RC input, driven by an axis or a radio's switch.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RcAxisBinding {
    /// Explicit input channel, 5 through 16; never a servo output number.
    pub channel: u8,
    pub input: AxisBinding,
}

/// A gimbal's pitch or yaw, as an input turns it (S407).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalAxis {
    Pitch,
    Yaw,
}

/// An axis that turns the target vehicle's gimbal (S407): the further it
/// is pushed the faster the gimbal turns, up and right positive, and at
/// its centre the gimbal stops. It turns the gimbal only once it has been
/// at its centre after the controller's current route began.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GimbalAxisBinding {
    pub control: GimbalAxis,
    pub input: AxisBinding,
}

/// What an axis does to the target vehicle's camera (S416).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraAxis {
    Zoom,
}

/// An axis that zooms the target vehicle's camera (S416): up or squeezed
/// zooms in, the further the faster, down zooms out, and at its centre
/// the zoom stops. It zooms only once it has been at its centre after the
/// controller's current route began, as a gimbal's axis turns (S407).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraAxisBinding {
    pub control: CameraAxis,
    pub input: AxisBinding,
}

/// A button that sends a command to the target vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonBinding {
    pub button: GamepadButton,
    /// The platform's own code for the button, for a device whose layout
    /// the platform does not know. When set it is matched instead of
    /// `button`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    pub command: Command,
    /// How long it must be held before the command goes; none: on the
    /// press itself. With a hold it is the input's long press (S418).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
    /// A press, or two quick presses (S418). Absent: a press.
    #[serde(default, skip_serializing_if = "ButtonGesture::is_press")]
    pub gesture: ButtonGesture,
    /// The Shift button held for it (S418): it goes only while that Shift
    /// is held, and one without goes only while no Shift is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<ShiftButton>,
    /// Sent again every this many milliseconds while the button stays
    /// down (S418), for a step such as a zoom step. Absent: once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_ms: Option<u32>,
}

/// How an input is pressed for its binding (S418). A long press is a
/// press with a hold (`ButtonBinding::hold_ms`).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ButtonGesture {
    #[default]
    Press,
    /// Two presses, the second within `DOUBLE_PRESS_MS` of letting go of
    /// the first.
    DoublePress,
}

impl ButtonGesture {
    #[must_use]
    pub fn is_press(&self) -> bool {
        *self == Self::Press
    }
}

/// A button bound as a Shift (`HeldAction::Shift`), named as a binding
/// on its layer names it (S418).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShiftButton {
    pub button: GamepadButton,
    /// As for `ButtonBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
}

impl ShiftButton {
    /// Whether it is this input, by code when it has one.
    #[must_use]
    pub fn matches(&self, button: Option<GamepadButton>, code: Option<u32>) -> bool {
        match self.code {
            Some(wanted) => code == Some(wanted),
            None => button == Some(self.button),
        }
    }
}

/// How long a press waits for a second one before it goes, on an input
/// with a double press bound (S418). 300 ms is about the slowest a
/// pilot's deliberate double tap on a gamepad takes, and short enough
/// that a single press still feels prompt. An input with no double press
/// bound never waits.
pub const DOUBLE_PRESS_MS: u32 = 300;

/// The quickest a held button may repeat its command (S418): the engine
/// looks at held buttons on each pilot tick, 25 times a second, so it
/// cannot repeat any faster.
pub const REPEAT_MIN_MS: u32 = 40;

impl ButtonBinding {
    /// Whether it is this input, by code when it has one.
    #[must_use]
    pub fn matches(&self, button: Option<GamepadButton>, code: Option<u32>) -> bool {
        match self.code {
            Some(wanted) => code == Some(wanted),
            None => button == Some(self.button),
        }
    }

    /// Whether it is on the same input as `other`, compared as stored.
    fn same_input(&self, button: GamepadButton, code: Option<u32>) -> bool {
        match (self.code, code) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.button == button,
            _ => false,
        }
    }
}

/// A button held for an action that lasts while it is held, such as the
/// gripper's opening (S263): unlike `ButtonBinding`, nothing is sent on the
/// press as a command. The gripper acts only while pressed after the
/// controller's current route began, and stops on release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

/// What a held button does: holds the gripper open or closed (S263),
/// turns the target vehicle's gimbal up, down, left or right while it is
/// held (S407), or Talk, held to speak to the agent in the window that
/// claimed the controller and let go to send (S21). The daemon only keeps
/// Talk, and the window reads the button. The app's actions (S420), from
/// `NextVehicle` to `Navigate`, are kept the same way: pressed, the window
/// that claimed the controller acts, and no vehicle hears of it.
/// `Shift` sends nothing: while it is held the other buttons do what their
/// Shift layer says (S418).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// The buttons Navigate (S420) moves focus with, presses and goes back
/// with: the direction pad, South and East. While a window navigates with
/// a controller these send nothing to its vehicle, and every other input
/// flies as before.
pub const NAVIGATE_BUTTONS: [GamepadButton; 6] = [
    GamepadButton::DpadUp,
    GamepadButton::DpadDown,
    GamepadButton::DpadLeft,
    GamepadButton::DpadRight,
    GamepadButton::South,
    GamepadButton::East,
];

impl HeldAction {
    /// The gripper action it asks for, or none for any other.
    #[must_use]
    pub const fn gripper(self) -> Option<GripperAction> {
        match self {
            Self::Open => Some(GripperAction::Open),
            Self::Close => Some(GripperAction::Close),
            Self::Talk
            | Self::GimbalUp
            | Self::GimbalDown
            | Self::GimbalLeft
            | Self::GimbalRight
            | Self::ZoomIn
            | Self::ZoomOut
            | Self::FocusNear
            | Self::FocusFar
            | Self::NextVehicle
            | Self::PreviousVehicle
            | Self::CockpitTab
            | Self::CameraTab
            | Self::MapTab
            | Self::NextCamera
            | Self::CameraFullWindow
            | Self::ShowOnMap
            | Self::Navigate
            | Self::Shift => None,
        }
    }

    /// What it asks of a camera while held (S416): its zoom (index 0) or
    /// its focus (index 1), and which way, zooming in and focusing farther
    /// positive. None for any other.
    #[must_use]
    pub const fn camera(self) -> Option<(usize, f64)> {
        match self {
            Self::ZoomIn => Some((0, 1.0)),
            Self::ZoomOut => Some((0, -1.0)),
            Self::FocusFar => Some((1, 1.0)),
            Self::FocusNear => Some((1, -1.0)),
            Self::Open
            | Self::Close
            | Self::Talk
            | Self::GimbalUp
            | Self::GimbalDown
            | Self::GimbalLeft
            | Self::GimbalRight
            | Self::NextVehicle
            | Self::PreviousVehicle
            | Self::CockpitTab
            | Self::CameraTab
            | Self::MapTab
            | Self::NextCamera
            | Self::CameraFullWindow
            | Self::ShowOnMap
            | Self::Navigate
            | Self::Shift => None,
        }
    }

    /// Which way it turns a gimbal and how fast, as a full push of an
    /// axis would: up and right positive. None for any other.
    #[must_use]
    pub const fn gimbal(self) -> Option<(GimbalAxis, f64)> {
        match self {
            Self::GimbalUp => Some((GimbalAxis::Pitch, 1.0)),
            Self::GimbalDown => Some((GimbalAxis::Pitch, -1.0)),
            Self::GimbalLeft => Some((GimbalAxis::Yaw, -1.0)),
            Self::GimbalRight => Some((GimbalAxis::Yaw, 1.0)),
            Self::Open
            | Self::Close
            | Self::Talk
            | Self::ZoomIn
            | Self::ZoomOut
            | Self::FocusNear
            | Self::FocusFar
            | Self::NextVehicle
            | Self::PreviousVehicle
            | Self::CockpitTab
            | Self::CameraTab
            | Self::MapTab
            | Self::NextCamera
            | Self::CameraFullWindow
            | Self::ShowOnMap
            | Self::Navigate
            | Self::Shift => None,
        }
    }

    /// Whether the window that claimed the controller does it, not the
    /// vehicle: Talk (S21) and the app's actions (S420).
    #[must_use]
    pub const fn for_window(self) -> bool {
        matches!(
            self,
            Self::Talk
                | Self::NextVehicle
                | Self::PreviousVehicle
                | Self::CockpitTab
                | Self::CameraTab
                | Self::MapTab
                | Self::NextCamera
                | Self::CameraFullWindow
                | Self::ShowOnMap
                | Self::Navigate
        )
    }
}

impl ActionBinding {
    #[must_use]
    pub fn matches(&self, button: Option<GamepadButton>, code: Option<u32>) -> bool {
        match self.code {
            Some(wanted) => code == Some(wanted),
            None => button == Some(self.button),
        }
    }
}

/// The shortest and longest hold a button or position may ask for.
pub const HOLD_MS: std::ops::RangeInclusive<u32> = 100..=10_000;

/// The shortest hold a dangerous command takes on a controller
/// (`Command::controller_hold_reason`, S417): the shortest hold the
/// mapping's editor offers, longer than a button is down when it is bumped.
pub const DANGEROUS_HOLD_MS: u32 = 1_000;

/// A position of an axis, such as a switch's or a six-way button group's,
/// that sends a command when the axis moves into it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionBinding {
    pub axis: GamepadAxis,
    /// As for `AxisBinding::code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    /// The axis's raw value at the position, -1 to 1.
    pub at: f64,
    pub command: Command,
    /// As for `ButtonBinding::hold_ms`: how long the axis must stay there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
}

/// How near its `at` an axis must be to be in a position.
pub const POSITION_TOLERANCE: f64 = 0.1;

impl PositionBinding {
    /// The axis this position is of, as a binding to read it by.
    #[must_use]
    pub fn binding(&self) -> AxisBinding {
        AxisBinding {
            axis: self.axis,
            code: self.code,
            inverted: false,
            calibration: None,
        }
    }

    #[must_use]
    pub fn holds(&self, value: f64) -> bool {
        (value - self.at).abs() <= POSITION_TOLERANCE
    }
}

/// Which part of an axis provides the throttle's 0 to 100% input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StickControl {
    Roll,
    Pitch,
    Yaw,
}

/// An axis whose feel a mapping sets (S418): the four sticks and the
/// gimbal's two axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeelAxis {
    Roll,
    Pitch,
    Yaw,
    Throttle,
    GimbalPitch,
    GimbalYaw,
}

/// How an axis feels after its deadzone (S418): `expo` softens it
/// around the centre, and `rate` scales its full push.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxisFeel {
    pub axis: FeelAxis,
    /// 0 is linear, 1 the softest: `(1 - expo) * x + expo * x³`. Above 1
    /// the curve turns back on itself, so a push further would send less.
    #[serde(default)]
    pub expo: f64,
    /// What a full push sends, above 0 and up to 1, a fraction of the
    /// stick's full travel or of the gimbal's full rate: `MANUAL_CONTROL`
    /// carries nothing past a full push. The throttle's stays 1, so it
    /// still reaches idle and full.
    #[serde(default = "full_rate")]
    pub rate: f64,
}

const fn full_rate() -> f64 {
    1.0
}

impl AxisFeel {
    /// A value from -1 to 1, after the deadzone, through the curve.
    #[must_use]
    pub fn apply(&self, value: f64) -> f64 {
        let curved = (1.0 - self.expo) * value + self.expo * value.powi(3);
        curved * self.rate
    }

    /// Why it cannot be used as it is, if it cannot.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if !(0.0..=1.0).contains(&self.expo) {
            return Some("Expo is from 0 to 100%.");
        }
        if !(self.rate > 0.0 && self.rate <= 1.0) {
            return Some("Rate is above 0 and up to 100%.");
        }
        if self.axis == FeelAxis::Throttle && (self.rate - 1.0).abs() > f64::EPSILON {
            return Some("The throttle's rate stays 100%, so it still reaches idle and full.");
        }
        None
    }
}

/// Controller inputs to the four pilot axes, additional ArduPilot RC
/// channels, and buttons and axis positions to commands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unused: Vec<StickControl>,
    /// Buttons held for the gripper (S263) or to turn a gimbal (S407).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionBinding>,
    /// Axes that turn the target vehicle's gimbal (S407): its pitch and
    /// its yaw, each at most once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gimbal: Vec<GimbalAxisBinding>,
    /// Axes that work the target vehicle's camera (S416): its zoom, at
    /// most once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub camera: Vec<CameraAxisBinding>,
    /// Expo and rate for the sticks and the gimbal's axes (S418), each
    /// axis at most once. An axis not listed is linear at full rate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feel: Vec<AxisFeel>,
    pub version: u64,
}

/// How far from the centre roll, pitch and yaw must be, at most, to count
/// as neutral for claiming and switching, where throttle is not judged;
/// and a keyboard's starting throttle on a vehicle whose throttle centres.
pub const NEUTRAL: f64 = 0.1;

/// What a vehicle's throttle is called when its neutral is mid-stick, as
/// UXDuck sends it: forward / reverse on an `ArduPilot` rover or boat (stop
/// at the throttle channel's RC trim on both input methods) and on a PX4
/// one; ascend / descend on a sub or a blimp. Any autopilot but PX4 is
/// driven as `ArduPilot` is. `None` where the throttle rests at the
/// bottom, as an aircraft's does.
#[must_use]
pub fn centred_throttle(autopilot: &str, vehicle_type: &str) -> Option<&'static str> {
    match Family::for_vehicle(autopilot, vehicle_type)? {
        Family::Rover => Some("forward / reverse"),
        Family::Sub | Family::Blimp => Some("ascend / descend"),
        Family::Px4 if matches!(vehicle_type, "GroundRover" | "SurfaceBoat") => {
            Some("forward / reverse")
        }
        Family::Copter | Family::Plane | Family::Px4 => None,
    }
}

/// An `ArduRover` throttle channel's calibration: `RCMAP_THROTTLE` and that
/// channel's `RCn_MIN`, `RCn_TRIM` and `RCn_MAX`. `ArduRover` stops at the
/// trim, wherever it is: mid-range on a radio whose throttle centres, at the
/// minimum on a forward-only setup whose stick rests at the bottom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoverThrottle {
    pub channel: u8,
    pub min: f64,
    pub trim: f64,
    pub max: f64,
}

impl RoverThrottle {
    /// From the vehicle's parameters, none of them assumed, with the same
    /// limits as RC overrides: what is missing or wrong otherwise.
    ///
    /// # Errors
    ///
    /// The first parameter missing, or why the calibration is unusable.
    pub fn from_params(param: impl Fn(&str) -> Option<f64>) -> Result<Self, String> {
        let map = param("RCMAP_THROTTLE").ok_or("RCMAP_THROTTLE is missing")?;
        if map.fract() != 0.0 || !(1.0..=16.0).contains(&map) {
            return Err(format!("RCMAP_THROTTLE {map} is not a channel"));
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a whole channel number from 1 to 16"
        )]
        let channel = map as u8;
        let read = |what: &str| {
            let name = format!("RC{channel}_{what}");
            param(&name)
                .filter(|value| value.is_finite())
                .ok_or_else(|| format!("{name} is missing"))
        };
        let (min, trim, max) = (read("MIN")?, read("TRIM")?, read("MAX")?);
        if !(800.0..=2200.0).contains(&min) || !(800.0..=2200.0).contains(&max) || min >= max {
            return Err(format!(
                "RC{channel}_MIN {min} to RC{channel}_MAX {max} is not a PWM range"
            ));
        }
        if !(min..=max).contains(&trim) {
            return Err(format!(
                "RC{channel}_TRIM {trim} is outside RC{channel}_MIN to RC{channel}_MAX"
            ));
        }
        Ok(Self {
            channel,
            min,
            trim,
            max,
        })
    }

    /// `MANUAL_CONTROL`'s z, -1000 to 1000, for a throttle of 0 to 1000
    /// whose middle is neutral. `ArduRover` turns z into this channel's PWM
    /// as min + (max - min) * (z + 1000) / 2000, truncated, and reads it
    /// around the trim (`GCS_MAVLINK::manual_override` and
    /// `RC_Channel::pwm_to_angle_dz_trim`, Rover 4.5 to 4.7), so the middle
    /// goes out as the trim's PWM, the bottom as min and the top as max,
    /// as RC overrides put them. Below the middle reverses only where the
    /// trim is above min.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "from -1000 to 1000 for a calibration from_params accepted"
    )]
    pub fn manual_z(&self, throttle: i16) -> i16 {
        let value = f64::from(throttle.clamp(0, 1000)) / 500.0 - 1.0;
        let pwm = if value >= 0.0 {
            self.trim + value * (self.max - self.trim)
        } else {
            self.trim + value * (self.trim - self.min)
        };
        // The smallest z whose truncated PWM reaches this one: a whole PWM
        // such as the trim exactly, since a step of z is under 1 µs.
        ((pwm - self.min) * 2000.0 / (self.max - self.min) - 1e-9).ceil() as i16 - 1000
    }
}

/// The HID code of a joystick's X input (Generic Desktop page, usage 0x30);
/// Y, Z and Rx follow it.
const HID_X: u32 = 0x1_0030;

impl InputMapping {
    /// A command position must have its own input and an unambiguous range.
    /// Used at admission and again on live input, including old saved mappings
    /// and aliases first learned after a route was granted.
    #[must_use]
    pub fn position_conflicts(
        &self,
        position: &PositionBinding,
        inputs: &[(Option<u32>, Option<GamepadAxis>)],
    ) -> bool {
        let input = position.binding();
        !(-1.0..=1.0).contains(&position.at)
            || self
                .stick_bindings()
                .chain(self.rc_channels.iter().map(|b| &b.input))
                .chain(self.gimbal.iter().map(|b| &b.input))
                .chain(self.camera.iter().map(|b| &b.input))
                .any(|b| input.shares_input(b, inputs))
            || self
                .positions
                .iter()
                .filter(|other| {
                    input.shares_input(&other.binding(), inputs)
                        && (position.at - other.at).abs() <= 2.0 * POSITION_TOLERANCE
                })
                .count()
                > 1
    }

    /// The inputs the sticks read: throttle's and those of the controls
    /// in use.
    pub fn stick_bindings(&self) -> impl Iterator<Item = &AxisBinding> {
        [
            (Some(StickControl::Roll), &self.roll),
            (Some(StickControl::Pitch), &self.pitch),
            (Some(StickControl::Yaw), &self.yaw),
            (None, &self.throttle),
        ]
        .into_iter()
        .filter(|(control, _)| control.is_none_or(|c| !self.unused.contains(&c)))
        .map(|(_, binding)| binding)
    }

    /// What a button does when pressed with no Shift held: its first
    /// binding without a Shift that is not a double press. Matched by its
    /// code when the binding has one, else by its gamepad name.
    #[must_use]
    pub fn button(
        &self,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) -> Option<&ButtonBinding> {
        self.buttons
            .iter()
            .find(|b| b.matches(button, code) && b.shift.is_none() && b.gesture.is_press())
    }

    /// The held action a button is bound to with no Shift held, if any.
    #[must_use]
    pub fn action(
        &self,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) -> Option<&ActionBinding> {
        self.action_in(button, code, None)
    }

    /// The held action a button is bound to on a Shift's layer, or with
    /// none (S418). A Shift button is one on any layer.
    #[must_use]
    pub fn action_in(
        &self,
        button: Option<GamepadButton>,
        code: Option<u32>,
        shift: Option<ShiftButton>,
    ) -> Option<&ActionBinding> {
        self.actions.iter().find(|b| {
            b.matches(button, code) && (b.shift == shift || b.action == HeldAction::Shift)
        })
    }

    /// The buttons bound as a Shift (S418).
    pub fn shifts(&self) -> impl Iterator<Item = ShiftButton> + '_ {
        self.actions
            .iter()
            .filter(|b| b.action == HeldAction::Shift && b.shift.is_none())
            .map(|b| ShiftButton {
                button: b.button,
                code: b.code,
            })
    }

    /// Why the buttons' bindings cannot all be used as they are (S418,
    /// extending S12): two actions for one gesture of one input on one
    /// layer, a command and a held action together there, a Shift that
    /// does anything else, a layer of a button that is no Shift, a double
    /// press held, or a repeat that cannot be. None when they can.
    #[must_use]
    pub fn button_problem(&self) -> Option<String> {
        let shifts: Vec<ShiftButton> = self.shifts().collect();
        let is_shift = |button: GamepadButton, code: Option<u32>| {
            shifts.iter().any(|s| s.button == button && s.code == code)
        };
        let named = |button: GamepadButton, code: Option<u32>| {
            code.map_or_else(
                || button.label().to_owned(),
                |code| format!("Button {code:#x}"),
            )
        };
        for b in &self.buttons {
            let name = named(b.button, b.code);
            if is_shift(b.button, b.code) {
                return Some(format!(
                    "{name} is a Shift, so it sends nothing of its own. Remove its other actions."
                ));
            }
            if let Some(shift) = b.shift
                && !is_shift(shift.button, shift.code)
            {
                return Some(format!(
                    "{name} has an action for {}, which is not a Shift.",
                    named(shift.button, shift.code)
                ));
            }
            if b.gesture == ButtonGesture::DoublePress && b.hold_ms.is_some() {
                return Some(format!("{name}'s double press can't also be held."));
            }
            let same = |other: &&ButtonBinding| {
                other.same_input(b.button, b.code) && other.shift == b.shift
            };
            let kind = |other: &ButtonBinding| (other.gesture, other.hold_ms.is_some());
            if self
                .buttons
                .iter()
                .filter(same)
                .filter(|other| kind(other) == kind(b))
                .count()
                > 1
            {
                return Some(format!("{name} has two actions for the same press."));
            }
            if self
                .actions
                .iter()
                .any(|a| a.shift == b.shift && b.same_input(a.button, a.code))
            {
                return Some(format!(
                    "{name} either sends a command or is held for an action, on each layer."
                ));
            }
            if let Some(problem) = self.repeat_problem(b, &name) {
                return Some(problem);
            }
        }
        for a in &self.actions {
            let name = named(a.button, a.code);
            if a.action == HeldAction::Shift && a.shift.is_some() {
                return Some(format!(
                    "{name} is a Shift, so it can't be on another Shift."
                ));
            }
            if a.action.for_window() && a.shift.is_some() {
                return Some(format!(
                    "{name} has Talk or an app action on a Shift's layer. The window does those only with no Shift held, so bind it to {name} without a Shift."
                ));
            }
            if let Some(shift) = a.shift
                && !is_shift(shift.button, shift.code)
            {
                return Some(format!(
                    "{name} has an action for {}, which is not a Shift.",
                    named(shift.button, shift.code)
                ));
            }
            let alike = |other: &&ActionBinding| {
                other.button == a.button
                    && other.code == a.code
                    && (other.shift == a.shift
                        || other.action == HeldAction::Shift
                        || a.action == HeldAction::Shift)
            };
            if self.actions.iter().filter(alike).count() > 1 {
                return Some(if is_shift(a.button, a.code) {
                    format!("{name} is a Shift, so it does nothing else.")
                } else {
                    format!("{name} is held for two actions on one layer.")
                });
            }
        }
        None
    }

    /// Why a button's repeat while held cannot be (S418): not a step, not
    /// its input's only press on its layer, or faster than a pilot tick.
    fn repeat_problem(&self, b: &ButtonBinding, name: &str) -> Option<String> {
        let repeat = b.repeat_ms?;
        if !b.command.repeats_while_held() {
            return Some(format!(
                "{name} repeats {}, which is not a step. Only steps, such as a zoom step, repeat.",
                b.command.label()
            ));
        }
        let presses = self
            .buttons
            .iter()
            .filter(|other| other.same_input(b.button, b.code) && other.shift == b.shift)
            .count();
        if b.hold_ms.is_some() || !b.gesture.is_press() || presses > 1 {
            return Some(format!(
                "{name} repeats while held, so it goes on the press and can't also have a long or double press."
            ));
        }
        if repeat < REPEAT_MIN_MS {
            return Some(format!(
                "{name} repeats every {repeat} ms. UXDuck repeats at most every {REPEAT_MIN_MS} ms, 25 times a second."
            ));
        }
        None
    }

    /// Whether a gimbal's axis may be bound as it is (S407): pitch and yaw
    /// once each at most, on an input no stick, RC channel, command
    /// position or other gimbal axis reads. `inputs` are a device's own,
    /// by code and gamepad name, where they are known.
    #[must_use]
    pub fn gimbal_conflicts(
        &self,
        binding: &GimbalAxisBinding,
        inputs: &[(Option<u32>, Option<GamepadAxis>)],
    ) -> bool {
        self.gimbal
            .iter()
            .filter(|other| other.control == binding.control)
            .count()
            > 1
            || self
                .stick_bindings()
                .chain(self.rc_channels.iter().map(|b| &b.input))
                .any(|b| binding.input.shares_input(b, inputs))
            || self
                .positions
                .iter()
                .any(|p| binding.input.shares_input(&p.binding(), inputs))
            || self
                .gimbal
                .iter()
                .filter(|other| binding.input.shares_input(&other.input, inputs))
                .count()
                > 1
            || self
                .camera
                .iter()
                .any(|other| binding.input.shares_input(&other.input, inputs))
    }

    /// Whether a camera's axis may be bound as it is (S416): zoom once at
    /// most, on an input no stick, RC channel, command position, gimbal
    /// axis or other camera axis reads.
    #[must_use]
    pub fn camera_conflicts(
        &self,
        binding: &CameraAxisBinding,
        inputs: &[(Option<u32>, Option<GamepadAxis>)],
    ) -> bool {
        self.camera
            .iter()
            .filter(|other| other.control == binding.control)
            .count()
            > 1
            || self
                .stick_bindings()
                .chain(self.rc_channels.iter().map(|b| &b.input))
                .chain(self.gimbal.iter().map(|b| &b.input))
                .any(|b| binding.input.shares_input(b, inputs))
            || self
                .positions
                .iter()
                .any(|p| binding.input.shares_input(&p.binding(), inputs))
            || self
                .camera
                .iter()
                .filter(|other| binding.input.shares_input(&other.input, inputs))
                .count()
                > 1
    }

    /// What the camera's axes ask for, each -1 to 1 after the deadzone and
    /// inversion, in the order of `camera` (S416). None where an input is
    /// not known yet, which asks for nothing.
    #[must_use]
    pub fn camera_axes(&self, input: impl Fn(&AxisBinding) -> Option<f64>) -> Vec<f64> {
        self.camera
            .iter()
            .map(|binding| self.past_deadzone(&binding.input, input(&binding.input)))
            .collect()
    }

    /// A raw reading after the binding's calibration, the deadzone and
    /// inversion: 0 for one not known.
    fn past_deadzone(&self, binding: &AxisBinding, raw: Option<f64>) -> f64 {
        let Some(raw) = raw else {
            return 0.0;
        };
        let raw = binding.calibrated(raw.clamp(-1.0, 1.0));
        let value = if raw.abs() <= self.deadzone {
            0.0
        } else {
            raw.signum() * (raw.abs() - self.deadzone) / (1.0 - self.deadzone)
        };
        if binding.inverted { -value } else { value }
    }

    /// How fast the gimbal's axes ask it to turn, pitch then yaw, each -1
    /// to 1 with up and right positive, after the deadzone and inversion.
    /// `input` reads a binding's raw value. None where it is not known
    /// yet, which asks for nothing.
    #[must_use]
    pub fn gimbal_axes(&self, input: impl Fn(&AxisBinding) -> Option<f64>) -> Vec<f64> {
        self.gimbal
            .iter()
            .map(|binding| {
                let Some(raw) = input(&binding.input) else {
                    return 0.0;
                };
                let raw = binding.input.calibrated(raw.clamp(-1.0, 1.0));
                let value = if raw.abs() <= self.deadzone {
                    0.0
                } else {
                    raw.signum() * (raw.abs() - self.deadzone) / (1.0 - self.deadzone)
                };
                let value = if binding.input.inverted {
                    -value
                } else {
                    value
                };
                self.felt(
                    match binding.control {
                        GimbalAxis::Pitch => FeelAxis::GimbalPitch,
                        GimbalAxis::Yaw => FeelAxis::GimbalYaw,
                    },
                    value,
                )
            })
            .collect()
    }

    /// The direction pad as the gimbal's held actions (S407), on each of
    /// its buttons that does nothing else. The built-in gamepad layouts
    /// carry it. A radio has no direction pad, and its switches and knobs
    /// differ from one model to the next, so its pilot binds them.
    #[must_use]
    pub fn with_gimbal_pad(mut self) -> Self {
        for (button, action) in [
            (GamepadButton::DpadUp, HeldAction::GimbalUp),
            (GamepadButton::DpadDown, HeldAction::GimbalDown),
            (GamepadButton::DpadLeft, HeldAction::GimbalLeft),
            (GamepadButton::DpadRight, HeldAction::GimbalRight),
        ] {
            let taken = self
                .buttons
                .iter()
                .map(|b| (b.button, b.code))
                .chain(self.actions.iter().map(|a| (a.button, a.code)))
                .any(|bound| bound == (button, None));
            if !taken {
                self.actions.push(ActionBinding {
                    button,
                    code: None,
                    action,
                    shift: None,
                });
            }
        }
        self
    }

    /// Mode 2, the common one: throttle and yaw on the left stick, pitch
    /// and roll on the right.
    #[must_use]
    pub fn mode_2() -> Self {
        let bind = |axis| AxisBinding {
            axis,
            code: None,
            inverted: false,
            calibration: None,
        };
        Self {
            id: "mode-2".into(),
            name: "Mode 2".into(),
            roll: bind(GamepadAxis::RightStickX),
            pitch: bind(GamepadAxis::RightStickY),
            yaw: bind(GamepadAxis::LeftStickX),
            throttle: bind(GamepadAxis::LeftStickY),
            throttle_range: ThrottleRange::FullTravel,
            stick_mode: Some(StickMode::Mode2),
            deadzone: 0.05,
            buttons: vec![
                ButtonBinding {
                    button: GamepadButton::East,
                    code: None,
                    command: Command::Rtl,
                    hold_ms: None,
                    gesture: ButtonGesture::Press,
                    shift: None,
                    repeat_ms: None,
                },
                ButtonBinding {
                    button: GamepadButton::North,
                    code: None,
                    command: Command::SetMode {
                        mode: "LOITER".into(),
                    },
                    hold_ms: None,
                    gesture: ButtonGesture::Press,
                    shift: None,
                    repeat_ms: None,
                },
            ],
            positions: Vec::new(),
            rc_channels: Vec::new(),
            unused: Vec::new(),
            actions: Vec::new(),
            gimbal: Vec::new(),
            camera: Vec::new(),
            feel: Vec::new(),
            version: 1,
        }
    }

    /// Mode 1: pitch and yaw on the left stick, throttle and roll on the
    /// right.
    #[must_use]
    pub fn mode_1() -> Self {
        let bind = |axis| AxisBinding {
            axis,
            code: None,
            inverted: false,
            calibration: None,
        };
        Self {
            id: "mode-1".into(),
            name: "Mode 1".into(),
            roll: bind(GamepadAxis::RightStickX),
            pitch: bind(GamepadAxis::LeftStickY),
            yaw: bind(GamepadAxis::LeftStickX),
            throttle: bind(GamepadAxis::RightStickY),
            stick_mode: Some(StickMode::Mode1),
            ..Self::mode_2()
        }
    }

    /// A radio as a USB joystick (`EdgeTX`, `OpenTX`): channels 1 to 4 in
    /// AETR order on its first four inputs, read by their HID codes, since
    /// no platform knows a radio's layout. No buttons: a radio's switches
    /// differ from one model to the next.
    #[must_use]
    pub fn radio() -> Self {
        let bind = |axis, code| AxisBinding {
            axis,
            code: Some(code),
            inverted: false,
            calibration: None,
        };
        Self {
            id: "radio".into(),
            name: "Radio (AETR)".into(),
            roll: bind(GamepadAxis::LeftStickX, HID_X),
            pitch: bind(GamepadAxis::LeftStickY, HID_X + 1),
            throttle: bind(GamepadAxis::RightStickX, HID_X + 2),
            yaw: bind(GamepadAxis::RightStickY, HID_X + 3),
            buttons: Vec::new(),
            // A radio's own setting says which stick is which.
            stick_mode: None,
            ..Self::mode_2()
        }
    }

    /// An `ExpressLRS` 3 transmitter as a Bluetooth joystick (named
    /// `ExpressLRS Joystick`): it puts channels 1 to 4 on X, Y, Rx and Ry, and its first
    /// switches on Z and Rz.
    #[must_use]
    pub fn expresslrs_bluetooth() -> Self {
        let radio = Self::radio();
        Self {
            id: "expresslrs-bluetooth".into(),
            name: "ExpressLRS 3.x Bluetooth".into(),
            throttle: AxisBinding {
                code: Some(HID_X + 3),
                ..radio.throttle
            },
            yaw: AxisBinding {
                code: Some(HID_X + 4),
                ..radio.yaw
            },
            ..radio
        }
    }

    /// An `ExpressLRS` 4 transmitter as a Bluetooth joystick (named
    /// `ELRS Joystick`): its newer gamepad library takes the same axes in another
    /// order, so channels 1 to 4 are on X, Y, Ry and Rz, and its first
    /// switches on Z and Rx.
    #[must_use]
    pub fn expresslrs_4_bluetooth() -> Self {
        let radio = Self::radio();
        Self {
            id: "expresslrs-4-bluetooth".into(),
            name: "ExpressLRS 4.x Bluetooth".into(),
            throttle: AxisBinding {
                code: Some(HID_X + 4),
                ..radio.throttle
            },
            yaw: AxisBinding {
                code: Some(HID_X + 5),
                ..radio.yaw
            },
            ..radio
        }
    }

    /// The built-in mapping that fits a controller whose layout the
    /// platform does not know, by the name it gives. `ELRS Joystick` is
    /// matched by `ExpressLRS` 4's name only, not its Bluetooth ids, which
    /// other ESP32 gamepads share.
    #[must_use]
    pub fn for_radio(name: &str) -> Self {
        let name = name.trim().to_lowercase();
        if name == "elrs joystick" {
            Self::expresslrs_4_bluetooth()
        } else if name.contains("expresslrs") {
            Self::expresslrs_bluetooth()
        } else {
            Self::radio()
        }
    }

    /// A radio sending channels 1 to 4 in TAER order (Spektrum, JR).
    #[must_use]
    pub fn radio_taer() -> Self {
        let radio = Self::radio();
        Self {
            id: "radio-taer".into(),
            name: "Radio (TAER)".into(),
            throttle: AxisBinding {
                code: Some(HID_X),
                ..radio.throttle
            },
            roll: AxisBinding {
                code: Some(HID_X + 1),
                ..radio.roll
            },
            pitch: AxisBinding {
                code: Some(HID_X + 2),
                ..radio.pitch
            },
            yaw: radio.yaw,
            ..radio
        }
    }

    /// The four sticks from raw values (-1 to 1, up and right positive),
    /// read for each binding by `input`, each -1 to 1 after the deadzone and
    /// inversion; throttle is then mapped into the chosen range. A control
    /// not used reads centred.
    #[must_use]
    pub fn sticks(&self, input: impl Fn(&AxisBinding) -> f64) -> Sticks {
        let read = |binding: &AxisBinding| {
            let raw = binding.calibrated(input(binding).clamp(-1.0, 1.0));
            let value = if raw.abs() <= self.deadzone {
                0.0
            } else {
                // Rescaled so the edge of the deadzone is zero, not a step.
                raw.signum() * (raw.abs() - self.deadzone) / (1.0 - self.deadzone)
            };
            if binding.inverted { -value } else { value }
        };
        let control = |control: StickControl, binding: &AxisBinding, axis: FeelAxis| {
            if self.unused.contains(&control) {
                0.0
            } else {
                self.felt(axis, read(binding))
            }
        };
        let throttle = self.felt(FeelAxis::Throttle, read(&self.throttle));
        Sticks {
            roll: control(StickControl::Roll, &self.roll, FeelAxis::Roll),
            pitch: control(StickControl::Pitch, &self.pitch, FeelAxis::Pitch),
            yaw: control(StickControl::Yaw, &self.yaw, FeelAxis::Yaw),
            throttle: match self.throttle_range {
                ThrottleRange::FullTravel => throttle,
                ThrottleRange::CentreToTop => throttle.max(0.0) * 2.0 - 1.0,
            },
        }
    }

    /// A value after the deadzone and inversion through its axis's feel
    /// (S418): unchanged where the mapping sets none, or one it cannot use.
    #[must_use]
    pub fn felt(&self, axis: FeelAxis, value: f64) -> f64 {
        self.feel
            .iter()
            .find(|feel| feel.axis == axis)
            .filter(|feel| feel.problem().is_none())
            .map_or(value, |feel| feel.apply(value))
    }
}

/// The four mapped sticks, each -1 to 1; throttle -1 is minimum input,
/// 0 is 50% input, and 1 is maximum input, regardless of physical position.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sticks {
    pub roll: f64,
    pub pitch: f64,
    pub yaw: f64,
    pub throttle: f64,
}

impl Sticks {
    /// Roll, pitch and yaw centred; throttle anywhere.
    #[must_use]
    pub fn neutral(&self) -> bool {
        [self.roll, self.pitch, self.yaw]
            .iter()
            .all(|v| v.abs() < NEUTRAL)
    }

    /// Whether throttle is within `band` of where it rests: either side of
    /// the middle when it is `centred`, else anywhere up to `band` above
    /// the middle, a radio's resting bottom included.
    #[must_use]
    pub fn throttle_neutral(&self, centred: bool, band: f64) -> bool {
        if centred {
            self.throttle.abs() < band
        } else {
            self.throttle < band
        }
    }

    /// `MANUAL_CONTROL`'s x (pitch), y (roll), z (throttle, 0 to 1000,
    /// 500 at its midpoint) and r (yaw).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "clamped to the protocol's range first"
    )]
    pub fn manual_control(&self) -> [i16; 4] {
        let scale = |v: f64| (v.clamp(-1.0, 1.0) * 1000.0).round() as i16;
        let throttle = ((self.throttle.clamp(-1.0, 1.0) + 1.0) * 500.0).round() as i16;
        [
            scale(self.pitch),
            scale(self.roll),
            throttle,
            scale(self.yaw),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expresslrs_joystick_gets_the_layout_of_its_version_by_name() {
        // ExpressLRS 3 calls ESP32-BLE-Gamepad 0.5.2's setAxes(x, y, z, rZ,
        // rX, rY, ..), 4 calls 0.7.4's setAxes(x, y, z, rX, rY, rZ, ..),
        // both with channels 1, 2, 5, 6, 3, 4 (FB-88).
        let codes = |m: InputMapping| [m.roll, m.pitch, m.throttle, m.yaw].map(|b| b.code);
        let (x, y, rx, ry, rz) = (HID_X, HID_X + 1, HID_X + 3, HID_X + 4, HID_X + 5);
        for name in ["ELRS Joystick", "elrs joystick", " ELRS Joystick "] {
            let mapping = InputMapping::for_radio(name);
            assert_eq!(mapping.id, "expresslrs-4-bluetooth", "{name}");
            assert_eq!(codes(mapping), [x, y, ry, rz].map(Some), "{name}");
        }
        let mapping = InputMapping::for_radio("ExpressLRS Joystick");
        assert_eq!(mapping.id, "expresslrs-bluetooth");
        assert_eq!(codes(mapping), [x, y, rx, ry].map(Some));
        for name in [
            "ELRS Joystick 2",
            "My ELRS Joystick",
            "ESP32 BLE Gamepad",
            "TBS Joystick",
        ] {
            assert_eq!(InputMapping::for_radio(name).id, "radio", "{name}");
        }
    }

    #[test]
    fn throttle_centres_on_vehicles_that_drive_or_climb_both_ways() {
        for (autopilot, kind, centred) in [
            ("Ardupilotmega", "GroundRover", Some("forward / reverse")),
            ("Ardupilotmega", "SurfaceBoat", Some("forward / reverse")),
            ("Px4", "GroundRover", Some("forward / reverse")),
            ("Px4", "SurfaceBoat", Some("forward / reverse")),
            ("Ardupilotmega", "Submarine", Some("ascend / descend")),
            ("Ardupilotmega", "Airship", Some("ascend / descend")),
            ("Ardupilotmega", "Quadrotor", None),
            ("Ardupilotmega", "Helicopter", None),
            ("Ardupilotmega", "FixedWing", None),
            ("Ardupilotmega", "VtolQuadrotor", None),
            ("Px4", "Quadrotor", None),
            ("Px4", "FixedWing", None),
            ("Ardupilotmega", "Unknown", None),
            ("Generic", "GroundRover", Some("forward / reverse")),
            ("Generic", "Submarine", Some("ascend / descend")),
            ("Generic", "Airship", Some("ascend / descend")),
            ("Generic", "Quadrotor", None),
        ] {
            assert_eq!(
                centred_throttle(autopilot, kind),
                centred,
                "{autopilot} {kind}"
            );
        }
    }

    #[test]
    fn a_centred_throttle_is_neutral_only_in_the_middle_and_another_anywhere_below_it() {
        let at = |throttle| Sticks {
            throttle,
            ..Sticks::default()
        };
        for (throttle, centred, bottom) in [
            (-1.0, false, true),
            (-0.1, false, true),
            (-0.099, true, true),
            (0.0, true, true),
            (0.099, true, true),
            (0.1, false, false),
            (1.0, false, false),
        ] {
            assert_eq!(
                at(throttle).throttle_neutral(true, NEUTRAL),
                centred,
                "centred, at {throttle}"
            );
            assert_eq!(
                at(throttle).throttle_neutral(false, NEUTRAL),
                bottom,
                "resting at the bottom, at {throttle}"
            );
        }
    }

    /// What `ArduRover` makes of `MANUAL_CONTROL`'s z on a throttle channel
    /// (Rover 4.5.7 `Rover/GCS_Mavlink.cpp`, 4.6.3 the same, 4.7.2
    /// `Rover/GCS_MAVLink_Rover.cpp`: `manual_override(channel_throttle,
    /// z, 1000, 2000)`): `GCS_MAVLINK::manual_override`'s PWM, truncated,
    /// and `RC_Channel::pwm_to_angle_dz_trim`'s throttle percent around the
    /// trim with no dead zone, negated on a reversed channel.
    #[allow(clippy::cast_possible_truncation, reason = "as the C++ truncates")]
    fn ardurover(z: i16, [min, trim, max]: [i32; 3], reversed: bool) -> (i32, f64) {
        let pwm = (f64::from(min) + f64::from((max - min) * (i32::from(z) + 1000)) / 2000.0) as i32;
        let percent = if pwm > trim && max != trim {
            100.0 * f64::from(pwm - trim) / f64::from(max - trim)
        } else if pwm < trim && trim != min {
            100.0 * f64::from(pwm - trim) / f64::from(trim - min)
        } else {
            0.0
        };
        (pwm, if reversed { -percent } else { percent })
    }

    fn rover_throttle(values: &[(&str, f64)]) -> Result<RoverThrottle, String> {
        RoverThrottle::from_params(|name| {
            values
                .iter()
                .rev()
                .find(|(n, _)| *n == name)
                .map(|(_, value)| *value)
        })
    }

    /// The middle goes out as the trim's PWM wherever the trim is, the ends
    /// as min and max, as RC overrides put them; below the middle reverses
    /// only where the trim is above min, so a forward-only setup's lower
    /// half stops too, and above it drives forward unless the trim is max.
    #[test]
    #[allow(clippy::float_cmp, reason = "stop is exactly 0%")]
    fn manual_control_puts_a_rover_s_middle_on_its_throttle_trim() {
        for calibration @ [min, trim, max] in [
            [1100, 1500, 1900],
            [1100, 1460, 1900],
            [982, 1310, 2006],
            [1100, 1100, 1900],
            [1100, 1900, 1900],
        ] {
            let throttle = rover_throttle(&[
                ("RCMAP_THROTTLE", 3.0),
                ("RC3_MIN", f64::from(min)),
                ("RC3_TRIM", f64::from(trim)),
                ("RC3_MAX", f64::from(max)),
            ])
            .unwrap();
            let at = |input| ardurover(throttle.manual_z(input), calibration, false);
            assert_eq!(at(500), (trim, 0.0), "{calibration:?}: stop");
            assert_eq!(at(0).0, min, "{calibration:?}: bottom");
            assert_eq!(at(1000).0, max, "{calibration:?}: top");
            for input in [0, 250, 490] {
                let percent = at(input).1;
                if trim == min {
                    assert_eq!(
                        percent, 0.0,
                        "{calibration:?} at {input}: forward-only stops"
                    );
                } else {
                    assert!(percent < 0.0, "{calibration:?} at {input}: reverses");
                }
            }
            for input in [510, 750, 1000] {
                let percent = at(input).1;
                if trim == max {
                    assert_eq!(percent, 0.0, "{calibration:?} at {input}");
                } else {
                    assert!(percent > 0.0, "{calibration:?} at {input}: drives forward");
                }
            }
            // A reversed channel turns the direction around, as it does with
            // RC overrides; the trim is still stop.
            assert_eq!(
                ardurover(throttle.manual_z(500), calibration, true),
                (trim, 0.0)
            );
        }
        // The input is clamped to 0 to 1000, as the sticks are.
        let mid = rover_throttle(&[
            ("RCMAP_THROTTLE", 3.0),
            ("RC3_MIN", 1100.0),
            ("RC3_TRIM", 1500.0),
            ("RC3_MAX", 1900.0),
        ])
        .unwrap();
        assert_eq!([mid.manual_z(-5), mid.manual_z(1005)], [-1000, 1000]);
    }

    /// Nothing is assumed: each parameter must be there, the throttle map a
    /// channel, the range a PWM range and the trim within it, with the same
    /// limits as RC overrides; a remapped throttle reads its own channel.
    #[test]
    fn a_rover_s_throttle_calibration_needs_every_parameter_and_a_usable_range() {
        let good = [
            ("RCMAP_THROTTLE", 3.0),
            ("RC3_MIN", 1100.0),
            ("RC3_TRIM", 1500.0),
            ("RC3_MAX", 1900.0),
        ];
        assert_eq!(
            rover_throttle(&good),
            Ok(RoverThrottle {
                channel: 3,
                min: 1100.0,
                trim: 1500.0,
                max: 1900.0
            })
        );
        for missing in 0..good.len() {
            let some: Vec<_> = good
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != missing)
                .map(|(_, p)| *p)
                .collect();
            assert_eq!(
                rover_throttle(&some),
                Err(format!("{} is missing", good[missing].0))
            );
        }
        let with = |name: &'static str, value: f64| {
            let mut values = good.to_vec();
            values.push((name, value));
            rover_throttle(&values)
        };
        for (name, value, problem) in [
            ("RCMAP_THROTTLE", 0.0, "RCMAP_THROTTLE 0 is not a channel"),
            ("RCMAP_THROTTLE", 17.0, "RCMAP_THROTTLE 17 is not a channel"),
            ("RCMAP_THROTTLE", 2.5, "RCMAP_THROTTLE 2.5 is not a channel"),
            (
                "RCMAP_THROTTLE",
                f64::NAN,
                "RCMAP_THROTTLE NaN is not a channel",
            ),
            (
                "RC3_MIN",
                1900.0,
                "RC3_MIN 1900 to RC3_MAX 1900 is not a PWM range",
            ),
            (
                "RC3_MIN",
                2000.0,
                "RC3_MIN 2000 to RC3_MAX 1900 is not a PWM range",
            ),
            (
                "RC3_MIN",
                500.0,
                "RC3_MIN 500 to RC3_MAX 1900 is not a PWM range",
            ),
            (
                "RC3_MAX",
                2500.0,
                "RC3_MIN 1100 to RC3_MAX 2500 is not a PWM range",
            ),
            (
                "RC3_TRIM",
                1950.0,
                "RC3_TRIM 1950 is outside RC3_MIN to RC3_MAX",
            ),
            (
                "RC3_TRIM",
                1000.0,
                "RC3_TRIM 1000 is outside RC3_MIN to RC3_MAX",
            ),
            ("RC3_TRIM", f64::NAN, "RC3_TRIM is missing"),
            ("RC3_MAX", f64::INFINITY, "RC3_MAX is missing"),
        ] {
            assert_eq!(with(name, value), Err(problem.into()), "{name} {value}");
        }
        // Remapped to channel 2, its own calibration counts and channel 3's
        // does not.
        assert_eq!(
            with("RCMAP_THROTTLE", 2.0),
            Err("RC2_MIN is missing".into())
        );
        let mut remapped = good.to_vec();
        remapped.extend([
            ("RCMAP_THROTTLE", 2.0),
            ("RC2_MIN", 982.0),
            ("RC2_TRIM", 1310.0),
            ("RC2_MAX", 2006.0),
        ]);
        let throttle = rover_throttle(&remapped).unwrap();
        assert_eq!(throttle.channel, 2);
        assert_eq!(
            ardurover(throttle.manual_z(500), [982, 1310, 2006], false),
            (1310, 0.0)
        );
    }

    #[test]
    fn centre_to_top_throttle_uses_only_the_positive_half_after_deadzone_and_inversion() {
        let mut mapping = InputMapping::mode_2();
        mapping.throttle_range = ThrottleRange::CentreToTop;
        for (raw, expected) in [(-1.0, 0), (0.0, 0), (0.05, 0), (0.525, 500), (1.0, 1000)] {
            let sticks = mapping.sticks(|binding| {
                if binding.axis == GamepadAxis::LeftStickY {
                    raw
                } else {
                    0.0
                }
            });
            assert_eq!(sticks.manual_control(), [0, 0, expected, 0]);
            assert!(sticks.neutral());
        }
        mapping.throttle.inverted = true;
        assert_eq!(mapping.sticks(|_| -1.0).manual_control()[2], 1000);
        assert_eq!(mapping.sticks(|_| 1.0).manual_control()[2], 0);
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test round trips")]
    fn old_stored_mappings_keep_full_travel_and_new_ranges_round_trip() {
        let mut mapping = InputMapping::mode_2();
        let old = serde_json::to_value(&mapping).expect("mapping");
        assert!(old.get("throttleRange").is_none());
        let restored: InputMapping = serde_json::from_value(old).expect("old mapping");
        assert_eq!(restored.sticks(|_| 0.0).manual_control()[2], 500);
        mapping.throttle_range = ThrottleRange::CentreToTop;
        let stored = serde_json::to_value(&mapping).expect("mapping");
        let restored: InputMapping = serde_json::from_value(stored).expect("new mapping");
        assert_eq!(restored.sticks(|_| 0.0).manual_control()[2], 0);
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test round trips")]
    fn mappings_stored_before_held_actions_load_without_any_and_new_ones_round_trip() {
        let mut mapping = InputMapping::mode_2();
        let old = serde_json::to_value(&mapping).expect("mapping");
        assert!(old.get("actions").is_none(), "stored as before");
        let restored: InputMapping = serde_json::from_value(old).expect("old mapping");
        assert!(restored.actions.is_empty());
        mapping.actions = vec![
            ActionBinding {
                button: GamepadButton::RightTrigger,
                code: None,
                action: HeldAction::Open,
                shift: None,
            },
            ActionBinding {
                button: GamepadButton::LeftTrigger,
                code: Some(7),
                action: HeldAction::Close,
                shift: None,
            },
            ActionBinding {
                button: GamepadButton::North,
                code: None,
                action: HeldAction::Talk,
                shift: None,
            },
        ];
        let stored = serde_json::to_value(&mapping).expect("mapping");
        assert_eq!(
            stored["actions"],
            serde_json::json!([
                { "button": "right-trigger", "action": "open" },
                { "button": "left-trigger", "code": 7, "action": "close" },
                { "button": "north", "action": "talk" },
            ])
        );
        let restored: InputMapping = serde_json::from_value(stored).expect("new mapping");
        assert_eq!(restored, mapping);
        assert_eq!(
            restored
                .action(Some(GamepadButton::RightTrigger), None)
                .map(|b| b.action),
            Some(HeldAction::Open)
        );
        assert!(
            restored
                .action(Some(GamepadButton::LeftTrigger), None)
                .is_none(),
            "matched by its code"
        );
        assert_eq!(
            restored.action(None, Some(7)).map(|b| b.action),
            Some(HeldAction::Close)
        );
    }

    #[test]
    fn the_gimbal_pad_takes_only_the_direction_pad_buttons_that_do_nothing_else() {
        let held = |mapping: &InputMapping| -> Vec<(GamepadButton, HeldAction)> {
            mapping
                .actions
                .iter()
                .map(|binding| (binding.button, binding.action))
                .collect()
        };
        assert_eq!(
            held(&InputMapping::mode_2().with_gimbal_pad()),
            [
                (GamepadButton::DpadUp, HeldAction::GimbalUp),
                (GamepadButton::DpadDown, HeldAction::GimbalDown),
                (GamepadButton::DpadLeft, HeldAction::GimbalLeft),
                (GamepadButton::DpadRight, HeldAction::GimbalRight),
            ]
        );
        // Up sends a command and left is held to talk: both stay as they are.
        let mut taken = InputMapping::mode_2();
        taken.buttons.push(ButtonBinding {
            button: GamepadButton::DpadUp,
            code: None,
            command: Command::Rtl,
            hold_ms: None,
            gesture: ButtonGesture::Press,
            shift: None,
            repeat_ms: None,
        });
        taken.actions.push(ActionBinding {
            button: GamepadButton::DpadLeft,
            code: None,
            action: HeldAction::Talk,
            shift: None,
        });
        let padded = taken.clone().with_gimbal_pad();
        assert_eq!(padded.buttons, taken.buttons);
        assert_eq!(
            held(&padded),
            [
                (GamepadButton::DpadLeft, HeldAction::Talk),
                (GamepadButton::DpadDown, HeldAction::GimbalDown),
                (GamepadButton::DpadRight, HeldAction::GimbalRight),
            ]
        );
    }

    #[test]
    fn a_calibrated_radio_reads_its_measured_ends_as_full_travel() {
        let mut mapping = InputMapping::radio();
        // A throttle that bottoms out at -0.982 and tops out at 0.97.
        mapping.throttle.calibration = Some(AxisCalibration {
            min: -0.982,
            centre: -0.006,
            max: 0.97,
        });
        // A roll stick resting a little off the middle.
        mapping.roll.calibration = Some(AxisCalibration {
            min: -0.9,
            centre: 0.04,
            max: 0.95,
        });
        let read = |throttle: f64, roll: f64| {
            mapping.sticks(|b| match b.code {
                Some(0x1_0032) => throttle,
                Some(0x1_0030) => roll,
                _ => 0.0,
            })
        };
        assert_eq!(
            read(-0.982, 0.04).manual_control()[2],
            0,
            "throttle down is 0%"
        );
        assert_eq!(read(-1.0, 0.04).manual_control()[2], 0, "and beyond it");
        assert_eq!(read(0.97, 0.04).manual_control()[2], 1000, "up is 100%");
        assert_eq!(
            read(-0.982, 0.04).manual_control()[1],
            0,
            "roll at rest is centred"
        );
        assert_eq!(read(-0.982, 0.95).manual_control()[1], 1000);
        assert_eq!(read(-0.982, -0.9).manual_control()[1], -1000);
        // Uncalibrated, the same throttle reads 0.9%.
        assert_eq!(
            InputMapping::radio().sticks(|_| -0.982).manual_control()[2],
            9
        );
    }

    #[test]
    fn a_calibration_without_room_either_side_of_the_centre_is_ignored() {
        let binding = AxisBinding {
            calibration: Some(AxisCalibration {
                min: 0.5,
                centre: 0.5,
                max: 0.9,
            }),
            ..InputMapping::radio().roll
        };
        assert!(!binding.calibration.is_some_and(|c| c.is_valid()));
        assert!((binding.calibrated(0.3) - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn sticks_follow_the_mapping_deadzone_and_inversion() {
        let mut mapping = InputMapping::mode_2();
        let centred = mapping.sticks(|_| 0.03);
        assert!(centred.neutral());
        assert_eq!(centred.manual_control(), [0, 0, 500, 0]);
        let full = mapping.sticks(|binding| match binding.axis {
            GamepadAxis::RightStickY => 1.0,
            GamepadAxis::LeftStickY => -1.0,
            _ => 0.0,
        });
        assert!(!full.neutral());
        assert_eq!(full.manual_control(), [1000, 0, 0, 0]);
        let idle = mapping.sticks(|binding| match binding.axis {
            GamepadAxis::LeftStickY => -1.0,
            _ => 0.0,
        });
        assert!(idle.neutral(), "throttle is the operator's to choose");
        mapping.throttle.code = Some(0x1_0032);
        let by_code = mapping.sticks(|binding| match binding.code {
            Some(0x1_0032) => 1.0,
            _ => 0.0,
        });
        assert_eq!(by_code.manual_control()[2], 1000, "read by its code");
        mapping.throttle.code = None;
        mapping.pitch.inverted = true;
        assert_eq!(
            mapping
                .sticks(|b| if b.axis == GamepadAxis::RightStickY {
                    0.525
                } else {
                    0.0
                })
                .manual_control()[0],
            -500
        );
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test round trips")]
    fn controls_not_used_read_centred_and_need_no_input() {
        let mut mapping = InputMapping::radio();
        let old = serde_json::to_value(&mapping).expect("mapping");
        assert!(
            old.get("unused").is_none(),
            "stored as before when all used"
        );
        mapping.unused = vec![StickControl::Pitch, StickControl::Yaw];
        assert_eq!(
            mapping
                .stick_bindings()
                .filter_map(|b| b.code)
                .collect::<Vec<_>>(),
            [0x1_0030, 0x1_0032],
            "only roll's and throttle's inputs are read"
        );
        let sticks = mapping.sticks(|b| match b.code {
            Some(0x1_0030) => 0.5,
            Some(0x1_0032) => 1.0,
            _ => -1.0,
        });
        assert_eq!(sticks.manual_control(), [0, 474, 1000, 0]);
        let resting = mapping.sticks(|b| match b.code {
            Some(0x1_0030) => 0.0,
            _ => -1.0,
        });
        assert!(resting.neutral(), "pitch and yaw not used are centred");
        let stored = serde_json::to_value(&mapping).expect("mapping");
        assert_eq!(stored["unused"], serde_json::json!(["pitch", "yaw"]));
        let restored: InputMapping = serde_json::from_value(stored).expect("stored");
        assert_eq!(restored, mapping);
    }

    fn press(button: GamepadButton, command: Command) -> ButtonBinding {
        ButtonBinding {
            button,
            code: None,
            command,
            hold_ms: None,
            gesture: ButtonGesture::Press,
            shift: None,
            repeat_ms: None,
        }
    }

    const LB: ShiftButton = ShiftButton {
        button: GamepadButton::LeftTrigger,
        code: None,
    };

    fn shift_held(button: GamepadButton) -> ActionBinding {
        ActionBinding {
            button,
            code: None,
            action: HeldAction::Shift,
            shift: None,
        }
    }

    fn zoom_in() -> Command {
        Command::Camera {
            component_id: None,
            device_id: None,
            action: crate::domain::camera::CameraAction::ZoomStep { steps: 1 },
        }
    }

    /// A mapping stored before S418 loads as it was and is stored again
    /// the same: no gesture, Shift, repeat or feel appears in it.
    #[test]
    #[allow(clippy::expect_used, reason = "test round trips")]
    fn mappings_stored_before_gestures_and_feel_load_and_store_as_they_were() {
        let mut mapping = InputMapping::mode_2().with_gimbal_pad();
        mapping.buttons[0].hold_ms = Some(1_000);
        let old = serde_json::to_value(&mapping).expect("mapping");
        assert!(old.get("feel").is_none(), "feel absent");
        for binding in old["buttons"].as_array().expect("buttons") {
            for key in ["gesture", "shift", "repeatMs"] {
                assert!(binding.get(key).is_none(), "{key} absent from {binding}");
            }
        }
        for binding in old["actions"].as_array().expect("actions") {
            assert!(binding.get("shift").is_none(), "{binding}");
        }
        let restored: InputMapping = serde_json::from_value(old.clone()).expect("old mapping");
        assert_eq!(restored, mapping);
        assert_eq!(serde_json::to_value(&restored).expect("again"), old);
        assert!(restored.button_problem().is_none());
        // And new ones round trip.
        mapping.buttons.push(ButtonBinding {
            gesture: ButtonGesture::DoublePress,
            shift: Some(LB),
            ..press(GamepadButton::South, Command::Rtl)
        });
        mapping.buttons.push(ButtonBinding {
            repeat_ms: Some(250),
            ..press(GamepadButton::West, zoom_in())
        });
        mapping.actions.push(shift_held(GamepadButton::LeftTrigger));
        mapping.feel = vec![AxisFeel {
            axis: FeelAxis::Roll,
            expo: 0.4,
            rate: 0.8,
        }];
        let stored = serde_json::to_value(&mapping).expect("mapping");
        assert_eq!(
            stored["buttons"][2],
            serde_json::json!({
                "button": "south",
                "command": { "type": "rtl" },
                "gesture": "double-press",
                "shift": { "button": "left-trigger" },
            })
        );
        assert_eq!(stored["buttons"][3]["repeatMs"], 250);
        assert_eq!(
            stored["feel"],
            serde_json::json!([{ "axis": "roll", "expo": 0.4, "rate": 0.8 }])
        );
        let restored: InputMapping = serde_json::from_value(stored).expect("new mapping");
        assert_eq!(restored, mapping);
        // A feel written without its rate is at full rate.
        let feel: AxisFeel =
            serde_json::from_value(serde_json::json!({ "axis": "yaw", "expo": 0.2 }))
                .expect("feel");
        assert!((feel.rate - 1.0).abs() < f64::EPSILON);
    }

    /// Expo softens the sticks around the centre after the deadzone, and
    /// rate scales a full push; the ends and the centre stay where they are.
    #[test]
    fn expo_and_rate_shape_the_sticks_and_the_gimbal_after_the_deadzone() {
        let mut mapping = InputMapping::mode_2();
        mapping.deadzone = 0.0;
        mapping.feel = vec![
            AxisFeel {
                axis: FeelAxis::Roll,
                expo: 0.4,
                rate: 1.0,
            },
            AxisFeel {
                axis: FeelAxis::Pitch,
                expo: 0.0,
                rate: 0.5,
            },
        ];
        let at = |mapping: &InputMapping, value: f64| {
            mapping.sticks(|b| {
                if matches!(b.axis, GamepadAxis::RightStickX | GamepadAxis::RightStickY) {
                    value
                } else {
                    0.0
                }
            })
        };
        // Half a push of roll sends 0.6 * 0.5 + 0.4 * 0.125 = 0.35.
        assert_eq!(at(&mapping, 0.5).manual_control()[1], 350);
        assert_eq!(
            at(&mapping, -0.5).manual_control()[1],
            -350,
            "the same either way"
        );
        assert_eq!(
            at(&mapping, 1.0).manual_control()[1],
            1000,
            "a full push is still full"
        );
        assert_eq!(at(&mapping, 0.0).manual_control()[1], 0);
        // Pitch at half rate: a full push sends half.
        assert_eq!(at(&mapping, 1.0).manual_control()[0], 500);
        // After the deadzone: its edge is still zero, not a step.
        mapping.deadzone = 0.1;
        assert_eq!(at(&mapping, 0.1).manual_control()[1], 0);
        // The gimbal's axes too.
        mapping.gimbal = vec![GimbalAxisBinding {
            control: GimbalAxis::Yaw,
            input: AxisBinding {
                axis: GamepadAxis::RightZ,
                code: None,
                inverted: false,
                calibration: None,
            },
        }];
        mapping.deadzone = 0.0;
        mapping.feel.push(AxisFeel {
            axis: FeelAxis::GimbalYaw,
            expo: 1.0,
            rate: 0.5,
        });
        let yaw = mapping.gimbal_axes(|_| Some(0.5))[0];
        assert!((yaw - 0.0625).abs() < 1e-9, "{yaw}");
        // One it cannot use changes nothing.
        mapping.feel = vec![AxisFeel {
            axis: FeelAxis::Roll,
            expo: 2.0,
            rate: 1.0,
        }];
        assert_eq!(at(&mapping, 0.5).manual_control()[1], 500);
    }

    #[test]
    fn expo_and_rate_out_of_their_range_or_a_throttle_below_full_rate_are_refused() {
        let feel = |axis, expo, rate| AxisFeel { axis, expo, rate }.problem();
        assert!(feel(FeelAxis::Roll, 0.0, 1.0).is_none());
        assert!(feel(FeelAxis::GimbalPitch, 1.0, 0.1).is_none());
        assert!(feel(FeelAxis::Throttle, 0.5, 1.0).is_none());
        for (axis, expo, rate) in [
            (FeelAxis::Roll, -0.1, 1.0),
            (FeelAxis::Roll, 1.1, 1.0),
            (FeelAxis::Roll, f64::NAN, 1.0),
            (FeelAxis::Yaw, 0.0, 0.0),
            (FeelAxis::Yaw, 0.0, 1.5),
            (FeelAxis::Throttle, 0.0, 0.8),
        ] {
            assert!(feel(axis, expo, rate).is_some(), "{axis:?} {expo} {rate}");
        }
    }

    /// S12's checks reach gestures and Shift layers: one action for each
    /// gesture of an input on a layer, a Shift that does nothing else, a
    /// layer only of a Shift, and a repeat only of a lone step.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one refusal for each way a binding can clash"
    )]
    fn gestures_and_shift_layers_are_checked_as_s12_checks_bindings() {
        let mut mapping = InputMapping::mode_2();
        mapping.actions.push(shift_held(GamepadButton::LeftTrigger));
        let long = |command| ButtonBinding {
            hold_ms: Some(1_000),
            ..press(GamepadButton::South, command)
        };
        let double = |command| ButtonBinding {
            gesture: ButtonGesture::DoublePress,
            ..press(GamepadButton::South, command)
        };
        mapping.buttons.extend([
            press(GamepadButton::South, Command::Land),
            long(Command::Rtl),
            double(Command::Arm),
            ButtonBinding {
                shift: Some(LB),
                ..press(GamepadButton::South, Command::Rtl)
            },
            ButtonBinding {
                repeat_ms: Some(REPEAT_MIN_MS),
                ..press(GamepadButton::West, zoom_in())
            },
        ]);
        mapping.actions.push(ActionBinding {
            shift: Some(LB),
            ..shift_held(GamepadButton::DpadUp)
        });
        mapping.actions.last_mut().expect("added").action = HeldAction::GimbalUp;
        assert_eq!(mapping.button_problem(), None);
        assert_eq!(
            mapping.button(Some(GamepadButton::South), None),
            Some(&press(GamepadButton::South, Command::Land)),
            "a plain press is its press without a Shift"
        );
        assert_eq!(
            mapping
                .action_in(Some(GamepadButton::DpadUp), None, Some(LB))
                .map(|a| a.action),
            Some(HeldAction::GimbalUp)
        );
        assert!(
            mapping.action(Some(GamepadButton::DpadUp), None).is_none(),
            "Up's held action is on the Shift's layer only"
        );

        let refused = |change: &dyn Fn(&mut InputMapping)| {
            let mut wrong = mapping.clone();
            change(&mut wrong);
            wrong.button_problem()
        };
        for (why, change) in [
            (
                "two long presses",
                &(|m: &mut InputMapping| m.buttons.push(long(Command::Land)))
                    as &dyn Fn(&mut InputMapping),
            ),
            ("two double presses", &|m| {
                m.buttons.push(double(Command::Rtl));
            }),
            ("Talk on a Shift's layer, which the window reads", &|m| {
                m.actions.push(ActionBinding {
                    action: HeldAction::Talk,
                    shift: Some(LB),
                    ..shift_held(GamepadButton::North)
                });
            }),
            ("Navigate on a Shift's layer", &|m| {
                m.actions.push(ActionBinding {
                    action: HeldAction::Navigate,
                    shift: Some(LB),
                    ..shift_held(GamepadButton::Start)
                });
            }),
            ("a held double press", &|m| {
                m.buttons.push(ButtonBinding {
                    hold_ms: Some(1_000),
                    ..double(Command::Rtl)
                });
            }),
            ("a Shift with a command", &|m| {
                m.buttons
                    .push(press(GamepadButton::LeftTrigger, Command::Rtl));
            }),
            ("a Shift held for something else", &|m| {
                m.actions.push(ActionBinding {
                    action: HeldAction::Talk,
                    ..shift_held(GamepadButton::LeftTrigger)
                });
            }),
            ("a Shift on a Shift", &|m| {
                m.actions.push(ActionBinding {
                    shift: Some(LB),
                    ..shift_held(GamepadButton::RightTrigger)
                });
            }),
            ("a layer of a button that is no Shift", &|m| {
                m.buttons.push(ButtonBinding {
                    shift: Some(ShiftButton {
                        button: GamepadButton::RightTrigger,
                        code: None,
                    }),
                    ..press(GamepadButton::North, Command::Rtl)
                });
            }),
            ("a command and a held action on one layer", &|m| {
                m.buttons.push(ButtonBinding {
                    shift: Some(LB),
                    ..press(GamepadButton::DpadUp, Command::Rtl)
                });
            }),
            ("a repeat of what is not a step", &|m| {
                m.buttons.push(ButtonBinding {
                    repeat_ms: Some(250),
                    ..press(GamepadButton::North, Command::Arm)
                });
                m.buttons
                    .retain(|b| !(b.button == GamepadButton::North && b.command != Command::Arm));
            }),
            ("a repeat faster than the pilot tick", &|m| {
                m.buttons.iter_mut().for_each(|b| {
                    if b.repeat_ms.is_some() {
                        b.repeat_ms = Some(REPEAT_MIN_MS - 1);
                    }
                });
            }),
            ("a repeat beside a long press", &|m| {
                m.buttons.push(ButtonBinding {
                    hold_ms: Some(1_000),
                    ..press(GamepadButton::West, Command::Rtl)
                });
            }),
        ] {
            assert!(refused(change).is_some(), "{why}");
        }
        // The same gestures on another layer, or on another button, are fine.
        assert_eq!(
            refused(&|m| m.buttons.push(ButtonBinding {
                shift: Some(LB),
                ..long(Command::Land)
            })),
            None
        );
    }
}

//! Controllers and input mappings on the wire (ADR-0005).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::command::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    /// One vehicle; the client's selection never moves it.
    Pinned,
    /// The claiming client's selected vehicle, switched only with the
    /// sticks centred and the change confirmed.
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Controller {
    pub id: String,
    pub name: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    pub mode: TargetMode,
    /// The vehicle it drives while claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Where the selection moved; the switch waits for neutral sticks and
    /// `controller/setTarget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_target: Option<String>,
    pub mapping_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
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
    /// Around the centre, as a fraction of full travel, read as zero.
    pub deadzone: f64,
    pub buttons: Vec<ButtonBinding>,
    /// Commands sent when an axis moves into a position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<PositionBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rc_channels: Vec<RcAxisBinding>,
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

/// Changes what a claimed controller drives: the mode, and for `pinned`
/// the vehicle. Moving to another vehicle needs the sticks centred; this is
/// also how a pending switch is confirmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerSetTargetParams {
    pub id: String,
    pub mode: TargetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
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
    pub deadzone: f64,
    pub buttons: Vec<ButtonBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<PositionBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rc_channels: Vec<RcAxisBinding>,
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
}

/// Every plugged-in controller's raw input, for testing a gamepad and a
/// mapping. Changes nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControllerInputsResult {
    pub controllers: Vec<ControllerInputs>,
}

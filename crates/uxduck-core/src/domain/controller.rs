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
    pub version: u64,
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
    /// press itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<u32>,
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
}

/// What a held button does: holds the gripper open or closed (S263), or
/// Talk, held to speak to the agent in the window that claimed the
/// controller and let go to send (S21). The daemon only keeps Talk; the
/// window reads the button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeldAction {
    Open,
    Close,
    Talk,
}

impl HeldAction {
    /// The gripper action it asks for; none for Talk.
    #[must_use]
    pub const fn gripper(self) -> Option<GripperAction> {
        match self {
            Self::Open => Some(GripperAction::Open),
            Self::Close => Some(GripperAction::Close),
            Self::Talk => None,
        }
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
    /// Buttons held for the gripper (S263).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionBinding>,
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

    /// What a button does: matched by its code when the binding has one,
    /// else by its gamepad name.
    #[must_use]
    pub fn button(
        &self,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) -> Option<&ButtonBinding> {
        self.buttons.iter().find(|b| match b.code {
            Some(wanted) => code == Some(wanted),
            None => button == Some(b.button),
        })
    }

    /// The held action a button is bound to, if any.
    #[must_use]
    pub fn action(
        &self,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) -> Option<&ActionBinding> {
        self.actions.iter().find(|b| b.matches(button, code))
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
                },
                ButtonBinding {
                    button: GamepadButton::North,
                    code: None,
                    command: Command::SetMode {
                        mode: "LOITER".into(),
                    },
                    hold_ms: None,
                },
            ],
            positions: Vec::new(),
            rc_channels: Vec::new(),
            unused: Vec::new(),
            actions: Vec::new(),
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
        let control = |control: StickControl, binding: &AxisBinding| {
            if self.unused.contains(&control) {
                0.0
            } else {
                read(binding)
            }
        };
        Sticks {
            roll: control(StickControl::Roll, &self.roll),
            pitch: control(StickControl::Pitch, &self.pitch),
            yaw: control(StickControl::Yaw, &self.yaw),
            throttle: match self.throttle_range {
                ThrottleRange::FullTravel => read(&self.throttle),
                ThrottleRange::CentreToTop => read(&self.throttle).max(0.0) * 2.0 - 1.0,
            },
        }
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
            },
            ActionBinding {
                button: GamepadButton::LeftTrigger,
                code: Some(7),
                action: HeldAction::Close,
            },
            ActionBinding {
                button: GamepadButton::North,
                code: None,
                action: HeldAction::Talk,
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
}

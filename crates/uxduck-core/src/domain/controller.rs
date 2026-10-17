//! Controllers and input mappings (ADR-0005 controllers and multi-vehicle
//! safety). A controller is an input device the daemon owns. It drives a
//! vehicle only while an operator client has claimed it, and it changes
//! target only with roll, pitch and yaw centred and the change confirmed.
//! Throttle is left where the operator holds it: hover, idle or full are all
//! theirs to choose, so it never blocks a claim or a switch.

use serde::{Deserialize, Serialize};

use crate::domain::command::Command;

/// What a claimed controller drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetMode {
    /// One vehicle; the client's selection never moves it.
    Pinned,
    /// The claiming client's selected vehicle, switched only at neutral and
    /// with confirmation.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// and a confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_target: Option<String>,
    pub mapping_id: String,
    pub version: u64,
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
}

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
    pub version: u64,
}

/// How far from the centre roll, pitch and yaw must be, at most, to count
/// as neutral for claiming and switching. Throttle is not judged.
pub const NEUTRAL: f64 = 0.1;

/// The HID code of a joystick's X input (Generic Desktop page, usage 0x30);
/// Y, Z and Rx follow it.
const HID_X: u32 = 0x1_0030;

impl InputMapping {
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
                },
                ButtonBinding {
                    button: GamepadButton::North,
                    code: None,
                    command: Command::SetMode {
                        mode: "LOITER".into(),
                    },
                },
            ],
            positions: Vec::new(),
            rc_channels: Vec::new(),
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

    /// An `ExpressLRS` transmitter as a Bluetooth joystick: it puts channels
    /// 1 to 4 on X, Y, Rx and Ry, and its first switches on Z and Rz.
    #[must_use]
    pub fn expresslrs_bluetooth() -> Self {
        let radio = Self::radio();
        Self {
            id: "expresslrs-bluetooth".into(),
            name: "ExpressLRS Bluetooth".into(),
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

    /// The built-in mapping that fits a controller whose layout the
    /// platform does not know, by the name it gives.
    #[must_use]
    pub fn for_radio(name: &str) -> Self {
        if name.to_lowercase().contains("expresslrs") {
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
    /// inversion; throttle is then mapped into the chosen range.
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
        Sticks {
            roll: read(&self.roll),
            pitch: read(&self.pitch),
            yaw: read(&self.yaw),
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
}

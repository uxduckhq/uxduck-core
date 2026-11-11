//! A vehicle's gripper (S263): one single-axis claw, such as Blue Robotics'
//! Newton, on an `ArduSub` actuator. Its PWM asks for movement, not an
//! aperture: one end of the output's range closes it, the other opens it and
//! the middle stops it. Nothing here measures where the jaws are.
//!
//! The operator says which actuator is the gripper and which end opens it.
//! The rest is read from the vehicle's parameters every time: the output the
//! actuator drives (`SERVOn_FUNCTION`), the joystick button slots set to its
//! momentary functions (`BTNn_FUNCTION`) and the output's limits. Held
//! buttons travel in `MANUAL_CONTROL`'s button fields, merged into the pilot
//! input already being sent (ADR-0005).
//!
//! Verified in `ArduSub` SITL (4.7.2 and UXDuck's pinned build `d5420ff9`,
//! 2026-10-31): a momentary minimum or maximum drives `SERVOn_MIN` or
//! `SERVOn_MAX` while its bit is set, and the frame that clears the bit
//! returns the output to the middle of the two, not to `SERVOn_TRIM`.
//! `SERVOn_REVERSED` does not change it. Without a frame clearing the bit
//! (a lost link) the output keeps driving, even after the pilot-input failsafe
//! disarms. An output given an actuator function without a reboot drives
//! `SERVOn_MIN` until a button is released.

use serde::{Deserialize, Serialize};

/// The end of the output's range that opens the claw. Newton opens above
/// its 1500 µs neutral.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GripperEnd {
    #[default]
    Max,
    Min,
}

impl GripperEnd {
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::Max => Self::Min,
            Self::Min => Self::Max,
        }
    }
}

/// What the operator said about a vehicle's gripper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GripperSetup {
    /// `ArduSub`'s Actuator 1 to 6.
    pub actuator: u8,
    pub opens_at: GripperEnd,
}

/// The two held actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GripperAction {
    Open,
    Close,
}

/// `ArduSub` actuators 1 to 6.
pub const ACTUATORS: std::ops::RangeInclusive<u8> = 1..=6;

/// `SERVOn_FUNCTION` for Actuator 1; Actuator 2 to 6 follow it.
pub const ACTUATOR_FUNCTION: u8 = 184;

/// The outputs a gripper may be on.
pub const OUTPUTS: std::ops::RangeInclusive<u8> = 1..=16;

/// `BTNn_FUNCTION` slots `MANUAL_CONTROL` carries: 0 to 15 in `buttons`, 16
/// to 31 in `buttons2` (verified in SITL for 4.7.2 and `d5420ff9`).
pub const BUTTON_SLOTS: std::ops::RangeInclusive<u8> = 0..=31;

/// `AP_JSButton`'s momentary minimum and maximum for each actuator.
#[must_use]
pub fn momentary(actuator: u8, end: GripperEnd) -> Option<u8> {
    let (min, max) = match actuator {
        1 => (76, 77),
        2 => (80, 81),
        3 => (84, 85),
        4 => (117, 118),
        5 => (126, 127),
        6 => (135, 136),
        _ => return None,
    };
    Some(match end {
        GripperEnd::Min => min,
        GripperEnd::Max => max,
    })
}

/// Where release returns the output: the middle of its limits.
pub const NEWTON_NEUTRAL_US: u16 = 1500;

/// How far the middle of the limits may be from Newton's neutral before
/// setup warns that release may not stop it.
pub const NEUTRAL_TOLERANCE_US: u16 = 25;

/// A gripper's wiring as its parameters say now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GripperWiring {
    pub actuator: u8,
    pub opens_at: GripperEnd,
    pub output: u8,
    pub open_button: u8,
    pub close_button: u8,
    pub min_us: u16,
    pub max_us: u16,
}

impl GripperWiring {
    /// Where release returns the output.
    #[must_use]
    pub fn neutral_us(&self) -> u16 {
        u16::midpoint(self.min_us, self.max_us)
    }

    /// The `MANUAL_CONTROL` button bits for an action.
    #[must_use]
    pub fn buttons(&self, action: Option<GripperAction>) -> u32 {
        match action {
            Some(GripperAction::Open) => 1 << self.open_button,
            Some(GripperAction::Close) => 1 << self.close_button,
            None => 0,
        }
    }
}

/// Why a gripper cannot be used, in the operator's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GripperProblem {
    /// Not an `ArduSub` vehicle.
    NotSub,
    /// Its parameters are not read yet.
    NoParameters,
    /// No `ACTUATOR1_INC`: an `ArduSub` from before actuator functions,
    /// whose button functions drove fixed outputs. Not verified, so not
    /// supported.
    NoActuators,
    /// The setup names an actuator outside 1 to 6.
    BadActuator,
    /// No output has the actuator's function.
    NoOutput,
    /// More than one output has it.
    SeveralOutputs(Vec<u8>),
    /// No button slot has the momentary function for this end.
    NoButton(GripperEnd),
    /// The slot is beyond what this firmware lists (`BTNn_FUNCTION` missing).
    ButtonNotListed(u8),
    /// The output's limits are missing or not in order.
    BadLimits,
}

impl GripperProblem {
    #[must_use]
    pub fn message(&self, setup: Option<&GripperSetup>) -> String {
        let actuator = setup.map_or(1, |s| s.actuator);
        match self {
            Self::NotSub => "Gripper control is for ArduSub vehicles".into(),
            Self::NoParameters => "Read the vehicle's parameters first".into(),
            Self::NoActuators => "This ArduSub has no actuator functions (no ACTUATOR1_INC). Update ArduSub to 4.7 or later. UXDuck does not drive the older fixed servo buttons".into(),
            Self::BadActuator => "Choose Actuator 1 to 6".into(),
            Self::NoOutput => format!("No output is set to Actuator {actuator} (SERVOn_FUNCTION {})", ACTUATOR_FUNCTION + actuator - 1),
            Self::SeveralOutputs(outputs) => format!(
                "Actuator {actuator} is on more than one output ({}). Keep one",
                outputs.iter().map(|o| format!("SERVO{o}")).collect::<Vec<_>>().join(", ")
            ),
            Self::NoButton(end) => format!(
                "No joystick button slot is set to Actuator {actuator}'s momentary {} (BTNn_FUNCTION {})",
                match end {
                    GripperEnd::Min => "minimum",
                    GripperEnd::Max => "maximum",
                },
                momentary(actuator, *end).unwrap_or(0)
            ),
            Self::ButtonNotListed(slot) => format!("This firmware has no BTN{slot}_FUNCTION"),
            Self::BadLimits => "The output's SERVOn_MIN and SERVOn_MAX are missing or out of order".into(),
        }
    }
}

/// The gripper's wiring from the setup and the vehicle's parameters, read
/// by name, once they have been read (else `GripperProblem::NoParameters`
/// is the caller's to report). The lowest slot wins where several have the
/// same function.
///
/// # Errors
/// Every problem found, in the order setup fixes them.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "parameter values are whole numbers checked against their ranges first"
)]
pub fn wiring(
    setup: &GripperSetup,
    param: impl Fn(&str) -> Option<f64>,
) -> Result<GripperWiring, Vec<GripperProblem>> {
    if !ACTUATORS.contains(&setup.actuator) {
        return Err(vec![GripperProblem::BadActuator]);
    }
    if param("ACTUATOR1_INC").is_none() {
        return Err(vec![GripperProblem::NoActuators]);
    }
    let mut problems = Vec::new();
    let function = f64::from(ACTUATOR_FUNCTION + setup.actuator - 1);
    let outputs: Vec<u8> = OUTPUTS
        .filter(|n| param(&format!("SERVO{n}_FUNCTION")) == Some(function))
        .collect();
    let output = match outputs.as_slice() {
        [] => {
            problems.push(GripperProblem::NoOutput);
            None
        }
        [one] => Some(*one),
        several => {
            problems.push(GripperProblem::SeveralOutputs(several.to_vec()));
            None
        }
    };
    let slot = |end: GripperEnd| {
        let wanted = f64::from(momentary(setup.actuator, end)?);
        BUTTON_SLOTS
            .clone()
            .find(|n| param(&format!("BTN{n}_FUNCTION")) == Some(wanted))
    };
    let open_button = slot(setup.opens_at);
    let close_button = slot(setup.opens_at.other());
    if open_button.is_none() {
        problems.push(GripperProblem::NoButton(setup.opens_at));
    }
    if close_button.is_none() {
        problems.push(GripperProblem::NoButton(setup.opens_at.other()));
    }
    let limits = output.and_then(|n| {
        let read = |name: &str| {
            param(&format!("SERVO{n}_{name}"))
                .filter(|v| v.is_finite() && (500.0..=2500.0).contains(v))
                .map(|v| v.round() as u16)
        };
        Some((read("MIN")?, read("MAX")?)).filter(|(min, max)| min < max)
    });
    if output.is_some() && limits.is_none() {
        problems.push(GripperProblem::BadLimits);
    }
    match (output, open_button, close_button, limits) {
        (Some(output), Some(open_button), Some(close_button), Some((min_us, max_us)))
            if problems.is_empty() =>
        {
            Ok(GripperWiring {
                actuator: setup.actuator,
                opens_at: setup.opens_at,
                output,
                open_button,
                close_button,
                min_us,
                max_us,
            })
        }
        _ => Err(problems),
    }
}

/// Outputs with no function, where a gripper may go without displacing a
/// motor, lights or a camera mount.
#[must_use]
pub fn free_outputs(param: impl Fn(&str) -> Option<f64>) -> Vec<u8> {
    OUTPUTS
        .filter(|n| param(&format!("SERVO{n}_FUNCTION")) == Some(0.0))
        .collect()
}

/// Button slots with no function, plain or shifted, so a gripper takes none
/// of the vehicle's existing button functions.
#[must_use]
pub fn free_buttons(param: impl Fn(&str) -> Option<f64>) -> Vec<u8> {
    BUTTON_SLOTS
        .filter(|n| {
            param(&format!("BTN{n}_FUNCTION")) == Some(0.0)
                && param(&format!("BTN{n}_SFUNCTION")).is_none_or(|v| v == 0.0)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests")]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn sub(extra: &[(&str, f64)]) -> BTreeMap<String, f64> {
        let mut params: BTreeMap<String, f64> = [
            ("ACTUATOR1_INC", 0.01),
            ("SERVO9_FUNCTION", 181.0),
            ("SERVO10_FUNCTION", 7.0),
            ("SERVO11_FUNCTION", 0.0),
            ("SERVO11_MIN", 1100.0),
            ("SERVO11_MAX", 1900.0),
            ("BTN0_FUNCTION", 0.0),
            ("BTN1_FUNCTION", 6.0),
            ("BTN15_FUNCTION", 0.0),
            ("BTN16_FUNCTION", 0.0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        for (k, v) in extra {
            params.insert((*k).to_owned(), *v);
        }
        params
    }

    fn read(params: &BTreeMap<String, f64>) -> impl Fn(&str) -> Option<f64> + '_ {
        |name| params.get(name).copied()
    }

    const NEWTON: GripperSetup = GripperSetup {
        actuator: 1,
        opens_at: GripperEnd::Max,
    };

    #[test]
    fn a_newton_on_actuator_1_resolves_its_output_slots_and_neutral() {
        let params = sub(&[
            ("SERVO11_FUNCTION", 184.0),
            ("BTN0_FUNCTION", 76.0),
            ("BTN15_FUNCTION", 77.0),
        ]);
        let wiring = wiring(&NEWTON, read(&params)).expect("wired");
        assert_eq!(
            (wiring.output, wiring.open_button, wiring.close_button),
            (11, 15, 0)
        );
        assert_eq!(wiring.neutral_us(), 1500);
        assert_eq!(wiring.buttons(Some(GripperAction::Open)), 1 << 15);
        assert_eq!(wiring.buttons(Some(GripperAction::Close)), 1);
        assert_eq!(wiring.buttons(None), 0);
        let reversed = GripperSetup {
            opens_at: GripperEnd::Min,
            ..NEWTON
        };
        let wiring = super::wiring(&reversed, read(&params)).expect("wired");
        assert_eq!((wiring.open_button, wiring.close_button), (0, 15));
    }

    #[test]
    fn slots_in_buttons2_and_other_actuators_resolve() {
        let params = sub(&[
            ("SERVO11_FUNCTION", 187.0),
            ("BTN16_FUNCTION", 118.0),
            ("BTN0_FUNCTION", 117.0),
        ]);
        let setup = GripperSetup {
            actuator: 4,
            opens_at: GripperEnd::Max,
        };
        let wiring = wiring(&setup, read(&params)).expect("wired");
        assert_eq!((wiring.open_button, wiring.close_button), (16, 0));
        assert_eq!(wiring.buttons(Some(GripperAction::Open)), 1 << 16);
    }

    #[test]
    fn every_missing_piece_is_reported() {
        let params = sub(&[]);
        assert_eq!(
            wiring(&NEWTON, read(&params)),
            Err(vec![
                GripperProblem::NoOutput,
                GripperProblem::NoButton(GripperEnd::Max),
                GripperProblem::NoButton(GripperEnd::Min),
            ])
        );
        let twice = sub(&[
            ("SERVO11_FUNCTION", 184.0),
            ("SERVO12_FUNCTION", 184.0),
            ("BTN0_FUNCTION", 76.0),
            ("BTN15_FUNCTION", 77.0),
        ]);
        assert_eq!(
            wiring(&NEWTON, read(&twice)),
            Err(vec![GripperProblem::SeveralOutputs(vec![11, 12])])
        );
        let old = sub(&[])
            .into_iter()
            .filter(|(k, _)| k != "ACTUATOR1_INC")
            .collect();
        assert_eq!(
            wiring(&NEWTON, read(&old)),
            Err(vec![GripperProblem::NoActuators])
        );
        let bad = GripperSetup {
            actuator: 7,
            ..NEWTON
        };
        assert_eq!(
            wiring(&bad, read(&params)),
            Err(vec![GripperProblem::BadActuator])
        );
        let limits = sub(&[
            ("SERVO11_FUNCTION", 184.0),
            ("SERVO11_MIN", 1900.0),
            ("SERVO11_MAX", 1100.0),
            ("BTN0_FUNCTION", 76.0),
            ("BTN15_FUNCTION", 77.0),
        ]);
        assert_eq!(
            wiring(&NEWTON, read(&limits)),
            Err(vec![GripperProblem::BadLimits])
        );
    }

    #[test]
    fn free_outputs_and_buttons_leave_assigned_ones_alone() {
        let params = sub(&[("BTN15_SFUNCTION", 22.0)]);
        assert_eq!(free_outputs(read(&params)), vec![11]);
        assert_eq!(free_buttons(read(&params)), vec![0, 16]);
    }

    #[test]
    fn momentary_functions_match_ap_jsbutton() {
        assert_eq!(momentary(1, GripperEnd::Min), Some(76));
        assert_eq!(momentary(1, GripperEnd::Max), Some(77));
        assert_eq!(momentary(3, GripperEnd::Max), Some(85));
        assert_eq!(momentary(6, GripperEnd::Max), Some(136));
        assert_eq!(momentary(0, GripperEnd::Max), None);
    }

    #[test]
    fn the_setup_round_trips() {
        let json = serde_json::to_value(NEWTON).expect("setup");
        assert_eq!(json, serde_json::json!({ "actuator": 1, "opensAt": "max" }));
        let back: GripperSetup = serde_json::from_value(json).expect("setup");
        assert_eq!(back, NEWTON);
    }
}

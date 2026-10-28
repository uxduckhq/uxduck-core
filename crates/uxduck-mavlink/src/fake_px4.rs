//! The fake vehicle as PX4 (S191): it heartbeats as `MAV_AUTOPILOT_PX4` and
//! calibrates as PX4 1.17 does, saying how in `[cal]` status text, so the
//! Setup tab, the engine and the CLI can be tested without SITL. Its lines
//! are PX4's own wording, as recorded from PX4 1.17.0 SITL
//! (`tests/fixtures/px4_cal/`).
//!
//! `MAV_CMD_PREFLIGHT_CALIBRATION` (241), answered at once: parameter 5 at
//! 1 calibrates the accelerometers and parameter 2 at 1 the compass, each
//! on six sides that it finds itself, as if a person turned it onto each
//! in turn, one every few seconds; parameter 5 at 2 is level; parameter 6
//! at 2 the airspeed sensor, zeroed then blown into; parameter 4 at 1 the
//! radio, which only stops it using its radio; every parameter 0 cancels
//! one running, or ends the radio's. `MAV_CMD_FIXED_MAG_CAL_YAW` (42006)
//! calibrates from the heading given. `MAV_CMD_ACTUATOR_TEST` (310) drives
//! motor n (`ACTUATOR_OUTPUT_FUNCTION` n) at its value for its time, at most
//! 3 s; a time of 0 stops it; refused (`DENIED`) while armed or with
//! `COM_MOT_TEST_EN` 0. For tests and `uxduck dev fake-vehicle --px4`.

use crate::px4_cal::Side;

/// How long it takes to find a side and measure it.
const SIDE_S: f64 = 2.0;
/// How long level and the airspeed's steps take.
const STEP_S: f64 = 1.0;
/// PX4's longest actuator test.
const ACTUATOR_MAX_S: f64 = 3.0;

/// The order it is turned onto its sides: level first, as a person picks
/// it up, then the others.
const ORDER: [Side; 6] = [
    Side::Down,
    Side::Front,
    Side::Back,
    Side::Left,
    Side::Right,
    Side::Up,
];

/// The device id PX4 SITL gives its accelerometer, once calibrated.
pub const ACCEL_ID: f32 = 1_310_988.0;

/// A calibration it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Accel,
    Mag,
    Level,
    Airspeed,
}

impl Kind {
    const fn word(self) -> &'static str {
        match self {
            Self::Accel => "accel",
            Self::Mag => "mag",
            Self::Level => "level",
            Self::Airspeed => "airspeed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Running {
    kind: Kind,
    from_s: f64,
    /// Lines said so far, by step: where it is in its script.
    said: usize,
}

/// What the fake PX4 does or says, for the fake vehicle to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A `STATUSTEXT`: severity and text.
    Say(u8, String),
    /// A parameter it changed by calibrating.
    Set(&'static str, f32),
}

/// The fake vehicle's PX4.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FakePx4 {
    /// A plane, with an airspeed sensor.
    pub plane: bool,
    /// The accelerometer calibration fails after its second side, as PX4
    /// does when it is not turned onto another side in time.
    pub accel_fails: bool,
    running: Option<Running>,
    /// Calibrating its radio: it ignores its radio.
    pub radio_calibrating: bool,
    /// A motor test: motor n, its output in microseconds, until when.
    actuator: Option<(u8, u16, f64)>,
}

/// One line it says while a calibration runs, `at_s` into it.
struct Line {
    at_s: f64,
    severity: u8,
    text: String,
    set: Option<(&'static str, f32)>,
}

fn line(at_s: f64, text: impl Into<String>) -> Line {
    Line {
        at_s,
        severity: 6,
        text: text.into(),
        set: None,
    }
}

/// The sides not done yet, as PX4 lists them, in its order.
fn pending(done: usize) -> String {
    let left: Vec<&str> = Side::ALL
        .iter()
        .filter(|s| !ORDER[..done].contains(s))
        .map(|s| s.word())
        .collect();
    format!("[cal] pending: {}", left.join(" "))
}

impl FakePx4 {
    /// What a calibration says, in order, with when.
    #[allow(clippy::too_many_lines, reason = "one script per calibration")]
    fn script(&self, kind: Kind) -> Vec<Line> {
        let mut lines = vec![line(
            0.0,
            format!("[cal] calibration started: 2 {}", kind.word()),
        )];
        match kind {
            Kind::Accel | Kind::Mag => {
                let sides = if kind == Kind::Accel && self.accel_fails {
                    2
                } else {
                    ORDER.len()
                };
                for (i, side) in ORDER.iter().take(sides).enumerate() {
                    #[allow(clippy::cast_precision_loss, reason = "six sides")]
                    let at = i as f64 * SIDE_S;
                    let word = side.word();
                    lines.push(line(at + 0.05, pending(i)));
                    lines.push(line(at + 0.1, "[cal] hold vehicle still on a pending side"));
                    lines.push(line(at + 0.5, format!("[cal] {word} orientation detected")));
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "a percentage"
                    )]
                    let progress = ((i + 1) * 100 / ORDER.len()) as u32;
                    if kind == Kind::Accel {
                        lines.push(line(
                            at + 0.6,
                            format!("[cal] Hold still, measuring {word} side"),
                        ));
                    } else {
                        lines.push(line(at + 0.6, "[cal] Rotate vehicle"));
                        let half = (i * 100 + 50) / ORDER.len();
                        lines.push(line(
                            at + 1.2,
                            format!("[cal] {word} side calibration: progress <{half}>"),
                        ));
                    }
                    lines.push(line(at + 1.8, format!("[cal] progress <{progress}>")));
                    lines.push(line(
                        at + 1.85,
                        format!("[cal] {word} side done, rotate to a different side"),
                    ));
                }
                #[allow(clippy::cast_precision_loss, reason = "six sides")]
                let end = sides as f64 * SIDE_S;
                if sides < ORDER.len() {
                    lines.push(line(end, pending(sides)));
                    lines.push(Line {
                        severity: 2,
                        ..line(end + 0.5, "[cal] calibration failed: timeout: no motion")
                    });
                    lines.push(Line {
                        severity: 2,
                        ..line(
                            end + 0.55,
                            format!("[cal] calibration failed: {}", kind.word()),
                        )
                    });
                } else {
                    let set = if kind == Kind::Accel {
                        ("CAL_ACC0_ID", ACCEL_ID)
                    } else {
                        ("CAL_MAG0_XOFF", 0.012)
                    };
                    lines.push(Line {
                        set: Some(set),
                        ..line(end, format!("[cal] calibration done: {}", kind.word()))
                    });
                }
            }
            Kind::Level => {
                for (i, progress) in [0, 20, 40, 60, 80, 100].iter().enumerate() {
                    #[allow(clippy::cast_precision_loss, reason = "six steps")]
                    let at = 0.05 + i as f64 * 0.1;
                    lines.push(line(at, format!("[cal] progress <{progress}>")));
                }
                lines.push(Line {
                    set: Some(("SENS_BOARD_X_OFF", -0.1)),
                    ..line(STEP_S, "[cal] calibration done: level")
                });
            }
            Kind::Airspeed => {
                lines.push(Line {
                    severity: 2,
                    ..line(0.05, "[cal] Ensure sensor is not measuring wind")
                });
                lines.push(line(STEP_S, "[cal] progress <40>"));
                lines.push(Line {
                    set: Some(("SENS_DPRES_OFF", 7.0)),
                    ..line(STEP_S * 2.0, "[cal] Offset of 7 Pascal")
                });
                lines.push(Line {
                    severity: 2,
                    ..line(
                        STEP_S * 2.5,
                        "[cal] Blow into front of pitot without touching",
                    )
                });
                lines.push(line(STEP_S * 4.0, "[cal] Positive pressure: OK (93 Pa)"));
                lines.push(line(STEP_S * 4.05, "[cal] progress <100>"));
                lines.push(line(STEP_S * 4.1, "[cal] calibration done: airspeed"));
            }
        }
        lines
    }

    /// Answers `MAV_CMD_PREFLIGHT_CALIBRATION`: its `MAV_RESULT` and what it
    /// says at once.
    pub fn calibrate(&mut self, t_s: f64, armed: bool, params: [f32; 7]) -> (u8, Vec<Out>) {
        let at = |i: usize, value: f32| (params[i] - value).abs() < 0.5;
        if params.iter().all(|p| p.abs() < 0.5) {
            // The cancel PX4's calibration takes, or the radio's end.
            let mut said = Vec::new();
            if self.running.take().is_some() {
                said.push(Out::Say(2, "[cal] calibration cancelled".into()));
            }
            if self.radio_calibrating {
                self.radio_calibrating = false;
                said.push(Out::Say(6, "Calibration: Restoring RC input\t".into()));
            }
            return (0, said);
        }
        if armed || self.running.is_some() {
            return (1, Vec::new());
        }
        let kind = if at(4, 1.0) {
            Kind::Accel
        } else if at(1, 1.0) {
            Kind::Mag
        } else if at(4, 2.0) {
            Kind::Level
        } else if at(5, 2.0) && self.plane {
            Kind::Airspeed
        } else if at(3, 1.0) {
            self.radio_calibrating = true;
            return (
                0,
                vec![Out::Say(6, "Calibration: Disabling RC input\t".into())],
            );
        } else {
            return (3, Vec::new());
        };
        self.running = Some(Running {
            kind,
            from_s: t_s,
            said: 0,
        });
        (0, self.tick(t_s))
    }

    /// Answers `MAV_CMD_FIXED_MAG_CAL_YAW` with the heading given: it
    /// assumes it faces it, and is done.
    #[must_use]
    pub fn quick_mag(armed: bool, heading_deg: f32) -> (u8, Vec<Out>) {
        if armed {
            return (1, Vec::new());
        }
        (
            0,
            vec![
                Out::Say(
                    6,
                    format!("Assuming vehicle is facing heading {heading_deg:.1} degrees"),
                ),
                Out::Say(6, "Mag quick calibration finished".into()),
                Out::Set("CAL_MAG0_XOFF", 0.195),
            ],
        )
    }

    /// What the calibration running says by `t_s`.
    pub fn tick(&mut self, t_s: f64) -> Vec<Out> {
        let Some(running) = self.running.clone() else {
            return Vec::new();
        };
        let script = self.script(running.kind);
        let mut out = Vec::new();
        let mut said = running.said;
        while let Some(next) = script.get(said) {
            if t_s - running.from_s + 1e-9 < next.at_s {
                break;
            }
            out.push(Out::Say(next.severity, next.text.clone()));
            if let Some((name, value)) = next.set {
                out.push(Out::Set(name, value));
            }
            said += 1;
        }
        self.running = (said < script.len()).then_some(Running { said, ..running });
        out
    }

    /// Answers `MAV_CMD_ACTUATOR_TEST`: value, time, function.
    pub fn actuator_test(
        &mut self,
        t_s: f64,
        armed: bool,
        enabled: bool,
        (value, timeout_s, function): (f32, f32, f32),
    ) -> u8 {
        if armed || !enabled {
            return 2;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a motor's number"
        )]
        let motor = (function.round().clamp(0.0, 2000.0) as u32 % 1000) as u8;
        if timeout_s <= 0.0 || !value.is_finite() {
            self.actuator = None;
            return 0;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a pulse width"
        )]
        let pwm = (1000.0 + 1000.0 * value.clamp(0.0, 1.0)).round() as u16;
        let motor = if motor > 100 { motor - 100 } else { motor };
        self.actuator = Some((motor, pwm, t_s + f64::from(timeout_s).min(ACTUATOR_MAX_S)));
        0
    }

    /// The motor tested now and its output, until its time runs out.
    #[must_use]
    pub fn output(&self, t_s: f64) -> Option<(usize, u16)> {
        let (motor, pwm, until) = self.actuator?;
        (t_s < until).then_some((usize::from(motor), pwm))
    }

    /// A calibration runs.
    #[must_use]
    pub const fn calibrating(&self) -> bool {
        self.running.is_some()
    }

    /// Restarted: nothing runs.
    pub fn restarted(&mut self) {
        self.running = None;
        self.radio_calibrating = false;
        self.actuator = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px4_cal::{CalText, parse};

    fn texts(out: &[Out]) -> Vec<String> {
        out.iter()
            .filter_map(|o| match o {
                Out::Say(_, text) => Some(text.clone()),
                Out::Set(..) => None,
            })
            .collect()
    }

    #[test]
    fn the_accelerometers_take_six_sides_and_are_saved() {
        let mut px4 = FakePx4::default();
        let (result, first) = px4.calibrate(0.0, false, [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(result, 0);
        assert_eq!(texts(&first), ["[cal] calibration started: 2 accel"]);
        let mut all = Vec::new();
        for tenth in 1..200 {
            all.extend(px4.tick(f64::from(tenth) / 10.0));
        }
        let read: Vec<CalText> = texts(&all).iter().filter_map(|t| parse(t)).collect();
        assert!(read.contains(&CalText::Detected(Side::Down)));
        assert!(read.contains(&CalText::SideDone(Side::Up)));
        assert_eq!(read.last(), Some(&CalText::Done("accel".into())));
        assert!(all.contains(&Out::Set("CAL_ACC0_ID", ACCEL_ID)));
        // Nothing more once done.
        assert!(px4.tick(30.0).is_empty());
    }

    #[test]
    fn a_failing_calibration_says_why_then_its_kind() {
        let mut px4 = FakePx4 {
            accel_fails: true,
            ..FakePx4::default()
        };
        px4.calibrate(0.0, false, [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let said = texts(&px4.tick(10.0));
        assert_eq!(
            said[said.len() - 2..],
            [
                "[cal] calibration failed: timeout: no motion",
                "[cal] calibration failed: accel"
            ]
        );
    }

    #[test]
    fn cancelled_with_zeros_and_refused_armed() {
        let mut px4 = FakePx4::default();
        assert_eq!(
            px4.calibrate(0.0, true, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0])
                .0,
            1
        );
        px4.calibrate(0.0, false, [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let (result, said) = px4.calibrate(1.0, false, [0.0; 7]);
        assert_eq!(result, 0);
        assert_eq!(texts(&said), ["[cal] calibration cancelled"]);
        assert!(px4.tick(5.0).is_empty());
    }

    #[test]
    fn a_motor_runs_for_its_time_or_until_stopped() {
        let mut px4 = FakePx4::default();
        assert_eq!(px4.actuator_test(0.0, false, false, (0.1, 1.0, 1.0)), 2);
        assert_eq!(px4.actuator_test(0.0, true, true, (0.1, 1.0, 1.0)), 2);
        assert_eq!(px4.actuator_test(0.0, false, true, (0.3, 1.0, 2.0)), 0);
        assert_eq!(px4.output(0.5), Some((2, 1300)));
        assert_eq!(px4.output(1.1), None);
        // At most 3 s, and stopped by a time of 0.
        px4.actuator_test(2.0, false, true, (0.05, 9.0, 1101.0));
        assert_eq!(px4.output(4.9), Some((1, 1050)));
        assert_eq!(px4.output(5.1), None);
        px4.actuator_test(6.0, false, true, (0.2, 1.0, 3.0));
        px4.actuator_test(6.2, false, true, (0.0, 0.0, 3.0));
        assert_eq!(px4.output(6.3), None);
    }
}

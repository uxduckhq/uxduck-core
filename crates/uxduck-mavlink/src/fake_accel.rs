//! The fake vehicle's accelerometers (S186), calibrated as `ArduPilot`
//! calibrates them: `MAV_CMD_PREFLIGHT_CALIBRATION` with parameter 5 at 1
//! starts the full calibration, which asks for each of six positions in
//! turn with `MAV_CMD_ACCELCAL_VEHICLE_POS` (target 0, as `ArduPilot` sends it),
//! once a second until answered with the same command and position; each
//! position is measured for a second; then it says "Calibration successful"
//! (or "Calibration FAILED") and repeats the result as the same command once
//! a second until it restarts, needing that restart to use the result.
//! Parameter 5 at 2 is level, answered at once. For tests and `uxduck dev
//! fake-vehicle`; it does not sense how it is held.

/// `ACCELCAL_VEHICLE_POS_SUCCESS` and `_FAILED`.
pub const ACCELCAL_SUCCESS: u32 = 16_777_215;
pub const ACCELCAL_FAILED: u32 = 16_777_216;

/// How long it measures a position once it is confirmed.
const MEASURE_S: f64 = 1.0;
/// How often it asks again for a position, or repeats its result.
const ASK_EVERY_S: f64 = 1.0;

/// The trim level finds, radians: a small tilt, as a bench is not quite
/// level.
pub const LEVEL_TRIM: (f32, f32) = (0.002_1, -0.007);

/// The offsets the full calibration finds, m/s².
pub const OFFSETS: [f32; 3] = [0.112, -0.064, 0.231];

/// What the accelerometers do or say, for the fake vehicle to send.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// `MAV_CMD_ACCELCAL_VEHICLE_POS` to the ground station: a position,
    /// or the result.
    Ask(u32),
    /// A `STATUSTEXT`: severity and text.
    Say(u8, String),
    /// The calibration succeeded: keep its offsets.
    Saved,
}

/// The fake vehicle's accelerometers.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is a state ArduPilot's calibration has"
)]
pub struct FakeAccel {
    /// Its offsets are in its parameters (`add_accel`), so it reports its
    /// pre-arm checks.
    pub listed: bool,
    pub calibrated: bool,
    /// Calibrated since it booted: it uses the result once restarted.
    pub restart_needed: bool,
    /// The full calibration fails after its last position.
    pub fails: bool,
    /// The position asked for, 1 to 6, while the calibration runs.
    step: Option<u32>,
    /// Confirmed, it measures until then.
    measuring_until_s: Option<f64>,
    asked_at_s: Option<f64>,
    /// Said "Place vehicle …" for this step.
    prompted: bool,
    /// The last result, repeated until it restarts.
    result: Option<bool>,
    result_at_s: Option<f64>,
    pub(crate) said_prearm_s: Option<f64>,
}

impl Default for FakeAccel {
    fn default() -> Self {
        Self {
            listed: false,
            calibrated: true,
            restart_needed: false,
            fails: false,
            step: None,
            measuring_until_s: None,
            asked_at_s: None,
            prompted: false,
            result: None,
            result_at_s: None,
            said_prearm_s: None,
        }
    }
}

/// How `ArduPilot` names a position in its prompt.
fn named(step: u32) -> &'static str {
    match step {
        1 => "level",
        2 => "on its LEFT side",
        3 => "on its RIGHT side",
        4 => "nose DOWN",
        5 => "nose UP",
        _ => "on its BACK",
    }
}

impl FakeAccel {
    /// The position it asks for now, while it runs.
    #[must_use]
    pub const fn asking(&self) -> Option<u32> {
        match (self.step, self.measuring_until_s) {
            (Some(step), None) => Some(step),
            _ => None,
        }
    }

    /// What its pre-arm check says, if it fails, as `ArduPilot` says it.
    #[must_use]
    pub const fn prearm(&self) -> Option<&'static str> {
        if !self.listed || self.step.is_some() {
            None
        } else if self.restart_needed {
            Some("Accels calibrated requires reboot")
        } else if !self.calibrated {
            Some("3D Accel calibration needed")
        } else {
            None
        }
    }

    /// Calibrated and nothing waiting on a restart: it may arm.
    #[must_use]
    pub const fn in_use(&self) -> bool {
        self.calibrated && !self.restart_needed
    }

    pub fn restarted(&mut self) {
        if self.restart_needed {
            self.calibrated = true;
            self.restart_needed = false;
        }
        self.step = None;
        self.measuring_until_s = None;
        self.asked_at_s = None;
        self.result = None;
        self.result_at_s = None;
        self.said_prearm_s = None;
    }

    /// `MAV_CMD_PREFLIGHT_CALIBRATION` with this parameter 5, at `now_s`:
    /// its `MAV_RESULT` and what it says, or `None` for one that is not
    /// the accelerometers'. Refused armed. A start while one runs is taken
    /// and changes nothing, as `ArduPilot`'s does.
    pub fn calibrate(&mut self, param5: f32, armed: bool) -> Option<(u8, Option<String>)> {
        let full = (param5 - 1.0).abs() < 0.5;
        let level = (param5 - 2.0).abs() < 0.5;
        if !full && !level {
            return None;
        }
        if armed || (level && self.step.is_some()) {
            return Some((4, None));
        }
        if level {
            let (roll, pitch) = LEVEL_TRIM;
            return Some((
                0,
                Some(format!(
                    "Trim OK: roll={:.2} pitch={:.2}",
                    roll.to_degrees(),
                    pitch.to_degrees()
                )),
            ));
        }
        if self.step.is_none() {
            self.step = Some(1);
            self.measuring_until_s = None;
            self.asked_at_s = None;
            self.prompted = false;
            self.result = None;
            self.result_at_s = None;
        }
        Some((0, None))
    }

    /// The ground station's `MAV_CMD_ACCELCAL_VEHICLE_POS`: taken, and
    /// measured, when it is the position asked for.
    pub fn placed(&mut self, position: f32, now_s: f64) -> u8 {
        match self.asking() {
            Some(step) if (f64::from(position) - f64::from(step)).abs() < 0.5 => {
                self.measuring_until_s = Some(now_s + MEASURE_S);
                0
            }
            _ => 4,
        }
    }

    /// What it does by `now_s`: the next position once one is measured,
    /// its prompts, its requests once a second, and its result.
    pub fn tick(&mut self, now_s: f64) -> Vec<Out> {
        let mut out = Vec::new();
        if let (Some(step), Some(until)) = (self.step, self.measuring_until_s)
            && now_s >= until
        {
            self.measuring_until_s = None;
            self.asked_at_s = None;
            self.prompted = false;
            if step >= 6 {
                let succeeded = !self.fails;
                self.step = None;
                self.result = Some(succeeded);
                self.result_at_s = Some(now_s);
                if succeeded {
                    self.restart_needed = true;
                    out.push(Out::Saved);
                    out.push(Out::Say(2, "Calibration successful".into()));
                } else {
                    out.push(Out::Say(2, "Calibration FAILED".into()));
                }
                return out;
            }
            self.step = Some(step + 1);
        }
        if let Some(step) = self.asking() {
            if !self.prompted {
                self.prompted = true;
                out.push(Out::Say(
                    2,
                    format!("Place vehicle {} and press any key.", named(step)),
                ));
            }
            if self.asked_at_s.is_none_or(|at| now_s - at >= ASK_EVERY_S) {
                self.asked_at_s = Some(now_s);
                out.push(Out::Ask(step));
            }
        } else if let Some(succeeded) = self.result
            && self.result_at_s.is_none_or(|at| now_s - at >= ASK_EVERY_S)
        {
            self.result_at_s = Some(now_s);
            out.push(Out::Ask(if succeeded {
                ACCELCAL_SUCCESS
            } else {
                ACCELCAL_FAILED
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_asks_for_six_positions_in_turn_then_needs_a_restart() {
        let mut accel = FakeAccel {
            listed: true,
            calibrated: false,
            ..FakeAccel::default()
        };
        assert_eq!(accel.prearm(), Some("3D Accel calibration needed"));
        assert_eq!(accel.calibrate(1.0, true), Some((4, None)), "armed");
        assert_eq!(accel.calibrate(1.0, false), Some((0, None)));
        let mut t = 0.0;
        for step in 1..=6u16 {
            let out = accel.tick(t);
            assert!(out.contains(&Out::Ask(u32::from(step))), "{step}: {out:?}");
            // Asked again a second later, not sooner.
            assert!(accel.tick(t + 0.5).is_empty());
            assert_eq!(accel.tick(t + 1.0), [Out::Ask(u32::from(step))]);
            // Another position is refused.
            assert_eq!(accel.placed(f32::from(step % 6 + 1), t), 4);
            assert_eq!(accel.placed(f32::from(step), t + 1.0), 0);
            assert!(accel.tick(t + 1.5).is_empty(), "measuring");
            t += 2.0;
        }
        let out = accel.tick(t);
        assert_eq!(
            out,
            [Out::Saved, Out::Say(2, "Calibration successful".into())]
        );
        assert_eq!(accel.tick(t + 1.0), [Out::Ask(ACCELCAL_SUCCESS)]);
        assert_eq!(accel.prearm(), Some("Accels calibrated requires reboot"));
        accel.restarted();
        assert!(accel.in_use());
        assert_eq!(accel.prearm(), None);
        assert!(accel.tick(t + 5.0).is_empty());
    }

    #[test]
    fn it_can_fail_and_levels_at_once() {
        let mut accel = FakeAccel {
            listed: true,
            calibrated: false,
            fails: true,
            ..FakeAccel::default()
        };
        let level = accel.calibrate(2.0, false);
        assert!(matches!(level, Some((0, Some(ref said))) if said.starts_with("Trim OK")));
        accel.calibrate(1.0, false);
        let mut t = 0.0;
        for step in 1..=6u16 {
            accel.tick(t);
            accel.placed(f32::from(step), t);
            t += 1.0;
        }
        assert_eq!(accel.tick(t), [Out::Say(2, "Calibration FAILED".into())]);
        assert_eq!(accel.tick(t + 1.0), [Out::Ask(ACCELCAL_FAILED)]);
        assert!(!accel.restart_needed && !accel.calibrated);
        // Something that is not the accelerometers' is not answered here.
        assert_eq!(accel.calibrate(0.0, false), None);
    }
}

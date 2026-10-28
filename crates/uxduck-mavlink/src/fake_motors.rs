//! The fake vehicle's motor test (S188), as `ArduCopter`'s: it answers
//! `MAV_CMD_DO_MOTOR_TEST`, refused while it is armed by its pilot; a test
//! running arms it, as `ArduCopter` arms its motors for one, and drives one
//! output at the throttle asked for until its time runs out. A newer test
//! renews it. A throttle and time of 0 stop it at once. `ArduCopter` says
//! "starting motor test" and "finished motor test", and its refusals as
//! "Motor Test: …". Listed (`add_motors`), it has a quad X's `FRAME_CLASS`
//! and `FRAME_TYPE`.

/// The test's outputs for a quad X, by its place in the test's order:
/// `AP_MotorsMatrix`'s quad X, A front right on output 1, B back right on
/// 4, C back left on 2, D front left on 3.
pub const QUAD_X_OUTPUTS: [usize; 4] = [1, 4, 2, 3];

/// The lowest and highest output, microseconds.
pub const PWM_MIN: f64 = 1000.0;
pub const PWM_MAX: f64 = 2000.0;

/// A test running: which output, how hard, until when.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Running {
    output: usize,
    pwm: u16,
    until_s: f64,
}

/// The fake vehicle's motor test.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FakeMotors {
    /// Lists `FRAME_CLASS` and `FRAME_TYPE` (`add_motors`).
    pub listed: bool,
    running: Option<Running>,
}

/// What a motor test command did: its `MAV_RESULT` and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub result: u8,
    pub said: Vec<(u8, String)>,
}

impl FakeMotors {
    /// Answers `MAV_CMD_DO_MOTOR_TEST` at `t_s`: its place in the test's
    /// order, throttle type (0 percent, 1 PWM), throttle and time.
    #[must_use]
    pub fn test(
        &mut self,
        t_s: f64,
        armed: bool,
        (order, kind, throttle, timeout_s): (f32, f32, f32, f32),
    ) -> Answer {
        let mut said = Vec::new();
        if armed && self.running.is_none() {
            // ArduCopter refuses with the arming checks' words.
            said.push((2, "Motor Test: Vehicle is armed".to_owned()));
            return Answer { result: 4, said };
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small motor number"
        )]
        let output = QUAD_X_OUTPUTS
            .get((order.round() as usize).wrapping_sub(1))
            .copied();
        #[allow(clippy::cast_possible_truncation, reason = "a throttle type, 0 to 3")]
        let pwm = match kind.round() as i32 {
            0 if throttle <= 100.0 => PWM_MIN + (PWM_MAX - PWM_MIN) * f64::from(throttle) / 100.0,
            1 => f64::from(throttle),
            _ => 0.0,
        };
        if self.running.is_none() {
            said.push((6, "starting motor test".to_owned()));
        }
        // Out of time at once, or a pulse it will not send: it stops on its
        // next loop.
        if timeout_s <= 0.0 || !(800.0..=2200.0).contains(&pwm) {
            self.running = None;
            said.push((6, "finished motor test".to_owned()));
            return Answer { result: 0, said };
        }
        let Some(output) = output else {
            if self.running.take().is_some() {
                said.push((6, "Motor Test: cancelled".to_owned()));
            }
            return Answer { result: 0, said };
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a pulse in microseconds"
        )]
        let pwm = pwm.round() as u16;
        self.running = Some(Running {
            output,
            pwm,
            until_s: t_s + f64::from(timeout_s.min(600.0)),
        });
        Answer { result: 0, said }
    }

    /// Its time ran out by `t_s`: stopped, with what it says.
    pub fn tick(&mut self, t_s: f64) -> Option<String> {
        let running = self.running?;
        (t_s >= running.until_s).then(|| {
            self.running = None;
            "finished motor test".to_owned()
        })
    }

    /// A test is running: the vehicle is armed for it.
    #[must_use]
    pub const fn running(&self) -> bool {
        self.running.is_some()
    }

    /// The output a test drives, from 1, and its pulse.
    #[must_use]
    pub fn output(&self) -> Option<(usize, u16)> {
        self.running.map(|r| (r.output, r.pwm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_spins_one_output_until_its_time_runs_out_and_renews() {
        let mut motors = FakeMotors::default();
        let start = motors.test(10.0, false, (1.0, 0.0, 5.0, 1.0));
        assert_eq!(start.result, 0);
        assert_eq!(start.said[0].1, "starting motor test");
        assert_eq!(motors.output(), Some((1, 1050)));
        // Renewed before it ran out.
        assert!(
            motors
                .test(10.5, false, (1.0, 0.0, 5.0, 1.0))
                .said
                .is_empty()
        );
        assert_eq!(motors.tick(11.2), None);
        assert_eq!(motors.tick(11.5).as_deref(), Some("finished motor test"));
        assert_eq!(motors.output(), None);
        // B is output 4 on a quad X.
        let _ = motors.test(12.0, false, (2.0, 0.0, 30.0, 1.0));
        assert_eq!(motors.output(), Some((4, 1300)));
        // Stopped at once with a throttle and time of 0.
        let stop = motors.test(12.2, true, (2.0, 0.0, 0.0, 0.0));
        assert_eq!(stop.result, 0);
        assert!(!motors.running());
    }

    #[test]
    fn refused_while_armed_by_its_pilot() {
        let mut motors = FakeMotors::default();
        let refused = motors.test(1.0, true, (1.0, 0.0, 5.0, 1.0));
        assert_eq!(refused.result, 4);
        assert_eq!(refused.said[0].1, "Motor Test: Vehicle is armed");
        assert!(!motors.running());
    }
}

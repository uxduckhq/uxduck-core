//! The fake vehicle's radio (S187): eight channels it reports as
//! `RC_CHANNELS` when asked, and RC parameters at `ArduPilot`'s defaults,
//! saying "RC not calibrated" until channels 1 to 4 have their own ranges.
//! For tests and `uxduck dev fake-vehicle --radio`, it moves its sticks by
//! itself as a person following the radio calibration would, from when the
//! channels are first asked for at a rate: centred (throttle down) for 8 s,
//! every channel swept to both ends twice over 8 s, centred again for 8 s,
//! then roll right, pitch forward, throttle up and yaw right, each held
//! 3 s with 2 s centred between, and centred from then on. Confirm the
//! centre in the first 8 s and the ends between 16 and 24 s.

use std::f64::consts::TAU;

/// How many channels it gives.
pub const CHANNELS: usize = 8;

/// Each channel's ends, microseconds: a radio's sticks stop a little short
/// of 1000 and 2000, its switches do not.
pub const ENDS: [(u16, u16); CHANNELS] = [
    (1004, 1996),
    (1010, 1990),
    (1000, 2000),
    (1002, 1998),
    (1000, 2000),
    (1000, 2000),
    (1000, 2000),
    (1000, 2000),
];

/// Its readings with the sticks centred and the throttle down.
pub const CENTRED: [u16; CHANNELS] = [1500, 1500, 1000, 1500, 1000, 1500, 1000, 2000];

/// The script's phases, seconds from when it was asked for.
pub const SWEEP_FROM_S: f64 = 8.0;
pub const SWEEP_UNTIL_S: f64 = 16.0;
pub const STICKS_FROM_S: f64 = 24.0;
/// Each stick is held this long, then centred for `REST_S`.
pub const STICK_S: f64 = 3.0;
pub const REST_S: f64 = 2.0;

/// The RC parameters `ArduPilot` starts with.
pub const DEFAULT_MIN: f32 = 1100.0;
pub const DEFAULT_MAX: f32 = 1900.0;
pub const DEFAULT_TRIM: f32 = 1500.0;
/// `FS_THR_VALUE`'s default.
pub const FAILSAFE_US: f32 = 975.0;

/// The fake vehicle's radio.
#[derive(Clone, Debug, PartialEq)]
pub struct FakeRadio {
    /// Listed in its parameters (`add_radio`): it gives its channels and
    /// reports its pre-arm check.
    pub listed: bool,
    /// Its roll channel runs backwards: rolling right lowers channel 1, so
    /// a calibration offers to reverse it.
    pub roll_reversed: bool,
    /// How much faster than a person it moves: 1 by default, more in
    /// tests that should not wait 45 s.
    pub pace: f64,
    /// When its channels were last asked for at a rate, in `telemetry`
    /// time: the script runs from there.
    script_from_s: Option<f64>,
    pub(crate) said_prearm_s: Option<f64>,
}

impl Default for FakeRadio {
    fn default() -> Self {
        Self {
            listed: false,
            roll_reversed: false,
            pace: 1.0,
            script_from_s: None,
            said_prearm_s: None,
        }
    }
}

impl FakeRadio {
    /// Its channels were asked for at a rate: the sticks start moving.
    pub fn asked(&mut self, t_s: f64) {
        self.script_from_s = Some(t_s);
    }

    /// Its readings at `t_s`, microseconds.
    #[must_use]
    pub fn channels(&self, t_s: f64) -> [u16; CHANNELS] {
        let Some(from) = self.script_from_s else {
            return CENTRED;
        };
        let at = (t_s - from) * self.pace;
        if (SWEEP_FROM_S..SWEEP_UNTIL_S).contains(&at) {
            // Two full swings of every channel together, about its middle,
            // as thumbs going round both sticks at once.
            let turn = (at - SWEEP_FROM_S) / (SWEEP_UNTIL_S - SWEEP_FROM_S) * 2.0;
            let mut out = CENTRED;
            for (n, value) in out.iter_mut().enumerate() {
                let (low, high) = ENDS[n];
                let (low, high) = (f64::from(low), f64::from(high));
                let swing = f64::midpoint(low, high) + (high - low) / 2.0 * (TAU * turn).sin();
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "between the channel's ends"
                )]
                let swing = swing.round().clamp(low, high) as u16;
                *value = swing;
            }
            return out;
        }
        let mut out = CENTRED;
        if at >= STICKS_FROM_S {
            let each = STICK_S + REST_S;
            let k = ((at - STICKS_FROM_S) / each).floor();
            let held = (at - STICKS_FROM_S) - k * each < STICK_S;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "which stick, a small whole number from 0"
            )]
            let k = k.clamp(0.0, f64::from(u8::MAX)) as u8;
            if held {
                match k {
                    // Roll right: up, unless it runs backwards.
                    0 => {
                        out[0] = if self.roll_reversed {
                            ENDS[0].0
                        } else {
                            ENDS[0].1
                        }
                    }
                    // Pitch forward: down, as ArduPilot reads pitch.
                    1 => out[1] = ENDS[1].0,
                    2 => out[2] = ENDS[2].1,
                    3 => out[3] = ENDS[3].1,
                    _ => {}
                }
            }
        }
        out
    }

    /// When its ranges are its own: channels 1 to 4's `RCn_MIN` and
    /// `RCn_MAX` changed from `ArduPilot`'s defaults.
    #[must_use]
    pub fn calibrated(parameters: &[(String, f32, u8)]) -> bool {
        let value = |name: String| parameters.iter().find(|p| p.0 == name).map(|p| p.1);
        (1..=4).all(|n| {
            value(format!("RC{n}_MIN")).is_some_and(|v| (v - DEFAULT_MIN).abs() > 0.5)
                && value(format!("RC{n}_MAX")).is_some_and(|v| (v - DEFAULT_MAX).abs() > 0.5)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_follows_the_calibration_once_asked() {
        let mut radio = FakeRadio::default();
        assert_eq!(radio.channels(30.0), CENTRED);
        radio.asked(100.0);
        assert_eq!(radio.channels(104.0), CENTRED);
        // Every channel reaches both ends in the sweep.
        let mut low = [u16::MAX; CHANNELS];
        let mut high = [0u16; CHANNELS];
        let mut t = 100.0 + SWEEP_FROM_S;
        while t < 100.0 + SWEEP_UNTIL_S {
            for (n, value) in radio.channels(t).into_iter().enumerate() {
                low[n] = low[n].min(value);
                high[n] = high[n].max(value);
            }
            t += 0.05;
        }
        for n in 0..CHANNELS {
            assert!(low[n] <= ENDS[n].0 + 2, "channel {n} low {}", low[n]);
            assert!(high[n] + 2 >= ENDS[n].1, "channel {n} high {}", high[n]);
        }
        assert_eq!(radio.channels(100.0 + 20.0), CENTRED);
        // Then each stick in turn.
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 1.0)[0], 1996);
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 4.0), CENTRED);
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 6.0)[1], 1010);
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 11.0)[2], 2000);
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 16.0)[3], 1998);
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 30.0), CENTRED);
        radio.roll_reversed = true;
        assert_eq!(radio.channels(100.0 + STICKS_FROM_S + 1.0)[0], 1004);
    }

    #[test]
    fn calibrated_once_the_sticks_have_their_own_ranges() {
        let mut parameters: Vec<(String, f32, u8)> = (1..=4)
            .flat_map(|n| {
                [
                    (format!("RC{n}_MIN"), DEFAULT_MIN, 4),
                    (format!("RC{n}_MAX"), DEFAULT_MAX, 4),
                ]
            })
            .collect();
        assert!(!FakeRadio::calibrated(&parameters));
        for p in &mut parameters {
            p.1 = if p.0.ends_with("MIN") { 1004.0 } else { 1996.0 };
        }
        assert!(FakeRadio::calibrated(&parameters));
    }
}

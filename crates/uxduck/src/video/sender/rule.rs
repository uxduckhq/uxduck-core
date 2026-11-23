//! How a camera's encoder follows the connection (ADR-0064 W7): from the
//! lowest bandwidth estimate among its viewers to the bitrate, frame rate
//! and keyframe interval `MediaMTX` gives its Raspberry Pi camera.
//!
//! The target is 0.85 of the estimate, between the camera's floor and the
//! bitrate the person set, which is the ceiling. A fall is applied at once,
//! a rise at most once a second, and a change under 5% is left alone. Below
//! what the encoder can make at its coarsest, the frame rate steps to a
//! half, a fifth and a tenth of the camera's own, after 2 s of making 30%
//! more than the target, and back up after 3 s with the target 1.2 times
//! what the faster rate made.
//!
//! mediamtx-rpicamera 2.9.0 leaves the frame rate `OpenH264` started with,
//! which then spends the bitrate as if every picture came, so the bitrate
//! sent is the target times the camera's own rate over the current one. A
//! helper that passes the new rate on would need the target alone, so that
//! ratio is corrected a quarter of the way to target over measured every
//! 2 s, from the rate the sender reads from `MediaMTX`. The bitrate sent is
//! never above the ceiling, and the keyframe interval keeps one keyframe a
//! second.
//!
//! While the camera records on the vehicle (ADR-0066), its recording is a
//! second encode of the same sensor, so the live frame rate is the
//! recording's too. The rule then never steps below the recording's own
//! frame rate and follows with the bitrate alone.

use std::time::{Duration, Instant};

/// The share of the estimate the camera is set to, leaving room for
/// retransmissions, RTP's headers and the estimate's own error.
pub(super) const SHARE_OF_ESTIMATE: f64 = 0.85;
const RISE_EVERY: Duration = Duration::from_secs(1);
const SMALLEST_CHANGE: f64 = 0.05;
/// The camera's own frame rate divided by these: its own, a half, a fifth
/// and a tenth, the lowest the maintainer chose (2026-11-17).
const DIVISORS: [u32; 4] = [1, 2, 5, 10];
const OVER_TARGET: f64 = 1.3;
const OVER_FOR: u32 = 2;
const ROOM_TO_RISE: f64 = 1.2;
const ROOM_FOR: u32 = 3;
/// The ratio's correction: this share of the way, after this many seconds
/// measured at one setting.
const CORRECT_BY: f64 = 0.25;
const CORRECT_AFTER: u32 = 2;

/// What `MediaMTX` sets a Raspberry Pi camera's encoder to.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// `rpiCameraBitrate`, bit/s.
    pub bitrate: u64,
    /// `rpiCameraFPS`.
    pub fps: f64,
    /// `rpiCameraIDRPeriod`, in pictures.
    pub idr: u64,
}

#[derive(Debug)]
pub struct Rule {
    /// The settings the person chose: the ceiling and the camera's own rate.
    own: Settings,
    floor: u64,
    target: u64,
    last_rise: Option<Instant>,
    step: usize,
    /// What the camera made at each rate when the rule stepped down from it.
    needed: [u64; DIVISORS.len()],
    over: u32,
    room: u32,
    /// How far the bitrate sent is from the target times the divisor: 1
    /// for mediamtx-rpicamera 2.9.0, down to a tenth for a helper that
    /// passes the frame rate on.
    correction: f64,
    /// Bits made, and seconds, since the settings last changed.
    measured: (u64, u32),
    settings: Settings,
    /// The lowest frame rate it may step to, while the camera records.
    at_least: Option<f64>,
    /// The last second made more than the target, and only the recording
    /// kept the frame rate up.
    held: bool,
}

impl Rule {
    /// Starting from the camera's own settings, before any estimate. A
    /// floor above the ceiling is the ceiling, which the caller says.
    #[must_use]
    pub fn new(own: Settings, floor: u64) -> Self {
        Self {
            own,
            floor: floor.min(own.bitrate),
            target: own.bitrate,
            last_rise: None,
            step: 0,
            needed: [0; DIVISORS.len()],
            over: 0,
            room: 0,
            correction: 1.0,
            measured: (0, 0),
            settings: own,
            at_least: None,
            held: false,
        }
    }

    /// While the camera records at `at_least` frames a second, the rule steps
    /// no lower, and with `None` it may again. The settings, when they change.
    pub fn hold(&mut self, at_least: Option<f64>) -> Option<Settings> {
        if at_least == self.at_least {
            return None;
        }
        self.at_least = at_least;
        self.held = false;
        let deepest = self.deepest();
        if self.step > deepest {
            self.step = deepest;
            self.over = 0;
            self.room = 0;
        }
        self.publish()
    }

    /// Whether the connection carries less than the frame rate the recording
    /// keeps needs: the camera made 30% more than the target in the last
    /// second, at the lowest rate the recording allows.
    #[must_use]
    pub fn held(&self) -> bool {
        self.held
    }

    /// The slowest step the recording allows, or the slowest there is.
    fn deepest(&self) -> usize {
        let Some(at_least) = self.at_least else {
            return DIVISORS.len() - 1;
        };
        // A hundredth of a picture a second covers the rates' rounding.
        DIVISORS
            .iter()
            .rposition(|d| self.own.fps / f64::from(*d) >= at_least - 0.01)
            .unwrap_or(0)
    }

    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings
    }

    #[must_use]
    pub fn target(&self) -> u64 {
        self.target
    }

    /// The camera's own settings changed under the rule, as after a restart
    /// of `MediaMTX` with another bitrate. The settings, when they change.
    pub fn reconfigure(&mut self, own: Settings, floor: u64) -> Option<Settings> {
        if own == self.own {
            return None;
        }
        self.own = own;
        self.floor = floor.min(own.bitrate);
        self.target = self.target.clamp(self.floor, own.bitrate);
        self.settings = own;
        self.publish();
        Some(self.settings)
    }

    /// A new estimate, the lowest among the viewers. The settings, when
    /// they change.
    pub fn estimate(&mut self, estimate: u64, now: Instant) -> Option<Settings> {
        let want = whole(float(estimate) * SHARE_OF_ESTIMATE).clamp(self.floor, self.own.bitrate);
        let change = float(want) / float(self.target) - 1.0;
        let fall = change < -SMALLEST_CHANGE;
        let rise = change > SMALLEST_CHANGE
            && self
                .last_rise
                .is_none_or(|at| now.duration_since(at) >= RISE_EVERY);
        let to_a_limit = want != self.target && (want == self.floor || want == self.own.bitrate);
        if !(fall || rise || to_a_limit) {
            return None;
        }
        if want > self.target {
            self.last_rise = Some(now);
        }
        self.target = want;
        self.publish()
    }

    /// Once a second, with the bits the camera made in it. The settings,
    /// when they change.
    pub fn second(&mut self, made: u64) -> Option<Settings> {
        let target = float(self.target);
        let slower = self.step + 1;
        let over = float(made) > OVER_TARGET * target;
        self.held = over && self.at_least.is_some() && self.step == self.deepest();
        if over
            && slower <= self.deepest()
            && slower < DIVISORS.len()
            && self.bitrate_at(slower) <= self.own.bitrate
        {
            self.over += 1;
            if self.over >= OVER_FOR {
                self.needed[self.step] = made;
                self.step = slower;
                self.over = 0;
                self.room = 0;
                return self.publish();
            }
        } else {
            self.over = 0;
        }
        if self.step > 0 && target >= ROOM_TO_RISE * float(self.needed[self.step - 1]) {
            self.room += 1;
            if self.room >= ROOM_FOR {
                self.step -= 1;
                self.room = 0;
                return self.publish();
            }
        } else {
            self.room = 0;
        }
        if self.step == 0 || made == 0 {
            return None;
        }
        self.measured.0 += made;
        self.measured.1 += 1;
        if self.measured.1 < CORRECT_AFTER {
            return None;
        }
        let average = float(self.measured.0) / f64::from(self.measured.1);
        self.measured = (0, 0);
        // Between a helper that passes the frame rate on, which needs the
        // target alone, and 2.9.0's, which needs the divisor.
        let divisor = f64::from(DIVISORS[self.step]);
        self.correction = (self.correction * (1.0 + CORRECT_BY * (target / average - 1.0)))
            .clamp(1.0 / divisor, 1.0);
        self.publish()
    }

    /// The bitrate to send at `step` for the target: the target times the
    /// divisor, corrected.
    fn bitrate_at(&self, step: usize) -> u64 {
        let divisor = f64::from(DIVISORS[step]);
        whole(float(self.target) * (divisor * self.correction).clamp(1.0, divisor))
    }

    /// Works out the settings for the target and the step, and keeps the
    /// old ones when only the bitrate moved, by less than 5%. When the
    /// target needs more than the ceiling at this rate, the rate steps up,
    /// unless the faster one made 30% more than the target, which would only
    /// step it down again: then the ceiling is sent.
    fn publish(&mut self) -> Option<Settings> {
        while self.step > 0
            && self.bitrate_at(self.step) > self.own.bitrate
            && OVER_TARGET * float(self.target) >= float(self.needed[self.step - 1])
        {
            self.step -= 1;
        }
        let fps = self.own.fps / f64::from(DIVISORS[self.step]);
        let wanted = Settings {
            bitrate: self.bitrate_at(self.step).min(self.own.bitrate),
            fps,
            idr: whole(fps).max(1),
        };
        let same_rate =
            wanted.fps.total_cmp(&self.settings.fps).is_eq() && wanted.idr == self.settings.idr;
        let small = (float(wanted.bitrate) / float(self.settings.bitrate) - 1.0).abs()
            < SMALLEST_CHANGE
            && wanted.bitrate != self.own.bitrate
            && wanted.bitrate != self.floor;
        if wanted == self.settings || (same_rate && small) {
            return None;
        }
        self.settings = wanted;
        self.measured = (0, 0);
        Some(wanted)
    }
}

/// Rates in bit/s, which stay far inside f64's integers.
#[allow(clippy::cast_precision_loss, reason = "rates in bit/s")]
fn float(bits: u64) -> f64 {
    bits as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "rates in bit/s and pictures, never negative"
)]
fn whole(value: f64) -> u64 {
    value.round().max(0.0) as u64
}

#[cfg(test)]
mod tests;

//! How each axis's rate controller follows what it is asked (S330,
//! ADR-0054 T7): the samples a vehicle sends in `PID_TUNING`, or, without
//! them, its body rates in `ATTITUDE` against those `ATTITUDE_TARGET` asks
//! for, and a summary of the last ten seconds that says what to do next.

use serde::{Deserialize, Serialize};

use super::tuning::Axis;

/// Where an axis's samples come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// The rate controller's own `PID_TUNING`, with its terms.
    Pid,
    /// The body rates of `ATTITUDE` against `ATTITUDE_TARGET`'s: no terms.
    Attitude,
}

/// What each of a rate controller's terms added, in its output's units.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Terms {
    pub p: f64,
    pub i: f64,
    pub d: f64,
    pub ff: f64,
}

/// One moment of one axis: the rate asked for and the rate done, in
/// degrees per second, as received.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub axis: Axis,
    pub source: Source,
    pub t_us: u64,
    pub desired: f64,
    pub achieved: f64,
    pub terms: Option<Terms>,
}

/// The summary reads this long a stretch.
pub const SUMMARY_US: u64 = 10_000_000;

/// What the summary says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Too few samples yet.
    Gathering,
    /// It was hardly asked to turn: nothing to judge it by.
    Still,
    Following,
    /// It follows late or short.
    Slow,
    Overshooting,
    /// It rings at a rate it was not asked for.
    Oscillating,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub verdict: Verdict,
    /// The most it was asked to turn, in degrees per second.
    pub asked_degps: f64,
    /// How far what it did was from what it was asked, as a part of what
    /// it was asked, in percent; none while it was hardly asked.
    pub tracking_error_pct: Option<f64>,
    /// How far its biggest turn went past the biggest it was asked, in
    /// percent of that: from −90 °/s to 90, 25 % of the step is 50 %.
    pub overshoot_pct: Option<f64>,
    /// The strongest rate in the difference between done and asked that
    /// is not in what was asked, and how big it is in degrees per second.
    pub oscillation_hz: Option<f64>,
    pub oscillation_degps: Option<f64>,
}

const MIN_SAMPLES: usize = 25;
const MIN_SPAN_US: u64 = 2_000_000;
/// Asked for less than this, an axis is still.
const STILL_DEGPS: f64 = 10.0;
/// A ring smaller than this is noise…
const RING_DEGPS: f64 = 3.0;
/// …as is one less than this many times what was asked at the same rate…
const RING_OVER_ASKED: f64 = 2.0;
/// …or one not this far above the difference's other rates.
const RING_OVER_REST: f64 = 4.0;
/// Slower than this, a difference is the vehicle catching up.
const RING_LOWEST_HZ: f64 = 2.0;
const STEP_HZ: f64 = 0.1;
const OVERSHOOT_PCT: f64 = 20.0;
const SLOW_PCT: f64 = 60.0;

/// The summary of one axis's samples, oldest first, as (time, asked,
/// done), read over the last [`SUMMARY_US`] of them.
#[must_use]
pub fn summarize(samples: &[(u64, f64, f64)]) -> Summary {
    let end = samples.last().map_or(0, |s| s.0);
    let from = samples.partition_point(|s| s.0 + SUMMARY_US < end);
    let samples = &samples[from..];
    let asked = samples.iter().map(|s| s.1.abs()).fold(0.0, f64::max);
    let mut summary = Summary {
        verdict: Verdict::Gathering,
        asked_degps: asked,
        tracking_error_pct: None,
        overshoot_pct: None,
        oscillation_hz: None,
        oscillation_degps: None,
    };
    let span = end.saturating_sub(samples.first().map_or(end, |s| s.0));
    if samples.len() < MIN_SAMPLES || span < MIN_SPAN_US {
        return summary;
    }
    let Some(grid) = resample(samples) else {
        return summary;
    };
    if let Some((hz, degps)) = ring(&grid) {
        summary.oscillation_hz = Some(hz);
        summary.oscillation_degps = Some(degps);
        summary.verdict = Verdict::Oscillating;
    }
    if asked < STILL_DEGPS {
        if summary.verdict != Verdict::Oscillating {
            summary.verdict = Verdict::Still;
        }
        return summary;
    }
    let rms = |values: &mut dyn Iterator<Item = f64>| {
        let (sum, n) = values.fold((0.0, 0_u32), |(s, n), v| (v.mul_add(v, s), n + 1));
        (sum / f64::from(n.max(1))).sqrt()
    };
    let error = rms(&mut grid.points.iter().map(|p| p.2 - p.1));
    let wanted = rms(&mut grid.points.iter().map(|p| p.1));
    let tracking = 100.0 * error / wanted.max(f64::EPSILON);
    let sign = samples
        .iter()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map_or(1.0, |s| s.1.signum());
    let peak = |pick: fn(&(u64, f64, f64)) -> f64| {
        samples
            .iter()
            .map(|s| sign * pick(s))
            .fold(f64::MIN, f64::max)
    };
    let overshoot = (100.0 * (peak(|s| s.2) - peak(|s| s.1)) / asked).max(0.0);
    summary.tracking_error_pct = Some(tracking);
    summary.overshoot_pct = Some(overshoot);
    if summary.verdict != Verdict::Oscillating {
        summary.verdict = if overshoot >= OVERSHOOT_PCT {
            Verdict::Overshooting
        } else if tracking >= SLOW_PCT {
            Verdict::Slow
        } else {
            Verdict::Following
        };
    }
    summary
}

/// Samples at even times, as a vehicle's arrive unevenly.
struct Grid {
    step_s: f64,
    /// Seconds from the first, asked, done.
    points: Vec<(f64, f64, f64)>,
}

#[allow(
    clippy::cast_precision_loss,
    reason = "microseconds within ten seconds"
)]
fn resample(samples: &[(u64, f64, f64)]) -> Option<Grid> {
    let mut gaps: Vec<u64> = samples
        .windows(2)
        .map(|w| w[1].0.saturating_sub(w[0].0))
        .filter(|gap| *gap > 0)
        .collect();
    gaps.sort_unstable();
    let step_us = (*gaps.get(gaps.len() / 2)?).clamp(5_000, 200_000);
    let start = samples.first()?.0;
    let end = samples.last()?.0;
    let mut points = Vec::new();
    let mut at = start;
    let mut next = 1;
    while at <= end {
        while next < samples.len() - 1 && samples[next].0 < at {
            next += 1;
        }
        let (a, b) = (samples[next - 1], samples[next]);
        let width = b.0.saturating_sub(a.0);
        let f = if width == 0 {
            1.0
        } else {
            (at.saturating_sub(a.0) as f64 / width as f64).clamp(0.0, 1.0)
        };
        points.push((
            (at - start) as f64 / 1e6,
            (b.1 - a.1).mul_add(f, a.1),
            (b.2 - a.2).mul_add(f, a.2),
        ));
        at += step_us;
    }
    Some(Grid {
        step_s: step_us as f64 / 1e6,
        points,
    })
}

/// The strongest rate in done less asked that asked does not have, with
/// its size, when it is the controller ringing rather than noise.
#[allow(clippy::cast_precision_loss, reason = "a few hundred points")]
fn ring(grid: &Grid) -> Option<(f64, f64)> {
    let n = grid.points.len();
    let window: Vec<f64> = (0..n)
        .map(|k| 0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / (n - 1) as f64).cos())
        .collect();
    let weight: f64 = window.iter().sum();
    let mean =
        |pick: fn(&(f64, f64, f64)) -> f64| grid.points.iter().map(pick).sum::<f64>() / n as f64;
    let (asked_mean, error_mean) = (mean(|p| p.1), mean(|p| p.2 - p.1));
    let size = |hz: f64, pick: &dyn Fn(&(f64, f64, f64)) -> f64| {
        let (re, im) = grid
            .points
            .iter()
            .zip(&window)
            .fold((0.0, 0.0), |(re, im), (p, w)| {
                let angle = std::f64::consts::TAU * hz * p.0;
                let v = w * pick(p);
                (v.mul_add(angle.cos(), re), v.mul_add(-angle.sin(), im))
            });
        2.0 * f64::hypot(re, im) / weight
    };
    let error = |p: &(f64, f64, f64)| p.2 - p.1 - error_mean;
    let asked = |p: &(f64, f64, f64)| p.1 - asked_mean;
    let highest = 0.5 / grid.step_s - STEP_HZ;
    let rates: Vec<(f64, f64, f64)> = (0..=u16::MAX)
        .map(|k| f64::from(k).mul_add(STEP_HZ, RING_LOWEST_HZ))
        .take_while(|hz| *hz <= highest)
        .map(|hz| (hz, size(hz, &error), size(hz, &asked)))
        .collect();
    let mut sizes: Vec<f64> = rates.iter().map(|r| r.1).collect();
    sizes.sort_by(f64::total_cmp);
    let typical = *sizes.get(sizes.len() / 2)?;
    let &(hz, degps, _) = rates
        .iter()
        .filter(|r| r.1 >= RING_OVER_ASKED * r.2)
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    (degps >= RING_DEGPS && degps >= RING_OVER_REST * typical).then_some((hz, degps))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten seconds at 25 Hz, the times a little uneven as a radio's are.
    fn samples(asked: impl Fn(f64) -> f64, done: impl Fn(f64) -> f64) -> Vec<(u64, f64, f64)> {
        (0..250_u32)
            .map(|k| {
                let jitter = f64::from(k * 7919 % 9) - 4.0;
                let t = (f64::from(k) / 25.0 + jitter / 1000.0).max(0.0);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "test times"
                )]
                let t_us = (t * 1e6) as u64 + 1_000_000;
                (t_us, asked(t), done(t))
            })
            .collect()
    }

    /// ±90 °/s, changing every second.
    fn steps(t: f64) -> f64 {
        if t.floor().rem_euclid(2.0) < 1.0 {
            90.0
        } else {
            -90.0
        }
    }

    /// How a controller follows the steps, approaching each one by
    /// `response` of the time since it.
    fn following(t: f64, response: impl Fn(f64) -> f64) -> f64 {
        let to = steps(t);
        if t < 1.0 {
            return to * response(t);
        }
        (2.0 * to).mul_add(response(t - t.floor()), -to)
    }

    fn noise(t: f64, size: f64) -> f64 {
        size * (t * 1013.0).sin().mul_add(0.6, (t * 2711.0).cos() * 0.4)
    }

    fn lag(tau: f64) -> impl Fn(f64) -> f64 {
        move |dt| 1.0 - (-dt / tau).exp()
    }

    #[test]
    fn a_clean_step_follows_well() {
        let summary = summarize(&samples(steps, |t| following(t, lag(0.04))));
        assert_eq!(summary.verdict, Verdict::Following, "{summary:?}");
        assert!(summary.tracking_error_pct.unwrap() < 40.0, "{summary:?}");
        assert!(summary.overshoot_pct.unwrap() < 5.0, "{summary:?}");
    }

    #[test]
    fn a_slow_step_is_slow() {
        let summary = summarize(&samples(steps, |t| following(t, lag(0.35))));
        assert_eq!(summary.verdict, Verdict::Slow, "{summary:?}");
    }

    #[test]
    fn an_underdamped_step_overshoots() {
        let (zeta, omega) = (0.4_f64, std::f64::consts::TAU * 4.0);
        let ratio = zeta / zeta.mul_add(-zeta, 1.0).sqrt();
        let damped = omega * zeta.mul_add(-zeta, 1.0).sqrt();
        let response = |dt: f64| {
            1.0 - (-zeta * omega * dt).exp()
                * ratio.mul_add((damped * dt).sin(), (damped * dt).cos())
        };
        let summary = summarize(&samples(steps, |t| following(t, response)));
        assert_eq!(summary.verdict, Verdict::Overshooting, "{summary:?}");
        assert!(
            (30.0..60.0).contains(&summary.overshoot_pct.unwrap()),
            "{summary:?}"
        );
    }

    #[test]
    fn a_ring_is_found_at_its_rate_while_hovering() {
        let summary = summarize(&samples(
            |t| noise(t, 1.0),
            |t| 15.0f64.mul_add((std::f64::consts::TAU * 8.0 * t).sin(), noise(t, 1.0)),
        ));
        assert_eq!(summary.verdict, Verdict::Oscillating, "{summary:?}");
        assert!(
            (summary.oscillation_hz.unwrap() - 8.0).abs() < 0.3,
            "{summary:?}"
        );
        assert!(summary.oscillation_degps.unwrap() > 10.0, "{summary:?}");
    }

    #[test]
    fn a_ring_on_top_of_steps_is_still_a_ring() {
        let summary = summarize(&samples(steps, |t| {
            20.0f64.mul_add(
                (std::f64::consts::TAU * 11.0 * t).sin(),
                following(t, lag(0.04)),
            )
        }));
        assert_eq!(summary.verdict, Verdict::Oscillating, "{summary:?}");
        assert!(
            (summary.oscillation_hz.unwrap() - 11.0).abs() < 0.3,
            "{summary:?}"
        );
    }

    #[test]
    fn stillness_says_so() {
        let summary = summarize(&samples(|t| noise(t, 0.5), |t| noise(t, 0.8)));
        assert_eq!(summary.verdict, Verdict::Still, "{summary:?}");
        assert_eq!(summary.tracking_error_pct, None);
    }

    #[test]
    fn too_few_samples_wait() {
        let s = samples(steps, steps);
        assert_eq!(summarize(&s[..10]).verdict, Verdict::Gathering);
        assert_eq!(summarize(&[]).verdict, Verdict::Gathering);
    }

    #[test]
    fn only_the_last_ten_seconds_count() {
        let mut s = samples(|_| 0.0, |t| 15.0 * (std::f64::consts::TAU * 8.0 * t).sin());
        let later = samples(steps, |t| following(t, lag(0.04)));
        s.extend(later.into_iter().map(|(t, a, d)| (t + 11_000_000, a, d)));
        assert_eq!(
            summarize(&s).verdict,
            Verdict::Following,
            "{:?}",
            summarize(&s)
        );
    }
}

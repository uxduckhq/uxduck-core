//! A listening viewer's sound (ADR-0067 M5, M6): which live step it gets,
//! and where each of the step's packets goes on its one RTP stream.
//!
//! A viewer who listens gets the highest step whose wire cost is at most
//! 15% of its own estimate, never below the lowest. A step down is taken at
//! once, a step up after the estimate has allowed it for 5 s.
//!
//! The steps of one run of the capture process share one timeline, so a
//! viewer moved to another step goes on at the same times: the old step's
//! packets are sent up to where a packet of the new step starts, and the
//! new step's from there. A new run starts a new timeline, which goes on
//! from the last after the time that passed. The viewer's times only ever
//! grow, so its decoder hears a change of bitrate, never a jump back.

use std::time::{Duration, Instant};

use crate::video::companion::sound::STEPS;

/// The share of a viewer's estimate its sound may cost: sound is what the
/// person asked for when they unmuted, and at the 150 kbit/s floor this
/// still gives the lowest step, the least Opus needs for speech (M5).
const SHARE: f64 = 0.15;
/// How long the estimate must allow a higher step before it is taken.
const RISE_AFTER: Duration = Duration::from_secs(5);
const LOWEST: usize = STEPS.len() - 1;

/// The highest step whose wire cost `estimate` allows.
#[must_use]
pub fn allowed(estimate: u64) -> usize {
    #[allow(clippy::cast_precision_loss, reason = "rates in bit/s")]
    let share = estimate as f64 * SHARE;
    STEPS
        .iter()
        .position(|step| f64::from(step.wire()) <= share)
        .unwrap_or(LOWEST)
}

/// The estimate the picture is followed with while sound costs `step`
/// (M6).
#[must_use]
pub fn for_the_picture(estimate: u64, step: Option<usize>) -> u64 {
    step.map_or(estimate, |step| {
        estimate.saturating_sub(u64::from(STEPS[step].wire()))
    })
}

/// Where a step's packet goes.
#[derive(Clone, Copy, Debug)]
struct Sent {
    ssrc: u32,
    /// Where the last packet started, on the run's timeline and on the
    /// viewer's, and how long the step says it lasts. Opus can start a run
    /// with a shorter packet, so the next one is measured from this start.
    start: u32,
    out: u64,
    samples: u32,
    at: Instant,
}

impl Sent {
    fn end(&self) -> u32 {
        self.start.wrapping_add(self.samples)
    }

    fn after(&self) -> u64 {
        self.out + u64::from(self.samples)
    }
}

#[derive(Debug, Default)]
pub struct Listener {
    /// The step chosen, once there was an estimate.
    chosen: Option<usize>,
    rising_since: Option<Instant>,
    /// The step whose packets go now.
    sending: Option<usize>,
    /// While moving to the chosen step: a time one of its packets started
    /// at, and where the sending step stopped to meet it.
    phase: Option<u32>,
    stopped_at: Option<u32>,
    last: Option<Sent>,
}

impl Listener {
    /// The step chosen, the lowest before any estimate.
    #[must_use]
    pub fn step(&self) -> usize {
        self.chosen.unwrap_or(LOWEST)
    }

    /// Unmuted again: the step is chosen afresh from the next estimate,
    /// and its times go on from the last.
    pub fn start(&mut self, estimate: Option<u64>, now: Instant) {
        self.chosen = None;
        self.rising_since = None;
        self.sending = None;
        self.phase = None;
        self.stopped_at = None;
        self.estimate(estimate, now);
    }

    /// The viewer's estimate now.
    pub fn estimate(&mut self, estimate: Option<u64>, now: Instant) {
        let Some(estimate) = estimate else {
            return;
        };
        let allowed = allowed(estimate);
        let chosen = match self.chosen {
            None => allowed,
            Some(step) if allowed > step => allowed,
            Some(step) if allowed < step => {
                let since = *self.rising_since.get_or_insert(now);
                if now.duration_since(since) < RISE_AFTER {
                    return;
                }
                allowed
            }
            Some(step) => step,
        };
        self.rising_since = None;
        if self.chosen != Some(chosen) {
            self.chosen = Some(chosen);
            self.phase = None;
            self.stopped_at = None;
        }
    }

    /// A packet of `step`: the time on the viewer's stream, at 48 kHz, to
    /// send it at, or None when it is not this viewer's.
    pub fn packet(&mut self, step: usize, ssrc: u32, timestamp: u32, at: Instant) -> Option<u64> {
        let chosen = self.step();
        let samples = STEPS[step].samples();
        if let Some(last) = self.last
            && last.ssrc != ssrc
        {
            // A new run: its own timeline, after the time that passed.
            if step != chosen {
                return None;
            }
            let started = at.checked_sub(Duration::from_millis(u64::from(STEPS[step].ms)));
            let gap = started
                .map(|started| started.saturating_duration_since(last.at))
                .unwrap_or_default();
            let gap = u64::try_from(gap.as_micros() * 48 / 1000).unwrap_or(u64::MAX);
            return Some(self.send(step, ssrc, timestamp, last.after().saturating_add(gap), at));
        }
        let ahead = |last: &Sent| i64::from(timestamp.wrapping_sub(last.start).cast_signed());
        match self.sending {
            Some(sending) if sending != chosen && step == chosen => {
                let Some(stopped) = self.stopped_at else {
                    self.phase = Some(timestamp);
                    return None;
                };
                let last = self.last?;
                let ahead = i64::from(timestamp.wrapping_sub(stopped).cast_signed());
                if ahead < 0 {
                    return None;
                }
                Some(self.send(
                    step,
                    ssrc,
                    timestamp,
                    last.after() + ahead.unsigned_abs(),
                    at,
                ))
            }
            Some(sending) if sending != chosen => {
                if step != sending || self.stopped_at.is_some() {
                    return None;
                }
                let last = self.last?;
                let ahead = ahead(&last);
                if ahead <= 0 {
                    return None;
                }
                let out = self.send(step, ssrc, timestamp, last.out + ahead.unsigned_abs(), at);
                // Stop where the chosen step's next packet starts, within
                // this step's next packet.
                if let (Some(phase), Some(last)) = (self.phase, self.last) {
                    let next = i64::from(STEPS[chosen].samples());
                    let into =
                        i64::from(last.end().wrapping_sub(phase).cast_signed()).rem_euclid(next);
                    if into == 0 || next - into < i64::from(samples) {
                        self.stopped_at = Some(last.end());
                    }
                }
                Some(out)
            }
            _ if step != chosen => None,
            _ => {
                let out = match self.last {
                    None => 0,
                    Some(last) => {
                        let ahead = ahead(&last);
                        if ahead <= 0 {
                            return None;
                        }
                        last.out + ahead.unsigned_abs()
                    }
                };
                Some(self.send(step, ssrc, timestamp, out, at))
            }
        }
    }

    fn send(&mut self, step: usize, ssrc: u32, timestamp: u32, out: u64, at: Instant) -> u64 {
        let samples = STEPS[step].samples();
        if self.sending != Some(step) {
            self.phase = None;
            self.stopped_at = None;
        }
        self.sending = Some(step);
        self.last = Some(Sent {
            ssrc,
            start: timestamp,
            out,
            samples,
            at,
        });
        out
    }
}

#[cfg(test)]
mod tests;

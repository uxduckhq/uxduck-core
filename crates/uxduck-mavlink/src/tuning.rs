//! The samples of the Tuning tab's response graph (S330, ADR-0054 T7):
//! each `PID_TUNING` the vehicle sends, kept until the next tick so none is
//! lost to telemetry's coalescing. An axis without them, while the vehicle
//! sends `ATTITUDE_TARGET`, is worked out from `ATTITUDE`'s body rates
//! against the ones asked for there.
//!
//! `PID_TUNING`'s rates are rad/s from a copter's (and a VTOL's hover)
//! controllers and deg/s from a plane's, though the message says deg/s:
//! which one is read from `ATTITUDE`'s gyro rates while the axis moves.

use uxduck_core::domain::tuning::Axis;
use uxduck_core::domain::tuning_response::{Sample, Source, Terms};

use crate::decode::Message;

/// An axis heard in `PID_TUNING` this recently is not worked out.
const PID_FRESH_US: u64 = 1_000_000;
/// `ATTITUDE` is compared with an `ATTITUDE_TARGET` this recent.
const TARGET_FRESH_US: u64 = 300_000;
/// The most kept between ticks: three axes at 50 Hz through four seconds.
const MOST: usize = 600;
/// A gyro rate this large, in rad/s, tells radians from degrees.
const MOVING_RADPS: f32 = 0.1;

#[derive(Clone, Debug, Default)]
pub(crate) struct Feed {
    pending: Vec<Sample>,
    pid_heard_us: [Option<u64>; 3],
    /// When the latest `ATTITUDE_TARGET` came, and its rates in rad/s.
    target: Option<(u64, [Option<f32>; 3])>,
    /// When the latest `ATTITUDE` came, and its body rates in rad/s.
    gyro: Option<(u64, [f32; 3])>,
    /// Whether each axis' `PID_TUNING` was seen in deg/s; rad/s until then.
    in_degrees: [bool; 3],
}

const fn index(axis: Axis) -> usize {
    match axis {
        Axis::Roll => 0,
        Axis::Pitch => 1,
        Axis::Yaw => 2,
    }
}

/// `PID_TUNING_AXIS`'s roll, pitch and yaw; the others are not rates.
const fn axis_of(axis: u8) -> Option<Axis> {
    match axis {
        1 => Some(Axis::Roll),
        2 => Some(Axis::Pitch),
        3 => Some(Axis::Yaw),
        _ => None,
    }
}

impl Feed {
    pub(crate) fn take(&mut self, message: &Message, received_us: u64) {
        match *message {
            Message::PidTuning {
                axis,
                desired,
                achieved,
                ff,
                p,
                i,
                d,
            } => {
                let Some(axis) = axis_of(axis) else {
                    return;
                };
                let at = index(axis);
                self.pid_heard_us[at] = Some(received_us);
                if let Some((_, rates)) = self
                    .gyro
                    .filter(|(t, _)| received_us.saturating_sub(*t) <= TARGET_FRESH_US)
                    && rates[at].abs() >= MOVING_RADPS
                {
                    let gyro = rates[at];
                    self.in_degrees[at] =
                        (achieved - gyro.to_degrees()).abs() < (achieved - gyro).abs();
                }
                let to_degrees = |rate: f32| {
                    let rate = f64::from(rate);
                    if self.in_degrees[at] {
                        rate
                    } else {
                        rate.to_degrees()
                    }
                };
                self.push(Sample {
                    axis,
                    source: Source::Pid,
                    t_us: received_us,
                    desired: to_degrees(desired),
                    achieved: to_degrees(achieved),
                    terms: Some(Terms {
                        p: f64::from(p),
                        i: f64::from(i),
                        d: f64::from(d),
                        ff: f64::from(ff),
                    }),
                });
            }
            Message::AttitudeTarget {
                type_mask,
                body_roll_rate,
                body_pitch_rate,
                body_yaw_rate,
                ..
            } => {
                let given =
                    |bit: u8, rate: f32| (type_mask & bit == 0 && rate.is_finite()).then_some(rate);
                self.target = Some((
                    received_us,
                    [
                        given(1, body_roll_rate),
                        given(2, body_pitch_rate),
                        given(4, body_yaw_rate),
                    ],
                ));
            }
            Message::Attitude {
                rollspeed,
                pitchspeed,
                yawspeed,
                ..
            } => {
                self.gyro = Some((received_us, [rollspeed, pitchspeed, yawspeed]));
                let Some((_, asked)) = self
                    .target
                    .filter(|(at, _)| received_us.saturating_sub(*at) <= TARGET_FRESH_US)
                else {
                    return;
                };
                for (axis, done) in Axis::ALL.into_iter().zip([rollspeed, pitchspeed, yawspeed]) {
                    let heard = self.pid_heard_us[index(axis)]
                        .is_some_and(|t| received_us.saturating_sub(t) <= PID_FRESH_US);
                    if let (false, Some(asked), true) =
                        (heard, asked[index(axis)], done.is_finite())
                    {
                        self.push(Sample {
                            axis,
                            source: Source::Attitude,
                            t_us: received_us,
                            desired: f64::from(asked).to_degrees(),
                            achieved: f64::from(done).to_degrees(),
                            terms: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// What came since the last tick.
    pub(crate) fn drain(&mut self) -> Vec<Sample> {
        std::mem::take(&mut self.pending)
    }

    fn push(&mut self, sample: Sample) {
        if self.pending.len() >= MOST {
            self.pending.remove(0);
        }
        self.pending.push(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(axis: u8, desired: f32) -> Message {
        Message::PidTuning {
            axis,
            desired,
            achieved: desired / 2.0,
            ff: 0.1,
            p: 0.2,
            i: 0.3,
            d: 0.4,
        }
    }

    fn attitude(roll: f32) -> Message {
        Message::Attitude {
            time_boot_ms: 0,
            roll: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            rollspeed: roll,
            pitchspeed: 0.0,
            yawspeed: 0.0,
        }
    }

    fn target(mask: u8, roll: f32) -> Message {
        Message::AttitudeTarget {
            time_boot_ms: 0,
            type_mask: mask,
            body_roll_rate: roll,
            body_pitch_rate: 0.0,
            body_yaw_rate: 0.0,
        }
    }

    #[test]
    fn keeps_every_rate_sample_with_its_terms() {
        let mut feed = Feed::default();
        feed.take(&pid(1, 0.5), 10);
        feed.take(&pid(1, 1.0), 20);
        feed.take(&pid(3, 0.1), 20);
        feed.take(&pid(4, 9.0), 20);
        let samples = feed.drain();
        assert_eq!(samples.len(), 3, "roll twice and yaw; ACCZ is not a rate");
        assert_eq!(samples[1].axis, Axis::Roll);
        let degrees = (samples[1].desired, samples[1].achieved);
        assert!(
            (degrees.0 - 57.295_78).abs() < 1e-3,
            "rad/s until seen otherwise: {degrees:?}"
        );
        assert!((degrees.1 - 28.647_89).abs() < 1e-3);
        assert_eq!(samples[1].terms.unwrap().d, f64::from(0.4_f32));
        assert!(feed.drain().is_empty());
    }

    #[test]
    fn works_rates_out_from_attitude_only_without_pid_tuning() {
        let mut feed = Feed::default();
        feed.take(&attitude(1.0), 0);
        assert!(feed.drain().is_empty(), "no target yet");
        feed.take(&target(0, 1.0), 100_000);
        feed.take(&attitude(0.5), 150_000);
        let samples = feed.drain();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].source, Source::Attitude);
        assert!((samples[0].desired - 57.295_78).abs() < 1e-3);
        assert!((samples[0].achieved - 28.647_89).abs() < 1e-3);
        feed.take(&pid(1, 30.0), 200_000);
        feed.take(&target(4, 1.0), 210_000);
        feed.take(&attitude(0.5), 220_000);
        let axes: Vec<_> = feed.drain().iter().map(|s| (s.axis, s.source)).collect();
        assert_eq!(
            axes,
            [(Axis::Roll, Source::Pid), (Axis::Pitch, Source::Attitude)],
            "roll has PID_TUNING; yaw's rate is not given"
        );
        feed.take(&attitude(0.5), 900_000);
        assert!(feed.drain().is_empty(), "the target is stale");
    }

    #[test]
    fn reads_a_plane_s_degrees_from_the_gyro_while_the_axis_moves() {
        let mut feed = Feed::default();
        feed.take(&attitude(0.02), 0);
        feed.take(&pid(1, 2.0), 10);
        feed.take(&attitude(0.35), 40_000);
        feed.take(&pid(1, 40.0), 50_000);
        feed.take(&pid(1, 41.0), 90_000);
        let samples = feed.drain();
        assert!(
            (samples[0].desired - 114.591_56).abs() < 1e-3,
            "still: rad/s assumed"
        );
        assert_eq!(
            samples[1].desired, 40.0,
            "achieved 20 deg/s against a gyro of 20 deg/s"
        );
        assert_eq!(samples[2].desired, 41.0, "and kept");
        feed.take(&attitude(0.35), 100_000);
        feed.take(&pid(1, 0.7), 110_000);
        assert!(
            (feed.drain()[0].achieved - 20.053_52).abs() < 1e-3,
            "a copter's again: rad/s"
        );
    }
}

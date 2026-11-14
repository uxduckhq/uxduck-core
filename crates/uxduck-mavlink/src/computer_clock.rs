//! Each onboard computer's clock against this computer's (S356, ADR-0049
//! amended 2026-11-13), as logic over frames and the clocks the caller
//! gives: a monotonic one in microseconds for round trips, which a change of
//! the wall clock cannot bend, and this computer's wall clock in nanoseconds
//! since 1970.
//!
//! - **Over `TIMESYNC`**: each request UXDuck sends is remembered with when
//!   it went on both clocks ([`Clocks::asked`]). A reply from an onboard
//!   computer (191 to 194) to one of them, addressed to UXDuck, whose `tc1`
//!   is on the Unix scale (at least [`UNIX_SCALE_NS`], in 2001: ArduPilot and
//!   PX4 answer with their time since boot), is a sample. Its round trip is
//!   when it came less when the request went, and the computer's clock less
//!   this one's is `tc1` less this computer's wall clock halfway through,
//!   true to within half the round trip whatever the delay each way. A reply
//!   does not use its request up, because every computer on the vehicle
//!   answers the same one. Of the samples of the last [`WINDOW_US`], the one
//!   with the shortest round trip is reported. A sample further from an
//!   earlier one than both their uncertainties and [`DRIFT_NS`] means the
//!   computer's clock was set, so the earlier one is dropped.
//! - **One way**: a computer that does not answer (an older `uxduck
//!   companion`, a MAVSDK or ROS stack, a router that does not pass the
//!   request on) still stamps its `ONBOARD_COMPUTER_STATUS` with `time_usec`.
//!   On the Unix scale, `time_usec` less this computer's wall clock when it
//!   arrived is the offset less the delay on the way. The largest of the
//!   window, the offset less the shortest delay, is reported, with no
//!   uncertainty: the delay is not known.
//!
//! `TIMESYNC` wins whenever the window holds a sample of it.

use std::collections::{BTreeMap, VecDeque};

use uxduck_core::domain::telemetry::{ClockMeasured, ComputerClock};

use crate::computer::COMPUTER_IDS;
use crate::decode::{Message, decode};
use crate::framing::header;
use crate::link_health::TIMESYNC;

/// The window samples are taken from.
pub const WINDOW_US: u64 = 60_000_000;
/// How long a request waits for its replies.
pub const OUTSTANDING_US: u64 = 5_000_000;
/// A clock at least this far from 1970, in nanoseconds (September 2001),
/// is a wall clock rather than a time since boot.
pub const UNIX_SCALE_NS: i64 = 1_000_000_000_000_000_000;
/// How far two samples of an unchanged clock may part beyond their
/// uncertainties, for the clocks' drift over the window.
pub const DRIFT_NS: i64 = 2_000_000;
/// The message id of `ONBOARD_COMPUTER_STATUS`.
pub const ONBOARD_COMPUTER_STATUS: u32 = 390;

/// How many requests are remembered at once, for every vehicle.
const REQUESTS: usize = 256;
/// How many samples of each kind a computer keeps.
const SAMPLES: usize = 256;

/// When something happened, on both clocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moment {
    /// The monotonic clock, in microseconds.
    pub mono_us: u64,
    /// This computer's wall clock, in nanoseconds since 1970.
    pub wall_ns: i64,
}

#[derive(Debug, Default)]
pub struct Clocks {
    /// UXDuck's requests, oldest first: their `ts1` and when they went.
    requests: VecDeque<(i64, Moment)>,
    /// Each computer's samples, by its vehicle and component id.
    computers: BTreeMap<(String, u8), Samples>,
}

#[derive(Debug, Default)]
struct Samples {
    /// `TIMESYNC` replies, oldest first.
    exchanges: VecDeque<Exchange>,
    /// Status time stamps, oldest first: when each came, on the monotonic
    /// clock, and its `time_usec` less the wall clock then, in nanoseconds.
    stamps: VecDeque<(u64, i64)>,
}

#[derive(Clone, Copy, Debug)]
struct Exchange {
    at_us: u64,
    offset_ns: i64,
    round_trip_ns: i64,
}

impl Clocks {
    /// A request with this `ts1` went at `sent`.
    pub fn asked(&mut self, ts1: i64, sent: Moment) {
        self.expire(sent.mono_us);
        if self.requests.iter().any(|(t, _)| *t == ts1) {
            return;
        }
        if self.requests.len() >= REQUESTS {
            self.requests.pop_front();
        }
        self.requests.push_back((ts1, sent));
    }

    /// A request goes at `sent` to an onboard computer at an address of its
    /// own: the `ts1` to send, the monotonic clock in nanoseconds, distinct
    /// from those remembered.
    pub fn ask(&mut self, sent: Moment) -> i64 {
        self.expire(sent.mono_us);
        let mut ts1 = i64::try_from(sent.mono_us.saturating_mul(1000)).unwrap_or(i64::MAX);
        while self.requests.iter().any(|(t, _)| *t == ts1) {
            ts1 = ts1.saturating_add(1);
        }
        self.asked(ts1, sent);
        ts1
    }

    /// One whole frame from `vehicle_id`'s link: a `TIMESYNC` reply or a
    /// status from one of its onboard computers counts, anything else is
    /// passed over. `gcs` is the system id UXDuck sends as. `now` is when it
    /// arrived, on both clocks. A reply that seems to have come before its
    /// request went is passed over too.
    pub fn heard(&mut self, vehicle_id: &str, frame: &[u8], gcs: u8, now: Moment) {
        let Some(header) = header(frame) else {
            return;
        };
        if !COMPUTER_IDS.contains(&header.component_id)
            || !(header.message_id == TIMESYNC || header.message_id == ONBOARD_COMPUTER_STATUS)
        {
            return;
        }
        let Some(decoded) = decode(frame) else {
            return;
        };
        self.expire(now.mono_us);
        let key = (vehicle_id.to_owned(), header.component_id);
        match decoded.message {
            Message::Timesync {
                tc1,
                ts1,
                target_system,
                ..
            } if tc1 >= UNIX_SCALE_NS && (target_system == 0 || target_system == gcs) => {
                let Some((_, sent)) = self.requests.iter().find(|(t, _)| *t == ts1) else {
                    return;
                };
                let Some(elapsed_us) = now.mono_us.checked_sub(sent.mono_us) else {
                    return;
                };
                let round_trip_ns = i64::try_from(elapsed_us)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1000);
                let halfway_ns = sent.wall_ns.saturating_add(round_trip_ns / 2);
                self.computers.entry(key).or_default().exchanged(Exchange {
                    at_us: now.mono_us,
                    offset_ns: tc1.saturating_sub(halfway_ns),
                    round_trip_ns,
                });
            }
            Message::OnboardComputerStatus(status) => {
                let Some(stamped_ns) = i64::try_from(status.time_usec)
                    .ok()
                    .and_then(|us| us.checked_mul(1000))
                    .filter(|ns| *ns >= UNIX_SCALE_NS)
                else {
                    return;
                };
                let stamps = &mut self.computers.entry(key).or_default().stamps;
                if stamps.len() >= SAMPLES {
                    stamps.pop_front();
                }
                stamps.push_back((now.mono_us, stamped_ns.saturating_sub(now.wall_ns)));
            }
            _ => {}
        }
    }

    /// Each computer's clock from the samples of the last [`WINDOW_US`],
    /// with its vehicle. A computer with none left is forgotten.
    pub fn report(&mut self, now_mono_us: u64) -> Vec<(String, ComputerClock)> {
        self.expire(now_mono_us);
        self.computers
            .retain(|_, samples| !samples.exchanges.is_empty() || !samples.stamps.is_empty());
        self.computers
            .iter()
            .filter_map(|((vehicle_id, component_id), samples)| {
                Some((vehicle_id.clone(), samples.clock(*component_id)?))
            })
            .collect()
    }

    /// Forgets the computers of every vehicle `keep` does not keep.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.computers.retain(|(vehicle_id, _), _| keep(vehicle_id));
    }

    fn expire(&mut self, now_us: u64) {
        self.requests
            .retain(|(_, sent)| now_us.saturating_sub(sent.mono_us) < OUTSTANDING_US);
        let old = |at_us: u64| now_us.saturating_sub(at_us) >= WINDOW_US;
        for samples in self.computers.values_mut() {
            samples.exchanges.retain(|e| !old(e.at_us));
            samples.stamps.retain(|(at_us, _)| !old(*at_us));
        }
    }
}

impl Samples {
    fn exchanged(&mut self, sample: Exchange) {
        self.exchanges.retain(|e| {
            e.offset_ns.abs_diff(sample.offset_ns)
                <= (e.round_trip_ns.saturating_add(sample.round_trip_ns) / 2)
                    .saturating_add(DRIFT_NS)
                    .unsigned_abs()
        });
        if self.exchanges.len() >= SAMPLES {
            self.exchanges.pop_front();
        }
        self.exchanges.push_back(sample);
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "nanoseconds shown in milliseconds"
    )]
    fn clock(&self, component_id: u8) -> Option<ComputerClock> {
        let ms = |ns: i64| ns as f64 / 1_000_000.0;
        if let Some(best) = self.exchanges.iter().rev().min_by_key(|e| e.round_trip_ns) {
            return Some(ComputerClock {
                component_id,
                offset_ms: ms(best.offset_ns),
                measured: ClockMeasured::Timesync {
                    within_ms: ms(best.round_trip_ns) / 2.0,
                },
            });
        }
        let largest = self.stamps.iter().map(|(_, ns)| *ns).max()?;
        Some(ComputerClock {
            component_id,
            offset_ms: ms(largest),
            measured: ClockMeasured::OneWay,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{ComputerStatus, status_frame};
    use crate::sim::timesync_frame;

    const GCS: u8 = 255;
    /// 2026-11-13, on this computer's wall clock, in nanoseconds.
    const WALL_NS: i64 = 1_794_000_000_000_000_000;
    /// 8 days 6 h behind, as the maintainer's second Pi was.
    const BEHIND_NS: i64 = -(8 * 24 + 6) * 3_600_000_000_000;

    fn at(mono_us: u64) -> Moment {
        Moment {
            mono_us,
            wall_ns: WALL_NS + i64::try_from(mono_us).unwrap() * 1000,
        }
    }

    fn reply(component_id: u8, tc1: i64, ts1: i64, target_system: u8) -> Vec<u8> {
        timesync_frame((1, component_id, 0), (tc1, ts1), (target_system, 190))
    }

    fn offset(clocks: &mut Clocks, now_us: u64) -> Vec<(u8, f64, ClockMeasured)> {
        clocks
            .report(now_us)
            .into_iter()
            .map(|(vehicle, clock)| {
                assert_eq!(vehicle, "v");
                (clock.component_id, clock.offset_ms, clock.measured)
            })
            .collect()
    }

    #[test]
    fn a_computers_reply_gives_its_clock_within_half_the_round_trip() {
        let mut clocks = Clocks::default();
        let ts1 = clocks.ask(at(1_000_000));
        // It reads its clock 3 ms after the request went, and the reply is
        // back 4 ms after that: halfway is 3.5 ms, so it reads 0.5 ms early.
        let tc1 = at(1_003_000).wall_ns + BEHIND_NS;
        clocks.heard("v", &reply(192, tc1, ts1, GCS), GCS, at(1_007_000));
        assert_eq!(
            offset(&mut clocks, 1_007_000),
            [(
                192,
                -712_800_000.5,
                ClockMeasured::Timesync { within_ms: 3.5 }
            )]
        );
    }

    #[test]
    fn only_a_computers_wall_clock_in_reply_to_our_request_counts() {
        let mut clocks = Clocks::default();
        let ts1 = clocks.ask(at(1_000_000));
        let wall = at(1_001_000).wall_ns;
        let now = at(1_002_000);
        // ArduPilot answers with its time since boot.
        clocks.heard("v", &reply(192, 3_600_000_000_000, ts1, GCS), GCS, now);
        // The autopilot, a camera, another ground station, another request.
        clocks.heard("v", &reply(1, wall, ts1, GCS), GCS, now);
        clocks.heard("v", &reply(100, wall, ts1, GCS), GCS, now);
        clocks.heard("v", &reply(192, wall, ts1, 254), GCS, now);
        clocks.heard("v", &reply(192, wall, ts1 + 1, GCS), GCS, now);
        // A reply that seems to come before the request went, as when the
        // wall clock steps forward while the reply waits to be read.
        clocks.heard("v", &reply(192, wall, ts1, GCS), GCS, at(999_000));
        // A request of its own, and a reply after the request was given up.
        clocks.heard("v", &reply(192, 0, ts1, 0), GCS, now);
        clocks.heard(
            "v",
            &reply(193, wall, ts1, 0),
            GCS,
            at(1_000_000 + OUTSTANDING_US),
        );
        assert!(clocks.report(1_002_000).is_empty());
    }

    #[test]
    fn every_computer_answering_the_same_request_is_measured() {
        let mut clocks = Clocks::default();
        clocks.asked(5, at(1_000_000));
        clocks.heard(
            "v",
            &reply(191, at(1_001_000).wall_ns, 5, 0),
            GCS,
            at(1_002_000),
        );
        clocks.heard(
            "v",
            &reply(194, at(1_001_000).wall_ns + 3_200_000_000, 5, GCS),
            GCS,
            at(1_002_000),
        );
        assert_eq!(
            offset(&mut clocks, 1_002_000),
            [
                (191, 0.0, ClockMeasured::Timesync { within_ms: 1.0 }),
                (194, 3_200.0, ClockMeasured::Timesync { within_ms: 1.0 })
            ]
        );
    }

    #[test]
    fn the_shortest_round_trip_of_the_last_minute_wins() {
        let mut clocks = Clocks::default();
        let ahead = 1_000_000_000;
        let exchange = |clocks: &mut Clocks, sent_us: u64, round_trip_us: u64| {
            let ts1 = clocks.ask(at(sent_us));
            let tc1 = at(sent_us + round_trip_us / 2).wall_ns + ahead;
            clocks.heard(
                "v",
                &reply(192, tc1, ts1, GCS),
                GCS,
                at(sent_us + round_trip_us),
            );
        };
        exchange(&mut clocks, 1_000_000, 20_000);
        exchange(&mut clocks, 2_000_000, 2_000);
        exchange(&mut clocks, 3_000_000, 9_000);
        assert_eq!(
            offset(&mut clocks, 3_009_000),
            [(192, 1_000.0, ClockMeasured::Timesync { within_ms: 1.0 })]
        );
        // A minute on, the 2 ms one has gone and the 9 ms one is the best.
        assert_eq!(
            offset(&mut clocks, 2_002_000 + WINDOW_US),
            [(192, 1_000.0, ClockMeasured::Timesync { within_ms: 4.5 })]
        );
        assert!(clocks.report(3_009_000 + WINDOW_US).is_empty(), "forgotten");
    }

    #[test]
    fn a_clock_that_is_set_drops_what_was_measured_before() {
        let mut clocks = Clocks::default();
        let ts1 = clocks.ask(at(1_000_000));
        let tc1 = at(1_000_500).wall_ns + BEHIND_NS;
        clocks.heard("v", &reply(192, tc1, ts1, GCS), GCS, at(1_001_000));
        // Set by a time server: in step now, though over a slower trip.
        let ts1 = clocks.ask(at(2_000_000));
        let tc1 = at(2_015_000).wall_ns;
        clocks.heard("v", &reply(192, tc1, ts1, GCS), GCS, at(2_030_000));
        assert_eq!(
            offset(&mut clocks, 2_030_000),
            [(192, 0.0, ClockMeasured::Timesync { within_ms: 15.0 })]
        );
    }

    fn status(time_usec: u64) -> Vec<u8> {
        let status = ComputerStatus {
            time_usec,
            ..ComputerStatus::default()
        };
        status_frame((1, 193, 0), &status)
    }

    fn stamped(now: Moment, delay_us: u64, offset_ns: i64) -> u64 {
        u64::try_from((now.wall_ns - i64::try_from(delay_us).unwrap() * 1000 + offset_ns) / 1000)
            .unwrap()
    }

    #[test]
    fn without_timesync_its_status_says_its_clock_one_way() {
        let mut clocks = Clocks::default();
        for (i, delay_us) in [(1, 40_000), (2, 3_000), (3, 25_000)] {
            let now = at(i * 1_000_000);
            clocks.heard("v", &status(stamped(now, delay_us, BEHIND_NS)), GCS, now);
        }
        // Time since boot says nothing of its clock.
        clocks.heard("v", &status(60_000_000), GCS, at(4_000_000));
        assert_eq!(
            offset(&mut clocks, 4_000_000),
            [(193, -712_800_003.0, ClockMeasured::OneWay)],
            "the offset less the shortest delay"
        );
        // TIMESYNC wins once it has a sample, and one way is back after it.
        let ts1 = clocks.ask(at(5_000_000));
        let tc1 = at(5_001_000).wall_ns + BEHIND_NS;
        clocks.heard(
            "v",
            &timesync_frame((1, 193, 0), (tc1, ts1), (GCS, 190)),
            GCS,
            at(5_002_000),
        );
        assert_eq!(
            offset(&mut clocks, 5_002_000),
            [(
                193,
                -712_800_000.0,
                ClockMeasured::Timesync { within_ms: 1.0 }
            )]
        );
        let now = at(5_002_000 + WINDOW_US);
        clocks.heard("v", &status(stamped(now, 2_000, BEHIND_NS)), GCS, now);
        assert_eq!(
            offset(&mut clocks, now.mono_us),
            [(193, -712_800_002.0, ClockMeasured::OneWay)]
        );
    }

    #[test]
    fn a_vehicle_no_longer_known_takes_its_computers_with_it() {
        let mut clocks = Clocks::default();
        let now = at(1_000_000);
        clocks.heard("v", &status(stamped(now, 0, 0)), GCS, now);
        clocks.heard("w", &status(stamped(now, 0, 0)), GCS, now);
        clocks.retain(|vehicle_id| vehicle_id == "v");
        let kept: Vec<String> = clocks
            .report(1_000_000)
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(kept, ["v"]);
    }
}

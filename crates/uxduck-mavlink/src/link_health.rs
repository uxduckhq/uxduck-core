//! How the link to each vehicle is doing (S261, ADR-0004 2026-11-01), as
//! logic over frames and a monotonic clock the caller gives in
//! microseconds.
//!
//! - **Round trip**: MAVLink's own ping, `TIMESYNC`. The caller sends a
//!   request ([`Tracker::asked`] gives its `ts1`) where the ground station
//!   heartbeat goes, and says why it did not where it does not
//!   ([`Tracker::not_asked`]). A reply counts only when its `ts1` is one of
//!   UXDuck's own outstanding requests to that address, so replies to
//!   another ground station on the same channel and a PX4's own requests
//!   (`tc1` 0) are never counted. At most [`OUTSTANDING`] requests are kept,
//!   each for [`OUTSTANDING_US`]. What is reported is the median of the
//!   round trips of the last [`ROUND_TRIP_WINDOW_US`]: one late reply does
//!   not move it, and a change in the link shows within a few seconds.
//! - **Loss**: gaps in each sender's 8-bit sequence (link, peer, system and
//!   component), over the last [`LOSS_WINDOW_US`]. The 255 to 0 wrap is a
//!   step of one; a duplicate is ignored; a frame up to [`REORDER`] behind
//!   fills the gap it left. A sender silent for [`SILENCE_US`] starts again
//!   without a gap: the silence is the telemetry's age, not a loss.
//! - **Age**: since anything was last heard from the vehicle's address.

use std::collections::{BTreeMap, VecDeque};

use uxduck_core::domain::telemetry::{LinkHealth, RoundTripMissing};

use crate::decode::{Message, decode};
use crate::framing::header;

/// A vehicle's address: link, peer and system id.
pub type Address = (String, String, u8);

/// The window loss is counted over.
pub const LOSS_WINDOW_US: u64 = 10_000_000;
/// The window round trips are taken from.
pub const ROUND_TRIP_WINDOW_US: u64 = 5_000_000;
/// How long a request waits for its reply before it is given up.
pub const OUTSTANDING_US: u64 = 5_000_000;
/// How many requests to one address wait for a reply at once.
pub const OUTSTANDING: usize = 8;
/// A sender silent this long starts its sequence again without a gap.
pub const SILENCE_US: u64 = 3_000_000;
/// How far behind the newest a frame may arrive and still fill its gap.
pub const REORDER: u8 = 64;
/// The message id of `TIMESYNC`.
pub const TIMESYNC: u32 = 111;

const BUCKET_US: u64 = 1_000_000;

#[derive(Debug, Default)]
pub struct Tracker {
    addresses: BTreeMap<Address, Health>,
}

#[derive(Debug, Default)]
struct Health {
    last_heard_us: Option<u64>,
    /// Each component's sequence, by component id.
    senders: BTreeMap<u8, Sequence>,
    /// Frames heard and lost, a second each, oldest first.
    buckets: VecDeque<Bucket>,
    /// Requests waiting for a reply: their `ts1` and when they went.
    outstanding: VecDeque<(i64, u64)>,
    /// Replies: when each came and its round trip, oldest first.
    round_trips: VecDeque<(u64, u64)>,
    /// When asking began, since it was last not asked.
    asked_since_us: Option<u64>,
    /// Why it is not asked now.
    not_asked: Option<RoundTripMissing>,
}

#[derive(Debug)]
struct Sequence {
    last: u8,
    /// Bit `k` is set when the frame `k` behind `last` was heard.
    heard: u64,
    at_us: u64,
}

#[derive(Debug)]
struct Bucket {
    start_us: u64,
    received: u32,
    lost: u32,
}

impl Tracker {
    /// One whole frame from a vehicle's address: its sequence, when it was
    /// heard, and, for a `TIMESYNC` reply to one of UXDuck's requests, its
    /// round trip. `gcs` is the system id UXDuck sends as.
    pub fn heard(&mut self, address: &Address, frame: &[u8], gcs: u8, now_us: u64) {
        let Some(header) = header(frame) else {
            return;
        };
        let health = self.addresses.entry(address.clone()).or_default();
        health.last_heard_us = Some(now_us);
        health.sequence(header.component_id, header.sequence, now_us);
        if header.message_id == TIMESYNC
            && let Some(decoded) = decode(frame)
            && let Message::Timesync {
                tc1,
                ts1,
                target_system,
                ..
            } = decoded.message
            && tc1 != 0
            && (target_system == 0 || target_system == gcs)
        {
            health.expire(now_us);
            health.replied(ts1, now_us);
        }
    }

    /// A request is going to `address` now: the `ts1` to send, UXDuck's
    /// clock in nanoseconds, distinct from those still waiting.
    pub fn asked(&mut self, address: &Address, now_us: u64) -> i64 {
        let health = self.addresses.entry(address.clone()).or_default();
        if health.not_asked.take().is_some() || health.asked_since_us.is_none() {
            health.asked_since_us = Some(now_us);
        }
        health.expire(now_us);
        let mut ts1 = i64::try_from(now_us.saturating_mul(1000)).unwrap_or(i64::MAX);
        while health.outstanding.iter().any(|(t, _)| *t == ts1) {
            ts1 = ts1.saturating_add(1);
        }
        if health.outstanding.len() >= OUTSTANDING {
            health.outstanding.pop_front();
        }
        health.outstanding.push_back((ts1, now_us));
        ts1
    }

    /// No request goes to `address`, and why: what is waiting and measured
    /// is dropped, so the round trip shows the reason, not an old value.
    pub fn not_asked(&mut self, address: &Address, why: RoundTripMissing) {
        let health = self.addresses.entry(address.clone()).or_default();
        health.not_asked = Some(why);
        health.asked_since_us = None;
        health.outstanding.clear();
        health.round_trips.clear();
    }

    /// How the link to `address` is doing now.
    pub fn report(&mut self, address: &Address, now_us: u64) -> LinkHealth {
        let Some(health) = self.addresses.get_mut(address) else {
            return LinkHealth::default();
        };
        health.expire(now_us);
        let (received, lost) = health
            .buckets
            .iter()
            .fold((0u32, 0u32), |(r, l), b| (r + b.received, l + b.lost));
        let sent = received + lost;
        #[allow(clippy::cast_precision_loss, reason = "microseconds of a round trip")]
        let round_trip_ms = health
            .not_asked
            .is_none()
            .then(|| median(health.round_trips.iter().map(|(_, rtt)| *rtt)))
            .flatten()
            .map(|us| us as f64 / 1000.0);
        let round_trip_missing = match (round_trip_ms, health.not_asked) {
            (Some(_), _) => None,
            (None, Some(why)) => Some(why),
            (None, None) => Some(
                if health
                    .asked_since_us
                    .is_some_and(|since| now_us.saturating_sub(since) < ROUND_TRIP_WINDOW_US)
                {
                    RoundTripMissing::Waiting
                } else {
                    RoundTripMissing::NoReply
                },
            ),
        };
        LinkHealth {
            round_trip_ms,
            round_trip_missing,
            loss_percent: (sent > 0).then(|| f64::from(lost) * 100.0 / f64::from(sent)),
            age_ms: health
                .last_heard_us
                .map(|at| now_us.saturating_sub(at) / 1000),
        }
    }

    /// Forgets every address `keep` does not keep, such as those of
    /// vehicles no longer known, with how each was last: nothing goes there
    /// now, and only the age of what was heard remains.
    pub fn retain(
        &mut self,
        mut keep: impl FnMut(&Address) -> bool,
        now_us: u64,
    ) -> Vec<(Address, LinkHealth)> {
        let dropped: Vec<Address> = self
            .addresses
            .keys()
            .filter(|address| !keep(address))
            .cloned()
            .collect();
        dropped
            .into_iter()
            .filter_map(|address| {
                let health = self.addresses.remove(&address)?;
                let last = LinkHealth {
                    round_trip_missing: Some(RoundTripMissing::NotSent),
                    age_ms: health
                        .last_heard_us
                        .map(|at| now_us.saturating_sub(at) / 1000),
                    ..LinkHealth::default()
                };
                Some((address, last))
            })
            .collect()
    }

    /// Whether no address is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }
}

impl Health {
    fn bucket(&mut self, now_us: u64) -> &mut Bucket {
        if self
            .buckets
            .back()
            .is_none_or(|b| now_us.saturating_sub(b.start_us) >= BUCKET_US)
        {
            self.buckets.push_back(Bucket {
                start_us: now_us,
                received: 0,
                lost: 0,
            });
        }
        let last = self.buckets.len() - 1;
        &mut self.buckets[last]
    }

    fn sequence(&mut self, component_id: u8, sequence: u8, now_us: u64) {
        let Some(sender) = self
            .senders
            .get_mut(&component_id)
            .filter(|s| now_us.saturating_sub(s.at_us) < SILENCE_US)
        else {
            self.senders.insert(
                component_id,
                Sequence {
                    last: sequence,
                    heard: 1,
                    at_us: now_us,
                },
            );
            self.bucket(now_us).received += 1;
            return;
        };
        sender.at_us = now_us;
        let ahead = sequence.wrapping_sub(sender.last);
        if ahead == 0 {
            return;
        }
        if ahead <= 128 {
            sender.last = sequence;
            sender.heard = if ahead >= 64 {
                1
            } else {
                (sender.heard << ahead) | 1
            };
            let bucket = self.bucket(now_us);
            bucket.received += 1;
            bucket.lost += u32::from(ahead - 1);
            return;
        }
        let behind = sender.last.wrapping_sub(sequence);
        if behind >= REORDER || sender.heard & (1 << behind) != 0 {
            return;
        }
        sender.heard |= 1 << behind;
        self.bucket(now_us).received += 1;
        if let Some(bucket) = self.buckets.iter_mut().rev().find(|b| b.lost > 0) {
            bucket.lost -= 1;
        }
    }

    fn replied(&mut self, ts1: i64, now_us: u64) {
        let Some(at) = self.outstanding.iter().position(|(t, _)| *t == ts1) else {
            return;
        };
        if let Some((_, sent_us)) = self.outstanding.remove(at) {
            self.round_trips
                .push_back((now_us, now_us.saturating_sub(sent_us)));
        }
    }

    fn expire(&mut self, now_us: u64) {
        while self
            .buckets
            .front()
            .is_some_and(|b| now_us.saturating_sub(b.start_us) >= LOSS_WINDOW_US)
        {
            self.buckets.pop_front();
        }
        self.outstanding
            .retain(|(_, sent)| now_us.saturating_sub(*sent) < OUTSTANDING_US);
        while self
            .round_trips
            .front()
            .is_some_and(|(at, _)| now_us.saturating_sub(*at) >= ROUND_TRIP_WINDOW_US)
        {
            self.round_trips.pop_front();
        }
    }
}

fn median(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut values: Vec<u64> = values.collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        u64::midpoint(values[middle - 1], values[middle])
    } else {
        values[middle]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{heartbeat_frame, timesync_frame};

    const GCS: u8 = 255;

    fn address() -> Address {
        ("l1".into(), "127.0.0.1:5760".into(), 1)
    }

    fn reply(ts1: i64, sequence: u8, target_system: u8) -> Vec<u8> {
        timesync_frame((1, 1, sequence), (42, ts1), (target_system, 0))
    }

    #[test]
    fn a_reply_to_our_request_dates_the_round_trip() {
        let mut tracker = Tracker::default();
        let a = address();
        tracker.heard(&a, &heartbeat_frame(1, 1, 0), GCS, 0);
        let ts1 = tracker.asked(&a, 1_000_000);
        assert_eq!(ts1, 1_000_000_000, "our clock in nanoseconds");
        assert_eq!(
            tracker.report(&a, 1_000_000).round_trip_missing,
            Some(RoundTripMissing::Waiting)
        );
        tracker.heard(&a, &reply(ts1, 1, GCS), GCS, 1_012_000);
        let health = tracker.report(&a, 1_012_000);
        assert_eq!(health.round_trip_ms, Some(12.0));
        assert_eq!(health.round_trip_missing, None);
        // Answered once only: the same reply again is not a second sample.
        tracker.heard(&a, &reply(ts1, 2, GCS), GCS, 1_500_000);
        let next = tracker.asked(&a, 2_000_000);
        tracker.heard(&a, &reply(next, 3, 0), GCS, 2_020_000);
        assert_eq!(
            tracker.report(&a, 2_020_000).round_trip_ms,
            Some(16.0),
            "the median of 12 and 20 ms; a reply without the extension counts"
        );
    }

    #[test]
    fn foreign_replies_and_a_px4s_own_requests_are_not_counted() {
        let mut tracker = Tracker::default();
        let a = address();
        let ts1 = tracker.asked(&a, 1_000_000);
        // Another ground station's request, answered on the same channel.
        tracker.heard(&a, &reply(777_000_000, 0, 254), GCS, 1_010_000);
        // A reply with our ts1 addressed to another ground station.
        tracker.heard(&a, &reply(ts1, 1, 254), GCS, 1_011_000);
        // The vehicle asking us (PX4): tc1 0, whatever its ts1.
        tracker.heard(
            &a,
            &timesync_frame((1, 1, 2), (0, ts1), (0, 0)),
            GCS,
            1_012_000,
        );
        // Another vehicle on the same peer answering a request for this one.
        let other = ("l1".into(), "127.0.0.1:5760".into(), 2);
        tracker.heard(
            &other,
            &timesync_frame((2, 1, 0), (42, ts1), (GCS, 0)),
            GCS,
            1_013_000,
        );
        let health = tracker.report(&a, 1_013_000);
        assert_eq!(health.round_trip_ms, None);
        assert_eq!(health.round_trip_missing, Some(RoundTripMissing::Waiting));
        assert_eq!(tracker.report(&other, 1_013_000).round_trip_ms, None);
        // Unanswered past the window, it is said plainly.
        assert_eq!(
            tracker.report(&a, 7_000_000).round_trip_missing,
            Some(RoundTripMissing::NoReply)
        );
    }

    #[test]
    fn the_fake_vehicle_answers_as_ardupilot_does() {
        let mut fake = crate::sim::FakeVehicle::new(1, 0xAB);
        let mut tracker = Tracker::default();
        let a = address();
        let ts1 = tracker.asked(&a, 1_000_000);
        let request = timesync_frame((GCS, crate::GCS_COMPONENT_ID, 0), (0, ts1), (1, 0));
        let replies = fake.answer(&request);
        assert_eq!(replies.len(), 1, "one reply");
        let Some(Message::Timesync {
            tc1,
            ts1: echoed,
            target_system,
            target_component,
        }) = decode(&replies[0]).map(|d| d.message)
        else {
            panic!("a TIMESYNC reply");
        };
        assert!(tc1 > 0, "its own clock");
        assert_eq!(echoed, ts1);
        assert_eq!(
            (target_system, target_component),
            (GCS, crate::GCS_COMPONENT_ID)
        );
        tracker.heard(&a, &replies[0], GCS, 1_003_000);
        assert_eq!(tracker.report(&a, 1_003_000).round_trip_ms, Some(3.0));
        let elsewhere = timesync_frame((GCS, crate::GCS_COMPONENT_ID, 1), (0, 5), (9, 0));
        assert!(fake.answer(&elsewhere).is_empty(), "not for it");
        let a_reply = timesync_frame((GCS, crate::GCS_COMPONENT_ID, 2), (7, 5), (1, 0));
        assert!(fake.answer(&a_reply).is_empty(), "a reply is not a request");
    }

    #[test]
    fn outstanding_requests_are_few_and_expire() {
        let mut tracker = Tracker::default();
        let a = address();
        let first = tracker.asked(&a, 0);
        for i in 1..=u64::try_from(OUTSTANDING).unwrap() {
            tracker.asked(&a, i * 100_000);
        }
        tracker.heard(&a, &reply(first, 0, GCS), GCS, 1_000_000);
        assert_eq!(
            tracker.report(&a, 1_000_000).round_trip_ms,
            None,
            "the oldest made way for newer ones"
        );
        let late = tracker.asked(&a, 2_000_000);
        tracker.heard(&a, &reply(late, 1, GCS), GCS, 2_000_000 + OUTSTANDING_US);
        assert_eq!(
            tracker.report(&a, 2_000_000 + OUTSTANDING_US).round_trip_ms,
            None,
            "given up after its time"
        );
        // Two requests in the same microsecond still differ.
        assert_ne!(tracker.asked(&a, 9_000_000), tracker.asked(&a, 9_000_000));
    }

    #[test]
    fn not_asked_says_why_and_drops_what_was_measured() {
        let mut tracker = Tracker::default();
        let a = address();
        let ts1 = tracker.asked(&a, 0);
        let earlier = tracker.asked(&a, 1);
        tracker.heard(&a, &reply(ts1, 0, GCS), GCS, 5_000);
        tracker.not_asked(&a, RoundTripMissing::Unsupervised);
        let health = tracker.report(&a, 1_000_000);
        assert_eq!(health.round_trip_ms, None);
        assert_eq!(
            health.round_trip_missing,
            Some(RoundTripMissing::Unsupervised)
        );
        assert_eq!(health.age_ms, Some(995), "still heard");
        // A reply to a request made before is not counted any more.
        tracker.heard(&a, &reply(earlier, 1, GCS), GCS, 1_100_000);
        assert_eq!(tracker.report(&a, 1_100_000).round_trip_ms, None);
        tracker.asked(&a, 2_000_000);
        assert_eq!(
            tracker.report(&a, 2_000_000).round_trip_missing,
            Some(RoundTripMissing::Waiting),
            "asking again starts waiting again"
        );
    }

    #[test]
    fn a_forgotten_address_says_last_that_nothing_goes_there() {
        let mut tracker = Tracker::default();
        let (a, kept) = (address(), ("l2".into(), "127.0.0.1:5761".into(), 2));
        tracker.heard(&a, &heartbeat_frame(1, 1, 0), GCS, 0);
        let ts1 = tracker.asked(&a, 1_000_000);
        tracker.heard(&a, &reply(ts1, 1, GCS), GCS, 1_040_000);
        tracker.heard(&kept, &heartbeat_frame(2, 1, 0), GCS, 1_040_000);
        assert_eq!(tracker.report(&a, 1_040_000).round_trip_ms, Some(40.0));
        let dropped = tracker.retain(|address| *address == kept, 2_540_000);
        assert_eq!(
            dropped,
            vec![(
                a.clone(),
                LinkHealth {
                    round_trip_missing: Some(RoundTripMissing::NotSent),
                    age_ms: Some(1500),
                    ..LinkHealth::default()
                }
            )],
            "no round trip or loss, only how old the last frame is"
        );
        assert!(
            tracker
                .retain(|address| *address == kept, 3_000_000)
                .is_empty()
        );
        assert!(!tracker.is_empty());
        assert_eq!(tracker.retain(|_| false, 3_000_000).len(), 1);
        assert!(tracker.is_empty());
    }

    fn hear(tracker: &mut Tracker, sequences: impl IntoIterator<Item = u8>) -> u64 {
        let a = address();
        let mut at = 0;
        for sequence in sequences {
            tracker.heard(&a, &heartbeat_frame(1, 1, sequence), GCS, at);
            at += 20_000;
        }
        at
    }

    #[test]
    fn loss_counts_gaps_across_the_wrap() {
        let mut tracker = Tracker::default();
        // 250 to 255, then 0 to 9: no gap at the wrap.
        let at = hear(&mut tracker, (250..=255).chain(0..10));
        assert_eq!(tracker.report(&address(), at).loss_percent, Some(0.0));
        // 254 and 0 lost across the wrap, then 0 repeated.
        let mut tracker = Tracker::default();
        let sequences = (240..=253).chain([255, 1, 2]).chain([2, 2, 2]);
        let at = hear(&mut tracker, sequences);
        assert_eq!(
            tracker.report(&address(), at).loss_percent,
            Some(2.0 * 100.0 / 19.0),
            "17 heard, 2 lost; the repeated 2 are duplicates"
        );
    }

    #[test]
    fn a_late_frame_fills_its_gap_and_a_duplicate_counts_once() {
        let mut tracker = Tracker::default();
        let at = hear(&mut tracker, [1, 2, 4, 3, 5, 3, 4, 6]);
        assert_eq!(tracker.report(&address(), at).loss_percent, Some(0.0));
        let mut tracker = Tracker::default();
        let at = hear(&mut tracker, [254, 255, 1, 0, 2]);
        assert_eq!(
            tracker.report(&address(), at).loss_percent,
            Some(0.0),
            "late across the wrap"
        );
    }

    #[test]
    fn senders_are_counted_apart_and_a_silence_is_not_a_loss() {
        let mut tracker = Tracker::default();
        let a = address();
        let camera = |sequence| timesync_frame((1, 100, sequence), (0, 1), (0, 0));
        tracker.heard(&a, &heartbeat_frame(1, 1, 7), GCS, 0);
        tracker.heard(&a, &camera(200), GCS, 1_000);
        tracker.heard(&a, &heartbeat_frame(1, 1, 8), GCS, 2_000);
        tracker.heard(&a, &camera(201), GCS, 3_000);
        assert_eq!(tracker.report(&a, 3_000).loss_percent, Some(0.0));
        // Quiet for longer than the silence, back at another number.
        tracker.heard(&a, &heartbeat_frame(1, 1, 200), GCS, 3_500_000);
        let health = tracker.report(&a, 3_600_000);
        assert_eq!(health.loss_percent, Some(0.0), "not 191 lost");
        assert_eq!(health.age_ms, Some(100));
    }

    #[test]
    fn loss_is_over_the_last_ten_seconds() {
        let mut tracker = Tracker::default();
        let a = address();
        tracker.heard(&a, &heartbeat_frame(1, 1, 0), GCS, 0);
        tracker.heard(&a, &heartbeat_frame(1, 1, 10), GCS, 100_000);
        assert_eq!(
            tracker.report(&a, 100_000).loss_percent,
            Some(9.0 * 100.0 / 11.0),
            "2 heard, 9 lost"
        );
        let mut sequence = 10u8;
        let mut at = 200_000;
        while at <= 11_000_000 {
            sequence = sequence.wrapping_add(1);
            tracker.heard(&a, &heartbeat_frame(1, 1, sequence), GCS, at);
            at += 500_000;
        }
        assert_eq!(tracker.report(&a, at).loss_percent, Some(0.0));
        assert_eq!(
            tracker.report(&a, at + LOSS_WINDOW_US).loss_percent,
            None,
            "nothing heard in the window"
        );
    }
}

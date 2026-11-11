//! The browser's flight logs (ADR-0039 R9 in UXDuck Web): each link's
//! frames as received and as sent (ADR-0050 L1), with their times, so a flight flown in this browser
//! replays as one flown with UXDuck Desktop does. Kept in memory up to
//! [`CAP`], the oldest records first to go. The worker also keeps them as
//! tlog files in the origin private file system: it takes what is new with
//! [`Logs::take_unsaved`] and gives them back at the next visit through
//! [`Logs::restore`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What all links keep together: hours of a simulation's telemetry.
pub const CAP: usize = 96 << 20;

#[derive(Debug, Default)]
pub struct Logs {
    links: BTreeMap<String, VecDeque<(u64, Vec<u8>)>>,
    unordered: BTreeSet<String>,
    bytes: usize,
    cap: usize,
    /// Each link's records since the worker last took them, as a tlog.
    unsaved: BTreeMap<String, Vec<u8>>,
    unsaved_bytes: usize,
}

impl Logs {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Self::default()
        }
    }

    /// One frame received or sent on a link at `at_us`.
    pub fn record(&mut self, link_id: &str, at_us: u64, frame: &[u8]) {
        // Not taken for a while (a worker that keeps no files): no more
        // than the cap waits.
        if self.unsaved_bytes <= self.cap {
            let tlog = self.unsaved.entry(link_id.to_owned()).or_default();
            tlog.extend_from_slice(&at_us.to_be_bytes());
            tlog.extend_from_slice(frame);
            self.unsaved_bytes += 8 + frame.len();
        }
        self.keep(link_id, at_us, frame);
    }

    /// A link's tlog kept on an earlier visit, as it was saved: its records
    /// are read again and not saved twice. Give a link's files oldest first.
    pub fn restore(&mut self, link_id: &str, tlog: &[u8]) {
        for (at_us, frame) in uxduck_mavlink::tlog::read(tlog) {
            self.keep(link_id, at_us, frame);
        }
    }

    /// Forgets every record, as when the person clears them.
    pub fn clear(&mut self) {
        let cap = self.cap;
        *self = Self::new(cap);
    }

    /// When a link's last record kept here was received.
    #[must_use]
    pub fn last_us(&self, link_id: &str) -> Option<u64> {
        self.links.get(link_id)?.back().map(|(at, _)| *at)
    }

    /// What was recorded since the last call, each link's as a tlog.
    pub fn take_unsaved(&mut self) -> Vec<(String, Vec<u8>)> {
        self.unsaved_bytes = 0;
        std::mem::take(&mut self.unsaved).into_iter().collect()
    }

    #[must_use]
    pub fn available(&self, link_id: &str, from_us: u64, to_us: u64) -> bool {
        let Some(records) = self.links.get(link_id) else {
            return false;
        };
        if self.unordered.contains(link_id) {
            return records.iter().any(|(at, _)| (from_us..=to_us).contains(at));
        }
        records
            .get(records.partition_point(|(at, _)| *at < from_us))
            .is_some_and(|(at, _)| *at <= to_us)
    }

    fn keep(&mut self, link_id: &str, at_us: u64, frame: &[u8]) {
        let records = self.links.entry(link_id.to_owned()).or_default();
        if records.back().is_some_and(|(at, _)| *at > at_us) {
            self.unordered.insert(link_id.to_owned());
        }
        records.push_back((at_us, frame.to_vec()));
        self.bytes += 8 + frame.len();
        while self.bytes > self.cap {
            // The oldest record of any link goes.
            let Some(oldest) = self
                .links
                .iter()
                .filter_map(|(id, records)| records.front().map(|(at, _)| (*at, id.clone())))
                .min()
                .map(|(_, id)| id)
            else {
                break;
            };
            let Some(records) = self.links.get_mut(&oldest) else {
                break;
            };
            if let Some((_, frame)) = records.pop_front() {
                self.bytes -= 8 + frame.len();
            }
            if records.is_empty() {
                self.links.remove(&oldest);
                self.unordered.remove(&oldest);
            }
        }
    }

    /// A link's records stamped `from_us..=to_us`, as a tlog.
    ///
    /// # Errors
    /// When this browser kept none of them: flown on a visit that kept no
    /// files (a private window), or gone to make room for newer flights.
    pub fn read(&self, link_id: &str, from_us: u64, to_us: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for (at, frame) in self.links.get(link_id).into_iter().flatten() {
            if (from_us..=to_us).contains(at) {
                out.extend_from_slice(&at.to_be_bytes());
                out.extend_from_slice(frame);
            }
        }
        if out.is_empty() {
            return Err(
                "This browser no longer has this flight's telemetry log. Private windows keep none, and the oldest make room for newer flights. Use UXDuck Desktop to keep every flight."
                    .into(),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_links_records_in_range_read_as_a_tlog_and_the_oldest_go_first() {
        let mut logs = Logs::new(3 * (8 + 4));
        logs.record("sim-1", 10, &[1, 1, 1, 1]);
        logs.record("sim-2", 15, &[2, 2, 2, 2]);
        logs.record("sim-1", 20, &[3, 3, 3, 3]);
        let tlog = logs.read("sim-1", 0, 100).unwrap();
        assert_eq!(tlog.len(), 24);
        assert_eq!(&tlog[..8], &10u64.to_be_bytes());
        assert_eq!(logs.read("sim-1", 15, 25).unwrap()[8..], [3, 3, 3, 3]);
        // A fourth over the cap: the oldest, sim-1's first, goes.
        logs.record("sim-2", 30, &[4, 4, 4, 4]);
        assert_eq!(logs.read("sim-1", 0, 100).unwrap().len(), 12);
        assert!(
            logs.read("sim-1", 0, 15)
                .unwrap_err()
                .contains("no longer has this flight")
        );
        assert!(logs.read("sim-3", 0, 100).is_err());
    }

    #[test]
    fn what_is_recorded_is_taken_once_and_restored_on_the_next_visit() {
        let frame = |seq: u8| {
            let mut frame = vec![0xfe, 9, seq, 1, 1, 0];
            frame.extend_from_slice(&[0; 9]);
            frame.extend_from_slice(&[0, 0]);
            frame
        };
        let mut logs = Logs::new(CAP);
        logs.record("sim-1", 10, &frame(1));
        logs.record("sim-2", 15, &frame(2));
        logs.record("sim-1", 20, &frame(3));
        let taken = logs.take_unsaved();
        assert_eq!(
            taken
                .iter()
                .map(|(l, t)| (l.as_str(), t.len()))
                .collect::<Vec<_>>(),
            [("sim-1", 2 * (8 + 17)), ("sim-2", 8 + 17)]
        );
        assert!(logs.take_unsaved().is_empty());
        // The next visit: the files give back what was flown.
        let mut next = Logs::new(CAP);
        for (link, tlog) in &taken {
            next.restore(link, tlog);
        }
        assert_eq!(next.read("sim-1", 0, 100).unwrap(), taken[0].1);
        assert!(next.take_unsaved().is_empty());
        // A torn last record is left out.
        let mut torn = taken[1].1.clone();
        torn.extend_from_slice(&30u64.to_be_bytes());
        torn.extend_from_slice(&frame(4)[..5]);
        let mut again = Logs::new(CAP);
        again.restore("sim-2", &torn);
        assert_eq!(again.read("sim-2", 0, 100).unwrap(), taken[1].1);
    }

    #[test]
    fn records_waiting_to_be_saved_stop_at_the_cap() {
        let mut logs = Logs::new(2 * (8 + 4));
        for at in 0..5 {
            logs.record("sim-1", at, &[1, 2, 3, 4]);
        }
        let waiting: usize = logs.take_unsaved().iter().map(|(_, t)| t.len()).sum();
        assert!(waiting <= 3 * (8 + 4), "{waiting}");
    }

    #[test]
    fn availability_agrees_with_reads_after_eviction_and_for_nonmonotonic_timestamps() {
        let mut logs = Logs::new(3 * (8 + 4));
        logs.record("first", 10, &[1, 2, 3, 4]);
        logs.record("first", 30, &[1, 2, 3, 4]);
        logs.record("first", 20, &[1, 2, 3, 4]);
        for link in ["first", "missing"] {
            for (from, to) in [(0, 10), (20, 20), (25, 29), (30, 50), (0, 100), (20, 10)] {
                assert_eq!(
                    logs.available(link, from, to),
                    logs.read(link, from, to).is_ok()
                );
            }
        }
        logs.record("second", 40, &[1, 2, 3, 4]);
        assert!(!logs.available("first", 0, 10));
        assert!(logs.available("first", 20, 20));
        logs.clear();
        assert!(!logs.available("first", 0, 100));
    }
}

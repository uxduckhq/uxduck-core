//! A flight's telemetry gaps (S460, ADR-0039 "Autopilot logs from
//! anywhere"): the stretches of its armed time in which no connection's
//! log holds anything from its vehicle for longer than UXDuck takes a
//! vehicle to be lost. Measured once, off the engine's task, over the same
//! merged logs its replay reads (R10): as a flight ends, and the first time
//! a flight from before is listed. The result is kept with the flight.

use std::collections::{BTreeSet, VecDeque};
use std::io::Write;

use uxduck_core::domain::flight::{FlightEnd, TelemetryGap, TelemetryGaps};
use uxduck_core::events::Change;
use uxduck_mavlink::session::OFFLINE_AFTER_US;

use crate::Engine;
use crate::replay::Worked;
use crate::runtime::{ReplayRead, ReplayWork, ReplayWorked};

/// Longer than this with nothing from the vehicle is a gap: the time
/// UXDuck takes a vehicle to be lost, so a gap is a stretch in which the
/// vehicle would have shown as offline had its log been all there was.
const GAP_AFTER_US: u64 = OFFLINE_AFTER_US;

/// The flights waiting to be measured, one at a time, so a Logbook of
/// years of flights from before reads one log at once.
#[derive(Debug, Default)]
pub(crate) struct Queue {
    measuring: Option<String>,
    waiting: VecDeque<String>,
    /// Asked for since UXDuck started: measured, waiting, or whose logs
    /// could not be read, which are not read again until it starts again.
    asked: BTreeSet<String>,
}

/// Goes through a flight's telemetry a record at a time, as a tlog written
/// to it, and keeps where its vehicle was silent.
pub(crate) struct Meter {
    system_id: u8,
    from_us: u64,
    to_us: u64,
    /// When the vehicle was last heard, from the arming on.
    heard_us: u64,
    gaps: Vec<TelemetryGap>,
    /// A record not yet whole.
    pending: Vec<u8>,
}

impl Meter {
    /// For the vehicle with `system_id`, armed `from_us` to `to_us`.
    pub(crate) fn new(system_id: u8, (from_us, to_us): (u64, u64)) -> Self {
        Self {
            system_id,
            from_us,
            to_us,
            heard_us: from_us,
            gaps: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// One record. What UXDuck sent, and what other vehicles said, is not
    /// the vehicle heard.
    fn record(&mut self, stamp: u64, frame: &[u8]) {
        if stamp < self.from_us || stamp > self.to_us {
            return;
        }
        if uxduck_mavlink::framing::header(frame).is_none_or(|h| h.system_id != self.system_id) {
            return;
        }
        if stamp > self.heard_us.saturating_add(GAP_AFTER_US) {
            self.gaps.push(TelemetryGap {
                from_us: self.heard_us,
                to_us: stamp,
            });
        }
        self.heard_us = self.heard_us.max(stamp);
    }

    /// The gaps, the one from the last record heard to the flight's end
    /// included.
    pub(crate) fn finish(mut self) -> TelemetryGaps {
        if self.to_us > self.heard_us.saturating_add(GAP_AFTER_US) {
            self.gaps.push(TelemetryGap {
                from_us: self.heard_us,
                to_us: self.to_us,
            });
        }
        TelemetryGaps { gaps: self.gaps }
    }
}

impl Write for Meter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(bytes);
        let mut at = 0;
        // A stamp, then a frame whose length its first three bytes give.
        while pending.len() - at >= 8 + 3 {
            let len = uxduck_mavlink::framing::frame_len(&pending[at + 8..]).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "not a MAVLink frame")
            })?;
            if pending.len() - at < 8 + len {
                break;
            }
            let mut stamp = [0u8; 8];
            stamp.copy_from_slice(&pending[at..at + 8]);
            self.record(u64::from_be_bytes(stamp), &pending[at + 8..at + 8 + len]);
            at += 8 + len;
        }
        pending.drain(..at);
        self.pending = pending;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Engine {
    /// Measures these flights' telemetry gaps in turn, off the engine's
    /// task: each that has ended and is not measured yet, once since UXDuck
    /// started. Called as a flight ends and with each page `flight/list`
    /// gives, so a flight from before is measured the first time it is
    /// listed, never holding up the list.
    pub(crate) fn measure_telemetry_gaps(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            let Some(flight) = self.state.flights.get(&id) else {
                continue;
            };
            if flight.end == FlightEnd::Open || flight.telemetry_gaps.is_some() {
                continue;
            }
            if self.replays.gaps.asked.insert(id.clone()) {
                self.replays.gaps.waiting.push_back(id);
            }
        }
        self.measure_next_gaps();
    }

    /// Starts on the next flight waiting, unless one is being measured. A
    /// host that reads logs off the engine's task does it there, and the
    /// result comes back through [`Engine::replay_worked`]. One that cannot
    /// reads it here.
    fn measure_next_gaps(&mut self) {
        while self.replays.gaps.measuring.is_none() {
            let Some(id) = self.replays.gaps.waiting.pop_front() else {
                return;
            };
            let Some((read, mut meter)) = self.gaps_job(&id) else {
                continue;
            };
            let Some(reader) = self.system.log_reader() else {
                let gaps = self
                    .system
                    .replay_read(&read)
                    .and_then(|bytes| meter.write_all(&bytes).map_err(|e| e.to_string()))
                    .map(|()| meter.finish());
                self.keep_telemetry_gaps(&id, gaps);
                continue;
            };
            self.replays.gaps.measuring = Some(id.clone());
            let work = ReplayWork::new(move || {
                let gaps = reader(&read, &mut meter).map(|_| meter.finish());
                ReplayWorked(Worked::Gaps {
                    flight_id: id,
                    gaps,
                })
            });
            // A host that is stopping does it here.
            if let Some(ReplayWorked(Worked::Gaps { flight_id, gaps })) =
                self.system.replay_work(work)
            {
                self.replays.gaps.measuring = None;
                self.keep_telemetry_gaps(&flight_id, gaps);
            }
        }
    }

    /// A flight's gaps were measured off the engine's task: kept, and the
    /// next flight waiting started on.
    pub(crate) fn telemetry_gaps_measured(
        &mut self,
        flight_id: &str,
        gaps: Result<TelemetryGaps, String>,
    ) {
        if self.replays.gaps.measuring.as_deref() == Some(flight_id) {
            self.replays.gaps.measuring = None;
        }
        self.keep_telemetry_gaps(flight_id, gaps);
        self.measure_next_gaps();
    }

    /// What a flight's measure reads, and the meter it goes through. None
    /// for one whose logs are not here: had from the account, or moved to
    /// a disk that is not plugged in.
    fn gaps_job(&self, id: &str) -> Option<(ReplayRead, Meter)> {
        let flight = self.state.flights.get(id)?;
        let ended = flight.ended_at_us?;
        if !flight.recorded_here && flight.copies.is_empty() {
            return None;
        }
        let system_id = flight
            .system_id
            .or_else(|| self.vehicle(&flight.vehicle_id).map(|v| v.system_id))?;
        let span = (flight.armed_at_us, ended.max(flight.armed_at_us));
        let read = self.flight_replay_read(flight, Some(span))?;
        if let ReplayRead::Packed { path, .. } = &read
            && self.system.file_present(path) == Some(false)
        {
            return None;
        }
        Some((read, Meter::new(system_id, span)))
    }

    /// Keeps a flight's gaps with it. One whose logs could not be read
    /// keeps nothing and is not measured again until UXDuck starts again.
    fn keep_telemetry_gaps(&mut self, flight_id: &str, gaps: Result<TelemetryGaps, String>) {
        let Ok(gaps) = gaps else {
            return;
        };
        let Some(mut flight) = self.state.flights.get(flight_id).cloned() else {
            return;
        };
        if flight.telemetry_gaps.as_ref() == Some(&gaps) {
            return;
        }
        flight.telemetry_gaps = Some(gaps);
        flight.version += 1;
        self.commit_as(
            crate::flight_time::flight_actor(),
            Change::FlightUpserted { flight },
        );
        self.refresh_replay_telemetry_gaps(flight_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heartbeat(system_id: u8) -> Vec<u8> {
        let mut frame = vec![0xFD, 9, 0, 0, 0, system_id, 1, 0, 0, 0];
        frame.extend_from_slice(&[0; 9]);
        frame.extend_from_slice(&[0, 0]);
        frame
    }

    /// A tlog of the vehicle (system 1) heard every second from `from_s`
    /// to `to_s`, but for the seconds `silent` says, with UXDuck (255)
    /// sending every second throughout.
    fn log(from_s: u64, to_s: u64, silent: impl Fn(u64) -> bool) -> Vec<u8> {
        let mut out = Vec::new();
        for s in from_s..=to_s {
            let at = s * 1_000_000;
            if !silent(s) {
                uxduck_mavlink::tlog::record(&mut out, at, &heartbeat(1)).unwrap();
            }
            uxduck_mavlink::tlog::record(&mut out, at + 500, &heartbeat(255)).unwrap();
        }
        out
    }

    fn measure(bytes: &[u8], span_s: (u64, u64), chunk: usize) -> TelemetryGaps {
        let mut meter = Meter::new(1, (span_s.0 * 1_000_000, span_s.1 * 1_000_000));
        for part in bytes.chunks(chunk) {
            meter.write_all(part).unwrap();
        }
        meter.finish()
    }

    const fn s(n: u64) -> u64 {
        n * 1_000_000
    }

    #[test]
    fn a_log_heard_throughout_has_no_gap() {
        let bytes = log(100, 400, |_| false);
        assert_eq!(measure(&bytes, (100, 400), 7), TelemetryGaps::default());
        // Silent for no longer than a vehicle takes to be lost.
        let bytes = log(100, 400, |n| (200..214).contains(&n));
        assert_eq!(measure(&bytes, (100, 400), 4096), TelemetryGaps::default());
    }

    /// What UXDuck sent goes on through a gap, and is not the vehicle.
    #[test]
    fn each_silence_longer_than_a_vehicle_takes_to_be_lost_is_a_gap() {
        let bytes = log(100, 400, |n| {
            (150..190).contains(&n) || (300..320).contains(&n)
        });
        let gaps = measure(&bytes, (100, 400), 13);
        assert_eq!(
            gaps.gaps,
            [
                TelemetryGap {
                    from_us: s(149),
                    to_us: s(190)
                },
                TelemetryGap {
                    from_us: s(299),
                    to_us: s(320)
                },
            ]
        );
        assert_eq!(gaps.missing_us(), s(41 + 21));
    }

    /// A log cut before the vehicle landed misses the rest of its flight,
    /// and one that began late the start.
    #[test]
    fn a_log_cut_short_misses_the_end_and_one_begun_late_the_start() {
        let bytes = log(160, 300, |_| false);
        assert_eq!(
            measure(&bytes, (100, 400), 64).gaps,
            [
                TelemetryGap {
                    from_us: s(100),
                    to_us: s(160)
                },
                TelemetryGap {
                    from_us: s(300),
                    to_us: s(400)
                },
            ]
        );
        assert_eq!(measure(&[], (100, 400), 64).missing_us(), s(300));
    }

    #[test]
    fn what_is_not_a_tlog_is_refused() {
        let mut meter = Meter::new(1, (0, 1));
        assert!(meter.write_all(&[0u8; 32]).is_err());
    }
}

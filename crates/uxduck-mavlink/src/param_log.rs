//! A vehicle's parameters as a log holds them (ADR-0050 L3 to L6): every
//! value read, with when and how, rebuilt from the log's frames and never
//! written back into it. Values come from `PARAM_VALUE` (a list's replies with
//! their index and count, a read, the vehicle's report of a change, the
//! echo of a `PARAM_SET`) and from `@PARAM/param.pck` read over MAVLink FTP,
//! put together from the replies of the session its request opened. A log
//! without the requests (recorded before UXDuck kept what it sent, or by
//! another app) gives a file by its magic at offset 0, marked guessed.

use std::collections::{BTreeMap, BTreeSet};

use crate::decode::{Message, decode};
use crate::transfers::{from_bytewise, ftp, parse_param_pck_partial};

/// How long after a `PARAM_SET` the vehicle's report of that parameter is its
/// echo.
const ECHO_US: u64 = 5_000_000;

/// How far `time_boot_ms` must go back to be a restart, not a late frame.
const REBOOT_MS: u32 = 5_000;

/// How a value was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// One of a list download: its place and the vehicle's count.
    Listed { index: u16, count: u16 },
    /// Reported by the vehicle by itself: a change, or a read by name.
    Reported,
    /// The vehicle's echo of a `PARAM_SET` that system `by` sent.
    Set { by: u8 },
    /// In `param.pck`, read by file, whose header gave `count`. `guessed` when
    /// the request that opened it is not in the log.
    File { count: u16, guessed: bool },
}

impl Source {
    /// Part of a download of the whole set, so a change seen this way was
    /// made out of sight: over another link, or reported in a frame lost.
    #[must_use]
    pub fn downloaded(self) -> bool {
        matches!(self, Self::Listed { .. } | Self::File { .. })
    }
}

/// One value read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub at_us: u64,
    pub value: f32,
    /// `MAV_PARAM_TYPE`.
    pub kind: u8,
    pub default: Option<f32>,
    pub source: Source,
}

/// What one component's log says of its parameters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    /// Each parameter's readings, oldest first.
    pub readings: BTreeMap<String, Vec<Reading>>,
    /// The counts the vehicle gave, with when, oldest first.
    pub counts: Vec<(u64, u16)>,
    /// When it started again, oldest first.
    pub boots: Vec<u64>,
}

/// How a value at a time is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Known {
    /// Read at or before that time.
    Seen,
    /// First read after it, with no change recorded in between.
    Later,
    /// Read before the most recent recorded restart, not current.
    BeforeBoot,
}

/// A parameter's value at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct At {
    pub name: String,
    pub reading: Reading,
    pub known: Known,
}

/// How much of the set a log had read by a time, in the boot it was in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Completeness {
    /// Distinct parameters read since the boot began, by then.
    pub read: u32,
    /// The latest count the vehicle gave by then, in that boot.
    pub total: Option<u16>,
    /// Parameters known only from later in the log.
    pub later: u32,
    /// When the boot it was in began, if the log saw it begin.
    pub boot_us: Option<u64>,
}

impl Completeness {
    /// Every parameter of the count read.
    #[must_use]
    pub fn whole(&self) -> bool {
        self.total
            .is_some_and(|total| total > 0 && self.read >= u32::from(total))
    }
}

/// A value that differs from the one read before it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub name: String,
    pub at_us: u64,
    pub from: f32,
    pub to: f32,
    pub source: Source,
}

impl Timeline {
    /// Each parameter's latest reading by `t_us`, marked if from an earlier
    /// boot, or its first later reading in the same boot unless it is a set
    /// echo whose earlier value is unknown.
    #[must_use]
    pub fn at(&self, t_us: u64) -> Vec<At> {
        let boot = self.boot_at(t_us);
        self.readings
            .iter()
            .filter_map(|(name, readings)| {
                let by_then = readings.partition_point(|r| r.at_us <= t_us);
                let (reading, known) = match by_then {
                    0 => {
                        let first = readings.first()?;
                        if matches!(first.source, Source::Set { .. })
                            || self.boot_at(first.at_us) != boot
                        {
                            return None;
                        }
                        (first, Known::Later)
                    }
                    n => {
                        let reading = &readings[n - 1];
                        let known = if boot.is_some_and(|b| reading.at_us < b) {
                            Known::BeforeBoot
                        } else {
                            Known::Seen
                        };
                        (reading, known)
                    }
                };
                Some(At {
                    name: name.clone(),
                    reading: *reading,
                    known,
                })
            })
            .collect()
    }

    /// How many readings and boot boundaries precede `t_us`: what changes
    /// the usable set, including a reboot with no new readings.
    #[must_use]
    pub fn read_by(&self, t_us: u64) -> usize {
        self.readings
            .values()
            .map(|r| r.partition_point(|r| r.at_us <= t_us))
            .sum::<usize>()
            + self.boots.partition_point(|b| *b <= t_us)
    }

    /// When the boot `t_us` is in began, if the log saw it.
    fn boot_at(&self, t_us: u64) -> Option<u64> {
        self.boots.iter().rev().find(|&&b| b <= t_us).copied()
    }

    /// How complete the set was at `t_us` (L6).
    #[must_use]
    pub fn completeness(&self, t_us: u64) -> Completeness {
        let boot_us = self.boot_at(t_us);
        let from = boot_us.unwrap_or(0);
        let in_boot = |at: u64| at >= from && at <= t_us;
        let mut out = Completeness {
            boot_us,
            total: self
                .counts
                .iter()
                .rev()
                .find(|(at, _)| in_boot(*at))
                .map(|(_, count)| *count),
            ..Completeness::default()
        };
        for readings in self.readings.values() {
            if readings.iter().any(|r| in_boot(r.at_us)) {
                out.read += 1;
            } else if readings.first().is_some_and(|r| {
                r.at_us > t_us
                    && !matches!(r.source, Source::Set { .. })
                    && self.boot_at(r.at_us) == boot_us
            }) {
                out.later += 1;
            }
        }
        out
    }

    /// The changes between `from_us` and `to_us`, oldest first.
    #[must_use]
    pub fn changes(&self, from_us: u64, to_us: u64) -> Vec<Change> {
        let mut out: Vec<Change> = self
            .readings
            .iter()
            .flat_map(|(name, readings)| {
                readings.windows(2).filter_map(move |pair| {
                    let (before, after) = (pair[0], pair[1]);
                    (before.value.to_bits() != after.value.to_bits()
                        && (from_us..=to_us).contains(&after.at_us))
                    .then(|| Change {
                        name: name.clone(),
                        at_us: after.at_us,
                        from: before.value,
                        to: after.value,
                        source: after.source,
                    })
                })
            })
            .collect();
        out.sort_by(|a, b| (a.at_us, &a.name).cmp(&(b.at_us, &b.name)));
        out
    }

    fn read(&mut self, name: String, reading: Reading) {
        self.readings.entry(name).or_default().push(reading);
    }

    fn counted(&mut self, at_us: u64, count: u16) {
        self.counts.push((at_us, count));
    }
}

/// A file being put together from a session's replies, until a refused
/// read (its end), the session's end or the log's. The size its opening
/// gave is not trusted: a vehicle may give the size without defaults.
struct Assembly {
    chunks: BTreeMap<u32, Vec<u8>>,
    guessed: bool,
    last_us: u64,
}

impl Assembly {
    /// The bytes from the start with no gap.
    fn contiguous(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (&offset, data) in &self.chunks {
            let at = u32::try_from(out.len()).unwrap_or(u32::MAX);
            if offset > at {
                break;
            }
            let skip = usize::try_from(at - offset).unwrap_or(usize::MAX);
            out.extend_from_slice(data.get(skip..).unwrap_or_default());
        }
        out
    }
}

/// `param.pck`'s magic, with and without defaults.
fn is_pck(data: &[u8]) -> bool {
    matches!(data.get(..2), Some([0x1b | 0x1c, 0x67]))
}

/// Each component's parameters in a log's records, by system and component.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per message of a download")]
pub fn timelines(records: &[(u64, &[u8])]) -> BTreeMap<(u8, u8), Timeline> {
    let mut out: BTreeMap<(u8, u8), Timeline> = BTreeMap::new();
    // By target system and name: who sent a set, and when.
    let mut sets: BTreeMap<(u8, String), (u8, u64)> = BTreeMap::new();
    // By vehicle and ground station: the path a request opened, and when.
    let mut opening: BTreeMap<(u8, u8), String> = BTreeMap::new();
    // By vehicle, component and session.
    let mut files: BTreeMap<(u8, u8, u8), Assembly> = BTreeMap::new();
    let mut booted: BTreeMap<(u8, u8), u32> = BTreeMap::new();
    let mut boots: BTreeMap<(u8, u8), Vec<u64>> = BTreeMap::new();
    // PX4 sends an integer parameter's bits, not its number, as it does live.
    let mut px4: BTreeSet<(u8, u8)> = BTreeSet::new();
    for &(at_us, frame) in records {
        let Some(decoded) = decode(frame) else {
            continue;
        };
        let (system, component) = (decoded.system_id, decoded.component_id);
        match decoded.message {
            Message::Heartbeat { autopilot, .. } if autopilot.eq_ignore_ascii_case("px4") => {
                px4.insert((system, component));
            }
            Message::GlobalPositionInt { time_boot_ms, .. }
            | Message::Attitude { time_boot_ms, .. } => {
                let last = booted.insert((system, component), time_boot_ms);
                if last.is_some_and(|last| time_boot_ms.saturating_add(REBOOT_MS) < last) {
                    boots.entry((system, component)).or_default().push(at_us);
                }
            }
            Message::ParamSet {
                name,
                target_system,
                ..
            } => {
                sets.insert((target_system, name), (system, at_us));
            }
            Message::ParamValue {
                name,
                value,
                param_type,
                count,
                index,
            } => {
                let echo = sets
                    .remove(&(system, name.clone()))
                    .filter(|(_, at)| at_us.saturating_sub(*at) <= ECHO_US);
                let source = match (echo, index) {
                    (Some((by, _)), _) => Source::Set { by },
                    (None, u16::MAX) => Source::Reported,
                    (None, index) => Source::Listed { index, count },
                };
                let value = if px4.contains(&(system, component)) {
                    from_bytewise(value, param_type)
                } else {
                    value
                };
                let timeline = out.entry((system, component)).or_default();
                if count > 0 && count != u16::MAX {
                    timeline.counted(at_us, count);
                }
                timeline.read(
                    name,
                    Reading {
                        at_us,
                        value,
                        kind: param_type,
                        default: None,
                        source,
                    },
                );
            }
            Message::Ftp {
                target_system,
                payload,
                ..
            } => {
                let answer = ftp::read(&payload);
                match (answer.opcode, answer.req_opcode) {
                    (ftp::OPEN_FILE_RO, _) => {
                        let path = answer.data.split(|b| *b == 0).next().unwrap_or_default();
                        opening.insert(
                            (target_system, system),
                            String::from_utf8_lossy(path).into_owned(),
                        );
                    }
                    (ftp::TERMINATE_SESSION, _) => {
                        if let Some(key) = files
                            .keys()
                            .find(|(v, _, s)| *v == target_system && *s == answer.session)
                            .copied()
                            && let Some(file) = files.remove(&key)
                        {
                            finish(&mut out, (key.0, key.1), &file);
                        }
                    }
                    (ftp::ACK, ftp::OPEN_FILE_RO) => {
                        let opened = opening.remove(&(system, target_system));
                        if opened.is_some_and(|path| path.starts_with("@PARAM/param.pck")) {
                            files.insert(
                                (system, component, answer.session),
                                Assembly {
                                    chunks: BTreeMap::new(),
                                    guessed: false,
                                    last_us: at_us,
                                },
                            );
                        }
                    }
                    (ftp::ACK, ftp::READ_FILE | ftp::BURST_READ_FILE) => {
                        let key = (system, component, answer.session);
                        if !files.contains_key(&key) && answer.offset == 0 && is_pck(&answer.data) {
                            files.insert(
                                key,
                                Assembly {
                                    chunks: BTreeMap::new(),
                                    guessed: true,
                                    last_us: at_us,
                                },
                            );
                        }
                        if let Some(file) = files.get_mut(&key) {
                            file.chunks.insert(answer.offset, answer.data);
                            file.last_us = at_us;
                        }
                    }
                    (ftp::NAK, ftp::READ_FILE | ftp::BURST_READ_FILE) => {
                        if let Some(file) = files.remove(&(system, component, answer.session)) {
                            finish(&mut out, (system, component), &file);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for ((system, component, _), file) in files {
        finish(&mut out, (system, component), &file);
    }
    for (key, at) in boots {
        if let Some(timeline) = out.get_mut(&key) {
            timeline.boots = at;
        }
    }
    for timeline in out.values_mut() {
        timeline.counts.sort_by_key(|(at, _)| *at);
        for readings in timeline.readings.values_mut() {
            readings.sort_by_key(|r| r.at_us);
        }
    }
    out
}

/// A file's parameters, as far as it holds them without a gap.
fn finish(out: &mut BTreeMap<(u8, u8), Timeline>, key: (u8, u8), file: &Assembly) {
    let bytes = file.contiguous();
    if !is_pck(&bytes) || bytes.len() < 6 {
        return;
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]);
    let timeline = out.entry(key).or_default();
    timeline.counted(file.last_us, count);
    for (param, default) in parse_param_pck_partial(&bytes) {
        timeline.read(
            param.name,
            Reading {
                at_us: file.last_us,
                value: param.value,
                kind: param.kind,
                default,
                source: Source::File {
                    count,
                    guessed: file.guessed,
                },
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;
    use crate::transfers::{Param, encode_param_pck};

    const T0: u64 = 1_790_000_000_000_000;
    const S: u64 = 1_000_000;

    fn log(frames: &[(u64, Vec<u8>)]) -> Vec<(u64, &[u8])> {
        frames.iter().map(|(at, f)| (*at, f.as_slice())).collect()
    }

    fn value(name: &str, value: f32, count: u16, index: u16) -> Vec<u8> {
        sim::param_value_frame(1, 1, 0, name, value, 9, count, index)
    }

    #[test]
    fn the_same_count_after_a_reboot_can_be_complete_again() {
        let frames = vec![
            (
                T0,
                sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 60_000),
            ),
            (T0, value("P0", 1.0, 1, 0)),
            (
                T0 + S,
                sim::global_position_at_frame((1, 1), (0, 0), (0, 0), 1_000),
            ),
            (T0 + 2 * S, value("P0", 2.0, 1, 0)),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        assert!(timeline.completeness(T0).whole());
        assert!(!timeline.completeness(T0 + S).whole());
        assert!(timeline.completeness(T0 + 2 * S).whole());
    }

    #[test]
    fn a_px4_integer_is_read_as_its_number_and_an_ardupilot_one_as_sent() {
        let frames = vec![
            (T0, sim::px4_heartbeat_frame(1, 0, 0)),
            (
                T0 + S,
                sim::param_value_frame(1, 1, 1, "SYS_AUTOSTART", f32::from_bits(10040), 6, 1, 0),
            ),
            (
                T0 + S,
                sim::param_value_frame(2, 1, 0, "FRAME_CLASS", 1.0, 6, 1, 0),
            ),
        ];
        let found = timelines(&log(&frames));
        assert_eq!(found[&(1, 1)].at(T0 + S)[0].reading.value, 10040.0);
        assert_eq!(found[&(2, 1)].at(T0 + S)[0].reading.value, 1.0);
    }

    #[test]
    fn a_first_set_echo_does_not_invent_the_value_before_the_write() {
        let frames = vec![
            (T0 + S, sim::param_set_frame(255, 1, 1, 0, "P0", 2.0, 9)),
            (T0 + 2 * S, value("P0", 2.0, 1, 0)),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        assert!(timeline.at(T0).is_empty());
        assert_eq!(timeline.completeness(T0).later, 0);
        assert_eq!(timeline.at(T0 + 2 * S)[0].reading.value, 2.0);
    }

    #[test]
    fn a_value_first_seen_after_a_reboot_is_not_a_value_of_the_previous_boot() {
        let frames = vec![
            (
                T0,
                sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 60_000),
            ),
            (
                T0 + S,
                sim::global_position_at_frame((1, 1), (0, 0), (0, 0), 1_000),
            ),
            (T0 + 2 * S, value("NEW_BOOT", 2.0, 1, 0)),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        assert!(timeline.at(T0).is_empty());
        assert_eq!(timeline.completeness(T0).later, 0);
        assert_eq!(timeline.at(T0 + S)[0].known, Known::Later);
    }

    #[test]
    fn a_companions_boot_clock_does_not_restart_the_autopilots_parameters() {
        let position = sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 1_000);
        let companion = crate::custom::frame_from(
            (1, 191, 0),
            33,
            crate::framing::payload(&position).unwrap_or_default(),
            104,
        );
        let frames = vec![
            (
                T0,
                sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 60_000),
            ),
            (T0, value("P0", 1.0, 1, 0)),
            (T0 + S, companion),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        assert!(timeline.boots.is_empty());
        assert!(timeline.completeness(T0 + S).whole());
    }

    fn ftp(from: (u8, u8), to: (u8, u8), parts: (u8, u8, u8, u32), data: &[u8]) -> Vec<u8> {
        let (session, opcode, req_opcode, offset) = parts;
        let mut payload = [0u8; 251];
        payload[2] = session;
        payload[3] = opcode;
        payload[4] = u8::try_from(data.len()).unwrap_or(u8::MAX);
        payload[5] = req_opcode;
        payload[8..12].copy_from_slice(&offset.to_le_bytes());
        payload[12..12 + data.len()].copy_from_slice(data);
        sim::ftp_frame(from, to, 0, payload)
    }

    fn pck() -> Vec<u8> {
        let params: Vec<WithDefaultOwned> = (0..30)
            .map(|i| {
                (
                    Param {
                        name: format!("PARAM_{i:02}"),
                        value: f32::from(u8::try_from(i).unwrap_or(0)),
                        kind: 9,
                    },
                    Some(1.0),
                )
            })
            .collect();
        encode_param_pck(&params)
    }

    type WithDefaultOwned = crate::transfers::WithDefault;

    /// The file's replies, from offset 0 in chunks of 239.
    fn replies(at: u64, file: &[u8], session: u8, skip: Option<usize>) -> Vec<(u64, Vec<u8>)> {
        file.chunks(239)
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .map(|(i, chunk)| {
                let offset = u32::try_from(i * 239).unwrap_or(0);
                let parts = (session, ftp::ACK, ftp::BURST_READ_FILE, offset);
                (at, ftp((1, 1), (255, 190), parts, chunk))
            })
            .collect()
    }

    #[test]
    fn a_list_download_is_whole_from_its_end_and_known_later_before_it() {
        let frames: Vec<(u64, Vec<u8>)> = (0..3u16)
            .map(|i| (T0 + 10 * S, value(&format!("P{i}"), f32::from(i), 3, i)))
            .collect();
        let all = timelines(&log(&frames));
        let timeline = &all[&(1, 1)];
        let after = timeline.completeness(T0 + 11 * S);
        assert_eq!((after.read, after.total, after.whole()), (3, Some(3), true));
        let before = timeline.completeness(T0);
        assert_eq!((before.read, before.later, before.whole()), (0, 3, false));
        let at = timeline.at(T0);
        assert!(at.iter().all(|a| a.known == Known::Later), "{at:?}");
        assert!(matches!(
            at[0].reading.source,
            Source::Listed { index: 0, count: 3 }
        ));
    }

    #[test]
    fn replies_alone_with_one_lost_stay_partial() {
        let frames = vec![(T0, value("P0", 0.0, 3, 0)), (T0, value("P2", 2.0, 3, 2))];
        let c = timelines(&log(&frames))[&(1, 1)].completeness(T0 + S);
        assert_eq!((c.read, c.total, c.whole()), (2, Some(3), false));
    }

    #[test]
    fn a_change_is_set_by_its_sender_and_the_value_follows_the_clock() {
        let frames = vec![
            (T0, value("RTL_ALT", 1500.0, 2, 0)),
            (T0, value("WPNAV_SPEED", 500.0, 2, 1)),
            (
                T0 + 20 * S,
                sim::param_set_frame(255, 1, 1, 0, "RTL_ALT", 3000.0, 9),
            ),
            (T0 + 20 * S + 50_000, value("RTL_ALT", 3000.0, 2, u16::MAX)),
            (T0 + 40 * S, value("WPNAV_SPEED", 800.0, 2, u16::MAX)),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        let rtl = |t: u64| {
            timeline
                .at(t)
                .into_iter()
                .find(|a| a.name == "RTL_ALT")
                .map(|a| a.reading.value)
        };
        assert_eq!(rtl(T0 + 10 * S), Some(1500.0));
        assert_eq!(rtl(T0 + 30 * S), Some(3000.0));
        let changes = timeline.changes(0, u64::MAX);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].name, "RTL_ALT");
        assert_eq!((changes[0].from, changes[0].to), (1500.0, 3000.0));
        assert_eq!(changes[0].source, Source::Set { by: 255 });
        assert_eq!(changes[1].source, Source::Reported);
        assert_eq!(timeline.changes(T0 + 30 * S, u64::MAX).len(), 1);
    }

    #[test]
    fn a_file_download_is_put_together_from_the_session_its_request_opened() {
        let file = pck();
        let mut frames = vec![
            (
                T0,
                ftp(
                    (255, 190),
                    (1, 1),
                    (0, ftp::OPEN_FILE_RO, 0, 0),
                    ftp::PARAM_PATH.as_bytes(),
                ),
            ),
            (
                T0,
                ftp(
                    (1, 1),
                    (255, 190),
                    (3, ftp::ACK, ftp::OPEN_FILE_RO, 0),
                    // The size without defaults, as a vehicle may give it.
                    &u32::try_from(file.len() / 2).unwrap_or(0).to_le_bytes(),
                ),
            ),
        ];
        frames.extend(replies(T0 + S, &file, 3, None));
        let end = u32::try_from(file.len()).unwrap_or(0);
        frames.push((
            T0 + S,
            ftp(
                (1, 1),
                (255, 190),
                (3, ftp::NAK, ftp::BURST_READ_FILE, end),
                &[ftp::EOF],
            ),
        ));
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        let c = timeline.completeness(T0 + 2 * S);
        assert_eq!((c.read, c.total, c.whole()), (30, Some(30), true));
        let at = timeline.at(T0 + 2 * S);
        assert_eq!(at[7].name, "PARAM_07");
        assert_eq!(at[7].reading.value, 7.0);
        assert_eq!(at[7].reading.default, Some(1.0));
        assert_eq!(
            at[7].reading.source,
            Source::File {
                count: 30,
                guessed: false
            }
        );
    }

    #[test]
    fn a_file_without_its_request_is_guessed_by_its_magic_and_a_gap_ends_it() {
        let file = pck();
        assert!(file.len() > 239, "more than one reply");
        let whole = timelines(&log(&replies(T0, &file, 2, None)));
        let at = whole[&(1, 1)].at(T0);
        assert_eq!(at.len(), 30);
        assert!(matches!(
            at[0].reading.source,
            Source::File { guessed: true, .. }
        ));
        let gap = timelines(&log(&replies(T0, &file, 2, Some(1))));
        let c = gap[&(1, 1)].completeness(T0);
        assert!(c.read < 30 && c.total == Some(30), "{c:?}");
    }

    #[test]
    fn a_reboot_starts_the_count_again() {
        let position =
            |boot_ms: u32| sim::global_position_at_frame((1, 0), (0, 0), (0, 0), boot_ms);
        let frames = vec![
            (T0, position(60_000)),
            (T0, value("P0", 0.0, 2, 0)),
            (T0, value("P1", 1.0, 2, 1)),
            (T0 + 10 * S, position(70_000)),
            (T0 + 20 * S, position(1_000)),
            (T0 + 30 * S, value("P0", 0.0, 3, 0)),
        ];
        let timeline = &timelines(&log(&frames))[&(1, 1)];
        assert_eq!(timeline.boots, [T0 + 20 * S]);
        let before = timeline.completeness(T0 + 15 * S);
        assert_eq!(
            (before.read, before.total, before.whole()),
            (2, Some(2), true)
        );
        let after = timeline.completeness(T0 + 31 * S);
        assert_eq!(
            (after.read, after.total, after.boot_us),
            (1, Some(3), Some(T0 + 20 * S))
        );
        // Values carry over by name.
        assert_eq!(timeline.at(T0 + 31 * S).len(), 2);
    }

    #[test]
    fn what_a_ground_station_sends_is_never_a_value() {
        let frames = vec![
            (T0, sim::param_set_frame(255, 1, 1, 0, "RTL_ALT", 3000.0, 9)),
            (T0, sim::param_request_list_frame(255, 1, 1, 0)),
        ];
        assert!(timelines(&log(&frames)).is_empty());
    }
}

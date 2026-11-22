//! Telemetry logs in the de facto tlog format: each frame's exact bytes,
//! prefixed with the time it was received or sent as 8 big-endian bytes of
//! microseconds since the Unix epoch. Mission Planner and pymavlink read
//! these. A record says nothing of its direction: UXDuck's tlogs hold what
//! it sent beside what it heard (ADR-0050 L1), as Mission Planner's do, and
//! a frame's sender tells them apart ([`without_ground_stations`]).

use std::io::Write;

/// Appends one frame, received or sent at `at_us`.
///
/// # Errors
/// When the writer fails.
pub fn record(out: &mut impl Write, at_us: u64, frame: &[u8]) -> std::io::Result<()> {
    out.write_all(&at_us.to_be_bytes())?;
    out.write_all(frame)
}

/// Telemetry safe for UDP replay: recognized, checksum-valid telemetry
/// from an autopilot identified by a recorded heartbeat. Commands,
/// transfers, unknown messages and unidentified senders are excluded even
/// when no GCS heartbeat was recorded. Component 190 and systems with a
/// GCS heartbeat are always excluded. This is not a lossless log reader.
#[must_use]
pub fn without_ground_stations(records: Vec<(u64, &[u8])>) -> Vec<(u64, &[u8])> {
    use crate::decode::{Message, decode};
    let mut stations = std::collections::BTreeSet::new();
    let mut autopilots = std::collections::BTreeSet::new();
    for (_, frame) in &records {
        if crate::framing::header(frame).is_none_or(|h| h.message_id != 0) {
            continue;
        }
        let Some(decoded) = decode(frame) else {
            continue;
        };
        if let Message::Heartbeat {
            mav_type,
            autopilot,
            ..
        } = decoded.message
        {
            if mav_type == "Gcs" {
                stations.insert(decoded.system_id);
            } else if crate::autopilot::flight_controller(
                &mav_type,
                &autopilot,
                decoded.component_id,
            ) {
                autopilots.insert((decoded.system_id, decoded.component_id));
            }
        }
    }
    records
        .into_iter()
        .filter(|(_, frame)| {
            let Some(header) = crate::framing::header(frame) else {
                return false;
            };
            if header.component_id == crate::GCS_COMPONENT_ID
                || stations.contains(&header.system_id)
                || !autopilots.contains(&(header.system_id, header.component_id))
            {
                return false;
            }
            decode(frame).is_some_and(|decoded| {
                matches!(
                    decoded.message,
                    Message::Heartbeat { .. }
                        | Message::AutopilotVersion { .. }
                        | Message::AvailableModes { .. }
                        | Message::AvailableModesMonitor { .. }
                        | Message::GlobalPositionInt { .. }
                        | Message::GpsRawInt { .. }
                        | Message::Attitude { .. }
                        | Message::AttitudeQuaternion { .. }
                        | Message::SysStatus { .. }
                        | Message::VfrHud { .. }
                        | Message::ServoOutputs { .. }
                        | Message::RcChannels { .. }
                        | Message::MissionCurrent { .. }
                        | Message::MissionItemReached { .. }
                        | Message::StatusText { .. }
                        | Message::NavControllerOutput { .. }
                        | Message::RadioStatus { .. }
                        | Message::EkfStatus { .. }
                        | Message::EstimatorStatus { .. }
                        | Message::Vibration { .. }
                        | Message::BatteryStatus { .. }
                        | Message::DistanceSensor { .. }
                        | Message::Rangefinder { .. }
                        | Message::TerrainReport { .. }
                        | Message::FenceStatus { .. }
                        | Message::ExtendedSysState { .. }
                        | Message::Wind { .. }
                        | Message::AirspeedAutocal { .. }
                        | Message::ScaledPressure2 { .. }
                        | Message::OnboardComputerStatus(_)
                        | Message::PositionTarget { .. }
                        | Message::HomePosition { .. }
                        | Message::GpsGlobalOrigin { .. }
                        | Message::ParamValue { .. }
                        | Message::VideoStream { .. }
                        | Message::GimbalAttitude { .. }
                        | Message::MountStatus { .. }
                        | Message::AdsbVehicle { .. }
                        | Message::MagCalProgress { .. }
                        | Message::MagCalReport { .. }
                )
            })
        })
        .collect()
}

/// Reads records back, for replay and tests. Stops at the first record that
/// is not a whole frame.
#[must_use]
pub fn read(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    records(bytes).collect()
}

/// The records [`read`] gives, one at a time: a long log has millions, and
/// most of its readers only pass over them.
pub fn records(bytes: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at + 8 >= bytes.len() {
            return None;
        }
        let mut stamp = [0u8; 8];
        stamp.copy_from_slice(&bytes[at..at + 8]);
        let start = at + 8;
        let len = crate::framing::frame_len(&bytes[start..])?;
        if start + len > bytes.len() {
            return None;
        }
        at = start + len;
        Some((u64::from_be_bytes(stamp), &bytes[start..start + len]))
    })
}

/// The first of the year 2000, in microseconds since the Unix epoch:
/// a stamp before it is nonsense (a recorder without a clock), and a replay
/// plays such a log on the vehicle's own time since boot instead.
pub const SANE_FROM_US: u64 = 946_684_800_000_000;

/// Whether a record's stamp is a time a computer could have written.
#[must_use]
pub fn sane(stamp_us: u64) -> bool {
    // Before 2100.
    (SANE_FROM_US..4_102_444_800_000_000).contains(&stamp_us)
}

/// The record at `at`, if one starts there: its stamp and its length with
/// the stamp.
fn record_at(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let start = at.checked_add(8)?;
    let stamp = u64::from_be_bytes(bytes.get(at..start)?.try_into().ok()?);
    let len = crate::framing::frame_len(bytes.get(start..)?)?;
    (start + len <= bytes.len()).then_some((stamp, 8 + len))
}

/// Where whole records start again in bytes read from the middle of a
/// tlog: the first offset from which three records follow each other with
/// sane stamps that do not go back by more than a minute. None when there
/// is no such place.
#[must_use]
pub fn resync(bytes: &[u8]) -> Option<usize> {
    let chained = |mut at: usize| {
        let mut last: Option<u64> = None;
        for _ in 0..3 {
            let Some((stamp, len)) = record_at(bytes, at) else {
                // The end of what was read: a whole record that reaches it
                // counts.
                return last.is_some() && at == bytes.len();
            };
            if !sane(stamp) || last.is_some_and(|l| stamp + 60_000_000 < l) {
                return false;
            }
            last = Some(stamp);
            at += len;
        }
        true
    };
    (0..bytes.len()).find(|&at| chained(at))
}

/// The offset of the first whole record stamped at or after `from_us` in
/// bytes read from anywhere in a tlog (ADR-0039 R10): resynchronised
/// first, then record by record. `bytes.len()` when every record is
/// earlier; None when no records start in them.
#[must_use]
pub fn seek(bytes: &[u8], from_us: u64) -> Option<usize> {
    let mut at = resync(bytes)?;
    while let Some((stamp, len)) = record_at(bytes, at) {
        if stamp >= from_us {
            return Some(at);
        }
        at += len;
    }
    Some(at)
}

/// How long after one connection brought a frame the same frame on another
/// is still that frame. A vehicle's router sends every frame down each of
/// its connections, and a 4G modem in poor coverage holds them for seconds
/// before delivering them all. A later frame that matches only because its
/// 8-bit sequence wrapped round says the same as the one before, so taking
/// it for a copy loses nothing.
pub const COPY_WITHIN_US: u64 = 30_000_000;

/// How far a vehicle's own time may go back between connections and still
/// be a copy or a late frame rather than the vehicle starting again: a
/// reboot sets it back to nearly zero, and a vehicle is up much longer
/// than this in flight.
pub const BACK_IS_REBOOT_US: u64 = 60_000_000;

/// The vehicle's own time a frame carries, in microseconds, for the
/// messages the maps place and turn a vehicle by: `GPS_RAW_INT`'s
/// `time_usec`, and the `time_boot_ms` that begins `ATTITUDE`,
/// `ATTITUDE_QUATERNION`, `LOCAL_POSITION_NED` and `GLOBAL_POSITION_INT`.
/// MAVLink 2 drops trailing zeros, so a short payload reads as zeros.
fn vehicle_time_us(message_id: u32, payload: &[u8]) -> Option<u64> {
    let field = |n: usize| {
        let mut bytes = [0u8; 8];
        let have = payload.len().min(n);
        bytes[..have].copy_from_slice(&payload[..have]);
        u64::from_le_bytes(bytes)
    };
    match message_id {
        24 => Some(field(8)),
        30..=33 => Some(field(4) * 1_000),
        _ => None,
    }
}

/// One vehicle's logs from several connections as one (ADR-0039 R10,
/// 2026-11-22). A vehicle heard on a radio and on 4G at once is in both
/// connections' logs, each frame at the time it arrived there, and each log
/// misses what its connection dropped. Records offered in time order come
/// out once each: a frame another log brought within [`COPY_WITHIN_US`] is
/// its copy, and a position or attitude older than the last one taken from
/// another log arrived late and would step the vehicle back. Each log's own
/// records are never compared with each other, so one log comes out whole.
#[derive(Debug, Default)]
pub struct Merge {
    /// Each frame taken lately, by its sender, message, sequence and
    /// payload: which log brought it and when.
    recent: std::collections::HashMap<Vec<u8>, (usize, u64)>,
    /// The same, oldest first, to let them go.
    order: std::collections::VecDeque<(u64, Vec<u8>)>,
    /// By sender and message: the vehicle's own time of the last taken,
    /// and its log.
    timed: std::collections::HashMap<(u8, u8, u32), (u64, usize)>,
}

impl Merge {
    /// Whether the record from log `log` at `stamp` is kept. Records are
    /// offered in time order.
    pub fn keeps(&mut self, log: usize, stamp: u64, frame: &[u8]) -> bool {
        while self
            .order
            .front()
            .is_some_and(|(at, _)| at.saturating_add(COPY_WITHIN_US) < stamp)
        {
            if let Some((at, key)) = self.order.pop_front()
                && self.recent.get(&key).is_some_and(|(_, kept)| *kept == at)
            {
                self.recent.remove(&key);
            }
        }
        let (Some(header), Some(payload)) = (
            crate::framing::header(frame),
            crate::framing::payload(frame),
        ) else {
            return true;
        };
        let mut key = Vec::with_capacity(6 + payload.len());
        key.extend_from_slice(&[header.system_id, header.component_id, header.sequence]);
        key.extend_from_slice(&header.message_id.to_le_bytes()[..3]);
        key.extend_from_slice(payload);
        if self.recent.get(&key).is_some_and(|(from, _)| *from != log) {
            return false;
        }
        if let Some(time) = vehicle_time_us(header.message_id, payload) {
            let sender = (header.system_id, header.component_id, header.message_id);
            if let Some(&(last, from)) = self.timed.get(&sender)
                && from != log
                && time <= last
                && last - time < BACK_IS_REBOOT_US
            {
                return false;
            }
            self.timed.insert(sender, (time, log));
        }
        self.recent.insert(key.clone(), (log, stamp));
        self.order.push_back((stamp, key));
        true
    }
}

/// Several connections' logs of the same vehicles as one log, in time
/// order, as [`Merge`] keeps them. Records at the same time come in the
/// order of `logs`.
#[must_use]
pub fn merge(logs: &[&[u8]]) -> Vec<u8> {
    let mut all: Vec<(u64, usize, &[u8])> = logs
        .iter()
        .enumerate()
        .flat_map(|(i, log)| records(log).map(move |(stamp, frame)| (stamp, i, frame)))
        .collect();
    all.sort_by_key(|&(stamp, i, _)| (stamp, i));
    let mut merge = Merge::default();
    let mut out = Vec::with_capacity(logs.iter().map(|l| l.len()).max().unwrap_or(0));
    for (stamp, i, frame) in all {
        if merge.keeps(i, stamp, frame) {
            let _ = record(&mut out, stamp, frame);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;

    const T0: u64 = 1_760_000_000_000_000;

    /// A position `ms` into the flight, as sequence `seq`.
    fn at(seq: u8, ms: u32) -> Vec<u8> {
        sim::global_position_at_frame((1, seq), (i32::try_from(ms).unwrap_or(0), 0), (0, 0), ms)
    }

    /// The positions' times since boot, in the order they come.
    fn times(log: &[u8]) -> Vec<u32> {
        read(log)
            .iter()
            .filter_map(|(_, f)| crate::framing::payload(f))
            .map(|p| u32::from_le_bytes(p[..4].try_into().unwrap_or_default()))
            .collect()
    }

    #[test]
    fn a_connection_that_alone_hears_the_vehicle_fills_the_others_gap_once() {
        // The radio drops 3 to 6. 4G brings every one 100 ms behind it.
        let (mut radio, mut cell) = (Vec::new(), Vec::new());
        for i in 0..10u8 {
            let ms = u32::from(i) * 1_000;
            let stamp = T0 + u64::from(ms) * 1_000;
            if !(3..=6).contains(&i) {
                record(&mut radio, stamp, &at(i, ms)).unwrap_or_default();
            }
            record(&mut cell, stamp + 100_000, &at(i, ms)).unwrap_or_default();
        }
        assert_eq!(
            times(&merge(&[&radio, &cell])),
            [
                0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000, 7_000, 8_000, 9_000
            ]
        );
    }

    #[test]
    fn a_position_that_arrives_after_a_newer_one_is_left_out() {
        // The radio drops 4 to 6, and 4G brings them 400 ms late, after
        // the radio's 7: taking them would step the vehicle back.
        let (mut radio, mut cell) = (Vec::new(), Vec::new());
        for i in 0..10u8 {
            let ms = u32::from(i) * 100;
            if !(4..=6).contains(&i) {
                record(&mut radio, T0 + u64::from(ms) * 1_000, &at(i, ms)).unwrap_or_default();
            }
            record(&mut cell, T0 + (u64::from(ms) + 400) * 1_000, &at(i, ms)).unwrap_or_default();
        }
        assert_eq!(
            times(&merge(&[&radio, &cell])),
            [0, 100, 200, 300, 700, 800, 900]
        );
    }

    #[test]
    fn one_log_merged_alone_comes_out_whole() {
        let mut log = Vec::new();
        for i in 0..5u64 {
            // The same heartbeat again and again, and a repeated position.
            record(&mut log, T0 + i, &sim::heartbeat_frame(1, 1, 0)).unwrap_or_default();
            record(&mut log, T0 + i, &at(0, 0)).unwrap_or_default();
        }
        assert_eq!(merge(&[&log]), log);
    }

    #[test]
    fn a_reboot_heard_on_the_other_connection_starts_the_vehicle_again() {
        let (mut radio, mut cell) = (Vec::new(), Vec::new());
        record(&mut radio, T0, &at(0, 120_000)).unwrap_or_default();
        record(&mut cell, T0 + 5_000_000, &at(1, 500)).unwrap_or_default();
        assert_eq!(times(&merge(&[&radio, &cell])), [120_000, 500]);
    }

    #[test]
    fn replay_rejects_commands_without_a_ground_station_heartbeat() {
        let command = sim::command_long_frame(200, 1, 1, 0, 400, [1.0; 7], 0);
        let command = crate::custom::frame_from(
            (200, 1, 0),
            76,
            crate::framing::payload(&command).unwrap_or_default(),
            152,
        );
        assert!(crate::decode::decode(&command).is_some());
        let heartbeat = sim::heartbeat_frame(1, 1, 0);
        let frames = vec![(T0, heartbeat.as_slice()), (T0 + 1, command.as_slice())];
        assert_eq!(
            without_ground_stations(frames),
            [(T0, heartbeat.as_slice())]
        );
    }

    #[test]
    fn replay_excludes_control_and_transfer_families_from_identified_senders() {
        use mavio::dialects::ardupilotmega::Ardupilotmega;
        use mavio::protocol::Dialect;
        let heartbeat = sim::heartbeat_frame(1, 1, 0);
        let route = sim::Route {
            from: (1, 1),
            to: (1, 1),
        };
        let frames = [
            sim::command_int_frame(
                1,
                1,
                1,
                0,
                192,
                sim::IntPlace {
                    frame: 6,
                    params: [0.0; 4],
                    x: 0,
                    y: 0,
                    z: 1.0,
                },
            ),
            sim::param_set_frame(1, 1, 1, 0, "P0", 1.0, 9),
            sim::param_request_list_frame(1, 1, 1, 0),
            sim::manual_control_frame(1, 1, 0, [0, 0, 500, 0], 0),
            sim::rc_override_frame(1, 1, 1, 0, [1500; 18]),
            sim::mission_count_frame(route, 0, 1, 0),
            sim::ftp_frame((1, 1), (1, 1), 0, [0; 251]),
            sim::command_ack_frame(1, 1, (1, 1), 0, 400, 0),
        ];
        for frame in frames {
            let header = crate::framing::header(&frame).unwrap();
            let extra = Ardupilotmega::message_info(header.message_id)
                .unwrap()
                .crc_extra();
            let frame = crate::custom::frame_from(
                (1, 1, 0),
                header.message_id,
                crate::framing::payload(&frame).unwrap(),
                extra,
            );
            assert!(crate::decode::decode(&frame).is_some());
            assert_eq!(
                without_ground_stations(vec![(T0, &heartbeat), (T0, &frame)]),
                [(T0, heartbeat.as_slice())]
            );
        }
    }

    #[test]
    fn replay_rejects_commands_even_from_an_identified_vehicle_and_unknown_records() {
        let heartbeat = sim::heartbeat_frame(1, 1, 0);
        let command = sim::command_long_frame(1, 1, 1, 0, 400, [1.0; 7], 0);
        let command = crate::custom::frame_from(
            (1, 1, 0),
            76,
            crate::framing::payload(&command).unwrap_or_default(),
            152,
        );
        let unknown = crate::custom::frame_from((1, 1, 0), 50_000, &[1, 2, 3], 0);
        let unseen = sim::heartbeat_frame(2, 1, 0);
        let telemetry = sim::global_position_at_frame((2, 0), (0, 0), (0, 0), 1000);
        let mut bad = heartbeat.clone();
        bad[10] ^= 1;
        let frames = vec![
            (T0, heartbeat.as_slice()),
            (T0, command.as_slice()),
            (T0, unknown.as_slice()),
            (T0, telemetry.as_slice()),
            (T0, bad.as_slice()),
        ];
        assert_eq!(
            without_ground_stations(frames),
            [(T0, heartbeat.as_slice())]
        );
        assert_eq!(
            without_ground_stations(vec![(T0, unseen.as_slice()), (T0, telemetry.as_slice())])
                .len(),
            2
        );
    }

    fn log(n: u64) -> Vec<u8> {
        let mut log = Vec::new();
        for i in 0..n {
            let frame = sim::heartbeat_frame(1, 1, u8::try_from(i % 256).unwrap_or(0));
            record(&mut log, T0 + i * 100_000, &frame).unwrap_or_default();
        }
        log
    }

    #[test]
    fn what_ground_stations_sent_is_told_from_what_vehicles_did() {
        let mut bytes = Vec::new();
        let frames = [
            sim::heartbeat_frame(1, 1, 0),
            sim::gcs_heartbeat_frame(255, 190, 0),
            sim::command_long_frame(255, 1, 1, 1, 400, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0),
            // A ground station on another component, known by its heartbeat.
            sim::gcs_heartbeat_frame(200, 1, 0),
            sim::param_request_list_frame(200, 1, 1, 1),
            sim::heartbeat_frame(2, 1, 0),
        ];
        for (at, frame) in (T0..).zip(&frames) {
            record(&mut bytes, at, frame).unwrap_or_default();
        }
        let left: Vec<u8> = without_ground_stations(read(&bytes))
            .iter()
            .filter_map(|(_, f)| crate::framing::header(f).map(|h| h.system_id))
            .collect();
        assert_eq!(left, [1, 2]);
    }

    #[test]
    fn a_seek_from_the_middle_finds_the_record_at_its_time() {
        let bytes = log(100);
        let size = bytes.len() / 100;
        // From inside the 10th record: the 11th is the first whole one.
        let cut = &bytes[size * 10 + 3..];
        let at = seek(cut, T0 + 50 * 100_000).unwrap_or_default();
        let first = read(&cut[at..]);
        assert_eq!(first.first().map(|r| r.0), Some(T0 + 50 * 100_000));
        // Before them all: the first whole record.
        let at = seek(cut, 0).unwrap_or_default();
        assert_eq!(
            read(&cut[at..]).first().map(|r| r.0),
            Some(T0 + 11 * 100_000)
        );
        // After them all: the end.
        assert_eq!(seek(cut, u64::MAX), Some(cut.len()));
        assert_eq!(seek(&[0u8; 40], 0), None);
    }

    #[test]
    fn stamps_before_2000_are_not_sane() {
        assert!(!sane(0));
        assert!(!sane(1_000_000));
        assert!(sane(T0));
    }

    #[test]
    fn frames_come_back_byte_for_byte() {
        let a = sim::heartbeat_frame(1, 1, 0);
        let b = sim::autopilot_version_frame(1, 1, 1, 42, 0);
        let mut log = Vec::new();
        record(&mut log, 1_000, &a).unwrap_or_default();
        record(&mut log, 2_000, &b).unwrap_or_default();
        assert_eq!(read(&log), [(1_000, a.as_slice()), (2_000, b.as_slice())]);
    }
}

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
            } else if autopilot != "Invalid" {
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
    let mut records = Vec::new();
    let mut at = 0;
    while at + 8 < bytes.len() {
        let mut stamp = [0u8; 8];
        stamp.copy_from_slice(&bytes[at..at + 8]);
        let start = at + 8;
        let Some(len) = crate::framing::frame_len(&bytes[start..]) else {
            break;
        };
        if start + len > bytes.len() {
            break;
        }
        records.push((u64::from_be_bytes(stamp), &bytes[start..start + len]));
        at = start + len;
    }
    records
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim;

    const T0: u64 = 1_760_000_000_000_000;

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

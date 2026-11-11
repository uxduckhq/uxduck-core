use std::collections::{BTreeMap, BTreeSet};

use mavio::Receiver;
use mavio::dialects::ardupilotmega::Ardupilotmega;
use mavio::io::StdIoReader;
use mavio::protocol::Versionless;

use crate::decode::{Message, MissionItemInt, decode};
use crate::framing::{header, payload};

const DEADLINE_US: u64 = 30_000_000;
const REBOOT_MS: u32 = 5_000;
pub const MESSAGE_IDS: [u32; 11] = [38, 39, 40, 42, 43, 44, 45, 46, 47, 51, 73];

#[derive(Clone, Debug)]
pub struct Reading {
    pub at_us: u64,
    pub items: Option<Vec<MissionItemInt>>,
    pub detail: &'static str,
}

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    readings: Vec<Reading>,
}

impl Timeline {
    #[must_use]
    pub fn at(&self, t_us: u64) -> Option<(usize, &Reading)> {
        self.readings
            .partition_point(|r| r.at_us <= t_us)
            .checked_sub(1)
            .map(|index| (index, &self.readings[index]))
    }

    fn unknown(&mut self, at_us: u64, detail: &'static str) {
        if self
            .readings
            .last()
            .is_some_and(|r| r.items.is_none() && r.detail == detail)
        {
            return;
        }
        self.readings.push(Reading {
            at_us,
            items: None,
            detail,
        });
    }
}

struct Assembly {
    peer: (u8, u8),
    deadline: u64,
    count: Option<u16>,
    requested: BTreeSet<u16>,
    items: BTreeMap<u16, MissionItemInt>,
    reported_total: Option<u16>,
    valid: bool,
}

struct Vehicle {
    component: u8,
    ardupilot: bool,
    ambiguous: bool,
    boot_ms: Option<u32>,
    known_count: Option<u16>,
    observed_id: Option<u32>,
    active: Option<Assembly>,
    timeline: Timeline,
}

impl Vehicle {
    fn unknown(&mut self, at: u64, detail: &'static str) {
        self.known_count = None;
        self.timeline.unknown(at, detail);
    }

    fn reject(&mut self, at: u64, detail: &'static str) {
        if let Some(active) = &mut self.active {
            active.valid = false;
            active.items.clear();
        }
        self.unknown(at, detail);
    }

    fn invalidate(&mut self, at: u64, detail: &'static str) {
        self.active = None;
        self.unknown(at, detail);
    }

    fn expire(&mut self, at: u64) {
        if let Some(deadline) = self.active.as_ref().map(|a| a.deadline)
            && at >= deadline
        {
            self.invalidate(
                deadline,
                "Recorded mission readback did not complete before its deadline.",
            );
        }
    }

    fn open(&mut self, at: u64, peer: (u8, u8)) {
        if self.active.is_some() {
            self.reject(at, "Overlapping recorded mission readbacks are ambiguous.");
            return;
        }
        self.unknown(at, "Recorded mission readback is incomplete at this time.");
        self.active = Some(Assembly {
            peer,
            deadline: at.saturating_add(DEADLINE_US),
            count: None,
            requested: BTreeSet::new(),
            items: BTreeMap::new(),
            reported_total: None,
            valid: true,
        });
    }

    fn count(&mut self, at: u64, peer: (u8, u8), count: u16) {
        let Some(active) = &mut self.active else {
            self.unknown(
                at,
                "The opening of this recorded mission readback is missing.",
            );
            return;
        };
        if !active.valid {
            return;
        }
        if active.peer != peer
            || active.reported_total.is_some_and(|total| {
                total
                    != if self.ardupilot {
                        count.saturating_sub(1)
                    } else {
                        count
                    }
            })
            || active
                .count
                .is_some_and(|old| old != count || !active.items.is_empty())
        {
            self.reject(
                at,
                "Recorded mission readback count or endpoint changed before completion.",
            );
            return;
        }
        active.count = Some(count);
    }

    fn request(&mut self, at: u64, peer: (u8, u8), seq: u16) {
        let Some(active) = &mut self.active else {
            return;
        };
        if !active.valid {
            return;
        }
        if active.peer != peer || active.count.is_none_or(|n| seq >= n) {
            self.reject(
                at,
                "Recorded mission item requests do not match the readback.",
            );
            return;
        }
        active.requested.insert(seq);
    }

    fn item(&mut self, at: u64, peer: (u8, u8), item: MissionItemInt) {
        let Some(active) = &mut self.active else {
            return;
        };
        if !active.valid {
            return;
        }
        if active.peer != peer
            || active.count.is_none_or(|n| item.seq >= n)
            || !active.requested.contains(&item.seq)
        {
            self.reject(
                at,
                "Recorded mission items do not match the requested readback.",
            );
            return;
        }
        if active
            .items
            .get(&item.seq)
            .is_some_and(|old| !same_item(old, &item))
        {
            self.reject(
                at,
                "Conflicting recorded mission items have the same sequence.",
            );
            return;
        }
        active.items.insert(item.seq, item);
    }

    fn finish(&mut self, at: u64, peer: (u8, u8), result: u8) {
        if self.active.as_ref().is_none_or(|a| a.peer != peer) {
            return;
        }
        let Some(active) = self.active.take() else {
            return;
        };
        if result != 0
            || !active.valid
            || active
                .count
                .is_none_or(|n| active.items.len() != usize::from(n))
        {
            self.unknown(
                at,
                "No complete accepted vehicle mission readback was recorded at this time.",
            );
            return;
        }
        self.known_count = active.count;
        self.timeline.readings.push(Reading {
            at_us: at,
            items: Some(active.items.into_values().collect()),
            detail: "Complete recorded vehicle mission readback.",
        });
    }

    fn current(&mut self, at: u64, total: u16, mission_id: u32) {
        let changed_id = mission_id != 0
            && self
                .observed_id
                .replace(mission_id)
                .is_some_and(|old| old != mission_id);
        let count = self
            .active
            .as_ref()
            .and_then(|a| a.count)
            .or(self.known_count);
        let mismatch = total != 0
            && (count.is_some_and(|count| {
                total
                    != if self.ardupilot {
                        count.saturating_sub(1)
                    } else {
                        count
                    }
            }) || self
                .active
                .as_ref()
                .and_then(|a| a.reported_total)
                .is_some_and(|old| old != total));
        if changed_id || mismatch {
            self.invalidate(
                at,
                "The recorded mission changed without a complete new readback.",
            );
        } else if total != 0
            && let Some(active) = &mut self.active
        {
            active.reported_total = Some(total);
        }
    }
}

fn same_item(a: &MissionItemInt, b: &MissionItemInt) -> bool {
    a.seq == b.seq
        && a.command == b.command
        && a.frame == b.frame
        && a.current == b.current
        && a.autocontinue == b.autocontinue
        && a.params.map(f32::to_bits) == b.params.map(f32::to_bits)
        && a.x == b.x
        && a.y == b.y
        && a.z.to_bits() == b.z.to_bits()
        && a.mission_type == b.mission_type
}

fn route(id: u32, bytes: &[u8]) -> Option<((u8, u8), u8)> {
    let mut p = [0u8; 38];
    let n = bytes.len().min(p.len());
    p[..n].copy_from_slice(&bytes[..n]);
    Some(match id {
        38 => ((p[4], p[5]), p[6]),
        39 | 73 => ((p[32], p[33]), p[37]),
        40 | 44 | 51 => ((p[2], p[3]), p[4]),
        43 | 45 => ((p[0], p[1]), p[2]),
        47 => ((p[0], p[1]), p[3]),
        _ => return None,
    })
}

fn valid_mission(frame: &[u8]) -> bool {
    let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(frame));
    receiver
        .recv()
        .is_ok_and(|frame| frame.validate_checksum::<Ardupilotmega>().is_ok())
}

#[must_use]
pub fn saved_prefix<'a>(records: &[(u64, &'a [u8])], system: u8) -> Vec<(u64, &'a [u8])> {
    let mut stations = BTreeSet::new();
    let mut out = Vec::new();
    for &(at, frame) in records {
        let Some(h) = header(frame) else { continue };
        let sender = (h.system_id, h.component_id);
        let decoded = decode(frame);
        if decoded.as_ref().is_some_and(
            |d| matches!(&d.message, Message::Heartbeat { mav_type, .. } if mav_type == "Gcs"),
        ) {
            stations.insert(sender);
            out.push((at, frame));
            continue;
        }
        let mission = MESSAGE_IDS.contains(&h.message_id);
        let invalid = mission && !valid_mission(frame);
        let route = payload(frame).and_then(|p| route(h.message_id, p));
        let mission_kind =
            route.is_none_or(|(_, kind)| kind == 0 || (h.message_id == 45 && kind == 255));
        if h.system_id == system {
            if !mission || invalid || mission_kind {
                out.push((at, frame));
            }
            continue;
        }
        if decoded.as_ref().is_some_and(|d| match &d.message {
            Message::ParamSet { target_system, .. } | Message::Ftp { target_system, .. } => {
                *target_system == system || *target_system == 0
            }
            _ => false,
        }) {
            out.push((at, frame));
            continue;
        }
        if !mission || !stations.contains(&sender) {
            continue;
        }
        if invalid {
            out.retain(|(_, previous)| {
                header(previous).is_none_or(|h| !MESSAGE_IDS.contains(&h.message_id))
            });
        } else if mission_kind
            && route.is_some_and(|(target, _)| target.0 == system || target.0 == 0)
        {
            out.push((at, frame));
        }
    }
    out
}

#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one passive mission-protocol state machine"
)]
pub fn timelines(records: &[(u64, &[u8])]) -> BTreeMap<(u8, u8), Timeline> {
    let mut vehicles: BTreeMap<u8, Vehicle> = BTreeMap::new();
    let mut stations = BTreeSet::new();
    for &(at, frame) in records {
        for vehicle in vehicles.values_mut() {
            vehicle.expire(at);
        }
        let Some(h) = header(frame) else { continue };
        let sender = (h.system_id, h.component_id);
        let invalid_mission = MESSAGE_IDS.contains(&h.message_id) && !valid_mission(frame);
        if invalid_mission {
            if stations.contains(&sender) {
                for vehicle in vehicles.values_mut() {
                    vehicle.reject(at, "Recorded mission traffic has an invalid checksum.");
                }
            } else if let Some(vehicle) = vehicles
                .get_mut(&h.system_id)
                .filter(|v| v.component == h.component_id)
            {
                vehicle.reject(at, "Recorded mission traffic has an invalid checksum.");
            }
            continue;
        }
        let decoded = decode(frame);
        if let Some(d) = &decoded
            && let Message::Heartbeat {
                mav_type,
                autopilot,
                ..
            } = &d.message
        {
            if mav_type == "Gcs" {
                stations.insert((h.system_id, h.component_id));
                if let Some(v) = vehicles
                    .get_mut(&h.system_id)
                    .filter(|v| v.component == h.component_id)
                {
                    v.ambiguous = true;
                    v.invalidate(at, "The recorded autopilot identity is ambiguous.");
                }
            } else if autopilot != "Invalid" && h.component_id != crate::GCS_COMPONENT_ID {
                let v = vehicles.entry(h.system_id).or_insert_with(|| Vehicle {
                    component: h.component_id,
                    ardupilot: autopilot.to_lowercase().contains("ardupilot"),
                    ambiguous: false,
                    boot_ms: None,
                    known_count: None,
                    observed_id: None,
                    active: None,
                    timeline: Timeline::default(),
                });
                if v.component != h.component_id
                    || stations.contains(&(h.system_id, h.component_id))
                {
                    v.ambiguous = true;
                    v.invalidate(at, "The recorded autopilot identity is ambiguous.");
                }
            }
            continue;
        }
        if let Some(v) = vehicles
            .get_mut(&h.system_id)
            .filter(|v| v.component == h.component_id && !v.ambiguous)
            && let Some(d) = &decoded
        {
            match &d.message {
                Message::GlobalPositionInt { time_boot_ms, .. }
                | Message::Attitude { time_boot_ms, .. } => {
                    if v.boot_ms
                        .replace(*time_boot_ms)
                        .is_some_and(|old| time_boot_ms.saturating_add(REBOOT_MS) < old)
                    {
                        v.observed_id = None;
                        v.invalidate(at, "The vehicle restarted. No complete mission readback is recorded for this boot.");
                    }
                }
                Message::MissionCurrent {
                    total, mission_id, ..
                } => v.current(at, *total, *mission_id),
                _ => {}
            }
        }
        let Some((target, kind)) = payload(frame).and_then(|p| route(h.message_id, p)) else {
            continue;
        };
        if kind != 0 && !(h.message_id == 45 && kind == 255) {
            continue;
        }
        let from_station = stations.contains(&sender);
        if from_station && matches!(h.message_id, 38 | 39 | 44 | 45 | 73) {
            for (&system, v) in &mut vehicles {
                if (target.0 == 0 || target.0 == system)
                    && (target.1 == 0 || target.1 == v.component)
                {
                    v.invalidate(at, "A recorded mission write needs a complete vehicle readback before it can be shown.");
                }
            }
            continue;
        }
        let system = if from_station { target.0 } else { sender.0 };
        let Some(v) = vehicles.get_mut(&system) else {
            continue;
        };
        if v.ambiguous
            || (from_station && target.1 != v.component)
            || (!from_station && sender.1 != v.component)
        {
            continue;
        }
        if !from_station && !stations.contains(&target) {
            continue;
        }
        if from_station {
            match h.message_id {
                43 => v.open(at, sender),
                40 | 51 => {
                    if let Some(d) = decoded
                        && let Message::MissionRequest { seq, .. } = d.message
                    {
                        v.request(at, sender, seq);
                    }
                }
                47 => {
                    if let Some(d) = decoded
                        && let Message::MissionAck { result, .. } = d.message
                    {
                        v.finish(at, sender, result);
                    }
                }
                _ => {}
            }
        } else {
            match h.message_id {
                44 => {
                    if let Some(d) = decoded
                        && let Message::MissionCount { count, .. } = d.message
                    {
                        v.count(at, target, count);
                    }
                }
                73 => {
                    if let Some(d) = decoded
                        && let Message::MissionItem(item) = d.message
                    {
                        v.item(at, target, item);
                    }
                }
                39 => v.reject(
                    at,
                    "This recording uses mission items whose format cannot be reconstructed.",
                ),
                47 => {
                    if v.active.is_some()
                        && let Some(d) = decoded
                        && let Message::MissionAck { result, .. } = d.message
                        && result != 0
                    {
                        v.reject(
                            at,
                            "The recorded vehicle refused or interrupted a mission readback.",
                        );
                    }
                }
                _ => {}
            }
        }
    }
    vehicles
        .into_iter()
        .map(|(system, mut v)| {
            v.expire(u64::MAX);
            ((system, v.component), v.timeline)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::cast_possible_truncation,
        reason = "synthetic protocol fixtures"
    )]

    use super::*;
    use crate::decode::check;
    use crate::framing::Check;
    use crate::{custom, sim};

    const AP: sim::Route = sim::Route {
        from: (1, 1),
        to: (255, 190),
    };
    const GCS: sim::Route = sim::Route {
        from: (255, 190),
        to: (1, 1),
    };

    fn item(seq: u16) -> MissionItemInt {
        MissionItemInt {
            seq,
            frame: if seq == 0 { 0 } else { 255 },
            command: if seq == 0 { 16 } else { 32001 },
            current: false,
            autocontinue: true,
            params: [1.25, -2.5, 7.0, f32::from_bits(0x7fc0_0042)],
            x: 10_000_000 + i32::from(seq),
            y: -20_000_000,
            z: 47.25,
            mission_type: 0,
        }
    }

    fn identities() -> Vec<(u64, Vec<u8>)> {
        vec![
            (0, sim::heartbeat_frame(1, 1, 0)),
            (1, sim::gcs_heartbeat_frame(255, 190, 0)),
        ]
    }

    fn readback(at: u64, count: u16) -> Vec<(u64, Vec<u8>)> {
        let mut out = vec![
            (at, sim::mission_request_list_frame(255, 1, 1, 0, 0)),
            (at + 1, sim::mission_count_frame(AP, 0, count, 0)),
        ];
        for seq in 0..count {
            out.push((
                at + 2 + u64::from(seq) * 2,
                sim::mission_request_frame(GCS, 0, seq, 0),
            ));
            out.push((
                at + 3 + u64::from(seq) * 2,
                sim::mission_item_frame(AP, 0, &item(seq)),
            ));
        }
        out.push((
            at + 2 + u64::from(count) * 2,
            sim::mission_ack_frame(GCS, 0, 0, 0),
        ));
        out
    }

    fn parsed(records: &[(u64, Vec<u8>)]) -> Timeline {
        let refs: Vec<_> = records.iter().map(|(t, b)| (*t, b.as_slice())).collect();
        timelines(&refs).remove(&(1, 1)).unwrap()
    }

    fn current(total: u16, id: u32) -> Vec<u8> {
        let mut p = vec![0; 18];
        p[2..4].copy_from_slice(&total.to_le_bytes());
        p[6..10].copy_from_slice(&id.to_le_bytes());
        custom::frame_from((1, 1, 0), 42, &p, 28)
    }

    #[test]
    fn publishes_only_complete_vehicle_readback_at_accepted_ack_preserving_bits() {
        let mut records = identities();
        records.push((2, sim::mission_count_frame(GCS, 0, 2, 0)));
        let mut intent = item(1);
        intent.z = 999.0;
        records.push((3, sim::mission_item_frame(GCS, 0, &intent)));
        records.push((4, sim::mission_ack_frame(AP, 0, 0, 0)));
        records.extend(readback(10, 2));
        let timeline = parsed(&records);
        assert!(timeline.at(15).unwrap().1.items.is_none());
        let reading = timeline.at(16).unwrap().1;
        let items = reading.items.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(reading.at_us, 16);
        assert!(same_item(&items[1], &item(1)));
        assert_eq!(items[1].frame, 255);
        assert_eq!(items[1].command, 32001);
        assert_eq!(items[1].params[3].to_bits(), 0x7fc0_0042);
    }

    #[test]
    fn duplicate_nan_item_is_not_a_conflict_or_an_extra_item() {
        let mut records = identities();
        let mut read = readback(10, 2);
        read.insert(
            read.len() - 1,
            (15, sim::mission_item_frame(AP, 0, &item(1))),
        );
        records.extend(read);
        assert_eq!(
            parsed(&records)
                .at(16)
                .unwrap()
                .1
                .items
                .as_ref()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn incomplete_unopened_rejected_conflicting_and_corrupt_transfers_stay_unknown() {
        for fault in 0..6 {
            let mut records = identities();
            let mut read = readback(10, 2);
            match fault {
                0 => {
                    read.remove(0);
                }
                1 => {
                    read.remove(5);
                }
                2 => {
                    read.last_mut().unwrap().1 = sim::mission_ack_frame(GCS, 0, 1, 0);
                }
                3 => {
                    let mut other = item(1);
                    other.z = 900.0;
                    read.insert(read.len() - 1, (15, sim::mission_item_frame(AP, 0, &other)));
                }
                4 => {
                    let frame = &mut read[5].1;
                    let last = frame.len() - 1;
                    frame[last] ^= 1;
                }
                _ => {
                    read[4].1 = sim::mission_request_frame(GCS, 0, 0, 0);
                }
            }
            records.extend(read);
            assert!(
                parsed(&records).at(100).unwrap().1.items.is_none(),
                "fault {fault}"
            );
        }
    }

    #[test]
    fn missing_ack_expires_at_its_deadline_without_another_packet() {
        let mut records = identities();
        let mut read = readback(10, 2);
        read.pop();
        records.extend(read);
        let timeline = parsed(&records);
        assert!(
            timeline
                .at(10 + DEADLINE_US)
                .unwrap()
                .1
                .detail
                .contains("deadline")
        );
        assert_eq!(timeline.at(u64::MAX).unwrap().1.at_us, 10 + DEADLINE_US);
    }

    #[test]
    fn overlapping_same_endpoint_and_same_count_cannot_reuse_items_or_late_ack() {
        let mut records = identities();
        let mut read = readback(10, 2);
        read.insert(4, (13, sim::mission_request_list_frame(255, 1, 1, 1, 0)));
        read.insert(5, (13, sim::mission_count_frame(AP, 1, 2, 0)));
        records.extend(read);
        assert!(parsed(&records).at(100).unwrap().1.items.is_none());
    }

    #[test]
    fn foreign_endpoint_component_and_type_cannot_complete_a_mission() {
        for fault in 0..3 {
            let mut records = identities();
            records.push((2, sim::gcs_heartbeat_frame(254, 190, 0)));
            let mut read = readback(10, 2);
            let mut route = AP;
            let mut next = item(1);
            match fault {
                0 => route.to = (254, 190),
                1 => route.from.1 = 2,
                _ => next.mission_type = 1,
            }
            read[5].1 = sim::mission_item_frame(route, 0, &next);
            records.extend(read);
            assert!(
                parsed(&records).at(100).unwrap().1.items.is_none(),
                "fault {fault}"
            );
        }
    }

    #[test]
    fn home_count_and_missing_current_extensions_do_not_erase_the_mission() {
        let mut records = identities();
        records.extend(readback(10, 9));
        records.extend([
            (40, current(8, 5)),
            (41, current(0, 0)),
            (42, current(8, 5)),
            (43, current(8, 6)),
        ]);
        let timeline = parsed(&records);
        assert_eq!(timeline.at(42).unwrap().1.items.as_ref().unwrap().len(), 9);
        assert!(timeline.at(43).unwrap().1.items.is_none());
    }

    #[test]
    fn empty_readback_is_distinct_from_clear_intent_and_prior_or_future_versions() {
        let mut records = identities();
        records.extend(readback(10, 2));
        records.push((20, custom::frame_from((255, 190, 0), 45, &[1, 1, 255], 232)));
        records.extend(readback(30, 0));
        records.extend(readback(40, 1));
        let timeline = parsed(&records);
        assert!(timeline.at(9).is_none());
        assert_eq!(timeline.at(16).unwrap().1.items.as_ref().unwrap().len(), 2);
        assert!(timeline.at(20).unwrap().1.items.is_none());
        assert!(
            timeline
                .at(32)
                .unwrap()
                .1
                .items
                .as_ref()
                .unwrap()
                .is_empty()
        );
        assert_eq!(timeline.at(44).unwrap().1.items.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn reboot_unknown_identity_and_unsupported_float_items_never_retain_a_path() {
        for fault in 0..3 {
            let mut records = identities();
            records.extend(readback(10, 2));
            match fault {
                0 => {
                    records.push((
                        20,
                        sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 20_000),
                    ));
                    records.push((21, sim::global_position_at_frame((1, 1), (0, 0), (0, 0), 0)));
                }
                1 => records.push((20, sim::heartbeat_frame(1, 2, 0))),
                _ => {
                    let int = sim::mission_item_frame(AP, 0, &item(1));
                    records.push((
                        20,
                        custom::frame_from((1, 1, 0), 39, payload(&int).unwrap(), 254),
                    ));
                }
            }
            records.sort_by_key(|(t, _)| *t);
            assert!(
                parsed(&records).at(100).unwrap().1.items.is_none(),
                "fault {fault}"
            );
        }
    }

    #[test]
    fn current_contradictions_before_count_or_ack_reject_an_active_generation() {
        for fault in 0..3 {
            let mut records = identities();
            records.extend(readback(10, 2));
            records.push((17, current(1, 5)));
            let mut next = readback(30, 2);
            match fault {
                0 => next.insert(1, (30, current(2, 5))),
                1 => next.insert(next.len() - 1, (35, current(2, 5))),
                _ => next.insert(next.len() - 1, (35, current(1, 6))),
            }
            records.extend(next);
            records.extend(readback(50, 2));
            let timeline = parsed(&records);
            assert!(timeline.at(17).unwrap().1.items.is_some());
            assert!(timeline.at(37).unwrap().1.items.is_none(), "fault {fault}");
            assert!(timeline.at(56).unwrap().1.items.is_some());
        }
    }

    #[test]
    fn corrupt_targets_and_types_cannot_preserve_a_path_or_complete_an_assembly() {
        for during in [false, true] {
            for fault in 0..4 {
                let mut records = identities();
                records.extend(readback(10, 2));
                let mut next = readback(30, 2);
                let mut corrupt = match fault {
                    0 => sim::mission_count_frame(GCS, 0, 2, 0),
                    1 => sim::mission_count_frame(GCS, 0, 2, 1),
                    _ => {
                        let mut item = item(1);
                        if fault == 3 {
                            item.mission_type = 1;
                        }
                        sim::mission_item_frame(AP, 0, &item)
                    }
                };
                let field = match fault {
                    0 => 2,
                    1 => 4,
                    2 => 33,
                    _ => 37,
                };
                corrupt[10 + field] ^= 2;
                assert_eq!(check(&corrupt), Check::Invalid);
                if during {
                    next.insert(next.len() - 1, (35, corrupt));
                    records.extend(next);
                } else {
                    records.push((20, corrupt));
                }
                let timeline = parsed(&records);
                assert!(timeline.at(16).unwrap().1.items.is_some());
                assert!(
                    timeline.at(100).unwrap().1.items.is_none(),
                    "during {during}, fault {fault}"
                );
            }
        }
    }

    #[test]
    fn orphan_items_and_broadcast_writes_invalidate_but_fence_and_rally_do_not() {
        for fault in 0..5 {
            let mut records = identities();
            records.extend(readback(10, 2));
            let broadcast = sim::Route { to: (1, 0), ..GCS };
            let write = match fault {
                0 => sim::mission_item_frame(GCS, 0, &item(1)),
                1 => {
                    let int = sim::mission_item_frame(GCS, 0, &item(1));
                    custom::frame_from((255, 190, 0), 39, payload(&int).unwrap(), 254)
                }
                2 => sim::mission_count_frame(broadcast, 0, 2, 0),
                3 => custom::frame_from((255, 190, 0), 45, &[1, 0, 0], 232),
                _ => custom::frame_from((255, 190, 0), 45, &[0, 0, 255], 232),
            };
            records.push((20, write));
            assert!(
                parsed(&records).at(20).unwrap().1.items.is_none(),
                "fault {fault}"
            );
        }
        let mut records = identities();
        records.extend(readback(10, 2));
        for kind in [1, 2] {
            let mut foreign = item(1);
            foreign.mission_type = kind;
            records.extend([
                (
                    20 + u64::from(kind),
                    sim::mission_count_frame(GCS, 0, 2, kind),
                ),
                (
                    20 + u64::from(kind),
                    sim::mission_item_frame(GCS, 0, &foreign),
                ),
                (
                    20 + u64::from(kind),
                    custom::frame_from((255, 190, 0), 45, &[1, 0, kind], 232),
                ),
            ]);
        }
        assert_eq!(
            parsed(&records)
                .at(100)
                .unwrap()
                .1
                .items
                .as_ref()
                .unwrap()
                .len(),
            2
        );
    }
}

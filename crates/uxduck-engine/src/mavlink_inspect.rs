//! The MAVLink inspector's table: for each vehicle, the latest message of
//! each type it sent and UXDuck sent to it, with a count and a rate, which
//! `mavlink/inspect` reads. Each composition root owns one and records every
//! frame with [`Inspector::record`]: a copy of its payload and a few
//! counters, nothing decoded. Decoding waits for a read, which happens only
//! while someone has the inspector open, and can happen apart from the
//! table ([`Seen::decode`]), so a table behind a lock is held only to copy.
//!
//! It also keeps the last [`HISTORY`] of each type and direction, which
//! `mavlink/history` pages through newest first ([`Inspector::history`]):
//! everything in the browser, and what UXDuck sent in the installed daemon,
//! which reads what vehicles sent from its tlogs instead
//! ([`Inspector::keep_history`]).

use std::collections::HashMap;
use std::collections::hash_map::Entry as Slot;

use serde_json::{Map, Value};
use uxduck_contracts::dto::inspect::{
    InspectDirection, InspectedMessage, MavlinkHistoryResult, MavlinkInspectResult, PastMessage,
};
use uxduck_mavlink::framing::Header;

/// Message types kept per vehicle. A vehicle that sends more than this is
/// not sending MAVLink as we know it: its entries start again.
const MAX_TYPES: usize = 512;
/// Vehicles kept; the one heard from least recently goes first.
const MAX_VEHICLES: usize = 64;
/// A rate is frames counted over at least this long.
const WINDOW_US: u64 = 2_000_000;
/// A message stopped once nothing came for this long, or three of its
/// periods if that is longer.
const STOPPED_US: u64 = 3_000_000;
/// Earlier messages kept of each type, each way.
pub const HISTORY: usize = 200;
/// Bytes of earlier messages kept per vehicle. Past this its histories stop
/// growing and each keeps what it has, overwriting its oldest: a vehicle
/// sending dozens of types fits well inside it (about 60 types at 200 each
/// of 60 bytes is 1.5 MB), one sending hundreds of long ones does not.
const HISTORY_BYTES: usize = 4 << 20;
/// A page of `mavlink/history` when the call does not say.
const PAGE: usize = 50;
/// The most one page may hold.
const MAX_PAGE: usize = 500;

/// A page's size from `mavlink/history`'s `limit`: 50 when omitted, between
/// 1 and 500.
#[must_use]
pub fn page_size(limit: Option<u32>) -> usize {
    limit.map_or(PAGE, |n| {
        usize::try_from(n).unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE)
    })
}

/// The latest of one message type, one way.
#[derive(Clone, Debug)]
struct Latest {
    system_id: u8,
    component_id: u8,
    payload: Vec<u8>,
    count: u64,
    last_us: u64,
    /// Frames since `window_us`, not counting the one that started it.
    window_frames: u32,
    window_us: u64,
    /// The last whole window's rate; None until one has passed.
    rate: Option<f64>,
}

impl Latest {
    fn new(header: Header, payload: &[u8], at_us: u64) -> Self {
        Self {
            system_id: header.system_id,
            component_id: header.component_id,
            payload: payload.to_vec(),
            count: 1,
            last_us: at_us,
            window_frames: 0,
            window_us: at_us,
            rate: None,
        }
    }

    fn update(&mut self, header: Header, payload: &[u8], at_us: u64) {
        self.system_id = header.system_id;
        self.component_id = header.component_id;
        self.payload.clear();
        self.payload.extend_from_slice(payload);
        self.count += 1;
        self.last_us = at_us;
        self.window_frames = self.window_frames.saturating_add(1);
        let elapsed = at_us.saturating_sub(self.window_us);
        if elapsed >= WINDOW_US {
            self.rate = Some(f64::from(self.window_frames) * 1e6 / micros(elapsed));
            self.window_frames = 0;
            self.window_us = at_us;
        }
    }

    /// Messages a second at `now_us`: the last whole window's rate, or
    /// before one has passed, the frames so far; 0 once they stopped.
    fn hz(&self, now_us: u64) -> f64 {
        let rate = self.rate.unwrap_or_else(|| {
            let elapsed = self.last_us.saturating_sub(self.window_us);
            if self.window_frames == 0 || elapsed == 0 {
                0.0
            } else {
                f64::from(self.window_frames) * 1e6 / micros(elapsed)
            }
        });
        let silent = now_us.saturating_sub(self.last_us);
        let periods = if rate > 0.0 { 3e6 / rate } else { 0.0 };
        if micros(silent) > periods.max(micros(STOPPED_US)) {
            0.0
        } else {
            rate
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "microsecond spans fit an f64 exactly for 285 years"
)]
const fn micros(us: u64) -> f64 {
    us as f64
}

type Messages = HashMap<(InspectDirection, u32), Latest>;

/// One earlier message.
#[derive(Debug)]
struct Past {
    /// The order it was recorded in, across the whole table: what a cursor
    /// points past.
    seq: u64,
    at_us: u64,
    system_id: u8,
    component_id: u8,
    payload: Vec<u8>,
}

/// The last [`HISTORY`] messages of one type, one way. Once full, each new
/// one overwrites the oldest in place, reusing its buffer.
#[derive(Debug, Default)]
struct Ring {
    entries: Vec<Past>,
    /// The oldest, overwritten next once the ring stops growing.
    next: usize,
}

impl Ring {
    /// Keeps one; the bytes it added, when the ring grew.
    fn push(&mut self, seq: u64, header: Header, payload: &[u8], at_us: u64, grow: bool) -> usize {
        if self.entries.is_empty() || (grow && self.entries.len() < HISTORY) {
            self.entries.push(Past {
                seq,
                at_us,
                system_id: header.system_id,
                component_id: header.component_id,
                payload: payload.to_vec(),
            });
            return std::mem::size_of::<Past>() + payload.len();
        }
        let past = &mut self.entries[self.next];
        past.seq = seq;
        past.at_us = at_us;
        past.system_id = header.system_id;
        past.component_id = header.component_id;
        past.payload.clear();
        past.payload.extend_from_slice(payload);
        self.next = (self.next + 1) % self.entries.len();
        0
    }
}

/// What the table keeps of one vehicle.
#[derive(Debug, Default)]
struct Vehicle {
    latest: Messages,
    history: HashMap<(InspectDirection, u32), Ring>,
    /// What `history` holds, roughly: its entries and their payloads.
    history_bytes: usize,
}

/// The latest messages of every vehicle heard, and the last few of each.
#[derive(Debug)]
pub struct Inspector {
    vehicles: HashMap<String, Vehicle>,
    /// Frames kept so far, numbering each one kept.
    seq: u64,
    /// Whether earlier messages are kept, received and sent.
    keep: (bool, bool),
}

impl Default for Inspector {
    fn default() -> Self {
        Self {
            vehicles: HashMap::new(),
            seq: 0,
            keep: (true, true),
        }
    }
}

/// A `before` that is not one of [`Inspector::history`]'s cursors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadCursor;

/// A cursor of [`Inspector::history`]: `m` and the number of the oldest
/// message the last page held. It stays valid as long as the table lives;
/// one past messages already overwritten pages what is left.
fn cursor(seq: u64) -> String {
    format!("m{seq}")
}

fn parse_cursor(text: &str) -> Result<u64, BadCursor> {
    text.strip_prefix('m')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .ok_or(BadCursor)
}

impl Inspector {
    /// Which ways earlier messages are kept; both by default. The installed
    /// daemon reads what vehicles sent from its tlogs, so keeps only what it
    /// sent.
    #[must_use]
    pub const fn keep_history(mut self, received: bool, sent: bool) -> Self {
        self.keep = (received, sent);
        self
    }

    /// One frame from a vehicle, or to it.
    pub fn record(
        &mut self,
        vehicle_id: &str,
        direction: InspectDirection,
        header: Header,
        payload: &[u8],
        at_us: u64,
    ) {
        if !self.vehicles.contains_key(vehicle_id) {
            if self.vehicles.len() >= MAX_VEHICLES {
                self.drop_quietest();
            }
            self.vehicles
                .insert(vehicle_id.to_owned(), Vehicle::default());
        }
        let Some(vehicle) = self.vehicles.get_mut(vehicle_id) else {
            return;
        };
        let key = (direction, header.message_id);
        if vehicle.latest.len() >= MAX_TYPES && !vehicle.latest.contains_key(&key) {
            *vehicle = Vehicle::default();
        }
        match vehicle.latest.entry(key) {
            Slot::Occupied(mut latest) => latest.get_mut().update(header, payload, at_us),
            Slot::Vacant(slot) => {
                slot.insert(Latest::new(header, payload, at_us));
            }
        }
        let keep = match direction {
            InspectDirection::Received => self.keep.0,
            InspectDirection::Sent => self.keep.1,
        };
        if keep {
            self.seq += 1;
            let grow = vehicle.history_bytes < HISTORY_BYTES;
            vehicle.history_bytes += vehicle
                .history
                .entry(key)
                .or_default()
                .push(self.seq, header, payload, at_us, grow);
        }
    }

    fn drop_quietest(&mut self) {
        let quietest = self
            .vehicles
            .iter()
            .min_by_key(|(_, vehicle)| vehicle.latest.values().map(|m| m.last_us).max())
            .map(|(id, _)| id.clone());
        if let Some(id) = quietest {
            self.vehicles.remove(&id);
        }
    }

    /// Latest received heartbeat under these ids. Sent ground-station
    /// heartbeats and other received messages do not establish vehicle liveness.
    #[must_use]
    pub fn heartbeat_us(&self, vehicle_ids: &[String]) -> Option<u64> {
        vehicle_ids
            .iter()
            .filter_map(|id| self.vehicles.get(id))
            .filter_map(|vehicle| vehicle.latest.get(&(InspectDirection::Received, 0)))
            .map(|heartbeat| heartbeat.last_us)
            .max()
    }

    /// Each type's latest under any of the ids (a vehicle's id and its
    /// earlier ones), the newest winning and the counts added up, with its
    /// rate at `now_us`; copied, to decode with [`Seen::decode`].
    #[must_use]
    pub fn read(&self, vehicle_ids: &[String], now_us: u64) -> Seen {
        let mut merged: HashMap<(InspectDirection, u32), Latest> = HashMap::new();
        for vehicle in vehicle_ids.iter().filter_map(|id| self.vehicles.get(id)) {
            for (key, latest) in &vehicle.latest {
                match merged.entry(*key) {
                    Slot::Vacant(slot) => {
                        slot.insert(latest.clone());
                    }
                    Slot::Occupied(mut slot) => {
                        let count = slot.get().count + latest.count;
                        if latest.last_us > slot.get().last_us {
                            *slot.get_mut() = latest.clone();
                        }
                        slot.get_mut().count = count;
                    }
                }
            }
        }
        Seen {
            now_us,
            messages: merged
                .into_iter()
                .map(|((direction, id), latest)| {
                    let hz = latest.hz(now_us);
                    (direction, id, latest, hz)
                })
                .collect(),
        }
    }
}

impl Inspector {
    /// Earlier messages of one type, one way, under any of the ids, newest
    /// first: up to `limit` of them older than `before` (a previous page's
    /// `older`), copied, to decode with [`Earlier::decode`].
    ///
    /// # Errors
    /// When `before` is not a cursor this table wrote.
    pub fn history(
        &self,
        vehicle_ids: &[String],
        direction: InspectDirection,
        id: u32,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Earlier, BadCursor> {
        let before = before.map(parse_cursor).transpose()?.unwrap_or(u64::MAX);
        let mut found: Vec<&Past> = vehicle_ids
            .iter()
            .filter_map(|vehicle_id| self.vehicles.get(vehicle_id))
            .filter_map(|vehicle| vehicle.history.get(&(direction, id)))
            .flat_map(|ring| &ring.entries)
            .filter(|past| past.seq < before)
            .collect();
        found.sort_unstable_by_key(|past| std::cmp::Reverse(past.seq));
        let older = (found.len() > limit).then(|| cursor(found[limit - 1].seq));
        found.truncate(limit);
        Ok(Earlier {
            id,
            messages: found
                .into_iter()
                .map(|past| {
                    (
                        past.at_us,
                        past.system_id,
                        past.component_id,
                        past.payload.clone(),
                    )
                })
                .collect(),
            older,
        })
    }
}

/// What [`Inspector::history`] copied out, not yet decoded: each message's
/// time, system and component ids and payload, newest first.
#[derive(Debug)]
pub struct Earlier {
    id: u32,
    messages: Vec<(u64, u8, u8, Vec<u8>)>,
    older: Option<String>,
}

impl Earlier {
    /// The page `mavlink/history` answers, decoded as [`Seen::decode`] does.
    #[must_use]
    pub fn decode(self, fallback: Option<Fallback>) -> MavlinkHistoryResult {
        MavlinkHistoryResult {
            messages: self
                .messages
                .into_iter()
                .map(|(at_us, system_id, component_id, payload)| PastMessage {
                    at_us,
                    system_id,
                    component_id,
                    fields: fields(self.id, &payload, fallback),
                    ..Default::default()
                })
                .collect(),
            older: self.older,
            from_log: false,
            coverage: None,
        }
    }
}

/// A message's fields, decoded with ArduPilot's dialect, else with
/// `fallback`; None when neither knows it.
#[must_use]
pub fn fields(id: u32, payload: &[u8], fallback: Option<Fallback>) -> Option<Map<String, Value>> {
    if let Some(decoded) = uxduck_mavlink::dialect::decode(id, payload) {
        return Some(decoded.fields);
    }
    fallback
        .and_then(|decode| decode(id, payload))
        .map(|(_, fields)| fields)
}

/// A message the dialect table lacks, such as a plugin's vendor message,
/// decoded some other way: its name and fields.
pub type Fallback<'a> = &'a dyn Fn(u32, &[u8]) -> Option<(String, Map<String, Value>)>;

/// What [`Inspector::read`] copied out, not yet decoded.
#[derive(Debug)]
pub struct Seen {
    now_us: u64,
    messages: Vec<(InspectDirection, u32, Latest, f64)>,
}

impl Seen {
    /// What the inspector shows: received first, then sent, each by name,
    /// decoded with ArduPilot's dialect, else with `fallback`.
    #[must_use]
    pub fn decode(self, fallback: Option<Fallback>) -> MavlinkInspectResult {
        let now_us = self.now_us;
        let mut messages: Vec<InspectedMessage> = self
            .messages
            .into_iter()
            .map(|(direction, id, latest, hz)| {
                let (name, dialect, fields) = describe(id, &latest.payload, fallback);
                InspectedMessage {
                    direction,
                    id,
                    name,
                    system_id: latest.system_id,
                    component_id: latest.component_id,
                    hz,
                    count: latest.count,
                    age_ms: now_us.saturating_sub(latest.last_us) / 1000,
                    dialect,
                    fields,
                }
            })
            .collect();
        messages.sort_by(|a, b| {
            a.direction
                .cmp(&b.direction)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        MavlinkInspectResult {
            decoded: true,
            messages,
        }
    }
}

type Described = (String, Option<String>, Option<Map<String, Value>>);

/// A message's name, dialect and fields; only its id when nothing knows it.
fn describe(id: u32, payload: &[u8], fallback: Option<Fallback>) -> Described {
    if let Some(decoded) = uxduck_mavlink::dialect::decode(id, payload) {
        return (
            decoded.name.to_owned(),
            Some(decoded.dialect.name().to_owned()),
            Some(decoded.fields),
        );
    }
    match fallback.and_then(|decode| decode(id, payload)) {
        Some((name, fields)) => (name, None, Some(fields)),
        None => (format!("#{id}"), None, None),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, reason = "tests")]

    use super::*;

    const S: u64 = 1_000_000;

    fn header(message_id: u32) -> Header {
        Header {
            sequence: 0,
            system_id: 1,
            component_id: 1,
            message_id,
        }
    }

    fn ids(id: &str) -> Vec<String> {
        vec![id.to_owned()]
    }

    fn only(table: &Inspector, id: &str, now_us: u64) -> (Latest, f64) {
        let mut read = table.read(&ids(id), now_us).messages;
        assert_eq!(read.len(), 1);
        let (_, _, latest, hz) = read.remove(0);
        (latest, hz)
    }

    #[test]
    fn ten_a_second_for_three_seconds_reads_ten_hz() {
        let mut table = Inspector::default();
        let mut last = 0;
        for n in 0..30 {
            last = 5 * S + n * S / 10;
            table.record(
                "v",
                InspectDirection::Received,
                header(33),
                &[u8::try_from(n).unwrap()],
                last,
            );
        }
        let (latest, hz) = only(&table, "v", last + S / 20);
        assert!((hz - 10.0).abs() < 0.5, "{hz}");
        assert_eq!(latest.count, 30);
        assert_eq!(latest.payload, [29]);
    }

    #[test]
    fn a_rate_shows_before_a_whole_window_has_passed() {
        let mut table = Inspector::default();
        for n in 0..5 {
            table.record("v", InspectDirection::Received, header(0), &[], n * S / 4);
        }
        let (_, hz) = only(&table, "v", S);
        assert!((hz - 4.0).abs() < 0.01, "{hz}");
    }

    #[test]
    fn a_slow_message_keeps_its_rate_between_frames() {
        let mut table = Inspector::default();
        for n in 0..4 {
            table.record("v", InspectDirection::Received, header(1), &[], n * 5 * S);
        }
        // 0.2 Hz: stopped only after three periods, 15 s.
        let (_, hz) = only(&table, "v", 15 * S + 10 * S);
        assert!((hz - 0.2).abs() < 0.01, "{hz}");
        let (_, hz) = only(&table, "v", 15 * S + 16 * S);
        assert_eq!(hz, 0.0);
    }

    #[test]
    fn stopping_drops_the_rate_to_zero() {
        let mut table = Inspector::default();
        let mut last = 0;
        for n in 0..50 {
            last = n * S / 10;
            table.record("v", InspectDirection::Received, header(33), &[], last);
        }
        assert!(only(&table, "v", last + S).1 > 9.0);
        let (latest, hz) = only(&table, "v", last + 3 * S + 1);
        assert_eq!(hz, 0.0);
        assert_eq!(latest.count, 50);
    }

    #[test]
    fn sent_and_received_are_kept_apart() {
        let mut table = Inspector::default();
        table.record("v", InspectDirection::Received, header(0), &[1], S);
        let mut ground = header(0);
        ground.system_id = 255;
        ground.component_id = 190;
        table.record("v", InspectDirection::Sent, ground, &[2], S);
        table.record("v", InspectDirection::Sent, ground, &[3], 2 * S);
        let mut read = table.read(&ids("v"), 2 * S).messages;
        read.sort_by_key(|(direction, ..)| *direction);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].0, InspectDirection::Received);
        assert_eq!((read[0].2.count, read[0].2.system_id), (1, 1));
        assert_eq!(read[1].0, InspectDirection::Sent);
        assert_eq!((read[1].2.count, read[1].2.system_id), (2, 255));
        assert_eq!(read[1].2.payload, [3]);
    }

    #[test]
    fn too_many_types_start_the_vehicle_again() {
        let mut table = Inspector::default();
        for id in 0..u32::try_from(MAX_TYPES).unwrap() {
            table.record("v", InspectDirection::Received, header(id), &[], S);
        }
        table.record("w", InspectDirection::Received, header(0), &[], S);
        assert_eq!(table.vehicles["v"].latest.len(), MAX_TYPES);
        // A type it has already is no new entry.
        table.record("v", InspectDirection::Received, header(7), &[], 2 * S);
        assert_eq!(table.vehicles["v"].latest.len(), MAX_TYPES);
        table.record("v", InspectDirection::Received, header(100_000), &[], 2 * S);
        assert_eq!(table.vehicles["v"].latest.len(), 1);
        assert_eq!(table.vehicles["v"].history.len(), 1, "and its history");
        assert_eq!(
            table.vehicles["w"].latest.len(),
            1,
            "other vehicles keep theirs"
        );
    }

    #[test]
    fn too_many_vehicles_drop_the_quietest() {
        let mut table = Inspector::default();
        for n in 0..u64::try_from(MAX_VEHICLES).unwrap() {
            table.record(
                &format!("v{n}"),
                InspectDirection::Received,
                header(0),
                &[],
                (n + 1) * S,
            );
        }
        table.record("new", InspectDirection::Received, header(0), &[], 100 * S);
        assert_eq!(table.vehicles.len(), MAX_VEHICLES);
        assert!(!table.vehicles.contains_key("v0"));
        assert!(table.vehicles.contains_key("new"));
    }

    #[test]
    fn earlier_ids_are_merged_newest_first() {
        let mut table = Inspector::default();
        table.record(
            "provisional",
            InspectDirection::Received,
            header(0),
            &[1],
            S,
        );
        table.record(
            "provisional",
            InspectDirection::Received,
            header(0),
            &[2],
            2 * S,
        );
        table.record(
            "lasting",
            InspectDirection::Received,
            header(0),
            &[3],
            3 * S,
        );
        let both = vec!["lasting".to_owned(), "provisional".to_owned()];
        let read = table.read(&both, 3 * S).messages;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].2.count, 3);
        assert_eq!(read[0].2.payload, [3]);
    }

    #[test]
    fn heartbeat_freshness_uses_received_beats_and_all_vehicle_aliases() {
        let mut table = Inspector::default();
        let ids = vec!["lasting".to_owned(), "provisional".to_owned()];
        assert_eq!(table.heartbeat_us(&ids), None);
        table.record("provisional", InspectDirection::Received, header(0), &[], S);
        table.record("lasting", InspectDirection::Received, header(0), &[], 2 * S);
        table.record("lasting", InspectDirection::Sent, header(0), &[], 3 * S);
        table.record(
            "lasting",
            InspectDirection::Received,
            header(33),
            &[],
            4 * S,
        );
        table.record(
            "unrelated",
            InspectDirection::Received,
            header(0),
            &[],
            5 * S,
        );
        assert_eq!(table.heartbeat_us(&ids), Some(2 * S));
        assert_eq!(table.heartbeat_us(&["provisional".into()]), Some(S));
    }

    #[test]
    fn a_heartbeat_is_decoded_on_read_and_an_unknown_message_by_the_fallback() {
        // HEARTBEAT: custom_mode 0, type 2 (quadrotor), autopilot 3
        // (ArduPilot), base_mode 81, system_status 3, mavlink_version 3.
        let payload = [0, 0, 0, 0, 2, 3, 81, 3, 3];
        let mut table = Inspector::default();
        table.record("v", InspectDirection::Received, header(0), &payload, 10 * S);
        table.record(
            "v",
            InspectDirection::Received,
            header(46_000),
            &[7],
            10 * S,
        );
        table.record("v", InspectDirection::Received, header(46_001), &[], 10 * S);
        let vendor = |id: u32, payload: &[u8]| {
            (id == 46_000).then(|| {
                let mut fields = Map::new();
                fields.insert("mode".into(), Value::from(payload[0]));
                ("ACME_STATUS".to_owned(), fields)
            })
        };
        let result = table.read(&ids("v"), 10 * S + S / 2).decode(Some(&vendor));
        assert!(result.decoded);
        let names: Vec<&str> = result.messages.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["#46001", "ACME_STATUS", "HEARTBEAT"]);
        let heartbeat = &result.messages[2];
        assert_eq!(heartbeat.dialect.as_deref(), Some("common"));
        assert_eq!(heartbeat.age_ms, 500);
        let fields = heartbeat.fields.as_ref().unwrap();
        assert_eq!(fields["type"], 2);
        assert_eq!(fields["autopilot"], 3);
        let vendor = &result.messages[1];
        assert_eq!(vendor.dialect, None);
        assert_eq!(vendor.fields.as_ref().unwrap()["mode"], 7);
        assert_eq!(result.messages[0].fields, None);
        // Without a fallback, only its id.
        let result = table.read(&ids("v"), 11 * S).decode(None);
        assert!(result.messages.iter().any(|m| m.name == "#46000"));
    }

    fn page(
        table: &Inspector,
        direction: InspectDirection,
        before: Option<&str>,
        limit: usize,
    ) -> (Vec<u8>, Option<String>) {
        let page = table
            .history(&ids("v"), direction, 33, before, limit)
            .unwrap();
        let firsts = page.messages.iter().map(|m| m.3[0]).collect();
        (firsts, page.older)
    }

    #[test]
    fn history_keeps_the_last_200_and_pages_newest_first() {
        let mut table = Inspector::default();
        for n in 0..250u64 {
            let byte = u8::try_from(n % 256).unwrap();
            table.record("v", InspectDirection::Received, header(33), &[byte], n * S);
        }
        let (first, older) = page(&table, InspectDirection::Received, None, 3);
        assert_eq!(first, [249, 248, 247]);
        let (second, older) = page(&table, InspectDirection::Received, older.as_deref(), 3);
        assert_eq!(second, [246, 245, 244]);
        // The rest in one page: the 194 left, and nothing older.
        let (rest, older) = page(&table, InspectDirection::Received, older.as_deref(), 500);
        assert_eq!(rest.len(), 194);
        assert_eq!(rest.last(), Some(&50));
        assert_eq!(older, None);
        // Times and ids come back with each.
        let page = table
            .history(&ids("v"), InspectDirection::Received, 33, None, 1)
            .unwrap()
            .decode(None);
        assert_eq!(page.messages[0].at_us, 249 * S);
        assert_eq!(page.messages[0].system_id, 1);
        assert!(!page.from_log);
        // The ring reused its buffers once full.
        assert_eq!(
            table.vehicles["v"].history[&(InspectDirection::Received, 33)]
                .entries
                .len(),
            HISTORY
        );
    }

    #[test]
    fn history_keeps_sent_and_received_apart_and_merges_earlier_ids() {
        let mut table = Inspector::default();
        table.record("old", InspectDirection::Received, header(33), &[1], S);
        table.record("v", InspectDirection::Sent, header(33), &[2], 2 * S);
        table.record("v", InspectDirection::Received, header(33), &[3], 3 * S);
        table.record("v", InspectDirection::Received, header(34), &[4], 4 * S);
        let both = vec!["v".to_owned(), "old".to_owned()];
        let received = table
            .history(&both, InspectDirection::Received, 33, None, 10)
            .unwrap();
        let firsts: Vec<u8> = received.messages.iter().map(|m| m.3[0]).collect();
        assert_eq!(firsts, [3, 1]);
        let (sent, older) = page(&table, InspectDirection::Sent, None, 10);
        assert_eq!((sent, older), (vec![2], None));
    }

    #[test]
    fn history_kept_one_way_only_and_bad_cursors_refused() {
        let mut table = Inspector::default().keep_history(false, true);
        table.record("v", InspectDirection::Received, header(33), &[1], S);
        table.record("v", InspectDirection::Sent, header(33), &[2], S);
        assert_eq!(
            page(&table, InspectDirection::Received, None, 10).0,
            Vec::<u8>::new()
        );
        assert_eq!(page(&table, InspectDirection::Sent, None, 10).0, [2]);
        for bad in ["", "m", "x12", "m-1", "t1.2"] {
            assert_eq!(
                table
                    .history(&ids("v"), InspectDirection::Sent, 33, Some(bad), 10)
                    .unwrap_err(),
                BadCursor,
                "{bad}"
            );
        }
        assert_eq!(page_size(None), 50);
        assert_eq!(page_size(Some(0)), 1);
        assert_eq!(page_size(Some(10_000)), 500);
    }

    #[test]
    fn history_stops_growing_past_its_bytes() {
        let mut table = Inspector::default();
        let long = [7u8; 255];
        // 400 types of 200 long messages would be 20 MB.
        for n in 0..HISTORY {
            for id in 0..400 {
                let at = u64::try_from(n).unwrap() * S;
                table.record("v", InspectDirection::Received, header(id), &long, at);
            }
        }
        let vehicle = &table.vehicles["v"];
        assert!(
            vehicle.history_bytes <= HISTORY_BYTES + 300,
            "{}",
            vehicle.history_bytes
        );
        assert!(
            vehicle
                .history
                .values()
                .all(|ring| !ring.entries.is_empty())
        );
        // Each still keeps its newest.
        let page = table
            .history(&ids("v"), InspectDirection::Received, 399, None, 1)
            .unwrap();
        assert_eq!(page.messages[0].0, 199 * S);
    }
}

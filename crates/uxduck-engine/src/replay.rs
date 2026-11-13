//! Replay (ADR-0039): a flight played back on one clock. The replayer
//! feeds tlog records, each with its own recorded time as "now", to a
//! MAVLink session of its own (ADR-0004's isolated replay session), and its
//! effects reach the engine as the live session's do: every tab, the agent
//! and MCP see a replay vehicle through the same state.
//!
//! A replay vehicle is recorded nowhere (R3): it is committed under the
//! [`ACTOR`] that sync ignores, never goes through the hooks that make
//! flights, history, notifications, recordings, practice, preflight or
//! analytics of a live vehicle, refuses every command, and goes when the
//! replay closes or UXDuck starts.
//!
//! Seeking back rebuilds (R4): a new session fed from the range's start up
//! to the clock, with the effects held and applied once, then one push with
//! the generation raised.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use base64::Engine as _;
use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::flight::FlightLogResult;
use uxduck_contracts::dto::param_definitions::ParamChangeBy;
use uxduck_contracts::dto::replay::{
    FlightSample, FlightSeries, FlightSeriesParams, FlightSeriesResult, LogColumn, LogMessage,
    ReplayAttitude, ReplayClock, ReplayGap, ReplayLogFieldsParams, ReplayLogFieldsResult,
    ReplayMark, ReplayMarkKind, ReplayMatch, ReplayMotion, ReplayMotionParams, ReplayMotionResult,
    ReplayMotionSample, ReplayOpenParams, ReplayPath, ReplayPathPoint, ReplayRecording,
    ReplaySource, ReplaySourceKind, ReplayState, ReplayVehicle,
};
use uxduck_contracts::dto::telemetry::Altitude;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::FLYING_ABOVE_HOME_M;
use uxduck_core::domain::parameter::{ParamKind, Parameter};
use uxduck_core::domain::telemetry::{
    AltitudeFrame, PositionEstimate, TelemetryItem, VehicleTelemetry, off_the_ground,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};
use uxduck_mavlink::decode::Message;
use uxduck_mavlink::identity::replay_vid;
use uxduck_mavlink::mission_log;
use uxduck_mavlink::param_log::{self, Known, Timeline};
use uxduck_mavlink::session::{Config, Effect, Observation, Session};

use crate::rpc::handlers::to_value;
use crate::runtime::ReplayRead;
use crate::{ConnectionId, Engine};

/// A replay vehicle's link: no link entity, nothing open, nothing sent.
pub(crate) const REPLAY_LINK: &str = "replay";
/// Why nothing reaches a replay vehicle: the Cockpit tab's line and every
/// refusal's detail.
pub(crate) const NOTHING_SENT: &str = "This is a replay. Nothing can be sent to it.";
/// Who changes replay vehicles: sync sends nothing of it.
pub(crate) const ACTOR: &str = "replay";
/// Before arming and after disarming, shown greyed (R10).
const LEAD_US: u64 = 10_000_000;
/// The slowest a replay runs. Any speed from here to `MAX_SPEED` is taken;
/// the bar offers ¼, ½, 1, 2, 5, 10 and 16 (R1).
pub const MIN_SPEED: f64 = 0.1;
/// The fastest: what a browser plays video at, so the picture keeps up
/// (R7; maintainer, 2026-10-29).
pub const MAX_SPEED: f64 = 16.0;
/// How often the state is pushed while it plays.
const PUSH_EVERY_US: u64 = 250_000;
/// How often the session is ticked, in the log's time, as the live data
/// plane ticks it.
const SESSION_TICK_US: u64 = 100_000;
/// A vehicle's whole path, at most this many points.
const PATH_POINTS: usize = 2_000;
/// A path point at most this often.
const PATH_EVERY_US: u64 = 1_000_000;
/// The most of a replay's motion `replay/motion` gives at once.
const MOTION_WINDOW_US: u64 = 600_000_000;
/// `MAV_SEVERITY_WARNING`: messages at or above it are marks.
const WARNING: u8 = 4;
const PEER: &str = "replay";

/// When a source plays, on the replay's clock.
fn span(sources: &[ReplaySource], index: u16) -> Option<(u64, u64)> {
    sources
        .get(usize::from(index))
        .map(|s| (s.start_us, s.end_us))
}

/// The same vehicle in a second source playing at the same time as the
/// first (one flight recorded by two apps, or a flight nudged onto another)
/// is a copy of its own, beside it (R20): its id from its original and its
/// source. One that follows the first in time stays the same vehicle.
fn copy_if_shown_twice(
    mut identity: Identity,
    key: Key,
    others: &BTreeMap<Key, Identity>,
    sources: &[ReplaySource],
) -> Identity {
    let Some((from, to)) = span(sources, key.0) else {
        return identity;
    };
    let together = others.iter().any(|(other, them)| {
        them.original_id == identity.original_id
            && other.0 != key.0
            && span(sources, other.0).is_some_and(|(a, b)| a < to && from < b)
    });
    if together {
        let source = &sources[usize::from(key.0)];
        let tag = source
            .flight_id
            .clone()
            .unwrap_or_else(|| source.start_us.to_string());
        identity.replay_id = replay_vid(&format!("{}@{tag}", identity.original_id));
    }
    identity
}

/// Each copy of a vehicle shown twice says when its source begins, and
/// which copy it is in that order (R20).
fn mark_copies(vehicles: &mut BTreeMap<Key, Identity>, sources: &[ReplaySource]) {
    // By original: each copy's id and when it begins, earliest first.
    let mut copies: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    for (key, identity) in vehicles.iter() {
        let at = span(sources, key.0).map_or(0, |(from, _)| from);
        let ids = copies.entry(identity.original_id.clone()).or_default();
        if !ids.iter().any(|(_, id)| *id == identity.replay_id) {
            ids.push((at, identity.replay_id.clone()));
        }
    }
    for ids in copies.values_mut() {
        ids.sort();
    }
    for (key, identity) in vehicles.iter_mut() {
        let Some(ids) = copies
            .get(&identity.original_id)
            .filter(|ids| ids.len() > 1)
        else {
            continue;
        };
        identity.copy_at = span(sources, key.0).map(|(from, _)| from);
        identity.copy_index = ids
            .iter()
            .position(|(_, id)| *id == identity.replay_id)
            .unwrap_or(0);
    }
}

/// A replay vehicle shows its original as it is now (S238): its name,
/// look, colour and size. A later copy of it at the same time takes the
/// colour after its original's, so the tracks are told apart (R20).
fn follow(vehicle: &mut Vehicle, original: &Vehicle, copy_index: usize) {
    vehicle.replay_of = Some(original.id.clone());
    vehicle.name.clone_from(&original.name);
    vehicle.look = original.look;
    vehicle.own_model.clone_from(&original.own_model);
    vehicle.colour = original.colour.map(|colour| {
        let all = uxduck_core::domain::vehicle::VehicleColour::ALL;
        let at = all.iter().position(|c| *c == colour).unwrap_or(0);
        all[(at + copy_index) % all.len()]
    });
    vehicle.size_cm = original.size_cm;
    vehicle.definitions.clone_from(&original.definitions);
    vehicle.dialects.clone_from(&original.dialects);
}

fn actor() -> Actor {
    Actor {
        client_id: ACTOR.into(),
        operator_id: None,
        identity: None,
    }
}

/// One record of the log: its stamp on the replay's clock, its source
/// (R17), and where its frame is.
#[derive(Clone, Copy, Debug)]
struct Record {
    stamp: u64,
    at: usize,
    len: usize,
    source: u16,
}

/// A vehicle in a replay: its source and its system id there.
type Key = (u16, u8);

/// The replay session's link for a source: vehicles of different sources
/// with the same system id stay apart.
fn link_of(source: u16) -> String {
    if source == 0 {
        REPLAY_LINK.to_owned()
    } else {
        format!("{REPLAY_LINK}-{source}")
    }
}

fn source_of(link: &str) -> u16 {
    link.strip_prefix(REPLAY_LINK)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Who a system id in the log is.
#[derive(Clone, Debug)]
struct Identity {
    replay_id: String,
    original_id: String,
    name: String,
    /// When its source begins, for a vehicle shown twice at once (R20).
    copy_at: Option<u64>,
    /// Which copy it is, by when its source begins: a later one takes the
    /// next colour (R20).
    copy_index: usize,
}

/// What the replay was opened from, to name its vehicles.
#[derive(Clone, Debug, Default)]
struct Hint {
    /// The flight's vehicle: its id, system id (unknown once its record
    /// is gone, as a deleted simulation's is) and name.
    vehicle: Option<(String, Option<u8>, String)>,
    link_id: Option<String>,
    /// A DataFlash log as it was read, for its own fields (R15), and the
    /// mission its `CMD` records give.
    log: Option<std::sync::Arc<Vec<u8>>>,
    mission: Vec<uxduck_mavlink::decode::MissionItemInt>,
    /// The parameter frames of its link's log before it (ADR-0050 L7), as
    /// a tlog: where its vehicle's download usually is.
    state_before: Vec<u8>,
    /// A `.bin`'s parameters' defaults, from its `PARM` records.
    param_defaults: BTreeMap<String, f32>,
}

/// A log's bytes, what it is, its range and who it names.
type Opened = (Vec<u8>, ReplaySource, Option<(u64, u64)>, Hint);

/// One source as read, kept to build the replay again on an added source
/// or a nudge.
struct Input {
    source: ReplaySource,
    bytes: Vec<u8>,
    range: Option<(u64, u64)>,
    hint: Hint,
    /// Recorded on this computer: on its clock.
    here: bool,
    clock_from_flight: bool,
    nudge_us: i64,
    /// The flights it reads, when it is flights: where its nudge is kept.
    flight_ids: Vec<String>,
}

/// The clock carried over a rebuild of the sources.
struct Kept {
    name: Option<String>,
    t_us: u64,
    speed: f64,
    playing: bool,
    generation: u64,
    saved: Vec<String>,
}

/// The parameters each source's log holds over time (ADR-0050 L3 to L7),
/// on the replay's clock: from the part of its link's log before it, then
/// its own. Only a uniquely identified autopilot's component supplies a
/// system's set; a larger companion set is not evidence of identity.
fn parameter_timelines(inputs: &[Input], sources: &[ReplaySource]) -> BTreeMap<Key, Timeline> {
    let mut out = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        let Ok(source) = u16::try_from(index) else {
            continue;
        };
        let offset = sources.get(index).map_or(0, |s| s.offset_us);
        let shift = |stamp: u64| -> u64 {
            u64::try_from((i128::from(stamp) + i128::from(offset)).max(0)).unwrap_or(0)
        };
        let records: Vec<(u64, &[u8])> = state_records(input)
            .into_iter()
            .map(|(stamp, frame)| (shift(stamp), frame))
            .collect();
        let mut autopilots: BTreeMap<u8, BTreeSet<u8>> = BTreeMap::new();
        for (_, frame) in &records {
            if uxduck_mavlink::framing::header(frame).is_none_or(|h| h.message_id != 0) {
                continue;
            }
            if let Some(decoded) = uxduck_mavlink::decode::decode(frame)
                && let Message::Heartbeat {
                    mav_type,
                    autopilot,
                    ..
                } = decoded.message
                && mav_type != "Gcs"
                && autopilot != "Invalid"
                && decoded.component_id != uxduck_mavlink::GCS_COMPONENT_ID
            {
                autopilots
                    .entry(decoded.system_id)
                    .or_default()
                    .insert(decoded.component_id);
            }
        }
        for ((system, component), mut timeline) in param_log::timelines(&records) {
            if autopilots
                .get(&system)
                .is_none_or(|components| components.len() != 1 || !components.contains(&component))
            {
                continue;
            }
            for (name, readings) in &mut timeline.readings {
                let default = input.hint.param_defaults.get(name).copied();
                for reading in readings.iter_mut().filter(|r| r.default.is_none()) {
                    reading.default = default;
                }
            }
            out.insert((source, system), timeline);
        }
    }
    out
}

fn state_records(input: &Input) -> Vec<(u64, &[u8])> {
    let own = uxduck_mavlink::tlog::read(&input.bytes);
    let present: std::collections::HashSet<_> = own.iter().copied().collect();
    let mut records: Vec<_> = uxduck_mavlink::tlog::read(&input.hint.state_before)
        .into_iter()
        .filter(|record| !present.contains(record))
        .chain(own)
        .collect();
    records.sort_by_key(|(stamp, _)| *stamp);
    records
}

fn mission_timelines(
    inputs: &[Input],
    sources: &[ReplaySource],
) -> BTreeMap<Key, mission_log::Timeline> {
    let mut out = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        if input.hint.log.is_some() {
            continue;
        }
        let Ok(source) = u16::try_from(index) else {
            continue;
        };
        let offset = sources.get(index).map_or(0, |s| s.offset_us);
        let records: Vec<_> = state_records(input)
            .into_iter()
            .map(|(stamp, frame)| {
                (
                    u64::try_from((i128::from(stamp) + i128::from(offset)).max(0)).unwrap_or(0),
                    frame,
                )
            })
            .collect();
        for ((system, _), timeline) in mission_log::timelines(&records) {
            out.insert((source, system), timeline);
        }
    }
    out
}

/// How far before a flight its link's log is read for recorded state
/// (ADR-0050 L7).
const STATE_BEFORE_BYTES: u64 = 64 << 20;
/// As long, where the flight does not know where it began in its file.
const STATE_BEFORE_US: u64 = 30 * 60 * 1_000_000;

/// Parameter transfers, heartbeat identity, and the two boot clocks used
/// by the parameter timeline. These do not extend replay telemetry.
const PARAM_MESSAGES: [u32; 6] = [0, 22, 23, 30, 33, 110];

/// A gap this long between flights is drawn and skipped (R17).
const GAP_US: u64 = 60_000_000;

/// Where no source covers the range, past a minute.
fn gaps_between(sources: &[ReplaySource]) -> Vec<ReplayGap> {
    let mut spans: Vec<(u64, u64)> = sources.iter().map(|s| (s.start_us, s.end_us)).collect();
    spans.sort_unstable();
    let mut gaps = Vec::new();
    let Some(&(_, mut reach)) = spans.first() else {
        return gaps;
    };
    for &(from, to) in &spans[1..] {
        if from > reach.saturating_add(GAP_US) {
            gaps.push(ReplayGap {
                from_us: reach,
                to_us: from,
            });
        }
        reach = reach.max(to);
    }
    gaps
}

/// A vehicle clock this far from ours is another computer's (R8).
const VEHICLE_CLOCK_US: u64 = 2_000_000;

/// `SYSTEM_TIME`: GPS time against the receive stamp.
const SYSTEM_TIME: u32 = 2;

/// A source's clock against GPS time (R17): the median of
/// `SYSTEM_TIME.time_unix_usec` minus the receive stamp, where the vehicle
/// had a fix. None without it.
fn gps_offset(bytes: &[u8], records: &[Record]) -> Option<i64> {
    let mut offsets: Vec<i64> = records
        .iter()
        .filter_map(|r| {
            let frame = &bytes[r.at..r.at + r.len];
            let header = uxduck_mavlink::framing::header(frame)?;
            if header.message_id != SYSTEM_TIME {
                return None;
            }
            let payload = uxduck_mavlink::framing::payload(frame)?;
            let mut unix = [0u8; 8];
            let n = payload.len().min(8);
            unix[..n].copy_from_slice(&payload[..n]);
            let unix = u64::from_le_bytes(unix);
            uxduck_mavlink::tlog::sane(unix)
                .then(|| i64::try_from(i128::from(unix) - i128::from(r.stamp)).ok())
                .flatten()
        })
        .collect();
    if offsets.is_empty() {
        return None;
    }
    offsets.sort_unstable();
    Some(offsets[offsets.len() / 2])
}

/// The replay open now.
struct Replay {
    /// The first source, which names it.
    source: ReplaySource,
    /// Every source, on the replay's clock (R17).
    sources: Vec<ReplaySource>,
    /// As read, to build it again.
    inputs: Vec<Input>,
    gaps: Vec<ReplayGap>,
    clock: ReplayClock,
    bytes: Vec<u8>,
    records: Vec<Record>,
    start_us: u64,
    end_us: u64,
    /// The clock.
    t_us: u64,
    speed: f64,
    playing: bool,
    /// While playing: this computer's time and T when it last started or
    /// changed speed.
    anchor: (u64, u64),
    generation: u64,
    /// The next record to feed.
    next: usize,
    session: Session,
    last_tick: u64,
    /// By source and system id.
    vehicles: BTreeMap<Key, Identity>,
    /// The session's ids, and their keys.
    session_ids: HashMap<String, Key>,
    focus: Option<Key>,
    marks: Vec<ReplayMark>,
    paths: Vec<ReplayPath>,
    /// Every position each vehicle logged, by replay id, for `replay/motion`.
    motion: BTreeMap<String, Vec<ReplayMotionSample>>,
    /// The cameras' recordings that meet the range (R7).
    recordings: Vec<ReplayRecording>,
    /// Its flights saved to the account (R11).
    saved: Vec<String>,
    /// The name a person gave it, when it is not an operation's (R21).
    name: Option<String>,
    /// Each vehicle's parameters over its log (ADR-0050 L3 to L6), by
    /// source and system id, on the replay's clock.
    params: BTreeMap<Key, Timeline>,
    /// How many readings and boot boundaries were last applied for each one.
    params_applied: BTreeMap<Key, usize>,
    missions: BTreeMap<Key, mission_log::Timeline>,
    missions_applied: BTreeMap<String, (Option<Key>, Option<usize>)>,
}

impl Replay {
    fn state(&self, now_us: u64, with_paths: bool) -> ReplayState {
        ReplayState {
            open: true,
            name: self.name.clone(),
            clock: self.clock,
            start_us: self.start_us,
            end_us: self.end_us,
            time_us: self.t_us,
            at_us: now_us,
            speed: self.speed,
            playing: self.playing,
            generation: self.generation,
            sources: self.sources.clone(),
            vehicles: self
                .vehicles
                .values()
                .map(|v| ReplayVehicle {
                    id: v.replay_id.clone(),
                    original_id: v.original_id.clone(),
                    name: v.name.clone(),
                })
                .collect(),
            focus_vehicle_id: self
                .focus
                .and_then(|s| self.vehicles.get(&s))
                .map(|v| v.replay_id.clone()),
            marks: self.marks.clone(),
            recordings: self.recordings.clone(),
            gaps: self.gaps.clone(),
            saved_flight_ids: self.saved.clone(),
            paths: with_paths.then(|| self.paths.clone()),
        }
    }

    /// Feeds the records up to `t`, returning the session's effects.
    fn feed(&mut self, t: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        let first = self.next;
        while let Some(&record) = self.records.get(self.next) {
            if record.stamp > t {
                break;
            }
            let frame = self.bytes[record.at..record.at + record.len].to_vec();
            effects.extend(self.session.ingest(
                &link_of(record.source),
                PEER,
                &frame,
                record.stamp,
            ));
            if tick_due(&self.records, self.next, self.last_tick) {
                effects.extend(self.session.tick(record.stamp));
                self.last_tick = record.stamp;
            }
            self.next += 1;
        }
        // What is pending goes out now, at the last record fed: also when
        // it came at the moment of the last tick, after it, as a position
        // logged with the first heartbeat does.
        if self.next > first {
            let last = self.records[self.next - 1].stamp;
            effects.extend(self.session.tick(last));
            self.last_tick = last;
        }
        effects
    }

    /// The clock now, while playing.
    fn running(&self, now_us: u64) -> u64 {
        if !self.playing {
            return self.t_us;
        }
        let (wall, t) = self.anchor;
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "microseconds of a replay, far inside f64's integers"
        )]
        let moved = (now_us.saturating_sub(wall) as f64 * self.speed) as u64;
        t.saturating_add(moved).min(self.end_us)
    }

    fn remember(&mut self, seen: &Observation) {
        let key = (source_of(&seen.link_id), seen.system_id);
        for id in std::iter::once(&seen.id).chain(&seen.aliases) {
            self.session_ids.insert(id.clone(), key);
        }
    }

    fn system_of(&self, session_id: &str) -> Option<Key> {
        self.session_ids.get(session_id).copied()
    }
}

/// The replay open now, and who is told about it.
#[derive(Default)]
pub(crate) struct Replays {
    open: Option<Replay>,
    watchers: BTreeSet<ConnectionId>,
    last_push_us: u64,
    /// While a replay's effects are applied: the clock, for the times of
    /// what they record (messages).
    pub(crate) now_us: Option<u64>,
    /// Account logs being joined for a replay, by account order (R12).
    pub(crate) awaiting: BTreeMap<String, (ConnectionId, ReplayOpenParams)>,
}

impl std::fmt::Debug for Replays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replays")
            .field("open", &self.open.is_some())
            .field("watchers", &self.watchers)
            .finish_non_exhaustive()
    }
}

/// When a link's tlog began, by its name (`<link id>-<unix seconds>.tlog`).
fn log_start_us(file: &str) -> Option<u64> {
    let (_, secs) = file.strip_suffix(".tlog")?.rsplit_once('-')?;
    secs.parse::<u64>().ok()?.checked_mul(1_000_000)
}

/// A DataFlash log (`.bin`) as tlog records on its own clock, through
/// its producer (R15), kept as read for its own fields; a tlog as it is.
fn as_tlog(
    bytes: Vec<u8>,
    source: &mut ReplaySource,
    hint: &mut Hint,
) -> Result<Vec<u8>, ApiError> {
    if !uxduck_mavlink::dataflash::is_dataflash(&bytes) {
        return Ok(bytes);
    }
    source.kind = ReplaySourceKind::Log;
    let (tlog, about) = uxduck_mavlink::dataflash::to_tlog(&bytes)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    // The log names no vehicle: its file does, with what it said of itself.
    if let Some(firmware) = about.firmware {
        source.detail = Some(format!("System {} · {firmware}", about.system_id));
    }
    hint.mission = about.mission;
    hint.param_defaults = about.defaults;
    hint.log = Some(std::sync::Arc::new(bytes));
    Ok(tlog)
}

/// Messages that begin with `time_boot_ms`: `SCALED_PRESSURE`, `ATTITUDE`,
/// `LOCAL_POSITION_NED`, `GLOBAL_POSITION_INT`, `RC_CHANNELS`,
/// `NAV_CONTROLLER_OUTPUT` is not one.
const BOOT_FIRST: [u32; 5] = [29, 30, 32, 33, 65];

/// A tlog whose stamps are nonsense (zero, or before 2000) restamped on
/// the vehicle's own time since boot (R11): each record at the last
/// `time_boot_ms` heard before it. None when its stamps are good, or it
/// gives no time since boot either.
fn on_boot_time(bytes: &[u8]) -> Option<Vec<u8>> {
    let records = uxduck_mavlink::tlog::read(bytes);
    let sane = records
        .iter()
        .filter(|(stamp, _)| uxduck_mavlink::tlog::sane(*stamp))
        .count();
    if records.is_empty() || sane * 2 > records.len() {
        return None;
    }
    let mut boot_us: Option<u64> = None;
    let mut out = Vec::with_capacity(bytes.len());
    for (_, frame) in &records {
        let header = uxduck_mavlink::framing::header(frame);
        if let (Some(header), Some(payload)) = (header, uxduck_mavlink::framing::payload(frame))
            && BOOT_FIRST.contains(&header.message_id)
        {
            let mut ms = [0u8; 4];
            let n = payload.len().min(4);
            ms[..n].copy_from_slice(&payload[..n]);
            boot_us = Some(u64::from(u32::from_le_bytes(ms)) * 1_000);
        }
        let _ = uxduck_mavlink::tlog::record(&mut out, boot_us.unwrap_or(0), frame);
    }
    boot_us.map(|_| out)
}

/// A log's records in time order, of source 0.
fn index_of(bytes: &[u8]) -> Vec<Record> {
    index(bytes)
}

/// A log's records in time order.
fn index(bytes: &[u8]) -> Vec<Record> {
    let base = bytes.as_ptr() as usize;
    let mut records: Vec<Record> = uxduck_mavlink::tlog::read(bytes)
        .into_iter()
        .map(|(stamp, frame)| Record {
            stamp,
            at: frame.as_ptr() as usize - base,
            len: frame.len(),
            source: 0,
        })
        .collect();
    records.sort_by_key(|r| r.stamp);
    records
}

/// Whether the session ticks after record `i`: once its tick period has
/// passed, and only after the last record of its moment, so what was
/// logged together comes out together, at that moment.
fn tick_due(records: &[Record], i: usize, last_tick: u64) -> bool {
    let stamp = records[i].stamp;
    stamp.saturating_sub(last_tick) >= SESSION_TICK_US
        && records.get(i + 1).is_none_or(|next| next.stamp != stamp)
}

/// A position this soon after a vehicle is first heard is part of its
/// first moment: a log's first heartbeat comes a moment before it.
const PLACED_WITHIN_US: u64 = 2_000_000;

impl Pass {
    /// Where each vehicle begins: when first heard, or when it first had a
    /// position if that came a moment later, so a replay never opens on a
    /// vehicle about to say where it is. One with no fix for longer begins
    /// without, as it was.
    fn starts(&self) -> Vec<u64> {
        self.first_heard
            .iter()
            .map(|(key, &heard)| match self.first_placed.get(key) {
                Some(&placed) if placed.saturating_sub(heard) <= PLACED_WITHIN_US => {
                    placed.max(heard)
                }
                _ => heard,
            })
            .collect()
    }

    /// Where each vehicle is first shown: as [`Self::starts`], or at its
    /// first position when that came before it armed, so a flight opens
    /// with its vehicle on the map even when the log's lead-in began
    /// before its GPS had a fix. Scrubbing back still shows it without.
    fn opens(&self) -> Vec<u64> {
        self.first_heard
            .iter()
            .map(|(key, &heard)| {
                let armed = self
                    .marks
                    .iter()
                    .find(|(k, m)| k == key && m.kind == ReplayMarkKind::Armed)
                    .map(|(_, m)| m.at_us);
                match self.first_placed.get(key) {
                    Some(&placed)
                        if placed.saturating_sub(heard) <= PLACED_WITHIN_US
                            || armed.is_some_and(|armed| placed <= armed) =>
                    {
                        placed.max(heard)
                    }
                    _ => heard,
                }
            })
            .collect()
    }
}

/// What one pass over a log finds (R6): the vehicles heard, the marks and
/// each vehicle's whole path.
#[derive(Default)]
struct Pass {
    /// By key: the session's last id for it.
    heard: BTreeMap<Key, String>,
    /// Those that said they are simulators (a `SIM_` parameter): their UID
    /// is every simulator's of a build, so it says nothing of who they are.
    simulated: BTreeSet<Key>,
    /// The UID each said it has.
    uids: BTreeMap<Key, String>,
    /// What each said of itself last: its type, autopilot and firmware.
    observed: BTreeMap<Key, Observation>,
    first_heard: BTreeMap<Key, u64>,
    /// When each vehicle first had a position.
    first_placed: BTreeMap<Key, u64>,
    marks: Vec<(Key, ReplayMark)>,
    paths: BTreeMap<Key, Vec<ReplayPathPoint>>,
    /// Every position each vehicle logged, oldest first.
    motion: BTreeMap<Key, Vec<ReplayMotionSample>>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one pass, one arm per kind of effect"
)]
fn pass(bytes: &[u8], records: &[Record]) -> Pass {
    #[derive(Default)]
    struct Track {
        armed: bool,
        mode: Option<(String, u64)>,
        air: Option<u64>,
        in_air: bool,
        telemetry: VehicleTelemetry,
        /// Its latest attitude items, for each position's sample.
        attitude: Vec<TelemetryItem>,
        /// The mission step it last reached, and when.
        reached: Option<(i64, u64)>,
        last_point: u64,
        last: u64,
    }
    let mut out = Pass::default();
    let mut session = Session::for_replay(Config::default());
    let mut ids: HashMap<String, Key> = HashMap::new();
    let mut tracks: BTreeMap<Key, Track> = BTreeMap::new();
    let mut last_tick = 0u64;
    let mut handle = |effects: Vec<Effect>, at: u64, out: &mut Pass| {
        for effect in effects {
            match effect {
                Effect::Vehicle(seen) => {
                    let system = (source_of(&seen.link_id), seen.system_id);
                    for id in std::iter::once(&seen.id).chain(&seen.aliases) {
                        ids.insert(id.clone(), system);
                    }
                    out.heard.insert(system, seen.id.clone());
                    if let Some(uid) = &seen.uid {
                        out.uids.insert(system, uid.clone());
                    }
                    out.observed.insert(system, (*seen).clone());
                    out.first_heard.entry(system).or_insert(at);
                    let track = tracks.entry(system).or_default();
                    track.last = at;
                    if seen.armed != track.armed {
                        track.armed = seen.armed;
                        out.marks.push((
                            system,
                            ReplayMark {
                                kind: if seen.armed {
                                    ReplayMarkKind::Armed
                                } else {
                                    ReplayMarkKind::Disarmed
                                },
                                at_us: at,
                                ..ReplayMark::default()
                            },
                        ));
                    }
                    if seen.mode.as_ref() != track.mode.as_ref().map(|(m, _)| m) {
                        if let Some((mode, since)) = track.mode.take() {
                            out.marks.push((system, mode_mark(mode, since, at)));
                        }
                        track.mode = seen.mode.clone().map(|m| (m, at));
                    }
                }
                Effect::Telemetry(update) => {
                    let Some(&system) = ids.get(&update.vehicle_id) else {
                        continue;
                    };
                    if !update.positions.is_empty() {
                        out.first_placed.entry(system).or_insert(at);
                    }
                    let track = tracks.entry(system).or_default();
                    track.last = at;
                    track.telemetry.merge(&update);
                    // Each mission step reached (S343), once, though a log
                    // may hold it from more than one channel. PX4 counts
                    // its items from 0, ArduPilot from home.
                    if let Some(seq) = update
                        .items
                        .iter()
                        .find(|i| i.key == "mission.reached")
                        .and_then(|i| i.value)
                    {
                        let px4 = out
                            .observed
                            .get(&system)
                            .is_some_and(|o| o.autopilot == "Px4");
                        #[allow(clippy::cast_possible_truncation, reason = "a mission item number")]
                        let step = seq.round() as i64 + i64::from(px4);
                        let again = track.reached.is_some_and(|(last, when)| {
                            last == step && at.saturating_sub(when) < 2_000_000
                        });
                        if step >= 1 && !again {
                            track.reached = Some((step, at));
                            out.marks.push((
                                system,
                                ReplayMark {
                                    kind: ReplayMarkKind::Step,
                                    at_us: at,
                                    text: Some(format!("Reached step {step}")),
                                    ..ReplayMark::default()
                                },
                            ));
                        }
                    }
                    for item in update
                        .items
                        .iter()
                        .filter(|i| i.key.starts_with("attitude."))
                    {
                        match track.attitude.iter_mut().find(|kept| kept.key == item.key) {
                            Some(kept) => kept.clone_from(item),
                            None => track.attitude.push(item.clone()),
                        }
                    }
                    // The position the maps show (`bestPosition`): the
                    // autopilot's own estimate first.
                    let shown = track
                        .telemetry
                        .positions
                        .iter()
                        .find(|p| p.source == "fused")
                        .or_else(|| track.telemetry.positions.first());
                    if let Some(position) = shown {
                        let samples = out.motion.entry(system).or_default();
                        if samples
                            .last()
                            .is_none_or(|last| position.time_us > last.time_us)
                        {
                            samples.push(motion_sample(position, &track.attitude));
                        }
                    }
                    let was = track.in_air;
                    track.in_air = in_the_air(&track.telemetry, was);
                    if track.in_air && !was {
                        track.air = Some(at);
                    } else if !track.in_air
                        && was
                        && let Some(from) = track.air.take()
                    {
                        out.marks.push((system, air_mark(from, at)));
                    }
                    if let Some(fused) = update.positions.iter().find(|p| p.source == "fused")
                        && at.saturating_sub(track.last_point) >= PATH_EVERY_US
                    {
                        track.last_point = at;
                        out.paths.entry(system).or_default().push(ReplayPathPoint {
                            latitude_deg: fused.latitude_deg,
                            longitude_deg: fused.longitude_deg,
                            amsl_m: fused
                                .altitudes
                                .iter()
                                .find(|a| a.frame == AltitudeFrame::Amsl)
                                .map(|a| a.meters),
                            time_us: at,
                        });
                    }
                }
                Effect::StatusText {
                    vehicle_id,
                    severity,
                    text,
                    ..
                } if severity <= WARNING => {
                    let Some(&system) = ids.get(&vehicle_id) else {
                        continue;
                    };
                    out.marks.push((
                        system,
                        ReplayMark {
                            kind: ReplayMarkKind::Message,
                            at_us: at,
                            text: Some(text),
                            severity: Some(severity),
                            ..ReplayMark::default()
                        },
                    ));
                }
                Effect::ParamValue {
                    vehicle_id, param, ..
                } if param.name.starts_with("SIM_") => {
                    if let Some(&system) = ids.get(&vehicle_id) {
                        out.simulated.insert(system);
                    }
                }
                _ => {}
            }
        }
    };
    for (i, record) in records.iter().enumerate() {
        let frame = &bytes[record.at..record.at + record.len];
        let effects = session.ingest(&link_of(record.source), PEER, frame, record.stamp);
        handle(effects, record.stamp, &mut out);
        if tick_due(records, i, last_tick) {
            let effects = session.tick(record.stamp);
            handle(effects, record.stamp, &mut out);
            last_tick = record.stamp;
        }
    }
    if let Some(last) = records.last() {
        let effects = session.tick(last.stamp);
        handle(effects, last.stamp, &mut out);
    }
    for (system, track) in tracks {
        if let Some((mode, since)) = track.mode {
            out.marks.push((system, mode_mark(mode, since, track.last)));
        }
        if let Some(from) = track.air {
            out.marks.push((system, air_mark(from, track.last)));
        }
    }
    out.marks.sort_by_key(|(_, m)| m.at_us);
    for points in out.paths.values_mut() {
        thin(points, PATH_POINTS);
    }
    out
}

/// A vehicle as logged with `position`: there, facing its heading, else
/// its yaw, in its latest attitude.
fn motion_sample(position: &PositionEstimate, attitude: &[TelemetryItem]) -> ReplayMotionSample {
    let yaw = attitude
        .iter()
        .find(|i| i.key == "attitude.yaw_deg")
        .and_then(|i| i.value);
    ReplayMotionSample {
        time_us: position.time_us,
        latitude_deg: position.latitude_deg,
        longitude_deg: position.longitude_deg,
        altitudes: position
            .altitudes
            .iter()
            .map(|a| Altitude {
                meters: a.meters,
                frame: a.frame.into(),
            })
            .collect(),
        heading_deg: position.heading_deg.or(yaw),
        attitude: attitude_of(attitude),
    }
}

/// The values of `keys` when one message gave them all, and when.
fn one_message<const N: usize>(
    items: &[TelemetryItem],
    keys: [&str; N],
) -> Option<([f64; N], u64)> {
    let found = keys.map(|key| items.iter().find(|i| i.key == key));
    let first = found[0]?;
    let mut values = [0.0; N];
    for (value, item) in values.iter_mut().zip(found) {
        let item = item?;
        if item.time_us != first.time_us
            || item.component_id != first.component_id
            || item.source != first.source
        {
            return None;
        }
        *value = item.value.filter(|v| v.is_finite())?;
    }
    Some((values, first.time_us))
}

/// The attitude telemetry gives, as the maps read it (`readAttitude`): its
/// quaternion when at least as recent as its angles, else its angles.
fn attitude_of(items: &[TelemetryItem]) -> Option<ReplayAttitude> {
    let angles = one_message(
        items,
        [
            "attitude.roll_deg",
            "attitude.pitch_deg",
            "attitude.yaw_deg",
        ],
    );
    let quaternion = one_message(
        items,
        [
            "attitude.quaternion_w",
            "attitude.quaternion_x",
            "attitude.quaternion_y",
            "attitude.quaternion_z",
        ],
    );
    if let Some(([w, x, y, z], at)) = quaternion
        && angles.is_none_or(|(_, angles_at)| at >= angles_at)
    {
        let n = (w * w + x * x + y * y + z * z).sqrt();
        if n.is_finite() && n > 1e-6 {
            return Some(ReplayAttitude {
                w: w / n,
                x: x / n,
                y: y / n,
                z: z / n,
            });
        }
    }
    let ([roll, pitch, yaw], _) = angles?;
    let (sr, cr) = (roll.to_radians() / 2.0).sin_cos();
    let (sp, cp) = (pitch.to_radians() / 2.0).sin_cos();
    let (sy, cy) = (yaw.to_radians() / 2.0).sin_cos();
    Some(ReplayAttitude {
        w: cr * cp * cy + sr * sp * sy,
        x: sr * cp * cy - cr * sp * sy,
        y: cr * sp * cy + sr * cp * sy,
        z: cr * cp * sy - sr * sp * cy,
    })
}

fn mode_mark(mode: String, since: u64, until: u64) -> ReplayMark {
    ReplayMark {
        kind: ReplayMarkKind::Mode,
        at_us: since,
        until_us: Some(until),
        text: Some(mode),
        ..ReplayMark::default()
    }
}

fn air_mark(from: u64, until: u64) -> ReplayMark {
    ReplayMark {
        kind: ReplayMarkKind::Air,
        at_us: from,
        until_us: Some(until),
        ..ReplayMark::default()
    }
}

/// Off the ground, as the live flight timer decides it: the autopilot's
/// word, else the height above home.
fn in_the_air(telemetry: &VehicleTelemetry, was: bool) -> bool {
    let said = telemetry
        .items
        .iter()
        .find(|i| i.key == "flight.state")
        .and_then(|i| i.text.as_deref())
        .and_then(off_the_ground);
    if let Some(said) = said {
        return said;
    }
    let height = telemetry
        .positions
        .iter()
        .filter(|p| p.source == "fused")
        .flat_map(|p| &p.altitudes)
        .find(|a| a.frame == AltitudeFrame::AboveHome)
        .map(|a| a.meters);
    match height {
        Some(h) if h > FLYING_ABOVE_HOME_M => true,
        Some(h) if h < 1.0 => false,
        Some(_) => was,
        None => false,
    }
}

/// The most keys `flight/series` takes, and the most points a series.
const MAX_SERIES: usize = 16;
const MAX_POINTS: usize = 3_000;

/// A frame's name in a `position.fused.<frame>` key.
fn frame_of(name: &str) -> Option<AltitudeFrame> {
    Some(match name {
        "amsl" => AltitudeFrame::Amsl,
        "ellipsoid" => AltitudeFrame::Ellipsoid,
        "above-home" => AltitudeFrame::AboveHome,
        "above-ground" => AltitudeFrame::AboveGround,
        _ => return None,
    })
}

/// One system's values for `keys` over a log (R13), each at most `max`
/// points with the highs and lows kept.
fn series_of(
    bytes: &[u8],
    records: &[Record],
    system: Option<Key>,
    keys: &[String],
    max: usize,
) -> Vec<FlightSeries> {
    let mut session = Session::for_replay(Config::default());
    let mut ids: HashMap<String, Key> = HashMap::new();
    let mut out: BTreeMap<&str, Vec<FlightSample>> = BTreeMap::new();
    let mut last_tick = 0u64;
    let mut take = |effects: Vec<Effect>, at: u64, ids: &mut HashMap<String, Key>| {
        for effect in effects {
            match effect {
                Effect::Vehicle(seen) => {
                    for id in std::iter::once(&seen.id).chain(&seen.aliases) {
                        ids.insert(id.clone(), (source_of(&seen.link_id), seen.system_id));
                    }
                }
                Effect::Telemetry(update) => {
                    let Some(&from) = ids.get(&update.vehicle_id) else {
                        continue;
                    };
                    if system.is_some_and(|s| s != from) {
                        continue;
                    }
                    for item in &update.items {
                        if let (Some(value), Some(key)) =
                            (item.value, keys.iter().find(|k| **k == item.key))
                            && value.is_finite()
                        {
                            out.entry(key.as_str())
                                .or_default()
                                .push(FlightSample { time_us: at, value });
                        }
                    }
                    for position in update.positions.iter().filter(|p| p.source == "fused") {
                        for key in keys {
                            let Some(frame) =
                                key.strip_prefix("position.fused.").and_then(frame_of)
                            else {
                                continue;
                            };
                            if let Some(a) = position.altitudes.iter().find(|a| a.frame == frame) {
                                out.entry(key.as_str()).or_default().push(FlightSample {
                                    time_us: at,
                                    value: a.meters,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    };
    for (i, record) in records.iter().enumerate() {
        let frame = &bytes[record.at..record.at + record.len];
        let effects = session.ingest(&link_of(record.source), PEER, frame, record.stamp);
        take(effects, record.stamp, &mut ids);
        if tick_due(records, i, last_tick) {
            let effects = session.tick(record.stamp);
            take(effects, record.stamp, &mut ids);
            last_tick = record.stamp;
        }
    }
    if let Some(last) = records.last() {
        let effects = session.tick(last.stamp);
        take(effects, last.stamp, &mut ids);
    }
    keys.iter()
        .filter_map(|key| {
            let samples = out.remove(key.as_str())?;
            let min = samples
                .iter()
                .min_by(|a, b| a.value.total_cmp(&b.value))
                .cloned();
            let max_sample = samples
                .iter()
                .max_by(|a, b| a.value.total_cmp(&b.value))
                .cloned();
            Some(FlightSeries {
                key: key.clone(),
                samples: thin_extremes(samples, max),
                min,
                max: max_sample,
            })
        })
        .collect()
}

/// At most `n` samples: per bucket of the time, its lowest and highest in
/// time order, so a dip survives.
fn thin_extremes(samples: Vec<FlightSample>, n: usize) -> Vec<FlightSample> {
    if samples.len() <= n || n < 4 {
        return samples;
    }
    let buckets = n / 2;
    let per = samples.len().div_ceil(buckets);
    samples
        .chunks(per)
        .flat_map(|chunk| {
            let low = chunk
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.value.total_cmp(&b.1.value))
                .map_or(0, |(i, _)| i);
            let high = chunk
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.value.total_cmp(&b.1.value))
                .map_or(0, |(i, _)| i);
            let (first, second) = (low.min(high), low.max(high));
            if first == second {
                vec![chunk[first].clone()]
            } else {
                vec![chunk[first].clone(), chunk[second].clone()]
            }
        })
        .collect()
}

/// Keeps at most `n` points, evenly, with the first and the last.
fn thin<T: Clone>(points: &mut Vec<T>, n: usize) {
    if points.len() <= n || n < 2 {
        return;
    }
    let len = points.len();
    let kept: Vec<T> = (0..n)
        .map(|i| points[i * (len - 1) / (n - 1)].clone())
        .collect();
    *points = kept;
}

impl Engine {
    /// Brings the replays of `original_id` in step with it: renamed, or its
    /// look or size changed, every replay of it says so (S238).
    pub(crate) fn follow_original(&mut self, original_id: &str) {
        let Some(original) = self
            .state
            .vehicles
            .get(original_id)
            .filter(|v| !v.replay)
            .cloned()
        else {
            return;
        };
        let copy_index = |id: &str| {
            self.replays.open.as_ref().map_or(0, |replay| {
                replay
                    .vehicles
                    .values()
                    .find(|identity| identity.replay_id == id)
                    .map_or(0, |identity| identity.copy_index)
            })
        };
        let behind: Vec<Vehicle> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.replay && v.replay_of.as_deref() == Some(original_id))
            .filter_map(|v| {
                let mut after = v.clone();
                follow(&mut after, &original, copy_index(&v.id));
                (after != *v).then(|| {
                    after.version += 1;
                    after
                })
            })
            .collect();
        for vehicle in behind {
            self.commit_as(actor(), Change::VehicleUpserted { vehicle });
        }
    }

    /// Renames a replay vehicle with no original here: for this replay
    /// only, kept through rebuilds and when the log is opened again in
    /// this run (S238).
    pub(crate) fn rename_replay_vehicle(&mut self, id: &str, name: String) -> Option<Vehicle> {
        let mut vehicle = self.state.vehicles.get(id).filter(|v| v.replay)?.clone();
        if let Some(replay) = self.replays.open.as_mut() {
            for identity in replay.vehicles.values_mut() {
                if identity.replay_id == id {
                    identity.name.clone_from(&name);
                }
            }
        }
        vehicle.name = name;
        vehicle.version += 1;
        self.commit_as(
            actor(),
            Change::VehicleUpserted {
                vehicle: vehicle.clone(),
            },
        );
        Some(vehicle)
    }
    /// The replay's clock while its effects are applied, else now: the
    /// time a replay vehicle's message is said at.
    pub(crate) fn now_for(&self, vehicle: &Vehicle) -> u64 {
        match self.replays.now_us {
            Some(t) if vehicle.replay => t,
            _ => self.clock.now_us(),
        }
    }

    /// Whether a replay is open.
    #[must_use]
    pub fn replay_open(&self) -> bool {
        self.replays.open.is_some()
    }

    /// Replay vehicles left by a UXDuck that stopped with a replay open go
    /// at start: a replay is open while someone watches it (R3).
    pub(crate) fn drop_replay_vehicles(&mut self) {
        let left: Vec<String> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.replay)
            .map(|v| v.id.clone())
            .collect();
        for id in left {
            self.clear_replay_parameters(&id);
            self.commit_as(actor(), Change::VehicleRemoved { id });
        }
    }

    pub(crate) fn open_replay(&mut self, params: &ReplayOpenParams) -> Result<(), ApiError> {
        let inputs = self.replay_inputs(params)?;
        self.open_replay_inputs(inputs)
    }

    fn open_replay_inputs(&mut self, inputs: Vec<Input>) -> Result<(), ApiError> {
        // A file with nothing to play leaves an open replay as it was.
        if inputs
            .iter()
            .all(|i| uxduck_mavlink::tlog::read(&i.bytes).is_empty())
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "Nothing UXDuck can play was recorded there.",
            ));
        }
        let mut inputs = inputs;
        // An operation's flights open as a person matched them (R17).
        if let Some(operation) = self.operation_of(&inputs) {
            for input in &mut inputs {
                input.nudge_us = input
                    .flight_ids
                    .iter()
                    .find_map(|id| operation.clock_offsets_us.get(id))
                    .copied()
                    .unwrap_or(0);
            }
        }
        self.close_replay();
        self.start_replay(inputs, None)
    }

    /// The operation a replay of several flights is: the one holding all
    /// of them. None for one source, or flights of several operations.
    fn operation_of(&self, inputs: &[Input]) -> Option<uxduck_core::domain::operation::Operation> {
        if inputs.len() < 2 {
            return None;
        }
        let flights: Vec<&String> = inputs.iter().flat_map(|i| &i.flight_ids).collect();
        if flights.is_empty() || inputs.iter().any(|i| i.flight_ids.is_empty()) {
            return None;
        }
        self.state
            .operations
            .values()
            .find(|o| flights.iter().all(|f| o.flight_ids.contains(f)))
            .cloned()
    }

    /// The one flight a replay plays, when it plays one flight alone.
    fn flight_of(&self, inputs: &[Input]) -> Option<&uxduck_core::domain::flight::Flight> {
        match inputs {
            [input] => match input.flight_ids.as_slice() {
                [id] => self.state.flights.get(id),
                _ => None,
            },
            _ => None,
        }
    }

    /// A flight renamed while its replay is open: the replay's name follows.
    pub(crate) fn replay_flight_renamed(&mut self, id: &str) {
        let shown = self
            .replays
            .open
            .as_ref()
            .and_then(|r| self.flight_of(&r.inputs))
            .is_some_and(|f| f.id == id);
        if shown {
            self.push_replay(false);
        }
    }

    /// The open replay's nudges kept in its operation (R17), when it is one.
    fn keep_replay_offsets(&mut self, connection: ConnectionId) {
        let Some(replay) = self.replays.open.as_ref() else {
            return;
        };
        let Some(mut operation) = self.operation_of(&replay.inputs) else {
            return;
        };
        for input in &replay.inputs {
            for id in &input.flight_ids {
                if input.nudge_us == 0 {
                    operation.clock_offsets_us.remove(id);
                } else {
                    operation
                        .clock_offsets_us
                        .insert(id.clone(), input.nudge_us);
                }
            }
        }
        if self.state.operations.get(&operation.id) != Some(&operation) {
            operation.version += 1;
            self.commit(connection, Change::OperationUpserted { operation });
        }
    }

    /// The sources a `replay/open` names: a flight, a file, or an
    /// operation's or a set's flights (R17).
    fn replay_inputs(&mut self, params: &ReplayOpenParams) -> Result<Vec<Input>, ApiError> {
        let flights: Vec<String> = match &params.operation_id {
            Some(id) => self
                .state
                .operations
                .get(id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operation {id}")))?
                .flight_ids
                .clone(),
            None => params.flight_ids.clone(),
        };
        if flights.is_empty() {
            return Ok(vec![self.replay_input(params)?]);
        }
        // Flights on one connection are one read of its log: a fleet on one
        // link would otherwise be heard twice.
        let mut groups: Vec<(String, u64, u64, String, Vec<String>)> = Vec::new();
        for id in &flights {
            let Some(flight) = self.state.flights.get(id) else {
                continue;
            };
            let Some(link) = flight.link_id.clone() else {
                continue;
            };
            let (from, to) = self.flight_span(flight, params.ground);
            match groups.iter_mut().find(|g| {
                g.0 == link
                    && from <= g.2.saturating_add(GAP_US)
                    && to.saturating_add(GAP_US) >= g.1
            }) {
                Some(group) => {
                    group.1 = group.1.min(from);
                    group.2 = group.2.max(to);
                    group.4.push(id.clone());
                }
                None => groups.push((link, from, to, id.clone(), vec![id.clone()])),
            }
        }
        let mut inputs = Vec::new();
        let mut missing = 0;
        for (_, from, to, first, flight_ids) in groups {
            let state_before = self.state_before(&first, from);
            match self
                .flight_bytes(&first, Some((from, to)), params.ground)
                .map(|opened| {
                    let (bytes, source, range, mut hint) = opened;
                    hint.state_before = state_before;
                    let (bytes, range) = match on_boot_time(&bytes) {
                        Some(rebased) => (rebased, None),
                        None => (bytes, range),
                    };
                    Input {
                        source,
                        bytes,
                        range,
                        hint,
                        here: true,
                        clock_from_flight: false,
                        nudge_us: 0,
                        flight_ids,
                    }
                }) {
                Ok(input) => inputs.push(input),
                Err(_) => missing += 1,
            }
        }
        if inputs.is_empty() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "None of these flights' logs are on this computer.",
            ));
        }
        let _ = missing;
        Ok(inputs)
    }

    fn replay_input(&mut self, params: &ReplayOpenParams) -> Result<Input, ApiError> {
        let (bytes, source, range, mut hint) = self.replay_bytes(params)?;
        if let (Some(id), Some((from_us, _))) = (&source.flight_id, range) {
            hint.state_before = self.state_before(id, from_us);
        }
        // Nonsense stamps: the vehicle's time since boot instead (R11).
        let (bytes, range) = match on_boot_time(&bytes) {
            Some(rebased) => (rebased, None),
            None => (bytes, range),
        };
        let here = source.kind == ReplaySourceKind::Flight;
        Ok(Input {
            flight_ids: source.flight_id.clone().into_iter().collect(),
            source,
            bytes,
            range,
            hint,
            here,
            clock_from_flight: false,
            nudge_us: 0,
        })
    }

    /// Builds the replay from its sources: each on the replay's clock (R17),
    /// the union of their ranges, the marks pass over them all. `keep`
    /// carries the clock over an added source or a nudge.
    #[allow(clippy::too_many_lines, reason = "one replay from its sources")]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "taken whole, as the sources are"
    )]
    fn start_replay(&mut self, inputs: Vec<Input>, keep: Option<Kept>) -> Result<(), ApiError> {
        let mut bytes = Vec::new();
        let mut records = Vec::new();
        let mut sources = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            let index = u16::try_from(index).unwrap_or(u16::MAX);
            let own = index_of(&input.bytes);
            let (Some(first), Some(last)) = (own.first(), own.last()) else {
                continue;
            };
            let (matched, matched_us) = if input.here {
                (ReplayMatch::SameComputer, 0)
            } else if input.clock_from_flight {
                (ReplayMatch::NotMatched, 0)
            } else if let Some(gps) = gps_offset(&input.bytes, &own) {
                (ReplayMatch::Gps, gps)
            } else {
                (ReplayMatch::NotMatched, 0)
            };
            let offset = matched_us.saturating_add(input.nudge_us);
            let shift = |stamp: u64| -> u64 {
                u64::try_from((i128::from(stamp) + i128::from(offset)).max(0)).unwrap_or(0)
            };
            let base = bytes.len();
            bytes.extend_from_slice(&input.bytes);
            let (from, to) = input.range.map_or((first.stamp, last.stamp), |(from, to)| {
                (
                    from.max(first.stamp),
                    to.min(last.stamp).max(from.max(first.stamp)),
                )
            });
            for record in &own {
                records.push(Record {
                    stamp: shift(record.stamp),
                    at: base + record.at,
                    len: record.len,
                    source: index,
                });
            }
            let mut source = input.source.clone();
            source.start_us = shift(from);
            source.end_us = shift(to);
            source.matched = matched;
            source.offset_us = offset;
            source.nudge_us = input.nudge_us;
            sources.push(source);
        }
        if sources.is_empty() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "Nothing UXDuck can play was recorded there.",
            ));
        }
        records.sort_by_key(|r| r.stamp);
        let start_us = sources.iter().map(|s| s.start_us).min().unwrap_or(0);
        let end_us = sources.iter().map(|s| s.end_us).max().unwrap_or(start_us);
        // Before the first record of a source, nothing of it plays.
        records.retain(|r| {
            sources
                .get(usize::from(r.source))
                .is_some_and(|s| r.stamp >= s.start_us && r.stamp <= s.end_us)
        });
        let clock = if uxduck_mavlink::tlog::sane(start_us) {
            ReplayClock::Wall
        } else {
            ReplayClock::Boot
        };
        let gaps = gaps_between(&sources);
        let found = pass(&bytes, &records);
        // The range begins where a vehicle is first heard: before it there
        // is nothing to show.
        let starts = found.starts();
        let start_us = starts
            .iter()
            .min()
            .copied()
            .map_or(start_us, |heard| heard.clamp(start_us, end_us));
        // It opens once the vehicles heard together at its start all are, a
        // fleet's first heartbeats a moment apart.
        let first_moment = keep.as_ref().map_or_else(
            || {
                let heard = found.opens();
                let earliest = heard.iter().min().copied().unwrap_or(start_us);
                heard
                    .iter()
                    .copied()
                    .filter(|t| *t <= earliest.saturating_add(15_000_000))
                    .max()
                    .unwrap_or(earliest)
                    .clamp(start_us, end_us)
            },
            |k| k.t_us.clamp(start_us, end_us),
        );
        let params = parameter_timelines(&inputs, &sources);
        let missions = mission_timelines(&inputs, &sources);
        let mut replay = Replay {
            source: sources[0].clone(),
            sources,
            inputs: Vec::new(),
            gaps,
            clock,
            bytes: Vec::new(),
            records: Vec::new(),
            start_us,
            end_us,
            t_us: first_moment,
            speed: keep.as_ref().map_or(1.0, |k| k.speed),
            playing: false,
            anchor: (self.clock.now_us(), first_moment),
            generation: keep.as_ref().map_or(1, |k| k.generation + 1),
            next: 0,
            session: Session::for_replay(Config::default()),
            last_tick: 0,
            vehicles: BTreeMap::new(),
            session_ids: HashMap::new(),
            focus: None,
            marks: Vec::new(),
            paths: Vec::new(),
            motion: BTreeMap::new(),
            recordings: Vec::new(),
            saved: keep.as_ref().map(|k| k.saved.clone()).unwrap_or_default(),
            name: keep.as_ref().and_then(|k| k.name.clone()),
            params,
            params_applied: BTreeMap::new(),
            missions,
            missions_applied: BTreeMap::new(),
        };
        for (&key, session_id) in &found.heard {
            let index = usize::from(key.0);
            let Some(input) = inputs.get(index) else {
                continue;
            };
            let simulator = found.simulated.contains(&key);
            let only = found.heard.keys().filter(|k| k.0 == key.0).count() == 1;
            // A real board's UID names it anywhere; anything else is known
            // only as this log's, by when it was first heard in it.
            let board = found.uids.contains_key(&key) && !simulator;
            let own_id = if board {
                session_id.clone()
            } else {
                let heard = found.first_heard.get(&key).copied().unwrap_or_default();
                format!("{session_id}@{heard}")
            };
            let taken: Vec<String> = replay.vehicles.values().map(|v| v.name.clone()).collect();
            let identity = self.identify(
                key.1,
                (session_id, &own_id),
                &input.hint,
                (simulator, only),
                &taken,
            );
            // A real board met only in this log becomes a vehicle UXDuck
            // knows, so the name it is given sticks (S248).
            if board
                && identity.original_id == *session_id
                && self.vehicle(session_id).is_none()
                && let Some(seen) = found.observed.get(&key)
            {
                self.remember_log_vehicle(seen, &identity.name);
            }
            let identity = copy_if_shown_twice(identity, key, &replay.vehicles, &replay.sources);
            replay.vehicles.insert(key, identity);
        }
        mark_copies(&mut replay.vehicles, &replay.sources);
        replay.focus = inputs
            .first()
            .and_then(|i| i.hint.vehicle.as_ref()?.1)
            .map(|system| (0, system))
            .filter(|k| replay.vehicles.contains_key(k))
            .or_else(|| replay.vehicles.keys().next().copied());
        replay.marks = found
            .marks
            .into_iter()
            .filter_map(|(key, mut mark)| {
                mark.vehicle_id
                    .clone_from(&replay.vehicles.get(&key)?.replay_id);
                Some(mark)
            })
            .collect();
        // Each setting's change in its range (ADR-0050 L9), with the marks,
        // except the vehicle's own: those are its doing, not a person's.
        let gcs = self.state.settings.gcs_system_id;
        for (key, timeline) in &replay.params {
            let Some(identity) = replay.vehicles.get(key) else {
                continue;
            };
            let definitions = self.vehicle_definitions(&identity.replay_id);
            for change in timeline.changes(replay.start_us, replay.end_us) {
                let definition = definitions
                    .as_ref()
                    .and_then(|d| d.find(&change.name, false));
                if crate::param_definitions::queries::statistic(&change.name, definition) {
                    continue;
                }
                let by = match crate::param_definitions::queries::change_by(change.source, gcs) {
                    (ParamChangeBy::Uxduck, _) => self
                        .param_reason(&identity.original_id, &change.name, change.at_us)
                        .map_or_else(|| "by UXDuck".to_owned(), |why| format!("by UXDuck: {why}")),
                    (ParamChangeBy::GroundStation, system) => {
                        format!("by ground station {}", system.unwrap_or_default())
                    }
                    (ParamChangeBy::Vehicle, _) => continue,
                    (ParamChangeBy::Unseen, _) => "(not seen how)".to_owned(),
                };
                replay.marks.push(ReplayMark {
                    kind: ReplayMarkKind::Parameter,
                    vehicle_id: identity.replay_id.clone(),
                    at_us: change.at_us,
                    text: Some(format!(
                        "{} {} → {} {by}",
                        change.name, change.from, change.to
                    )),
                    ..ReplayMark::default()
                });
            }
        }
        replay.marks.sort_by_key(|m| m.at_us);
        replay.paths = found
            .paths
            .into_iter()
            .filter_map(|(key, points)| {
                Some(ReplayPath {
                    vehicle_id: replay.vehicles.get(&key)?.replay_id.clone(),
                    points,
                })
            })
            .collect();
        replay.motion = found
            .motion
            .into_iter()
            .filter_map(|(key, samples)| {
                Some((replay.vehicles.get(&key)?.replay_id.clone(), samples))
            })
            .collect();
        replay.recordings = self.replay_recordings(&replay);
        replay.bytes = bytes;
        replay.records = records;
        replay.inputs = inputs;
        self.replays.open = Some(replay);
        // Built again: the new generation first, so a client clears what it
        // gathered, then takes what this build gives.
        if keep.is_some() {
            self.push_replay(false);
        }
        // The first moment: the vehicles as they were when first heard.
        self.advance_replay(first_moment, true);
        if keep.as_ref().is_some_and(|k| k.playing) {
            self.play_replay(true);
        }
        self.push_replay(true);
        Ok(())
    }

    /// Adds a source to the replay open now (R17), or nudges one: built
    /// again on one clock, the clock where it was.
    fn rebuild_sources(
        &mut self,
        change: impl FnOnce(&mut Vec<Input>) -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        let now = self.clock.now_us();
        let Some(replay) = self.replays.open.as_mut() else {
            return Err(ApiError::new(ErrorKind::NotFound, "No replay is open."));
        };
        let mut inputs = std::mem::take(&mut replay.inputs);
        let kept = Kept {
            name: replay.name.clone(),
            t_us: replay.running(now),
            speed: replay.speed,
            playing: replay.playing,
            generation: replay.generation,
            saved: replay.saved.clone(),
        };
        if let Err(why) = change(&mut inputs) {
            if let Some(replay) = self.replays.open.as_mut() {
                replay.inputs = inputs;
            }
            return Err(why);
        }
        // Built again, never closed: a page that heard it close would
        // give its selection to the live vehicles (UXD-189).
        self.unload_replay();
        let built = self.start_replay(inputs, Some(kept));
        if built.is_err() {
            self.push_replay(false);
        }
        built
    }

    /// The log's bytes, what it is, its range and who it names.
    fn replay_bytes(&mut self, params: &ReplayOpenParams) -> Result<Opened, ApiError> {
        if let Some(id) = &params.flight_id {
            return self.flight_bytes(id, None, params.ground);
        }
        if let Some(data) = &params.data {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|_| ApiError::new(ErrorKind::InvalidValue, "data is not base64"))?;
            let name = params.name.clone().unwrap_or_else(|| "A flight log".into());
            let mut source = ReplaySource {
                kind: ReplaySourceKind::Tlog,
                name,
                ..ReplaySource::default()
            };
            let mut hint = Hint::default();
            let bytes = as_tlog(bytes, &mut source, &mut hint)?;
            return Ok((bytes, source, None, hint));
        }
        if let Some(file) = &params.vehicle_file {
            return self.vehicle_file_bytes(file);
        }
        // Attached to a message (S246): who may see it is checked first.
        if let Some(id) = &params.attachment {
            let (found, bytes) = self.attached_bytes(None, id)?;
            let mut source = ReplaySource {
                kind: ReplaySourceKind::Tlog,
                name: params.name.clone().unwrap_or(found.name),
                ..ReplaySource::default()
            };
            let mut hint = Hint::default();
            let bytes = as_tlog(bytes, &mut source, &mut hint)?;
            return Ok((bytes, source, None, hint));
        }
        if let Some(path) = &params.path {
            let bytes = self
                .system
                .replay_read(&ReplayRead::File { path: path.clone() })
                .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
            let name = params
                .name
                .clone()
                .unwrap_or_else(|| path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned());
            let mut source = ReplaySource {
                kind: ReplaySourceKind::Tlog,
                name,
                path: Some(path.clone()),
                ..ReplaySource::default()
            };
            let mut hint = Hint::default();
            let bytes = as_tlog(bytes, &mut source, &mut hint)?;
            return Ok((bytes, source, None, hint));
        }
        Err(ApiError::new(
            ErrorKind::InvalidParams,
            "give flightId, path or data",
        ))
    }

    /// A copy of a vehicle's file made in Files (S121), such as a `.BIN`.
    fn vehicle_file_bytes(
        &mut self,
        file: &uxduck_contracts::dto::replay::ReplayVehicleFile,
    ) -> Result<Opened, ApiError> {
        let vehicle_id = self
            .vehicle(&file.vehicle_id)
            .map_or_else(|| file.vehicle_id.clone(), |v| v.id.clone());
        let local = format!("{vehicle_id}/{}", file.path.trim_start_matches('/'));
        let bytes = self
            .system
            .replay_read(&ReplayRead::VehicleFile { local })
            .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
        let name = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_owned();
        let mut source = ReplaySource {
            kind: ReplaySourceKind::Tlog,
            name,
            ..ReplaySource::default()
        };
        // The vehicle whose card it came from (S238).
        let mut hint = Hint {
            vehicle: self
                .vehicle(&vehicle_id)
                .map(|v| (v.id.clone(), Some(v.system_id), v.name.clone())),
            ..Hint::default()
        };
        let bytes = as_tlog(bytes, &mut source, &mut hint)?;
        Ok((bytes, source, None, hint))
    }

    /// The recordings of the replay's vehicles that meet its range (R7):
    /// recorded on this computer's clock, as the tlog is, so they need no
    /// alignment.
    fn replay_recordings(&self, replay: &Replay) -> Vec<ReplayRecording> {
        let mut recordings: Vec<ReplayRecording> = self
            .state
            .video_recordings
            .values()
            .filter_map(|r| {
                let vehicle = replay
                    .vehicles
                    .values()
                    .find(|v| r.vehicle_id.as_deref() == Some(v.original_id.as_str()))?;
                let ended = r
                    .ended_at_us
                    .unwrap_or_else(|| r.started_at_us.saturating_add(r.duration_us));
                (r.started_at_us <= replay.end_us && ended >= replay.start_us).then(|| {
                    ReplayRecording {
                        id: r.id.clone(),
                        source_id: r.source_id.clone(),
                        vehicle_id: Some(vehicle.replay_id.clone()),
                        started_at_us: r.started_at_us,
                        ended_at_us: ended,
                        name: r
                            .alignment
                            .as_ref()
                            .and_then(|_| r.file.as_ref().map(|f| f.name.clone())),
                        aligned_by: r.alignment.as_ref().map(|a| a.by.into()),
                        codec: r.codec.clone(),
                    }
                })
            })
            .collect();
        recordings.sort_by_key(|r| (r.started_at_us, r.id.clone()));
        recordings
    }

    /// `flight/log`: a flight's part of its connection's log as a tlog
    /// file, named after its vehicle and when it armed (UTC).
    pub(crate) fn flight_log(&mut self, id: &str) -> Result<FlightLogResult, ApiError> {
        let (bytes, source, ..) = self.flight_bytes(id, None, false)?;
        let armed_us = self.state.flights.get(id).map_or(0, |f| f.armed_at_us);
        let vehicle: String = source
            .name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let when = crate::param_definitions::saved::date_time(armed_us)
            .replace(' ', "-")
            .replace(':', "");
        let vehicle = if vehicle.is_empty() {
            "flight".into()
        } else {
            vehicle
        };
        Ok(FlightLogResult {
            name: format!("{vehicle}-{when}.tlog"),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    pub(crate) fn flight_replay_read(
        &self,
        flight: &uxduck_core::domain::flight::Flight,
        range: Option<(u64, u64)>,
    ) -> Option<ReplayRead> {
        let ended = flight.ended_at_us.unwrap_or_else(|| self.clock.now_us());
        let (from_us, to_us) = range.unwrap_or((
            flight.armed_at_us.saturating_sub(LEAD_US),
            ended.saturating_add(LEAD_US),
        ));
        Some(ReplayRead::Link {
            link_id: flight.link_id.clone()?,
            from_us,
            to_us,
            start: flight.tlog.as_ref().map(|t| (t.file.clone(), t.offset)),
        })
    }

    /// The recordings deleting the flights `ids` frees (S348): each file of
    /// their links that one of them reaches, from 10 s before it armed to
    /// 10 s after it ended, and that no other flight recorded here reaches,
    /// unless its link is writing it. Also the bytes of the files they reach
    /// that stay.
    pub(crate) fn freed_link_logs(
        &self,
        ids: &BTreeSet<&str>,
    ) -> (Vec<(String, crate::runtime::LinkLog)>, u64) {
        let here: Vec<&uxduck_core::domain::flight::Flight> = self
            .state
            .flights
            .values()
            .filter(|f| f.recorded_here && f.link_id.is_some())
            .collect();
        let links: BTreeSet<&str> = here
            .iter()
            .filter(|f| ids.contains(f.id.as_str()))
            .filter_map(|f| f.link_id.as_deref())
            .collect();
        let mut freed = Vec::new();
        let mut kept = 0;
        for link in links {
            let writing = self.system.tlog_position(link).map(|(name, _)| name);
            let logs = self.system.link_logs(link);
            for (i, log) in logs.iter().enumerate() {
                let to = logs.get(i + 1).map_or(u64::MAX, |next| next.start_us);
                let reaching: Vec<bool> = here
                    .iter()
                    .filter(|f| f.link_id.as_deref() == Some(link))
                    .filter(|f| {
                        let (from, end) = self.flight_span(f, false);
                        from < to && log.start_us <= end
                    })
                    .map(|f| ids.contains(f.id.as_str()))
                    .collect();
                if !reaching.contains(&true) {
                    continue;
                }
                if !reaching.contains(&false) && writing.as_deref() != Some(log.name.as_str()) {
                    freed.push((link.to_owned(), log.clone()));
                } else {
                    kept += log.bytes;
                }
            }
        }
        (freed, kept)
    }

    /// UXDuck's own video recordings deleting the flights `ids` frees
    /// (S348): ended ones in the media folder that one of them reaches, by
    /// the flight or its vehicle over its time as the Logbook counts its
    /// video, and that no other flight recorded here reaches. A video added
    /// from elsewhere is the person's own file and is never among them.
    pub(crate) fn freed_recordings(
        &self,
        ids: &BTreeSet<&str>,
    ) -> Vec<uxduck_core::domain::video::VideoRecording> {
        use uxduck_core::domain::video::RecordingState;
        let reaches = |f: &uxduck_core::domain::flight::Flight,
                       r: &uxduck_core::domain::video::VideoRecording| {
            if r.flight_id.as_deref() == Some(f.id.as_str()) {
                return true;
            }
            let end = f.ended_at_us.unwrap_or_else(|| self.clock.now_us());
            let r_end = r
                .ended_at_us
                .unwrap_or_else(|| r.started_at_us.saturating_add(r.duration_us));
            r.vehicle_id.as_deref() == Some(f.vehicle_id.as_str())
                && r.started_at_us < end
                && f.armed_at_us < r_end
        };
        let here: Vec<&uxduck_core::domain::flight::Flight> = self
            .state
            .flights
            .values()
            .filter(|f| f.recorded_here)
            .collect();
        self.state
            .video_recordings
            .values()
            .filter(|r| r.state != RecordingState::Recording && r.alignment.is_none())
            .filter(|r| {
                let reaching: Vec<bool> = here
                    .iter()
                    .filter(|f| reaches(f, r))
                    .map(|f| ids.contains(f.id.as_str()))
                    .collect();
                reaching.contains(&true) && !reaching.contains(&false)
            })
            .cloned()
            .collect()
    }

    /// The part of its link's log a flight's replay plays: from 10 s before
    /// arming to 10 s after it ended (R3), or with its disarmed time
    /// (ADR-0050 L10) from after the vehicle's previous flight on the link
    /// or the start of its log, to the next flight's lead-in or the log's
    /// end. A log known only by time (UXDuck Web) is taken as the half hour
    /// either side.
    fn flight_span(
        &self,
        flight: &uxduck_core::domain::flight::Flight,
        ground: bool,
    ) -> (u64, u64) {
        let ended = flight.ended_at_us.unwrap_or_else(|| self.clock.now_us());
        let from = flight.armed_at_us.saturating_sub(LEAD_US);
        let to = ended.saturating_add(LEAD_US);
        if !ground {
            return (from, to);
        }
        let others: Vec<_> = self
            .state
            .flights
            .values()
            .filter(|f| {
                f.id != flight.id
                    && f.vehicle_id == flight.vehicle_id
                    && f.link_id == flight.link_id
            })
            .collect();
        let after_previous = others
            .iter()
            .filter_map(|f| f.ended_at_us)
            .filter(|e| *e <= flight.armed_at_us)
            .max()
            .map(|e| e.saturating_add(LEAD_US));
        let next_lead_in = others
            .iter()
            .map(|f| f.armed_at_us)
            .filter(|a| *a >= ended)
            .min()
            .map(|a| a.saturating_sub(LEAD_US));
        let (start, end) = match flight.tlog.as_ref().and_then(|t| log_start_us(&t.file)) {
            Some(start) => (start, u64::MAX),
            None => (
                from.saturating_sub(STATE_BEFORE_US),
                to.saturating_add(STATE_BEFORE_US),
            ),
        };
        (
            after_previous.map_or(start, |a| a.max(start)).min(from),
            next_lead_in.map_or(end, |n| n.min(end)).max(to),
        )
    }

    /// A flight's part of its connection's log (R3), with its time on the
    /// ground (`ground`, ADR-0050 L10), or `range` of it.
    fn flight_bytes(
        &mut self,
        id: &str,
        range: Option<(u64, u64)>,
        ground: bool,
    ) -> Result<Opened, ApiError> {
        let flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        let Some(link_id) = flight.link_id.clone() else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "This flight's log is not on this computer.",
            ));
        };
        let (from_us, to_us) = range.unwrap_or_else(|| self.flight_span(&flight, ground));
        // On the ground, its log is the file it is in, from its start: read
        // it whole rather than from just before the flight.
        let read = match &flight.tlog {
            Some(tlog) if ground => ReplayRead::LinkFile {
                link_id: link_id.clone(),
                file: tlog.file.clone(),
                from_us,
                to_us,
            },
            tlog => ReplayRead::Link {
                link_id: link_id.clone(),
                from_us,
                to_us,
                start: tlog.as_ref().map(|t| (t.file.clone(), t.offset)),
            },
        };
        let bytes = self
            .system
            .replay_read(&read)
            .map_err(|why| ApiError::new(ErrorKind::NotFound, why))?;
        let system = self
            .state
            .vehicles
            .get(&flight.vehicle_id)
            .map(|v| v.system_id);
        // Its name when it flew, even once its record is gone.
        let hint = Hint {
            vehicle: Some((
                flight.vehicle_id.clone(),
                system,
                flight.vehicle_name.clone(),
            )),
            link_id: Some(link_id),
            ..Hint::default()
        };
        let source = ReplaySource {
            kind: ReplaySourceKind::Flight,
            name: flight.vehicle_name.clone(),
            flight_id: Some(flight.id.clone()),
            vehicle_id: Some(flight.vehicle_id.clone()),
            ..ReplaySource::default()
        };
        Ok((bytes, source, Some((from_us, to_us)), hint))
    }

    /// The recorded-state frames of a flight's link's log before `before_us`
    /// (ADR-0050 L7): read back from where the flight began in its file, at
    /// most [`STATE_BEFORE_BYTES`]. Empty when there is nothing there or
    /// it cannot be read: the replay then has what its own part holds.
    fn state_before(&mut self, flight_id: &str, before_us: u64) -> Vec<u8> {
        let Some(flight) = self.state.flights.get(flight_id).cloned() else {
            return Vec::new();
        };
        let Some(link_id) = flight.link_id.as_deref() else {
            return Vec::new();
        };
        // Where the flight began in its file, or else (UXDuck Web, a flight
        // that does not know it) the half hour before it.
        let read = flight.tlog.as_ref().map_or_else(
            || ReplayRead::Link {
                link_id: link_id.to_owned(),
                from_us: before_us.saturating_sub(STATE_BEFORE_US),
                to_us: before_us,
                start: None,
            },
            |start| ReplayRead::LinkBefore {
                link_id: link_id.to_owned(),
                file: start.file.clone(),
                offset: start.offset,
                bytes: STATE_BEFORE_BYTES,
            },
        );
        let Ok(bytes) = self.system.replay_read(&read) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (stamp, frame) in uxduck_mavlink::tlog::read(&bytes) {
            if stamp < before_us
                && uxduck_mavlink::framing::header(frame).is_some_and(|h| {
                    PARAM_MESSAGES.contains(&h.message_id)
                        || mission_log::MESSAGE_IDS.contains(&h.message_id)
                })
            {
                let _ = uxduck_mavlink::tlog::record(&mut out, stamp, frame);
            }
        }
        out
    }

    /// Who a system id in the log was: the flight's vehicle (or the one a
    /// vehicle file came from), a vehicle
    /// known on its link, or a vehicle known by the id the log gave it,
    /// a UID, unless it is a simulator's, which every simulator of a build
    /// shares (S238). None of these: the first free callsign, avoiding
    /// `taken`, the names this replay gave already.
    fn identify(
        &self,
        system: u8,
        (session_id, own_id): (&str, &str),
        hint: &Hint,
        (simulator, only): (bool, bool),
        taken: &[String],
    ) -> Identity {
        // The hinted vehicle by its system id, or as the source's one
        // vehicle (a `.BIN` names no system id of its own).
        let known = hint
            .vehicle
            .as_ref()
            .filter(|(_, s, _)| *s == Some(system) || only)
            .map(|(id, _, name)| (id.clone(), name.clone()))
            .or_else(|| {
                let link = hint.link_id.as_deref()?;
                self.state
                    .vehicles
                    .values()
                    .find(|v| !v.replay && v.link_id == link && v.system_id == system)
                    .map(|v| (v.id.clone(), v.name.clone()))
            })
            .or_else(|| {
                self.vehicle(session_id)
                    .filter(|v| !v.replay && !simulator)
                    .map(|v| (v.id.clone(), v.name.clone()))
            });
        let (original_id, name) = if let Some((id, name)) = known {
            // Its name now, if it still has one here.
            let name = self
                .state
                .vehicles
                .get(&id)
                .map_or(name, |v| v.name.clone());
            (id, name)
        } else {
            // Never the file's name or a system id (ADR-0004, 2026-10-10),
            // and a simulator never a callsign (maintainer, 2026-11-01);
            // the name it had when this log was last open, renamed or not.
            let name = self
                .state
                .vehicles
                .get(&replay_vid(own_id))
                .filter(|v| v.replay && v.replay_of.is_none())
                .map_or_else(|| self.replay_name(simulator, taken), |v| v.name.clone());
            (own_id.to_owned(), name)
        };
        Identity {
            replay_id: replay_vid(&original_id),
            original_id,
            // Its original's name: `replay: true` says what it is (S238).
            name,
            copy_at: None,
            copy_index: 0,
        }
    }

    /// A vehicle record for a real board heard only in a log: offline,
    /// "Seen in a log" until it is heard live (S248).
    fn remember_log_vehicle(&mut self, seen: &Observation, name: &str) {
        let vehicle = Vehicle {
            connections: None,
            id: seen.id.clone(),
            name: name.to_owned(),
            aliases: Vec::new(),
            link_id: String::new(),
            peer: String::new(),
            system_id: seen.system_id,
            component_ids: seen.component_ids.clone(),
            autopilot: seen.autopilot.clone(),
            vehicle_type: seen.vehicle_type.clone(),
            uid: seen.uid.clone(),
            firmware: seen.firmware.clone(),
            firmware_commit: seen.firmware_commit.clone(),
            presence: Presence::Offline,
            last_heartbeat_us: 0,
            armed: false,
            mode: None,
            saved: true,
            dismissed: false,
            hidden: false,
            seen_in_log: true,
            colour: Some(self.given_colour(&seen.id)),
            local_only: false,
            labels: Vec::new(),
            bookmarks: Vec::new(),
            size_cm: None,
            gripper: None,
            companions: Vec::new(),
            traffic: false,
            look: None,
            own_model: None,
            preflight: uxduck_core::domain::preflight::PreflightSettings::default(),
            definitions: uxduck_core::domain::param_definitions::DefinitionsChoice::default(),
            dialects: uxduck_core::domain::dialect::DialectChoice::default(),
            replay: false,
            replay_of: None,
            replay_at_us: None,
            sim_evidence: false,
            simulated: None,
            version: 1,
        };
        self.commit_as(
            crate::vehicles::data_plane(),
            Change::VehicleUpserted { vehicle },
        );
    }

    /// The first name no vehicle here goes by or went by, nor one of
    /// `taken`: "Sim n" for a simulator, else a callsign.
    fn replay_name(&self, simulator: bool, taken: &[String]) -> String {
        let names = self
            .state
            .vehicles
            .values()
            .filter(|v| !v.replay)
            .map(|v| v.name.as_str())
            .chain(self.state.retired_callsigns.keys().map(String::as_str))
            .chain(taken.iter().map(String::as_str));
        if simulator {
            uxduck_core::domain::vehicle::simulation_name(None, names)
        } else {
            uxduck_core::domain::vehicle::callsign(names)
        }
    }

    /// Moves the clock on to `t`, feeding the records up to it. `held`
    /// gathers the effects and applies them once, as after a rebuild.
    fn advance_replay(&mut self, t: u64, held: bool) {
        let Some(mut replay) = self.replays.open.take() else {
            return;
        };
        replay.t_us = t;
        // Fed from the range's start (opened, or rebuilt): the batch is the
        // whole prefix. A long jump on is held too, but continues it.
        let whole = replay.next == 0;
        let effects = replay.feed(t);
        self.replays.now_us = Some(t);
        self.apply_replay(&mut replay, effects, held, whole);
        self.replays.now_us = None;
        // A vehicle whose sources have all ended is gone, as one not yet
        // heard is absent (R17).
        let ended: Vec<String> = replay
            .vehicles
            .values()
            .map(|v| v.replay_id.clone())
            .filter(|id| {
                replay
                    .vehicles
                    .iter()
                    .filter(|(_, v)| v.replay_id == *id)
                    .all(|(key, _)| {
                        replay
                            .sources
                            .get(usize::from(key.0))
                            .is_some_and(|s| t > s.end_us)
                    })
            })
            .collect();
        self.replays.open = Some(replay);
        if !ended.is_empty()
            && self
                .replays
                .open
                .as_ref()
                .is_some_and(|r| r.sources.len() > 1)
        {
            self.remove_replay_vehicles(&ended);
        }
    }

    /// The session's effects for replay vehicles: observations last of
    /// each, telemetry merged, messages in order; the rest goes nowhere.
    fn apply_replay(&mut self, replay: &mut Replay, effects: Vec<Effect>, held: bool, whole: bool) {
        let mut seen: BTreeMap<Key, Observation> = BTreeMap::new();
        let mut telemetry: BTreeMap<Key, VehicleTelemetry> = BTreeMap::new();
        let mut texts = Vec::new();
        let mut tuning: BTreeMap<Key, Vec<uxduck_core::domain::tuning_response::Sample>> =
            BTreeMap::new();
        // Who each session vehicle is, first: the session may say a
        // vehicle's first values before the observation naming it, as at
        // the replay's first moment.
        for effect in &effects {
            if let Effect::Vehicle(observation) = effect {
                replay.remember(observation);
            }
        }
        for effect in effects {
            match effect {
                Effect::Vehicle(observation) => {
                    let key = (source_of(&observation.link_id), observation.system_id);
                    seen.insert(key, *observation);
                }
                Effect::Telemetry(update) => {
                    if let Some(system) = replay.system_of(&update.vehicle_id) {
                        telemetry.entry(system).or_default().merge(&update);
                    }
                }
                Effect::TuningSamples {
                    vehicle_id,
                    samples,
                } => {
                    if let Some(system) = replay.system_of(&vehicle_id) {
                        tuning.entry(system).or_default().extend(samples);
                    }
                }
                Effect::StatusText {
                    vehicle_id,
                    component_id,
                    severity,
                    text,
                } => {
                    if let Some(system) = replay.system_of(&vehicle_id) {
                        texts.push((system, component_id, severity, text));
                    }
                }
                _ => {}
            }
        }
        for (system, observation) in seen {
            if let Some(identity) = replay.vehicles.get(&system).cloned() {
                self.replay_vehicle_seen(&identity, &observation);
            }
        }
        self.apply_replay_parameters(replay, whole);
        self.apply_replay_missions(replay);
        for (system, mut update) in telemetry {
            let Some(identity) = replay.vehicles.get(&system) else {
                continue;
            };
            if !self.state.vehicles.contains_key(&identity.replay_id) {
                continue;
            }
            update.vehicle_id.clone_from(&identity.replay_id);
            if held {
                self.telemetry.remove(&identity.replay_id);
            }
            self.telemetry_observed(update);
        }
        // The response graph's samples (S330): a move starts them afresh.
        for (system, identity) in &replay.vehicles {
            if !self.state.vehicles.contains_key(&identity.replay_id) {
                continue;
            }
            if held {
                self.forget_tuning_samples(&identity.replay_id);
            }
            if let Some(samples) = tuning.remove(system) {
                self.tuning_samples(&identity.replay_id, samples);
                if let Some(timeline) = replay.params.get(system).filter(|_| held) {
                    self.replay_tuning_marks(&identity.replay_id, timeline);
                }
            }
        }
        for (system, component_id, severity, text) in texts {
            if let Some(identity) = replay.vehicles.get(&system) {
                let id = identity.replay_id.clone();
                self.vehicle_message(&id, component_id, severity, &text);
            }
        }
    }

    /// Each replay vehicle's parameters at the replay's time (ADR-0050
    /// L5), from its timeline: the latest value read by then, or else the
    /// first safe later reading. Previous-boot values are withheld.
    /// Replaced on a reading, a boot boundary, or a rebuild (`whole`).
    fn apply_replay_parameters(&mut self, replay: &mut Replay, whole: bool) {
        let t = self.replays.now_us.unwrap_or(replay.t_us);
        for (key, timeline) in &replay.params {
            let Some(identity) = replay.vehicles.get(key) else {
                continue;
            };
            let id = identity.replay_id.clone();
            if !self.state.vehicles.contains_key(&id) {
                replay.params_applied.remove(key);
                continue;
            }
            let read = timeline.read_by(t);
            if !whole && replay.params_applied.get(key) == Some(&read) {
                continue;
            }
            replay.params_applied.insert(*key, read);
            let parameters = timeline
                .at(t)
                .into_iter()
                .filter(|at| at.known != Known::BeforeBoot)
                .map(|at| Parameter {
                    vehicle_id: id.clone(),
                    name: at.name,
                    value: f64::from(at.reading.value),
                    kind: ParamKind::from_mav(at.reading.kind),
                    default: at.reading.default.map(f64::from),
                })
                .collect();
            self.replay_parameters(&id, parameters, true);
        }
    }

    /// What the open replay's log says of a replay vehicle's parameters:
    /// its timeline and the replay's time.
    pub(crate) fn replay_timeline(&self, vehicle_id: &str) -> Option<(&Timeline, u64)> {
        let replay = self.replays.open.as_ref()?;
        let (key, _) = replay
            .vehicles
            .iter()
            .find(|(_, v)| v.replay_id == vehicle_id)?;
        Some((replay.params.get(key)?, replay.t_us))
    }

    /// A replay vehicle as the session sees it now.
    fn replay_vehicle_seen(&mut self, identity: &Identity, seen: &Observation) {
        let previous = self.state.vehicles.get(&identity.replay_id).cloned();
        let mut vehicle = previous.clone().unwrap_or_else(|| Vehicle {
            connections: None,
            id: identity.replay_id.clone(),
            name: identity.name.clone(),
            aliases: Vec::new(),
            link_id: REPLAY_LINK.into(),
            peer: PEER.into(),
            system_id: seen.system_id,
            component_ids: Vec::new(),
            autopilot: String::new(),
            vehicle_type: String::new(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 0,
            armed: false,
            mode: None,
            saved: false,
            dismissed: false,
            hidden: false,
            seen_in_log: false,
            local_only: true,
            labels: Vec::new(),
            bookmarks: Vec::new(),
            size_cm: None,
            gripper: None,
            companions: Vec::new(),
            traffic: false,
            look: None,
            own_model: None,
            colour: None,
            preflight: uxduck_core::domain::preflight::PreflightSettings::default(),
            definitions: uxduck_core::domain::param_definitions::DefinitionsChoice::default(),
            dialects: uxduck_core::domain::dialect::DialectChoice::default(),
            replay: true,
            replay_of: None,
            replay_at_us: None,
            sim_evidence: false,
            simulated: None,
            version: 0,
        });
        // A view of its original (S238): its name, look and size as they
        // are now. With none here, the name it was given in this replay.
        let original = self
            .state
            .vehicles
            .get(&identity.original_id)
            .filter(|live| !live.replay);
        vehicle.replay_at_us = identity.copy_at;
        if let Some(live) = original {
            follow(&mut vehicle, live, identity.copy_index);
            // A simulation's replay is marked simulated, as it was.
            vehicle.simulated = Some(self.is_simulated(live));
        } else {
            vehicle.replay_of = None;
            vehicle.name.clone_from(&identity.name);
        }
        if vehicle.colour.is_none() {
            vehicle.colour = Some(self.given_colour(&vehicle.id));
        }
        vehicle.component_ids.clone_from(&seen.component_ids);
        vehicle.autopilot.clone_from(&seen.autopilot);
        vehicle.vehicle_type.clone_from(&seen.vehicle_type);
        vehicle.uid = seen.uid.clone().or(vehicle.uid);
        vehicle.firmware = seen.firmware.clone().or(vehicle.firmware);
        vehicle.firmware_commit = seen.firmware_commit.clone().or(vehicle.firmware_commit);
        // Present while the clock is in its range: never an offline alarm.
        vehicle.presence = Presence::Online;
        vehicle.armed = seen.armed;
        vehicle.mode.clone_from(&seen.mode);
        let unchanged = previous.as_ref().is_some_and(|p| {
            let mut same = vehicle.clone();
            same.version = p.version;
            same.last_heartbeat_us = p.last_heartbeat_us;
            &same == p
        });
        if unchanged {
            return;
        }
        vehicle.last_heartbeat_us = seen.last_heartbeat_us;
        vehicle.version = previous.map_or(1, |p| p.version + 1);
        self.commit_as(actor(), Change::VehicleUpserted { vehicle });
    }

    /// Restore one vehicle's recorded values, once per batch. On a rebuild
    /// the batch is the entire prefix; a future value absent there is removed.
    fn replay_parameters(&mut self, id: &str, parameters: Vec<Parameter>, rebuilt: bool) {
        let old: BTreeMap<String, Parameter> = self
            .state
            .parameters
            .values()
            .filter(|p| p.vehicle_id == id)
            .map(|p| (p.name.clone(), p.clone()))
            .collect();
        let mut next = if rebuilt {
            BTreeMap::new()
        } else {
            old.clone()
        };
        for parameter in parameters {
            next.insert(parameter.name.clone(), parameter);
        }
        let removed: Vec<String> = old
            .keys()
            .filter(|name| !next.contains_key(*name))
            .cloned()
            .collect();
        let changed: Vec<Parameter> = next
            .values()
            .filter(|p| old.get(&p.name) != Some(*p))
            .cloned()
            .collect();
        if changed.is_empty() && removed.is_empty() {
            return;
        }
        let mut set = self.parameter_set(id);
        set.count = u32::try_from(next.len()).unwrap_or(u32::MAX);
        self.commit_as(
            actor(),
            Change::ParametersReplaced {
                set,
                parameters: changed,
                removed,
            },
        );
    }

    fn apply_replay_missions(&mut self, replay: &mut Replay) {
        let mut owners: BTreeMap<&str, Option<Key>> = BTreeMap::new();
        for (key, identity) in &replay.vehicles {
            if !self.state.vehicles.contains_key(&identity.replay_id) {
                continue;
            }
            let owner = owners.entry(&identity.replay_id).or_default();
            if let Some((start, end)) = span(&replay.sources, key.0)
                && start <= replay.t_us
                && replay.t_us <= end
                && owner.is_none_or(|previous| {
                    span(&replay.sources, previous.0).is_none_or(|(old, _)| start >= old)
                })
            {
                *owner = Some(*key);
            }
        }
        for (id, owner) in owners {
            let input = owner.and_then(|key| replay.inputs.get(usize::from(key.0)));
            let dataflash = input.is_some_and(|input| input.hint.log.is_some());
            let reading = owner
                .and_then(|key| replay.missions.get(&key))
                .and_then(|timeline| timeline.at(replay.t_us));
            let index = reading.map(|(index, _)| index);
            let applied = (owner, index);
            if replay.missions_applied.get(id) == Some(&applied)
                && self.state.missions.contains_key(id)
            {
                continue;
            }
            let reading = reading.map(|(_, reading)| reading);
            let items = if dataflash {
                input
                    .map(|input| input.hint.mission.as_slice())
                    .filter(|items| !items.is_empty())
            } else {
                reading.and_then(|r| r.items.as_deref())
            };
            if dataflash && items.is_none() && !self.state.missions.contains_key(id) {
                replay.missions_applied.insert(id.into(), applied);
                continue;
            }
            let mut mission = self.mission(id);
            mission.items = items
                .map(|items| items.iter().map(crate::plane::from_mavlink).collect())
                .unwrap_or_default();
            mission.fence.clear();
            mission.rally.clear();
            mission.progress = None;
            mission.boot_instant_us = None;
            mission.status = if items.is_some() {
                uxduck_core::domain::parameter::TransferStatus::Synced
            } else {
                uxduck_core::domain::parameter::TransferStatus::Unknown
            };
            mission.detail = if dataflash && items.is_some() {
                Some("Static mission from the whole DataFlash log. Not time-indexed.".into())
            } else {
                Some(reading.map_or("No complete vehicle mission readback was recorded for this source by this time.", |r| r.detail).into())
            };
            mission.synced_at_us = reading.filter(|r| r.items.is_some()).map(|r| r.at_us);
            mission.version = mission.version.saturating_add(1);
            self.commit_as(actor(), Change::MissionUpserted { mission });
            replay.missions_applied.insert(id.into(), applied);
        }
    }

    /// Close and startup cleanup also remove values from subscribed clients.
    fn clear_replay_parameters(&mut self, id: &str) {
        self.replay_parameters(id, Vec::new(), true);
    }

    /// Removes the replay's vehicles and what the engine holds of them.
    fn remove_replay_vehicles(&mut self, ids: &[String]) {
        for id in ids {
            self.clear_replay_parameters(id);
            self.telemetry.remove(id);
            self.messages.forget_vehicle(id);
            if self.state.vehicles.contains_key(id) {
                self.commit_as(actor(), Change::VehicleRemoved { id: id.clone() });
            }
        }
    }

    /// Back to the range's start and on to `t` (R4): a new session, the
    /// effects held and applied once, the generation raised.
    fn rebuild_replay(&mut self, t: u64) {
        let Some(replay) = self.replays.open.as_mut() else {
            return;
        };
        replay.session = Session::for_replay(Config::default());
        replay.session_ids.clear();
        replay.next = 0;
        replay.last_tick = 0;
        replay.generation += 1;
        let ids: Vec<String> = replay
            .vehicles
            .values()
            .map(|v| v.replay_id.clone())
            .collect();
        replay.t_us = t;
        for id in &ids {
            self.messages.forget_vehicle(id);
        }
        // The new generation first: a client clears what it gathered, then
        // takes what the rebuild gives.
        self.push_replay(false);
        self.advance_replay(t, true);
        // Vehicles not heard yet at T go, as a vehicle not yet heard is absent.
        let heard: BTreeSet<Key> = self
            .replays
            .open
            .as_ref()
            .map(|r| r.session_ids.values().copied().collect())
            .unwrap_or_default();
        let gone: BTreeSet<String> = self
            .replays
            .open
            .as_ref()
            .map(|r| {
                r.vehicles
                    .iter()
                    .filter(|(_, vehicle)| {
                        !r.vehicles.iter().any(|(system, other)| {
                            other.replay_id == vehicle.replay_id && heard.contains(system)
                        })
                    })
                    .map(|(_, v)| v.replay_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.remove_replay_vehicles(&gone.into_iter().collect::<Vec<_>>());
    }

    /// Moves the clock to `t`: on by feeding, back by rebuilding.
    pub(crate) fn seek_replay(&mut self, t: u64) {
        let now = self.clock.now_us();
        let Some(replay) = self.replays.open.as_mut() else {
            return;
        };
        let t = t.clamp(replay.start_us, replay.end_us);
        let back = t < replay.t_us;
        if replay.playing {
            replay.anchor = (now, t);
        }
        if back {
            self.rebuild_replay(t);
        } else {
            // A long jump on is held as a rebuild is.
            let held = t.saturating_sub(replay.t_us) > 5_000_000;
            self.advance_replay(t, held);
        }
        self.push_replay(false);
    }

    fn play_replay(&mut self, playing: bool) {
        let now = self.clock.now_us();
        let Some(replay) = self.replays.open.as_mut() else {
            return;
        };
        // At the end there is nothing to play on: Restart goes back to the
        // start, Play never does by itself (maintainer, 2026-10-29).
        if replay.playing == playing || (playing && replay.t_us >= replay.end_us) {
            return;
        }
        replay.t_us = replay.running(now);
        replay.playing = playing;
        replay.anchor = (now, replay.t_us);
        self.system.replay_clock(playing);
        self.push_replay(false);
    }

    /// `replay/restart`: from the range's start again, rebuilt (R4), playing.
    fn restart_replay(&mut self) {
        let Some(replay) = self.replays.open.as_mut() else {
            return;
        };
        let start = replay.start_us;
        if !replay.playing {
            replay.playing = true;
            self.system.replay_clock(true);
        }
        // Anchors the playing clock at the start and rebuilds.
        self.seek_replay(start);
    }

    fn speed_replay(&mut self, speed: f64) -> Result<(), ApiError> {
        if !speed.is_finite() || !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "speed is 0.1 to 16 times real time",
            ));
        }
        let now = self.clock.now_us();
        if let Some(replay) = self.replays.open.as_mut() {
            replay.t_us = replay.running(now);
            replay.anchor = (now, replay.t_us);
            replay.speed = speed;
        }
        self.push_replay(false);
        Ok(())
    }

    /// Closes the replay: its vehicles go and live is shown.
    pub(crate) fn close_replay(&mut self) {
        if self.unload_replay() {
            self.push_replay(false);
        }
    }

    /// The replay open now unloaded, its vehicles gone and its clock
    /// stopped, without saying so: false when none was open.
    fn unload_replay(&mut self) -> bool {
        let Some(replay) = self.replays.open.take() else {
            return false;
        };
        let ids: Vec<String> = replay
            .vehicles
            .values()
            .map(|v| v.replay_id.clone())
            .collect();
        self.remove_replay_vehicles(&ids);
        if replay.playing {
            self.system.replay_clock(false);
        }
        true
    }

    /// The clock's step, from the host's replay timer while it plays: the
    /// records up to the clock are fed, and the state pushed four times a
    /// second. Nothing while nothing plays.
    pub fn replay_tick(&mut self, now_us: u64) {
        let Some(replay) = self.replays.open.as_mut() else {
            return;
        };
        if !replay.playing {
            return;
        }
        let t = replay.running(now_us);
        let ended = t >= replay.end_us;
        if ended {
            replay.playing = false;
            replay.anchor = (now_us, t);
        }
        self.advance_replay(t, false);
        if ended {
            self.system.replay_clock(false);
            self.push_replay(false);
        } else if now_us.saturating_sub(self.replays.last_push_us) >= PUSH_EVERY_US {
            self.push_replay(false);
        }
    }

    /// The replay vehicle a request names, else the focused one: its key.
    fn replay_vehicle_key(
        replay: &Replay,
        vehicle_id: Option<&String>,
    ) -> Result<Option<Key>, ApiError> {
        match vehicle_id {
            Some(id) => replay
                .vehicles
                .iter()
                .find(|(_, v)| v.replay_id == *id || v.original_id == *id)
                .map(|(s, _)| Some(*s))
                .ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, format!("no replay vehicle {id}"))
                }),
            None => Ok(replay.focus),
        }
    }

    /// A replay vehicle's DataFlash fields as logged (R15): `df.` keys,
    /// on the replay's clock and within its source's range.
    fn log_series(
        replay: &Replay,
        system: Option<Key>,
        keys: &[String],
        max: usize,
    ) -> Vec<FlightSeries> {
        let index = usize::from(system.map_or(0, |s| s.0));
        let (Some(log), Some(source)) = (
            replay.inputs.get(index).and_then(|i| i.hint.log.as_ref()),
            replay.sources.get(index),
        ) else {
            return Vec::new();
        };
        let mut logged = uxduck_mavlink::dataflash::series(log, keys);
        keys.iter()
            .filter_map(|key| {
                let samples: Vec<FlightSample> = logged
                    .remove(key)?
                    .into_iter()
                    .map(|(stamp, value)| FlightSample {
                        time_us: u64::try_from(
                            (i128::from(stamp) + i128::from(source.offset_us)).max(0),
                        )
                        .unwrap_or(0),
                        value,
                    })
                    .filter(|s| (source.start_us..=source.end_us).contains(&s.time_us))
                    .collect();
                if samples.is_empty() {
                    return None;
                }
                let min = samples
                    .iter()
                    .min_by(|a, b| a.value.total_cmp(&b.value))
                    .cloned();
                let max_sample = samples
                    .iter()
                    .max_by(|a, b| a.value.total_cmp(&b.value))
                    .cloned();
                Some(FlightSeries {
                    key: key.clone(),
                    samples: thin_extremes(samples, max),
                    min,
                    max: max_sample,
                })
            })
            .collect()
    }

    /// `replay/logFields` (R15): what a replay vehicle's DataFlash log
    /// holds, so the agent says "not logged" rather than guessing.
    fn replay_log_fields(
        &self,
        params: &ReplayLogFieldsParams,
    ) -> Result<ReplayLogFieldsResult, ApiError> {
        let replay = self
            .replays
            .open
            .as_ref()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "No replay is open."))?;
        let system = Self::replay_vehicle_key(replay, params.vehicle_id.as_ref())?;
        let input = replay.inputs.get(usize::from(system.map_or(0, |s| s.0)));
        let Some((input, log)) = input.and_then(|i| i.hint.log.as_ref().map(|log| (i, log))) else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "This is a telemetry log (.tlog): it has what the vehicle sent, not the \
                 autopilot's own fields. Open the flight's autopilot log (.bin) for those.",
            ));
        };
        Ok(ReplayLogFieldsResult {
            name: input.source.name.clone(),
            messages: uxduck_mavlink::dataflash::messages(log)
                .into_iter()
                .map(|m| LogMessage {
                    name: m.name,
                    columns: m
                        .columns
                        .into_iter()
                        .map(|c| LogColumn {
                            name: c.name,
                            unit: c.unit,
                        })
                        .collect(),
                    instances: m
                        .instances
                        .into_iter()
                        .filter_map(|i| u32::try_from(i).ok())
                        .collect(),
                    records: u64::try_from(m.records).unwrap_or(u64::MAX),
                    rate_hz: m.rate_hz,
                })
                .collect(),
        })
    }

    /// `replay/motion`: each replay vehicle's positions as logged from
    /// `fromUs` to `toUs`, with the one before and the one after, so the
    /// maps draw it where the log has it at any moment.
    fn replay_motion(&self, params: &ReplayMotionParams) -> Result<ReplayMotionResult, ApiError> {
        let replay = self
            .replays
            .open
            .as_ref()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "No replay is open."))?;
        if params.to_us < params.from_us {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "toUs is before fromUs.",
            ));
        }
        let to_us = params
            .to_us
            .min(params.from_us.saturating_add(MOTION_WINDOW_US));
        let vehicles = replay
            .motion
            .iter()
            .filter_map(|(vehicle_id, samples)| {
                let from = samples
                    .partition_point(|s| s.time_us < params.from_us)
                    .saturating_sub(1);
                let to = (samples.partition_point(|s| s.time_us <= to_us) + 1).min(samples.len());
                (from < to).then(|| ReplayMotion {
                    vehicle_id: vehicle_id.clone(),
                    samples: samples[from..to].to_vec(),
                })
            })
            .collect();
        Ok(ReplayMotionResult {
            generation: replay.generation,
            from_us: params.from_us,
            to_us,
            vehicles,
        })
    }

    /// `flight/series` (R13): a flight's or a replay vehicle's values over
    /// the whole range, from its log, without opening a replay.
    fn flight_series(
        &mut self,
        params: &FlightSeriesParams,
    ) -> Result<FlightSeriesResult, ApiError> {
        if params.keys.is_empty() || params.keys.len() > MAX_SERIES {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("give 1 to {MAX_SERIES} keys"),
            ));
        }
        let max = params
            .max_points
            .map_or(1_000, |n| usize::try_from(n).unwrap_or(MAX_POINTS))
            .clamp(1, MAX_POINTS);
        let (series, start_us, end_us) = if let Some(flight_id) = &params.flight_id {
            let (bytes, _, range, hint) = self.replay_bytes(&ReplayOpenParams {
                flight_id: Some(flight_id.clone()),
                ..ReplayOpenParams::default()
            })?;
            let records = index(&bytes);
            let system = hint.vehicle.as_ref().and_then(|v| v.1).map(|s| (0, s));
            let (start, end) = range.unwrap_or_default();
            (
                series_of(&bytes, &records, system, &params.keys, max),
                start,
                end,
            )
        } else {
            let replay = self.replays.open.as_ref().ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    "give flightId, or open a replay and give one of its vehicles",
                )
            })?;
            let system = Self::replay_vehicle_key(replay, params.vehicle_id.as_ref())?;
            let (logged, telemetry): (Vec<String>, Vec<String>) = params
                .keys
                .iter()
                .cloned()
                .partition(|k| k.starts_with("df."));
            let mut series = Self::log_series(replay, system, &logged, max);
            if !telemetry.is_empty() {
                series.extend(series_of(
                    &replay.bytes,
                    &replay.records,
                    system,
                    &telemetry,
                    max,
                ));
            }
            (series, replay.start_us, replay.end_us)
        };
        let missing_keys = params
            .keys
            .iter()
            .filter(|k| !series.iter().any(|s| s.key == **k))
            .cloned()
            .collect();
        Ok(FlightSeriesResult {
            start_us,
            end_us,
            series,
            missing_keys,
        })
    }

    /// `replay/save` (R11): the flights of an opened log kept for the
    /// account, each with its part of the log, sent as one flight's log is.
    #[allow(
        clippy::too_many_lines,
        reason = "each flight of the log, kept with its part"
    )]
    fn save_replay(&mut self) -> Result<(), ApiError> {
        let Some(replay) = self.replays.open.as_ref() else {
            return Err(ApiError::new(ErrorKind::NotFound, "No replay is open."));
        };
        if replay.source.kind == ReplaySourceKind::Flight {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "This flight is this computer's: save it from Flights.",
            ));
        }
        if replay.clock == ReplayClock::Boot {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "This log has no times to file its flights by.",
            ));
        }
        if !replay.saved.is_empty() {
            return Ok(());
        }
        // Each vehicle's armed spans, from the marks.
        let mut flights = Vec::new();
        let mut seen = BTreeSet::new();
        for identity in replay.vehicles.values() {
            if !seen.insert(&identity.replay_id) {
                continue;
            }
            let marks: Vec<&ReplayMark> = replay
                .marks
                .iter()
                .filter(|m| m.vehicle_id == identity.replay_id)
                .collect();
            let mut armed_at = None;
            for mark in &marks {
                match mark.kind {
                    ReplayMarkKind::Armed => armed_at = Some(mark.at_us),
                    ReplayMarkKind::Disarmed => {
                        if let Some(from) = armed_at.take() {
                            flights.push((identity.clone(), from, mark.at_us));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(from) = armed_at {
                flights.push((identity.clone(), from, replay.end_us));
            }
        }
        if flights.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "No vehicle armed in this log, so it holds no flights to save.",
            ));
        }
        let mut saved = Vec::new();
        let mut logs = Vec::new();
        for (identity, from, to) in flights {
            let key = replay
                .vehicles
                .iter()
                .filter(|(key, found)| {
                    found.replay_id == identity.replay_id
                        && span(&replay.sources, key.0)
                            .is_some_and(|(start, end)| start <= from && to <= end)
                })
                .max_by_key(|(key, _)| span(&replay.sources, key.0).map(|(start, _)| start))
                .map(|(key, _)| *key)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::InvalidValue,
                        "This flight crosses recordings. Open one source to save its flight without mixing recorded state.",
                    )
                })?;
            let marks: Vec<ReplayMark> = replay
                .marks
                .iter()
                .filter(|m| m.vehicle_id == identity.replay_id)
                .cloned()
                .collect();
            let air_us = marks
                .iter()
                .filter(|m| m.kind == ReplayMarkKind::Air)
                .map(|m| {
                    let until = m.until_us.unwrap_or(m.at_us).min(to);
                    until.saturating_sub(m.at_us.max(from))
                })
                .sum();
            let mut modes: Vec<String> = Vec::new();
            for mark in marks.iter().filter(|m| {
                m.kind == ReplayMarkKind::Mode
                    && m.at_us < to
                    && m.until_us.unwrap_or(m.at_us) > from
            }) {
                if let Some(mode) = &mark.text
                    && !modes.contains(mode)
                {
                    modes.push(mode.clone());
                }
            }
            let vehicle = self.state.vehicles.get(&identity.replay_id);
            let id = format!("fl{}", &self.random.token()[..16]);
            let flight = uxduck_core::domain::flight::Flight {
                id: id.clone(),
                vehicle_id: identity.original_id.clone(),
                vehicle_name: identity.name.clone(),
                vehicle_type: vehicle.map(|v| v.vehicle_type.clone()).unwrap_or_default(),
                name: None,
                simulated: vehicle.is_some_and(|v| self.is_simulated(v)),
                seen_armed: false,
                armed_at_us: from,
                ended_at_us: Some(to),
                end: uxduck_core::domain::flight::FlightEnd::Disarmed,
                air_us,
                max_height_m: None,
                max_speed_mps: None,
                modes,
                battery_start_pct: None,
                battery_end_pct: None,
                operators: Vec::new(),
                keep: true,
                recorded_here: true,
                left_account: false,
                contact_lost_at_us: None,
                link_id: None,
                tlog: None,
                // Its log with it unless "What to sync" keeps summaries only
                // (ADR-0046 B11).
                log: if self.state.settings.sync_content
                    >= uxduck_core::domain::settings::SyncContent::Logs
                {
                    uxduck_core::domain::flight::FlightLog::Asked
                } else {
                    uxduck_core::domain::flight::FlightLog::None
                },
                sync_video: false,
                mission: None,
                params: None,
                checklist: None,
                version: 1,
            };
            // Its part of the log, with the lead-in and lead-out.
            let mut part = Vec::new();
            let beginning = from.saturating_sub(LEAD_US);
            let input = &replay.inputs[usize::from(key.0)];
            let offset = replay.sources[usize::from(key.0)].offset_us;
            let mut prefix = Vec::new();
            let mut prefix_bytes = 0;
            for (stamp, frame) in state_records(input).into_iter().rev() {
                let stamp =
                    u64::try_from((i128::from(stamp) + i128::from(offset)).max(0)).unwrap_or(0);
                if stamp < beginning.saturating_sub(STATE_BEFORE_US) {
                    break;
                }
                if stamp >= beginning
                    || !uxduck_mavlink::framing::header(frame).is_some_and(|h| {
                        PARAM_MESSAGES.contains(&h.message_id)
                            || mission_log::MESSAGE_IDS.contains(&h.message_id)
                            || h.message_id == 148
                    })
                {
                    continue;
                }
                prefix_bytes += frame.len() as u64 + 8;
                if prefix_bytes > STATE_BEFORE_BYTES {
                    break;
                }
                prefix.push((stamp, frame));
            }
            prefix.reverse();
            for (stamp, frame) in mission_log::saved_prefix(&prefix, key.1) {
                let _ = uxduck_mavlink::tlog::record(&mut part, stamp, frame);
            }
            for record in replay.records.iter().filter(|r| {
                r.source == key.0
                    && r.stamp >= beginning
                    && r.stamp <= to.saturating_add(LEAD_US)
                    && uxduck_mavlink::framing::header(&replay.bytes[r.at..r.at + r.len])
                        .is_none_or(|h| {
                            h.system_id == key.1
                                || !replay.vehicles.contains_key(&(key.0, h.system_id))
                        })
            }) {
                let _ = uxduck_mavlink::tlog::record(
                    &mut part,
                    record.stamp,
                    &replay.bytes[record.at..record.at + record.len],
                );
            }
            logs.push((id.clone(), part));
            saved.push(flight);
        }
        for (id, part) in &logs {
            self.system
                .keep_log(id, part)
                .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        }
        let actor = self
            .replying_to
            .as_ref()
            .map(|(connection, _)| self.actor(*connection))
            .unwrap_or_default();
        let ids: Vec<String> = saved.iter().map(|f| f.id.clone()).collect();
        for flight in saved {
            self.commit_as(actor.clone(), Change::FlightUpserted { flight });
        }
        let name = self.replays.open.as_mut().and_then(|replay| {
            replay.saved.clone_from(&ids);
            replay.name.clone()
        });
        self.regroup_operations();
        // Named, its flights keep the name in their operation (R21).
        if let Some(name) = name {
            self.name_saved_flights(actor, &name, &ids);
        }
        self.push_replay(false);
        Ok(())
    }

    /// The video `flight/addVideo` names: a path, or a file attached to a
    /// message (S246), read where UXDuck keeps it under the name it had.
    fn added_video_file(
        &self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::video::FlightAddVideoParams,
    ) -> Result<(String, Option<String>), ApiError> {
        match &params.attachment {
            Some(id) => {
                let (found, path) = self.attached_path(connection, id)?;
                Ok((path, Some(found.name)))
            }
            None if params.path.is_empty() => Err(ApiError::new(
                ErrorKind::InvalidParams,
                "give path or attachment",
            )),
            None => Ok((params.path.clone(), None)),
        }
    }

    /// `flight/addVideo` (R8): a video from elsewhere kept with a flight,
    /// placed by its own start where it has one, else at the arming.
    fn add_flight_video(
        &mut self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::video::FlightAddVideoParams,
    ) -> Result<uxduck_core::domain::video::VideoRecording, ApiError> {
        use uxduck_core::domain::video::{
            AlignedBy, RecordingState, RecordingTrigger, VideoAlignment, VideoFileInput,
            VideoRecording,
        };
        let flight = self
            .state
            .flights
            .get(&params.flight_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no flight {}", params.flight_id),
                )
            })?;
        let (path, attached) = self.added_video_file(connection, params)?;
        let probe = self
            .system
            .probe_video(&path)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        let created = probe.klv_start_us.or(probe.created_us);
        let flight_end = flight.ended_at_us.unwrap_or(flight.armed_at_us);
        let (by, own_start) = match created {
            // MISB time stamps: when each picture was taken, exactly.
            Some(at) if probe.klv_start_us.is_some() => (AlignedBy::Klv, at),
            // A screen recording of a simulator on another computer: made
            // on the vehicle's clock, so placed by its offset from ours.
            Some(at) if flight.simulated => match self.vehicle_clock_offset(&flight.id) {
                Some(offset) if offset.unsigned_abs() > VEHICLE_CLOCK_US => {
                    (AlignedBy::VehicleClock, at.saturating_add_signed(-offset))
                }
                _ => (AlignedBy::Metadata, at),
            },
            Some(at) => (AlignedBy::Metadata, at),
            None => (AlignedBy::Hand, flight.armed_at_us),
        };
        let duration_us = probe
            .duration_us
            .unwrap_or_else(|| flight_end.saturating_sub(flight.armed_at_us).max(1_000_000));
        let name =
            attached.unwrap_or_else(|| path.rsplit(['/', '\\']).next().unwrap_or(&path).to_owned());
        let recording = VideoRecording {
            id: format!("vr{}", &self.random.token()[..16]),
            // A camera named after the file: each added video its own.
            source_id: format!("file:{name}"),
            vehicle_id: Some(flight.vehicle_id.clone()),
            started_at_us: own_start,
            ended_at_us: Some(own_start.saturating_add(duration_us)),
            duration_us,
            bytes: probe.bytes,
            codec: probe.codec,
            width: None,
            height: None,
            state: RecordingState::Done,
            detail: None,
            trigger: RecordingTrigger::ByHand,
            capture: None,
            file: Some(VideoFileInput {
                path: path.clone(),
                name: name.clone(),
                repeat: false,
                demo_id: None,
            }),
            camera_receipt_id: None,
            sound: false,
            file_name: Some(path.clone()),
            flight_id: Some(flight.id.clone()),
            alignment: Some(VideoAlignment {
                by,
                own_start_us: created.unwrap_or(own_start),
            }),
            version: 1,
        };
        self.commit(
            connection,
            Change::VideoRecordingUpserted {
                recording: recording.clone(),
            },
        );
        self.refresh_replay_recordings();
        Ok(recording)
    }

    /// The flight's vehicle clock less ours, from `SYSTEM_TIME` over the
    /// flight; none without its log or a fix.
    fn vehicle_clock_offset(&mut self, flight_id: &str) -> Option<i64> {
        let (bytes, ..) = self.flight_bytes(flight_id, None, false).ok()?;
        gps_offset(&bytes, &index(&bytes))
    }

    /// `video/align` (R8): an added video's start set by hand.
    fn align_video(
        &mut self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::video::VideoAlignParams,
    ) -> Result<uxduck_core::domain::video::VideoRecording, ApiError> {
        let mut recording = self
            .state
            .video_recordings
            .get(&params.recording_id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::NotFound,
                    format!("no recording {}", params.recording_id),
                )
            })?;
        let Some(alignment) = recording.alignment.as_mut() else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Only a video added to a flight is aligned by hand.",
            ));
        };
        alignment.by = uxduck_core::domain::video::AlignedBy::Hand;
        recording.started_at_us = params.started_at_us;
        recording.ended_at_us = Some(params.started_at_us.saturating_add(recording.duration_us));
        recording.version += 1;
        self.commit(
            connection,
            Change::VideoRecordingUpserted {
                recording: recording.clone(),
            },
        );
        self.refresh_replay_recordings();
        Ok(recording)
    }

    /// The open replay's recordings again, after one was added or moved.
    fn refresh_replay_recordings(&mut self) {
        let Some(replay) = self.replays.open.take() else {
            return;
        };
        let mut replay = replay;
        replay.recordings = self.replay_recordings(&replay);
        self.replays.open = Some(replay);
        self.push_replay(false);
    }

    /// An account log joined on this computer: opened from its file.
    pub(crate) fn account_log_joined(
        &mut self,
        connection: ConnectionId,
        params: &ReplayOpenParams,
        joined: Result<Value, ApiError>,
    ) -> Result<Value, ApiError> {
        let joined = joined?;
        let path = joined["path"].as_str().ok_or_else(|| {
            ApiError::new(ErrorKind::Internal, "the account's log was not joined")
        })?;
        let open = ReplayOpenParams {
            path: Some(path.to_owned()),
            time_us: params.time_us,
            ..ReplayOpenParams::default()
        };
        let mut input = self.replay_input(&open)?;
        if let Some(flight) = params
            .account
            .as_ref()
            .and_then(|log| self.state.flights.get(&log.id))
        {
            input.clock_from_flight = true;
            input.range = Some(self.flight_span(flight, params.ground));
            input.hint.vehicle = Some((
                flight.vehicle_id.clone(),
                self.state
                    .vehicles
                    .get(&flight.vehicle_id)
                    .map(|v| v.system_id),
                flight.vehicle_name.clone(),
            ));
            input.source.flight_id = Some(flight.id.clone());
            input.source.vehicle_id = Some(flight.vehicle_id.clone());
            input.flight_ids.push(flight.id.clone());
        }
        self.open_replay_inputs(vec![input])?;
        if let Some(t) = params.time_us {
            self.seek_replay(t);
        }
        to_value(&self.replay_state_for(connection, true))
    }

    fn replay_state(&self, with_paths: bool) -> ReplayState {
        let now = self.clock.now_us();
        self.replays
            .open
            .as_ref()
            .map_or_else(ReplayState::default, |r| {
                let mut state = r.state(now, with_paths);
                // An operation's or a lone flight's own name, else the one
                // given it (R21).
                state.name = self
                    .operation_of(&r.inputs)
                    .and_then(|o| o.name)
                    .or_else(|| self.flight_of(&r.inputs).and_then(|f| f.name.clone()))
                    .or_else(|| r.name.clone());
                // Each vehicle as it is named now, its original's (S238).
                for vehicle in &mut state.vehicles {
                    if let Some(now) = self.state.vehicles.get(&vehicle.id) {
                        vehicle.name.clone_from(&now.name);
                    }
                }
                state
            })
    }

    /// `replay/startTogether` (R20): each source nudged so its flight
    /// begins when the first source's does. A flight begins at its first
    /// arming, else at its source's first record.
    fn start_together(&mut self) -> Result<(), ApiError> {
        let Some(replay) = self.replays.open.as_ref() else {
            return Err(ApiError::new(ErrorKind::NotFound, "No replay is open."));
        };
        let begins = |source: u16| -> Option<u64> {
            let span = replay.sources.get(usize::from(source))?;
            let (start, end) = (span.start_us, span.end_us);
            let armed = replay
                .vehicles
                .iter()
                .filter(|(key, _)| key.0 == source)
                .filter_map(|(_, identity)| {
                    replay
                        .marks
                        .iter()
                        .filter(|m| {
                            m.kind == ReplayMarkKind::Armed && m.vehicle_id == identity.replay_id
                        })
                        .map(|m| m.at_us)
                        // Its own: a vehicle in two sources that follow
                        // each other is one vehicle with both its armings.
                        .filter(|&at| (start..=end).contains(&at))
                        .min()
                })
                .min();
            Some(armed.unwrap_or(start))
        };
        let count = u16::try_from(replay.sources.len()).unwrap_or(u16::MAX);
        let Some(target) = begins(0) else {
            return Ok(());
        };
        let by: Vec<i64> = (0..count)
            .map(|i| {
                begins(i).map_or(0, |at| {
                    i64::try_from(target).unwrap_or(i64::MAX)
                        - i64::try_from(at).unwrap_or(i64::MAX)
                })
            })
            .collect();
        if by.iter().all(|&d| d == 0) {
            return Ok(());
        }
        self.rebuild_sources(|all| {
            for (input, by) in all.iter_mut().zip(by) {
                input.nudge_us = input.nudge_us.saturating_add(by);
            }
            Ok(())
        })
    }

    /// `replay/rename` (R21): an operation's replay renames the operation,
    /// and a lone flight's the flight; any other is named while it is open.
    fn rename_replay(
        &mut self,
        connection: ConnectionId,
        name: &str,
        config: bool,
    ) -> Result<(), ApiError> {
        let max = crate::flight_time::MAX_FLIGHT_NAME_CHARS;
        let name = Some(name.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        if name.as_ref().is_some_and(|n| n.chars().count() > max) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("a replay's name has up to {max} characters"),
            ));
        }
        let (operation, flight) = self.replays.open.as_ref().map_or((None, None), |r| {
            (
                self.operation_of(&r.inputs),
                self.flight_of(&r.inputs).map(|f| f.id.clone()),
            )
        });
        if (operation.is_some() || flight.is_some()) && !config {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                Capability::Config.denied_message(),
            ));
        }
        if let Some(id) = flight {
            self.rename_flight(connection, &id, name.as_deref().unwrap_or_default())?;
        } else if let Some(mut operation) = operation {
            operation.name = name;
            operation.auto = false;
            self.rename_operation(connection, operation);
        } else if let Some(replay) = self.replays.open.as_mut() {
            replay.name = name;
        }
        self.push_replay(false);
        Ok(())
    }

    /// Tells every watcher where the replay is; `with_paths` only for the
    /// methods' results, which call [`Engine::replay_state`] themselves.
    fn push_replay(&mut self, _opened: bool) {
        self.replays.last_push_us = self.clock.now_us();
        let state = self.replay_state(false);
        let watchers: Vec<ConnectionId> = self.replays.watchers.iter().copied().collect();
        let without_steps = (state.marks.iter().any(|m| m.kind == ReplayMarkKind::Step)
            && watchers.iter().any(|id| !self.reads_step_marks(*id)))
        .then(|| {
            let mut older = state.clone();
            older.marks.retain(|m| m.kind != ReplayMarkKind::Step);
            crate::rpc::router::notification(&ServerNotification::Replay(older))
        });
        let frame = crate::rpc::router::notification(&ServerNotification::Replay(state));
        for id in watchers {
            match &without_steps {
                Some(older) if !self.reads_step_marks(id) => self.send(id, older),
                _ => self.send(id, &frame),
            }
        }
    }

    /// Whether `connection` reads replay marks of kind `step` (S343).
    /// UXDuck's page from before them rejects a whole replay that has one.
    fn reads_step_marks(&self, connection: ConnectionId) -> bool {
        self.connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
            .is_some_and(|a| a.reads.replay_step_marks)
    }

    /// The replay as `connection` reads it: without step marks for a
    /// client that did not claim them.
    fn replay_state_for(&self, connection: ConnectionId, with_paths: bool) -> ReplayState {
        let mut state = self.replay_state(with_paths);
        if !self.reads_step_marks(connection) {
            state.marks.retain(|m| m.kind != ReplayMarkKind::Step);
        }
        state
    }

    pub(crate) fn replay_disconnected(&mut self, id: ConnectionId) {
        self.replays.watchers.remove(&id);
    }
}

/// Every `replay/*` method: `read` alone, since nothing reaches a vehicle.
#[allow(clippy::too_many_lines, reason = "one arm per method")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    if let ClientRequest::FlightSeries(params) = &request {
        return to_value(&engine.flight_series(params)?);
    }
    if let ClientRequest::ReplayLogFields(params) = &request {
        return to_value(&engine.replay_log_fields(params)?);
    }
    if let ClientRequest::ReplayMotion(params) = &request {
        return to_value(&engine.replay_motion(params)?);
    }
    match &request {
        ClientRequest::FlightAddVideo(params) => {
            if !granted.contains(&Capability::Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Config.denied_message(),
                ));
            }
            let recording = engine.add_flight_video(connection, params)?;
            return to_value(&uxduck_contracts::dto::video::VideoRecording::from(
                &recording,
            ));
        }
        ClientRequest::VideoAlign(params) => {
            if !granted.contains(&Capability::Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Config.denied_message(),
                ));
            }
            let recording = engine.align_video(connection, params)?;
            return to_value(&uxduck_contracts::dto::video::VideoRecording::from(
                &recording,
            ));
        }
        _ => {}
    }
    engine.replays.watchers.insert(connection);
    let opened = engine.replay_open();
    let needs_open = || {
        ApiError::new(
            ErrorKind::NotFound,
            "No replay is open. Open one with replay/open.",
        )
    };
    match request {
        // From the account: joined by the root first, answered then (R12).
        ClientRequest::ReplayOpen(params) if params.account.is_some() => {
            let Some(log) = params.account.clone() else {
                return Err(ApiError::new(ErrorKind::Internal, "no account log"));
            };
            let id = engine.ask_account(crate::account::AccountAction::ReplayLog {
                device_id: log.device_id,
                id: log.id,
            });
            engine.replays.awaiting.insert(id, (connection, params));
            return Ok(Value::Null);
        }
        ClientRequest::ReplayOpen(params) => {
            engine.open_replay_for(connection, &params)?;
            if let Some(t) = params.time_us {
                engine.seek_replay(t);
            }
        }
        ClientRequest::ReplayState(Empty {}) => {}
        ClientRequest::ReplayClose(Empty {}) => engine.close_replay(),
        _ if !opened => return Err(needs_open()),
        ClientRequest::ReplayAdd(params) => {
            let inputs = engine.replay_inputs(&params)?;
            engine.rebuild_sources(move |all| {
                all.extend(inputs);
                Ok(())
            })?;
        }
        ClientRequest::ReplayNudge(params) => {
            engine.rebuild_sources(|all| {
                let input = usize::try_from(params.source)
                    .ok()
                    .and_then(|i| all.get_mut(i))
                    .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such source"))?;
                input.nudge_us = input.nudge_us.saturating_add(params.by_us);
                Ok(())
            })?;
            engine.keep_replay_offsets(connection);
        }
        ClientRequest::ReplayPlay(Empty {}) => engine.play_replay(true),
        ClientRequest::ReplayPause(Empty {}) => engine.play_replay(false),
        ClientRequest::ReplayRestart(Empty {}) => engine.restart_replay(),
        ClientRequest::ReplaySpeed(params) => engine.speed_replay(params.speed)?,
        ClientRequest::ReplaySave(Empty {}) => {
            if !granted.contains(&Capability::Config) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Config.denied_message(),
                ));
            }
            engine.save_replay()?;
        }
        ClientRequest::ReplaySeek(params) => engine.seek_replay(params.time_us),
        ClientRequest::ReplayStartTogether(Empty {}) => {
            engine.start_together()?;
            engine.keep_replay_offsets(connection);
        }
        ClientRequest::ReplayRename(params) => engine.rename_replay(
            connection,
            &params.name,
            granted.contains(&Capability::Config),
        )?,
        _ => return Err(ApiError::new(ErrorKind::Internal, "not a replay method")),
    }
    to_value(&engine.replay_state_for(connection, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinning_keeps_both_ends() {
        let mut points: Vec<u32> = (0..10_000).collect();
        thin(&mut points, 5);
        assert_eq!(points, [0, 2499, 4999, 7499, 9999]);
        let mut few = vec![1, 2, 3];
        thin(&mut few, 5);
        assert_eq!(few, [1, 2, 3]);
    }
}

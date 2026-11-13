//! Parameters for the agent and MCP clients (ADR-0021 P8, S68): search,
//! read, list and compare a vehicle's parameters with what its definition
//! set says, in compact answers that name the set they come from. Writing
//! stays `param/set`, which asks the operator first.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde_json::Value;
use uxduck_contracts::dto::param_definitions::{
    DescribedBy, GroupCount, HeldParameters, ParamBrief, ParamChange, ParamChangeBy,
    ParamChangesParams, ParamChangesResult, ParamDetail, ParamDiffParams, ParamDiffResult,
    ParamDifference, ParamGetParams, ParamGetResult, ParamInLog, ParamKnown, ParamListParams,
    ParamListResult, ParamSearchParams, ParamSearchResult, ParamSeenLater,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::param_definitions::{Layered, ParamDefinition, UserLevel};
use uxduck_core::domain::parameter::{TransferStatus, same_value};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_mavlink::param_log::{Completeness, Known, Source};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Above this many, an unfiltered `param/list` answers with group counts.
const LIST_AT_ONCE: u32 = 100;
const DEFAULT_HITS: u32 = 20;
const MAX_HITS: u32 = 100;
const MAX_NAMES: usize = 50;
/// A page of briefs stays under this many characters, well inside what an
/// agent takes whole (`MAX_TOOL_OUTPUT_CHARS`), and says where the next
/// begins (FB-104).
const PAGE_CHARS: usize = 16_000;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// The vehicle asked about, or the only one there is.
pub(crate) fn which_vehicle(engine: &Engine, id: Option<&str>) -> Result<Vehicle, ApiError> {
    if let Some(id) = id {
        return engine
            .vehicle(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")));
    }
    let all: Vec<&Vehicle> = engine
        .state
        .vehicles
        .values()
        .filter(|v| !v.dismissed)
        .collect();
    match all.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(ApiError::new(ErrorKind::NotFound, "there is no vehicle")),
        many => Err(invalid(format!(
            "more than one vehicle: say which, by vehicleId: {}",
            many.iter()
                .map(|v| {
                    format!(
                        "{} {}, {}, {} parameters held",
                        named(v),
                        v.id,
                        match v.presence {
                            Presence::Online => "online",
                            Presence::Stale => "not heard lately",
                            Presence::Offline => "offline",
                        },
                        held_count(engine, &v.id)
                    )
                })
                .collect::<Vec<_>>()
                .join(". ")
        ))),
    }
}

/// How the agent calls a vehicle: "<name> (replay)" for a replay's.
fn named(vehicle: &Vehicle) -> String {
    if vehicle.replay {
        format!("{} (replay)", vehicle.name)
    } else {
        vehicle.name.clone()
    }
}

fn held_count(engine: &Engine, vehicle_id: &str) -> u32 {
    let prefix = format!("{vehicle_id}/");
    count(
        engine
            .state
            .parameters
            .range(prefix.clone()..)
            .take_while(|(id, _)| id.starts_with(&prefix))
            .count(),
    )
}

/// What UXDuck holds of a vehicle's parameters, and what to do when it is
/// not all of them or not current (FB-104).
fn held(engine: &Engine, vehicle: &Vehicle, held: u32, at_us: Option<u64>) -> HeldParameters {
    let set = engine.parameter_set(&vehicle.id);
    if vehicle.replay {
        // As its log had them at the replay's time, or the one asked
        // (ADR-0050 L6).
        let known = engine.replay_timeline(&vehicle.id).map(|(timeline, now)| {
            let t = at_us.unwrap_or(now);
            (timeline.completeness(t), t)
        });
        return HeldParameters {
            status: set.status.into(),
            count: held,
            total: known.and_then(|(c, _)| c.total).map(u32::from),
            complete: known.is_some_and(|(c, _)| c.whole()),
            synced_at_us: None,
            note: Some(replay_note(engine, vehicle, held, known)),
            at_us: known.map(|(_, t)| t),
        };
    }
    HeldParameters {
        status: set.status.into(),
        count: held,
        total: set.progress.and_then(|p| p.total),
        complete: set.status == TransferStatus::Synced,
        synced_at_us: set.synced_at_us,
        note: live_note(engine, vehicle, &set, held),
        at_us: None,
    }
}

/// What a replay vehicle's values are: how much of the set its log had
/// read by the replay's time, and where the rest is.
fn replay_note(
    engine: &Engine,
    vehicle: &Vehicle,
    held: u32,
    known: Option<(Completeness, u64)>,
) -> String {
    let (c, at_us) = known.unwrap_or_default();
    let at = super::saved::date_time(at_us);
    let boot = c
        .boot_us
        .map(|b| format!(" since it restarted at {}", super::saved::date_time(b)))
        .unwrap_or_default();
    let later = if c.later > 0 {
        format!(" {} more are known only from later in the log.", c.later)
    } else {
        String::new()
    };
    let mut note = match c.total {
        Some(total) if c.whole() => format!(
            "A replay at {at}: its log had read all {total} of the vehicle's parameters{boot} by \
             then, and these values are as they were at that time.{later}"
        ),
        Some(total) => {
            let rest = if c.read + c.later < u32::from(total) {
                " The others were never read in this log."
            } else {
                ""
            };
            format!(
                "A replay at {at}: its log had read {} of the vehicle's {total} parameters{boot} \
                 by then.{later}{rest}",
                c.read
            )
        }
        None if c.boot_us.is_some() => format!(
            "A replay at {at}: its log has no parameter count{boot}. It had read {} \
             parameters in this boot by then.{later}",
            c.read
        ),
        None if c.later > 0 => format!(
            "A replay at {at}: its log had read none of the vehicle's parameters by then. These \
             {held} are the values first read later in the log."
        ),
        None => format!(
            "A replay at {at}: its log holds no download of the vehicle's parameters, so these \
             {held} are only what the vehicle reported by itself, not its whole set."
        ),
    };
    if c.boot_us.is_some() {
        note.push_str(" Values seen only before the restart are withheld, not current.");
    }
    if known.is_none() {
        note.push_str(" No parameter set could be tied to a unique recorded autopilot identity.");
    }
    note.push_str(" None can be downloaded from a replay.");
    if c.whole() {
        return note;
    }
    match vehicle
        .replay_of
        .as_deref()
        .and_then(|id| engine.vehicle(id))
    {
        Some(original) => {
            let theirs = held_count(engine, &original.id);
            let _ = if theirs == 0 {
                write!(
                    note,
                    " The vehicle itself, {} (vehicleId {}), has none read yet: connect it and \
                     call param_refresh with id {}.",
                    original.name, original.id, original.id
                )
            } else {
                write!(
                    note,
                    " The vehicle itself, {} (vehicleId {}), has {theirs} read: ask with its \
                     vehicleId for them, as they are now rather than as this flight flew.",
                    original.name, original.id
                )
            };
        }
        None => note.push_str(
            " Its vehicle is not here: connect it to read all of them, or replay the flight's \
             ArduPilot autopilot log (.bin), which records every parameter.",
        ),
    }
    note
}

fn live_note(
    engine: &Engine,
    vehicle: &Vehicle,
    set: &uxduck_core::domain::parameter::ParameterSet,
    held: u32,
) -> Option<String> {
    let download = if vehicle.presence == Presence::Offline {
        format!(
            "It is offline: connect it, then call param_refresh with id {}.",
            vehicle.id
        )
    } else if let Some(why) = engine.not_writable(&vehicle.link_id) {
        format!("They cannot be downloaded now: {why}.")
    } else {
        format!(
            "Call param_refresh with id {} to download all of them, then ask again.",
            vehicle.id
        )
    };
    match set.status {
        TransferStatus::Synced if vehicle.presence == Presence::Offline => Some(
            "Offline: these are its values as last downloaded (syncedAtUs). They may have \
             changed since."
                .into(),
        ),
        TransferStatus::Synced => None,
        TransferStatus::Downloading | TransferStatus::Uploading => Some(format!(
            "Downloading: {} so far. These are the {held} held before it began. Ask again in a \
             few seconds for all of them.",
            set.progress.map_or_else(
                || "none".to_owned(),
                |p| p.total.map_or_else(
                    || p.done.to_string(),
                    |total| format!("{} of {total}", p.done)
                )
            )
        )),
        TransferStatus::Failed => Some(format!(
            "The last download failed{}: these are the {held} held. {download}",
            set.detail
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        )),
        TransferStatus::Unknown => Some(format!(
            "Not downloaded yet: only the {held} it reported by itself. {download}"
        )),
    }
}

/// The vehicle's parameters by name, and its set with the frame it needs.
struct Described {
    vehicle: Vehicle,
    values: BTreeMap<String, f64>,
    /// Firmware defaults, where the vehicle said them (S74).
    defaults: BTreeMap<String, f64>,
    set: Option<Layered>,
    helicopter: bool,
    /// A replay vehicle's: how its log knows each value, at `at_us`.
    log: BTreeMap<String, ParamInLog>,
    at_us: Option<u64>,
}

impl Described {
    fn of(engine: &Engine, vehicle: Vehicle, at_us: Option<u64>) -> Self {
        let prefix = format!("{}/", vehicle.id);
        let mine = || {
            engine
                .state
                .parameters
                .range(prefix.clone()..)
                .take_while(|(id, _)| id.starts_with(&prefix))
                .map(|(_, p)| p)
        };
        let mut values: BTreeMap<String, f64> = mine().map(|p| (p.name.clone(), p.value)).collect();
        let mut defaults: BTreeMap<String, f64> = mine()
            .filter_map(|p| p.default.map(|d| (p.name.clone(), d)))
            .collect();
        let mut log = BTreeMap::new();
        // A replay vehicle's, from its log at the time asked (ADR-0050 L9).
        if let Some((timeline, now)) = engine.replay_timeline(&vehicle.id) {
            let t = at_us.unwrap_or(now);
            for at in timeline.at(t) {
                let known = match at.known {
                    Known::Seen => ParamKnown::Seen,
                    Known::Later => ParamKnown::Later,
                    Known::BeforeBoot => {
                        values.remove(&at.name);
                        defaults.remove(&at.name);
                        continue;
                    }
                };
                let changes_at_us = timeline.readings.get(&at.name).and_then(|readings| {
                    readings
                        .iter()
                        .find(|r| r.at_us > t && r.value.to_bits() != at.reading.value.to_bits())
                        .map(|r| r.at_us)
                });
                values.insert(at.name.clone(), f64::from(at.reading.value));
                if let Some(default) = at.reading.default {
                    defaults.insert(at.name.clone(), f64::from(default));
                }
                log.insert(
                    at.name,
                    ParamInLog {
                        known,
                        read_at_us: at.reading.at_us,
                        changes_at_us,
                    },
                );
            }
        }
        let set = engine.vehicle_definitions(&vehicle.id);
        let helicopter = vehicle.vehicle_type == "Helicopter";
        Self {
            vehicle,
            values,
            defaults,
            set,
            helicopter,
            log,
            at_us,
        }
    }

    fn statistic(&self, name: &str) -> bool {
        statistic(name, self.definition(name))
    }

    /// Changed from its default, when the default is known (S74).
    fn changed(&self, name: &str) -> bool {
        self.values
            .get(name)
            .zip(self.defaults.get(name))
            .is_some_and(|(v, d)| !same_value(*v, *d))
    }

    fn definition(&self, name: &str) -> Option<&ParamDefinition> {
        self.set.as_ref()?.find(name, self.helicopter)
    }

    /// Its definition, naming the set it came from (ADR-0044 F9).
    fn detailed(
        &self,
        name: &str,
    ) -> Option<uxduck_contracts::dto::param_definitions::ParamDefinition> {
        let (definition, layer) = self.set.as_ref()?.described(name, self.helicopter)?;
        Some(uxduck_contracts::dto::param_definitions::ParamDefinition {
            from: Some(layer.title.clone()),
            ..definition.into()
        })
    }

    /// The set that described it.
    fn from(&self, name: &str) -> Option<String> {
        self.set
            .as_ref()?
            .described(name, self.helicopter)
            .map(|(_, layer)| layer.title.clone())
    }

    fn by(&self, engine: &Engine) -> DescribedBy {
        DescribedBy {
            vehicle_id: self.vehicle.id.clone(),
            vehicle_name: named(&self.vehicle),
            firmware: self.vehicle.firmware.clone(),
            definitions: engine
                .state
                .param_definitions
                .get(&self.vehicle.id)
                .map(Into::into),
            replay: self.vehicle.replay,
            held: Some(held(
                engine,
                &self.vehicle,
                count(self.values.len()),
                self.at_us,
            )),
        }
    }

    fn brief(&self, name: &str) -> ParamBrief {
        let definition = self.definition(name);
        let value = self.values.get(name).copied();
        ParamBrief {
            name: name.to_owned(),
            display_name: definition.map(|d| d.display_name.clone()),
            summary: definition.map(|d| summary(&d.description)),
            value,
            units: definition.and_then(|d| d.units.clone()),
            default: self.defaults.get(name).copied(),
            changed: self.changed(name),
            value_name: definition.zip(value).and_then(|(d, v)| {
                d.values
                    .iter()
                    .find(|n| same_value(n.value, v))
                    .map(|n| n.name.clone())
            }),
            from: self.from(name),
            statistic: self.statistic(name),
            in_log: self.log.get(name).cloned(),
        }
    }
}

/// A statistic (ADR-0050 L8): `STAT_*`, or read-only by its definition.
/// It changes as the vehicle runs, not because anyone set it.
pub(crate) fn statistic(name: &str, definition: Option<&ParamDefinition>) -> bool {
    name.starts_with("STAT_") || definition.is_some_and(|d| d.read_only)
}

/// Who made a change in a log (ADR-0050 L5): a set's sender, UXDuck by
/// its own system id; the vehicle by itself; or two downloads that differ.
pub(crate) fn change_by(source: Source, gcs_system_id: u8) -> (ParamChangeBy, Option<u8>) {
    match source {
        Source::Set { by } if by == gcs_system_id => (ParamChangeBy::Uxduck, None),
        Source::Set { by } => (ParamChangeBy::GroundStation, Some(by)),
        Source::Reported => (ParamChangeBy::Vehicle, None),
        Source::Listed { .. } | Source::File { .. } => (ParamChangeBy::Unseen, None),
    }
}

/// A description's first sentence, at most 160 characters.
fn summary(description: &str) -> String {
    let first = description
        .split_inclusive(". ")
        .next()
        .unwrap_or(description)
        .trim();
    if first.chars().count() <= 160 {
        first.to_owned()
    } else {
        format!("{}…", first.chars().take(159).collect::<String>())
    }
}

/// How well a parameter matches (P8): an exact name, a name prefix, the
/// name holding the query, then every word in the display name, then in
/// the description; 0 is no match.
fn rank(query: &str, name: &str, definition: Option<&ParamDefinition>) -> u8 {
    let upper = query.trim().to_uppercase();
    if name == upper {
        return 100;
    }
    if name.starts_with(&upper) {
        return 50;
    }
    if !upper.contains(' ') && name.contains(&upper) {
        return 30;
    }
    match definition.map_or(0, |d| d.meaning_score(&query.trim().to_lowercase())) {
        2 => 20,
        1 => 10,
        _ => 0,
    }
}

/// At most `limit` briefs from `offset`, fewer when they would pass
/// `PAGE_CHARS`, and where the next page begins when there are more.
fn page(
    described: &Described,
    names: &[&str],
    offset: Option<u32>,
    limit: u32,
) -> (Vec<ParamBrief>, Option<u32>) {
    let start = offset.unwrap_or(0) as usize;
    let mut briefs = Vec::new();
    let mut chars = 0;
    for name in names.iter().skip(start).take(limit as usize) {
        let brief = described.brief(name);
        chars += serde_json::to_string(&brief).map_or(0, |text| text.len() + 1);
        if chars > PAGE_CHARS && !briefs.is_empty() {
            break;
        }
        briefs.push(brief);
    }
    let next = start + briefs.len();
    (briefs, (next < names.len()).then(|| count(next)))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn search(engine: &Engine, params: &ParamSearchParams) -> Result<ParamSearchResult, ApiError> {
    if params.query.trim().is_empty() {
        return Err(invalid("say what to search for"));
    }
    let described = Described::of(
        engine,
        which_vehicle(engine, params.vehicle_id.as_deref())?,
        None,
    );
    let mut names: BTreeSet<&str> = described.values.keys().map(String::as_str).collect();
    if params.whole_set
        && let Some(set) = described.set.as_ref()
    {
        names.extend(set.names());
    }
    let mut ranked: Vec<(u8, &str)> = names
        .into_iter()
        .filter(|name| !params.changed_only || described.changed(name))
        .map(|name| (rank(&params.query, name, described.definition(name)), name))
        .filter(|(score, _)| *score > 0)
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let total = count(ranked.len());
    let limit = params.limit.unwrap_or(DEFAULT_HITS).clamp(1, MAX_HITS);
    let names: Vec<&str> = ranked.into_iter().map(|(_, name)| name).collect();
    let (hits, next_offset) = page(&described, &names, params.offset, limit);
    Ok(ParamSearchResult {
        described_by: described.by(engine),
        hits,
        total,
        next_offset,
    })
}

fn get(engine: &Engine, params: &ParamGetParams) -> Result<ParamGetResult, ApiError> {
    if params.names.is_empty() || params.names.len() > MAX_NAMES {
        return Err(invalid(format!("ask for 1 to {MAX_NAMES} parameters")));
    }
    let described = Described::of(
        engine,
        which_vehicle(engine, params.vehicle_id.as_deref())?,
        None,
    );
    let parameters = params
        .names
        .iter()
        .map(|name| {
            let name = name.trim().to_uppercase();
            ParamDetail {
                value: described.values.get(&name).copied(),
                default: described.defaults.get(&name).copied(),
                changed: described.changed(&name),
                definition: described.detailed(&name),
                statistic: described.statistic(&name),
                in_log: described.log.get(&name).cloned(),
                name,
            }
        })
        .collect();
    Ok(ParamGetResult {
        described_by: described.by(engine),
        parameters,
    })
}

/// A parameter's group: its definition's, or its name up to the first `_`.
fn group_of(name: &str, definition: Option<&ParamDefinition>) -> String {
    definition.map_or_else(
        || {
            name.find('_')
                .map_or_else(|| name.to_owned(), |end| name[..=end].to_owned())
        },
        |d| d.group.clone(),
    )
}

fn list(engine: &Engine, params: &ParamListParams) -> Result<ParamListResult, ApiError> {
    let described = Described::of(
        engine,
        which_vehicle(engine, params.vehicle_id.as_deref())?,
        params.at_us,
    );
    let wanted = params.group.as_deref().map(|g| g.trim().to_uppercase());
    let names: Vec<&str> = described
        .values
        .keys()
        .map(String::as_str)
        .filter(|name| {
            let definition = described.definition(name);
            let in_group = wanted
                .as_deref()
                .is_none_or(|g| name.starts_with(g) || definition.is_some_and(|d| d.group == g));
            in_group
                && (!params.standard_only
                    || definition.is_some_and(|d| d.user == UserLevel::Standard))
                && (!params.changed_only || described.changed(name))
        })
        .collect();
    let total = count(names.len());
    if wanted.is_none() && names.len() > LIST_AT_ONCE as usize {
        let mut groups: BTreeMap<String, u32> = BTreeMap::new();
        for name in &names {
            *groups
                .entry(group_of(name, described.definition(name)))
                .or_default() += 1;
        }
        return Ok(ParamListResult {
            described_by: described.by(engine),
            parameters: Vec::new(),
            groups: groups
                .into_iter()
                .map(|(group, count)| GroupCount { group, count })
                .collect(),
            total,
            next_offset: None,
        });
    }
    let limit = params.limit.unwrap_or(LIST_AT_ONCE).clamp(1, 500);
    let (parameters, next_offset) = page(&described, &names, params.offset, limit);
    Ok(ParamListResult {
        described_by: described.by(engine),
        parameters,
        groups: Vec::new(),
        total,
        next_offset,
    })
}

/// A replay vehicle's changes in its log (ADR-0050 L9), statistics left
/// out unless asked for, and the values it first read after the replay's
/// time.
fn changes(engine: &Engine, params: &ParamChangesParams) -> Result<ParamChangesResult, ApiError> {
    let vehicle = which_vehicle(engine, params.vehicle_id.as_deref())?;
    let Some((timeline, now)) = engine.replay_timeline(&vehicle.id) else {
        return Err(invalid(format!(
            "{} is not a replay: a live vehicle's changes are in param_history.",
            named(&vehicle)
        )));
    };
    let gcs = engine.state.settings.gcs_system_id;
    let original = vehicle.replay_of.clone();
    let described = Described::of(engine, vehicle, None);
    let mut statistics_left_out = 0;
    let mut changes = Vec::new();
    for change in timeline.changes(
        params.from_us.unwrap_or(0),
        params.to_us.unwrap_or(u64::MAX),
    ) {
        let statistic = described.statistic(&change.name);
        if statistic && !params.statistics {
            statistics_left_out += 1;
            continue;
        }
        let (by, system_id) = change_by(change.source, gcs);
        let reason = match (by, original.as_deref()) {
            (ParamChangeBy::Uxduck, Some(original)) => {
                engine.param_reason(original, &change.name, change.at_us)
            }
            _ => None,
        };
        changes.push(ParamChange {
            name: change.name,
            at_us: change.at_us,
            from: f64::from(change.from),
            to: f64::from(change.to),
            by,
            system_id,
            statistic,
            reason,
        });
    }
    let later = timeline
        .at(now)
        .into_iter()
        .filter(|at| at.known == Known::Later)
        .map(|at| ParamSeenLater {
            name: at.name,
            read_at_us: at.reading.at_us,
        })
        .collect();
    Ok(ParamChangesResult {
        described_by: described.by(engine),
        changes,
        statistics_left_out,
        later,
    })
}

/// A `.param` file's values, and the numbers of the lines it could not read.
/// Mission Planner writes `NAME,VALUE`, and QGroundControl's `.params` lead
/// with the vehicle and component and end with the type:
/// `1\t1\tMPC_XY_CRUISE\t5.0\t9`.
pub(crate) fn read_param_file(text: &str) -> (BTreeMap<String, f64>, Vec<u32>) {
    let mut values = BTreeMap::new();
    let mut unread = Vec::new();
    let numbered = |field: &str| field.bytes().all(|b| b.is_ascii_digit());
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|field| !field.is_empty())
            .collect();
        let (name, value) = match fields.as_slice() {
            [system, component, name, value, ..] if numbered(system) && numbered(component) => {
                (*name, Some(*value))
            }
            [name, value, ..] => (*name, Some(*value)),
            _ => ("", None),
        };
        let value = value.and_then(|v| v.parse::<f64>().ok());
        match value {
            Some(value)
                if name.starts_with(|c: char| c.is_ascii_alphabetic())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                values.insert(name.to_uppercase(), value);
            }
            _ => unread.push(u32::try_from(number + 1).unwrap_or(u32::MAX)),
        }
    }
    (values, unread)
}

fn diff(engine: &Engine, params: &ParamDiffParams) -> Result<ParamDiffResult, ApiError> {
    let described = Described::of(
        engine,
        which_vehicle(engine, params.vehicle_id.as_deref())?,
        None,
    );
    // A file is usually a few values; a vehicle or a snapshot is all of them.
    let (theirs, whole, unread_lines) = match (
        params.text.as_deref(),
        params.other_vehicle_id.as_deref(),
        params.snapshot_id.as_deref(),
        params.set_id.as_deref(),
    ) {
        (Some(text), None, None, None) => {
            let (values, unread) = read_param_file(text);
            (values, false, unread)
        }
        (None, Some(other), None, None) => {
            let other = which_vehicle(engine, Some(other))?;
            (Described::of(engine, other, None).values, true, Vec::new())
        }
        // A saved set (S217): what applying it would change, and what the
        // vehicle does not have; the vehicle's own new parameters are not
        // differences.
        (None, None, None, Some(id)) => {
            let set = engine.state.saved_params.get(id).ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no saved parameter set {id}"))
            })?;
            (set.values.clone(), false, Vec::new())
        }
        (None, None, Some(id), None) => {
            let snapshot =
                engine.state.snapshots.get(id).ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, format!("no snapshot {id}"))
                })?;
            let values = snapshot
                .parameters
                .iter()
                .map(|p| (p.name.clone(), p.value))
                .collect();
            (values, true, Vec::new())
        }
        _ => {
            return Err(invalid(
                "give one of text, otherVehicleId, snapshotId or setId",
            ));
        }
    };
    let mut names: BTreeSet<&str> = theirs.keys().map(String::as_str).collect();
    if whole {
        names.extend(described.values.keys().map(String::as_str));
    }
    let differences = names
        .into_iter()
        .filter_map(|name| {
            let value = described.values.get(name).copied();
            let other = theirs.get(name).copied();
            let same = matches!((value, other), (Some(a), Some(b)) if same_value(a, b));
            (!same).then(|| ParamDifference {
                name: name.to_owned(),
                value,
                theirs: other,
                units: described.definition(name).and_then(|d| d.units.clone()),
            })
        })
        .collect();
    Ok(ParamDiffResult {
        described_by: described.by(engine),
        differences,
        unread_lines,
    })
}

/// The vehicle's parameters now, by name (C7).
/// The parameters a vehicle arms with (C7): their digest, so two flights can
/// be told apart by the configuration they flew. None before they are read.
pub(crate) fn armed_with(
    engine: &Engine,
    vehicle_id: &str,
) -> Option<uxduck_core::domain::flight::FlightParams> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let values = vehicle_values(engine, which_vehicle(engine, Some(vehicle_id)).ok()?);
    if values.is_empty() {
        return None;
    }
    let mut lines = String::new();
    for (name, value) in &values {
        let _ = writeln!(lines, "{name}={value}");
    }
    let digest =
        Sha256::digest(lines.as_bytes())
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
    Some(uxduck_core::domain::flight::FlightParams {
        digest,
        count: u32::try_from(values.len()).unwrap_or(u32::MAX),
    })
}

pub(crate) fn vehicle_values(engine: &Engine, vehicle: Vehicle) -> BTreeMap<String, f64> {
    Described::of(engine, vehicle, None).values
}

pub(crate) fn handle(
    engine: &mut Engine,
    _connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    match request {
        ClientRequest::ParamSearch(params) => to_value(&search(engine, &params)?),
        ClientRequest::ParamGet(params) => to_value(&get(engine, &params)?),
        ClientRequest::ParamList(params) => to_value(&list(engine, &params)?),
        ClientRequest::ParamDiff(params) => to_value(&diff(engine, &params)?),
        ClientRequest::ParamChanges(params) => to_value(&changes(engine, &params)?),
        _ => Err(ApiError::new(ErrorKind::Internal, "not a parameter query")),
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "values read exactly from text")]
mod tests {
    use super::*;

    #[test]
    fn a_param_file_is_read_as_mission_planner_writes_it() {
        let (values, unread) = read_param_file(
            "# saved\nRTL_ALT,3000\nWPNAV_SPEED 1000.5\n\nnot a line\nfs_thr_enable\t1\n",
        );
        assert_eq!(values["RTL_ALT"], 3000.0);
        assert_eq!(values["WPNAV_SPEED"], 1000.5);
        assert_eq!(values["FS_THR_ENABLE"], 1.0);
        assert_eq!(unread, [5]);
    }

    #[test]
    fn a_params_file_is_read_as_qgroundcontrol_writes_it() {
        let (values, unread) = read_param_file(
            "# Onboard parameters for Vehicle 1\n#\n# Vehicle-Id Component-Id Name Value Type\n\
             1\t1\tMPC_XY_CRUISE\t5.000000000000000000\t9\n\
             1\t1\tSYS_AUTOSTART\t10040\t6\n1\t1\n",
        );
        assert_eq!(values.len(), 2, "{values:?}");
        assert_eq!(values["MPC_XY_CRUISE"], 5.0);
        assert_eq!(values["SYS_AUTOSTART"], 10040.0);
        assert_eq!(unread, [6]);
    }

    #[test]
    fn a_summary_is_the_first_sentence() {
        assert_eq!(summary("Climbs first. Then returns."), "Climbs first.");
        assert_eq!(summary("No full stop"), "No full stop");
        assert_eq!(summary(&"x".repeat(200)).chars().count(), 160);
    }

    #[test]
    fn names_rank_above_meanings() {
        let definition = ParamDefinition {
            name: "RTL_ALT".into(),
            display_name: "RTL Altitude".into(),
            description: "Height to climb to before returning.".into(),
            ..ParamDefinition::default()
        };
        assert_eq!(rank("rtl_alt", "RTL_ALT", Some(&definition)), 100);
        assert_eq!(rank("RTL", "RTL_ALT", Some(&definition)), 50);
        assert_eq!(rank("alt", "RTL_ALT", Some(&definition)), 30);
        assert_eq!(rank("return altitude", "RTL_ALT", Some(&definition)), 20);
        assert_eq!(rank("climb", "RTL_ALT", Some(&definition)), 10);
        assert_eq!(rank("compass", "RTL_ALT", Some(&definition)), 0);
    }
}

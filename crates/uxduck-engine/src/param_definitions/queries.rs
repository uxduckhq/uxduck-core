//! Parameters for the agent and MCP clients (ADR-0021 P8, S68): search,
//! read, list and compare a vehicle's parameters with what its definition
//! set says, in compact answers that name the set they come from. Writing
//! stays `param/set`, which asks the operator first.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::param_definitions::{
    DescribedBy, GroupCount, ParamBrief, ParamDetail, ParamDiffParams, ParamDiffResult,
    ParamDifference, ParamGetParams, ParamGetResult, ParamListParams, ParamListResult,
    ParamSearchParams, ParamSearchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::param_definitions::{DefinitionSet, ParamDefinition, UserLevel};
use uxduck_core::domain::parameter::same_value;
use uxduck_core::domain::vehicle::Vehicle;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Above this many, an unfiltered `param/list` answers with group counts.
const LIST_AT_ONCE: u32 = 100;
const DEFAULT_HITS: u32 = 20;
const MAX_HITS: u32 = 100;
const MAX_NAMES: usize = 50;

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
    let mut all = engine.state.vehicles.values().filter(|v| !v.dismissed);
    match (all.next(), all.next()) {
        (Some(only), None) => Ok(only.clone()),
        (None, _) => Err(ApiError::new(ErrorKind::NotFound, "there is no vehicle")),
        (Some(_), Some(_)) => Err(invalid("more than one vehicle: say which, by vehicleId")),
    }
}

/// The vehicle's parameters by name, and its set with the frame it needs.
struct Described {
    vehicle: Vehicle,
    values: BTreeMap<String, f64>,
    /// Firmware defaults, where the vehicle said them (S74).
    defaults: BTreeMap<String, f64>,
    set: Option<std::sync::Arc<DefinitionSet>>,
    helicopter: bool,
}

impl Described {
    fn of(engine: &Engine, vehicle: Vehicle) -> Self {
        let prefix = format!("{}/", vehicle.id);
        let mine = || {
            engine
                .state
                .parameters
                .range(prefix.clone()..)
                .take_while(|(id, _)| id.starts_with(&prefix))
                .map(|(_, p)| p)
        };
        let values = mine().map(|p| (p.name.clone(), p.value)).collect();
        let defaults = mine()
            .filter_map(|p| p.default.map(|d| (p.name.clone(), d)))
            .collect();
        let set = engine.vehicle_definitions(&vehicle.id);
        let helicopter = vehicle.vehicle_type == "Helicopter";
        Self {
            vehicle,
            values,
            defaults,
            set,
            helicopter,
        }
    }

    /// Changed from its default, when the default is known (S74).
    fn changed(&self, name: &str) -> bool {
        self.values
            .get(name)
            .zip(self.defaults.get(name))
            .is_some_and(|(v, d)| !same_value(*v, *d))
    }

    fn definition(&self, name: &str) -> Option<&ParamDefinition> {
        self.set.as_deref()?.find(name, self.helicopter)
    }

    fn by(&self, engine: &Engine) -> DescribedBy {
        DescribedBy {
            vehicle_id: self.vehicle.id.clone(),
            vehicle_name: self.vehicle.name.clone(),
            firmware: self.vehicle.firmware.clone(),
            definitions: engine
                .state
                .param_definitions
                .get(&self.vehicle.id)
                .map(Into::into),
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
        }
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

fn page<T>(items: Vec<T>, offset: Option<u32>, limit: u32) -> Vec<T> {
    items
        .into_iter()
        .skip(offset.unwrap_or(0) as usize)
        .take(limit as usize)
        .collect()
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn search(engine: &Engine, params: &ParamSearchParams) -> Result<ParamSearchResult, ApiError> {
    if params.query.trim().is_empty() {
        return Err(invalid("say what to search for"));
    }
    let described = Described::of(engine, which_vehicle(engine, params.vehicle_id.as_deref())?);
    let mut names: BTreeSet<&str> = described.values.keys().map(String::as_str).collect();
    if params.whole_set
        && let Some(set) = described.set.as_deref()
    {
        names.extend(set.parameters.iter().map(|p| p.name.as_str()));
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
    let hits = page(ranked, params.offset, limit)
        .into_iter()
        .map(|(_, name)| described.brief(name))
        .collect();
    Ok(ParamSearchResult {
        described_by: described.by(engine),
        hits,
        total,
    })
}

fn get(engine: &Engine, params: &ParamGetParams) -> Result<ParamGetResult, ApiError> {
    if params.names.is_empty() || params.names.len() > MAX_NAMES {
        return Err(invalid(format!("ask for 1 to {MAX_NAMES} parameters")));
    }
    let described = Described::of(engine, which_vehicle(engine, params.vehicle_id.as_deref())?);
    let parameters = params
        .names
        .iter()
        .map(|name| {
            let name = name.trim().to_uppercase();
            ParamDetail {
                value: described.values.get(&name).copied(),
                default: described.defaults.get(&name).copied(),
                changed: described.changed(&name),
                definition: described.definition(&name).map(Into::into),
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
    let described = Described::of(engine, which_vehicle(engine, params.vehicle_id.as_deref())?);
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
        });
    }
    let limit = params.limit.unwrap_or(LIST_AT_ONCE).clamp(1, 500);
    let parameters = page(names, params.offset, limit)
        .into_iter()
        .map(|name| described.brief(name))
        .collect();
    Ok(ParamListResult {
        described_by: described.by(engine),
        parameters,
        groups: Vec::new(),
        total,
    })
}

/// A `.param` file's values, and the numbers of the lines it could not read.
fn read_param_file(text: &str) -> (BTreeMap<String, f64>, Vec<u32>) {
    let mut values = BTreeMap::new();
    let mut unread = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split(|c: char| c == ',' || c.is_whitespace());
        let name = parts.next().unwrap_or_default().trim();
        let value = parts
            .find(|p| !p.is_empty())
            .and_then(|v| v.trim().parse::<f64>().ok());
        match value {
            Some(value)
                if !name.is_empty()
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
    let described = Described::of(engine, which_vehicle(engine, params.vehicle_id.as_deref())?);
    // A file is usually a few values; a vehicle or a snapshot is all of them.
    let (theirs, whole, unread_lines) = match (
        params.text.as_deref(),
        params.other_vehicle_id.as_deref(),
        params.snapshot_id.as_deref(),
    ) {
        (Some(text), None, None) => {
            let (values, unread) = read_param_file(text);
            (values, false, unread)
        }
        (None, Some(other), None) => {
            let other = which_vehicle(engine, Some(other))?;
            (Described::of(engine, other).values, true, Vec::new())
        }
        (None, None, Some(id)) => {
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
        _ => return Err(invalid("give one of text, otherVehicleId or snapshotId")),
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

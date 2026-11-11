//! Saved parameter sets (S217, ADR-0037 V19): all of a vehicle's
//! parameters exactly as read, under a name, kept as a `.param` file in its
//! Files on this computer and never synced to the account. A set is only
//! ever saved, applied, renamed or deleted. Applying one saves the vehicle's
//! values as "Before <set>" first, then writes every parameter that
//! differs through the ordinary parameter path, with the reason "Applied
//! <set>", so switching back is one more apply.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use uxduck_contracts::dto::param_definitions::{
    ParamSetApplyResult, ParamSetExportResult, ParamSetRenameParams, ParamSetsResult, RefusedParam,
    SavedParams as SavedParamsDto, SavedParamsOrigin as OriginDto, SkipReason, SkippedParam,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::history::{Author, HistoryEvent};
use uxduck_core::domain::parameter::{
    MAX_SAVED_PARAMS_TEXT, ParamKind, SavedParams, SavedParamsOrigin, file_value, parameter_id,
    same_value,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::Actor;

use super::queries::{read_param_file, which_vehicle};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Where a set's file is kept in the vehicle's Files.
const FOLDER: &str = "/UXDuck/Parameters";

/// A saved set being applied: the writes not answered yet, and how the
/// others went.
#[derive(Debug)]
pub(crate) struct Applying {
    reply: Option<(ConnectionId, Value)>,
    author: Author,
    vehicle_id: String,
    set_id: String,
    set_name: String,
    waiting: BTreeSet<String>,
    result: ParamSetApplyResult,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn no_set(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no saved parameter set {id}"))
}

/// A UTC date and time, `2026-10-23 14:05`, from microseconds since 1970.
pub(crate) fn date_time(us: u64) -> String {
    let days = i64::try_from(us / 86_400_000_000).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let minutes = us / 60_000_000 % 1440;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// A name for a file: letters, digits and dashes.
fn safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut out = String::new();
    for part in safe.split('-').filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(part);
    }
    if out.is_empty() { "set".into() } else { out }
}

fn clean(text: Option<&str>, what: &str) -> Result<Option<String>, ApiError> {
    let text = text.map(str::trim).filter(|t| !t.is_empty());
    if text.is_some_and(|t| t.chars().count() > MAX_SAVED_PARAMS_TEXT) {
        return Err(invalid(format!(
            "a saved set's {what} is at most {MAX_SAVED_PARAMS_TEXT} characters"
        )));
    }
    Ok(text.map(str::to_owned))
}

/// A set as a `.param` file: `NAME,value`, sorted, with a header.
fn file_text(vehicle: &Vehicle, set: &SavedParams) -> String {
    use std::fmt::Write;
    let mut text = format!(
        "# {}: {}, saved by UXDuck {} UTC{}\n",
        vehicle.name,
        set.name,
        date_time(set.saved_at_us),
        set.firmware
            .as_deref()
            .map_or_else(String::new, |f| format!(", firmware {f}"))
    );
    for (name, value) in &set.values {
        let _ = writeln!(text, "{name},{}", file_value(*value));
    }
    text
}

/// One write an apply makes: the name, its kind, and the value it goes to.
type Change = (String, ParamKind, f64);

/// What applying a set writes, and what it skips.
fn changes(
    engine: &Engine,
    vehicle: &Vehicle,
    set: &SavedParams,
) -> (Vec<Change>, Vec<SkippedParam>) {
    let helicopter = vehicle.vehicle_type == "Helicopter";
    let definitions = engine.vehicle_definitions(&vehicle.id);
    let mut writes = Vec::new();
    let mut skipped = Vec::new();
    for (name, to) in &set.values {
        let skip = |why| SkippedParam {
            name: name.clone(),
            why,
        };
        if name.starts_with("STAT_") {
            skipped.push(skip(SkipReason::Statistic));
            continue;
        }
        let Some(now) = engine
            .state
            .parameters
            .get(&parameter_id(&vehicle.id, name))
        else {
            skipped.push(skip(SkipReason::NotOnVehicle));
            continue;
        };
        if same_value(now.value, *to) {
            continue;
        }
        if definitions
            .as_ref()
            .and_then(|d| d.find(name, helicopter))
            .is_some_and(|d| d.read_only)
        {
            skipped.push(skip(SkipReason::ReadOnly));
            continue;
        }
        writes.push((name.clone(), now.kind, *to));
    }
    (writes, skipped)
}

fn origin(origin: SavedParamsOrigin) -> OriginDto {
    match origin {
        SavedParamsOrigin::Saved => OriginDto::Saved,
        SavedParamsOrigin::Imported => OriginDto::Imported,
        SavedParamsOrigin::Before => OriginDto::Before,
        SavedParamsOrigin::Firmware => OriginDto::Firmware,
        SavedParamsOrigin::Tuning => OriginDto::Tuning,
    }
}

fn dto(engine: &Engine, set: &SavedParams) -> SavedParamsDto {
    let differ = engine
        .vehicle(&set.vehicle_id)
        .filter(|v| {
            let prefix = format!("{}/", v.id);
            engine
                .state
                .parameters
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(id, _)| id.starts_with(&prefix))
        })
        .map(|v| u32::try_from(changes(engine, v, set).0.len()).unwrap_or(u32::MAX));
    SavedParamsDto {
        id: set.id.clone(),
        vehicle_id: set.vehicle_id.clone(),
        name: set.name.clone(),
        note: (!set.note.is_empty()).then(|| set.note.clone()),
        origin: origin(set.origin),
        saved_by: set.saved_by.clone(),
        saved_at_us: set.saved_at_us,
        firmware: set.firmware.clone(),
        count: u32::try_from(set.values.len()).unwrap_or(u32::MAX),
        differ,
        file: set.file.clone(),
    }
}

/// The vehicle's sets, newest first.
fn sets_of<'a>(engine: &'a Engine, vehicle: &Vehicle) -> Vec<&'a SavedParams> {
    let mut sets: Vec<&SavedParams> = engine
        .state
        .saved_params
        .values()
        .filter(|s| s.vehicle_id == vehicle.id || vehicle.aliases.contains(&s.vehicle_id))
        .collect();
    sets.sort_by_key(|s| std::cmp::Reverse(s.saved_at_us));
    sets
}

fn set_of(engine: &Engine, id: &str) -> Result<SavedParams, ApiError> {
    engine
        .state
        .saved_params
        .get(id)
        .cloned()
        .ok_or_else(|| no_set(id))
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let need = match &request {
        ClientRequest::ParamSets(_) | ClientRequest::ParamSetExport(_) => Capability::Read,
        _ => Capability::Config,
    };
    if !granted.contains(&need) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            need.denied_message(),
        ));
    }
    match request {
        ClientRequest::ParamSets(params) => {
            let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
            to_value(&ParamSetsResult {
                sets: sets_of(engine, &vehicle)
                    .into_iter()
                    .map(|s| dto(engine, s))
                    .collect(),
            })
        }
        ClientRequest::ParamSetSave(params) => {
            let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
            let name = clean(params.name.as_deref(), "name")?;
            let note = clean(params.note.as_deref(), "note")?.unwrap_or_default();
            let author = engine.author(connection);
            let actor = engine.actor(connection);
            let set = if params.tuning {
                engine.tuning_snapshot(&vehicle, (name, note), &author, actor)?
            } else if let Some(text) = params.text.as_deref() {
                let (values, _) = read_param_file(text);
                if values.is_empty() {
                    return Err(invalid(
                        "The file has no parameters in it: each line is NAME,value.",
                    ));
                }
                let name = name.unwrap_or_else(|| "Imported".into());
                engine.keep_params(
                    &vehicle,
                    (name, note, SavedParamsOrigin::Imported),
                    values,
                    &author,
                    actor,
                    Some(text),
                )
            } else {
                let now = engine.clock.now_us();
                let name = name.unwrap_or_else(|| format!("Saved {}", date_time(now)));
                engine.save_vehicle_params(
                    &vehicle,
                    (name, note),
                    SavedParamsOrigin::Saved,
                    &author,
                    actor,
                )?
            };
            to_value(&dto(engine, &set))
        }
        ClientRequest::ParamSetRename(params) => rename(engine, connection, &params),
        ClientRequest::ParamSetRemove(params) => {
            let set = set_of(engine, &params.id)?;
            if let Some(file) = &set.file_id {
                engine.forget_copy(file);
            }
            let actor = engine.actor(connection);
            engine.commit_as(
                actor,
                uxduck_core::events::Change::SavedParamsRemoved { id: set.id },
            );
            Ok(json!({}))
        }
        ClientRequest::ParamSetExport(params) => export(engine, &params.id),
        ClientRequest::ParamSetApply(params) => apply(engine, connection, &params),
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a saved parameter set method",
        )),
    }
}

/// `param/setRename`: a new name, and a new note when one is given.
fn rename(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &ParamSetRenameParams,
) -> Result<Value, ApiError> {
    let mut set = set_of(engine, &params.id)?;
    let name =
        clean(Some(&params.name), "name")?.ok_or_else(|| invalid("a saved set needs a name"))?;
    let note = match params.note.as_deref() {
        Some(note) => Some(clean(Some(note), "note")?.unwrap_or_default()),
        None => None,
    };
    let renamed = name != set.name;
    if !renamed && note.as_ref().is_none_or(|n| *n == set.note) {
        return to_value(&dto(engine, &set));
    }
    set.name = name;
    if let Some(note) = note {
        set.note = note;
    }
    set.version += 1;
    let actor = engine.actor(connection);
    // Its file takes the new name, so Files says which it is.
    if renamed && let Some(vehicle) = engine.vehicle(&set.vehicle_id).cloned() {
        let old = set.file_id.take();
        set.file = None;
        engine.write_set_file(&vehicle, &mut set, None);
        if let Some(old) = old {
            engine.forget_copy(&old);
        }
    }
    engine.commit_as(
        actor,
        uxduck_core::events::Change::SavedParamsUpserted { set: set.clone() },
    );
    to_value(&dto(engine, &set))
}

/// `param/setExport`: the set as a `.param` file, named for the vehicle.
fn export(engine: &Engine, id: &str) -> Result<Value, ApiError> {
    let set = set_of(engine, id)?;
    let vehicle = engine.vehicle(&set.vehicle_id).cloned();
    let vehicle_name = vehicle.as_ref().map_or("vehicle", |v| v.name.as_str());
    let text = match &vehicle {
        Some(vehicle) => file_text(vehicle, &set),
        None => set.values.iter().fold(String::new(), |mut text, (n, v)| {
            use std::fmt::Write;
            let _ = writeln!(text, "{n},{}", file_value(*v));
            text
        }),
    };
    to_value(&ParamSetExportResult {
        name: format!("{}-{}.param", safe(vehicle_name), safe(&set.name)),
        text,
    })
}

/// `param/setApply`: "Before <set>" saved, then each difference written as
/// a person's change, answered once all are.
fn apply(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &uxduck_contracts::dto::param_definitions::ParamSetApplyParams,
) -> Result<Value, ApiError> {
    let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
    let set = set_of(engine, &params.id)?;
    if let Some(why) = engine.not_writable(&vehicle.link_id) {
        return Err(invalid(format!("Can't write to {}: {why}.", vehicle.name)));
    }
    let (writes, skipped) = changes(engine, &vehicle, &set);
    if writes.is_empty() {
        return to_value(&ParamSetApplyResult {
            before: None,
            written: Vec::new(),
            refused: Vec::new(),
            skipped,
            reboot_required: Vec::new(),
        });
    }
    if vehicle.presence != Presence::Online && !params.confirm {
        let presence = format!("{:?}", vehicle.presence).to_lowercase();
        return Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            format!(
                "applying {} needs confirming: {} is {presence}",
                set.name, vehicle.name
            ),
        ));
    }
    let author = engine.author(connection);
    let actor = engine.actor(connection);
    // Switching back is one more apply. A tuning snapshot is a jump
    // (ADR-0054 T5): the tuning values before it are a snapshot too, unless
    // one already holds them.
    let jump = set.origin == SavedParamsOrigin::Tuning;
    let before = if jump {
        engine.tuning_snapshot(
            &vehicle,
            (Some(format!("Before {}", set.name)), String::new()),
            &author,
            actor.clone(),
        )?
    } else {
        engine.save_vehicle_params(
            &vehicle,
            (format!("Before {}", set.name), String::new()),
            SavedParamsOrigin::Before,
            &author,
            actor.clone(),
        )?
    };
    let before = dto(engine, &before);
    let mut refused = Vec::new();
    let mut sending = Vec::new();
    for (name, kind, to) in writes {
        if let Some(detail) = kind.refuses(to) {
            refused.push(RefusedParam { name, detail });
        } else {
            sending.push((name, kind, to));
        }
    }
    // As one file, quicker over a radio, when ArduPilot takes them so.
    let by_file = vehicle.autopilot == "Ardupilotmega" && sending.len() >= 8;
    let id = format!("ap{}", &engine.random.token()[..16]);
    let mut applying = Applying {
        reply: engine.replying_to.take(),
        author: author.clone(),
        vehicle_id: vehicle.id.clone(),
        set_id: set.id.clone(),
        set_name: set.name.clone(),
        waiting: sending.iter().map(|(n, ..)| n.clone()).collect(),
        result: ParamSetApplyResult {
            before: Some(before),
            written: Vec::new(),
            refused,
            skipped,
            reboot_required: Vec::new(),
        },
    };
    if sending.is_empty() {
        engine.replying_to = applying.reply.take();
        let result = applying.result.clone();
        engine.finish_apply(applying);
        return to_value(&result);
    }
    engine.transfers.applies.insert(id.clone(), applying);
    let reason = if jump {
        format!("Jumped to {}", set.name)
    } else {
        format!("Applied {}", set.name)
    };
    for (name, kind, to) in sending {
        engine.write_parameter_in(
            actor.clone(),
            author.clone(),
            &reason,
            (&vehicle.id, &name, to),
            kind,
            Some((&id, by_file)),
        );
    }
    Ok(Value::Null)
}

impl Engine {
    /// A tuning snapshot (ADR-0054 T5): the vehicle's tuning parameters as
    /// they are now, or the snapshot already kept with exactly these values,
    /// so a snapshot is never taken twice. Unnamed, the vehicle's first is
    /// "Before tuning".
    pub(crate) fn tuning_snapshot(
        &mut self,
        vehicle: &Vehicle,
        (name, note): (Option<String>, String),
        author: &Author,
        actor: Actor,
    ) -> Result<SavedParams, ApiError> {
        let values = crate::tuning::tuning_values(self, vehicle);
        if values.is_empty() {
            return Err(invalid(format!(
                "{} has no tuning parameters to keep: download its parameters first.",
                vehicle.name
            )));
        }
        let snapshots: Vec<&SavedParams> = sets_of(self, vehicle)
            .into_iter()
            .filter(|s| s.origin == SavedParamsOrigin::Tuning)
            .collect();
        let same = snapshots.iter().find(|s| {
            s.values.len() == values.len()
                && values
                    .iter()
                    .all(|(n, v)| s.values.get(n).is_some_and(|w| same_value(*v, *w)))
        });
        if let Some(same) = same {
            return Ok((*same).clone());
        }
        let name = name.unwrap_or_else(|| {
            if snapshots.is_empty() {
                "Before tuning".into()
            } else {
                format!("Snapshot {}", date_time(self.clock.now_us()))
            }
        });
        Ok(self.keep_params(
            vehicle,
            (name, note, SavedParamsOrigin::Tuning),
            values,
            author,
            actor,
            None,
        ))
    }

    /// Saves the vehicle's parameters as they are now.
    fn save_vehicle_params(
        &mut self,
        vehicle: &Vehicle,
        (name, note): (String, String),
        origin: SavedParamsOrigin,
        author: &Author,
        actor: Actor,
    ) -> Result<SavedParams, ApiError> {
        let prefix = format!("{}/", vehicle.id);
        let values: BTreeMap<String, f64> = self
            .state
            .parameters
            .range(prefix.clone()..)
            .take_while(|(id, _)| id.starts_with(&prefix))
            .map(|(_, p)| (p.name.clone(), p.value))
            .collect();
        if values.is_empty() {
            return Err(invalid(format!(
                "Download {}'s parameters before saving them.",
                vehicle.name
            )));
        }
        Ok(self.keep_params(vehicle, (name, note, origin), values, author, actor, None))
    }

    /// Keeps a set, and its file in the vehicle's Files: the file's own text
    /// for an import, as it came.
    fn keep_params(
        &mut self,
        vehicle: &Vehicle,
        (name, note, origin): (String, String, SavedParamsOrigin),
        values: BTreeMap<String, f64>,
        author: &Author,
        actor: Actor,
        text: Option<&str>,
    ) -> SavedParams {
        let mut set = SavedParams {
            id: format!("ps{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            name,
            note,
            origin,
            saved_by: author
                .operator_name
                .clone()
                .unwrap_or_else(|| author.client_name.clone()),
            saved_at_us: self.clock.now_us(),
            firmware: (origin != SavedParamsOrigin::Imported)
                .then(|| vehicle.firmware.clone())
                .flatten(),
            values,
            file: None,
            file_id: None,
            version: 1,
        };
        self.write_set_file(vehicle, &mut set, text);
        self.commit_as(
            actor,
            uxduck_core::events::Change::SavedParamsUpserted { set: set.clone() },
        );
        set
    }

    /// Writes a set's file under a name no other set of the vehicle has.
    /// Without files (a browser-only UXDuck) the set is kept all the same.
    fn write_set_file(&mut self, vehicle: &Vehicle, set: &mut SavedParams, text: Option<&str>) {
        let taken: BTreeSet<String> = self
            .state
            .saved_params
            .values()
            .filter(|s| s.vehicle_id == vehicle.id && s.id != set.id)
            .filter_map(|s| s.file.clone())
            .collect();
        let stem = safe(&set.name);
        let file = std::iter::once(format!("{stem}.param"))
            .chain((2..=taken.len() + 1).map(|n| format!("{stem}-{n}.param")))
            .find(|f| !taken.contains(f))
            .unwrap_or_else(|| format!("{stem}-{}.param", set.id));
        let text = text.map_or_else(|| file_text(vehicle, set), str::to_owned);
        if let Ok(id) = self.keep_file(&vehicle.id, &format!("{FOLDER}/{file}"), text.as_bytes()) {
            set.file = Some(file);
            set.file_id = Some(id);
        }
    }

    /// One of a set's writes answered: `refused` says why it was not taken.
    pub(crate) fn applied_one(&mut self, id: &str, name: &str, refused: Option<String>) {
        let Some(applying) = self.transfers.applies.get_mut(id) else {
            return;
        };
        applying.waiting.remove(name);
        if let Some(detail) = refused {
            applying.result.refused.push(RefusedParam {
                name: name.to_owned(),
                detail,
            });
        } else {
            applying.result.written.push(name.to_owned());
            let vehicle_id = applying.vehicle_id.clone();
            if self.param_facts(&vehicle_id, name).1
                && let Some(applying) = self.transfers.applies.get_mut(id)
            {
                applying.result.reboot_required.push(name.to_owned());
            }
        }
        if self
            .transfers
            .applies
            .get(id)
            .is_some_and(|a| a.waiting.is_empty())
            && let Some(applying) = self.transfers.applies.remove(id)
        {
            self.finish_apply(applying);
        }
    }

    /// Every write answered: the history says what the apply did, and
    /// whoever asked hears.
    fn finish_apply(&mut self, applying: Applying) {
        let result = &applying.result;
        let count = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let mut text = format!(
            "Applied {}: {} written",
            applying.set_name,
            count(result.written.len(), "change", "changes")
        );
        {
            use std::fmt::Write;
            if !result.refused.is_empty() {
                let _ = write!(text, ", {} refused", result.refused.len());
            }
            if !result.skipped.is_empty() {
                let _ = write!(text, ", {} skipped", result.skipped.len());
            }
        }
        self.record_by(
            &applying.vehicle_id,
            HistoryEvent::ParametersApplied {
                set_id: applying.set_id.clone(),
                name: applying.set_name.clone(),
                text,
            },
            applying.author.clone(),
        );
        if let Some((connection, request_id)) = applying.reply {
            let frame = json!({ "jsonrpc": "2.0", "id": request_id, "result": applying.result });
            self.send(connection, &frame);
        }
    }

    /// The vehicle's values kept as a set before a firmware install (V10),
    /// in the file the install wrote.
    pub(crate) fn kept_before_firmware(&mut self, vehicle: &Vehicle, file: &str, file_id: String) {
        let prefix = format!("{}/", vehicle.id);
        let values: BTreeMap<String, f64> = self
            .state
            .parameters
            .range(prefix.clone()..)
            .take_while(|(id, _)| id.starts_with(&prefix))
            .map(|(_, p)| (p.name.clone(), p.value))
            .collect();
        let now = self.clock.now_us();
        let set = SavedParams {
            id: format!("ps{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            name: format!(
                "Before firmware{}",
                vehicle
                    .firmware
                    .as_deref()
                    .map_or_else(String::new, |f| format!(" (was {f})"))
            ),
            note: String::new(),
            origin: SavedParamsOrigin::Firmware,
            saved_by: "UXDuck".into(),
            saved_at_us: now,
            firmware: vehicle.firmware.clone(),
            values,
            file: Some(file.to_owned()),
            file_id: Some(file_id),
            version: 1,
        };
        self.commit_as(
            crate::history::actor(),
            uxduck_core::events::Change::SavedParamsUpserted { set },
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_and_dates_read_plainly() {
        assert_eq!(super::date_time(0), "1970-01-01 00:00");
        assert_eq!(
            super::date_time(1_792_713_600_000_000 + 14 * 3_600_000_000 + 5 * 60_000_000),
            "2026-10-23 14:05"
        );
        assert_eq!(super::safe("Field tune"), "Field-tune");
        assert_eq!(super::safe("  High RTL!! "), "High-RTL");
        assert_eq!(super::safe("…"), "set");
    }
}

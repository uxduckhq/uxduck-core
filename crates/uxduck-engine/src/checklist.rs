//! A vehicle's checklist (S193, ADR-0038 K7 to K11, K14).
//!
//! Each vehicle has its own steps: manual ones a person ticks and rules
//! over a telemetry item or a parameter, held for some seconds. They are
//! checks in the vehicle's preflight list (`checklist:<id>`), so the gate
//! holds arming for a blocking one that does not pass. Copying from another
//! vehicle, a checklist file or a pasted list makes an independent copy
//! (K7). Only operator clients tick and clear ticks, and nothing clears a
//! tick by itself (K9); writing steps is `config`, so an agent without it
//! asks the operator first (K11). Arming writes the checklist as it stood
//! to the vehicle's history and its flight (K14), and arming that UXDuck
//! did not send while a blocking check failed raises a warning (K10).

use serde_json::Value;
use uxduck_contracts::dto::checklist::{
    ChecklistCopyParams, ChecklistFile as FileDto, ChecklistTickParams, ChecklistUpdateParams,
    StepChange, StepInput, StepKind as KindDto,
};
use uxduck_contracts::dto::preflight::{Preflight as PreflightDto, PreflightParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::checklist::{
    Checklist, ChecklistFile, ChecklistRecord, FILE_FORMAT, MAX_STEPS, Op, RecordStep, Rule, Step,
    StepDef, StepKind, Tick, steps_from_markdown,
};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::ReceiptStatus;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::{parameter_id, same_value};
use uxduck_core::domain::preflight::{Check, CheckRef, CheckSource, CheckStatus, ParamOffer};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::Change;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A telemetry item older than this is not read by a rule.
const FRESH_US: u64 = 10_000_000;
/// A receipt this recent is the arming UXDuck sent.
const SENT_WITHIN_US: u64 = 15_000_000;
/// Parameters that say how many cells the battery has, the first set
/// above 0 counting: PX4's, then ArduPilot's on-screen display's.
const CELL_COUNT_PARAMS: [&str; 4] = [
    "BAT1_N_CELLS",
    "BAT_N_CELLS",
    "OSD_CELL_COUNT",
    "MSP_OSD_NCELLS",
];
/// Parameters that say the battery's capacity in mAh.
const CAPACITY_PARAMS: [&str; 2] = ["BATT_CAPACITY", "BAT1_CAPACITY"];
/// The item for the battery's voltage per cell (K8).
pub(crate) const CELL_ITEM: &str = "battery.cell_v";
/// The item for what is left in the battery.
pub(crate) const LEFT_ITEM: &str = "battery.remaining_mah";

fn no_vehicle(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}"))
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn denied(capability: Capability) -> ApiError {
    ApiError::new(ErrorKind::CapabilityDenied, capability.denied_message())
}

fn only_a_person(what: &str) -> ApiError {
    ApiError::new(
        ErrorKind::CapabilityDenied,
        format!("only a person {what}, from UXDuck or its CLI"),
    )
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::ChecklistUpdate(params) => {
            if !granted.contains(&Capability::Config) {
                return Err(denied(Capability::Config));
            }
            update(engine, connection, params)
        }
        ClientRequest::ChecklistCopy(params) => {
            if !granted.contains(&Capability::Config) {
                return Err(denied(Capability::Config));
            }
            copy(engine, connection, params)
        }
        // Ticks are a person's (K9, K11): never an agent's, a plugin's or
        // an approval's, so this is not escalated.
        ClientRequest::ChecklistTick(params) => {
            if !operator {
                return Err(only_a_person("ticks a checklist step"));
            }
            tick(engine, connection, &params)
        }
        ClientRequest::ChecklistReset(PreflightParams { vehicle_id }) => {
            if !operator {
                return Err(only_a_person("clears a checklist's ticks"));
            }
            reset(engine, connection, &vehicle_id)
        }
        ClientRequest::ChecklistExport(PreflightParams { vehicle_id }) => {
            if !granted.contains(&Capability::Read) {
                return Err(denied(Capability::Read));
            }
            let vehicle = engine
                .vehicle(&vehicle_id)
                .cloned()
                .ok_or_else(|| no_vehicle(&vehicle_id))?;
            let checklist = engine.checklist_of(&vehicle.id);
            let file = checklist.to_file(Some(format!("{}'s checklist", vehicle.name)));
            to_value(&FileDto::from(&file))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a checklist method")),
    }
}

/// A step as written, checked: a rule names its operator and value.
fn step_def(input: StepInput) -> Result<StepDef, ApiError> {
    let title = input.title.trim().to_owned();
    if input.kind == KindDto::Rule {
        if input.op.is_none() {
            return Err(invalid(format!(
                "“{title}”: a rule needs an operator: >=, <=, >, <, == or !="
            )));
        }
        if input.value.is_none() {
            return Err(invalid(format!("“{title}”: a rule needs a value")));
        }
    }
    StepDef::from(input).checked().map_err(|e| invalid(e.0))
}

/// One step changed: what is given changes, the rest stays.
fn changed(step: &Step, change: StepChange) -> Result<Step, ApiError> {
    let before = step.definition();
    let kind = change.kind.map_or(before.kind, Into::into);
    let rule = before.rule.clone();
    let mut input = StepInput {
        kind: kind.into(),
        title: change.title.unwrap_or_else(|| before.title.clone()),
        severity: Some(change.severity.map_or(before.severity, Into::into).into()),
        item: rule.as_ref().and_then(|r| r.item.clone()),
        param: rule.as_ref().and_then(|r| r.param.clone()),
        op: rule.as_ref().map(|r| r.op.into()),
        value: rule.as_ref().map(|r| r.value),
        unit: rule.as_ref().and_then(|r| r.unit.clone()),
        hold_s: rule.as_ref().and_then(|r| r.hold_s),
    };
    if let Some(item) = change.item {
        input.item = Some(item);
        input.param = None;
        // The unit follows the new item's key, unless given.
        if change.unit.is_none() {
            input.unit = None;
        }
    }
    if let Some(param) = change.param {
        input.param = Some(param);
        input.item = None;
        if change.unit.is_none() {
            input.unit = None;
        }
    }
    if let Some(op) = change.op {
        input.op = Some(op);
    }
    if let Some(value) = change.value {
        input.value = Some(value);
    }
    if let Some(unit) = change.unit {
        input.unit = unit;
    }
    if let Some(hold) = change.hold_s {
        input.hold_s = hold;
    }
    if kind == StepKind::Manual {
        input.item = None;
        input.param = None;
        input.op = None;
        input.value = None;
        input.unit = None;
        input.hold_s = None;
    }
    let def = step_def(input)?;
    // A tick was for the step's words: new words, or a rule, need a new
    // tick from a person.
    let tick = if def.kind == StepKind::Manual && def.title == before.title {
        step.tick.clone()
    } else {
        None
    };
    Ok(Step {
        id: step.id.clone(),
        kind: def.kind,
        title: def.title,
        severity: def.severity,
        rule: def.rule,
        tick,
    })
}

fn update(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ChecklistUpdateParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let before = engine.checklist_of(&vehicle.id);
    let mut steps = before.steps.clone();
    for id in &params.remove {
        let at = steps
            .iter()
            .position(|s| &s.id == id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no step {id}")))?;
        steps.remove(at);
    }
    for change in params.change {
        let at = steps
            .iter()
            .position(|s| s.id == change.id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no step {}", change.id)))?;
        steps[at] = changed(&steps[at], change)?;
    }
    for input in params.add {
        let def = step_def(input)?;
        let id = engine.new_step_id(&steps);
        steps.push(new_step(id, def));
    }
    if let Some(order) = params.order {
        let mut placed: Vec<Step> = Vec::with_capacity(steps.len());
        for id in &order {
            let at = steps
                .iter()
                .position(|s| &s.id == id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no step {id}")))?;
            placed.push(steps.remove(at));
        }
        placed.append(&mut steps);
        steps = placed;
    }
    if steps.len() > MAX_STEPS {
        return Err(invalid(format!(
            "a checklist holds at most {MAX_STEPS} steps"
        )));
    }
    let copied_from = before.copied_from.clone();
    engine.keep_checklist(connection, &vehicle, &before, steps, copied_from)
}

fn new_step(id: String, def: StepDef) -> Step {
    Step {
        id,
        kind: def.kind,
        title: def.title,
        severity: def.severity,
        rule: def.rule,
        tick: None,
    }
}

/// A checklist file's JSON, read and checked (K7): a newer format is
/// refused by name.
fn read_file(value: Value) -> Result<ChecklistFile, ApiError> {
    let format = value
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let file: FileDto = if format == FILE_FORMAT {
        serde_json::from_value(value)
            .map_err(|e| invalid(format!("the checklist file is not as expected: {e}")))?
    } else {
        // Checked below for its words: newer, or not a checklist file.
        FileDto {
            format,
            name: None,
            steps: Vec::new(),
        }
    };
    let steps = file
        .steps
        .into_iter()
        .map(step_def)
        .collect::<Result<Vec<_>, _>>()?;
    ChecklistFile {
        format: file.format,
        name: file.name,
        steps,
    }
    .checked()
    .map_err(|e| invalid(e.0))
}

fn copy(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ChecklistCopyParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let sources = usize::from(params.from_vehicle_id.is_some())
        + usize::from(params.file.is_some())
        + usize::from(params.markdown.is_some());
    if sources != 1 {
        return Err(invalid("copy from one of fromVehicleId, file or markdown"));
    }
    let named = params
        .source_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    let (defs, from): (Vec<StepDef>, String) = if let Some(other) = &params.from_vehicle_id {
        let other = engine
            .vehicle(other)
            .cloned()
            .ok_or_else(|| no_vehicle(other))?;
        if other.id == vehicle.id {
            return Err(invalid("that is this vehicle's own checklist"));
        }
        let theirs = engine.checklist_of(&other.id);
        if theirs.steps.is_empty() {
            return Err(invalid(format!("{} has no checklist to copy", other.name)));
        }
        (
            theirs.steps.iter().map(Step::definition).collect(),
            format!("{}'s checklist", other.name),
        )
    } else if let Some(file) = params.file {
        let file = read_file(file)?;
        let from = named
            .clone()
            .or_else(|| file.name.clone())
            .unwrap_or_else(|| "a checklist file".into());
        (file.steps, from)
    } else {
        let text = params.markdown.unwrap_or_default();
        let defs = steps_from_markdown(&text).map_err(|e| invalid(e.0))?;
        (
            defs,
            named.clone().unwrap_or_else(|| "a pasted list".into()),
        )
    };
    let before = engine.checklist_of(&vehicle.id);
    let mut steps = if params.replace {
        Vec::new()
    } else {
        before.steps.clone()
    };
    for def in defs {
        let id = engine.new_step_id(&steps);
        steps.push(new_step(id, def));
    }
    if steps.len() > MAX_STEPS {
        return Err(invalid(format!(
            "that makes {} steps; a checklist holds at most {MAX_STEPS}",
            steps.len()
        )));
    }
    engine.keep_checklist(connection, &vehicle, &before, steps, Some(from))
}

fn tick(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &ChecklistTickParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let before = engine.checklist_of(&vehicle.id);
    let mut steps = before.steps.clone();
    let step = steps
        .iter_mut()
        .find(|s| s.id == params.step_id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no step {}", params.step_id)))?;
    if step.kind == StepKind::Rule {
        return Err(invalid(format!(
            "“{}” is a rule: UXDuck checks it, nobody ticks it",
            step.title
        )));
    }
    step.tick = params.ticked.then(|| Tick {
        by: engine.author(connection),
        at_us: engine.clock.now_us(),
    });
    let copied_from = before.copied_from.clone();
    engine.keep_checklist(connection, &vehicle, &before, steps, copied_from)
}

fn reset(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle_id: &str,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(vehicle_id))?;
    let before = engine.checklist_of(&vehicle.id);
    let mut steps = before.steps.clone();
    for step in &mut steps {
        step.tick = None;
    }
    let copied_from = before.copied_from.clone();
    engine.keep_checklist(connection, &vehicle, &before, steps, copied_from)
}

/// A rule's reading: the value, and what it is called.
struct Reading {
    value: Option<f64>,
    label: String,
}

impl Engine {
    /// A vehicle's checklist, empty when it has none.
    pub(crate) fn checklist_of(&self, vehicle_id: &str) -> Checklist {
        self.state
            .checklists
            .get(vehicle_id)
            .cloned()
            .unwrap_or_else(|| Checklist::empty(vehicle_id))
    }

    fn new_step_id(&mut self, steps: &[Step]) -> String {
        loop {
            let id = format!("st{}", &self.random.token()[..10]);
            if !steps.iter().any(|s| s.id == id) {
                return id;
            }
        }
    }

    /// Journals the checklist when it changed and answers with the checks.
    fn keep_checklist(
        &mut self,
        connection: ConnectionId,
        vehicle: &Vehicle,
        before: &Checklist,
        steps: Vec<Step>,
        copied_from: Option<String>,
    ) -> Result<Value, ApiError> {
        if steps != before.steps || copied_from != before.copied_from {
            let checklist = Checklist {
                vehicle_id: vehicle.id.clone(),
                steps,
                copied_from,
                updated_at_us: self.clock.now_us(),
                version: before.version + 1,
            };
            self.commit(connection, Change::ChecklistUpdated { checklist });
            self.track_rules(&vehicle.id);
        }
        self.review_preflight(&vehicle.id);
        let preflight = self
            .state
            .preflights
            .get(&vehicle.id)
            .ok_or_else(|| no_vehicle(&vehicle.id))?;
        to_value(&PreflightDto::from(preflight))
    }

    /// What a rule reads now: a fresh telemetry item, or a parameter.
    fn reading(&self, vehicle: &Vehicle, rule: &Rule) -> Reading {
        if let Some(name) = &rule.param {
            return Reading {
                value: self
                    .state
                    .parameters
                    .get(&parameter_id(&vehicle.id, name))
                    .map(|p| p.value),
                label: name.clone(),
            };
        }
        let key = rule.item.as_deref().unwrap_or_default();
        let now = self.clock.now_us();
        let item = self
            .telemetry(&vehicle.id)
            .and_then(|t| t.items.iter().find(|i| i.key == key));
        Reading {
            value: item
                .filter(|i| {
                    vehicle.presence == Presence::Online && now.saturating_sub(i.time_us) < FRESH_US
                })
                .and_then(|i| i.value),
            label: item.map_or_else(|| key.to_owned(), |i| i.label.clone()),
        }
    }

    /// Whether a rule's comparison holds now.
    fn rule_holds(&self, vehicle: &Vehicle, rule: &Rule) -> Option<bool> {
        self.reading(vehicle, rule)
            .value
            .map(|v| rule.op.holds(v, rule.value))
    }

    /// Keeps when each held rule of a vehicle began to hold: from telemetry
    /// and every review. Not journaled.
    pub(crate) fn track_rules(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let Some(checklist) = self.state.checklists.get(&vehicle.id) else {
            self.preflight.held_since.remove(&vehicle.id);
            return;
        };
        let now = self.clock.now_us();
        let holding: Vec<(String, bool)> = checklist
            .steps
            .iter()
            .filter_map(|s| {
                let rule = s.rule.as_ref()?;
                rule.hold()?;
                Some((s.id.clone(), self.rule_holds(&vehicle, rule) == Some(true)))
            })
            .collect();
        let since = self
            .preflight
            .held_since
            .entry(vehicle.id.clone())
            .or_default();
        since.retain(|id, _| holding.iter().any(|(h, on)| h == id && *on));
        for (id, on) in holding {
            if on {
                since.entry(id).or_insert(now);
            }
        }
    }

    /// The checks for a vehicle's checklist steps, in its order.
    pub(crate) fn checklist_checks(&self, vehicle: &Vehicle) -> Vec<Check> {
        let Some(checklist) = self.state.checklists.get(&vehicle.id) else {
            return Vec::new();
        };
        checklist
            .steps
            .iter()
            .map(|step| {
                let (status, detail) = match &step.rule {
                    None => match &step.tick {
                        Some(tick) => (CheckStatus::Pass, format!("Ticked by {}.", tick.who())),
                        None => (CheckStatus::ToDo, "For a person to tick.".to_owned()),
                    },
                    Some(rule) => self.rule_outcome(vehicle, &step.id, rule),
                };
                Check {
                    id: step.check_id(),
                    source: CheckSource::Checklist,
                    title: step.title.clone(),
                    severity: step.severity,
                    status,
                    detail: Some(detail),
                    fix: None,
                    plugin: None,
                }
            })
            .collect()
    }

    /// A rule's status and words, without the live value: the list is
    /// journaled when its words change (S192).
    fn rule_outcome(&self, vehicle: &Vehicle, step_id: &str, rule: &Rule) -> (CheckStatus, String) {
        let reading = self.reading(vehicle, rule);
        let target = rule.value_words();
        let label = &reading.label;
        let Some(value) = reading.value else {
            return (
                CheckStatus::Unknown,
                if rule.param.is_some() {
                    format!("UXDuck hasn't read {label} from {}.", vehicle.name)
                } else if rule.item.as_deref() == Some(CELL_ITEM) {
                    format!(
                        "{} reports no cells, and no cell count is set (MSP_OSD_NCELLS or OSD_CELL_COUNT, BAT1_N_CELLS on PX4).",
                        vehicle.name
                    )
                } else {
                    format!("{} isn't reporting {label} now.", vehicle.name)
                },
            );
        };
        if !rule.op.holds(value, rule.value) {
            return (
                CheckStatus::Fail,
                format!("{label} is {} {target}.", rule.op.failing_words()),
            );
        }
        let holds = format!("{label} is {} {target}", rule.op.words());
        let Some(hold) = rule.hold() else {
            return (CheckStatus::Pass, format!("{holds}."));
        };
        let held_us = self
            .preflight
            .held_since
            .get(&vehicle.id)
            .and_then(|s| s.get(step_id))
            .map_or(0, |since| self.clock.now_us().saturating_sub(*since));
        let seconds = uxduck_core::domain::parameter::show_value(hold);
        #[allow(
            clippy::cast_precision_loss,
            reason = "microseconds held, far below 2^52"
        )]
        if held_us as f64 >= hold * 1e6 {
            (CheckStatus::Pass, format!("{holds}, held {seconds} s."))
        } else {
            (
                CheckStatus::Waiting,
                format!("{holds}; it passes once that holds {seconds} s."),
            )
        }
    }

    /// Steps ticked before the vehicle's last flight began (K9).
    pub(crate) fn flown_since(&self, vehicle: &Vehicle) -> Vec<String> {
        let Some(checklist) = self.state.checklists.get(&vehicle.id) else {
            return Vec::new();
        };
        if checklist.steps.iter().all(|s| s.tick.is_none()) {
            return Vec::new();
        }
        let last = self
            .state
            .flights
            .values()
            .filter(|f| f.vehicle_id == vehicle.id || vehicle.aliases.contains(&f.vehicle_id))
            .map(|f| f.armed_at_us)
            .max();
        let Some(last) = last else {
            return Vec::new();
        };
        checklist
            .steps
            .iter()
            .filter(|s| s.tick.as_ref().is_some_and(|t| t.at_us < last))
            .map(|s| s.id.clone())
            .collect()
    }

    /// Rules the vehicle can hold itself with one of its own parameters
    /// (K10), when its firmware has that parameter.
    pub(crate) fn param_offers(&self, vehicle: &Vehicle) -> Vec<ParamOffer> {
        let Some(checklist) = self.state.checklists.get(&vehicle.id) else {
            return Vec::new();
        };
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        };
        checklist
            .steps
            .iter()
            .filter_map(|step| {
                let rule = step.rule.as_ref()?;
                if !matches!(rule.op, Op::Ge | Op::Gt) {
                    return None;
                }
                let name = match rule.item.as_deref()? {
                    "battery.voltage_v" => "BATT_ARM_VOLT",
                    LEFT_ITEM => "BATT_ARM_MAH",
                    _ => return None,
                };
                // Its firmware has it: the vehicle listed it.
                let current = param(name)?;
                let at_least = current > rule.value || same_value(current, rule.value);
                // ArduPilot leaves its battery check out when ARMING_CHECK
                // is 0, or not 1 and without the battery bit; from 4.7,
                // when ARMING_SKIPCHK has that bit (or skips them all).
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "bitmask parameters"
                )]
                let bits = |v: f64| v.max(0.0) as u32;
                let (setting, checks) = match (param("ARMING_SKIPCHK"), param("ARMING_CHECK")) {
                    (Some(skip), _) => ("ARMING_SKIPCHK", skip >= 0.0 && bits(skip) & 256 == 0),
                    (None, Some(check)) => {
                        ("ARMING_CHECK", bits(check) == 1 || bits(check) & 256 != 0)
                    }
                    (None, None) => ("", true),
                };
                Some(ParamOffer {
                    step_id: step.id.clone(),
                    name: name.to_owned(),
                    value: rule.value,
                    current: Some(current),
                    held: at_least && checks,
                    note: (!checks).then(|| {
                        format!(
                            "{setting} leaves the battery out, so {} doesn't use {name}.",
                            vehicle.name
                        )
                    }),
                })
            })
            .collect()
    }

    /// Items the checklist's rules read that the vehicle does not report
    /// as such (K8): the battery per cell, and what is left in it. Added
    /// to telemetry carrying battery readings.
    pub(crate) fn battery_items(&self, update: &VehicleTelemetry) -> Vec<TelemetryItem> {
        let now = self.clock.now_us();
        let latest = self.telemetry(&update.vehicle_id);
        let find = |key: &str| -> Option<&TelemetryItem> {
            update
                .items
                .iter()
                .find(|i| i.key == key)
                .or_else(|| latest.and_then(|t| t.items.iter().find(|i| i.key == key)))
                .filter(|i| now.saturating_sub(i.time_us) < FRESH_US && i.value.is_some())
        };
        let Some(vehicle) = self.vehicle(&update.vehicle_id) else {
            return Vec::new();
        };
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        };
        let mut out = Vec::new();
        let made =
            |key: &str, label: &str, unit: Unit, value: f64, from: &TelemetryItem| TelemetryItem {
                key: key.into(),
                label: label.into(),
                unit,
                value: Some(value),
                text: None,
                source: "checklist".into(),
                component_id: from.component_id,
                time_us: from.time_us,
            };
        // The vehicle's own cells, the lowest of them; else the pack over
        // its cell count.
        let mut cells: Vec<&TelemetryItem> = (1..=14)
            .filter_map(|n| find(&format!("battery.cell_{n}_v")))
            .collect();
        cells.sort_by(|a, b| {
            a.value
                .partial_cmp(&b.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if cells.len() > 1 {
            let lowest = cells[0];
            out.push(made(
                CELL_ITEM,
                "Battery per cell",
                Unit::Volt,
                lowest.value.unwrap_or_default(),
                lowest,
            ));
        } else if let Some(pack) = find("battery.voltage_v")
            && let Some(count) = CELL_COUNT_PARAMS
                .iter()
                .filter_map(|name| param(name))
                .find(|n| *n >= 1.0 && *n <= 24.0)
        {
            out.push(made(
                CELL_ITEM,
                "Battery per cell",
                Unit::Volt,
                pack.value.unwrap_or_default() / count.round(),
                pack,
            ));
        }
        if let Some(used) = find("battery.consumed_mah")
            && let Some(capacity) = CAPACITY_PARAMS
                .iter()
                .filter_map(|name| param(name))
                .find(|c| *c > 0.0)
        {
            out.push(made(
                LEFT_ITEM,
                "Battery left",
                Unit::MilliampHour,
                (capacity - used.value.unwrap_or_default()).max(0.0),
                used,
            ));
        }
        out
    }

    /// The vehicle armed (K14, K10): its checklist as it stood goes to its
    /// history and its flight, with every override; armed while a
    /// blocking check failed, and not by UXDuck, raises a warning.
    pub(crate) fn checklist_armed(&mut self, vehicle: &Vehicle) {
        let now = self.clock.now_us();
        // What it armed with: the list as last evaluated while disarmed.
        let checks: Vec<Check> = self.state.preflights.get(&vehicle.id).map_or_else(
            || {
                let mut disarmed = vehicle.clone();
                disarmed.armed = false;
                self.evaluate_preflight(&disarmed).checks
            },
            |p| p.checks.clone(),
        );
        let gating: Vec<CheckRef> = checks
            .iter()
            .filter(|c| c.gates())
            .map(Check::to_ref)
            .collect();
        let ids: Vec<&String> = std::iter::once(&vehicle.id)
            .chain(vehicle.aliases.iter())
            .collect();
        // The arming UXDuck sent, if it did.
        let sent = self
            .state
            .receipts
            .values()
            .filter(|r| {
                ids.contains(&&r.vehicle_id)
                    && r.command.arms_on_the_ground()
                    && matches!(r.status, ReceiptStatus::Sent | ReceiptStatus::Accepted)
                    && now.saturating_sub(r.created_at_us) < SENT_WITHIN_US
            })
            .max_by_key(|r| r.created_at_us)
            .cloned();
        let checklist = self.checklist_of(&vehicle.id);
        let steps: Vec<RecordStep> = checklist
            .steps
            .iter()
            .map(|step| RecordStep {
                title: step.title.clone(),
                kind: step.kind,
                severity: step.severity,
                status: checks
                    .iter()
                    .find(|c| c.id == step.check_id())
                    .map_or(CheckStatus::Unknown, |c| c.status),
                tick: step.tick.clone(),
                rule: step.rule.clone(),
                value: step
                    .rule
                    .as_ref()
                    .and_then(|rule| self.reading(vehicle, rule).value),
            })
            .collect();
        let (overridden, override_by) = match &sent {
            Some(r) if r.preflight_override.is_some() => {
                (r.preflight.clone(), r.preflight_override.clone())
            }
            _ => (Vec::new(), None),
        };
        let failing = if sent.is_none() { gating } else { Vec::new() };
        if steps.is_empty() && overridden.is_empty() && failing.is_empty() {
            return;
        }
        let record = ChecklistRecord {
            steps,
            overridden,
            override_by,
            failing,
        };
        if !record.failing.is_empty() {
            self.armed_while_failing(vehicle, &record.failing);
        }
        let flight_id = self.open_flight(&vehicle.id).map(|f| f.id.clone());
        if let Some(id) = &flight_id
            && let Some(mut flight) = self.state.flights.get(id).cloned()
        {
            flight.checklist = Some(record.clone());
            flight.version += 1;
            self.commit_as(crate::history::actor(), Change::FlightUpserted { flight });
        }
        let text = record.describe();
        self.record(
            &vehicle.id,
            uxduck_core::domain::history::HistoryEvent::Armed {
                flight_id,
                text,
                record,
            },
        );
    }

    /// A warning with its tone (K10): the vehicle armed, not by UXDuck,
    /// while blocking checks did not pass.
    fn armed_while_failing(&mut self, vehicle: &Vehicle, failing: &[CheckRef]) {
        let titles: Vec<&str> = failing.iter().map(|c| c.title.as_str()).collect();
        let n = failing.len();
        let offers = self.param_offers(vehicle);
        let hint = offers
            .iter()
            .find(|o| !o.held)
            .map(|o| {
                format!(
                    " Set {} so {} holds arming for it itself.",
                    o.name, vehicle.name
                )
            })
            .unwrap_or_default();
        self.raise_or_update(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!(
                "{} armed with {n} blocking {} failing",
                vehicle.name,
                if n == 1 { "check" } else { "checks" }
            ),
            body: Some(format!(
                "UXDuck didn't arm it (its radio may have): {}.{hint}",
                titles.join(", ")
            )),
            actions: Vec::new(),
            condition: Some(armed_condition(&vehicle.id)),
            report: None,
        });
    }

    /// The vehicle disarmed: its warning goes.
    pub(crate) fn checklist_disarmed(&mut self, vehicle_id: &str) {
        self.resolve(&armed_condition(vehicle_id));
    }
}

fn armed_condition(vehicle_id: &str) -> String {
    format!("preflight-armed:{vehicle_id}")
}

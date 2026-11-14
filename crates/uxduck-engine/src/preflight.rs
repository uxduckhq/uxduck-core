//! Preflight checks and the gate on the launch (S192, S352, ADR-0038 K1 to K6, K22 to K25).
//!
//! The engine evaluates each vehicle's checks while it is on the ground,
//! armed or not, and journals the list as the `preflight` entity whenever
//! a check's status, detail or setting changes, never for the same list
//! evaluated again. Once it has launched the list stays as it launched
//! with (`evaluating` false).
//!
//! The gate (`commands::send`) evaluates afresh: a launch, a take-off or
//! a mission started from the ground, is refused `not-sent` with reason
//! `preflight` while a check of UXDuck's, a checklist's or a plugin's set
//! to hold does not pass. Arming is never held (K22). The vehicle's own
//! pre-arm reasons are listed first and never gate (K5): the vehicle
//! refuses by itself. An operator launches anyway by naming the checks
//! (K24). An agent's or a plugin's launch is put to an operator as an
//! approval listing them, and allowing it is the override.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::preflight::{
    GroupRename, Preflight as PreflightDto, PreflightConfigureParams, PreflightParams,
};
use uxduck_contracts::dto::setup::{RowState, SetupRow as RowDto};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::checklist::{Step, group_name, same_group};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{Controller, centred_throttle};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::plan::{LatLon, distance_m};
use uxduck_core::domain::preflight::{
    Check, CheckFix, CheckRef, CheckSeverity, CheckSource, CheckStatus, ChosenPlan, InputPath,
    OwnCheck, Preflight, PreflightSettings,
};
use uxduck_core::domain::settings::PilotInputMethod;
use uxduck_core::domain::setup::{SetupKind, SetupRow, fix_for};
use uxduck_core::domain::telemetry::{RC_LOST, RC_RECEIVING};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::domain::video::{RecordPolicy, VideoState};
use uxduck_core::events::{Actor, Change};

use uxduck_mavlink::px4_events::ArmingChecks;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A vehicle's checks are looked at for its telemetry at most this often;
/// the 1 Hz chore and every change to the vehicle look at once.
const REVIEW_EVERY_US: u64 = 500_000;
/// A radio's signal older than this is not read.
const SIGNAL_FRESH_US: u64 = 10_000_000;
/// An EKF status older than this no longer says where a simulation is, as
/// for the simulation start's own wait (`launch.rs`).
const EKF_FRESH_US: u64 = 5_000_000;
/// `MAV_FRAME_GLOBAL_TERRAIN_ALT` and its integer form.
const TERRAIN_FRAMES: [u8; 2] = [10, 11];

/// What preflight checks need between evaluations. Not journaled.
#[derive(Debug, Default)]
pub(crate) struct Preflights {
    /// When each vehicle's checks were last looked at for its telemetry.
    reviewed_at: BTreeMap<String, u64>,
    /// Parameters a person changed that take effect once the vehicle
    /// restarts, by vehicle: cleared when it does. Lost with the daemon.
    pub(crate) restart_params: BTreeMap<String, BTreeSet<String>>,
    /// When each checklist rule that is held for some seconds began to
    /// hold, by vehicle and step (S193). Lost with the daemon: a rule
    /// waits its seconds again.
    pub(crate) held_since: BTreeMap<String, BTreeMap<String, u64>>,
    /// Plugins' latest reports of their checks (S194). Lost with the
    /// daemon, and with the plugin when it stops.
    pub(crate) plugin_reports: crate::plugin_checks::Reports,
    /// What each PX4 vehicle's latest health and arming report says about
    /// the mode it is in. Lost with the daemon: PX4 reports again when
    /// asked.
    px4_checks: BTreeMap<String, ArmingChecks>,
}

fn actor() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
        identity: None,
    }
}

fn no_vehicle(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}"))
}

fn no_group(name: &str) -> ApiError {
    ApiError::new(
        ErrorKind::NotFound,
        format!("no group “{name}” in the list"),
    )
}

fn invalid(reason: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, reason.to_string())
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::PreflightState(PreflightParams { vehicle_id }) => {
            if !granted.contains(&Capability::Read) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Read.denied_message(),
                ));
            }
            let id = engine
                .vehicle(&vehicle_id)
                .map(|v| v.id.clone())
                .ok_or_else(|| no_vehicle(&vehicle_id))?;
            engine.review_preflight(&id);
            let preflight = engine
                .state
                .preflights
                .get(&id)
                .ok_or_else(|| no_vehicle(&vehicle_id))?;
            to_value(&PreflightDto::from(preflight))
        }
        ClientRequest::PreflightConfigure(params) => {
            if !operator {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "only a person can change a vehicle's preflight checks, from UXDuck or its CLI",
                ));
            }
            configure(engine, connection, params)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a preflight method")),
    }
}

fn configure(
    engine: &mut Engine,
    connection: ConnectionId,
    params: PreflightConfigureParams,
) -> Result<Value, ApiError> {
    let mut vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let mut settings = vehicle.preflight.clone();
    match params.check.map(OwnCheck::from) {
        Some(check) => set_check(&mut settings, check, &params)?,
        None if params.on.is_some()
            || params.severity.is_some()
            || params.threshold.is_some()
            || params.input.is_some()
            || params.group.is_some() =>
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "check: name the check to change",
            ));
        }
        None => {}
    }
    if let Some(plan_id) = params.plan_id {
        if let Some(id) = &plan_id
            && engine
                .state
                .plans
                .get(id)
                .is_none_or(|p| p.deleted_at_us.is_some())
        {
            return Err(ApiError::new(ErrorKind::NotFound, format!("no plan {id}")));
        }
        settings.plan = Some(ChosenPlan {
            plan_id,
            at_us: engine.clock.now_us(),
        });
    }
    let checklist = engine.checklist_of(&vehicle.id);
    let mut steps = checklist.steps.clone();
    arrange(
        &mut settings,
        &mut steps,
        params.rename_group,
        params.groups,
    )?;
    if settings != vehicle.preflight {
        vehicle.preflight = settings;
        vehicle.version += 1;
        let id = vehicle.id.clone();
        engine.commit(connection, Change::VehicleUpserted { vehicle });
        engine.review_preflight(&id);
    }
    let copied_from = checklist.copied_from.clone();
    engine.store_steps(connection, &checklist, steps, copied_from);
    let id = engine
        .vehicle(&params.vehicle_id)
        .map(|v| v.id.clone())
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    engine.review_preflight(&id);
    let preflight = engine
        .state
        .preflights
        .get(&id)
        .ok_or_else(|| no_vehicle(&id))?;
    to_value(&PreflightDto::from(preflight))
}

/// One of UXDuck's own checks set as asked: what is left out stays.
fn set_check(
    settings: &mut PreflightSettings,
    check: OwnCheck,
    params: &PreflightConfigureParams,
) -> Result<(), ApiError> {
    let mut setting = settings.setting(check);
    if let Some(on) = params.on {
        setting.on = on;
    }
    if let Some(severity) = params.severity {
        setting.severity = severity.into();
    }
    if let Some(threshold) = params.threshold {
        let meta = check.threshold().ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!("“{}” has no threshold", check.title()),
            )
        })?;
        if let Some(value) = threshold
            && !(value.is_finite() && (meta.min..=meta.max).contains(&value))
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "threshold: {} takes {} to {} {}",
                    check.title(),
                    meta.min,
                    meta.max,
                    meta.unit
                ),
            ));
        }
        setting.threshold = threshold;
    }
    if let Some(input) = params.input {
        if !check.has_input() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("“{}” has no input to choose", check.title()),
            ));
        }
        setting.input = Some(input.into());
    }
    settings.set(check, setting);
    if let Some(group) = &params.group {
        let group = group
            .as_deref()
            .map(group_name)
            .transpose()
            .map_err(invalid)?;
        settings.set_group(check, group.flatten());
    }
    Ok(())
}

/// The list's grouping (K31), after a check's own group: a rename and
/// an order then name the groups as they are now.
fn arrange(
    settings: &mut PreflightSettings,
    steps: &mut [Step],
    rename: Option<GroupRename>,
    order: Option<Vec<String>>,
) -> Result<(), ApiError> {
    if let Some(GroupRename { from, to }) = rename {
        let from = group_name(&from).map_err(invalid)?.unwrap_or_default();
        let to = group_name(&to)
            .map_err(invalid)?
            .ok_or_else(|| invalid("a group needs a name"))?;
        let shown = settings.groups_shown(steps);
        if !shown.iter().any(|name| same_group(name, &from)) {
            return Err(no_group(&from));
        }
        // Another group's name joins the two, as that group spells it.
        let to = shown
            .into_iter()
            .find(|name| same_group(name, &to) && !same_group(name, &from))
            .unwrap_or(to);
        settings.rename_group(&from, &to);
        // A step's tick was for its words, not its group: it stays.
        for step in steps.iter_mut() {
            if step.group.as_deref().is_some_and(|g| same_group(g, &from)) {
                step.group = Some(to.clone());
            }
        }
    }
    if let Some(names) = order {
        let shown = settings.groups_shown(steps);
        let mut placed = Vec::with_capacity(names.len());
        for name in &names {
            let name = group_name(name).map_err(invalid)?.unwrap_or_default();
            let group = shown
                .iter()
                .find(|group| same_group(group, &name))
                .ok_or_else(|| no_group(&name))?;
            placed.push(group.clone());
        }
        settings.set_group_order(placed);
    }
    Ok(())
}

/// "the compass calibration", for a running procedure.
fn procedure_words(kind: SetupKind) -> &'static str {
    match kind {
        SetupKind::Compass => "The compass calibration",
        SetupKind::CompassHeading => "The compass calibration from a heading",
        SetupKind::Accelerometer => "The accelerometer calibration",
        SetupKind::Level => "Levelling",
        SetupKind::Radio => "The radio calibration",
        SetupKind::Motors => "The motor test",
        SetupKind::Firmware => "The firmware install",
        SetupKind::AirspeedZero => "Zeroing the airspeed sensor",
        SetupKind::AirspeedCheck => "The pitot check",
        SetupKind::AirspeedRatio => "The airspeed ratio",
        SetupKind::Escs => "The ESC calibration",
    }
}

/// What waits for a restart on a Setup row.
fn row_restart_words(row: RowDto) -> &'static str {
    match row {
        RowDto::Firmware => "the new firmware",
        RowDto::Accelerometer => "the accelerometer calibration",
        RowDto::Compass => "the compass calibration",
        RowDto::Radio => "the radio calibration",
        RowDto::Airspeed => "the airspeed calibration",
        RowDto::Motors => "the motor settings",
        RowDto::Escs => "the ESC calibration",
        RowDto::Restart => "the restart",
    }
}

/// "a, b and c".
pub(crate) fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A check's outcome: its status, detail and fix.
struct Outcome {
    status: CheckStatus,
    detail: String,
    fix: Option<CheckFix>,
}

/// PX4's own checks, from its latest health and arming report on the mode
/// it is in (K5): what holds that mode while it can't arm in it, and its
/// warnings.
fn px4_checks(vehicle: &Vehicle, report: &ArmingChecks) -> Vec<Check> {
    const WARNING: u8 = 4;
    let name = &vehicle.name;
    let mode = report.mode.as_deref().unwrap_or("its mode");
    let mut checks: Vec<Check> = Vec::new();
    for check in report
        .checks
        .iter()
        .filter(|c| c.holds || c.severity <= WARNING)
    {
        let id = format!("vehicle:{}", check.text);
        if checks.iter().any(|c| c.id == id) {
            continue;
        }
        let fix = match fix_for(&vehicle.autopilot, &check.text) {
            Some((row, _)) if uxduck_core::domain::setup::offered(&vehicle.autopilot, row) => {
                CheckFix::Setup { row }
            }
            _ => CheckFix::Messages,
        };
        let said = if check.holds {
            format!("{name} says it can't arm in {mode} until this clears.")
        } else if report.can_arm == Some(true) {
            format!("{name} can arm in {mode} but warns about this.")
        } else {
            format!("{name} warns about this.")
        };
        let detail = if check.description.is_empty() {
            said
        } else {
            format!("{said} {}", check.description)
        };
        checks.push(Check {
            id,
            source: CheckSource::Vehicle,
            title: check.text.clone(),
            severity: if check.holds {
                CheckSeverity::Block
            } else {
                CheckSeverity::Warn
            },
            status: CheckStatus::Fail,
            detail: Some(detail),
            fix: Some(fix),
            plugin: None,
            group: None,
            when: None,
        });
    }
    if checks.iter().any(|c| c.severity == CheckSeverity::Block) {
        return checks;
    }
    let (status, detail) = match report.can_arm {
        Some(true) => (CheckStatus::Pass, format!("It says it can arm in {mode}.")),
        Some(false) => (
            CheckStatus::Fail,
            format!("It says it can't arm in {mode} yet."),
        ),
        None => (
            CheckStatus::Unknown,
            format!("It hasn't said whether it can arm in {mode}."),
        ),
    };
    checks.insert(
        0,
        Check {
            id: "vehicle:ready".into(),
            source: CheckSource::Vehicle,
            title: format!("{name}'s own checks"),
            severity: CheckSeverity::Block,
            status,
            detail: Some(detail),
            fix: None,
            plugin: None,
            group: None,
            when: None,
        },
    );
    checks
}

fn pass(detail: impl Into<String>) -> Outcome {
    Outcome {
        status: CheckStatus::Pass,
        detail: detail.into(),
        fix: None,
    }
}

fn not(status: CheckStatus, detail: impl Into<String>, fix: Option<CheckFix>) -> Outcome {
    Outcome {
        status,
        detail: detail.into(),
        fix,
    }
}

/// The words for the checks a command was refused for.
pub(crate) fn refusal(label: &str, vehicle: &str, gating: &[CheckRef], operator: bool) -> String {
    let n = gating.len();
    let list: Vec<String> = gating
        .iter()
        .map(|c| match &c.detail {
            Some(detail) => format!("{} ({})", c.title, detail.trim_end_matches('.')),
            None => c.title.clone(),
        })
        .collect();
    let hint = match (operator, n) {
        (false, _) => String::new(),
        (true, 1) => format!(" Fix it, or hold {label} anyway to launch past it."),
        (true, _) => format!(" Fix them, or hold {label} anyway to launch past them."),
    };
    format!(
        "{label} wasn't sent to {vehicle}: {n} {} {}: {}.{hint}",
        if n == 1 {
            "check that holds the launch"
        } else {
            "checks that hold the launch"
        },
        if n == 1 { "doesn't pass" } else { "don't pass" },
        list.join(", ")
    )
}

impl Engine {
    /// A vehicle's checks as they stand now (K2).
    pub(crate) fn evaluate_preflight(&self, vehicle: &Vehicle) -> Preflight {
        let mut checks = self.vehicle_checks(vehicle);
        for check in OwnCheck::ALL {
            let setting = vehicle.preflight.setting(check);
            if !setting.on {
                continue;
            }
            let outcome = match check {
                OwnCheck::Setup => self.setup_check(vehicle),
                OwnCheck::Restart => self.restart_check(vehicle),
                OwnCheck::Link => self.link_check(vehicle),
                OwnCheck::RcReceiver => self.rc_check(vehicle),
                OwnCheck::Controller => self.controller_check(vehicle),
                OwnCheck::Plan => self.plan_check(vehicle).0,
                OwnCheck::Terrain => self.terrain_preflight(vehicle),
                OwnCheck::Recording => self.recording_check(vehicle),
                OwnCheck::Home => self.home_check(vehicle),
            };
            checks.push(Check {
                id: check.id(),
                source: CheckSource::Uxduck,
                title: check.title().to_owned(),
                group: Some(vehicle.preflight.group(check).to_owned()),
                severity: setting.severity,
                when: check.when(),
                status: outcome.status,
                detail: Some(outcome.detail),
                fix: outcome.fix,
                plugin: None,
            });
        }
        checks.extend(self.simulation_position_check(vehicle));
        // The vehicle's checklist (S193), then plugins' checks (S194).
        checks.extend(self.checklist_checks(vehicle));
        checks.extend(self.plugin_checks(vehicle));
        Preflight {
            vehicle_id: vehicle.id.clone(),
            checks,
            settings: vehicle.preflight.clone(),
            plan_id: self.plan_check(vehicle).1,
            evaluating: !self.launched(vehicle),
            checklist: self.state.checklists.get(&vehicle.id).cloned(),
            flown_since: self.flown_since(vehicle),
            offers: self.param_offers(vehicle),
            updated_at_us: self.clock.now_us(),
            version: 0,
        }
    }

    /// The checks holding a launch now, evaluated afresh (K23): what the
    /// gate refuses for.
    pub(crate) fn preflight_gating(&self, vehicle_id: &str) -> Vec<CheckRef> {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return Vec::new();
        };
        self.evaluate_preflight(vehicle)
            .gating()
            .map(Check::to_ref)
            .collect()
    }

    /// A request that would launch a vehicle past failing checks set to
    /// hold: the checks, for its approval to list (K24). Empty for
    /// anything else, arming included (K22).
    pub(crate) fn preflight_asks(&self, request: &ClientRequest) -> Vec<CheckRef> {
        let ClientRequest::CommandSend(params) = request else {
            return Vec::new();
        };
        let command = Command::from(params.command.clone());
        match self.vehicle(&params.vehicle_id) {
            Some(vehicle) if command.launches() && !self.launched(vehicle) => {
                self.preflight_gating(&vehicle.id)
            }
            _ => Vec::new(),
        }
    }

    /// A plugin's raw `COMMAND_LONG` or `COMMAND_INT` that would launch
    /// the vehicle while a check set to hold does not pass.
    pub(crate) fn raw_message_launches(
        &self,
        vehicle_id: &str,
        name: &str,
        fields: &serde_json::Map<String, Value>,
    ) -> bool {
        if !matches!(name, "COMMAND_LONG" | "COMMAND_INT") {
            return false;
        }
        let Some(number) = fields
            .get("command")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
        else {
            return false;
        };
        #[allow(clippy::cast_possible_truncation, reason = "MAVLink's floats")]
        let params: Vec<f32> = ["param1", "param2"]
            .iter()
            .map(|key| fields.get(*key).and_then(Value::as_f64).unwrap_or(0.0) as f32)
            .collect();
        let command = Command::Long {
            command: number,
            params,
        };
        command.launches()
            && self.vehicle(vehicle_id).is_some_and(|v| !self.launched(v))
            && !self.preflight_gating(vehicle_id).is_empty()
    }

    /// Looks at a vehicle's checks and journals them if they changed.
    pub(crate) fn review_preflight(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        self.preflight.reviewed_at.insert(vehicle.id.clone(), now);
        self.track_rules(&vehicle.id);
        let stored = self.state.preflights.get(&vehicle.id).cloned();
        let mut next = match (&stored, self.launched(&vehicle)) {
            // Launched: the list it launched with stays, marked so; the
            // checklist's steps and ticks are as they are now (S193). On
            // the ground it is read live, armed or not (K26).
            (Some(stored), true) => Preflight {
                evaluating: false,
                settings: vehicle.preflight.clone(),
                checklist: self.state.checklists.get(&vehicle.id).cloned(),
                flown_since: self.flown_since(&vehicle),
                offers: self.param_offers(&vehicle),
                ..stored.clone()
            },
            _ => self.evaluate_preflight(&vehicle),
        };
        if let Some(stored) = &stored
            && stored.same_as(&next)
        {
            return;
        }
        next.updated_at_us = now;
        next.version = stored.map_or(1, |s| s.version + 1);
        self.commit_as(actor(), Change::PreflightUpdated { preflight: next });
    }

    /// Telemetry came: looks at the vehicle's checks, at most every half
    /// second.
    pub(crate) fn review_preflight_soon(&mut self, vehicle_id: &str) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        let now = self.clock.now_us();
        if self
            .preflight
            .reviewed_at
            .get(&id)
            .is_some_and(|at| now.saturating_sub(*at) < REVIEW_EVERY_US)
        {
            return;
        }
        self.review_preflight(&id);
    }

    /// Every vehicle's checks, from the hosts' 1 Hz beat: what changes with
    /// time alone (a pre-arm reason no longer said, a link gone quiet).
    pub fn review_preflights(&mut self) {
        let ids: Vec<String> = self.state.vehicles.keys().cloned().collect();
        for id in ids {
            self.review_preflight(&id);
        }
    }

    /// A parameter write ended. A person's or an agent's (`asked`), accepted
    /// and of one that takes effect on restarting, makes a restart due; one
    /// of UXDuck's own (a simulation's start, practice) does not.
    pub(crate) fn preflight_written(&mut self, vehicle_id: &str, name: &str, asked: bool) {
        // Lua scripting starts only as the vehicle boots (ADR-0027 F14),
        // whether or not its definitions are loaded to say so.
        if !asked || !(self.param_facts(vehicle_id, name).1 || name == "SCR_ENABLE") {
            return;
        }
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        self.preflight
            .restart_params
            .entry(id.clone())
            .or_default()
            .insert(name.to_owned());
        self.review_preflight(&id);
    }

    /// Whether PX4's latest report says it can arm in the mode it is in:
    /// unknown before it reports, and from PX4 older than 1.14.
    pub(crate) fn px4_can_arm(&self, vehicle_id: &str) -> Option<bool> {
        self.preflight.px4_checks.get(vehicle_id)?.can_arm
    }

    /// PX4's report on the mode it is in, as its own checks (K5).
    pub fn arming_checks(&mut self, vehicle_id: &str, checks: ArmingChecks) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        self.preflight.px4_checks.insert(id.clone(), checks);
        self.review_preflight(&id);
    }

    /// The vehicle restarted: no parameter waits for it any more.
    pub(crate) fn preflight_restarted(&mut self, vehicle_id: &str) {
        let Some(id) = self.vehicle(vehicle_id).map(|v| v.id.clone()) else {
            return;
        };
        if self.preflight.restart_params.remove(&id).is_some() {
            self.review_preflight(&id);
        }
    }

    /// The vehicle's own pre-arm reasons (K5), or one row saying whether it
    /// is ready when it says nothing against it.
    fn vehicle_checks(&self, vehicle: &Vehicle) -> Vec<Check> {
        if let Some(report) = self.preflight.px4_checks.get(&vehicle.id) {
            return px4_checks(vehicle, report);
        }
        let reasons = self.prearm_reasons(&vehicle.id);
        if reasons.is_empty() {
            let ready = self.telemetry(&vehicle.id).and_then(|t| {
                t.items
                    .iter()
                    .find(|i| i.key == "arming.ready")
                    .and_then(|i| i.value)
            });
            let (status, detail) = match ready {
                Some(v) if v >= 0.5 => (CheckStatus::Pass, "It says it is ready to arm."),
                Some(_) => (CheckStatus::Fail, "It says it isn't ready to arm yet."),
                None => (
                    CheckStatus::Unknown,
                    "It hasn't said whether it is ready to arm.",
                ),
            };
            return vec![Check {
                id: "vehicle:ready".into(),
                source: CheckSource::Vehicle,
                title: format!("{}'s own checks", vehicle.name),
                group: None,
                severity: CheckSeverity::Block,
                when: None,
                status,
                detail: Some(detail.into()),
                fix: None,
                plugin: None,
            }];
        }
        reasons
            .into_iter()
            .map(|reason| {
                let fix = match fix_for(&vehicle.autopilot, &reason) {
                    Some((row, _))
                        if uxduck_core::domain::setup::offered(&vehicle.autopilot, row) =>
                    {
                        CheckFix::Setup { row }
                    }
                    _ => CheckFix::Messages,
                };
                Check {
                    id: format!("vehicle:{reason}"),
                    source: CheckSource::Vehicle,
                    title: reason,
                    group: None,
                    severity: CheckSeverity::Block,
                    when: None,
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "{} says it can't arm until this clears.",
                        vehicle.name
                    )),
                    fix: Some(fix),
                    plugin: None,
                }
            })
            .collect()
    }

    /// A simulated `ArduPilot` vehicle that says it may arm before its EKF
    /// knows where it is takes off in nothing but a stabilised mode: GUIDED,
    /// AUTO and Take off are refused until it does. Its launch waits until
    /// then, a few seconds after its own checks pass, and while its EKF
    /// status is missing or more than 5 s old. It is the one check that
    /// holds a launch without a person setting it to (K25): a simulation's
    /// alone, and gone in seconds. PX4 is not held: the rule is for
    /// ArduPilot's SITL, which says it may arm before its EKF has a position.
    fn simulation_position_check(&self, vehicle: &Vehicle) -> Option<Check> {
        if !self.is_simulated(vehicle) || vehicle.autopilot != "Ardupilotmega" {
            return None;
        }
        let now = self.clock.now_us();
        let ekf = self
            .telemetry(&vehicle.id)
            .and_then(|t| t.items.iter().find(|i| i.key == "ekf.status"));
        let name = &vehicle.name;
        let (status, detail) = match ekf {
            None => (
                CheckStatus::Waiting,
                format!("{name} hasn't said yet whether it knows where it is."),
            ),
            Some(item) if now.saturating_sub(item.time_us) > EKF_FRESH_US => (
                CheckStatus::Waiting,
                format!("{name} hasn't said for over 5 s whether it knows where it is."),
            ),
            Some(item) if item.text.as_deref() == Some("Good") => {
                (CheckStatus::Pass, format!("{name} knows where it is."))
            }
            Some(_) => (
                CheckStatus::Waiting,
                format!(
                    "{name} is still starting: it can take off once it knows where it is, in a few seconds."
                ),
            ),
        };
        Some(Check {
            id: "uxduck:simulation-position".into(),
            source: CheckSource::Uxduck,
            title: "Position".into(),
            group: None,
            severity: CheckSeverity::Block,
            when: None,
            status,
            detail: Some(detail),
            fix: None,
            plugin: None,
        })
    }

    fn setup_check(&self, vehicle: &Vehicle) -> Outcome {
        match self.running_setup(&vehicle.id) {
            Some(setup) => not(
                CheckStatus::Fail,
                format!("{} is running.", procedure_words(setup.kind)),
                Some(CheckFix::Setup {
                    row: setup.kind.row(),
                }),
            ),
            None => pass("None running."),
        }
    }

    fn restart_check(&self, vehicle: &Vehicle) -> Outcome {
        let rows: Vec<RowDto> = self
            .setup_state(&vehicle.id)
            .map(|state| {
                state
                    .rows
                    .iter()
                    .filter(|r| r.state == RowState::NeedsRestart)
                    .map(|r| r.row)
                    .collect()
            })
            .unwrap_or_default();
        let params: Vec<String> = self
            .preflight
            .restart_params
            .get(&vehicle.id)
            .map(|names| names.iter().cloned().collect())
            .unwrap_or_default();
        if rows.is_empty() && params.is_empty() {
            return pass("Nothing waits for a restart.");
        }
        let mut what: Vec<String> = rows.iter().map(|r| row_restart_words(*r).into()).collect();
        what.extend(params.iter().cloned());
        // The Restart row, wherever the restart is due from (S216, V16).
        let fix = CheckFix::Setup {
            row: SetupRow::Restart,
        };
        let mut detail = and_list(&what);
        if let Some(first) = detail.get(..1) {
            detail = format!("{}{}", first.to_uppercase(), &detail[1..]);
        }
        let verb = if what.len() == 1 { "takes" } else { "take" };
        not(
            CheckStatus::Fail,
            format!("{detail} {verb} effect once {} restarts.", vehicle.name),
            Some(fix),
        )
    }

    fn link_check(&self, vehicle: &Vehicle) -> Outcome {
        match vehicle.presence {
            Presence::Online => {}
            Presence::Stale => {
                return not(
                    CheckStatus::Fail,
                    format!("UXDuck can't hear {} right now.", vehicle.name),
                    Some(CheckFix::Connections),
                );
            }
            Presence::Offline => {
                return not(
                    CheckStatus::Fail,
                    format!("{} is offline.", vehicle.name),
                    Some(CheckFix::Connections),
                );
            }
        }
        let now = self.clock.now_us();
        let least = self.telemetry(&vehicle.id).and_then(|t| {
            t.items
                .iter()
                .filter(|i| {
                    (i.key == "link.rssi_pct" || i.key == "link.remote_rssi_pct")
                        && now.saturating_sub(i.time_us) < SIGNAL_FRESH_US
                })
                .filter_map(|i| i.value)
                .reduce(f64::min)
        });
        let threshold = vehicle
            .preflight
            .threshold(OwnCheck::Link)
            .unwrap_or_default();
        match least {
            Some(signal) if signal < threshold => not(
                CheckStatus::Fail,
                // No live number in its words: the list is journaled when
                // they change, and a radio's signal changes all the time.
                format!("The radio signal is below {threshold:.0} %."),
                Some(CheckFix::Connections),
            ),
            Some(_) => pass(format!(
                "Heard just now. The radio signal is at least {threshold:.0} %."
            )),
            None => pass("Heard just now."),
        }
    }

    /// The vehicle's controller input as it says in `SYS_STATUS`
    /// (`rc.status`). It counts a ground station's stick input as RC, and
    /// so does its failsafe, so a controller through UXDuck passes as its
    /// own receiver does (maintainer, 2026-11-13). A person may ask for one
    /// of the two (`InputPath`): for its own receiver alone, while a
    /// controller drives it from UXDuck it cannot say whether that works.
    fn rc_check(&self, vehicle: &Vehicle) -> Outcome {
        let name = &vehicle.name;
        let now = self.clock.now_us();
        let said = self
            .telemetry(&vehicle.id)
            .and_then(|t| t.items.iter().find(|i| i.key == "rc.status"))
            .filter(|i| now.saturating_sub(i.time_us) < SIGNAL_FRESH_US)
            .and_then(|i| i.text.as_deref());
        let radio = Some(CheckFix::Setup {
            row: SetupRow::Radio,
        });
        let path = vehicle
            .preflight
            .input(OwnCheck::RcReceiver)
            .unwrap_or_default();
        let driving = self.driving(vehicle);
        let controller = driving.first().map(|c| c.name.as_str());
        if path == InputPath::Link {
            let Some(controller) = controller else {
                return not(
                    CheckStatus::Fail,
                    format!("No controller drives {name} from UXDuck. Choose one in Controls."),
                    Some(CheckFix::Controllers),
                );
            };
            return match said {
                None => not(
                    CheckStatus::Unknown,
                    format!("{name} hasn't said whether it takes {controller}'s input."),
                    None,
                ),
                Some(RC_RECEIVING) => {
                    pass(format!("{name} takes {controller}'s input from UXDuck."))
                }
                Some(_) => not(
                    CheckStatus::Fail,
                    format!("{name} isn't taking {controller}'s input from UXDuck."),
                    Some(CheckFix::Controllers),
                ),
            };
        }
        match said {
            None => not(
                CheckStatus::Unknown,
                format!("{name} hasn't said whether it has RC input."),
                None,
            ),
            Some(RC_RECEIVING) => match controller {
                Some(controller) if path == InputPath::Rc => not(
                    CheckStatus::Unknown,
                    format!(
                        "{name} takes {controller}'s input from UXDuck as RC, so it can't say whether its own receiver works."
                    ),
                    None,
                ),
                Some(controller) => pass(format!(
                    "{name} takes {controller}'s input from UXDuck as RC."
                )),
                None => pass(format!("{name} is receiving RC input.")),
            },
            Some(RC_LOST) => not(
                CheckStatus::Fail,
                format!("{name} has lost its RC signal. Check its transmitter is on and in range."),
                radio,
            ),
            Some(_) => not(
                CheckStatus::Fail,
                format!(
                    "{name} has had no RC input since it started. Check its transmitter is on and bound."
                ),
                radio,
            ),
        }
    }

    /// The controllers driving a vehicle from UXDuck.
    fn driving(&self, vehicle: &Vehicle) -> Vec<&Controller> {
        let ids: Vec<&String> = std::iter::once(&vehicle.id)
            .chain(vehicle.aliases.iter())
            .collect();
        self.state
            .controllers
            .values()
            .filter(|c| {
                c.connected
                    && c.claim.is_some()
                    && c.target.as_ref().is_some_and(|t| ids.contains(&t))
            })
            .collect()
    }

    fn controller_check(&self, vehicle: &Vehicle) -> Outcome {
        let driving = self.driving(vehicle);
        if driving.is_empty() {
            return pass(format!("No controller drives {}.", vehicle.name));
        }
        // ArduRover stops at its throttle channel's trim, which
        // `MANUAL_CONTROL` reaches only through that calibration, as the
        // data plane's session holds it and says: without it the data plane
        // sends the vehicle nothing.
        if Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type) == Some(Family::Rover)
            && self.pilot_input_method(&vehicle.id) == PilotInputMethod::ManualControl
        {
            let problem = match self.transfers.rc_calibrations.get(&vehicle.id) {
                Some(problem) => problem.clone(),
                None => Some(format!(
                    "{}'s parameters haven't been read on this connection",
                    vehicle.name
                )),
            };
            if let Some(problem) = problem {
                return not(
                    CheckStatus::Fail,
                    format!(
                        "{problem}, so UXDuck sends {} no MANUAL_CONTROL. Read its parameters, or calibrate its radio.",
                        vehicle.name
                    ),
                    Some(CheckFix::Setup {
                        row: SetupRow::Radio,
                    }),
                );
            }
        }
        let band = vehicle
            .preflight
            .threshold(OwnCheck::Controller)
            .unwrap_or(10.0)
            / 100.0;
        for controller in &driving {
            let Some((sticks, _)) = self.mapped_controller_input(controller) else {
                return not(
                    CheckStatus::Unknown,
                    format!(
                        "Move each stick on {} once, so UXDuck can read it.",
                        controller.name
                    ),
                    Some(CheckFix::Controllers),
                );
            };
            let centred = [sticks.roll, sticks.pitch, sticks.yaw]
                .iter()
                .all(|v| v.abs() < band);
            if !centred {
                return not(
                    CheckStatus::Fail,
                    format!("Centre the sticks on {}.", controller.name),
                    Some(CheckFix::Controllers),
                );
            }
            // 0 is the throttle's middle. An aircraft's rests below it, where
            // a gamepad's springs back to the middle and a radio's sits at
            // the bottom; forward / reverse and ascend / descend rest in the
            // middle and move the vehicle either way from it.
            let centred = centred_throttle(&vehicle.autopilot, &vehicle.vehicle_type);
            if !sticks.throttle_neutral(centred.is_some(), band) {
                return not(
                    CheckStatus::Fail,
                    match centred {
                        Some(control) => format!("Centre {control} on {}.", controller.name),
                        None => format!("Lower the throttle on {}.", controller.name),
                    },
                    Some(CheckFix::Controllers),
                );
            }
        }
        let names: Vec<String> = driving.iter().map(|c| c.name.clone()).collect();
        pass(format!("{} at neutral.", and_list(&names)))
    }

    /// The plan check (ADR-0029), and the plan that counts as chosen: the
    /// one a person picked, or the one uploaded since.
    fn plan_check(&self, vehicle: &Vehicle) -> (Outcome, Option<String>) {
        let ids: BTreeSet<&str> = std::iter::once(vehicle.id.as_str())
            .chain(vehicle.aliases.iter().map(String::as_str))
            .collect();
        let last = self
            .state
            .plans
            .values()
            .flat_map(|plan| {
                plan.uploads
                    .iter()
                    .filter(|u| ids.contains(u.vehicle_id.as_str()))
                    .map(move |u| (plan, u))
            })
            .max_by_key(|(_, u)| u.at_us);
        let chosen: Option<String> = match (&vehicle.preflight.plan, last) {
            (Some(picked), Some((_, upload))) if picked.at_us >= upload.at_us => {
                picked.plan_id.clone()
            }
            (Some(picked), None) => picked.plan_id.clone(),
            (_, Some((plan, _))) => Some(plan.id.clone()),
            (None, None) => None,
        };
        let name = &vehicle.name;
        let Some(id) = chosen else {
            return (pass(format!("No plan chosen for {name}.")), None);
        };
        let fix = Some(CheckFix::Plan {
            plan_id: id.clone(),
        });
        let Some(plan) = self
            .state
            .plans
            .get(&id)
            .filter(|p| p.deleted_at_us.is_none())
        else {
            return (
                not(
                    CheckStatus::Fail,
                    "The plan chosen was deleted.",
                    Some(CheckFix::Plans),
                ),
                Some(id),
            );
        };
        let title = &plan.body.name;
        let upload = plan
            .uploads
            .iter()
            .filter(|u| ids.contains(u.vehicle_id.as_str()))
            .max_by_key(|u| u.at_us);
        let mission_version = self.mission(&vehicle.id).version;
        let outcome = match (last, upload) {
            (Some((other, _)), _) if other.id != plan.id => not(
                CheckStatus::Fail,
                format!("{title} is chosen, but {name} has {}.", other.body.name),
                fix,
            ),
            (_, None) => not(
                CheckStatus::Fail,
                format!("{title} is chosen, but it isn't on {name} yet."),
                fix,
            ),
            (_, Some(u)) if !u.verified => not(
                CheckStatus::Fail,
                format!("{name} read {title} back different."),
                fix,
            ),
            (_, Some(u)) if plan.changed_at_us > u.at_us => not(
                CheckStatus::Fail,
                format!("{title} was edited after it was uploaded to {name}."),
                fix,
            ),
            (_, Some(u)) if mission_version != u.mission_version => not(
                CheckStatus::Fail,
                format!("{name}'s mission changed after {title} was uploaded."),
                fix,
            ),
            (_, Some(_)) => pass(format!("{title}, verified on {name}.")),
        };
        (outcome, Some(id))
    }

    /// Terrain, where the vehicle's mission flies above it.
    fn terrain_preflight(&self, vehicle: &Vehicle) -> Outcome {
        let mission = self.mission(&vehicle.id);
        let name = &vehicle.name;
        if !mission
            .items
            .iter()
            .any(|i| TERRAIN_FRAMES.contains(&i.frame))
        {
            return pass(format!("{name}'s mission has no heights above terrain."));
        }
        let item = |key: &str| {
            self.telemetry(&vehicle.id)
                .and_then(|t| t.items.iter().find(|i| i.key == key).and_then(|i| i.value))
        };
        match (item("terrain.pending"), item("terrain.loaded")) {
            (Some(pending), _) if pending > 0.0 => not(
                CheckStatus::Waiting,
                format!("{name} is loading terrain."),
                Some(CheckFix::Mission),
            ),
            (Some(_), Some(loaded)) if loaded > 0.0 => pass(format!("{name} has its terrain.")),
            (Some(_), Some(_)) => not(
                CheckStatus::Waiting,
                format!("{name} has no terrain loaded yet."),
                Some(CheckFix::Mission),
            ),
            _ => not(
                CheckStatus::Unknown,
                format!("{name} hasn't reported its terrain."),
                Some(CheckFix::Mission),
            ),
        }
    }

    /// Home is where the vehicle is (K27): a return goes to home, and one
    /// left indoors or at an earlier place is not where it takes off. No
    /// live distance in its words, as for the radio's signal. `ArduPilot`
    /// and PX4 move home to the vehicle when it arms unless home was set
    /// by hand, so while it is disarmed a home far away is not a warning
    /// and the words say why (maintainer, 2026-11-13). Once it is armed a
    /// home still far away is one that stays, and that is the warning.
    fn home_check(&self, vehicle: &Vehicle) -> Outcome {
        let name = &vehicle.name;
        let within = vehicle
            .preflight
            .threshold(OwnCheck::Home)
            .unwrap_or_default();
        let telemetry = self.telemetry(&vehicle.id);
        let item = |key: &str| {
            telemetry.and_then(|t| t.items.iter().find(|i| i.key == key).and_then(|i| i.value))
        };
        let at = telemetry.and_then(|t| t.positions.iter().find(|p| p.source == "fused"));
        // Every family UXDuck knows is offered Set home (`home_and_origin`).
        let fix =
            Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type).map(|_| CheckFix::Home);
        match (item("home.latitude_deg"), item("home.longitude_deg"), at) {
            (Some(latitude_deg), Some(longitude_deg), Some(at)) => {
                let away = distance_m(
                    LatLon {
                        latitude_deg,
                        longitude_deg,
                    },
                    LatLon {
                        latitude_deg: at.latitude_deg,
                        longitude_deg: at.longitude_deg,
                    },
                );
                if away <= within {
                    pass(format!("Home is within {within:.0} m of {name}."))
                } else if vehicle.armed {
                    not(
                        CheckStatus::Fail,
                        format!(
                            "Home is more than {within:.0} m from {name}. A return goes to home."
                        ),
                        fix,
                    )
                } else {
                    pass(format!(
                        "Home is more than {within:.0} m from {name}. Most autopilots move it here when they arm, so UXDuck reads it again once {name} is armed."
                    ))
                }
            }
            (Some(_), Some(_), None) => not(
                CheckStatus::Unknown,
                format!("{name} hasn't said where it is."),
                None,
            ),
            _ => not(
                CheckStatus::Unknown,
                format!("{name} has no home yet. It sets one once it knows where it is."),
                None,
            ),
        }
    }

    /// The cameras that record while it is armed (ADR-0017 V16).
    fn recording_check(&self, vehicle: &Vehicle) -> Outcome {
        let name = &vehicle.name;
        if self.state.settings.video_recording == RecordPolicy::ByHand {
            return pass("Cameras record when someone presses Record.");
        }
        let ids: BTreeSet<&str> = std::iter::once(vehicle.id.as_str())
            .chain(vehicle.aliases.iter().map(String::as_str))
            .collect();
        let cameras: Vec<_> = self
            .state
            .video_sources
            .values()
            .filter(|s| !s.record_alone && s.vehicle_id.as_deref().is_some_and(|v| ids.contains(v)))
            .collect();
        if cameras.is_empty() {
            return pass(format!("No camera records with {name}."));
        }
        // The vehicle's own camera records what the downlink loses.
        let quiet: Vec<String> = cameras
            .iter()
            .filter(|s| s.state != VideoState::Receiving && !s.vehicle_camera)
            .map(|s| s.name.clone())
            .collect();
        if quiet.is_empty() {
            let n = cameras.len();
            return pass(if n == 1 {
                "Its camera is sending.".to_owned()
            } else {
                format!("Its {n} cameras are sending.")
            });
        }
        let verb = if quiet.len() == 1 { "isn't" } else { "aren't" };
        not(
            CheckStatus::Fail,
            format!(
                "{} {verb} sending video, so nothing records when {name} arms.",
                and_list(&quiet)
            ),
            Some(CheckFix::Cameras),
        )
    }
}

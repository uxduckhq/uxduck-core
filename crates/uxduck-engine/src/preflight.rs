//! Preflight checks and the arming gate (S192, ADR-0038 K1 to K6).
//!
//! The engine evaluates each vehicle's checks while it is disarmed and
//! journals the list as the `preflight` entity whenever a check's status,
//! detail or setting changes, never for the same list evaluated again.
//! While it is armed the list stays as it armed with (`evaluating` false).
//!
//! The gate (`commands::send`) evaluates afresh: a command that arms on the
//! ground is refused `not-sent` with reason `preflight` while a blocking
//! check of UXDuck's, a checklist's or a plugin's does not pass.
//! The vehicle's own pre-arm reasons are listed first and never gate (K5):
//! the vehicle refuses by itself. An operator arms anyway by naming the
//! checks (K4); an agent's or a plugin's arming is put to an operator as an
//! approval listing them, and allowing it is the override.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::preflight::{
    Preflight as PreflightDto, PreflightConfigureParams, PreflightParams,
};
use uxduck_contracts::dto::setup::{RowState, SetupRow as RowDto};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::Command;
use uxduck_core::domain::preflight::{
    Check, CheckFix, CheckRef, CheckSeverity, CheckSource, CheckStatus, ChosenPlan, OwnCheck,
    Preflight,
};
use uxduck_core::domain::setup::{SetupKind, SetupRow, fix_for};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::domain::video::{RecordPolicy, VideoState};
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A vehicle's checks are looked at for its telemetry at most this often;
/// the 1 Hz chore and every change to the vehicle look at once.
const REVIEW_EVERY_US: u64 = 500_000;
/// A radio's signal older than this is not read.
const SIGNAL_FRESH_US: u64 = 10_000_000;
/// `MAV_FRAME_GLOBAL_TERRAIN_ALT` and its integer form.
const TERRAIN_FRAMES: [u8; 2] = [10, 11];

/// What preflight checks need between evaluations. Not journaled.
#[derive(Debug, Default)]
pub(crate) struct Preflights {
    /// When each vehicle's checks were last looked at for its telemetry.
    reviewed_at: BTreeMap<String, u64>,
    /// Parameters a person changed that take effect once the vehicle
    /// restarts, by vehicle: cleared when it does. Lost with the daemon.
    restart_params: BTreeMap<String, BTreeSet<String>>,
    /// When each checklist rule that is held for some seconds began to
    /// hold, by vehicle and step (S193). Lost with the daemon: a rule
    /// waits its seconds again.
    pub(crate) held_since: BTreeMap<String, BTreeMap<String, u64>>,
    /// Plugins' latest reports of their checks (S194). Lost with the
    /// daemon, and with the plugin when it stops.
    pub(crate) plugin_reports: crate::plugin_checks::Reports,
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
        Some(check) => {
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
            settings.set(check, setting);
        }
        None if params.on.is_some() || params.severity.is_some() || params.threshold.is_some() => {
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
    if settings != vehicle.preflight {
        vehicle.preflight = settings;
        vehicle.version += 1;
        let id = vehicle.id.clone();
        engine.commit(connection, Change::VehicleUpserted { vehicle });
        engine.review_preflight(&id);
    }
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
    }
}

fn row_of(row: RowDto) -> SetupRow {
    row.into()
}

/// "a, b and c".
fn and_list(items: &[String]) -> String {
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
    let hint = if operator {
        " Fix them, or hold Arm anyway to arm past them."
    } else {
        ""
    };
    format!(
        "{label} wasn't sent to {vehicle}: {n} preflight {} {}: {}.{hint}",
        if n == 1 { "check" } else { "checks" },
        if n == 1 { "doesn't pass" } else { "don't pass" },
        list.join("; ")
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
                OwnCheck::Controller => self.controller_check(vehicle),
                OwnCheck::Plan => self.plan_check(vehicle).0,
                OwnCheck::Terrain => self.terrain_preflight(vehicle),
                OwnCheck::Recording => self.recording_check(vehicle),
            };
            checks.push(Check {
                id: check.id(),
                source: CheckSource::Uxduck,
                title: check.title().to_owned(),
                severity: setting.severity,
                status: outcome.status,
                detail: Some(outcome.detail),
                fix: outcome.fix,
                plugin: None,
            });
        }
        // The vehicle's checklist (S193), then plugins' checks (S194).
        checks.extend(self.checklist_checks(vehicle));
        checks.extend(self.plugin_checks(vehicle));
        Preflight {
            vehicle_id: vehicle.id.clone(),
            checks,
            settings: vehicle.preflight.clone(),
            plan_id: self.plan_check(vehicle).1,
            evaluating: !vehicle.armed,
            checklist: self.state.checklists.get(&vehicle.id).cloned(),
            flown_since: self.flown_since(vehicle),
            offers: self.param_offers(vehicle),
            updated_at_us: self.clock.now_us(),
            version: 0,
        }
    }

    /// The checks holding arming now, evaluated afresh (K3): what the gate
    /// refuses for.
    pub(crate) fn preflight_gating(&self, vehicle_id: &str) -> Vec<CheckRef> {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return Vec::new();
        };
        self.evaluate_preflight(vehicle)
            .gating()
            .map(Check::to_ref)
            .collect()
    }

    /// A request that would arm a disarmed vehicle past failing blocking
    /// checks, from a client here (not from away): the checks, for its
    /// approval to list (K4). Empty for anything else.
    pub(crate) fn preflight_asks(
        &self,
        connection: ConnectionId,
        request: &ClientRequest,
    ) -> Vec<CheckRef> {
        let ClientRequest::CommandSend(params) = request else {
            return Vec::new();
        };
        if self.remote_sender(connection).is_some() {
            return Vec::new();
        }
        let command = Command::from(params.command.clone());
        match self.vehicle(&params.vehicle_id) {
            Some(vehicle) if !vehicle.armed && command.arms_on_the_ground() => {
                self.preflight_gating(&vehicle.id)
            }
            _ => Vec::new(),
        }
    }

    /// A plugin's raw `COMMAND_LONG` or `COMMAND_INT` that would arm the
    /// disarmed vehicle while a blocking check does not pass.
    pub(crate) fn raw_message_arms(
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
        command.arms_on_the_ground()
            && self.vehicle(vehicle_id).is_some_and(|v| !v.armed)
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
        let mut next = match (&stored, vehicle.armed) {
            // Armed: the list it armed with stays, marked so; the
            // checklist's steps and ticks are as they are now (S193).
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
        if !asked || !self.param_facts(vehicle_id, name).1 {
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
                severity: CheckSeverity::Block,
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
                    severity: CheckSeverity::Block,
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
        let fix = rows
            .first()
            .map_or(CheckFix::Restart, |row| CheckFix::Setup {
                row: row_of(*row),
            });
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
                "Heard just now; the radio signal is at least {threshold:.0} %."
            )),
            None => pass("Heard just now."),
        }
    }

    fn controller_check(&self, vehicle: &Vehicle) -> Outcome {
        let ids: Vec<&String> = std::iter::once(&vehicle.id)
            .chain(vehicle.aliases.iter())
            .collect();
        let driving: Vec<_> = self
            .state
            .controllers
            .values()
            .filter(|c| {
                c.connected
                    && c.claim.is_some()
                    && c.target.as_ref().is_some_and(|t| ids.contains(&t))
            })
            .collect();
        if driving.is_empty() {
            return pass(format!("No controller drives {}.", vehicle.name));
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
            // 0 is the throttle's middle: at rest, a gamepad's springs back
            // there and a radio's sits at the bottom.
            if sticks.throttle >= band {
                return not(
                    CheckStatus::Fail,
                    format!("Lower the throttle on {}.", controller.name),
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
            return pass(format!("{name}'s mission doesn't fly above terrain."));
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

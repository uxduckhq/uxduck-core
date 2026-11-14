//! Runs of a vehicle's checklist (S350, ADR-0038 K18, K19, K21, K27).
//!
//! The checklist is a template, and each time the preflight is done is a
//! run of it. A person starts one, with New preflight or the first tick,
//! and the ticks on the steps are its ticks. A person finishes it, which
//! stamps it with who and when, writes it to the vehicle's history as it
//! stood and locks its steps and its preflight ticks. A person can reopen
//! it, change it and finish it again: each finish is written to the
//! history as it stood, and none of them changes after. A new preflight
//! clears every tick and leaves the run before it in the history:
//! finished, as it was locked, or unfinished, as it stood then. Nothing
//! else starts or ends a run: not arming, a take-off, a landing or a
//! restart. A run never holds or opens Arm (K21). "Preflight finished" is
//! one of UXDuck's own checks for those who want the launch to wait for
//! it (K27, `finished_check`).

use serde_json::Value;
use uxduck_contracts::dto::checklist::{ChecklistRunsParams, ChecklistRunsResult};
use uxduck_contracts::errors::ApiError;
use uxduck_core::domain::checklist::{Checklist, Reopening, Run, RunRecord, Tick};
use uxduck_core::domain::history::{Author, HistoryEvent};
use uxduck_core::domain::preflight::{Check, CheckSource, OwnCheck};
use uxduck_core::domain::vehicle::Vehicle;

use crate::checklist::{invalid, no_vehicle};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// How many earlier runs `checklist/runs` answers with, unless asked.
const DEFAULT_RUNS: u32 = 20;
const MAX_RUNS: u32 = 100;
/// How many times one preflight is reopened, so its record stays small.
const MAX_REOPENINGS: usize = 20;

/// New preflight (K18, K29): every tick is cleared and a new run starts.
/// The run before it stays in the vehicle's history. A finished one is
/// there already, and one left unfinished goes there as it stands when
/// it has ticks in it or was reopened.
pub(crate) fn start(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle_id: &str,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(vehicle_id))?;
    engine.review_preflight(&vehicle.id);
    let before = engine.checklist_of(&vehicle.id);
    let by = engine.author(connection);
    let left = match &before.run {
        Some(run) if run.finished.is_none() => Some(run.clone()),
        Some(_) => None,
        // Ticks stored before runs are the first one.
        None => before.run_of_ticks(engine.new_run_id()),
    };
    if let Some(run) = left
        && (!run.reopenings.is_empty() || before.steps.iter().any(|s| s.tick.is_some()))
    {
        engine.keep_run(&vehicle, &before, &run, by.clone());
    }
    let mut steps = before.steps.clone();
    for step in &mut steps {
        step.tick = None;
    }
    let run = Run {
        id: engine.new_run_id(),
        started: Tick {
            by,
            at_us: engine.clock.now_us(),
        },
        finished: None,
        reopenings: Vec::new(),
        restarted_at_us: None,
    };
    let actor = engine.actor(connection);
    engine.store_checklist(actor, &before, steps, before.copied_from.clone(), Some(run));
    engine.preflight_answer(&vehicle.id)
}

/// Finish (K19): the run is stamped with who and when and written to the
/// vehicle's history as it stands. Its steps then stay as they are
/// (`Checklist::finished`) and its preflight steps take no tick
/// (`Checklist::locks`).
pub(crate) fn finish(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle_id: &str,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(vehicle_id))?;
    engine.review_preflight(&vehicle.id);
    let before = engine.checklist_of(&vehicle.id);
    if before.finished() {
        return Err(invalid(format!(
            "{}'s preflight is finished already. Reopen it or start a new preflight to finish again",
            vehicle.name
        )));
    }
    let by = engine.author(connection);
    let now = Tick {
        by: by.clone(),
        at_us: engine.clock.now_us(),
    };
    // With no run yet, its ticks began one, or it begins and ends now.
    let mut run = before.run.clone().unwrap_or_else(|| {
        let id = engine.new_run_id();
        before.run_of_ticks(id.clone()).unwrap_or(Run {
            id,
            started: now.clone(),
            finished: None,
            reopenings: Vec::new(),
            restarted_at_us: None,
        })
    });
    run.finished = Some(now);
    engine.keep_run(&vehicle, &before, &run, by);
    let actor = engine.actor(connection);
    engine.store_checklist(
        actor,
        &before,
        before.steps.clone(),
        before.copied_from.clone(),
        Some(run),
    );
    engine.preflight_answer(&vehicle.id)
}

/// Reopen (K19): a finished run is under way again, with its ticks, and
/// keeps who finished it and when and who reopened it and when. What was
/// written to the history at its finish stays as it was written.
pub(crate) fn reopen(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle_id: &str,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(vehicle_id))?;
    let before = engine.checklist_of(&vehicle.id);
    let Some((mut run, finished)) = before
        .run
        .clone()
        .and_then(|mut run| run.finished.take().map(|finished| (run, finished)))
    else {
        return Err(invalid(format!(
            "{}'s preflight is not finished, so there is nothing to reopen",
            vehicle.name
        )));
    };
    if run.reopenings.len() >= MAX_REOPENINGS {
        return Err(invalid(format!(
            "{}'s preflight has been reopened {MAX_REOPENINGS} times. Start a new preflight",
            vehicle.name
        )));
    }
    run.reopenings.push(Reopening {
        finished,
        reopened: Tick {
            by: engine.author(connection),
            at_us: engine.clock.now_us(),
        },
    });
    let actor = engine.actor(connection);
    engine.store_checklist(
        actor,
        &before,
        before.steps.clone(),
        before.copied_from.clone(),
        Some(run),
    );
    engine.preflight_answer(&vehicle.id)
}

/// `checklist/runs`: a vehicle's earlier preflights, newest first.
pub(crate) fn list(engine: &Engine, params: &ChecklistRunsParams) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let limit = params.limit.unwrap_or(DEFAULT_RUNS).clamp(1, MAX_RUNS) as usize;
    to_value(&ChecklistRunsResult {
        runs: engine
            .preflight_runs(vehicle)
            .into_iter()
            .take(limit)
            .map(Into::into)
            .collect(),
    })
}

impl Engine {
    /// Writes a run as it stands to the vehicle's history (K18, K19):
    /// each step, and the vehicle's, UXDuck's and plugins' checks as last
    /// read.
    fn keep_run(&mut self, vehicle: &Vehicle, checklist: &Checklist, run: &Run, by: Author) {
        let checks: Vec<Check> = self.state.preflights.get(&vehicle.id).map_or_else(
            || self.evaluate_preflight(vehicle).checks,
            |p| p.checks.clone(),
        );
        let record = RunRecord {
            id: run.id.clone(),
            started: run.started.clone(),
            finished: run.finished.clone(),
            reopenings: run.reopenings.clone(),
            steps: self.record_steps(vehicle, checklist, &checks),
            // The steps are above, and whether it is finished is the run
            // itself. Where a fix is leads nowhere later.
            checks: checks
                .into_iter()
                .filter(|c| c.source != CheckSource::Checklist && c.id != OwnCheck::Finished.id())
                .map(|c| Check { fix: None, ..c })
                .collect(),
        };
        self.record_by(
            &vehicle.id,
            HistoryEvent::PreflightRun {
                text: record.describe(),
                run: Box::new(record),
            },
            by,
        );
    }

    /// The vehicle restarted: its run says when (K27), so the launch can
    /// say that it has since the preflight was finished.
    pub(crate) fn checklist_restarted(&mut self, vehicle_id: &str) {
        let before = self.checklist_of(vehicle_id);
        let Some(mut run) = before.run.clone() else {
            return;
        };
        run.restarted_at_us = Some(self.clock.now_us());
        self.store_checklist(
            crate::preflight::actor(),
            &before,
            before.steps.clone(),
            before.copied_from.clone(),
            Some(run),
        );
    }
}

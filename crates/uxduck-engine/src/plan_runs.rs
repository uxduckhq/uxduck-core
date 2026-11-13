//! Flying plans in simulation (ADR-0011 M18, S293): `plan/simulate` starts
//! one simulated vehicle per run, all at once, on the ground at its plan's
//! home. Each run uploads its plan with only the plan's fence and its wind,
//! goes into Auto, arms, and flies to the end of its mission; then its
//! simulation is stopped. The runs are one operation in the Logbook, made
//! at once and kept up to date, whose `simulation` says how each went.
//!
//! As for a take-off (`launch.rs`), the engine has no timer: each run is
//! moved on whenever its vehicle is heard and once a second by the daemon.
//! Its commands and parameter writes are the simulation start's, so their
//! refusals are not told one by one; only the run's outcome is.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::link::{SimAutopilot, SimVehicle};
use uxduck_contracts::dto::operation::{
    PlanSimulateCheckResult, PlanSimulateParams, PlanSimulateResult,
};
use uxduck_contracts::dto::plan::PlanUploadStatus;
use uxduck_contracts::dto::sim::SimStartParams;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::controller::Controller;
use uxduck_core::domain::link::LinkStatus;
use uxduck_core::domain::operation::{Operation, RunState, Simulation, SimulationRun};
use uxduck_core::domain::parameter::{ParamKind, TransferStatus, parameter_id};
use uxduck_core::domain::plan::{
    Plan, PlanAutopilot, PlanBody, PlanFrame, PlanVehicleType, has_position,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, NO_CONNECTION};

/// Who a run's mission upload and its operation's changes are recorded
/// as. Its commands and parameter writes are the simulation start's.
pub const ACTOR: &str = "plan-simulation";

/// A run may fly this long, in the simulator's minutes, unless asked.
const LIMIT_MIN: u32 = 60;
const MAX_LIMIT_MIN: u32 = 600;
const MAX_WIND_MPS: u8 = 30;

/// How long, in real time, a run's vehicle may take to connect with its
/// parameters and mission read, for its plan to go up, and to be ready
/// to arm and armed, each separately.
const BOOT_US: u64 = 180_000_000;
const UPLOAD_US: u64 = 120_000_000;
const READY_US: u64 = 120_000_000;
/// A mode change or arm not taken yet is sent again after this, until the
/// readiness deadline.
const RETRY_US: u64 = 1_000_000;
/// Once its mission is complete, a vehicle still armed is left this long,
/// in the simulator's time, to land by itself before the run is ended as
/// finished, holding at its last item.
const HOLD_AT_END_US: u64 = 120_000_000;
/// Not heard for this long while flying, its simulator has stopped.
const SILENT_US: u64 = 30_000_000;

/// `FENCE_TYPE` bits (ArduPilot): the ceiling, and the polygons and circles.
const FENCE_ALT_MAX_BIT: u32 = 1;
const FENCE_POLYGON_BIT: u32 = 4;
/// Copter's `AUTO_OPTIONS`: arm in Auto, and take off without the throttle.
const AUTO_OPTIONS: f64 = 3.0;

#[cfg(test)]
#[path = "plan_runs/tests_handover.rs"]
mod handover_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Until the vehicle is heard with its parameters and mission read.
    Boot,
    /// Until its plan, fence and wind are up.
    Upload,
    /// Until it is ready to arm.
    Ready,
    /// Until it is in Auto.
    Auto,
    /// Until it is armed.
    Arm,
    /// In its mission until it disarms, ends or runs out of time.
    Flying,
}

/// One run's progress.
#[derive(Debug)]
struct Run {
    operation_id: String,
    plan_id: String,
    step: Step,
    started_us: u64,
    since_us: u64,
    retry_at_us: Option<u64>,
    /// The command the current step last sent, for its refusal.
    order: Option<String>,
    speedup: u8,
    wind_mps: Option<u8>,
    wind_from_deg: Option<u16>,
    /// The time limit in real time.
    limit_us: u64,
    /// When its mission was first seen complete.
    complete_at_us: Option<u64>,
    /// Its mission's landing item, where a disarm ends the mission.
    landing_seq: Option<u16>,
    handover: Option<Handover>,
}

#[derive(Debug)]
struct Handover {
    receipt_id: String,
    connection: ConnectionId,
    controller: Controller,
    mode: String,
}

impl Handover {
    fn held_by(&self, controller: &Controller) -> bool {
        controller.connected
            && controller.input_pause.is_none()
            && controller.claim.is_some()
            && controller.claim == self.controller.claim
            && controller.target == self.controller.target
            && controller.mode == self.controller.mode
            && controller.mapping_id == self.controller.mapping_id
    }
}

/// Runs by their simulation's link.
#[derive(Debug, Default)]
pub(crate) struct PlanRuns {
    running: BTreeMap<String, Run>,
    /// Runs an earlier UXDuck left unfinished have been marked stopped.
    tidied: bool,
}

/// One run asked for, checked.
struct Wanted {
    plan: Plan,
    vehicle: SimVehicle,
    speedup: u8,
    wind_mps: Option<u8>,
    wind_from_deg: Option<u16>,
}

/// The sequence number of its mission's last item when that item lands. Home
/// is item 0, so the plan's items start at 1.
fn landing_seq(body: &PlanBody) -> Option<u16> {
    const LANDINGS: [u16; 3] = [20, 21, 85];
    let last = body.items.last()?;
    LANDINGS
        .contains(&last.command)
        .then(|| u16::try_from(body.items.len()).ok())
        .flatten()
}

fn actor() -> Actor {
    Actor {
        client_id: ACTOR.into(),
        operator_id: None,
        identity: None,
    }
}

fn sim_vehicle(vehicle_type: PlanVehicleType) -> Option<SimVehicle> {
    match vehicle_type {
        PlanVehicleType::Multicopter => Some(SimVehicle::Multicopter),
        PlanVehicleType::Plane => Some(SimVehicle::Plane),
        PlanVehicleType::Vtol => Some(SimVehicle::Vtol),
        PlanVehicleType::Rover => Some(SimVehicle::Rover),
        PlanVehicleType::Boat => Some(SimVehicle::Boat),
        PlanVehicleType::Submarine | PlanVehicleType::Blimp => None,
    }
}

/// Where its vehicle starts: the planned home, else its first place.
fn home(body: &PlanBody) -> Option<(f64, f64, Option<f64>)> {
    body.planned_home
        .as_ref()
        .map(|h| (h.latitude_deg, h.longitude_deg, h.altitude_m))
        .or_else(|| {
            body.items
                .iter()
                .find(|i| has_position(i))
                .map(|i| (i.latitude_deg, i.longitude_deg, None))
        })
}

/// Which way it faces at the start: towards its first place away from
/// home, so a plane takes off along its way out.
fn heading(body: &PlanBody, from: (f64, f64)) -> f64 {
    let Some(to) = body
        .items
        .iter()
        .filter(|i| has_position(i))
        .map(|i| (i.latitude_deg, i.longitude_deg))
        .find(|to| (to.0 - from.0).abs() > 1e-6 || (to.1 - from.1).abs() > 1e-6)
    else {
        return 0.0;
    };
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlon = (to.1 - from.1).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Why a plan cannot be flown in simulation, if it cannot.
fn refusal(body: &PlanBody) -> Option<String> {
    let name = &body.name;
    if body.autopilot == PlanAutopilot::Px4 {
        return Some(format!(
            "{name} is a PX4 plan: plans fly in simulation on ArduPilot for now"
        ));
    }
    if sim_vehicle(body.vehicle_type).is_none() {
        return Some(format!(
            "{name} is for a vehicle that cannot fly a plan in simulation yet: multicopters, planes, VTOLs, rovers and boats can"
        ));
    }
    if body.items.is_empty() {
        return Some(format!("{name} has no items to fly"));
    }
    let Some((_, _, altitude_m)) = home(body) else {
        return Some(format!(
            "{name} has no place to start from: add a waypoint or a planned home"
        ));
    };
    if body.frame == PlanFrame::AboveSeaLevel && altitude_m.is_none() {
        return Some(format!(
            "{name}'s heights are above sea level, so its planned home needs its height"
        ));
    }
    // MAV_CMD_NAV_TAKEOFF and MAV_CMD_NAV_VTOL_TAKEOFF.
    let takes_off = body.items.iter().any(|i| matches!(i.command, 22 | 84));
    if body.vehicle_type.flies() && !takes_off {
        return Some(format!(
            "{name} has no take-off, and a simulated vehicle starts on the ground"
        ));
    }
    None
}

/// The runs recommended at once on this computer at a speed-up: about a
/// processor each at real time, one spare for UXDuck and the screen, and
/// fewer as each runs faster. A browser runs its simulators in the page,
/// so two.
fn recommended(processors: Option<usize>, speedup: u8) -> u32 {
    let Some(processors) = processors else {
        return 2;
    };
    let spare = u32::try_from(processors.saturating_sub(1)).unwrap_or(u32::MAX);
    (spare.saturating_mul(2) / (1 + u32::from(speedup))).max(1)
}

impl Engine {
    /// The runs asked for, each as many times as its count, and why any
    /// cannot start.
    fn wanted_runs(&self, params: &PlanSimulateParams) -> (Vec<Wanted>, Vec<String>) {
        let mut problems = Vec::new();
        let mut wanted = Vec::new();
        if !self.config.capabilities.simulator {
            problems.push("this UXDuck does not run simulated vehicles".to_owned());
        }
        if params
            .limit_min
            .is_some_and(|m| !(1..=MAX_LIMIT_MIN).contains(&m))
        {
            problems.push(format!(
                "a run's time limit is 1 to {MAX_LIMIT_MIN} minutes"
            ));
        }
        for run in &params.runs {
            let Some(plan) = self.state.plans.get(&run.plan_id) else {
                problems.push(format!("there is no plan {}", run.plan_id));
                continue;
            };
            let speedup = run.speedup.unwrap_or(1);
            let wind_mps = run.wind_mps.filter(|w| *w > 0);
            let problem = refusal(&plan.body)
                .or_else(|| {
                    (!(1..=10).contains(&speedup))
                        .then(|| "the speed-up is from 1 to 10 times real time".to_owned())
                })
                .or_else(|| {
                    wind_mps
                        .is_some_and(|w| w > MAX_WIND_MPS)
                        .then(|| format!("the wind is 0 to {MAX_WIND_MPS} m/s"))
                })
                .or_else(|| {
                    run.wind_from_deg
                        .is_some_and(|d| d >= 360)
                        .then(|| "the wind blows from 0 to 359°".to_owned())
                });
            if let Some(problem) = problem {
                problems.push(problem);
                continue;
            }
            let Some(vehicle) = sim_vehicle(plan.body.vehicle_type) else {
                continue;
            };
            for _ in 0..run.count.unwrap_or(1) {
                wanted.push(Wanted {
                    plan: plan.clone(),
                    vehicle,
                    speedup,
                    wind_mps,
                    wind_from_deg: wind_mps.map(|_| run.wind_from_deg.unwrap_or(0)),
                });
            }
        }
        if wanted.is_empty() && problems.is_empty() {
            problems.push("there is no run to start: add a plan to fly at least once".into());
        }
        // The installed daemon's own limit, from its ports, not this one's.
        if self.config.capabilities.runs_in == RunsIn::Installed {
            let simulations = self
                .state
                .links
                .values()
                .filter(|l| l.kind.is_simulator())
                .count();
            let free = usize::from(crate::sim::MAX_INSTANCES).saturating_sub(simulations);
            if wanted.len() > free {
                problems.push(format!(
                    "UXDuck keeps {} simulated vehicles at most, stopped ones too, and {free} more fit: delete some simulations first",
                    crate::sim::MAX_INSTANCES
                ));
            }
        }
        (wanted, problems)
    }

    fn simulate_check(&self, params: &PlanSimulateParams) -> PlanSimulateCheckResult {
        let (wanted, problems) = self.wanted_runs(params);
        let fastest = wanted.iter().map(|w| w.speedup).max().unwrap_or(1);
        let processors = self.system.processors();
        let recommended = recommended(processors, fastest);
        let runs = u32::try_from(wanted.len()).unwrap_or(u32::MAX);
        let warning = (runs > recommended).then(|| {
            let at = if fastest > 1 {
                format!(" at {fastest}×")
            } else {
                String::new()
            };
            let here = processors.map_or_else(
                || "in a browser".to_owned(),
                |n| format!("for this computer ({n} processors)"),
            );
            format!(
                "{runs} at once is more than the {recommended} recommended {here}{at}. They may run slower than asked, and a simulator that falls behind can fly badly."
            )
        });
        PlanSimulateCheckResult {
            runs,
            recommended,
            warning,
            problems,
        }
    }

    /// `plan/simulate`: every run's simulation started, and their operation.
    #[allow(
        clippy::too_many_lines,
        reason = "each run's start, then the operation"
    )]
    fn simulate(
        &mut self,
        connection: ConnectionId,
        params: &PlanSimulateParams,
    ) -> Result<PlanSimulateResult, ApiError> {
        let check = self.simulate_check(params);
        if let Some(problem) = check.problems.first() {
            return Err(ApiError::new(ErrorKind::InvalidValue, problem.clone()));
        }
        let (wanted, _) = self.wanted_runs(params);
        let limit_min = params.limit_min.unwrap_or(LIMIT_MIN);
        let operation_id = format!("op{}", &self.random.token()[..16]);
        let first = &wanted[0].plan.body.name;
        let name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map_or_else(
                || {
                    if wanted.iter().all(|w| &w.plan.body.name == first) {
                        format!("{first} · simulated")
                    } else {
                        "Plans · simulated".to_owned()
                    }
                },
                |n| n.chars().take(120).collect(),
            );
        let mut runs = Vec::new();
        for (n, want) in wanted.iter().enumerate() {
            let body = &want.plan.body;
            let Some((latitude_deg, longitude_deg, altitude_m)) = home(body) else {
                continue;
            };
            let short: String = body.name.chars().take(40).collect();
            let vehicle_name = format!("{short} {}", n + 1);
            let start = SimStartParams {
                vehicle: want.vehicle,
                autopilot: SimAutopilot::Ardupilot,
                latitude_deg,
                longitude_deg,
                altitude_m: Some(altitude_m.unwrap_or(0.0)),
                heading_deg: Some(heading(body, (latitude_deg, longitude_deg))),
                frame: None,
                speedup: Some(want.speedup),
                in_air_m: None,
                binary: None,
                allow_network: false,
                wipe: true,
                with_mission: false,
                traffic: false,
                name: Some(vehicle_name.clone()),
                forward: None,
            };
            let mut run = SimulationRun {
                link_id: String::new(),
                vehicle_name,
                plan_id: want.plan.id.clone(),
                plan_name: body.name.clone(),
                speedup: want.speedup,
                wind_mps: want.wind_mps,
                wind_from_deg: want.wind_from_deg,
                state: RunState::Starting,
                preparing: false,
                detail: None,
                flight_id: None,
                breached: false,
            };
            match crate::sim::start_link(self, connection, crate::sim::add_params(start), None) {
                Ok(link) => {
                    run.link_id.clone_from(&link.id);
                    run.vehicle_name = link.name;
                    let now = self.clock.now_us();
                    self.plan_runs.running.insert(
                        link.id,
                        Run {
                            operation_id: operation_id.clone(),
                            plan_id: want.plan.id.clone(),
                            step: Step::Boot,
                            started_us: now,
                            since_us: now,
                            retry_at_us: None,
                            order: None,
                            speedup: want.speedup,
                            wind_mps: want.wind_mps,
                            wind_from_deg: want.wind_from_deg,
                            limit_us: u64::from(limit_min) * 60_000_000 / u64::from(want.speedup),
                            complete_at_us: None,
                            landing_seq: landing_seq(&want.plan.body),
                            handover: None,
                        },
                    );
                }
                Err(error) => {
                    run.state = RunState::Failed;
                    run.detail = Some(format!(
                        "Its simulation did not start: {}.",
                        error.message.trim_end_matches('.')
                    ));
                }
            }
            runs.push(run);
        }
        let now = self.clock.now_us();
        let operation = Operation {
            id: operation_id,
            name: Some(name),
            auto: false,
            flight_ids: Vec::new(),
            from_us: now,
            to_us: now,
            video_ids: Vec::new(),
            clock_offsets_us: BTreeMap::new(),
            keep: false,
            simulation: Some(Simulation {
                runs,
                recommended: check.recommended,
                limit_min,
            }),
            version: 0,
        };
        let actor = self.actor(connection);
        let operation = self.put_operation(actor, operation);
        Ok(PlanSimulateResult {
            operation: self.operation_dto(&operation),
            warning: check.warning,
        })
    }

    /// The operation a run's link is in.
    fn operation_of_run(&self, link_id: &str) -> Option<String> {
        if let Some(run) = self.plan_runs.running.get(link_id) {
            return Some(run.operation_id.clone());
        }
        self.state
            .operations
            .values()
            .find(|o| {
                o.simulation
                    .as_ref()
                    .is_some_and(|s| s.runs.iter().any(|r| r.link_id == link_id))
            })
            .map(|o| o.id.clone())
    }

    /// Changes one run's line in its operation, and files a flight there.
    fn update_run(
        &mut self,
        link_id: &str,
        flight_id: Option<&str>,
        change: impl FnOnce(&mut SimulationRun),
    ) {
        let Some(operation_id) = self.operation_of_run(link_id) else {
            return;
        };
        let Some(mut operation) = self.state.operations.get(&operation_id).cloned() else {
            return;
        };
        let Some(run) = operation
            .simulation
            .as_mut()
            .and_then(|s| s.runs.iter_mut().find(|r| r.link_id == link_id))
        else {
            return;
        };
        let before = run.clone();
        change(run);
        if run.state.over() {
            run.preparing = false;
        }
        if *run == before && flight_id.is_none() {
            return;
        }
        if let Some(flight_id) = flight_id {
            self.take_flights_from_others(&actor(), &[flight_id.to_owned()], &operation_id);
            operation.flight_ids.push(flight_id.to_owned());
        }
        self.put_operation(actor(), operation);
    }

    /// A run ends: its line says how, and its simulation is stopped.
    fn end_run(&mut self, link_id: &str, state: RunState, detail: Option<String>) {
        if !self.plan_runs.running.contains_key(link_id) {
            return;
        }
        self.update_run(link_id, None, |run| {
            run.state = state;
            run.detail = detail;
        });
        self.plan_runs.running.remove(link_id);
        // Stopping one that is stopped already changes nothing.
        let _ = crate::sim::stop(self, NO_CONNECTION, link_id);
    }

    pub(crate) fn plan_run_command_sent(
        &mut self,
        connection: ConnectionId,
        vehicle: &Vehicle,
        command: &Command,
        receipt_id: &str,
    ) {
        let Command::SetMode { mode } = command else {
            return;
        };
        let Some(run) = self.plan_runs.running.get_mut(&vehicle.link_id) else {
            return;
        };
        run.handover = None;
        if run.step != Step::Flying
            || mode.eq_ignore_ascii_case("AUTO")
            || vehicle.presence != Presence::Online
            || !vehicle.armed
        {
            return;
        }
        let controller = self.state.controllers.values().find(|controller| {
            controller.connected
                && controller.claim.is_some()
                && controller.input_pause.is_none()
                && controller.target.as_deref() == Some(vehicle.id.as_str())
                && self.live.claimant.get(&controller.id) == Some(&connection)
                && self.live.routed_gripper.contains_key(&controller.id)
        });
        if let Some(controller) = controller {
            run.handover = Some(Handover {
                receipt_id: receipt_id.to_owned(),
                connection,
                controller: controller.clone(),
                mode: mode.clone(),
            });
        }
    }

    pub(crate) fn plan_run_after(&mut self, change: &Change) {
        for (link_id, run) in &mut self.plan_runs.running {
            let Some(handover) = &run.handover else {
                continue;
            };
            let invalid = match change {
                Change::ControllerUpserted { controller } => {
                    controller.id == handover.controller.id && !handover.held_by(controller)
                }
                Change::ControllerRemoved { id } => id == &handover.controller.id,
                Change::LinkUpserted { link } => {
                    &link.id == link_id && (link.paused || link.status != LinkStatus::Up)
                }
                Change::LinkRemoved { id } => id == link_id,
                _ => false,
            };
            if invalid {
                run.handover = None;
            }
        }
    }

    fn current_run_handover(&self, link_id: &str) -> Option<&Handover> {
        let handover = self.plan_runs.running.get(link_id)?.handover.as_ref()?;
        let controller = self.state.controllers.get(&handover.controller.id)?;
        let receipt = self.state.receipts.get(&handover.receipt_id)?;
        (handover.held_by(controller)
            && matches!(
                receipt.status,
                ReceiptStatus::Sent | ReceiptStatus::Accepted
            )
            && self.connections.contains_key(&handover.connection)
            && self.live.claimant.get(&controller.id) == Some(&handover.connection)
            && self.live.routed_gripper.contains_key(&controller.id))
        .then_some(handover)
    }

    pub(crate) fn plan_run_command_done(&mut self, receipt: &Receipt) {
        let Some(vehicle) = self.vehicle(&receipt.vehicle_id).cloned() else {
            return;
        };
        let link_id = &vehicle.link_id;
        let matches = self
            .plan_runs
            .running
            .get(link_id)
            .and_then(|run| run.handover.as_ref())
            .is_some_and(|handover| handover.receipt_id == receipt.id);
        if !matches {
            return;
        }
        if receipt.status == ReceiptStatus::Accepted {
            self.drive_plan_run(link_id);
        }
        let valid = receipt.status == ReceiptStatus::Accepted
            && self.current_run_handover(link_id).is_some()
            && vehicle.presence == Presence::Online
            && vehicle.armed
            && self
                .state
                .links
                .get(link_id)
                .is_some_and(|l| l.status == LinkStatus::Up)
            && self
                .plan_runs
                .running
                .get(link_id)
                .is_some_and(|run| run.step == Step::Flying && run.complete_at_us.is_none())
            && self.state.operations.values().any(|operation| {
                operation.simulation.as_ref().is_some_and(|simulation| {
                    simulation
                        .runs
                        .iter()
                        .any(|run| run.link_id == *link_id && !run.breached)
                })
            });
        if let Some(run) = self.plan_runs.running.get_mut(link_id) {
            run.handover = None;
        }
        if valid {
            self.update_run(link_id, None, |run| {
                run.state = RunState::EndedEarly;
                run.detail = Some("You took control. Its simulation is still running.".into());
            });
            self.plan_runs.running.remove(link_id);
        }
    }

    /// Its simulation was stopped, started again or removed by someone:
    /// the run is over.
    pub(crate) fn drop_plan_run(&mut self, link_id: &str) {
        if !self.plan_runs.running.contains_key(link_id) {
            return;
        }
        self.update_run(link_id, None, |run| {
            run.state = RunState::Stopped;
            run.detail = Some("Its simulation was stopped before the run ended.".into());
        });
        self.plan_runs.running.remove(link_id);
    }

    /// Its simulation crashed into the 3D scenery: the run ended early.
    pub(crate) fn crashed_plan_run(&mut self, link_id: &str) {
        if !self.plan_runs.running.contains_key(link_id) {
            return;
        }
        self.update_run(link_id, None, |run| {
            run.state = RunState::EndedEarly;
            run.detail = Some("It crashed into the 3D scenery.".into());
        });
        self.plan_runs.running.remove(link_id);
    }

    /// A run's plan finished going up, or did not.
    pub(crate) fn plan_run_uploaded(
        &mut self,
        vehicle_id: &str,
        status: PlanUploadStatus,
        detail: Option<&str>,
    ) {
        let Some(link_id) = self.vehicle(vehicle_id).map(|v| v.link_id.clone()) else {
            return;
        };
        if self
            .plan_runs
            .running
            .get(&link_id)
            .is_none_or(|run| run.step != Step::Upload)
        {
            return;
        }
        match status {
            PlanUploadStatus::Verified | PlanUploadStatus::Different => {
                self.next_run_step(&link_id, Step::Ready);
                self.drive_plan_run(&link_id);
            }
            PlanUploadStatus::Failed | PlanUploadStatus::NotSent | PlanUploadStatus::Cancelled => {
                let why = detail.map_or_else(
                    || "Its plan did not upload.".to_owned(),
                    |d| format!("Its plan did not upload: {d}."),
                );
                self.end_run(&link_id, RunState::Failed, Some(why));
            }
        }
    }

    fn next_run_step(&mut self, link_id: &str, step: Step) {
        let now = self.clock.now_us();
        if let Some(run) = self.plan_runs.running.get_mut(link_id) {
            run.step = step;
            run.since_us = now;
            run.retry_at_us = None;
            run.order = None;
        }
        let (state, preparing, detail) = match step {
            Step::Upload => (RunState::Uploading, false, None),
            Step::Flying => (RunState::Flying, false, None),
            Step::Ready => (
                RunState::Uploading,
                true,
                Some("Waiting for pre-arm checks.".into()),
            ),
            Step::Auto => (RunState::Uploading, true, Some("Switching to Auto.".into())),
            Step::Arm => (RunState::Uploading, true, Some("Arming.".into())),
            Step::Boot => return,
        };
        self.update_run(link_id, None, |run| {
            run.state = state;
            run.preparing = preparing;
            run.detail = detail;
        });
    }

    /// Moves every run on: deadlines pass also while a simulator says
    /// nothing. The first time, runs an earlier UXDuck left unfinished are
    /// marked stopped, since their simulations did not outlive it.
    pub fn drive_plan_runs(&mut self) {
        if !self.plan_runs.tidied {
            self.plan_runs.tidied = true;
            self.tidy_plan_runs();
        }
        let links: Vec<String> = self.plan_runs.running.keys().cloned().collect();
        for link_id in links {
            self.drive_plan_run(&link_id);
        }
    }

    fn tidy_plan_runs(&mut self) {
        let left: Vec<Operation> = self
            .state
            .operations
            .values()
            .filter(|o| o.simulation.as_ref().is_some_and(|s| !s.over()))
            .cloned()
            .collect();
        for mut operation in left {
            if let Some(simulation) = operation.simulation.as_mut() {
                for run in &mut simulation.runs {
                    if !run.state.over() && !self.plan_runs.running.contains_key(&run.link_id) {
                        run.state = RunState::Stopped;
                        run.preparing = false;
                        run.detail = Some("UXDuck stopped before the run ended.".into());
                    }
                }
            }
            self.put_operation(actor(), operation);
        }
    }

    pub(crate) fn drive_plan_run_of_vehicle(&mut self, vehicle_id: &str) {
        let Some(link_id) = self.vehicle(vehicle_id).map(|v| v.link_id.clone()) else {
            return;
        };
        if self.plan_runs.running.contains_key(&link_id) {
            self.drive_plan_run(&link_id);
        }
    }

    fn run_vehicle(&self, link_id: &str) -> Option<Vehicle> {
        self.state
            .vehicles
            .values()
            .find(|v| v.link_id == link_id && !v.replay)
            .cloned()
    }

    fn drive_plan_run(&mut self, link_id: &str) {
        let now = self.clock.now_us();
        let Some(run) = self.plan_runs.running.get(link_id) else {
            return;
        };
        if run.retry_at_us.is_some_and(|at| now < at) {
            return;
        }
        let (step, started_us) = (run.step, run.started_us);
        let waited = now.saturating_sub(run.since_us);
        let vehicle = self.run_vehicle(link_id);
        let online = vehicle
            .clone()
            .filter(|v| v.presence == Presence::Online && v.last_heartbeat_us >= started_us);
        match step {
            Step::Boot => {
                let ready = online.as_ref().is_some_and(|v| {
                    let parameters = self.parameter_set(&v.id);
                    parameters.status == TransferStatus::Synced
                        && parameters.synced_at_us.is_some_and(|at| at >= started_us)
                        && self.mission(&v.id).status == TransferStatus::Synced
                });
                if ready {
                    self.next_run_step(link_id, Step::Upload);
                    self.upload_run(link_id);
                } else if waited > BOOT_US {
                    let why = if online.is_some() {
                        "Its parameters and mission never finished downloading."
                    } else {
                        "Its simulated vehicle never connected."
                    };
                    self.end_run(link_id, RunState::Failed, Some(why.into()));
                }
            }
            Step::Upload => {
                if waited > UPLOAD_US {
                    let why = "Its plan took too long to upload.".to_owned();
                    self.end_run(link_id, RunState::Failed, Some(why));
                }
            }
            Step::Ready => {
                let Some(vehicle) = online else {
                    return;
                };
                if self.run_ready_to_arm(&vehicle.id, started_us) {
                    self.next_run_step(link_id, Step::Auto);
                    self.drive_plan_run(link_id);
                } else {
                    let detail = self.run_prearm_detail(&vehicle.id, started_us);
                    if waited > READY_US {
                        let why = format!(
                            "Its pre-arm checks did not pass in time. {detail} Check Messages and terrain availability, then try again."
                        );
                        self.end_run(link_id, RunState::Failed, Some(why));
                    } else {
                        self.update_run(link_id, None, |run| run.detail = Some(detail));
                    }
                }
            }
            Step::Auto | Step::Arm => {
                if let Some(vehicle) = online {
                    self.go_on_auto(link_id, step, &vehicle, waited);
                }
            }
            Step::Flying => self.watch_run(link_id, vehicle.as_ref(), waited),
        }
    }

    /// Into Auto, then armed: sent again each second until it is, or the
    /// readiness deadline passes.
    fn go_on_auto(&mut self, link_id: &str, step: Step, vehicle: &Vehicle, waited: u64) {
        let done = if step == Step::Auto {
            vehicle
                .mode
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case("AUTO"))
        } else {
            vehicle.armed
        };
        if done {
            let next = if step == Step::Auto {
                Step::Arm
            } else {
                Step::Flying
            };
            self.next_run_step(link_id, next);
            self.drive_plan_run(link_id);
            return;
        }
        if waited > READY_US {
            let refused = self.run_refusal(link_id);
            let why = match (step, refused) {
                (Step::Auto, Some(why)) => format!("It would not go into Auto: {why}."),
                (Step::Auto, None) => "It would not go into Auto.".to_owned(),
                (_, Some(why)) => format!("It would not arm: {why}."),
                (_, None) => "It would not arm.".to_owned(),
            };
            self.end_run(link_id, RunState::Failed, Some(why));
            return;
        }
        let command = if step == Step::Auto {
            Command::SetMode {
                mode: "AUTO".into(),
            }
        } else {
            Command::Arm
        };
        let id = self.send_quietly(link_id, vehicle, command);
        let now = self.clock.now_us();
        if let Some(run) = self.plan_runs.running.get_mut(link_id) {
            run.order = Some(id);
            run.retry_at_us = Some(now + RETRY_US);
        }
    }

    /// Watches a run in its mission until it ends.
    fn watch_run(&mut self, link_id: &str, vehicle: Option<&Vehicle>, waited: u64) {
        let now = self.clock.now_us();
        let Some(vehicle) = vehicle else {
            let why = "Its simulated vehicle went away.".to_owned();
            self.end_run(link_id, RunState::EndedEarly, Some(why));
            return;
        };
        let Some(run) = self.plan_runs.running.get(link_id) else {
            return;
        };
        let (speedup, limit_us, landing_seq, operation_id) = (
            run.speedup,
            run.limit_us,
            run.landing_seq,
            run.operation_id.clone(),
        );
        let filed = self
            .state
            .operations
            .get(&operation_id)
            .and_then(|o| o.simulation.as_ref())
            .and_then(|s| s.runs.iter().find(|r| r.link_id == link_id))
            .map(|r| (r.flight_id.clone(), r.breached));
        let (filed_flight, was_breached) = filed.unwrap_or_default();
        let flight = self
            .open_flight(&vehicle.id)
            .map(|f| f.id.clone())
            .filter(|id| filed_flight.as_deref() != Some(id.as_str()));
        let breached = self
            .telemetry_value(&vehicle.id, "fence.breached")
            .is_some_and(|v| v > 0.5);
        if flight.is_some() || (breached && !was_breached) {
            self.update_run(link_id, flight.as_deref(), |run| {
                if let Some(id) = &flight {
                    run.flight_id = Some(id.clone());
                }
                run.breached |= breached;
            });
        }
        // MISSION_CURRENT's state: 5 is complete.
        let complete = self
            .telemetry_value(&vehicle.id, "mission.state")
            .is_some_and(|s| (s - 5.0).abs() < 0.5);
        let complete_at_us = self.plan_runs.running.get_mut(link_id).and_then(|run| {
            if complete && run.complete_at_us.is_none() {
                run.complete_at_us = Some(now);
            }
            run.complete_at_us
        });
        if !vehicle.armed {
            let current = self.telemetry_value(&vehicle.id, "mission.current");
            // ArduPilot disarms on its landing item without reporting the
            // mission complete.
            let landed = landing_seq
                .zip(current)
                .is_some_and(|(landing, seq)| seq >= f64::from(landing) - 0.5);
            if complete_at_us.is_some() || landed {
                self.end_run(link_id, RunState::Finished, None);
            } else {
                let at = current.map_or_else(String::new, |seq| format!(" at item {seq:.0}"));
                let why = format!("It disarmed{at}, before the end of its mission.");
                self.end_run(link_id, RunState::EndedEarly, Some(why));
            }
            return;
        }
        if vehicle.presence != Presence::Online
            && now.saturating_sub(vehicle.last_heartbeat_us) > SILENT_US
        {
            let why = "Its simulator stopped answering.".to_owned();
            self.end_run(link_id, RunState::EndedEarly, Some(why));
            return;
        }
        if let Some(at) = complete_at_us
            && now.saturating_sub(at) > HOLD_AT_END_US / u64::from(speedup)
        {
            let why = "It ended its mission in the air, holding at its last item.".to_owned();
            self.end_run(link_id, RunState::Finished, Some(why));
            return;
        }
        // A breach takes it out of Auto by itself (RTL, Land, Brake): the
        // run goes on to its landing and reports the breach.
        let in_auto = vehicle
            .mode
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("AUTO"));
        let handover_pending = self.current_run_handover(link_id).is_some_and(|handover| {
            vehicle
                .mode
                .as_deref()
                .is_some_and(|mode| mode.eq_ignore_ascii_case(&handover.mode))
        });
        if !in_auto && complete_at_us.is_none() && !(breached || was_breached || handover_pending) {
            let mode = vehicle.mode.as_deref().unwrap_or("another mode");
            let why = format!("It left Auto for {mode} before the end of its mission.");
            self.end_run(link_id, RunState::EndedEarly, Some(why));
            return;
        }
        if waited > limit_us {
            let minutes = limit_us * u64::from(speedup) / 60_000_000;
            let why = format!("It was still flying after {minutes} minutes.");
            self.end_run(link_id, RunState::TimedOut, Some(why));
        }
    }

    /// The pre-arm checks pass in this simulator run.
    fn run_ready_to_arm(&self, vehicle_id: &str, started_us: u64) -> bool {
        self.telemetry(vehicle_id).is_some_and(|telemetry| {
            telemetry
                .items
                .iter()
                .find(|i| i.key == "arming.ready")
                .is_some_and(|i| i.time_us >= started_us && i.value == Some(1.0))
        })
    }

    fn run_prearm_detail(&self, vehicle_id: &str, started_us: u64) -> String {
        if let Some(why) = self.terrain_unavailable_for(vehicle_id) {
            return format!(
                "Terrain unavailable: {why}. The simulation stays disarmed. Check your connection and Terrain in Settings."
            );
        }
        let value = |key: &str| {
            self.telemetry(vehicle_id)?
                .items
                .iter()
                .find(|i| i.key == key && i.time_us >= started_us)?
                .value
        };
        if let Some(pending) = value("terrain.pending").filter(|v| *v > 0.0) {
            let loaded =
                value("terrain.loaded").map_or_else(String::new, |v| format!(", {v:.0} loaded"));
            return format!(
                "Loading terrain: {pending:.0} blocks pending{loaded}. Waiting for the vehicle's pre-arm checks. It stays disarmed."
            );
        }
        let reasons = self.prearm_reasons(vehicle_id);
        if reasons.is_empty() {
            "Waiting for the vehicle's pre-arm checks. It stays disarmed.".into()
        } else {
            format!("Waiting for pre-arm checks: {}.", reasons.join(", "))
        }
    }

    /// Why the step's last command was refused, as the vehicle said.
    fn run_refusal(&self, link_id: &str) -> Option<String> {
        let id = self.plan_runs.running.get(link_id)?.order.as_ref()?;
        let receipt = self.state.receipts.get(id)?;
        if receipt.status == ReceiptStatus::Rejected {
            receipt.detail.clone()
        } else {
            None
        }
    }

    /// Sends a run's plan with only the plan's fence types, Auto's options
    /// for a copter, and its wind.
    fn upload_run(&mut self, link_id: &str) {
        let Some(run) = self.plan_runs.running.get(link_id) else {
            return;
        };
        let (plan_id, wind_mps, wind_from_deg) =
            (run.plan_id.clone(), run.wind_mps, run.wind_from_deg);
        let (Some(vehicle), Some(plan)) = (
            self.run_vehicle(link_id),
            self.state.plans.get(&plan_id).cloned(),
        ) else {
            let why = "Its plan was deleted.".to_owned();
            self.end_run(link_id, RunState::Failed, Some(why));
            return;
        };
        let mut job = match crate::plans::plan_job(self, &plan.body, vehicle.id.clone()) {
            Ok(job) => job,
            Err(why) => {
                self.end_run(link_id, RunState::Failed, Some(why.message));
                return;
            }
        };
        // A simulator's own fence (ArduCopter's 150 m circle) is not the
        // plan's: only the plan's fence types are on.
        let fence = &plan.body.fence;
        let mut bits = 0u32;
        if !fence.polygons.is_empty() || !fence.circles.is_empty() {
            bits |= FENCE_POLYGON_BIT;
        }
        if fence.settings.ceiling_m.is_some() {
            bits |= FENCE_ALT_MAX_BIT;
        }
        job.fence_settings
            .retain(|(name, _, _)| name != "FENCE_TYPE" && name != "FENCE_ENABLE");
        let mut writes = vec![
            ("FENCE_TYPE", f64::from(bits)),
            ("FENCE_ENABLE", if bits == 0 { 0.0 } else { 1.0 }),
            ("SIM_WIND_SPD", f64::from(wind_mps.unwrap_or(0))),
            ("SIM_WIND_DIR", f64::from(wind_from_deg.unwrap_or(0))),
        ];
        if plan.body.vehicle_type == PlanVehicleType::Multicopter {
            writes.push(("AUTO_OPTIONS", AUTO_OPTIONS));
        }
        if crate::plan_examples::needs_autoland(&plan.body) {
            writes.push(("RTL_AUTOLAND", crate::plan_examples::RTL_AUTOLAND));
        }
        for (name, value) in writes {
            let current = self
                .state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| (p.value, p.kind));
            match current {
                Some((now, kind)) if (now - value).abs() > 0.05 => {
                    job.fence_settings.push((name.to_owned(), value, kind));
                }
                None if name == "AUTO_OPTIONS" => {
                    job.fence_settings
                        .push((name.to_owned(), value, ParamKind::Int32));
                }
                None if name == "RTL_AUTOLAND" => {
                    job.fence_settings
                        .push((name.to_owned(), value, ParamKind::Int8));
                }
                Some(_) | None => {}
            }
        }
        if let Err(error) = self.upload_plan_mission_as(actor(), job) {
            let why = format!("Its plan did not upload: {}.", error.message);
            self.end_run(link_id, RunState::Failed, Some(why));
        }
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::PlanSimulateCheck(params) => {
            require(Capability::Read)?;
            to_value(&engine.simulate_check(&params))
        }
        ClientRequest::PlanSimulate(params) => {
            require(Capability::Config)?;
            to_value(&engine.simulate(connection, &params)?)
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a plan simulation method",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use uxduck_core::domain::plan::PlanItem;
    use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
    use uxduck_core::domain::vehicle::Observation;

    use crate::runtime::{
        Clock, CommandOrder, Forget, LinkPort, MemoryCredentials, NoAgent, NoPlugins, NoSystem,
        Ports, Random,
    };
    use crate::{EngineConfig, Loaded};

    use super::*;

    #[derive(Clone, Default)]
    struct Commands(Arc<Mutex<Vec<CommandOrder>>>);

    impl LinkPort for Commands {
        fn open(&mut self, _: &uxduck_core::domain::link::Link) {}
        fn close(&mut self, _: &str) {}
        fn command(&mut self, order: CommandOrder) {
            self.0.lock().expect("commands").push(order);
        }
    }

    struct Time(Arc<std::sync::atomic::AtomicU64>);

    impl Clock for Time {
        fn now_us(&self) -> u64 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    struct Ids(u8);

    impl Random for Ids {
        fn fill(&mut self, bytes: &mut [u8]) {
            self.0 = self.0.wrapping_add(1);
            bytes.fill(self.0);
        }
    }

    fn waiting_run() -> (Engine, Commands, Arc<std::sync::atomic::AtomicU64>) {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let commands = Commands::default();
        let mut engine = Engine::new(
            EngineConfig {
                daemon_version: "test".into(),
                capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
                ui_url: String::new(),
                checkpoint_every: 1_000,
                models: Vec::new(),
                agent_provider: "fake".into(),
                about: crate::HostAbout::default(),
            },
            Box::new(Time(now.clone())),
            Box::new(Ids(0)),
            Ports {
                persistence: Box::new(Forget),
                agent: Box::new(NoAgent),
                credentials: Box::new(MemoryCredentials::default()),
                links: Box::new(commands.clone()),
                system: Box::new(NoSystem::default()),
                plugins: Box::new(NoPlugins),
            },
            Loaded::default(),
        );
        let link = serde_json::from_value(json!({
            "id": "sim", "name": "Simulation", "createdAtUs": 1,
            "kind": { "type": "simulator", "vehicle": "plane", "latitudeDeg": 53.5,
                "longitudeDeg": -4.9, "altitudeM": 20, "headingDeg": 0 },
            "status": "up", "policy": "read-write", "topology": "auto", "version": 1
        }))
        .expect("simulation link");
        engine.state.links.insert("sim".into(), link);
        engine.vehicle_observed(Observation {
            connections: None,
            id: "vehicle".into(),
            aliases: Vec::new(),
            link_id: "sim".into(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "FixedWing".into(),
            uid: None,
            firmware: None,
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 1_000,
            armed: false,
            mode: Some("MANUAL".into()),
        });
        let operation = serde_json::from_value(json!({
            "id": "operation", "flightIds": [], "fromUs": 1, "toUs": 1, "version": 1,
            "simulation": { "recommended": 1, "limitMin": 60, "runs": [{
                "linkId": "sim", "vehicleName": "Simulation", "planId": "plan",
                "planName": "Terrain plan", "speedup": 5, "state": "uploading"
            }] }
        }))
        .expect("operation");
        engine
            .state
            .operations
            .insert("operation".into(), operation);
        engine.plan_runs.tidied = true;
        engine.plan_runs.running.insert(
            "sim".into(),
            Run {
                operation_id: "operation".into(),
                plan_id: "plan".into(),
                step: Step::Upload,
                started_us: 1_000,
                since_us: 1_000,
                retry_at_us: None,
                order: None,
                speedup: 5,
                wind_mps: None,
                wind_from_deg: None,
                limit_us: 60_000_000,
                complete_at_us: None,
                landing_seq: None,
                handover: None,
            },
        );
        (engine, commands, now)
    }

    fn readiness(engine: &mut Engine, pending: f64, loaded: f64, ready: f64, at: u64) {
        engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "vehicle".into(),
            positions: Vec::new(),
            items: [
                ("terrain.pending", pending),
                ("terrain.loaded", loaded),
                ("arming.ready", ready),
            ]
            .into_iter()
            .map(|(key, value)| TelemetryItem {
                key: key.into(),
                label: key.into(),
                value: Some(value),
                text: None,
                unit: Unit::Count,
                source: "test".into(),
                component_id: 1,
                time_us: at,
            })
            .collect(),
        });
    }

    fn filed_run(engine: &Engine) -> &SimulationRun {
        &engine.state.operations["operation"]
            .simulation
            .as_ref()
            .expect("simulation")
            .runs[0]
    }

    #[test]
    fn preparation_keeps_the_legacy_wire_state() {
        let (mut engine, _, _) = waiting_run();
        readiness(&mut engine, 48.0, 400.0, 0.0, 1_000);
        engine.plan_run_uploaded("vehicle", PlanUploadStatus::Verified, None);
        let dto = engine.operation_dto(&engine.state.operations["operation"]);
        let wire = serde_json::to_value(dto).expect("operation on the wire");
        assert_eq!(wire["simulation"]["runs"][0]["state"], "uploading");
        assert_eq!(wire["simulation"]["runs"][0]["preparing"], true);
    }

    #[test]
    fn an_uploaded_plan_reports_terrain_preparation_without_launching_early() {
        let (mut engine, commands, now) = waiting_run();
        readiness(&mut engine, 48.0, 400.0, 0.0, 1_000);
        engine.plan_run_uploaded("vehicle", PlanUploadStatus::Verified, None);
        assert!(filed_run(&engine).preparing);
        assert!(
            filed_run(&engine)
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("48 blocks pending") && d.contains("400 loaded"))
        );
        assert_eq!(
            filed_run(&engine).detail.as_deref(),
            Some(
                "Loading terrain: 48 blocks pending, 400 loaded. Waiting for the vehicle's pre-arm checks. It stays disarmed."
            )
        );
        assert!(commands.0.lock().expect("commands").is_empty());
        let version = engine.state.operations["operation"].version;
        engine.drive_plan_runs();
        assert_eq!(engine.state.operations["operation"].version, version);
        readiness(&mut engine, 0.0, 560.0, 0.0, 1_000);
        engine.drive_plan_runs();
        assert!(
            commands.0.lock().expect("commands").is_empty(),
            "terrain alone does not pass pre-arm checks"
        );
        readiness(&mut engine, 0.0, 560.0, 1.0, 1_000);
        engine.drive_plan_runs();
        assert!(
            commands
                .0
                .lock()
                .expect("commands")
                .iter()
                .any(|c| matches!(&c.command, Command::SetMode { mode } if mode == "AUTO"))
        );
        assert!(
            !commands
                .0
                .lock()
                .expect("commands")
                .iter()
                .any(|c| matches!(c.command, Command::Arm)),
            "Auto must be confirmed before arming"
        );
        engine
            .state
            .vehicles
            .get_mut("vehicle")
            .expect("vehicle")
            .mode = Some("AUTO".into());
        now.store(1_000 + RETRY_US, std::sync::atomic::Ordering::SeqCst);
        engine.drive_plan_runs();
        assert!(
            commands
                .0
                .lock()
                .expect("commands")
                .iter()
                .any(|c| matches!(c.command, Command::Arm))
        );
        engine
            .state
            .vehicles
            .get_mut("vehicle")
            .expect("vehicle")
            .armed = true;
        now.store(1_000 + 2 * RETRY_US, std::sync::atomic::Ordering::SeqCst);
        engine.drive_plan_runs();
        assert_eq!(filed_run(&engine).state, RunState::Flying);
        assert!(!filed_run(&engine).preparing);
        assert!(
            filed_run(&engine).detail.is_none(),
            "preparation detail clears once flying"
        );
    }

    #[test]
    fn unavailable_terrain_names_the_failure_and_times_out_without_a_command() {
        struct TerrainHost;
        impl crate::terrain::TerrainPort for TerrainHost {
            fn order(&mut self, _: uxduck_terrain::Order) {}
        }
        let (mut engine, commands, now) = waiting_run();
        engine.set_terrain(Box::new(TerrainHost), None);
        readiness(&mut engine, 56.0, 0.0, 0.0, 1_000);
        engine.terrain_requested(
            "vehicle",
            uxduck_mavlink::terrain::Block {
                lat: 535_000_000,
                lon: -49_000_000,
                spacing: 100,
            },
            1,
        );
        for key in [
            uxduck_terrain::manifest::MANIFEST,
            uxduck_terrain::manifest::SIGNATURE,
        ] {
            engine.terrain_arrived(uxduck_terrain::Arrived::Read {
                key: key.into(),
                bytes: None,
            });
            engine.terrain_arrived(uxduck_terrain::Arrived::Fetched {
                key: key.into(),
                result: Err(uxduck_terrain::FetchError {
                    offline: false,
                    message: "503 Service Unavailable".into(),
                }),
            });
        }
        engine.plan_run_uploaded("vehicle", PlanUploadStatus::Verified, None);
        let detail = filed_run(&engine)
            .detail
            .as_deref()
            .expect("terrain failure");
        assert!(
            detail.contains("Terrain unavailable")
                && detail.contains("503")
                && detail.contains("Settings"),
            "{detail}"
        );
        assert!(commands.0.lock().expect("commands").is_empty());
        now.store(READY_US + 2_000, std::sync::atomic::Ordering::SeqCst);
        engine.drive_plan_runs();
        assert_eq!(filed_run(&engine).state, RunState::Failed);
        assert!(!filed_run(&engine).preparing);
        let detail = filed_run(&engine)
            .detail
            .as_deref()
            .expect("timeout reason");
        assert!(
            detail.contains("503") && detail.contains("try again"),
            "{detail}"
        );
        assert!(commands.0.lock().expect("commands").is_empty());
    }

    #[test]
    fn recovering_a_preparing_run_clears_terminal_preparation_metadata() {
        let (mut engine, commands, _) = waiting_run();
        readiness(&mut engine, 48.0, 400.0, 0.0, 1_000);
        engine.plan_run_uploaded("vehicle", PlanUploadStatus::Verified, None);
        assert!(filed_run(&engine).preparing);
        engine.plan_runs.running.clear();
        engine.plan_runs.tidied = false;
        engine.drive_plan_runs();
        assert_eq!(filed_run(&engine).state, RunState::Stopped);
        assert!(!filed_run(&engine).preparing);
        assert!(commands.0.lock().expect("commands").is_empty());
    }

    #[test]
    fn stale_readiness_cannot_launch_a_preparing_run() {
        let (mut engine, commands, _) = waiting_run();
        readiness(&mut engine, 0.0, 560.0, 1.0, 999);
        engine.plan_run_uploaded("vehicle", PlanUploadStatus::Verified, None);
        assert_eq!(filed_run(&engine).state, RunState::Uploading);
        assert!(filed_run(&engine).preparing);
        assert!(!filed_run(&engine).state.over());
        assert!(commands.0.lock().expect("commands").is_empty());
    }

    #[test]
    fn the_recommendation_follows_the_processors_and_the_speed_up() {
        assert_eq!(recommended(Some(8), 1), 7);
        assert_eq!(recommended(Some(8), 3), 3);
        assert_eq!(recommended(Some(16), 10), 2);
        assert_eq!(recommended(Some(1), 1), 1);
        assert_eq!(recommended(Some(2), 10), 1);
        assert_eq!(recommended(None, 1), 2);
    }

    fn item(command: u16, latitude_deg: f64, longitude_deg: f64) -> PlanItem {
        PlanItem {
            command,
            frame: 3,
            params: [None; 4],
            latitude_deg,
            longitude_deg,
            altitude_m: 50.0,
            autocontinue: true,
            pattern: None,
        }
    }

    #[test]
    fn a_mission_ending_in_a_landing_ends_on_its_last_item() {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.items.push(item(22, 1.0, 1.0));
        body.items.push(item(16, 1.001, 1.0));
        assert_eq!(landing_seq(&body), None);
        body.items.push(item(20, 0.0, 0.0));
        assert_eq!(landing_seq(&body), Some(3));
        body.items[2].command = 21;
        assert_eq!(landing_seq(&body), Some(3));
    }

    #[test]
    fn a_vehicle_starts_at_home_facing_its_way_out() {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Plane,
            PlanAutopilot::Ardupilot,
        );
        body.items.push(item(22, 1.0, 1.0));
        body.items.push(item(16, 1.001, 0.999));
        assert_eq!(home(&body), Some((1.0, 1.0, None)));
        assert!((heading(&body, (1.0, 1.0)) - 315.0).abs() < 0.5);
        assert_eq!(refusal(&body), None);
    }

    #[test]
    fn plans_it_cannot_fly_say_why() {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        assert_eq!(refusal(&body).as_deref(), Some("Field has no items to fly"));
        body.items.push(item(16, 1.0, 1.0));
        assert_eq!(
            refusal(&body).as_deref(),
            Some("Field has no take-off, and a simulated vehicle starts on the ground")
        );
        body.autopilot = PlanAutopilot::Px4;
        assert!(refusal(&body).is_some_and(|r| r.contains("PX4")));
        let mut boat = PlanBody::new(
            "Lake".into(),
            PlanVehicleType::Boat,
            PlanAutopilot::Ardupilot,
        );
        boat.items.push(item(16, 1.0, 1.0));
        assert_eq!(refusal(&boat), None);
    }
}

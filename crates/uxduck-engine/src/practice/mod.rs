//! Practice (ADR-0024, S100): exercises on a simulated vehicle UXDuck
//! started, judged here, not in the page (X3). An attempt starts the
//! vehicle again at the exercise's height (X4, through "Start again"),
//! then, as the client `practice-start`, moves it to the exercise's start
//! and sets its battery; only then is it judged, on every change to the
//! vehicle, by the checks in the exercise's data (`judge.rs`). Its result
//! is journaled on the operator (X7).

mod exercises;
pub mod judge;

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::practice::{
    PracticeAnswerResult, PracticeAttempt, PracticeBest, PracticeExercise, PracticeListResult,
    PracticeQuestion, PracticeStatusResult, PracticeStep,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::history::{EntryKind, HistoryEvent, ParamSource};
use uxduck_core::domain::link::{LinkKind, SimAutopilot, SimVehicle};
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::practice::{Attempt, AttemptState, Best, Exercise, StartParameter};
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;
use crate::state::State;
use crate::{ConnectionId, Engine};
pub use exercises::{all as exercises, find as exercise};
use judge::{Sample, Verdict, View};

/// Who sets an attempt up (X4), never counted as the operator (X5).
pub const ACTOR: &str = "practice-start";
/// Close enough to the exercise's start to begin judging.
const AT_START_M: f64 = 15.0;
/// The same for a plane or VTOL plane, which circles a place it is sent to,
/// 60 m out by ArduPilot's default, rather than stopping there (maintainer,
/// 2026-10-13: exercises keep the vehicle's own behaviour).
const AT_START_CIRCLING_M: f64 = 100.0;
/// `MAV_CMD_NAV_WAYPOINT` and `MAV_CMD_NAV_SPLINE_WAYPOINT`.
const WAYPOINTS: [u16; 2] = [16, 82];
/// ArduPilot's `MAV_CMD_BATTERY_RESET`: battery mask, then percentage.
const BATTERY_RESET: u16 = 42_651;

fn practice_start() -> Actor {
    Actor {
        client_id: ACTOR.into(),
        operator_id: None,
        identity: None,
    }
}

/// What an attempt's set-up has sent, kept while it is starting.
#[derive(Debug, Default)]
pub(crate) struct Setup {
    parameters: bool,
    /// What the start's parameters were before, to write back at the end.
    restore: Vec<StartParameter>,
    moved: bool,
    battery: bool,
}

/// The engine's practice: set-ups in flight and running attempts'
/// tracks, by attempt id.
#[derive(Debug, Default)]
pub(crate) struct Practice {
    setups: BTreeMap<String, Setup>,
    tracks: BTreeMap<String, Vec<Sample>>,
    /// When an operator last uploaded a mission, by vehicle id.
    uploads: BTreeMap<String, u64>,
}

/// A track keeps a point at most this often…
const SAMPLE_EVERY_US: u64 = 1_000_000;
/// …and at most this many, an hour of them.
const MAX_SAMPLES: usize = 3_600;

fn not_found(what: String) -> ApiError {
    ApiError::new(ErrorKind::NotFound, what)
}

/// Keeps an ended attempt as its operator's best at the exercise, if it is.
pub(crate) fn keep_best(state: &mut State, attempt: &Attempt) {
    if attempt.state == AttemptState::GivenUp {
        return;
    }
    let Some(exercise) = exercise(&attempt.exercise_id) else {
        return;
    };
    let key = format!(
        "{}/{}",
        attempt.operator_id.as_deref().unwrap_or_default(),
        attempt.exercise_id
    );
    let best = Best {
        passed: attempt.state == AttemptState::Passed,
        line: attempt.line.clone().unwrap_or_default(),
        measure: attempt.measure,
    };
    let higher = exercise
        .measure
        .as_ref()
        .is_some_and(uxduck_core::domain::practice::Measure::higher_is_better);
    let better = state
        .practice_best
        .get(&key)
        .is_none_or(|was| was.beaten_by(&best, higher));
    if better {
        state.practice_best.insert(key, best);
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let needed = match &request {
        ClientRequest::PracticeList(_) | ClientRequest::PracticeStatus(_) => Capability::Read,
        ClientRequest::PracticeStart(_) => Capability::Config,
        _ => Capability::Control,
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator may practise",
        ));
    }
    match request {
        ClientRequest::PracticeList(params) => {
            let operator = engine.actor(connection).operator_id;
            let exercises = exercises()
                .iter()
                .map(|exercise| {
                    let unavailable = match &params.vehicle_id {
                        Some(id) => engine.practice_unavailable(id, exercise),
                        None => Some("Choose a simulated vehicle to practise with.".into()),
                    };
                    listed(engine, exercise, operator.as_deref(), unavailable)
                })
                .collect();
            to_value(&PracticeListResult { exercises })
        }
        ClientRequest::PracticeStart(params) => {
            let attempt =
                engine.practice_start(connection, &params.exercise_id, &params.vehicle_id)?;
            to_value(&PracticeAttempt::from(&attempt))
        }
        ClientRequest::PracticeAnswer(params) => {
            let (right, attempt) = engine.practice_answer(
                connection,
                &params.vehicle_id,
                &params.question,
                &params.answer,
            )?;
            to_value(&PracticeAnswerResult {
                right,
                attempt: PracticeAttempt::from(&attempt),
            })
        }
        ClientRequest::PracticeStop(params) => {
            let attempt = engine.running_attempt(&params.vehicle_id).ok_or_else(|| {
                not_found(format!("no practice running on {}", params.vehicle_id))
            })?;
            let ended = engine.end_attempt(
                &attempt,
                AttemptState::GivenUp,
                Some("Given up.".into()),
                None,
                Some(connection),
            );
            to_value(&PracticeAttempt::from(&ended))
        }
        ClientRequest::PracticeGuide(params) => {
            let attempt = engine.practice_guide(connection, &params.vehicle_id)?;
            to_value(&PracticeAttempt::from(&attempt))
        }
        ClientRequest::PracticeStatus(params) => to_value(&PracticeStatusResult {
            attempt: engine
                .state
                .practice
                .get(&params.vehicle_id)
                .map(PracticeAttempt::from),
        }),
        _ => Err(ApiError::new(ErrorKind::Internal, "not a practice method")),
    }
}

fn listed(
    engine: &Engine,
    exercise: &Exercise,
    operator: Option<&str>,
    unavailable: Option<String>,
) -> PracticeExercise {
    let best = engine
        .state
        .practice_best
        .get(&format!("{}/{}", operator.unwrap_or_default(), exercise.id))
        .map(|b| PracticeBest {
            passed: b.passed,
            line: b.line.clone(),
            measure: b.measure,
            unit: exercise.measure.as_ref().map(|m| m.unit().to_owned()),
        });
    PracticeExercise {
        id: exercise.id.clone(),
        title: exercise.title.clone(),
        audience: exercise.audience.into(),
        vehicles: exercise.vehicles.iter().map(|&v| v.into()).collect(),
        guide: exercise.guide,
        goal: exercise.goal.clone(),
        steps: exercise
            .steps
            .iter()
            .map(|s| PracticeStep {
                text: s.text.clone(),
                hint: s.hint.clone(),
                hint_after_s: s.hint_after_s,
            })
            .collect(),
        questions: exercise
            .questions
            .iter()
            .map(|q| PracticeQuestion {
                id: q.id.clone(),
                text: q.text.clone(),
                unit: q.unit.clone(),
            })
            .collect(),
        unavailable,
        best,
    }
}

/// A simulated vehicle in a sentence, as the app says it (ADR-0011 M7).
const fn words(vehicle: SimVehicle) -> &'static str {
    match vehicle {
        SimVehicle::Multicopter => "multicopter",
        SimVehicle::Plane => "plane",
        SimVehicle::Vtol => "VTOL plane",
        SimVehicle::Rover => "rover",
        SimVehicle::Boat => "boat",
        SimVehicle::Submarine => "submarine",
        SimVehicle::Airship => "blimp",
        SimVehicle::Jet => "jet",
    }
}

/// "This exercise is for a simulated multicopter or VTOL plane."
fn only_for(vehicles: &[SimVehicle]) -> String {
    let names: Vec<&str> = vehicles.iter().map(|&v| words(v)).collect();
    let listed = match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => "another vehicle".to_owned(),
    };
    format!("This exercise is for a simulated {listed}.")
}

impl Engine {
    /// What kind of simulated vehicle UXDuck started this is (X1), or why
    /// it cannot practise.
    fn simulated_kind(&self, vehicle_id: &str) -> Result<SimVehicle, String> {
        let Some(vehicle) = self.state.vehicles.get(vehicle_id) else {
            return Err("Choose a simulated vehicle to practise with.".into());
        };
        match self
            .state
            .links
            .get(&vehicle.link_id)
            .map(|link| &link.kind)
        {
            Some(LinkKind::Simulator {
                vehicle,
                autopilot: SimAutopilot::Ardupilot,
                ..
            }) => Ok(*vehicle),
            _ => Err("Practice needs a simulated vehicle started in UXDuck.".into()),
        }
    }

    /// Why a vehicle cannot run an exercise (X1), or none if it can.
    pub(crate) fn practice_unavailable(
        &self,
        vehicle_id: &str,
        exercise: &Exercise,
    ) -> Option<String> {
        let kind = match self.simulated_kind(vehicle_id) {
            Ok(kind) => kind,
            Err(why) => return Some(why),
        };
        if !exercise.vehicles.contains(&kind) {
            return Some(only_for(&exercise.vehicles));
        }
        if exercise.needs_controller && !self.controller_drives(vehicle_id) {
            return Some("Claim a controller to try this.".into());
        }
        None
    }

    /// A controller connected, claimed and driving the vehicle (ADR-0005).
    fn controller_drives(&self, vehicle_id: &str) -> bool {
        self.state
            .controllers
            .values()
            .any(|c| c.connected && c.claim.is_some() && c.target.as_deref() == Some(vehicle_id))
    }

    /// The vehicle's attempt, while starting or running.
    pub(crate) fn running_attempt(&self, vehicle_id: &str) -> Option<Attempt> {
        self.state
            .practice
            .get(vehicle_id)
            .filter(|a| !a.state.is_over())
            .cloned()
    }

    fn practice_start(
        &mut self,
        connection: ConnectionId,
        exercise_id: &str,
        vehicle_id: &str,
    ) -> Result<Attempt, ApiError> {
        let exercise =
            exercise(exercise_id).ok_or_else(|| not_found(format!("no exercise {exercise_id}")))?;
        if let Some(why) = self.practice_unavailable(vehicle_id, exercise) {
            return Err(ApiError::new(ErrorKind::InvalidValue, why));
        }
        match self.running_attempt(vehicle_id) {
            // One attempt per vehicle: another exercise ends its guide.
            Some(guide) if guide.guide => {
                self.end_attempt(
                    &guide,
                    AttemptState::GivenUp,
                    Some("Given up: an exercise started.".into()),
                    None,
                    Some(connection),
                );
            }
            Some(_) => {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "This vehicle is already practising. Give that up first.",
                ));
            }
            None => {}
        }
        if exercise.guide {
            return Ok(self.start_guide(connection, exercise, vehicle_id));
        }
        let link_id = self.state.vehicles[vehicle_id].link_id.clone();
        let mut link = self.state.links[&link_id].clone();
        let LinkKind::Simulator {
            latitude_deg,
            longitude_deg,
            in_air_m,
            snapshot_id,
            ..
        } = &mut link.kind
        else {
            return Err(ApiError::new(ErrorKind::Internal, "not a simulator"));
        };
        let home = (*latitude_deg, *longitude_deg);
        // Started again in the air at the exercise's height, or on the
        // ground, from nothing saved.
        *in_air_m = (!exercise.start.on_ground).then(|| {
            exercise
                .start
                .height_m
                .clamp(crate::sim::MIN_IN_AIR_M, crate::sim::MAX_IN_AIR_M)
        });
        *snapshot_id = None;
        link.version += 1;
        self.commit(connection, Change::LinkUpserted { link });
        crate::sim::restart(self, connection, &link_id)?;
        let now = self.clock.now_us();
        let attempt = Attempt {
            id: format!("pa{}", &self.random.token()[..16]),
            exercise_id: exercise.id.clone(),
            exercise_version: exercise.version,
            guide: false,
            vehicle_id: vehicle_id.to_owned(),
            link_id,
            operator_id: self.actor(connection).operator_id,
            state: AttemptState::Starting,
            step: 0,
            line: None,
            measure: None,
            home_latitude_deg: home.0,
            home_longitude_deg: home.1,
            features: judge::place_features(exercise, home),
            answered: std::collections::BTreeSet::new(),
            wrong: BTreeMap::new(),
            set_up: false,
            restore: Vec::new(),
            started_at_us: now,
            step_started_at_us: now,
            ended_at_us: None,
            version: 1,
        };
        self.practice
            .setups
            .insert(attempt.id.clone(), Setup::default());
        self.commit(
            connection,
            Change::PracticeAttemptUpserted {
                attempt: attempt.clone(),
            },
        );
        self.track_practice_started(&attempt.exercise_id);
        Ok(attempt)
    }

    /// `practice/guide` (S117): the vehicle's running guide, or the guide
    /// for its kind started on it as it is.
    fn practice_guide(
        &mut self,
        connection: ConnectionId,
        vehicle_id: &str,
    ) -> Result<Attempt, ApiError> {
        match self.running_attempt(vehicle_id) {
            Some(guide) if guide.guide => return Ok(guide),
            Some(_) => {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "This vehicle is practising. Give that up first.",
                ));
            }
            None => {}
        }
        let kind = self
            .simulated_kind(vehicle_id)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        let guide = exercises()
            .iter()
            .find(|e| e.guide && e.vehicles.contains(&kind))
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("There is no guide for a simulated {} yet.", words(kind)),
                )
            })?;
        if let Some(why) = self.practice_unavailable(vehicle_id, guide) {
            return Err(ApiError::new(ErrorKind::InvalidValue, why));
        }
        Ok(self.start_guide(connection, guide, vehicle_id))
    }

    /// A guide starts on the vehicle as it is: no restart, no set-up, and
    /// judged from now, so nothing sent before counts.
    fn start_guide(
        &mut self,
        connection: ConnectionId,
        guide: &Exercise,
        vehicle_id: &str,
    ) -> Attempt {
        let link_id = self.state.vehicles[vehicle_id].link_id.clone();
        // Home is where the simulation started, as for an exercise.
        let home = match self.state.links.get(&link_id).map(|l| &l.kind) {
            Some(LinkKind::Simulator {
                latitude_deg,
                longitude_deg,
                ..
            }) => (*latitude_deg, *longitude_deg),
            _ => (0.0, 0.0),
        };
        let now = self.clock.now_us();
        let attempt = Attempt {
            id: format!("pa{}", &self.random.token()[..16]),
            exercise_id: guide.id.clone(),
            exercise_version: guide.version,
            guide: true,
            vehicle_id: vehicle_id.to_owned(),
            link_id,
            operator_id: self.actor(connection).operator_id,
            state: AttemptState::Running,
            step: 0,
            line: None,
            measure: None,
            home_latitude_deg: home.0,
            home_longitude_deg: home.1,
            features: judge::place_features(guide, home),
            answered: std::collections::BTreeSet::new(),
            wrong: BTreeMap::new(),
            set_up: false,
            restore: Vec::new(),
            started_at_us: now,
            step_started_at_us: now,
            ended_at_us: None,
            version: 1,
        };
        self.commit(
            connection,
            Change::PracticeAttemptUpserted {
                attempt: attempt.clone(),
            },
        );
        self.track_practice_started(&attempt.exercise_id);
        // The vehicle may already be doing what the first step asks.
        self.review_practice(vehicle_id);
        self.state
            .practice
            .get(vehicle_id)
            .cloned()
            .unwrap_or(attempt)
    }

    fn practice_answer(
        &mut self,
        connection: ConnectionId,
        vehicle_id: &str,
        question: &str,
        answer: &str,
    ) -> Result<(bool, Attempt), ApiError> {
        let mut attempt = self
            .running_attempt(vehicle_id)
            .filter(|a| a.state == AttemptState::Running)
            .ok_or_else(|| not_found(format!("no practice running on {vehicle_id}")))?;
        let exercise = exercise(&attempt.exercise_id)
            .ok_or_else(|| not_found(format!("no exercise {}", attempt.exercise_id)))?;
        let asked = exercise
            .questions
            .iter()
            .find(|q| q.id == question)
            .ok_or_else(|| not_found(format!("no question {question}")))?;
        let right = judge::right(asked, answer, &self.practice_view(&attempt));
        if right {
            attempt.answered.insert(question.to_owned());
        } else {
            *attempt.wrong.entry(question.to_owned()).or_default() += 1;
        }
        attempt.version += 1;
        self.commit(
            connection,
            Change::PracticeAttemptUpserted {
                attempt: attempt.clone(),
            },
        );
        self.review_practice(vehicle_id);
        let now = self
            .state
            .practice
            .get(vehicle_id)
            .cloned()
            .unwrap_or(attempt);
        Ok((right, now))
    }

    /// Ends an attempt: journaled with its result, its set-up forgotten.
    fn end_attempt(
        &mut self,
        attempt: &Attempt,
        state: AttemptState,
        line: Option<String>,
        measure: Option<f64>,
        by: Option<ConnectionId>,
    ) -> Attempt {
        let mut ended = attempt.clone();
        ended.state = state;
        ended.line = line;
        ended.measure = measure;
        ended.ended_at_us = Some(self.clock.now_us());
        ended.version += 1;
        // Ended while starting, its set-up holds what to put back.
        let setup = self.practice.setups.remove(&ended.id);
        let restore = std::mem::take(&mut ended.restore);
        let restore = if restore.is_empty() {
            setup.map(|s| s.restore).unwrap_or_default()
        } else {
            restore
        };
        self.restore_practice_parameters(&ended, &restore);
        self.practice.tracks.remove(&ended.id);
        let change = Change::PracticeAttemptUpserted {
            attempt: ended.clone(),
        };
        match by {
            Some(connection) => {
                self.commit(connection, change);
            }
            None => {
                self.commit_as(practice_start(), change);
            }
        }
        self.track_practice_ended(&ended);
        ended
    }

    /// "Start again" or "Stop simulation" on the vehicle's link gives up its
    /// attempt (ADR-0024 "Judging").
    pub(crate) fn give_up_practice_on(&mut self, link_id: &str, connection: ConnectionId) {
        let running: Vec<Attempt> = self
            .state
            .practice
            .values()
            .filter(|a| a.link_id == link_id && !a.state.is_over())
            .cloned()
            .collect();
        for attempt in running {
            self.end_attempt(
                &attempt,
                AttemptState::GivenUp,
                Some("Given up: the simulation was started again or stopped.".into()),
                None,
                Some(connection),
            );
        }
    }

    /// What the judge reads of a vehicle now (X5).
    #[allow(clippy::too_many_lines, reason = "one field of the view after another")]
    pub(crate) fn practice_view(&self, attempt: &Attempt) -> View {
        let id = &attempt.vehicle_id;
        let vehicle = self.state.vehicles.get(id);
        let telemetry = self.telemetry(id);
        let item = |key: &str| telemetry.and_then(|t| t.items.iter().find(|i| i.key == key));
        let position = telemetry
            .and_then(|t| t.positions.iter().find(|p| p.source == "fused"))
            .map(|p| (p.latitude_deg, p.longitude_deg));
        let altitude = |frame: AltitudeFrame| {
            telemetry
                .and_then(|t| t.positions.iter().find(|p| p.source == "fused"))
                .and_then(|p| p.altitudes.iter().find(|a| a.frame == frame))
                .map(|a| a.meters)
        };
        let height_m = altitude(AltitudeFrame::AboveHome);
        // The operator's upload since the attempt began (S132): the
        // vehicle's mission as last read, with its fence and rally points.
        let uploaded = self
            .practice
            .uploads
            .get(id)
            .filter(|&&at| at >= attempt.started_at_us)
            .and_then(|_| self.state.missions.get(id));
        let on_ground = item("flight.state")
            .and_then(|i| i.text.as_deref())
            .map(|text| text == "On the ground");
        let commands = self
            .state
            .receipts
            .values()
            .filter(|r| {
                r.vehicle_id == *id
                    && r.created_at_us >= attempt.started_at_us
                    && !matches!(r.status, ReceiptStatus::Rejected | ReceiptStatus::NotSent)
                    && self.operator_client(&r.client_id)
            })
            .map(|r| (r.created_at_us, r.command.clone()))
            .collect();
        // Set by a person in UXDuck, not by the attempt's own set-up, which
        // writes as UXDuck.
        let parameters_set = self
            .state
            .history
            .values()
            .filter(|e| {
                e.vehicle_id == *id && e.deleted.is_none() && e.author.client_name != "UXDuck"
            })
            .filter_map(|e| match &e.kind {
                EntryKind::Event {
                    event:
                        HistoryEvent::ParameterChanged {
                            name,
                            to,
                            source: ParamSource::Person,
                            span_us,
                            ..
                        },
                } => Some((e.happened_at_us + span_us.unwrap_or(0), name.clone(), *to)),
                _ => None,
            })
            .collect();
        View {
            now_us: self.clock.now_us(),
            position,
            height_m,
            mode: vehicle.and_then(|v| v.mode.clone()),
            armed: vehicle.is_some_and(|v| v.armed),
            on_ground,
            battery_pct: item("battery.remaining_pct").and_then(|i| i.value),
            commands,
            parameters_set,
            controller: self.controller_drives(id),
            mission_commands: uploaded.map(|m| m.items.iter().skip(1).map(|i| i.command).collect()),
            mission_frames: uploaded.map(|m| {
                m.items
                    .iter()
                    .skip(1)
                    .filter(|i| WAYPOINTS.contains(&i.command))
                    .map(|i| i.frame)
                    .collect()
            }),
            fence_uploaded: uploaded.is_some_and(|m| !m.fence.is_empty()),
            rally: uploaded
                .map(|m| {
                    m.rally
                        .iter()
                        .map(|r| (r.latitude_deg, r.longitude_deg))
                        .collect()
                })
                .unwrap_or_default(),
            fence_breached: item("fence.breached")
                .and_then(|i| i.value)
                .map(|v| v > 0.5),
            clearance_m: altitude(AltitudeFrame::AboveGround),
            mission: uploaded.map(|m| {
                // Its waypoints: item 0 is home on ArduPilot, and a
                // take-off or a landing is not a place on the survey.
                m.items
                    .iter()
                    .skip(1)
                    .filter(|i| WAYPOINTS.contains(&i.command))
                    .map(|i| (i.latitude_deg, i.longitude_deg))
                    .collect()
            }),
            track: self
                .practice
                .tracks
                .get(&attempt.id)
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// An operator's client: not the agent, a plugin or a set-up. Only
    /// clients that are not operators are remembered, so an unknown client
    /// is an operator's.
    fn operator_client(&self, client_id: &str) -> bool {
        if matches!(client_id, ACTOR | crate::launch::ACTOR | "daemon" | "agent") {
            return false;
        }
        self.state
            .clients
            .get(client_id)
            .is_none_or(|c| c.kind.is_operator())
    }

    /// A mission upload succeeded: an operator's counts for "Plan the survey"
    /// (`operator_client` leaves out a simulated vehicle's example, P12).
    pub(crate) fn practice_mission_uploaded(&mut self, vehicle_id: &str, client_id: &str) {
        if self.operator_client(client_id) {
            self.practice
                .uploads
                .insert(vehicle_id.to_owned(), self.clock.now_us());
        }
    }

    /// Something changed on a vehicle: its attempt set up, judged or lost.
    pub(crate) fn review_practice(&mut self, vehicle_id: &str) {
        let Some(attempt) = self.running_attempt(vehicle_id) else {
            return;
        };
        let Some(exercise) = exercise(&attempt.exercise_id) else {
            return;
        };
        let offline = self
            .state
            .vehicles
            .get(vehicle_id)
            .is_none_or(|v| v.presence == Presence::Offline);
        // A guide never fails: it waits for the vehicle to come back.
        if offline && attempt.guide {
            return;
        }
        if offline && attempt.state == AttemptState::Running {
            self.end_attempt(
                &attempt,
                AttemptState::Failed,
                Some("Failed. The vehicle was lost.".into()),
                None,
                None,
            );
            return;
        }
        if attempt.state == AttemptState::Starting {
            self.set_up_attempt(&attempt, exercise);
            return;
        }
        let mut view = self.practice_view(&attempt);
        self.keep_sample(&attempt.id, &mut view);
        match judge::judge(exercise, &attempt, &view) {
            Verdict::Keep => {}
            Verdict::Step(step) => {
                let mut next = attempt;
                next.step = step;
                next.step_started_at_us = view.now_us;
                next.version += 1;
                self.commit_as(
                    practice_start(),
                    Change::PracticeAttemptUpserted { attempt: next },
                );
            }
            Verdict::Passed { line, measure } => {
                let mut done = attempt;
                done.step = u32::try_from(exercise.steps.len()).unwrap_or(u32::MAX);
                self.end_attempt(&done, AttemptState::Passed, Some(line), measure, None);
            }
            Verdict::Failed { line, measure } => {
                self.end_attempt(&attempt, AttemptState::Failed, Some(line), measure, None);
            }
        }
    }

    /// Adds where the vehicle is now to its attempt's track, at most once a
    /// second and an hour's worth, and ends the view's track with it.
    fn keep_sample(&mut self, attempt_id: &str, view: &mut View) {
        let Some(position) = view.position else {
            return;
        };
        let sample = Sample {
            at_us: view.now_us,
            position,
            height_m: view.height_m,
            mode: view.mode.clone(),
            clearance_m: view.clearance_m,
        };
        let track = self
            .practice
            .tracks
            .entry(attempt_id.to_owned())
            .or_default();
        let due = track
            .last()
            .is_none_or(|last| view.now_us.saturating_sub(last.at_us) >= SAMPLE_EVERY_US);
        if due {
            if track.len() >= MAX_SAMPLES {
                track.remove(0);
            }
            track.push(sample.clone());
        }
        view.track.clone_from(track);
        if !due {
            view.track.push(sample);
        }
    }

    /// The set-up (X4): once the take-off is done, moved to the start and
    /// its battery set, as `practice-start`; then it is judged from now.
    fn set_up_attempt(&mut self, attempt: &Attempt, exercise: &Exercise) {
        if self.is_launching(&attempt.link_id) {
            return;
        }
        let Some(vehicle) = self.state.vehicles.get(&attempt.vehicle_id).cloned() else {
            return;
        };
        if vehicle.presence != Presence::Online || (!vehicle.armed && !exercise.start.on_ground) {
            return;
        }
        let view = self.practice_view(attempt);
        let home = (attempt.home_latitude_deg, attempt.home_longitude_deg);
        self.set_start_parameters(attempt, exercise, &vehicle.id);
        if let Some(offset) = exercise.start.offset {
            let target = judge::place(home, offset);
            let moved = self
                .practice
                .setups
                .get(&attempt.id)
                .is_some_and(|s| s.moved);
            if !moved {
                self.send_as_practice_start(
                    &vehicle.id,
                    Command::GoTo {
                        latitude_deg: target.0,
                        longitude_deg: target.1,
                        altitude_m: exercise.start.height_m,
                        altitude_frame: AltitudeFrame::AboveHome,
                        heading: None,
                    },
                );
                self.practice
                    .setups
                    .entry(attempt.id.clone())
                    .or_default()
                    .moved = true;
                return;
            }
            let near = match self.simulated_kind(&vehicle.id) {
                Ok(SimVehicle::Plane | SimVehicle::Vtol | SimVehicle::Jet) => AT_START_CIRCLING_M,
                _ => AT_START_M,
            };
            if view
                .position
                .is_none_or(|at| judge::distance_m(at, target) > near)
            {
                return;
            }
        }
        if let Some(pct) = exercise.start.battery_pct {
            let battery_set = self
                .practice
                .setups
                .get(&attempt.id)
                .is_some_and(|s| s.battery);
            if !battery_set {
                #[allow(clippy::cast_possible_truncation, reason = "a percentage")]
                self.send_as_practice_start(
                    &vehicle.id,
                    Command::Long {
                        command: BATTERY_RESET,
                        params: vec![1.0, pct as f32],
                    },
                );
                self.practice
                    .setups
                    .entry(attempt.id.clone())
                    .or_default()
                    .battery = true;
            }
        }
        let now = self.clock.now_us();
        let mut running = attempt.clone();
        running.state = AttemptState::Running;
        running.set_up = true;
        running.started_at_us = now;
        running.step_started_at_us = now;
        running.version += 1;
        if let Some(setup) = self.practice.setups.remove(&running.id) {
            running.restore = setup.restore;
        }
        self.commit_as(
            practice_start(),
            Change::PracticeAttemptUpserted { attempt: running },
        );
    }

    /// The exercise's parameters, written once as `practice-start`.
    fn set_start_parameters(&mut self, attempt: &Attempt, exercise: &Exercise, vehicle_id: &str) {
        if self
            .practice
            .setups
            .get(&attempt.id)
            .is_some_and(|s| s.parameters)
        {
            return;
        }
        let mut restore = Vec::new();
        for parameter in &exercise.start.parameters {
            let known = self
                .state
                .parameters
                .get(&format!("{vehicle_id}/{}", parameter.name));
            let kind = known.map_or(ParamKind::Real32, |p| p.kind);
            // Put back when the attempt ends, when its value is known and
            // the exercise changes it.
            if let Some(was) = known.map(|p| p.value)
                && (was - parameter.value).abs() > f64::EPSILON
            {
                restore.push(StartParameter {
                    name: parameter.name.clone(),
                    value: was,
                });
            }
            self.write_parameter_as(
                practice_start(),
                vehicle_id,
                &parameter.name,
                parameter.value,
                kind,
            );
        }
        let setup = self.practice.setups.entry(attempt.id.clone()).or_default();
        setup.parameters = true;
        setup.restore = restore;
    }

    /// Writes back the parameters an attempt's start changed, as
    /// `practice-start`, so the vehicle is as the exercise found it.
    fn restore_practice_parameters(&mut self, attempt: &Attempt, restore: &[StartParameter]) {
        for parameter in restore {
            let kind = self
                .state
                .parameters
                .get(&format!("{}/{}", attempt.vehicle_id, parameter.name))
                .map_or(ParamKind::Real32, |p| p.kind);
            self.write_parameter_as(
                practice_start(),
                &attempt.vehicle_id,
                &parameter.name,
                parameter.value,
                kind,
            );
        }
    }

    /// A command as `practice-start`: journaled with a receipt like any
    /// other, never counted as the operator's.
    fn send_as_practice_start(&mut self, vehicle_id: &str, command: Command) {
        let receipt = Receipt {
            id: format!("rc{}", &self.random.token()[..16]),
            vehicle_id: vehicle_id.to_owned(),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            sent_without_recent_heartbeat: false,
            forced: false,
            attempts: 0,
            client_id: ACTOR.into(),
            operator_id: None,
            remote: None,
            created_at_us: self.clock.now_us(),
            completed_at_us: None,
            version: 1,
        };
        let id = receipt.id.clone();
        self.commit_as(practice_start(), Change::ReceiptUpserted { receipt });
        self.links.command(CommandOrder {
            receipt_id: id,
            vehicle_id: vehicle_id.to_owned(),
            command,
            force: false,
        });
    }

    fn track_practice_started(&mut self, exercise_id: &str) {
        self.track(
            uxduck_contracts::dto::analytics::DaemonEvent::PracticeStarted {
                exercise: exercise_id.to_owned(),
            },
        );
    }

    fn track_practice_ended(&mut self, attempt: &Attempt) {
        if attempt.state == AttemptState::Passed {
            self.track_task(
                &attempt.vehicle_id,
                uxduck_contracts::dto::analytics::EngagementTask::PracticePassed,
                true,
            );
        }
        let result = match attempt.state {
            AttemptState::Passed => "passed",
            AttemptState::Failed => "failed",
            _ => "given-up",
        };
        self.track(
            uxduck_contracts::dto::analytics::DaemonEvent::PracticeEnded {
                exercise: attempt.exercise_id.clone(),
                result: result.into(),
            },
        );
    }
}

//! The Tuning tab's parts (S329, ADR-0054 T8): which of a vehicle's
//! parameters fill each one, read from the parameters it has. And its
//! response graph (S330, T7): every rate controller sample of the last
//! minute, each tuning change made meanwhile as a mark, and the vehicle
//! asked for its samples 25 times a second while the graph shows.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;
use uxduck_contracts::convert::tuning::parts;
use uxduck_contracts::dto::tuning::{
    TuningAutotune, TuningAxis, TuningAxisResponse, TuningAxisSummary, TuningLiveParams,
    TuningLiveResult, TuningMark, TuningParam, TuningPrepare, TuningPrepareParams, TuningProposal,
    TuningResponse, TuningResponseParams, TuningSample, TuningSource, TuningState, TuningStepState,
    TuningSummary, TuningUnsupported, TuningVerdict,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::Command;
use uxduck_core::domain::link::LinkPolicy;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::tuning::{self, Axis, Resolved};
use uxduck_core::domain::tuning_autotune;
use uxduck_core::domain::tuning_response::{self as response, Sample, Source, Verdict};
use uxduck_core::domain::tuning_steps::{self, Build};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_mavlink::param_log::Timeline;

use crate::Engine;
use crate::param_definitions::queries::{vehicle_values, which_vehicle};
use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;

/// The controllers the vehicle is tuned by, with its parameters in each
/// part, and its values by name. Empty when its parameters are not read.
pub(crate) fn resolved(
    engine: &Engine,
    vehicle: &Vehicle,
) -> Result<(Vec<Resolved>, BTreeMap<String, f64>), TuningUnsupported> {
    let controllers = tuning::controllers(&vehicle.autopilot, &vehicle.vehicle_type)
        .map_err(TuningUnsupported::from)?;
    let values = vehicle_values(engine, vehicle.clone());
    if values.is_empty() {
        return Err(TuningUnsupported::NotRead);
    }
    let resolved = controllers
        .into_iter()
        .filter_map(|c| tuning::resolve(c, |name| values.contains_key(name)))
        .collect();
    Ok((resolved, values))
}

/// The vehicle's tuning parameters and their values now: what a snapshot
/// keeps (T5). Empty when there are none.
pub(crate) fn tuning_values(engine: &Engine, vehicle: &Vehicle) -> BTreeMap<String, f64> {
    let Ok((resolved, values)) = resolved(engine, vehicle) else {
        return BTreeMap::new();
    };
    resolved
        .iter()
        .flat_map(Resolved::names)
        .filter_map(|name| values.get(name).map(|v| (name.to_owned(), *v)))
        .collect()
}

fn state(engine: &Engine, vehicle: &Vehicle) -> TuningState {
    match resolved(engine, vehicle) {
        Err(why) => TuningState {
            vehicle_id: vehicle.id.clone(),
            unsupported: Some(why),
            controllers: Vec::new(),
            autotune: None,
            flying: Vec::new(),
            marks: Vec::new(),
        },
        Ok((resolved, values)) => {
            let param = |name: &str| TuningParam {
                name: name.to_owned(),
                value: values.get(name).copied().unwrap_or_default(),
                default: engine
                    .state
                    .parameters
                    .get(&parameter_id(&vehicle.id, name))
                    .and_then(|p| p.default),
                units: engine.param_facts(&vehicle.id, name).0,
            };
            let default = |name: &str| {
                engine
                    .state
                    .parameters
                    .get(&parameter_id(&vehicle.id, name))
                    .and_then(|p| p.default)
            };
            let controllers = resolved
                .iter()
                .map(|r| {
                    let mut part = parts(r, param);
                    part.steps = tuning_steps::steps(r, |n| values.get(n).copied(), default)
                        .into_iter()
                        .map(|s| TuningStepState {
                            step: s.step.into(),
                            done: s.done,
                            changed_at_us: engine.param_changed_at(&vehicle.id, &s.names),
                            why: s.why,
                            names: s.names,
                        })
                        .collect();
                    let driver = tuning_autotune::driver(r.controller);
                    part.autotune = values.contains_key(driver.axes).then(|| {
                        TuningAutotune::of(
                            &driver,
                            tuning_autotune::entry(&driver, |n| values.get(n).copied()),
                        )
                    });
                    part
                })
                .collect();
            let response = engine.tuning_response(vehicle, Some(u64::MAX));
            TuningState {
                vehicle_id: vehicle.id.clone(),
                unsupported: None,
                controllers,
                autotune: engine.autotune_run(&vehicle.id),
                flying: response
                    .axes
                    .into_iter()
                    .map(|a| TuningAxisSummary {
                        axis: a.axis,
                        source: a.source,
                        summary: a.summary,
                    })
                    .collect(),
                marks: engine
                    .responses
                    .by_vehicle
                    .get(&vehicle.id)
                    .map(|r| r.marks.iter().cloned().collect())
                    .unwrap_or_default(),
            }
        }
    }
}

/// `tuning/state`.
pub(crate) fn handle(
    engine: &Engine,
    params: &uxduck_contracts::dto::tuning::TuningStateParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
    to_value(&state(engine, &vehicle))
}

/// Samples and marks are kept this long: the graph's minute.
const KEEP_US: u64 = 60_000_000;
/// A sample this much older than the newest kept means the clock went
/// back, as a replay moved back does: what was kept is not before it.
const BACK_US: u64 = 1_000_000;
/// `MAV_CMD_SET_MESSAGE_INTERVAL`.
const SET_MESSAGE_INTERVAL: u16 = 511;
/// 25 times a second while the graph shows (T7).
const LIVE_US: f32 = 40_000.0;
/// `PID_TUNING` and `ATTITUDE_TARGET` go back to their usual rate (0);
/// `ATTITUDE` to the one UXDuck asks for anyway.
const STREAMS: [(f32, f32); 3] = [(194.0, 0.0), (83.0, 0.0), (30.0, 100_000.0)];

#[derive(Debug, Default)]
pub(crate) struct Responses {
    by_vehicle: BTreeMap<String, Response>,
}

#[derive(Debug, Default)]
struct Response {
    /// Roll, pitch and yaw, oldest first.
    axes: [VecDeque<Sample>; 3],
    marks: VecDeque<TuningMark>,
    /// Its tuning values as last seen, to find changes by.
    values: BTreeMap<String, f64>,
    live: bool,
    /// Message intervals still to ask for: one at a time, since a vehicle
    /// takes one command of a kind at a time.
    intervals: VecDeque<(f32, f32)>,
    /// The interval order waiting for its answer.
    asking: Option<String>,
}

impl Response {
    fn newest_us(&self) -> Option<u64> {
        self.axes
            .iter()
            .filter_map(|a| a.back().map(|s| s.t_us))
            .max()
    }
}

const fn index(axis: Axis) -> usize {
    match axis {
        Axis::Roll => 0,
        Axis::Pitch => 1,
        Axis::Yaw => 2,
    }
}

const fn axis_dto(axis: Axis) -> TuningAxis {
    match axis {
        Axis::Roll => TuningAxis::Roll,
        Axis::Pitch => TuningAxis::Pitch,
        Axis::Yaw => TuningAxis::Yaw,
    }
}

const fn verdict_dto(verdict: Verdict) -> TuningVerdict {
    match verdict {
        Verdict::Gathering => TuningVerdict::Gathering,
        Verdict::Still => TuningVerdict::Still,
        Verdict::Following => TuningVerdict::Following,
        Verdict::Slow => TuningVerdict::Slow,
        Verdict::Overshooting => TuningVerdict::Overshooting,
        Verdict::Oscillating => TuningVerdict::Oscillating,
    }
}

fn sample_dto(sample: &Sample) -> TuningSample {
    TuningSample {
        t_us: sample.t_us,
        desired: sample.desired,
        achieved: sample.achieved,
        p: sample.terms.map(|t| t.p),
        i: sample.terms.map(|t| t.i),
        d: sample.terms.map(|t| t.d),
        ff: sample.terms.map(|t| t.ff),
    }
}

impl Engine {
    /// A vehicle's rate controller samples came (S330): kept for a
    /// minute, with a mark for each tuning value changed since the last.
    pub fn tuning_samples(&mut self, vehicle_id: &str, samples: Vec<Sample>) {
        let Some(first) = samples.iter().map(|s| s.t_us).min() else {
            return;
        };
        let values = self
            .state
            .vehicles
            .get(vehicle_id)
            .map(|v| tuning_values(self, v))
            .unwrap_or_default();
        let response = self
            .responses
            .by_vehicle
            .entry(vehicle_id.to_owned())
            .or_default();
        if response
            .newest_us()
            .is_some_and(|newest| first + BACK_US < newest)
        {
            *response = Response {
                live: response.live,
                intervals: std::mem::take(&mut response.intervals),
                asking: response.asking.take(),
                ..Response::default()
            };
        }
        let at = samples.iter().map(|s| s.t_us).max().unwrap_or(first);
        if !response.values.is_empty() {
            for (name, to) in &values {
                let from = response.values.get(name).copied();
                if let Some(from) = from.filter(|from| from.to_bits() != to.to_bits()) {
                    response.marks.push_back(TuningMark {
                        t_us: at,
                        name: name.clone(),
                        from,
                        to: *to,
                    });
                }
            }
        }
        if !values.is_empty() {
            response.values = values;
        }
        for sample in samples {
            response.axes[index(sample.axis)].push_back(sample);
        }
        let keep = at.saturating_sub(KEEP_US);
        for axis in &mut response.axes {
            while axis.front().is_some_and(|s| s.t_us < keep) {
                axis.pop_front();
            }
        }
        while response.marks.front().is_some_and(|m| m.t_us < keep) {
            response.marks.pop_front();
        }
    }

    /// A replay moved: what was kept of its vehicle is not of the new time.
    pub(crate) fn forget_tuning_samples(&mut self, vehicle_id: &str) {
        self.responses.by_vehicle.remove(vehicle_id);
    }

    /// A replay moved: the tuning changes its log shows while the samples
    /// kept were taken, marked as they were when played through.
    pub(crate) fn replay_tuning_marks(&mut self, vehicle_id: &str, timeline: &Timeline) {
        let Some(names) = self
            .state
            .vehicles
            .get(vehicle_id)
            .map(|v| tuning_values(self, v))
        else {
            return;
        };
        let Some(response) = self.responses.by_vehicle.get_mut(vehicle_id) else {
            return;
        };
        let first = response
            .axes
            .iter()
            .filter_map(|a| a.front().map(|s| s.t_us))
            .min();
        let (Some(first), Some(newest)) = (first, response.newest_us()) else {
            return;
        };
        response.marks = timeline
            .changes(first, newest)
            .into_iter()
            .filter(|c| names.contains_key(&c.name))
            .map(|c| TuningMark {
                t_us: c.at_us,
                name: c.name,
                from: f64::from(c.from),
                to: f64::from(c.to),
            })
            .collect();
    }

    fn tuning_response(&self, vehicle: &Vehicle, after_us: Option<u64>) -> TuningResponse {
        let response = self.responses.by_vehicle.get(&vehicle.id);
        let newest = response.and_then(Response::newest_us);
        let now_us = if vehicle.replay {
            newest.unwrap_or_default()
        } else {
            self.clock.now_us()
        };
        let after = |t: u64| after_us.is_none_or(|after| t > after);
        let axes = response
            .map(|r| {
                Axis::ALL
                    .into_iter()
                    .filter_map(|axis| {
                        let kept = &r.axes[index(axis)];
                        let last = kept.back()?;
                        let points: Vec<(u64, f64, f64)> = kept
                            .iter()
                            .map(|s| (s.t_us, s.desired, s.achieved))
                            .collect();
                        let summary = response::summarize(&points);
                        Some(TuningAxisResponse {
                            axis: axis_dto(axis),
                            source: match last.source {
                                Source::Pid => TuningSource::Pid,
                                Source::Attitude => TuningSource::Attitude,
                            },
                            samples: kept
                                .iter()
                                .filter(|s| after(s.t_us))
                                .map(sample_dto)
                                .collect(),
                            summary: TuningSummary {
                                verdict: verdict_dto(summary.verdict),
                                asked_degps: summary.asked_degps,
                                tracking_error_pct: summary.tracking_error_pct,
                                overshoot_pct: summary.overshoot_pct,
                                oscillation_hz: summary.oscillation_hz,
                                oscillation_degps: summary.oscillation_degps,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        TuningResponse {
            vehicle_id: vehicle.id.clone(),
            now_us,
            live: response.is_some_and(|r| r.live),
            axes,
            marks: response
                .map(|r| r.marks.iter().filter(|m| after(m.t_us)).cloned().collect())
                .unwrap_or_default(),
        }
    }

    /// Asks the vehicle for its samples 25 times a second, or at their
    /// usual rates again. Nothing is kept of the order: a rate is the
    /// vehicle's until it restarts, never a setting.
    fn tuning_live(&mut self, vehicle: &Vehicle, on: bool) -> bool {
        let writable = self
            .state
            .links
            .get(&vehicle.link_id)
            .is_some_and(|l| l.policy != LinkPolicy::Read);
        if vehicle.replay || vehicle.presence != Presence::Online || !writable {
            return false;
        }
        let response = self
            .responses
            .by_vehicle
            .entry(vehicle.id.clone())
            .or_default();
        response.live = on;
        response.intervals = STREAMS
            .iter()
            .map(|&(message, usual)| (message, if on { LIVE_US } else { usual }))
            .collect();
        if response.asking.is_none() {
            self.ask_interval(&vehicle.id);
        }
        true
    }

    /// Sends the next message interval still to ask for.
    fn ask_interval(&mut self, vehicle_id: &str) {
        let Some(response) = self.responses.by_vehicle.get_mut(vehicle_id) else {
            return;
        };
        let Some((message, interval)) = response.intervals.pop_front() else {
            return;
        };
        let receipt_id = format!("tl{}", &self.random.token()[..16]);
        response.asking = Some(receipt_id.clone());
        self.links.command(CommandOrder {
            receipt_id,
            vehicle_id: vehicle_id.to_owned(),
            command: Command::Long {
                command: SET_MESSAGE_INTERVAL,
                params: vec![message, interval],
            },
            force: false,
        });
    }

    /// An interval order was answered, however it went: the next one goes.
    pub(crate) fn tuning_order_done(&mut self, receipt_id: &str) -> bool {
        let Some(vehicle_id) = self
            .responses
            .by_vehicle
            .iter_mut()
            .find(|(_, r)| r.asking.as_deref() == Some(receipt_id))
            .map(|(id, r)| {
                r.asking = None;
                id.clone()
            })
        else {
            return false;
        };
        self.ask_interval(&vehicle_id);
        true
    }
}

/// `tuning/response`.
pub(crate) fn handle_response(
    engine: &Engine,
    params: &TuningResponseParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
    to_value(&engine.tuning_response(&vehicle, params.after_us))
}

/// `tuning/live`.
pub(crate) fn handle_live(
    engine: &mut Engine,
    params: &TuningLiveParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Control) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Control.denied_message(),
        ));
    }
    let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
    to_value(&TuningLiveResult {
        sent: engine.tuning_live(&vehicle, params.on),
    })
}

/// `tuning/prepare`: the starting values for the props and battery (T9),
/// for the parameters the vehicle has, with their values now.
pub(crate) fn handle_prepare(
    engine: &Engine,
    params: &TuningPrepareParams,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    if !(1.0..=40.0).contains(&params.prop_inches) || !(1..=24).contains(&params.cells) {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "Props are 1 to 40 inches and batteries 1 to 24 cells",
        ));
    }
    let vehicle = which_vehicle(engine, Some(&params.vehicle_id))?;
    let controller = params.controller.into();
    let values = match resolved(engine, &vehicle) {
        Ok((resolved, values)) if resolved.iter().any(|r| r.controller == controller) => values,
        _ => BTreeMap::new(),
    };
    let build = Build {
        prop_inches: params.prop_inches,
        cells: params.cells,
        chemistry: params.chemistry.into(),
    };
    let proposals = tuning_steps::prepare(controller, &build, |n| values.contains_key(n))
        .into_iter()
        .map(|p| TuningProposal {
            now: values.get(&p.name).copied().unwrap_or_default(),
            name: p.name,
            value: p.value,
            why: p.why,
        })
        .collect();
    to_value(&TuningPrepare {
        vehicle_id: vehicle.id,
        proposals,
    })
}

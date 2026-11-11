//! AutoTune, guided (S332, ADR-0054 T10): each vehicle's last AutoTune,
//! folded from its status texts, with its tuning values when it started.
//! When it ends kept, its parameters are read again, since ArduPilot saves
//! the new gains without saying so, and the history has the gains before
//! and after. A person flies AutoTune: an agent's command that would start
//! it is refused.

use std::collections::BTreeMap;

use uxduck_contracts::dto::command::Command;
use uxduck_contracts::dto::tuning::{TuningAutotuneRun, TuningValue};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::history::{HistoryEvent, TunedGain};
use uxduck_core::domain::tuning::{self, Controller, Resolved};
use uxduck_core::domain::tuning_autotune::{self as autotune, Family, Phase, Run, Said};
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;
use crate::tuning::resolved;

/// `MAV_CMD_DO_SET_MODE`, whose second parameter is the custom mode.
const DO_SET_MODE: u16 = 176;
/// `MAV_CMD_DO_AUX_FUNCTION`: an `RCx_OPTION` and a switch position.
const DO_AUX_FUNCTION: u16 = 218;

#[derive(Debug, Default)]
pub(crate) struct AutoTunes {
    by_vehicle: BTreeMap<String, Tune>,
}

#[derive(Debug)]
struct Tune {
    run: Run,
    started_us: u64,
    ended_us: Option<u64>,
    before: BTreeMap<String, f64>,
    /// Kept: its parameters are being read again for the history.
    reading: bool,
}

impl Engine {
    /// A status text: AutoTune's is folded into the vehicle's run.
    pub(crate) fn autotune_said(&mut self, vehicle: &Vehicle, text: &str, now: u64) {
        let Some((family, said)) = autotune::said(text) else {
            return;
        };
        let Ok(controllers) = tuning::controllers(&vehicle.autopilot, &vehicle.vehicle_type) else {
            return;
        };
        let controller = match family {
            Family::Fixed => Controller::Plane,
            Family::Multi if controllers.contains(&Controller::Copter) => Controller::Copter,
            Family::Multi => Controller::Hover,
        };
        if !controllers.contains(&controller) {
            return;
        }
        let going = self
            .autotunes
            .by_vehicle
            .get(&vehicle.id)
            .is_some_and(|t| !t.run.ended() && t.run.controller == controller);
        let ending = matches!(
            said,
            Said::Stopped | Said::Saved | Said::Restored | Said::Failed(_) | Said::Note(_)
        );
        if !going {
            if ending {
                return;
            }
            let before = autotune_values(self, vehicle, controller);
            self.autotunes.by_vehicle.insert(
                vehicle.id.clone(),
                Tune {
                    run: Run::new(controller),
                    started_us: now,
                    ended_us: None,
                    before,
                    reading: false,
                },
            );
        }
        let Some(tune) = self.autotunes.by_vehicle.get_mut(&vehicle.id) else {
            return;
        };
        tune.run.said(said);
        if !tune.run.ended() {
            return;
        }
        tune.ended_us = Some(now);
        let kept = tune.run.phase == Phase::Kept;
        // A replay's AutoTune is shown, and kept nowhere beyond its log.
        if vehicle.replay {
            return;
        }
        if kept && self.not_writable(&vehicle.link_id).is_none() {
            if let Some(tune) = self.autotunes.by_vehicle.get_mut(&vehicle.id) {
                tune.reading = true;
            }
            self.download_parameters(&vehicle.id);
        } else {
            self.autotune_record(vehicle);
        }
    }

    /// The parameters were read: a kept AutoTune's history entry, with the
    /// gains it changed.
    pub(crate) fn autotune_read(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        if self
            .autotunes
            .by_vehicle
            .get_mut(&vehicle.id)
            .is_some_and(|t| std::mem::take(&mut t.reading))
        {
            self.autotune_record(&vehicle);
        }
    }

    fn autotune_record(&mut self, vehicle: &Vehicle) {
        let Some(tune) = self.autotunes.by_vehicle.get(&vehicle.id) else {
            return;
        };
        let now = autotune_values(self, vehicle, tune.run.controller);
        let gains: Vec<TunedGain> = tune
            .before
            .iter()
            .filter_map(|(name, from)| {
                let to = *now.get(name)?;
                ((to - from).abs() > 1e-6 * from.abs().max(1e-3)).then(|| TunedGain {
                    name: name.clone(),
                    from: *from,
                    to,
                })
            })
            .collect();
        let kept = tune.run.phase == Phase::Kept;
        let shown: Vec<String> = gains
            .iter()
            .take(3)
            .map(|g| format!("{} {} → {}", g.name, short(g.from), short(g.to)))
            .collect();
        let more = gains.len().saturating_sub(shown.len());
        let axes = autotune::axes_words(&tune.run.done);
        let line = tune.run.line();
        let text = match (kept, more) {
            _ if !kept && line.starts_with("AutoTune") => line,
            (false, _) => format!("AutoTune: {line}"),
            _ if gains.is_empty() => format!("AutoTune kept the gains for {axes}: none changed"),
            (true, 0) => format!("AutoTune kept new gains for {axes}: {}", shown.join(", ")),
            (true, more) => format!(
                "AutoTune kept new gains for {axes}: {} and {more} more",
                shown.join(", ")
            ),
        };
        self.record(&vehicle.id, HistoryEvent::AutoTuned { text, kept, gains });
    }

    /// The vehicle's last AutoTune, for `tuning/state`.
    pub(crate) fn autotune_run(&self, vehicle_id: &str) -> Option<TuningAutotuneRun> {
        let tune = self.autotunes.by_vehicle.get(vehicle_id)?;
        Some(TuningAutotuneRun {
            controller: tune.run.controller.into(),
            phase: tune.run.phase.into(),
            line: tune.run.line(),
            axis: tune.run.axis.map(Into::into),
            done: tune.run.done.iter().map(|a| (*a).into()).collect(),
            started_at_us: tune.started_us,
            ended_at_us: tune.ended_us,
            before: tune
                .before
                .iter()
                .map(|(name, value)| TuningValue {
                    name: name.clone(),
                    value: *value,
                })
                .collect(),
        })
    }
}

/// The vehicle's tuning values and the others its AutoTune saves, such as
/// the acceleration limits: what its before and after compares.
fn autotune_values(
    engine: &Engine,
    vehicle: &Vehicle,
    controller: Controller,
) -> BTreeMap<String, f64> {
    let Ok((resolved, values)) = resolved(engine, vehicle) else {
        return BTreeMap::new();
    };
    let saved: Vec<&str> = autotune::driver(controller)
        .saved(|n| values.contains_key(n))
        .collect();
    resolved
        .iter()
        .flat_map(Resolved::names)
        .chain(saved)
        .filter_map(|name| values.get(name).map(|v| (name.to_owned(), *v)))
        .collect()
}

/// A gain as the history writes it: four significant figures at most.
fn short(value: f64) -> String {
    let rounded: f64 = format!("{value:.4e}").parse().unwrap_or(value);
    format!("{rounded}")
}

/// Why an agent may not send this request: it would start AutoTune, which
/// a person flies (T10, ADR-0037 V4). None when it may.
pub(crate) fn agent_refusal(engine: &Engine, request: &ClientRequest) -> Option<&'static str> {
    let ClientRequest::CommandSend(params) = request else {
        return None;
    };
    let vehicle = engine.vehicle(&params.vehicle_id)?;
    let drivers: Vec<autotune::Driver> =
        tuning::controllers(&vehicle.autopilot, &vehicle.vehicle_type)
            .ok()?
            .into_iter()
            .map(autotune::driver)
            .collect();
    let is = |value: Option<&f32>, want: u32| {
        value.is_some_and(|v| (f64::from(*v) - f64::from(want)).abs() < 0.5)
    };
    let starts = match &params.command {
        Command::SetMode { mode } => drivers.iter().any(|d| d.mode.eq_ignore_ascii_case(mode)),
        Command::Long {
            command: DO_SET_MODE,
            params,
        } => drivers.iter().any(|d| is(params.get(1), d.mode_number)),
        Command::Long {
            command: DO_AUX_FUNCTION,
            params,
        } => drivers.iter().any(|d| {
            d.switch_option
                .is_some_and(|o| is(params.first(), o) && is(params.get(1), 2))
        }),
        _ => false,
    };
    starts.then_some(
        "A person flies AutoTune: the agent can't start it. Open Tuning's AutoTune part to see how to enter it.",
    )
}

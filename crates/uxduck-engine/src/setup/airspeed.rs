//! The airspeed row (S190, ADR-0037 V15), for an `ArduPilot` plane or VTOL
//! (a quadplane heartbeats as a plane) whose airspeed sensor is enabled
//! (`ARSPD_TYPE` not 0, and `ARSPD_ENABLE` not 0 where it is listed). Three procedures, one at a time like any:
//!
//! - **Zero** (`airspeed-zero`): the person covers the pitot from the
//!   wind; UXDuck sends `MAV_CMD_PREFLIGHT_CALIBRATION` with its ground
//!   pressure parameter (p3 = 1). `ArduPlane` zeroes its airspeed sensors
//!   with it: "Airspeed 1 calibration started", "in progress" answers,
//!   then "Airspeed 1 calibrated" and "accepted" about 2.5 s later (SITL,
//!   `ArduPlane` 4.8.0). Its airspeed parameter (p6 = 2) is answered
//!   "unsupported" there, so it is not sent; p6 = 1 is compass/motor
//!   interference on `ArduPilot`, never sent. It also takes the ground
//!   pressure again, harmless on the ground. Disarmed only.
//! - **Check** (`airspeed-check`): the person blows gently into the pitot;
//!   UXDuck watches `speed.air_mps` (`VFR_HUD`) and passes once it has
//!   risen 3 m/s above where it started, or fails after 15 s saying what
//!   to look at. Nothing is sent. Disarmed only.
//! - **Ratio** (`airspeed-ratio`): in flight only. UXDuck turns on the
//!   vehicle's automatic calibration (`ARSPD_AUTOCAL` 1) through the
//!   parameter path, asks for circles, and follows its estimate
//!   (`AIRSPEED_AUTOCAL`, which `ArduPlane` sends whatever the streams, as
//!   `airspeed.ratio`). Once the estimate has moved less than 2% over a
//!   minute it has settled: autocal goes back to 0 and the ratio is
//!   written, so the vehicle keeps it, with a reason, in its history and
//!   Parameters. Cancelled, disarmed (landed), the link lost, the vehicle
//!   restarted or UXDuck restarted before then, it stops and autocal is
//!   turned off; the ratio is left as the vehicle has it (it may have
//!   saved some of its estimates), not set back, so nothing jumps in
//!   flight. `autocal_on` is journaled until the vehicle took the 0, and
//!   the 0 is written again every 10 s whenever the vehicle can be told,
//!   so autocal is never left on silently.
//!
//! Parameter names come from the vehicle's parameter definitions
//! (ADR-0021), else the vehicle's own list; one neither has is said to be
//! missing rather than guessed. History: the zero and the ratio, well or
//! not (V13); the check is not a calibration.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::setup::{RowState, Setup as SetupDto, SetupRowState};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::{ParamKind, TransferStatus, parameter_id, same_value};
use uxduck_core::domain::setup::{
    AirspeedRun, Instruction, RadioWrite, Setup, SetupKind, SetupRow, SetupStep, WriteState,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};

use super::{Order, actor, round1};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// `MAV_CMD_PREFLIGHT_CALIBRATION`.
const MAV_CMD_PREFLIGHT_CALIBRATION: u16 = 241;
/// The reading, from `VFR_HUD`.
const READING: &str = "speed.air_mps";
/// The automatic calibration's estimate, from `AIRSPEED_AUTOCAL`.
const ESTIMATE: &str = "airspeed.ratio";
/// The check passes once the reading rose this far…
const RISE_MPS: f64 = 3.0;
/// …within this long.
const CHECK_US: u64 = 15_000_000;
/// A reading that moved less than this never moved: a flat sensor.
const FLAT_MPS: f64 = 0.2;
/// The ratio is judged over this long…
const SETTLE_US: u64 = 60_000_000;
/// …and settled when it moved less than this percentage of itself.
const SETTLED_PCT: f64 = 2.0;
/// An estimate older than this is not the vehicle's latest.
const FRESH_US: u64 = 5_000_000;
/// Autocal's 0 is written again this often until the vehicle takes it.
const RETRY_US: u64 = 10_000_000;
/// The zero's reading is followed this long after it ends.
const AFTER_ZERO_US: u64 = 10_000_000;

/// Each parameter by the names firmware has used, newest first.
const TYPE_NAMES: [&str; 1] = ["ARSPD_TYPE"];
/// The airspeed library's own switch: at 0 its other parameters are
/// hidden (`ArduPlane` 4.x lists it beside `ARSPD_TYPE`).
const ENABLE_NAMES: [&str; 1] = ["ARSPD_ENABLE"];
const AUTOCAL_NAMES: [&str; 1] = ["ARSPD_AUTOCAL"];
const RATIO_NAMES: [&str; 1] = ["ARSPD_RATIO"];
const OFFSET_NAMES: [&str; 1] = ["ARSPD_OFFSET"];

/// The reasons kept with each value written.
const REASON_ON: &str = "Airspeed ratio: automatic calibration on";
const REASON_OFF: &str = "Airspeed ratio: automatic calibration off";
const REASON_KEPT: &str = "Airspeed ratio";

/// A vehicle's airspeed sensor, for the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sensor {
    /// Its parameters are not read yet.
    Unread,
    /// None enabled, or its firmware has none: no row.
    Absent,
    Present,
}

/// What a value written was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    On,
    Off,
    Kept,
}

/// What the engine keeps of the airspeed beyond the journal.
#[derive(Debug, Default)]
pub(crate) struct Airspeeds {
    /// The estimates of each ratio running, by vehicle: when, and the
    /// ratio. One per run started since UXDuck started.
    runs: BTreeMap<String, Vec<(u64, f64)>>,
    /// Values written, by order id: the vehicle, the parameter, what for.
    writes: BTreeMap<String, (String, String, Purpose)>,
    /// When autocal's 0 was last written, by vehicle.
    off_sent: BTreeMap<String, u64>,
}

/// The percentage the estimates moved over, of the latest.
fn spread(samples: &[(u64, f64)]) -> Option<f64> {
    let latest = samples.last()?.1;
    let (lo, hi) = samples
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, r)| {
            (lo.min(*r), hi.max(*r))
        });
    (latest > 0.0).then(|| (hi - lo) / latest * 100.0)
}

/// "2.04", as a ratio is written and said.
fn ratio_text(ratio: f64) -> String {
    format!("{ratio:.3}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// A ratio as written: three decimals.
fn rounded(ratio: f64) -> f64 {
    (ratio * 1000.0).round() / 1000.0
}

/// `setup/start` for one of the airspeed's procedures.
#[allow(clippy::too_many_lines, reason = "one arm per procedure")]
pub(super) fn start(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle: &Vehicle,
    kind: SetupKind,
) -> Result<Value, ApiError> {
    let invalid = |why: String| ApiError::new(ErrorKind::InvalidValue, why);
    if kind == SetupKind::AirspeedRatio {
        if let Some(why) = engine.cannot_fly_ratio(vehicle) {
            return Err(invalid(why));
        }
    } else if let Some(why) = engine.cannot_set_up(vehicle) {
        return Err(invalid(why));
    }
    match engine.airspeed_sensor(vehicle) {
        None => {
            return Err(invalid(format!(
                "{} is not a plane: it has no airspeed sensor UXDuck sets up.",
                vehicle.name
            )));
        }
        Some(Sensor::Unread) => {
            return Err(invalid(format!(
                "UXDuck has not read {}'s parameters yet. Refresh them in Parameters, then try \
                 again.",
                vehicle.name
            )));
        }
        Some(Sensor::Absent) => {
            return Err(invalid(format!(
                "{} has no airspeed sensor enabled ({} is 0).",
                vehicle.name,
                if super::px4::px4(vehicle) {
                    "SYS_HAS_NUM_ASPD"
                } else {
                    engine
                        .airspeed_name(vehicle, &TYPE_NAMES)
                        .unwrap_or(TYPE_NAMES[0])
                }
            )));
        }
        Some(Sensor::Present) => {}
    }
    if let Some(running) = engine.running_setup(&vehicle.id) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!(
                "{} is already running on {}. Cancel it first.",
                running.kind.words(),
                vehicle.name
            ),
        ));
    }
    let reading = engine.telemetry_value(&vehicle.id, READING);
    let now = engine.clock.now_us();
    let mut run = AirspeedRun {
        start_mps: reading,
        reading_mps: reading,
        ..AirspeedRun::default()
    };
    let (step, instruction) = match kind {
        SetupKind::AirspeedZero => (SetupStep::Starting, Instruction::CoverPitot),
        SetupKind::AirspeedCheck => {
            if reading.is_none() {
                return Err(invalid(format!(
                    "UXDuck has no airspeed reading from {} yet.",
                    vehicle.name
                )));
            }
            run.peak_mps = reading;
            run.rise_mps = Some(RISE_MPS);
            run.until_us = Some(now + CHECK_US);
            (SetupStep::Running, Instruction::BlowIntoPitot)
        }
        _ => {
            let (autocal, ratio) = engine.ratio_names(vehicle).map_err(invalid)?;
            let before = engine.param_now(&vehicle.id, &ratio).ok_or_else(|| {
                invalid(format!(
                    "UXDuck has not read {}'s {ratio} yet. Refresh its parameters, then try again.",
                    vehicle.name
                ))
            })?;
            let autocal_now = engine.param_now(&vehicle.id, &autocal).unwrap_or(0.0);
            run.ratio_param = Some(ratio);
            run.ratio_before = Some(before);
            run.writes.push(RadioWrite {
                name: autocal.clone(),
                from: autocal_now,
                to: 1.0,
                state: WriteState::Writing,
            });
            run.autocal_param = Some(autocal);
            // On until the vehicle is seen to take the 0.
            run.autocal_on = true;
            (SetupStep::Starting, Instruction::FlyCircles)
        }
    };
    let receipt_id =
        (kind == SetupKind::AirspeedZero).then(|| format!("rc{}", &engine.random.token()[..16]));
    let setup = Setup {
        vehicle_id: vehicle.id.clone(),
        kind,
        step,
        progress: 0,
        sensors: Vec::new(),
        instruction: Some(instruction),
        detail: None,
        restart_needed: false,
        heading: None,
        position: None,
        placed: false,
        sides: None,
        radio: None,
        motor: None,
        firmware: None,
        airspeed: Some(Box::new(run)),
        started_by: Some(engine.author(connection)),
        receipt_id: receipt_id.clone(),
        started_at_us: now,
        updated_at_us: now,
        finished_at_us: None,
        version: 0,
    };
    let actor = engine.actor(connection);
    let saved = engine.save_setup(setup, true, actor.clone());
    match (kind, receipt_id) {
        // The ground pressure, which zeroes the airspeed sensors too; PX4's
        // own airspeed calibration, which zeroes it and asks for a blow
        // (S191).
        (SetupKind::AirspeedZero, Some(receipt_id)) => {
            let command = super::px4::px4(vehicle)
                .then(|| super::px4::start_command(kind))
                .flatten()
                .unwrap_or_else(|| Command::Long {
                    command: MAV_CMD_PREFLIGHT_CALIBRATION,
                    params: vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
                });
            engine.send_setup_command(vehicle, actor, receipt_id, command, Order::Start);
        }
        (SetupKind::AirspeedRatio, _) => {
            engine
                .setup
                .airspeed
                .runs
                .insert(vehicle.id.clone(), Vec::new());
            let name = saved
                .airspeed
                .as_ref()
                .and_then(|r| r.autocal_param.clone())
                .unwrap_or_default();
            let author = engine.author(connection);
            engine.write_airspeed(vehicle, (actor, author), &name, 1.0, Purpose::On);
        }
        _ => {}
    }
    to_value(&SetupDto::from(&saved))
}

impl Engine {
    /// A parameter's value on the vehicle, as last read.
    fn param_now(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
    }

    /// The first of `names` the vehicle's firmware has: from its parameter
    /// definitions, else its own list.
    fn airspeed_name(&self, vehicle: &Vehicle, names: &[&'static str]) -> Option<&'static str> {
        let defined = self.vehicle_definitions(&vehicle.id);
        names.iter().copied().find(|name| {
            defined
                .as_ref()
                .is_some_and(|set| set.find(name, false).is_some())
                || self.param_now(&vehicle.id, name).is_some()
        })
    }

    /// Autocal's and the ratio's names on this vehicle, or why not.
    fn ratio_names(&self, vehicle: &Vehicle) -> Result<(String, String), String> {
        let missing = |what: &str, names: &[&str]| {
            format!(
                "{}'s firmware{} has no {what} parameter UXDuck knows ({}), so UXDuck can't find \
                 its ratio in flight. Set its ratio in Parameters.",
                vehicle.name,
                vehicle
                    .firmware
                    .as_deref()
                    .map_or_else(String::new, |v| format!(" ({v})")),
                names.join(", ")
            )
        };
        let autocal = self
            .airspeed_name(vehicle, &AUTOCAL_NAMES)
            .ok_or_else(|| missing("automatic airspeed calibration", &AUTOCAL_NAMES))?;
        let ratio = self
            .airspeed_name(vehicle, &RATIO_NAMES)
            .ok_or_else(|| missing("airspeed ratio", &RATIO_NAMES))?;
        Ok((autocal.to_owned(), ratio.to_owned()))
    }

    /// The vehicle's airspeed sensor: none for anything but an `ArduPilot`
    /// plane or VTOL.
    pub(super) fn airspeed_sensor(&self, vehicle: &Vehicle) -> Option<Sensor> {
        if Family::of(&vehicle.vehicle_type) != Some(Family::Plane) {
            return None;
        }
        // PX4's planes and VTOLs (S191).
        if super::px4::px4(vehicle) {
            return Some(match self.px4_airspeed_present(&vehicle.id) {
                Some(true) => Sensor::Present,
                Some(false) => Sensor::Absent,
                None if self.parameter_set(&vehicle.id).status == TransferStatus::Synced => {
                    Sensor::Absent
                }
                None => Sensor::Unread,
            });
        }
        if vehicle.autopilot != "Ardupilotmega" {
            return None;
        }
        let switched_off = self
            .airspeed_name(vehicle, &ENABLE_NAMES)
            .and_then(|name| self.param_now(&vehicle.id, name))
            .is_some_and(|on| on == 0.0);
        if switched_off {
            return Some(Sensor::Absent);
        }
        let kind = self
            .airspeed_name(vehicle, &TYPE_NAMES)
            .and_then(|name| self.param_now(&vehicle.id, name));
        Some(match kind {
            Some(kind) if kind != 0.0 => Sensor::Present,
            Some(_) => Sensor::Absent,
            None if self.parameter_set(&vehicle.id).status == TransferStatus::Synced => {
                Sensor::Absent
            }
            None => Sensor::Unread,
        })
    }

    /// Armed and off the ground.
    fn flying(&self, vehicle: &Vehicle) -> bool {
        vehicle.armed && self.off_the_ground(&vehicle.id, true)
    }

    /// Why the ratio cannot start now: it is found in flight.
    fn cannot_fly_ratio(&self, vehicle: &Vehicle) -> Option<String> {
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            return Some(format!("Can't set up {}: {why}.", vehicle.name));
        }
        if super::px4::px4(vehicle) {
            return Some(format!(
                "UXDuck finds the airspeed ratio on ArduPilot only: PX4 scales {}'s airspeed in                  flight itself (ASPD_SCALE_APPLY).",
                vehicle.name
            ));
        }
        if vehicle.autopilot != "Ardupilotmega" {
            return Some(format!(
                "UXDuck sets up ArduPilot and PX4 vehicles, and {} runs neither.",
                vehicle.name
            ));
        }
        if vehicle.presence != Presence::Online {
            return Some(format!("UXDuck can't hear {} right now.", vehicle.name));
        }
        if !self.flying(vehicle) {
            return Some(format!(
                "Take off first: {}'s ratio is found in flight, circling.",
                vehicle.name
            ));
        }
        None
    }

    /// Writes one of the ratio's values through the parameter path.
    fn write_airspeed(
        &mut self,
        vehicle: &Vehicle,
        (actor, author): (
            uxduck_core::events::Actor,
            uxduck_core::domain::history::Author,
        ),
        name: &str,
        value: f64,
        purpose: Purpose,
    ) {
        let reason = match purpose {
            Purpose::On => REASON_ON,
            Purpose::Off => REASON_OFF,
            Purpose::Kept => REASON_KEPT,
        };
        let kind = self
            .state
            .parameters
            .get(&parameter_id(&vehicle.id, name))
            .map_or(
                if purpose == Purpose::Kept {
                    ParamKind::Real32
                } else {
                    ParamKind::Int8
                },
                |p| p.kind,
            );
        let order_id =
            self.write_parameter_for(actor, author, reason, (&vehicle.id, name, value), kind);
        if purpose == Purpose::Off {
            self.setup
                .airspeed
                .off_sent
                .insert(vehicle.id.clone(), self.clock.now_us());
        }
        self.setup
            .airspeed
            .writes
            .insert(order_id, (vehicle.id.clone(), name.to_owned(), purpose));
    }

    /// Autocal's 0, written as UXDuck's own.
    fn turn_autocal_off(&mut self, vehicle: &Vehicle, name: &str) {
        let author = uxduck_core::domain::history::Author {
            client_name: "UXDuck".into(),
            ..Default::default()
        };
        self.write_airspeed(vehicle, (actor(), author), name, 0.0, Purpose::Off);
    }

    /// The vehicle answered the zero (S190): zeroed, or why not; in the
    /// history either way when the vehicle answered.
    pub(super) fn zero_answered(&mut self, mut setup: Setup, receipt: &Receipt) {
        let Some(vehicle) = self.vehicle(&setup.vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let accepted = receipt.status == ReceiptStatus::Accepted;
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        setup.progress = 100;
        let offset = self
            .airspeed_name(&vehicle, &OFFSET_NAMES)
            .and_then(|name| Some((name, self.param_now(&vehicle.id, name)?)));
        let text = if accepted {
            setup.step = SetupStep::Succeeded;
            setup.instruction = None;
            setup.detail = None;
            match offset {
                Some((name, value)) => {
                    format!("Airspeed sensor zeroed ({name} {})", round1(value))
                }
                None => "Airspeed sensor zeroed".to_owned(),
            }
        } else {
            setup.step = SetupStep::Failed;
            let name = &vehicle.name;
            // What the vehicle said just before, if it said why.
            let said = setup.detail.take();
            let refusal = match (receipt.status, receipt.detail.as_deref(), said) {
                (ReceiptStatus::Rejected, _, Some(said)) => {
                    format!("{name} could not zero its airspeed sensor: {said}.")
                }
                (ReceiptStatus::Rejected, Some("Unsupported"), None) => {
                    format!(
                        "{name} can't zero its airspeed sensor: its firmware does not support it."
                    )
                }
                (ReceiptStatus::Rejected, _, None) => format!(
                    "{name} refused to zero its airspeed sensor. Disarm it, cover the pitot and \
                     try again."
                ),
                (ReceiptStatus::Timeout, ..) => format!("{name} did not answer."),
                (_, why, _) => format!("It was not sent to {name}: {}.", why.unwrap_or("not sent")),
            };
            setup.instruction =
                (receipt.detail.as_deref() != Some("Unsupported")).then_some(Instruction::TryAgain);
            setup.detail = Some(refusal.clone());
            format!("Airspeed zero failed: {refusal}")
        };
        if accepted || receipt.status == ReceiptStatus::Rejected {
            let event = HistoryEvent::Calibrated {
                kind: SetupKind::AirspeedZero,
                succeeded: accepted,
                text,
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        }
        self.save_setup(setup, true, actor());
    }

    /// The vehicle's airspeed telemetry came in (S190): the check's peak,
    /// the zero's reading, and the ratio's estimates.
    pub(crate) fn review_airspeed(&mut self, vehicle_id: &str) {
        let Some(setup) = self.row_setup(vehicle_id, SetupRow::Airspeed) else {
            return;
        };
        let now = self.clock.now_us();
        match setup.kind {
            SetupKind::AirspeedCheck if setup.step == SetupStep::Running => {
                self.review_check(setup, now);
            }
            SetupKind::AirspeedZero
                if setup.step.running()
                    || setup
                        .finished_at_us
                        .is_some_and(|at| now.saturating_sub(at) < AFTER_ZERO_US) =>
            {
                let reading = self.telemetry_value(vehicle_id, READING);
                let mut setup = setup;
                if let (Some(run), Some(reading)) = (setup.airspeed.as_mut(), reading)
                    && run.reading_mps != Some(reading)
                {
                    run.reading_mps = Some(reading);
                    setup.updated_at_us = now;
                    self.save_setup(setup, false, actor());
                }
            }
            SetupKind::AirspeedRatio if setup.step == SetupStep::Running => {
                self.review_ratio(setup, now);
            }
            _ => {}
        }
    }

    /// The check: passed once the reading rose far enough, failed once its
    /// time is up.
    fn review_check(&mut self, mut setup: Setup, now: u64) {
        let reading = self.telemetry_value(&setup.vehicle_id, READING);
        let Some(run) = setup.airspeed.as_mut() else {
            return;
        };
        let start = run.start_mps.unwrap_or(0.0).max(0.0);
        if let Some(reading) = reading {
            run.reading_mps = Some(reading);
            run.peak_mps = Some(run.peak_mps.map_or(reading, |p| p.max(reading)));
        }
        let peak = run.peak_mps.unwrap_or(start);
        let passed = peak >= start + RISE_MPS;
        let late = run.until_us.is_some_and(|until| now >= until);
        setup.updated_at_us = now;
        if passed {
            setup.step = SetupStep::Succeeded;
            setup.instruction = None;
            setup.detail = None;
            setup.finished_at_us = Some(now);
            setup.progress = 100;
            self.save_setup(setup, true, actor());
        } else if late {
            let name = self.vehicle_name(&setup.vehicle_id);
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::CheckPitot);
            setup.finished_at_us = Some(now);
            setup.detail = Some(if peak - start < FLAT_MPS {
                format!(
                    "{name}'s airspeed stayed at {} m/s and did not move. Check the sensor is \
                     connected and working, and the pitot's tubes are clear.",
                    round1(start)
                )
            } else {
                format!(
                    "{name}'s airspeed rose only to {} m/s. Check the pitot's tubes for a \
                     blockage or a leak, and that they are not swapped.",
                    round1(peak)
                )
            });
            self.save_setup(setup, true, actor());
        } else {
            self.save_setup(setup, false, actor());
        }
    }

    /// The ratio: each fresh estimate kept; settled once it moved less
    /// than 2% over a minute.
    fn review_ratio(&mut self, mut setup: Setup, now: u64) {
        let vehicle_id = setup.vehicle_id.clone();
        // Each estimate once, at the time it was heard.
        let fresh = self
            .telemetry(&vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == ESTIMATE))
            .map(|i| (if i.time_us == 0 { now } else { i.time_us }, i.value))
            .filter(|(at, _)| now.saturating_sub(*at) < FRESH_US)
            .and_then(|(at, value)| Some((at, value?)))
            .filter(|(_, r)| r.is_finite() && *r > 0.0);
        let Some((heard, estimate)) = fresh else {
            return;
        };
        let samples = self
            .setup
            .airspeed
            .runs
            .entry(vehicle_id.clone())
            .or_default();
        if samples.last().is_some_and(|(at, _)| *at >= heard) {
            return;
        }
        samples.push((heard, estimate));
        samples.retain(|(at, _)| heard.saturating_sub(*at) <= SETTLE_US);
        let first = samples.first().map_or(heard, |(at, _)| *at);
        let gathered = heard.saturating_sub(first);
        let spread = spread(samples);
        let Some(run) = setup.airspeed.as_mut() else {
            return;
        };
        run.ratio = Some(estimate);
        run.spread_pct = spread.map(|s| (s * 100.0).round() / 100.0);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "seconds of a minute at most"
        )]
        let seconds = (gathered / 1_000_000) as u32;
        run.gathered_s = seconds;
        setup.updated_at_us = now;
        #[allow(clippy::cast_possible_truncation, reason = "a percentage")]
        let progress = (gathered * 100 / SETTLE_US).min(99) as u8;
        setup.progress = progress;
        let settled = gathered + 1_000_000 >= SETTLE_US && spread.is_some_and(|s| s < SETTLED_PCT);
        if !settled {
            self.save_setup(setup, false, actor());
            return;
        }
        self.settle_ratio(setup, estimate);
    }

    /// Settled: autocal goes back to 0, and the ratio is written so the
    /// vehicle keeps it. It succeeds once the vehicle has taken both.
    fn settle_ratio(&mut self, mut setup: Setup, estimate: f64) {
        let Some(vehicle) = self.vehicle(&setup.vehicle_id).cloned() else {
            return;
        };
        let Some(run) = setup.airspeed.as_mut() else {
            return;
        };
        let ratio = rounded(estimate);
        run.settled = true;
        let autocal = run.autocal_param.clone().unwrap_or_default();
        let name = run.ratio_param.clone().unwrap_or_default();
        let now_ratio = self.param_now(&vehicle.id, &name).unwrap_or(estimate);
        run.writes.push(RadioWrite {
            name: autocal.clone(),
            from: 1.0,
            to: 0.0,
            state: WriteState::Writing,
        });
        run.writes.push(RadioWrite {
            name: name.clone(),
            from: run.ratio_before.unwrap_or(now_ratio),
            to: ratio,
            state: WriteState::Writing,
        });
        setup.instruction = None;
        setup.progress = 100;
        setup.updated_at_us = self.clock.now_us();
        let author = setup.started_by.clone().unwrap_or_default();
        self.save_setup(setup, true, actor());
        self.turn_autocal_off(&vehicle, &autocal);
        self.write_airspeed(&vehicle, (actor(), author), &name, ratio, Purpose::Kept);
    }

    /// One of the airspeed's values was written, or not (S190).
    pub(crate) fn airspeed_written(&mut self, order_id: &str, taken: bool) {
        let Some((vehicle_id, name, purpose)) = self.setup.airspeed.writes.remove(order_id) else {
            return;
        };
        // Refused or unanswered, it is written again 10 s after it was sent.
        if purpose == Purpose::Off && taken {
            self.setup.airspeed.off_sent.remove(&vehicle_id);
        }
        let Some(mut setup) = self
            .row_setup(&vehicle_id, SetupRow::Airspeed)
            .filter(|s| s.kind == SetupKind::AirspeedRatio)
        else {
            return;
        };
        let Some(vehicle) = self.vehicle(&vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let Some(run) = setup.airspeed.as_mut() else {
            return;
        };
        let to = match purpose {
            Purpose::On => 1.0,
            Purpose::Off => 0.0,
            Purpose::Kept => run
                .writes
                .iter()
                .rev()
                .find(|w| w.name == name)
                .map_or(0.0, |w| w.to),
        };
        if let Some(write) = run
            .writes
            .iter_mut()
            .rev()
            .find(|w| w.name == name && same_value(w.to, to) && w.state == WriteState::Writing)
        {
            write.state = if taken {
                WriteState::Written
            } else {
                WriteState::Refused
            };
        }
        if purpose == Purpose::Off && taken {
            run.autocal_on = false;
        }
        setup.updated_at_us = now;
        match purpose {
            Purpose::On if setup.step == SetupStep::Starting => {
                if taken {
                    setup.step = SetupStep::Running;
                    setup.instruction = Some(Instruction::FlyCircles);
                    self.setup
                        .airspeed
                        .runs
                        .insert(vehicle_id.clone(), Vec::new());
                } else {
                    setup.step = SetupStep::Failed;
                    setup.instruction = Some(Instruction::TryAgain);
                    setup.finished_at_us = Some(now);
                    setup.detail = Some(format!(
                        "{} did not take {name} = 1. Try again, or check it in Parameters.",
                        vehicle.name
                    ));
                    self.setup.airspeed.runs.remove(&vehicle_id);
                }
            }
            _ if setup.step == SetupStep::Running
                && run_settled(&setup)
                && !setup
                    .airspeed
                    .as_ref()
                    .is_some_and(|r| r.writes.iter().any(|w| w.state == WriteState::Writing)) =>
            {
                self.finish_ratio(&mut setup, &vehicle);
            }
            _ => {}
        }
        self.save_setup(setup, true, actor());
    }

    /// The settled ratio's writes are answered: kept, or why not; in the
    /// history either way.
    fn finish_ratio(&mut self, setup: &mut Setup, vehicle: &Vehicle) {
        let now = self.clock.now_us();
        self.setup.airspeed.runs.remove(&vehicle.id);
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        let Some(run) = setup.airspeed.as_ref() else {
            return;
        };
        let name = run.ratio_param.clone().unwrap_or_default();
        let kept = run
            .writes
            .iter()
            .rev()
            .find(|w| w.name == name)
            .filter(|w| w.state == WriteState::Written)
            .map(|w| w.to);
        let before = run
            .ratio_before
            .map_or_else(|| "unknown".to_owned(), ratio_text);
        let spread = run.spread_pct.map_or_else(String::new, |s| {
            format!(", within {}% over a minute of circling", round1(s))
        });
        let autocal_off = !run.autocal_on;
        let text = if let Some(kept) = kept {
            setup.step = SetupStep::Succeeded;
            setup.detail = (!autocal_off).then(|| {
                format!(
                    "{} has not taken its automatic calibration's 0 yet: UXDuck writes it again \
                     until it does.",
                    vehicle.name
                )
            });
            format!(
                "Airspeed ratio set to {} (was {before}){spread}",
                ratio_text(kept)
            )
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            let ratio = run.ratio.map_or_else(|| "unknown".to_owned(), ratio_text);
            let why = format!(
                "{} did not take {name} = {ratio}. Set it in Parameters.",
                vehicle.name
            );
            setup.detail = Some(why.clone());
            format!("Airspeed ratio not kept: {why}")
        };
        let event = HistoryEvent::Calibrated {
            kind: SetupKind::AirspeedRatio,
            succeeded: setup.step == SetupStep::Succeeded,
            text,
        };
        match setup.started_by.clone() {
            Some(author) => self.record_by(&vehicle.id, event, author),
            None => self.record(&vehicle.id, event),
        }
    }

    /// Stops a ratio before it settled: cancelled, landed, its link lost or
    /// a restart. Autocal is turned off now if the vehicle can be told,
    /// else as soon as it can; the ratio stays as the vehicle has it.
    pub(super) fn stop_ratio(
        &mut self,
        vehicle_id: &str,
        step: SetupStep,
        why: &str,
    ) -> Option<Setup> {
        let mut setup = self
            .row_setup(vehicle_id, SetupRow::Airspeed)
            .filter(|s| s.kind == SetupKind::AirspeedRatio && s.step.running())?;
        let vehicle = self.vehicle(vehicle_id).cloned()?;
        let now = self.clock.now_us();
        self.setup.airspeed.runs.remove(vehicle_id);
        setup.step = step;
        setup.instruction = None;
        setup.detail = Some(why.to_owned());
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        let run = setup.airspeed.as_deref().cloned().unwrap_or_default();
        let on_written = run
            .writes
            .iter()
            .any(|w| same_value(w.to, 1.0) && w.state == WriteState::Written);
        if on_written || run.autocal_on {
            let ratio = self
                .param_now(vehicle_id, run.ratio_param.as_deref().unwrap_or_default())
                .or(run.ratio)
                .map_or_else(|| "unknown".to_owned(), ratio_text);
            let before = run
                .ratio_before
                .map_or_else(|| "unknown".to_owned(), ratio_text);
            let event = HistoryEvent::Calibrated {
                kind: SetupKind::AirspeedRatio,
                succeeded: false,
                text: format!(
                    "Airspeed ratio stopped before it settled: {} Its ratio is {ratio} (was \
                     {before}); its automatic calibration is turned off.",
                    why.trim_start_matches("Stopped: ")
                ),
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        }
        let saved = self.save_setup(setup, true, actor());
        if run.autocal_on
            && self.not_writable(&vehicle.link_id).is_none()
            && vehicle.presence == Presence::Online
            && let Some(name) = run.autocal_param.as_deref()
        {
            self.turn_autocal_off(&vehicle, name);
        }
        Some(saved)
    }

    /// The vehicle armed or disarmed: disarmed, it landed, and a ratio not
    /// settled stops.
    pub(crate) fn airspeed_armed(&mut self, vehicle_id: &str, armed: bool) {
        if armed {
            return;
        }
        let unsettled = self
            .row_setup(vehicle_id, SetupRow::Airspeed)
            .is_some_and(|s| {
                s.kind == SetupKind::AirspeedRatio && s.step.running() && !run_settled(&s)
            });
        if unsettled {
            let name = self.vehicle_name(vehicle_id);
            self.stop_ratio(
                vehicle_id,
                SetupStep::Cancelled,
                &format!("Stopped: {name} landed before its ratio settled."),
            );
        }
    }

    /// Once a second: a check whose time is up fails; a ratio whose link
    /// was lost, or that UXDuck no longer follows since it restarted,
    /// stops; and autocal left on is turned off when the vehicle can be
    /// told.
    pub fn expire_airspeed(&mut self, now: u64) {
        let rows: Vec<Setup> = self
            .state
            .setups
            .values()
            .filter(|s| s.kind.row() == SetupRow::Airspeed)
            .map(|s| {
                self.row_setup(&s.vehicle_id, SetupRow::Airspeed)
                    .unwrap_or_else(|| s.clone())
            })
            .collect();
        for setup in rows {
            let vehicle_id = setup.vehicle_id.clone();
            let Some(vehicle) = self.vehicle(&vehicle_id).cloned() else {
                continue;
            };
            match setup.kind {
                SetupKind::AirspeedCheck if setup.step == SetupStep::Running => {
                    self.review_check(setup, now);
                    continue;
                }
                SetupKind::AirspeedRatio if setup.step.running() && !run_settled(&setup) => {
                    if !self.setup.airspeed.runs.contains_key(&vehicle_id) {
                        self.stop_ratio(
                            &vehicle_id,
                            SetupStep::Cancelled,
                            "Stopped: UXDuck restarted during it.",
                        );
                    } else if vehicle.presence == Presence::Offline {
                        self.stop_ratio(
                            &vehicle_id,
                            SetupStep::Cancelled,
                            &format!("Stopped: UXDuck lost {}'s link.", vehicle.name),
                        );
                    }
                    continue;
                }
                _ => {}
            }
            let pending = setup
                .airspeed
                .as_ref()
                .filter(|r| r.autocal_on && !setup.step.running());
            let Some(name) = pending.and_then(|r| r.autocal_param.clone()) else {
                continue;
            };
            let due = self
                .setup
                .airspeed
                .off_sent
                .get(&vehicle_id)
                .is_none_or(|at| now.saturating_sub(*at) >= RETRY_US);
            if due
                && vehicle.presence == Presence::Online
                && self.not_writable(&vehicle.link_id).is_none()
            {
                self.turn_autocal_off(&vehicle, &name);
            }
        }
    }

    /// What a vehicle says of its airspeed: the zero's messages belong to
    /// the row (true: no notice), and a failure becomes its words; the
    /// automatic calibration's "ratio reset" while a ratio runs.
    pub(super) fn airspeed_said(&mut self, setup: &mut Setup, text: &str) -> bool {
        let now = self.clock.now_us();
        let lately = setup.step.running()
            || setup
                .finished_at_us
                .is_some_and(|at| now.saturating_sub(at) < super::SAID_US);
        if !lately {
            return false;
        }
        let lower = text.trim().to_lowercase();
        match setup.kind {
            SetupKind::AirspeedZero => {
                let ours = lower.starts_with("updating barometer")
                    || lower.starts_with("barometer calibration")
                    || (lower.starts_with("airspeed") && lower.contains("calibrat"));
                if ours && lower.contains("fail") && setup.step.running() {
                    // Kept for the answer, which says it failed.
                    setup.detail = Some(text.trim().trim_end_matches('.').to_owned());
                    setup.updated_at_us = now;
                    self.save_setup(setup.clone(), true, actor());
                }
                ours
            }
            SetupKind::AirspeedRatio => lower.starts_with("airspeed") && lower.contains("ratio"),
            _ => false,
        }
    }

    /// The vehicle restarted during a ratio: it stops, and autocal, which
    /// the vehicle kept, is turned off.
    pub(super) fn airspeed_restarted(&mut self, vehicle_id: &str) -> bool {
        self.stop_ratio(
            vehicle_id,
            SetupStep::Cancelled,
            "Stopped: it restarted during it.",
        )
        .is_some()
    }

    /// The airspeed's row (S190): running, zeroed when UXDuck zeroed it,
    /// and a word when autocal is still on.
    pub(super) fn airspeed_row(
        &self,
        vehicle: &Vehicle,
        procedure: Option<&Setup>,
        sensor: Sensor,
    ) -> SetupRowState {
        let calibrated_at_us = self.calibrated_at(vehicle, SetupRow::Airspeed);
        let state = if procedure.is_some_and(|p| p.step.running()) {
            RowState::Running
        } else if calibrated_at_us.is_some()
            || procedure.is_some_and(|p| {
                p.kind == SetupKind::AirspeedZero && p.step == SetupStep::Succeeded
            })
        {
            RowState::Calibrated
        } else {
            RowState::Unknown
        };
        let detail = procedure
            .and_then(|p| p.airspeed.as_ref())
            .filter(|r| r.autocal_on && procedure.is_some_and(|p| !p.step.running()))
            .map(|r| {
                format!(
                    "Its automatic airspeed calibration ({}) is still on: UXDuck turns it off as \
                     soon as it can tell {}.",
                    r.autocal_param.as_deref().unwrap_or(AUTOCAL_NAMES[0]),
                    vehicle.name
                )
            });
        SetupRowState {
            row: SetupRow::Airspeed.into(),
            state,
            calibrated_at_us,
            detail,
            procedure: (sensor == Sensor::Present).then_some(SetupKind::AirspeedZero.into()),
        }
    }
}

/// The ratio settled, and is being kept.
fn run_settled(setup: &Setup) -> bool {
    setup.airspeed.as_ref().is_some_and(|r| r.settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_a_percentage_of_the_latest() {
        assert_eq!(spread(&[]), None);
        let s = spread(&[(0, 1.9), (1, 2.0), (2, 1.95)]).unwrap();
        assert!((s - 5.128).abs() < 0.01, "{s}");
        assert_eq!(ratio_text(1.99), "1.99");
        assert_eq!(ratio_text(2.0), "2");
        assert!((rounded(1.987_65) - 1.988).abs() < 1e-9);
    }
}

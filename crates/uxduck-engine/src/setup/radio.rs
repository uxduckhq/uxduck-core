//! The radio calibration (S187, ADR-0037 V8), done by UXDuck rather than
//! the vehicle: `setup/start` asks the vehicle for `RC_CHANNELS` ten times a
//! second (its answer starts it), and each reading arrives as telemetry
//! (`rc.1` to `rc.18`). The person centres the sticks and confirms (each
//! channel's centre), moves every stick and switch to its ends and confirms
//! (each channel's lowest and highest), then moves each stick in turn as
//! asked and holds it, which is found as it is held: the one channel that
//! moved far from where it was when asked and stayed there, and which way. The sticks are compared with
//! `RCMAP_*` and `RCn_REVERSED`, and a difference is offered as a fix.
//!
//! Confirmed at review, the ranges (`RCn_MIN`, `RCn_MAX`, `RCn_TRIM`, the
//! throttle's trim at its lowest as `ArduPilot` expects, except on a rover
//! or boat whose throttle centres) and the fixes accepted are written
//! through the parameter path as one change, with one reason, so they are
//! in the vehicle's history and Parameters like any change, each with its
//! value before for undoing. Before writing, ranges must be plausible and,
//! with the throttle failsafe on, the throttle's lowest must be above its
//! value. The session's RC overrides take every value the vehicle echoes,
//! so a controller uses the new ranges at once (ADR-0005).
//!
//! Refused while UXDuck drives the vehicle with a controller or the
//! keyboard, whose overrides show in `RC_CHANNELS`; one that starts during
//! it stops it.
//!
//! PX4 (S191): the same capture, started with `MAV_CMD_PREFLIGHT_CALIBRATION`
//! parameter 4 at 1 (PX4 stops using its radio while it runs) and ended with
//! every parameter 0, written as PX4's `RCn_MIN`, `RCn_MAX`, `RCn_TRIM`,
//! `RCn_REV` (-1 reversed, 1 not) and `RC_MAP_ROLL`/`_PITCH`/`_THROTTLE`/
//! `_YAW` (0 is no channel: a stick found where its map is 0 is written
//! without asking). PX4 reads its map as it changes: no restart. Its
//! throttle failsafe is `RC_FAILS_THR` (0 off).

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::setup::{
    RowState, Setup as SetupDto, SetupConfirmParams, SetupRowState,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::{parameter_id, same_value};
use uxduck_core::domain::setup::{
    Fix, Instruction, Radio, RadioChannel, RadioFix, RadioStep, RadioWrite, Setup, SetupKind,
    SetupRow, SetupStep, Stick, StickFound, WriteState, setup_id,
};
use uxduck_core::domain::vehicle::Vehicle;

use super::{Order, actor, no_vehicle};
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// `MAV_CMD_SET_MESSAGE_INTERVAL`.
const MAV_CMD_SET_MESSAGE_INTERVAL: u16 = 511;
/// `RC_CHANNELS`.
const RC_CHANNELS: f32 = 65.0;
/// Ten times a second while it runs.
const RC_EVERY_US: f32 = 100_000.0;
/// A channel moved at least this far between its ends counts as moved,
/// and is written; every stick must have.
const SPAN_US: u16 = 400;
/// Readings outside these are not a receiver's.
const LOWEST_US: u16 = 800;
const HIGHEST_US: u16 = 2200;
/// A stick asked for is found once a channel moved this much of half its
/// range from where it was when asked…
const MOVED: f64 = 0.6;
/// …and was held there, within `HELD_SPAN` of its range, this long: a
/// stick still being swept is not one held as asked.
const HELD_US: u64 = 600_000;
const HELD_SPAN: f64 = 0.05;
/// A channel this near where it was centred, as a part of half its range,
/// is at rest there.
const REST: f64 = 0.2;
/// `ArduPilot`'s pre-arm margin: the throttle's lowest must be more than
/// this above the failsafe's value.
const FAILSAFE_MARGIN_US: f64 = 10.0;
/// The reason kept with each value written.
pub(super) const REASON: &str = "Radio calibration";

/// Asks for the radio's channels ten times a second, or at their usual
/// rate again.
/// A stick's map parameter: `RCMAP_ROLL`, or PX4's `RC_MAP_ROLL`.
const fn map_name(stick: Stick, px4: bool) -> &'static str {
    if px4 { stick.px4_map() } else { stick.rcmap() }
}

/// A channel's reversal parameter: `RC1_REVERSED`, or PX4's `RC1_REV`.
fn reversed_name(channel: u8, px4: bool) -> String {
    if px4 {
        format!("RC{channel}_REV")
    } else {
        format!("RC{channel}_REVERSED")
    }
}

/// Whether a reversal parameter's value is reversed: PX4's -1, `ArduPilot`'s
/// not 0.
fn is_reversed(value: f64, px4: bool) -> bool {
    if px4 { value < 0.0 } else { value != 0.0 }
}

/// A stick's map or a channel's reversal, of either firmware.
fn mapping(name: &str) -> bool {
    name.starts_with("RCMAP_") || name.starts_with("RC_MAP_")
}

fn reversal(name: &str) -> bool {
    name.ends_with("_REVERSED") || name.ends_with("_REV")
}

pub(super) fn stream(on: bool) -> Command {
    Command::Long {
        command: MAV_CMD_SET_MESSAGE_INTERVAL,
        params: vec![RC_CHANNELS, if on { RC_EVERY_US } else { 0.0 }],
    }
}

/// Keeps the latest readings: while the sticks are centred, as each
/// channel's centre; while moved, widening its lowest and highest.
fn take(radio: &mut Radio, readings: &[(u8, u16)]) {
    for &(channel, value) in readings {
        let widen = matches!(radio.step, RadioStep::Ends | RadioStep::Sticks);
        match radio.channels.iter_mut().find(|c| c.channel == channel) {
            Some(c) if widen => {
                c.value = value;
                c.min = c.min.min(value);
                c.max = c.max.max(value);
            }
            Some(c) if radio.step == RadioStep::Centre => {
                *c = RadioChannel {
                    channel,
                    value,
                    min: value,
                    max: value,
                    centre: value,
                };
            }
            Some(c) => c.value = value,
            None => radio.channels.push(RadioChannel {
                channel,
                value,
                min: value,
                max: value,
                centre: value,
            }),
        }
    }
    radio.channels.sort_by_key(|c| c.channel);
}

/// What the readings say of the stick asked for.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Nothing,
    /// One channel not yet taken by another moved far enough from where it
    /// was when asked.
    Found(StickFound),
    /// Several moved at once, as while the sticks are still being swept or
    /// come back to centre: not one stick. It waits, from where they are.
    Several,
}

fn found(radio: &Radio, from: &BTreeMap<u8, u16>) -> Seen {
    let Some(stick) = radio.asked else {
        return Seen::Nothing;
    };
    let half = |c: &RadioChannel| f64::from(c.span()) / 2.0;
    let moved: Vec<(u8, f64)> = radio
        .channels
        .iter()
        .filter(|c| c.span() >= SPAN_US && !radio.sticks.iter().any(|f| f.channel == c.channel))
        // Roll, pitch and yaw spring back to the middle: a channel that
        // rested near an end when the sticks were centred is a switch.
        .filter(|c| {
            stick == Stick::Throttle
                || (f64::from(c.centre) - f64::from(c.min) - half(c)).abs() <= half(c) / 2.0
        })
        .filter_map(|c| {
            let at = f64::from(*from.get(&c.channel)?);
            let moved = (f64::from(c.value) - at) / half(c);
            // Away from where it was centred too, the same way: a switch
            // put back as it was is not a stick moved.
            let off = (f64::from(c.value) - f64::from(c.centre)) / half(c);
            (moved.abs() >= MOVED && off.abs() >= MOVED && off.signum() == moved.signum())
                .then_some((c.channel, moved))
        })
        .collect();
    match moved.as_slice() {
        [] => Seen::Nothing,
        [(channel, moved)] => Seen::Found(StickFound {
            stick,
            channel: *channel,
            lowers: *moved < 0.0,
        }),
        _ => Seen::Several,
    }
}

/// A channel back near where it was centred is at rest: where it is now
/// is where a move asked for is measured from. One left away from its
/// centre, such as a throttle left down, keeps where it was when asked.
fn settle(radio: &Radio, from: &mut BTreeMap<u8, u16>) {
    for c in radio.channels.iter().filter(|c| c.span() >= SPAN_US) {
        let off = (f64::from(c.value) - f64::from(c.centre)) / (f64::from(c.span()) / 2.0);
        if off.abs() < REST {
            from.insert(c.channel, c.value);
        }
    }
}

/// Each channel's reading now.
fn readings_of(radio: &Radio) -> BTreeMap<u8, u16> {
    radio
        .channels
        .iter()
        .map(|c| (c.channel, c.value))
        .collect()
}

/// Whether a stick's range is a receiver's and wide enough.
fn range_problem(c: &RadioChannel) -> Option<String> {
    if c.min < LOWEST_US || c.max > HIGHEST_US {
        Some(format!(
            "Channel {} read {} to {} µs, outside {LOWEST_US} to {HIGHEST_US}: check the \
             receiver's output.",
            c.channel, c.min, c.max
        ))
    } else {
        None
    }
}

/// "roll channel 1 1004–1996 (reversed)".
fn range_words(c: &RadioChannel, stick: Option<Stick>, reversed: bool) -> String {
    let what = stick.map_or_else(
        || format!("channel {}", c.channel),
        |s| format!("{} channel {}", s.name(), c.channel),
    );
    let reversed = if reversed { " (reversed)" } else { "" };
    format!("{what} {}–{}{reversed}", c.min, c.max)
}

/// Confirms a step of the radio calibration: the sticks centred, everything
/// moved to its ends, or the review, with the fixes accepted.
#[allow(clippy::too_many_lines, reason = "one arm per step")]
pub(super) fn confirm(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SetupConfirmParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let Some(mut setup) = engine
        .current_setup(&vehicle.id)
        .filter(|s| s.step == SetupStep::Running && s.kind == SetupKind::Radio)
    else {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("{} has not sent its radio's channels yet.", vehicle.name),
        ));
    };
    let mut radio = setup.radio.clone().unwrap_or_default();
    let Some(done) = params.radio_step.map(RadioStep::from) else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Say which step of the radio calibration is done.",
        ));
    };
    if done != radio.step {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{}'s radio calibration has moved on. Follow its row.",
                vehicle.name
            ),
        ));
    }
    if let Some(why) = engine.overridden(&vehicle) {
        return Err(ApiError::new(ErrorKind::Busy, why));
    }
    let actor = engine.actor(connection);
    match radio.step {
        RadioStep::Centre => {
            if radio.channels.is_empty() {
                return Err(ApiError::new(
                    ErrorKind::Busy,
                    format!(
                        "No channels from {}'s receiver yet. Is the transmitter on and bound?",
                        vehicle.name
                    ),
                ));
            }
            for c in &mut radio.channels {
                *c = RadioChannel {
                    channel: c.channel,
                    value: c.value,
                    min: c.value,
                    max: c.value,
                    centre: c.value,
                };
            }
            radio.step = RadioStep::Ends;
            setup.instruction = Some(Instruction::MoveToEnds);
        }
        RadioStep::Ends => {
            let moved: Vec<&RadioChannel> = radio
                .channels
                .iter()
                .filter(|c| c.span() >= SPAN_US)
                .collect();
            if moved.len() < Stick::ALL.len() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "Only {} channels moved far enough. Move every stick and switch all \
                         the way to both ends, then press Done.",
                        moved.len()
                    ),
                ));
            }
            if let Some(problem) = moved.iter().find_map(|c| range_problem(c)) {
                return Err(ApiError::new(ErrorKind::InvalidValue, problem));
            }
            radio.step = RadioStep::Sticks;
            radio.asked = Some(Stick::Roll);
            engine
                .setup
                .radio_from
                .insert(vehicle.id.clone(), readings_of(&radio));
            setup.instruction = Some(Instruction::MoveStick);
        }
        RadioStep::Sticks => {
            let stick = radio.asked.map_or("stick", Stick::words);
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("Move the {stick} as asked: UXDuck finds it as it moves."),
            ));
        }
        RadioStep::Review => {
            if let Some(why) = engine.cannot_set_up(&vehicle) {
                return Err(ApiError::new(ErrorKind::InvalidValue, why));
            }
            let writes = match engine.radio_plan(&vehicle, &radio, &params.accept) {
                Ok(writes) => writes,
                Err(problem) => {
                    setup.detail = Some(problem.clone());
                    setup.updated_at_us = engine.clock.now_us();
                    engine.save_setup(setup, true, actor);
                    return Err(ApiError::new(ErrorKind::InvalidValue, problem));
                }
            };
            radio.writes = writes;
            radio.step = RadioStep::Writing;
            setup.instruction = None;
            setup.detail = None;
            setup.started_by = Some(engine.author(connection));
            setup.radio = Some(radio);
            setup.updated_at_us = engine.clock.now_us();
            let setup = engine.write_radio(setup, &vehicle, connection);
            return to_value(&SetupDto::from(&setup));
        }
        RadioStep::Writing => {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("Writing the radio's ranges to {}…", vehicle.name),
            ));
        }
    }
    setup.detail = None;
    setup.radio = Some(radio);
    setup.updated_at_us = engine.clock.now_us();
    let saved = engine.save_setup(setup, true, actor);
    to_value(&SetupDto::from(&saved))
}

impl Engine {
    /// A parameter's value on the vehicle, as last read.
    fn param_value(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
    }

    /// Why the radio cannot be calibrated now, beyond any procedure's
    /// reasons: UXDuck drives it, or its parameters are not read.
    pub(super) fn can_calibrate_radio(&self, vehicle: &Vehicle) -> Result<(), ApiError> {
        if let Some(why) = self.overridden(vehicle) {
            return Err(ApiError::new(ErrorKind::Busy, why));
        }
        if self.param_value(&vehicle.id, "RC1_MIN").is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "UXDuck has not read {}'s parameters yet. Refresh them in Parameters, then \
                     try again.",
                    vehicle.name
                ),
            ));
        }
        Ok(())
    }

    /// A controller or the keyboard UXDuck drives the vehicle with: its RC
    /// overrides show in `RC_CHANNELS`, so the radio cannot be read.
    pub(super) fn overridden(&self, vehicle: &Vehicle) -> Option<String> {
        let controller = self.state.controllers.values().find(|c| {
            c.claim.is_some()
                && c.target
                    .as_deref()
                    .is_some_and(|t| t == vehicle.id || vehicle.aliases.iter().any(|a| a == t))
        })?;
        Some(format!(
            "{} is driving {}, and its stick input shows in the radio's channels. Release it \
             first.",
            controller.name, vehicle.name
        ))
    }

    /// The vehicle answered the request for its channels: running, or why
    /// not.
    pub(super) fn radio_answered(&mut self, mut setup: Setup, receipt: &Receipt) {
        let now = self.clock.now_us();
        let name = self.vehicle_name(&setup.vehicle_id);
        if receipt.status == ReceiptStatus::Accepted {
            setup.step = SetupStep::Running;
            setup.instruction = Some(Instruction::CentreSticks);
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            setup.detail = Some(match receipt.status {
                ReceiptStatus::Timeout => format!("{name} did not answer."),
                ReceiptStatus::Rejected => format!("{name} would not send its radio's channels."),
                _ => format!(
                    "It was not sent to {name}: {}.",
                    receipt.detail.as_deref().unwrap_or("not sent")
                ),
            });
            setup.finished_at_us = Some(now);
        }
        setup.updated_at_us = now;
        self.save_setup(setup, true, actor());
    }

    /// The vehicle's radio channels came in (S187): while its calibration
    /// runs, they widen the ranges and find the stick asked for. A
    /// controller that started driving it stops the calibration.
    pub(crate) fn review_radio(&mut self, vehicle_id: &str) {
        let running = self
            .setup
            .unsaved
            .get(&setup_id(vehicle_id, SetupRow::Radio))
            .or_else(|| {
                self.state
                    .setups
                    .get(&setup_id(vehicle_id, SetupRow::Radio))
            })
            .is_some_and(|s| s.kind == SetupKind::Radio && s.step == SetupStep::Running);
        if !running {
            return;
        }
        let (Some(vehicle), Some(mut setup)) = (
            self.vehicle(vehicle_id).cloned(),
            self.current_setup(vehicle_id),
        ) else {
            return;
        };
        let now = self.clock.now_us();
        if let Some(why) = self.overridden(&vehicle) {
            self.end_radio(&mut setup, &vehicle, Err(why));
            self.save_setup(setup, true, actor());
            return;
        }
        let readings: Vec<(u8, u16)> = self
            .telemetry
            .get(vehicle_id)
            .map(|t| {
                t.items
                    .iter()
                    .filter_map(|item| {
                        let channel = item.key.strip_prefix("rc.")?.parse::<u8>().ok()?;
                        #[allow(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "a pulse width in microseconds"
                        )]
                        let value = item.value?.round().clamp(0.0, 65_535.0) as u16;
                        Some((channel, value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if readings.is_empty() {
            return;
        }
        let mut radio = setup.radio.take().unwrap_or_default();
        take(&mut radio, &readings);
        let mut at_once = false;
        if radio.step == RadioStep::Sticks {
            let from = self
                .setup
                .radio_from
                .entry(vehicle.id.clone())
                .or_insert_with(|| readings_of(&radio));
            settle(&radio, from);
            let from = from.clone();
            let seen = found(&radio, &from);
            if seen == Seen::Several {
                self.setup
                    .radio_from
                    .insert(vehicle.id.clone(), readings_of(&radio));
            }
            let held = match seen {
                Seen::Found(stick) => self.radio_held(&vehicle.id, &radio, stick, now),
                Seen::Nothing | Seen::Several => {
                    self.setup.radio_held.remove(&vehicle.id);
                    None
                }
            };
            if let Some(stick) = held {
                radio.sticks.push(stick);
                radio.asked = Stick::ALL
                    .into_iter()
                    .find(|s| !radio.sticks.iter().any(|f| f.stick == *s));
                self.setup
                    .radio_from
                    .insert(vehicle.id.clone(), readings_of(&radio));
                at_once = true;
                if radio.asked.is_none() {
                    radio.step = RadioStep::Review;
                    radio.fixes = self.radio_fixes(&vehicle.id, &radio);
                    setup.instruction = Some(Instruction::CheckRadio);
                }
            }
        }
        // At review, whether it can be written: the failsafe may have
        // changed in Parameters meanwhile.
        if radio.step == RadioStep::Review {
            let accepted: Vec<String> = radio.fixes.iter().map(|f| f.name.clone()).collect();
            let problem = self.radio_plan(&vehicle, &radio, &accepted).err();
            if problem != setup.detail {
                setup.detail = problem;
                at_once = true;
            }
        }
        setup.radio = Some(radio);
        setup.updated_at_us = now;
        self.save_setup(setup, at_once, actor());
    }

    /// A stick seen moved as asked, once it has been held there long
    /// enough; until then, when it was first seen there.
    fn radio_held(
        &mut self,
        vehicle_id: &str,
        radio: &Radio,
        stick: StickFound,
        now: u64,
    ) -> Option<StickFound> {
        let channel = radio.channels.iter().find(|c| c.channel == stick.channel)?;
        let tolerance = f64::from(channel.span()) * HELD_SPAN;
        match self.setup.radio_held.get(vehicle_id) {
            Some(&(held, value, since))
                if held == stick.channel
                    && (f64::from(channel.value) - f64::from(value)).abs() <= tolerance =>
            {
                if now.saturating_sub(since) < HELD_US {
                    return None;
                }
                self.setup.radio_held.remove(vehicle_id);
                Some(stick)
            }
            _ => {
                self.setup
                    .radio_held
                    .insert(vehicle_id.to_owned(), (stick.channel, channel.value, now));
                None
            }
        }
    }

    /// The sticks found compared with `RCMAP_*` and `RCn_REVERSED`: each
    /// difference, offered as a fix. A parameter the vehicle does not have
    /// is not offered.
    fn radio_fixes(&self, vehicle_id: &str, radio: &Radio) -> Vec<RadioFix> {
        let px4 = self.vehicle(vehicle_id).is_some_and(super::px4::px4);
        let mut fixes = Vec::new();
        for found in &radio.sticks {
            let stick = found.stick;
            let map = map_name(stick, px4);
            if let Some(mapped) = self.param_value(vehicle_id, map)
                && !same_value(mapped, f64::from(found.channel))
            {
                fixes.push(RadioFix {
                    name: map.to_owned(),
                    stick,
                    from: mapped,
                    to: f64::from(found.channel),
                });
            }
            let name = reversed_name(found.channel, px4);
            let needed = found.lowers != stick.asked_lowers();
            if let Some(reversed) = self.param_value(vehicle_id, &name)
                && is_reversed(reversed, px4) != needed
            {
                let to = match (needed, px4) {
                    (true, true) => -1.0,
                    (true, false) | (false, true) => 1.0,
                    (false, false) => 0.0,
                };
                fixes.push(RadioFix {
                    name,
                    stick,
                    from: reversed,
                    to,
                });
            }
        }
        fixes
    }

    /// What writing the calibration with these fixes accepted would write,
    /// each with its value now; or why it may not be written.
    pub(super) fn radio_plan(
        &self,
        vehicle: &Vehicle,
        radio: &Radio,
        accepted: &[String],
    ) -> Result<Vec<RadioWrite>, String> {
        let px4 = super::px4::px4(vehicle);
        // PX4's map of 0 is no channel: the stick found is written without
        // asking.
        let fixes: Vec<&RadioFix> = radio
            .fixes
            .iter()
            .filter(|f| accepted.contains(&f.name) || (px4 && mapping(&f.name) && f.from == 0.0))
            .collect();
        if let Some(unknown) = accepted
            .iter()
            .find(|name| !radio.fixes.iter().any(|f| &f.name == *name))
        {
            return Err(format!("{unknown} is not one of the fixes offered."));
        }
        // Each stick's channel as ArduPilot will take it.
        let channel_of = |stick: Stick| -> f64 {
            fixes
                .iter()
                .find(|f| f.name == map_name(stick, px4))
                .map(|f| f.to)
                .or_else(|| self.param_value(&vehicle.id, map_name(stick, px4)))
                .unwrap_or_else(|| f64::from(stick.default_channel()))
        };
        let channel = |number: f64| {
            radio
                .channels
                .iter()
                .find(|c| same_value(f64::from(c.channel), number))
        };
        for stick in Stick::ALL {
            let number = channel_of(stick);
            match channel(number) {
                Some(c) if c.span() >= SPAN_US => {}
                Some(c) => {
                    return Err(format!(
                        "The {} (channel {}) moved only {} µs. Start again and move it all the \
                         way to both ends.",
                        stick.name(),
                        c.channel,
                        c.span()
                    ));
                }
                None => {
                    return Err(format!(
                        "{} has no channel {number} for the {}.",
                        vehicle.name,
                        stick.name()
                    ));
                }
            }
        }
        let moved: Vec<&RadioChannel> = radio
            .channels
            .iter()
            .filter(|c| c.span() >= SPAN_US)
            .collect();
        if let Some(problem) = moved.iter().find_map(|c| range_problem(c)) {
            return Err(problem);
        }
        let throttle = channel(channel_of(Stick::Throttle))
            .ok_or_else(|| "No throttle channel.".to_owned())?;
        if let Some(problem) = self.failsafe_problem(vehicle, f64::from(throttle.min)) {
            return Err(problem);
        }
        // A rover's or a boat's throttle centres: forward one way, reverse
        // the other.
        let centred =
            Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type) == Some(Family::Rover);
        let mut writes = Vec::new();
        let mut write = |name: String, to: f64| {
            if let Some(from) = self.param_value(&vehicle.id, &name)
                && !same_value(from, to)
            {
                writes.push(RadioWrite {
                    name,
                    from,
                    to,
                    state: WriteState::Writing,
                });
            }
        };
        for c in &moved {
            let n = c.channel;
            let trim = if n == throttle.channel && !centred {
                c.min
            } else {
                c.centre.clamp(c.min, c.max)
            };
            write(format!("RC{n}_MIN"), f64::from(c.min));
            write(format!("RC{n}_MAX"), f64::from(c.max));
            write(format!("RC{n}_TRIM"), f64::from(trim));
        }
        for fix in fixes {
            write(fix.name.clone(), fix.to);
        }
        Ok(writes)
    }

    /// With the throttle failsafe on, a throttle whose lowest is not above
    /// its value would trigger it at low throttle: `FS_THR_*` on Copter
    /// and Rover, `THR_FAILSAFE`/`THR_FS_VALUE` on Plane.
    fn failsafe_problem(&self, vehicle: &Vehicle, lowest: f64) -> Option<String> {
        // PX4's `RC_FAILS_THR`: a pulse width, 0 for none.
        if super::px4::px4(vehicle) {
            let failsafe = self.param_value(&vehicle.id, "RC_FAILS_THR")?;
            return (failsafe > 0.0 && lowest <= failsafe + FAILSAFE_MARGIN_US).then(|| {
                format!(
                    "The throttle's lowest, {lowest} µs, must be more than {FAILSAFE_MARGIN_US}                      µs above {}'s throttle failsafe, {failsafe} µs (RC_FAILS_THR), or the                      failsafe would trigger at low throttle. Raise the throttle's low end on the                      transmitter, or lower RC_FAILS_THR in Parameters.",
                    vehicle.name
                )
            });
        }
        let [enable, value] = [
            ["FS_THR_ENABLE", "FS_THR_VALUE"],
            ["THR_FAILSAFE", "THR_FS_VALUE"],
        ]
        .into_iter()
        .find(|[enable, _]| self.param_value(&vehicle.id, enable).is_some())?;
        let on = self.param_value(&vehicle.id, enable)? != 0.0;
        let failsafe = self.param_value(&vehicle.id, value)?;
        (on && lowest <= failsafe + FAILSAFE_MARGIN_US).then(|| {
            format!(
                "The throttle's lowest, {lowest} µs, must be more than {FAILSAFE_MARGIN_US} µs \
                 above {}'s throttle failsafe, {failsafe} µs ({value}), or the failsafe would \
                 trigger at low throttle. Raise the throttle's low end on the transmitter, or \
                 lower {value} in Parameters.",
                vehicle.name
            )
        })
    }

    /// Writes the calibration through the parameter path, as the person
    /// who confirmed it, with one reason. With nothing to change, it is
    /// done at once.
    fn write_radio(
        &mut self,
        mut setup: Setup,
        vehicle: &Vehicle,
        connection: ConnectionId,
    ) -> Setup {
        let actor = self.actor(connection);
        let author = self.author(connection);
        let writes = setup
            .radio
            .as_ref()
            .map(|r| r.writes.clone())
            .unwrap_or_default();
        if writes.is_empty() {
            self.end_radio(&mut setup, vehicle, Ok(()));
            return self.save_setup(setup, true, actor);
        }
        let saved = self.save_setup(setup, true, actor.clone());
        for write in writes {
            let kind = self
                .state
                .parameters
                .get(&parameter_id(&vehicle.id, &write.name))
                .map_or(uxduck_core::domain::parameter::ParamKind::Real32, |p| {
                    p.kind
                });
            let order_id = self.write_parameter_for(
                actor.clone(),
                author.clone(),
                REASON,
                (&vehicle.id, &write.name, write.to),
                kind,
            );
            self.setup
                .radio_writes
                .insert(order_id, (vehicle.id.clone(), write.name));
        }
        saved
    }

    /// One of the calibration's values was written, or not (S187). Once
    /// every one has an answer, the calibration ends.
    pub(crate) fn radio_written(&mut self, order_id: &str, taken: bool) {
        let Some((vehicle_id, name)) = self.setup.radio_writes.remove(order_id) else {
            return;
        };
        let Some(mut setup) = self.current_setup(&vehicle_id).filter(|s| {
            s.kind == SetupKind::Radio
                && s.step == SetupStep::Running
                && s.radio
                    .as_ref()
                    .is_some_and(|r| r.step == RadioStep::Writing)
        }) else {
            return;
        };
        let Some(vehicle) = self.vehicle(&vehicle_id).cloned() else {
            return;
        };
        let Some(radio) = setup.radio.as_mut() else {
            return;
        };
        for write in radio.writes.iter_mut().filter(|w| w.name == name) {
            write.state = if taken {
                WriteState::Written
            } else {
                WriteState::Refused
            };
        }
        let refused: Vec<String> = radio
            .writes
            .iter()
            .filter(|w| w.state == WriteState::Refused)
            .map(|w| w.name.clone())
            .collect();
        let waiting = radio.writes.iter().any(|w| w.state == WriteState::Writing);
        setup.updated_at_us = self.clock.now_us();
        if !waiting {
            let result = if refused.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{} did not take {}. Try again, or set {} in Parameters.",
                    vehicle.name,
                    names(&refused),
                    if refused.len() == 1 { "it" } else { "them" }
                ))
            };
            self.end_radio(&mut setup, &vehicle, result);
        }
        self.save_setup(setup, true, actor());
    }

    /// Ends the calibration: written, or why not. Its channels go back to
    /// their usual rate; a written one goes in the history (V13), and one
    /// that changed a stick's channel needs a restart, as `ArduPilot` reads
    /// `RCMAP_*` only as it starts.
    fn end_radio(&mut self, setup: &mut Setup, vehicle: &Vehicle, result: Result<(), String>) {
        let now = self.clock.now_us();
        self.setup.radio_from.remove(&vehicle.id);
        self.setup.radio_held.remove(&vehicle.id);
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        setup.progress = 100;
        let px4 = super::px4::px4(vehicle);
        if self.not_writable(&vehicle.link_id).is_none() {
            let receipt_id = format!("rc{}", &self.random.token()[..16]);
            self.send_setup_command(vehicle, actor(), receipt_id, stream(false), Order::Rate);
            // PX4 uses its radio again.
            if px4 {
                let receipt_id = format!("rc{}", &self.random.token()[..16]);
                self.send_setup_command(
                    vehicle,
                    actor(),
                    receipt_id,
                    super::px4::stop(),
                    Order::Rate,
                );
            }
        }
        let radio = setup.radio.clone().unwrap_or_default();
        let wrote = radio.writes.iter().any(|w| w.state == WriteState::Written);
        match result {
            Ok(()) => {
                setup.step = SetupStep::Succeeded;
                setup.detail = None;
                // PX4 reads its map as it changes.
                setup.restart_needed = !px4
                    && radio
                        .writes
                        .iter()
                        .any(|w| mapping(&w.name) && w.state == WriteState::Written);
                setup.instruction = setup.restart_needed.then_some(Instruction::Restart);
            }
            Err(why) => {
                setup.step = SetupStep::Failed;
                setup.instruction = Some(Instruction::TryAgain);
                setup.detail = Some(why);
            }
        }
        // Only what reached the vehicle is history: a calibration stopped
        // before writing changed nothing.
        if wrote || setup.step == SetupStep::Succeeded {
            let succeeded = setup.step == SetupStep::Succeeded;
            let text = radio_words(&radio, succeeded);
            let event = HistoryEvent::Calibrated {
                kind: SetupKind::Radio,
                succeeded,
                text,
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        }
        if setup.step == SetupStep::Succeeded {
            self.forget_prearm(&vehicle.id, SetupRow::Radio);
        }
    }

    /// The radio's row (S187): running, needing a restart for a new stick
    /// mapping, or as the vehicle says, else calibrated when UXDuck
    /// calibrated it.
    pub(super) fn radio_row(
        &self,
        vehicle: &Vehicle,
        procedure: Option<&Setup>,
        says: &dyn Fn(SetupRow, Fix) -> Option<String>,
        ardupilot: bool,
    ) -> SetupRowState {
        let calibrated_at_us = self.calibrated_at(vehicle, SetupRow::Radio);
        let radio = procedure.filter(|p| p.kind == SetupKind::Radio);
        let (state, detail) = if radio.is_some_and(|p| p.step.running()) {
            (RowState::Running, None)
        } else if radio.is_some_and(|p| p.restart_needed) {
            (RowState::NeedsRestart, None)
        } else if let Some(reason) = says(SetupRow::Radio, Fix::Calibrate) {
            (RowState::NotCalibrated, Some(reason))
        } else if radio.is_some_and(|p| p.step == SetupStep::Succeeded)
            || calibrated_at_us.is_some()
        {
            (RowState::Calibrated, None)
        } else {
            (RowState::Unknown, None)
        };
        SetupRowState {
            row: SetupRow::Radio.into(),
            state,
            calibrated_at_us,
            detail,
            procedure: ardupilot.then_some(SetupKind::Radio.into()),
        }
    }
}

/// "A", "A and B", "A, B and C".
fn names(all: &[String]) -> String {
    match all {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The calibration in words, for the history: "Radio calibrated: roll
/// channel 1 1004–1996, …; channel 5 1000–2000; `RC1_REVERSED` 0 → 1".
fn radio_words(radio: &Radio, succeeded: bool) -> String {
    let written = |name: &str| {
        radio
            .writes
            .iter()
            .find(|w| w.name == name && w.state == WriteState::Written)
    };
    let mut parts: Vec<String> = radio
        .channels
        .iter()
        .filter(|c| c.span() >= SPAN_US)
        .map(|c| {
            let found = radio.sticks.iter().find(|f| f.channel == c.channel);
            let reversed = written(&format!("RC{}_REVERSED", c.channel))
                .is_some_and(|w| w.to != 0.0)
                || written(&format!("RC{}_REV", c.channel)).is_some_and(|w| w.to < 0.0);
            range_words(c, found.map(|f| f.stick), reversed)
        })
        .collect();
    parts.extend(
        radio
            .writes
            .iter()
            .filter(|w| w.state == WriteState::Written && (mapping(&w.name) || reversal(&w.name)))
            .map(|w| format!("{} {} → {}", w.name, w.from, w.to)),
    );
    if succeeded {
        format!("Radio calibrated: {}", parts.join("; "))
    } else {
        let refused: Vec<String> = radio
            .writes
            .iter()
            .filter(|w| w.state == WriteState::Refused)
            .map(|w| w.name.clone())
            .collect();
        format!(
            "Radio calibration partly written: {} not taken; {}",
            names(&refused),
            parts.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(step: RadioStep, channels: &[(u8, u16, u16, u16)]) -> Radio {
        Radio {
            step,
            channels: channels
                .iter()
                .map(|&(channel, min, max, centre)| RadioChannel {
                    channel,
                    value: centre,
                    min,
                    max,
                    centre,
                })
                .collect(),
            ..Radio::new()
        }
    }

    #[test]
    fn centred_readings_are_centres_and_moved_ones_widen_the_range() {
        let mut r = Radio::new();
        take(&mut r, &[(1, 1500), (3, 1000)]);
        take(&mut r, &[(1, 1510)]);
        assert_eq!(r.channels[0].centre, 1510);
        assert_eq!((r.channels[0].min, r.channels[0].max), (1510, 1510));
        r.step = RadioStep::Ends;
        take(&mut r, &[(1, 1004), (3, 2000)]);
        take(&mut r, &[(1, 1996), (3, 1000)]);
        assert_eq!((r.channels[0].min, r.channels[0].max), (1004, 1996));
        assert_eq!(r.channels[0].centre, 1510);
        assert_eq!(r.channels[1].span(), 1000);
        // At review, readings no longer widen it.
        r.step = RadioStep::Review;
        take(&mut r, &[(1, 900)]);
        assert_eq!(r.channels[0].min, 1004);
        assert_eq!(r.channels[0].value, 900);
    }

    #[test]
    fn a_stick_is_one_channel_moved_away_from_rest() {
        let mut r = radio(
            RadioStep::Sticks,
            &[
                (1, 1000, 2000, 1500),
                (2, 1000, 2000, 1500),
                (3, 1000, 2000, 1000),
            ],
        );
        r.asked = Some(Stick::Roll);
        let from = readings_of(&r);
        // A throttle left down is not roll; a small wobble is nothing.
        r.channels[1].value = 1600;
        assert_eq!(found(&r, &from), Seen::Nothing);
        r.channels[1].value = 1500;
        // Rolled right on a reversed channel 1: it went down.
        r.channels[0].value = 1150;
        assert_eq!(
            found(&r, &from),
            Seen::Found(StickFound {
                stick: Stick::Roll,
                channel: 1,
                lowers: true
            })
        );
        // Two channels moving at once are not one stick.
        r.channels[1].value = 1100;
        assert_eq!(found(&r, &from), Seen::Several);
        r.channels[1].value = 1500;
        // A channel already found is never found again.
        r.sticks.push(StickFound {
            stick: Stick::Roll,
            channel: 1,
            lowers: true,
        });
        r.asked = Some(Stick::Pitch);
        assert_eq!(found(&r, &from), Seen::Nothing);
    }

    #[test]
    fn out_of_range_readings_are_named() {
        let c = RadioChannel {
            channel: 3,
            value: 1500,
            min: 700,
            max: 2000,
            centre: 1500,
        };
        assert!(range_problem(&c).is_some_and(|p| p.starts_with("Channel 3 read 700")));
        let ok = RadioChannel { min: 1000, ..c };
        assert_eq!(range_problem(&ok), None);
    }

    #[test]
    fn history_words_name_each_stick_and_fix() {
        let mut r = radio(
            RadioStep::Writing,
            &[
                (1, 1004, 1996, 1500),
                (3, 1000, 2000, 1000),
                (6, 1500, 1520, 1500),
            ],
        );
        r.sticks = vec![StickFound {
            stick: Stick::Roll,
            channel: 1,
            lowers: true,
        }];
        r.writes = vec![RadioWrite {
            name: "RC1_REVERSED".into(),
            from: 0.0,
            to: 1.0,
            state: WriteState::Written,
        }];
        assert_eq!(
            radio_words(&r, true),
            "Radio calibrated: roll channel 1 1004–1996 (reversed); channel 3 1000–2000; \
             RC1_REVERSED 0 → 1"
        );
    }
}

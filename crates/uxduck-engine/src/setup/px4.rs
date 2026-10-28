//! PX4's driver (S191, ADR-0037 V11), behind the same `setup` entity and
//! rows as `ArduPilot`'s. Checked against PX4 1.17.0 SITL (SIH).
//!
//! PX4 calibrates through `MAV_CMD_PREFLIGHT_CALIBRATION` (241): the
//! compasses with parameter 2 at 1, the accelerometers with 5 at 1, level
//! with 5 at 2, the airspeed sensor with 6 at 2, and the radio with 4 at 1
//! (PX4 stops using its radio until 241 with every parameter 0 ends it).
//! It answers at once, before it calibrates, and says everything else in
//! `[cal]` status text (`uxduck_mavlink::px4_cal`): which sides it still
//! wants, the side it found down, each side done, its progress, and done,
//! failed or cancelled. It finds which side is down itself, so nothing is
//! confirmed: the row shows the sides done and the one it measures. 241
//! with every parameter 0 cancels a calibration running (PX4 answers it
//! twice: accepted by the calibration, refused by the commander), and PX4
//! says "calibration cancelled". PX4 uses a calibration at once: none needs
//! a restart.
//!
//! From a known heading, PX4's "mag quick calibration" is the same
//! `MAV_CMD_FIXED_MAG_CAL_YAW` as `ArduPilot`'s, answered at once and then
//! "Assuming vehicle is facing heading 90.0 degrees" and "Mag quick
//! calibration finished". PX4 1.17 SITL takes any heading; versions before
//! 1.17 are taken as facing true north only, as ADR-0037 V6 says, and a
//! heading other than 0 is refused for them.
//!
//! The motor test is `MAV_CMD_ACTUATOR_TEST` (310): parameter 1 the value
//! from 0 (the motor's lowest thrust) to 1, parameter 2 the time in seconds
//! (PX4 holds it at most 3 s), parameter 5 `ACTUATOR_OUTPUT_FUNCTION`'s
//! motor n (n; PX4's own output function 100 + n is 1000 more). A time of 0
//! stops it at once. PX4 refuses it (`DENIED`) while armed or with
//! `COM_MOT_TEST_EN` 0. The motors are drawn from the vehicle's control
//! allocation: `CA_ROTOR_COUNT`, each rotor's `CA_ROTORn_PX`/`_PY` (metres
//! forward and right) and `CA_ROTORn_KM` (positive spins counter-clockwise
//! seen from above), PX4's own parameters, read at run time.

use uxduck_contracts::dto::setup::CompassInfo;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::frames::{MotorLayout, MotorPlace, Spin};
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::parameter::TransferStatus;
use uxduck_core::domain::setup::{
    Instruction, Setup, SetupKind, SetupRow, SetupStep, Sides, VehiclePosition, is_px4,
};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_mavlink::px4_cal::{CalText, QuickCal, Side, parse, parse_quick};

use super::{MAV_CMD_PREFLIGHT_CALIBRATION, actor, heading_words};
use crate::Engine;

/// `MAV_CMD_ACTUATOR_TEST`.
pub(super) const MAV_CMD_ACTUATOR_TEST: u16 = 310;
/// What PX4 says just after a calibration ended (its failure's kind after
/// its reason, "calibration cancelled") belongs to it this long.
const AFTER_US: u64 = 5_000_000;
/// A PX4 calibration UXDuck hears nothing of for this long has stopped:
/// PX4 says something every few seconds while one runs.
const SILENT_US: u64 = 90_000_000;
/// The first PX4 taken to calibrate from any heading, not only true north.
const ANY_HEADING_FROM: (u32, u32) = (1, 17);
/// The first PX4 with a calibration from a known heading (ADR-0037 V6).
const HEADING_FROM: (u32, u32) = (1, 15);

/// A PX4 vehicle.
pub(super) fn px4(vehicle: &Vehicle) -> bool {
    is_px4(&vehicle.autopilot)
}

/// 241 with every parameter 0: ends a calibration, or the radio's.
pub(super) fn stop() -> Command {
    Command::Long {
        command: MAV_CMD_PREFLIGHT_CALIBRATION,
        params: vec![0.0; 7],
    }
}

/// The command that starts a procedure on PX4, where it differs from
/// `ArduPilot`'s: the compasses, the airspeed and the radio.
pub(super) fn start_command(kind: SetupKind) -> Option<Command> {
    let calibrate = |index: usize, value: f32| {
        let mut params = vec![0.0; 7];
        params[index] = value;
        Command::Long {
            command: MAV_CMD_PREFLIGHT_CALIBRATION,
            params,
        }
    };
    match kind {
        SetupKind::Compass => Some(calibrate(1, 1.0)),
        SetupKind::Radio => Some(calibrate(3, 1.0)),
        SetupKind::AirspeedZero => Some(calibrate(5, 2.0)),
        _ => None,
    }
}

/// PX4's motor test: motor `order` at a percent of its range for `seconds`;
/// 0 s stops it.
pub(super) fn actuator_test(order: u8, throttle: f32, seconds: f32) -> Command {
    let value = if seconds > 0.0 { throttle / 100.0 } else { 0.0 };
    Command::Long {
        command: MAV_CMD_ACTUATOR_TEST,
        params: vec![value, seconds, 0.0, 0.0, f32::from(order), 0.0, 0.0],
    }
}

/// "1.17.0" as (1, 17); none for anything else.
fn version(text: Option<&str>) -> Option<(u32, u32)> {
    let mut parts = text?.trim().trim_start_matches('v').split(['.', '-']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Why this PX4 vehicle cannot calibrate from this heading, or none.
pub(super) fn heading_refused(vehicle: &Vehicle, degrees: f64) -> Option<String> {
    let found = version(vehicle.firmware.as_deref());
    let named = vehicle.firmware.as_deref().unwrap_or("unknown");
    if found.is_some_and(|v| v < HEADING_FROM) {
        return Some(format!(
            "{}'s PX4 ({named}) has no calibration from a known heading: it came with PX4 1.15.",
            vehicle.name
        ));
    }
    let any = found.is_some_and(|v| v >= ANY_HEADING_FROM);
    let north = degrees.rem_euclid(360.0);
    if !any && north.min(360.0 - north) > 0.05 {
        return Some(format!(
            "{}'s PX4 ({named}) calibrates from true north only: turn it to face true north \
             and give 0°.",
            vehicle.name
        ));
    }
    None
}

/// A PX4 side as the position it is drawn in.
fn position(side: Side) -> Option<VehiclePosition> {
    VehiclePosition::from_px4(side.word())
}

/// Every side, in the positions' order, for a start; boxed as `Setup`
/// keeps it, so the journal's change stays small.
#[allow(clippy::unnecessary_box_returns, reason = "kept boxed in Setup")]
fn all_sides() -> Box<Sides> {
    Box::new(Sides {
        needed: VehiclePosition::ALL.to_vec(),
        done: Vec::new(),
    })
}

/// The kind a `[cal] calibration started: 2 <kind>` is, where UXDuck
/// follows it.
fn kind_of(word: &str) -> Option<SetupKind> {
    match word {
        "mag" => Some(SetupKind::Compass),
        "accel" => Some(SetupKind::Accelerometer),
        "level" => Some(SetupKind::Level),
        "airspeed" => Some(SetupKind::AirspeedZero),
        _ => None,
    }
}

/// The kinds that run by `[cal]` text.
const fn by_text(kind: SetupKind) -> bool {
    matches!(
        kind,
        SetupKind::Compass | SetupKind::Accelerometer | SetupKind::Level | SetupKind::AirspeedZero
    )
}

/// What PX4's failure means, in a sentence.
fn failure_words(name: &str, kind: SetupKind, reason: &str) -> String {
    let lower = reason.trim().to_lowercase();
    match lower.as_str() {
        "timeout: no motion" => format!(
            "{name} waited too long for it to be turned onto another side. Turn it onto the next \
             side sooner, and try again."
        ),
        "reading sensor" => format!("{name} could not read the sensor."),
        "to set parameters" | "to apply calibration" => {
            format!("{name} could not save the calibration.")
        }
        "mag" | "accel" | "level" | "airspeed" => match kind {
            SetupKind::AirspeedZero => format!(
                "{name}'s airspeed calibration failed: it read no usable pressure from its \
                 sensor, or none rising when the pitot was blown into."
            ),
            _ => format!("{name}'s {} failed.", kind.words().to_lowercase()),
        },
        _ => format!("{name}: {}.", reason.trim().trim_end_matches('.')),
    }
}

impl Engine {
    /// A parameter of the vehicle's, as last read.
    fn px4_param(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&uxduck_core::domain::parameter::parameter_id(
                vehicle_id, name,
            ))
            .map(|p| p.value)
    }

    /// PX4 answered a procedure's start: it calibrates now, saying how in
    /// status text; or why not.
    pub(super) fn px4_answered(&mut self, mut setup: Setup, receipt: &Receipt) {
        let now = self.clock.now_us();
        let name = self.vehicle_name(&setup.vehicle_id);
        if setup.step != SetupStep::Starting {
            return;
        }
        if receipt.status == ReceiptStatus::Accepted {
            setup.step = SetupStep::Running;
            setup.instruction = Some(match setup.kind {
                SetupKind::Compass | SetupKind::Accelerometer => {
                    setup.sides.get_or_insert_with(all_sides);
                    Instruction::HoldOnASide
                }
                SetupKind::AirspeedZero => Instruction::CoverPitot,
                _ => Instruction::HoldStill,
            });
            setup.updated_at_us = now;
            self.save_setup(setup, true, actor());
            return;
        }
        let what = match setup.kind {
            SetupKind::Compass => "calibrate its compasses",
            SetupKind::CompassHeading => "calibrate its compasses from a heading",
            SetupKind::Accelerometer => "calibrate its accelerometers",
            SetupKind::Level => "take this as level",
            SetupKind::AirspeedZero => "calibrate its airspeed sensor",
            _ => "start it",
        };
        let why = match (receipt.status, receipt.detail.as_deref()) {
            (ReceiptStatus::Rejected, Some("Unsupported")) => {
                format!("{name} can't {what}: its firmware does not support it.")
            }
            (ReceiptStatus::Rejected, _) => {
                format!("{name} refused to {what} now. Disarm it, keep it still, and try again.")
            }
            (ReceiptStatus::Timeout, _) => format!("{name} did not answer."),
            (_, Some(why)) => format!("It was not sent to {name}: {why}."),
            _ => format!("{name} did not {what}."),
        };
        setup.step = SetupStep::Failed;
        setup.instruction =
            (receipt.detail.as_deref() != Some("Unsupported")).then_some(Instruction::TryAgain);
        setup.detail = Some(why.clone());
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        // A refusal by the vehicle is a failed calibration, as for
        // ArduPilot's one-command calibrations.
        if receipt.status == ReceiptStatus::Rejected
            && matches!(
                setup.kind,
                SetupKind::CompassHeading | SetupKind::Level | SetupKind::AirspeedZero
            )
        {
            self.px4_history(
                &setup,
                false,
                format!("{} failed: {why}", setup.kind.words()),
            );
        }
        self.save_setup(setup, true, actor());
    }

    fn px4_history(&mut self, setup: &Setup, succeeded: bool, text: String) {
        let event = HistoryEvent::Calibrated {
            kind: setup.kind,
            succeeded,
            text,
        };
        match setup.started_by.clone() {
            Some(author) => self.record_by(&setup.vehicle_id, event, author),
            None => self.record(&setup.vehicle_id, event),
        }
    }

    /// What a PX4 vehicle said, if it is about a calibration: `Some(true)`
    /// when it belongs to the procedure and needs no notice of its own,
    /// `None` when it is not PX4's calibration text at all.
    pub(super) fn px4_said(&mut self, vehicle_id: &str, text: &str) -> Option<bool> {
        let now = self.clock.now_us();
        let current = self.current_setup(vehicle_id);
        let lately = |s: &Setup| {
            s.step.running()
                || s.finished_at_us
                    .is_some_and(|at| now.saturating_sub(at) < AFTER_US)
        };
        if let Some(quick) = parse_quick(text) {
            let setup = current.filter(|s| s.kind == SetupKind::CompassHeading && lately(s))?;
            if setup.step.running() {
                self.px4_quick(setup, quick);
            }
            return Some(true);
        }
        let Some(said) = parse(text) else {
            // The radio's own words while it runs.
            let lower = text.trim().to_lowercase();
            let radio = lower.starts_with("calibration: disabling rc input")
                || lower.starts_with("calibration: restoring rc input");
            return (radio && current.is_some_and(|s| s.kind == SetupKind::Radio && lately(&s)))
                .then_some(true);
        };
        if let CalText::Started { kind, .. } = &said {
            let kind = kind_of(kind)?;
            return Some(self.px4_started(vehicle_id, kind, current));
        }
        let setup = current.filter(|s| by_text(s.kind) && lately(s))?;
        if setup.step.running() {
            self.px4_moved(setup, said);
        }
        Some(true)
    }

    /// PX4 said a calibration started: the one UXDuck started, or one it
    /// finds running, started elsewhere (another ground station, its
    /// console).
    fn px4_started(&mut self, vehicle_id: &str, kind: SetupKind, current: Option<Setup>) -> bool {
        let now = self.clock.now_us();
        let mut setup = match current {
            Some(s) if s.step.running() && s.kind == kind => s,
            Some(s) if s.step.running() => return false,
            _ => Setup {
                vehicle_id: vehicle_id.to_owned(),
                kind,
                step: SetupStep::Running,
                progress: 0,
                sensors: Vec::new(),
                instruction: None,
                detail: None,
                restart_needed: false,
                heading: None,
                position: None,
                placed: false,
                sides: None,
                radio: None,
                motor: None,
                firmware: None,
                airspeed: (kind == SetupKind::AirspeedZero).then(Box::default),
                started_by: None,
                receipt_id: None,
                started_at_us: now,
                updated_at_us: now,
                finished_at_us: None,
                version: 0,
            },
        };
        setup.step = SetupStep::Running;
        setup.progress = 0;
        setup.instruction = Some(match kind {
            SetupKind::Compass | SetupKind::Accelerometer => {
                setup.sides = Some(all_sides());
                Instruction::HoldOnASide
            }
            SetupKind::AirspeedZero => Instruction::CoverPitot,
            _ => Instruction::HoldStill,
        });
        setup.updated_at_us = now;
        self.save_setup(setup, true, actor());
        true
    }

    /// A `[cal]` line moved a PX4 calibration on.
    #[allow(clippy::too_many_lines, reason = "one arm per line PX4 says")]
    fn px4_moved(&mut self, mut setup: Setup, said: CalText) {
        let now = self.clock.now_us();
        let name = self.vehicle_name(&setup.vehicle_id);
        let mut at_once = false;
        setup.updated_at_us = now;
        if setup.step == SetupStep::Starting {
            setup.step = SetupStep::Running;
            at_once = true;
        }
        match said {
            CalText::Progress(progress) | CalText::SideProgress { progress, .. } => {
                setup.progress = progress.max(setup.progress);
            }
            CalText::Pending(pending) => {
                let sides = setup.sides.get_or_insert_with(all_sides);
                let mut needed: Vec<VehiclePosition> = pending
                    .into_iter()
                    .filter_map(position)
                    .chain(sides.done.iter().copied())
                    .collect();
                needed.sort();
                needed.dedup();
                if sides.needed != needed {
                    sides.needed = needed;
                    at_once = true;
                }
                if setup.position.take().is_some() {
                    at_once = true;
                }
                setup.instruction = Some(Instruction::HoldOnASide);
            }
            CalText::HoldOnASide => {
                if setup.instruction != Some(Instruction::HoldOnASide) {
                    at_once = true;
                }
                setup.instruction = Some(Instruction::HoldOnASide);
            }
            CalText::Moving => {
                setup.instruction = Some(Instruction::HoldStill);
            }
            CalText::Detected(side) | CalText::Measuring(side) => {
                let found = position(side);
                if setup.position != found {
                    at_once = true;
                }
                setup.position = found;
                setup.detail = None;
                if setup.instruction != Some(Instruction::TurnOnThisSide) {
                    setup.instruction = Some(Instruction::HoldStill);
                }
            }
            CalText::Rotate => {
                setup.instruction = Some(Instruction::TurnOnThisSide);
                at_once = true;
            }
            CalText::SideDone(side) => {
                if let Some(done) = position(side) {
                    let sides = setup.sides.get_or_insert_with(all_sides);
                    if !sides.done.contains(&done) {
                        sides.done.push(done);
                        sides.done.sort();
                    }
                }
                setup.position = None;
                setup.detail = None;
                setup.instruction = Some(Instruction::HoldOnASide);
                at_once = true;
            }
            CalText::AlreadyDone(side) => {
                if let Some(done) = position(side) {
                    setup.detail = Some(format!(
                        "It is {} again, which is done: turn it onto another side.",
                        done.words()
                    ));
                }
                setup.position = None;
                setup.instruction = Some(Instruction::HoldOnASide);
                at_once = true;
            }
            CalText::Done(_) => {
                self.px4_finished(setup, true, None);
                return;
            }
            CalText::Failed(reason) => {
                let why = failure_words(&name, setup.kind, &reason);
                self.px4_finished(setup, false, Some(why));
                return;
            }
            CalText::Cancelled => {
                setup.step = SetupStep::Cancelled;
                setup.instruction = None;
                setup.position = None;
                setup.detail = Some(format!("{name} cancelled it."));
                setup.finished_at_us = Some(now);
                at_once = true;
            }
            CalText::Warning(warning) => {
                setup.detail = Some(format!("{name} warns: {warning}."));
                at_once = true;
            }
            CalText::CoverPitot => {
                setup.instruction = Some(Instruction::CoverPitot);
                at_once = true;
            }
            CalText::Offset(pa) => {
                self.setup
                    .px4_airspeed
                    .entry(setup.vehicle_id.clone())
                    .or_default()
                    .0 = Some(pa);
                setup.progress = setup.progress.max(50);
                at_once = true;
            }
            CalText::BlowIntoPitot => {
                setup.instruction = Some(Instruction::BlowIntoPitot);
                setup.detail = None;
                at_once = true;
            }
            CalText::MorePressure(got) => {
                setup.instruction = Some(Instruction::BlowIntoPitot);
                setup.detail = Some(format!(
                    "Blow a little harder: it reads {got} Pa of the 50 it needs."
                ));
                at_once = true;
            }
            CalText::PressureOk(pa) => {
                self.setup
                    .px4_airspeed
                    .entry(setup.vehicle_id.clone())
                    .or_default()
                    .1 = Some(pa);
                setup.detail = None;
                at_once = true;
            }
            CalText::PressureNegative(pa) => {
                setup.detail = Some(format!(
                    "Blowing into it lowered the pressure ({pa} Pa): its tubes look swapped."
                ));
                setup.instruction = Some(Instruction::CheckPitot);
                at_once = true;
            }
            CalText::SwapTubes => {
                setup.instruction = Some(Instruction::CheckPitot);
            }
            CalText::Started { .. } | CalText::AtRest | CalText::Other(_) => {}
        }
        self.save_setup(setup, at_once, actor());
    }

    /// A PX4 calibration ended, well or not; in the history either way.
    fn px4_finished(&mut self, mut setup: Setup, succeeded: bool, why: Option<String>) {
        let now = self.clock.now_us();
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        setup.position = None;
        let blown = self.setup.px4_airspeed.remove(&setup.vehicle_id);
        let sides = setup.sides.as_ref().map_or(0, |s| s.needed.len());
        let text = if succeeded {
            setup.step = SetupStep::Succeeded;
            setup.progress = 100;
            setup.instruction = None;
            setup.detail = None;
            if let Some(all) = setup.sides.as_mut() {
                all.done.clone_from(&all.needed);
            }
            match setup.kind {
                SetupKind::Compass => format!("Compasses calibrated on {sides} sides"),
                SetupKind::Accelerometer => format!("Accelerometers calibrated on {sides} sides"),
                SetupKind::Level => "Level set".to_owned(),
                _ => match blown {
                    Some((Some(offset), Some(blowing))) => format!(
                        "Airspeed sensor zeroed and its pitot checked (offset {offset} Pa, \
                         {blowing} Pa blown into)"
                    ),
                    _ => "Airspeed sensor zeroed and its pitot checked".to_owned(),
                },
            }
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(if setup.kind == SetupKind::AirspeedZero {
                Instruction::CheckPitot
            } else if setup.kind == SetupKind::Level {
                Instruction::SetLevel
            } else {
                Instruction::TryAgain
            });
            let why = why.unwrap_or_else(|| "It failed.".to_owned());
            setup.detail = Some(why.clone());
            let done = setup.sides.as_ref().map_or(0, |s| s.done.len());
            match setup.kind {
                SetupKind::Compass | SetupKind::Accelerometer => format!(
                    "{} failed after {done} of {sides} sides: {why}",
                    setup.kind.words()
                ),
                _ => format!("{} failed: {why}", setup.kind.words()),
            }
        };
        self.px4_history(&setup, succeeded, text);
        let vehicle_id = setup.vehicle_id.clone();
        let row = setup.kind.row();
        self.save_setup(setup, true, actor());
        if succeeded {
            if matches!(row, SetupRow::Compass | SetupRow::Accelerometer) {
                self.forget_prearm(&vehicle_id, row);
            }
            self.download_parameters(&vehicle_id);
        }
    }

    /// PX4's calibration from a known heading said how it went.
    fn px4_quick(&mut self, mut setup: Setup, quick: QuickCal) {
        let now = self.clock.now_us();
        let name = self.vehicle_name(&setup.vehicle_id);
        let words = setup
            .heading
            .as_ref()
            .map(heading_words)
            .unwrap_or_default();
        let (succeeded, text) = match quick {
            QuickCal::Facing(_) => return,
            QuickCal::Finished => {
                setup.step = SetupStep::Succeeded;
                setup.instruction = None;
                setup.detail = None;
                (true, format!("Compasses calibrated from {words}"))
            }
            QuickCal::Failed(said) => {
                setup.step = SetupStep::Failed;
                let why = if said.to_lowercase().starts_with("gps required") {
                    setup.instruction = Some(Instruction::GivePosition);
                    format!("{name} has no GPS position to calibrate from.")
                } else {
                    setup.instruction = Some(Instruction::TryAgain);
                    format!(
                        "{name} could not calibrate: {}.",
                        said.trim_end_matches('.')
                    )
                };
                setup.detail = Some(why.clone());
                (
                    false,
                    format!("Compass calibration from {words} failed: {why}"),
                )
            }
        };
        setup.progress = 100;
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        self.px4_history(&setup, succeeded, text);
        let vehicle_id = setup.vehicle_id.clone();
        self.save_setup(setup, true, actor());
        if succeeded {
            self.forget_prearm(&vehicle_id, SetupRow::Compass);
            self.download_parameters(&vehicle_id);
        }
    }

    /// Once a second: a PX4 calibration UXDuck has heard nothing of for a
    /// long time ended unseen (its last words lost, or the vehicle gone).
    pub fn expire_setups(&mut self, now: u64) {
        let silent: Vec<Setup> = self
            .state
            .setups
            .values()
            .filter(|s| {
                (by_text(s.kind) || s.kind == SetupKind::CompassHeading)
                    && s.step.running()
                    && self.vehicle(&s.vehicle_id).is_some_and(px4)
            })
            .filter_map(|s| self.row_setup(&s.vehicle_id, s.kind.row()))
            .filter(|s| now.saturating_sub(s.updated_at_us) >= SILENT_US)
            .collect();
        for mut setup in silent {
            let name = self.vehicle_name(&setup.vehicle_id);
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            setup.position = None;
            setup.detail = Some(format!(
                "UXDuck stopped hearing how it went from {name}. Try again."
            ));
            setup.updated_at_us = now;
            setup.finished_at_us = Some(now);
            self.save_setup(setup, true, actor());
        }
    }

    /// A PX4 vehicle's compasses, from `CAL_MAGn_ID` (listed when not 0),
    /// `CAL_MAGn_PRIO` (0: not used), `CAL_MAGn_ROT` (-1: internal) and its
    /// offsets in gauss, given here in milligauss.
    pub(super) fn px4_compasses(&self, vehicle_id: &str) -> Vec<CompassInfo> {
        let value = |name: String| self.px4_param(vehicle_id, &name);
        (0..4u8)
            .filter_map(|index| {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a device id is a 32-bit integer parameter"
                )]
                let device_id = value(format!("CAL_MAG{index}_ID")).filter(|id| *id > 0.0)? as u32;
                let offsets = ["X", "Y", "Z"]
                    .iter()
                    .map(|axis| value(format!("CAL_MAG{index}_{axis}OFF")).map(|g| g * 1000.0))
                    .collect::<Option<Vec<f64>>>()
                    .and_then(|all| <[f64; 3]>::try_from(all).ok());
                Some(CompassInfo {
                    index,
                    device_id,
                    used: value(format!("CAL_MAG{index}_PRIO")).is_none_or(|p| p != 0.0),
                    external: value(format!("CAL_MAG{index}_ROT")).map(|r| r >= 0.0),
                    offsets,
                })
            })
            .collect()
    }

    /// Whether a PX4 vehicle's first accelerometer is calibrated: its
    /// `CAL_ACC0_ID` is set, as PX4 checks. None before it is read.
    pub(super) fn px4_accel_calibrated(&self, vehicle_id: &str) -> Option<bool> {
        self.px4_param(vehicle_id, "CAL_ACC0_ID")
            .map(|id| id != 0.0)
    }

    /// A PX4 vehicle's motors from its control allocation, `None` while its
    /// parameters are not read; a layout with no motors when it lists no
    /// rotors (a PX4 before control allocation, or a frame without them).
    pub(super) fn px4_motors(&self, vehicle: &Vehicle) -> Option<MotorLayout> {
        let param = |name: String| self.px4_param(&vehicle.id, &name);
        let Some(count) = param("CA_ROTOR_COUNT".into()) else {
            if self.parameter_set(&vehicle.id).status == TransferStatus::Synced {
                return Some(MotorLayout {
                    frame: "Motors not listed (no CA_ROTOR_COUNT)".into(),
                    motors: Vec::new(),
                });
            }
            return None;
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a small count of rotors"
        )]
        let count = count.clamp(0.0, 12.0) as u8;
        let mut places: Vec<(u8, f64, f64, Option<Spin>)> = (0..count)
            .map(|i| {
                let forward = param(format!("CA_ROTOR{i}_PX")).unwrap_or(0.0);
                let right = param(format!("CA_ROTOR{i}_PY")).unwrap_or(0.0);
                let spin = param(format!("CA_ROTOR{i}_KM")).and_then(|km| {
                    if km > 0.0 {
                        Some(Spin::CounterClockwise)
                    } else if km < 0.0 {
                        Some(Spin::Clockwise)
                    } else {
                        None
                    }
                });
                (i + 1, right, forward, spin)
            })
            .collect();
        let farthest = places
            .iter()
            .map(|(_, r, f, _)| r.hypot(*f))
            .fold(0.0, f64::max);
        if farthest > 0.0 {
            for place in &mut places {
                place.1 /= farthest;
                place.2 /= farthest;
            }
        }
        let motors = places
            .iter()
            .map(|(n, right, forward, spin)| MotorPlace {
                order: *n,
                label: n.to_string(),
                motor: *n,
                right: *right,
                forward: *forward,
                spin: *spin,
                stack: u8::try_from(
                    places
                        .iter()
                        .filter(|(m, r, f, _)| {
                            m < n && (r - right).abs() < 1e-3 && (f - forward).abs() < 1e-3
                        })
                        .count(),
                )
                .unwrap_or(0),
            })
            .collect();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "CA_AIRFRAME is a small integer parameter"
        )]
        let airframe = match param("CA_AIRFRAME".into()).map(|a| a.round() as i64) {
            Some(0) => "Multicopter",
            Some(1) => "Fixed wing",
            Some(2) => "Standard VTOL",
            Some(3) => "Tiltrotor VTOL",
            Some(4) => "Tailsitter VTOL",
            _ => "PX4 frame",
        };
        Some(MotorLayout {
            frame: format!(
                "{airframe}, {count} {}",
                if count == 1 { "motor" } else { "motors" }
            ),
            motors,
        })
    }

    /// Why a PX4 vehicle refused its motor test, with its parameter.
    pub(super) fn px4_motor_refusal(&self, vehicle_id: &str, receipt: &Receipt) -> String {
        let name = self.vehicle_name(vehicle_id);
        match receipt.status {
            ReceiptStatus::Rejected
                if self
                    .px4_param(vehicle_id, "COM_MOT_TEST_EN")
                    .is_some_and(|on| on == 0.0) =>
            {
                format!(
                    "{name} has its motor test turned off (COM_MOT_TEST_EN is 0). Set it to 1 \
                     in Parameters, then try again."
                )
            }
            ReceiptStatus::Rejected => format!(
                "{name} refused the motor test. Check it is disarmed, its safety switch is off, \
                 and its motor test is on (COM_MOT_TEST_EN)."
            ),
            ReceiptStatus::Timeout => format!("{name} did not answer."),
            _ => format!(
                "It was not sent to {name}: {}.",
                receipt.detail.as_deref().unwrap_or("not sent")
            ),
        }
    }

    /// A PX4 plane's airspeed sensor: present when `SYS_HAS_NUM_ASPD` asks
    /// for one or `ASPD_PRIMARY` names one.
    pub(super) fn px4_airspeed_present(&self, vehicle_id: &str) -> Option<bool> {
        let wanted = self.px4_param(vehicle_id, "SYS_HAS_NUM_ASPD");
        let primary = self.px4_param(vehicle_id, "ASPD_PRIMARY");
        match (wanted, primary) {
            (None, None) => None,
            _ => Some(wanted.is_some_and(|n| n > 0.0) || primary.is_some_and(|p| p >= 1.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_motor_tests() {
        assert_eq!(version(Some("1.17.0")), Some((1, 17)));
        assert_eq!(version(Some("v1.15.4-beta1")), Some((1, 15)));
        assert_eq!(version(None), None);
        assert_eq!(
            actuator_test(2, 5.0, 1.0),
            Command::Long {
                command: 310,
                params: vec![0.05, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0],
            }
        );
        assert_eq!(actuator_test(2, 5.0, 1.0).label(), "Spin motor 2 at 5%");
        assert_eq!(actuator_test(2, 0.0, 0.0).label(), "Stop the motor test");
        assert_eq!(stop().label(), "Stop the calibration");
        assert_eq!(
            start_command(SetupKind::Compass).map(|c| c.label()),
            Some("Calibrate the compasses".into())
        );
        assert_eq!(
            start_command(SetupKind::AirspeedZero).map(|c| c.label()),
            Some("Calibrate the airspeed sensor".into())
        );
        assert_eq!(
            start_command(SetupKind::Radio).map(|c| c.label()),
            Some("Start the radio calibration".into())
        );
    }
}

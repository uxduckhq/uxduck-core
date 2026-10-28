//! The motor test (S188, ADR-0037 V9): one motor spins while a person holds
//! its press, and stops when they let go. `ArduPilot`'s
//! `MAV_CMD_DO_MOTOR_TEST` spins a motor for the time given and then stops
//! by itself; UXDuck gives it 1 s and renews it every 0.5 s while the press
//! is held, so a lost window, link or key stops it at the vehicle.
//!
//! The window drives the hold, UXDuck the renewing: `setup/spin` with
//! `held` starts a motor and, sent again every 0.25 s by the window,
//! keeps it held. Each one that comes 0.45 s or more after the last command
//! renews the vehicle's test, so renewals go every 0.5 s while the window
//! holds and stop when it does. Let go (`held` false, or `setup/cancel`),
//! the hold's keepalives missing for 0.6 s (found once a second), or the
//! window's connection closed, the motor is stopped: the renewing ends and
//! UXDuck sends a test of 0% for 0 s, which ends the vehicle's test at once
//! (checked against `ArduCopter` SITL: "finished motor test" and its output
//! back at its lowest straight away).
//!
//! A start and a stop have journaled receipts; renewals do not, and the data
//! plane sends each motor test once, a newer one replacing one still
//! waiting, so a renewal can never spin a motor after it was let go. A new
//! test while a stop waits for its answer is sent once it is answered: a
//! test that switches motors while one runs leaves the first at its last
//! output on `ArduCopter` until the test ends (seen in SITL).
//!
//! `ArduPilot` arms the vehicle for its motor test and disarms it after: that
//! arming is not a flight, and does not stop the next motor being tested.
//! The vehicle's refusals come as a failed `COMMAND_ACK` and its "Motor
//! Test: Safety switch", which becomes the row's words. No history event
//! (V13). Only a person at an operator's window or the CLI may spin one,
//! with `control`; never from away (the relay's gate refuses every setup
//! method) and never while someone away has the vehicle's lease.
//!
//! PX4 (S191) tests a motor with `MAV_CMD_ACTUATOR_TEST` instead, renewed and
//! stopped the same way (see `px4`); its motors are drawn from its control
//! allocation's parameters, numbered as PX4 numbers them.

use serde_json::Value;
use uxduck_contracts::dto::setup::{Setup as SetupDto, SetupSpinParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::frames::{self, MotorLayout};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::setup::{MotorTest, Setup, SetupKind, SetupStep};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Actor;

use super::{Order, actor, no_vehicle};
use crate::commands::Outcome;
use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;
use crate::{ConnectionId, Engine};

/// `MAV_CMD_DO_MOTOR_TEST`.
const MAV_CMD_DO_MOTOR_TEST: u16 = 209;
/// The vehicle's own time for each test, seconds.
const TEST_S: f32 = 1.0;
/// A hold renews the test when this long has passed since the last
/// command: with keepalives every 0.25 s, every 0.5 s.
const RENEW_AFTER_US: u64 = 450_000;
/// A hold whose keepalives stop for this long is let go.
const KEPT_US: u64 = 600_000;
/// A motor let go is stopped at the vehicle when a command went this
/// recently, so its test may still run; after, it has ended by itself.
const STOP_WITHIN_US: u64 = 1_200_000;
/// A new test waits this long at most for a stop's answer.
const STOP_WAIT_US: u64 = 1_000_000;
/// The vehicle armed or disarmed this soon after a test ended: its own
/// arming for the test.
const ARMED_FOR_TEST_US: u64 = 3_000_000;
/// The throttle a test may ask for, percent.
const MAX_THROTTLE: f32 = 30.0;

/// A motor held: whose hold, which motor, and when it was last kept and
/// renewed.
#[derive(Clone, Debug)]
pub(crate) struct Spin {
    connection: ConnectionId,
    /// Who holds it, for a start sent once a stop is answered.
    actor: Actor,
    order: u8,
    throttle: f32,
    kept_us: u64,
    /// The last command sent; none while it waits for a stop's answer.
    sent_us: Option<u64>,
    /// The vehicle took its start.
    accepted: bool,
}

/// A vehicle's motors, for one UXDuck tests them on.
pub(super) enum Motors {
    /// Its parameters are not read yet.
    Unread,
    Drawn(MotorLayout),
}

impl Motors {
    pub(super) fn drawn(self) -> Option<MotorLayout> {
        match self {
            Self::Drawn(layout) => Some(layout),
            Self::Unread => None,
        }
    }
}

/// How a hold ended.
enum Ended {
    Released,
    Lapsed,
    Closed,
    Failed(String),
}

/// The motor test's command: one motor in the test's order, at a percent
/// of its range, for 1 s, in sequence order. 0% for 0 s stops it.
#[cfg(test)]
fn test_command(order: u8, throttle: f32) -> Command {
    test_for(false, order, throttle)
}

/// The motor test's command for the vehicle's autopilot: PX4's
/// `MAV_CMD_ACTUATOR_TEST`, else `ArduPilot`'s.
fn test_for(px4: bool, order: u8, throttle: f32) -> Command {
    if px4 {
        let seconds = if throttle > 0.0 { TEST_S } else { 0.0 };
        return super::px4::actuator_test(order, throttle, seconds);
    }
    let seconds = if throttle > 0.0 { TEST_S } else { 0.0 };
    Command::Long {
        command: MAV_CMD_DO_MOTOR_TEST,
        params: vec![f32::from(order), 0.0, throttle, seconds, 0.0, 1.0],
    }
}

/// `setup/spin`: a motor pressed, kept held, or let go.
pub(super) fn spin(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SetupSpinParams,
    operator: bool,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Only a person at UXDuck, in its window or its CLI, can spin a motor.",
        ));
    }
    if !params.held {
        if engine.setup.spins.contains_key(&vehicle.id) {
            engine.end_spin(&vehicle.id, Ended::Released);
        }
        return engine.current_setup(&vehicle.id).map_or_else(
            || {
                Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no motor is spinning on {}", vehicle.name),
                ))
            },
            |s| to_value(&SetupDto::from(&s)),
        );
    }
    if let Some(held) = engine.setup.spins.get(&vehicle.id).cloned() {
        let label = engine.motor_label(&vehicle, held.order);
        if held.connection != connection {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!(
                    "Motor {label} is spinning on {} from elsewhere.",
                    vehicle.name
                ),
            ));
        }
        if held.order != params.motor {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("Let go of motor {label} first."),
            ));
        }
        engine.keep_spin(&vehicle);
        return engine.current_setup(&vehicle.id).map_or_else(
            || Err(no_vehicle(&vehicle.id)),
            |s| to_value(&SetupDto::from(&s)),
        );
    }
    // A keepalive for a hold that ended starts nothing.
    if params.keep {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!(
                "Motor {} is no longer spinning on {}.",
                engine.motor_label(&vehicle, params.motor),
                vehicle.name
            ),
        ));
    }
    press(engine, connection, &vehicle, params)
}

/// A motor pressed: checked, then started, or started once the last
/// motor's stop is answered.
#[allow(
    clippy::too_many_lines,
    reason = "each check before a motor spins, in order"
)]
fn press(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle: &Vehicle,
    params: &SetupSpinParams,
) -> Result<Value, ApiError> {
    // Nothing spins until the person has said the propellers are off
    // (maintainer, 2026-10-27): a switch on the row, not a line.
    if !params.propellers_off {
        return Err(ApiError::new(
            ErrorKind::ConfirmationRequired,
            format!(
                "Confirm {}'s propellers are off before testing a motor.",
                vehicle.name
            ),
        ));
    }
    let throttle = params.throttle;
    if !(throttle.is_finite() && throttle > 0.0 && throttle <= MAX_THROTTLE) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("A motor test's throttle is more than 0% and at most {MAX_THROTTLE}%."),
        ));
    }
    let layout = match engine.motors_of(vehicle) {
        Some(Motors::Drawn(layout)) => layout,
        Some(Motors::Unread) => {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "UXDuck has not read {}'s frame yet. Refresh its parameters, then try again.",
                    vehicle.name
                ),
            ));
        }
        None => {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("UXDuck has no motor test for {}.", vehicle.name),
            ));
        }
    };
    let Some(place) = layout.motors.iter().find(|m| m.order == params.motor) else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{} has no motor {} ({}).",
                vehicle.name,
                frames::letter(params.motor),
                layout.frame
            ),
        ));
    };
    if let Some(why) = engine.cannot_spin(vehicle) {
        return Err(why);
    }
    let now = engine.clock.now_us();
    let receipt_id = format!("rc{}", &engine.random.token()[..16]);
    let setup = Setup {
        vehicle_id: vehicle.id.clone(),
        kind: SetupKind::Motors,
        step: SetupStep::Starting,
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
        motor: Some(MotorTest {
            order: place.order,
            label: place.label.clone(),
            throttle,
        }),
        firmware: None,
        airspeed: None,
        started_by: Some(engine.author(connection)),
        receipt_id: Some(receipt_id.clone()),
        started_at_us: now,
        updated_at_us: now,
        finished_at_us: None,
        version: 0,
    };
    let actor = engine.actor(connection);
    let saved = engine.save_setup(setup, true, actor.clone());
    // A stop still on its way is answered first.
    let stopping = engine
        .setup
        .stopping
        .get(&vehicle.id)
        .is_some_and(|at| now.saturating_sub(*at) < STOP_WAIT_US);
    if !stopping {
        engine.send_setup_command(
            vehicle,
            actor.clone(),
            receipt_id,
            test_for(super::px4::px4(vehicle), place.order, throttle),
            Order::Start,
        );
    }
    engine.setup.spins.insert(
        vehicle.id.clone(),
        Spin {
            connection,
            actor,
            order: place.order,
            throttle,
            kept_us: now,
            sent_us: (!stopping).then_some(now),
            accepted: false,
        },
    );
    to_value(&SetupDto::from(&saved))
}

impl Engine {
    fn param_of(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
    }

    /// The vehicle's motors as the motor test draws them: none when it has
    /// none UXDuck tests (a plane without VTOL motors, a submarine, PX4
    /// until S191).
    pub(super) fn motors_of(&self, vehicle: &Vehicle) -> Option<Motors> {
        // PX4's from its control allocation (S191).
        if super::px4::px4(vehicle) {
            return Some(
                self.px4_motors(vehicle)
                    .map_or(Motors::Unread, Motors::Drawn),
            );
        }
        if vehicle.autopilot != "Ardupilotmega" {
            return None;
        }
        let param = |name: &str| self.param_of(&vehicle.id, name);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "frame class and type are small integer parameters"
        )]
        let frame = |class: Option<f64>, frame_type: Option<f64>| {
            class.map_or(Motors::Unread, |class| {
                let class = class.clamp(0.0, 255.0) as u8;
                Motors::Drawn(if class == 0 {
                    MotorLayout {
                        frame: "Frame class not set".into(),
                        motors: Vec::new(),
                    }
                } else {
                    frames::multicopter(class, frame_type.unwrap_or(0.0).clamp(0.0, 255.0) as u8)
                })
            })
        };
        match Family::of(&vehicle.vehicle_type)? {
            Family::Copter => Some(frame(param("FRAME_CLASS"), param("FRAME_TYPE"))),
            // A VTOL's motors: a quadplane (Q_ENABLE), with the same frames.
            Family::Plane => (param("Q_ENABLE")? >= 1.0)
                .then(|| frame(param("Q_FRAME_CLASS"), param("Q_FRAME_TYPE"))),
            Family::Rover => Some(param("SERVO1_FUNCTION").map_or(Motors::Unread, |_| {
                let skid = (1..=16).any(|n| {
                    param(&format!("SERVO{n}_FUNCTION"))
                        .is_some_and(|f| frames::SKID_STEER_FUNCTIONS.contains(&f))
                });
                Motors::Drawn(frames::rover(skid))
            })),
            Family::Sub | Family::Blimp | Family::Px4 => None,
        }
    }

    /// A motor's name for words: "A", or a rover's "Throttle".
    fn motor_label(&self, vehicle: &Vehicle, order: u8) -> String {
        self.motors_of(vehicle)
            .and_then(Motors::drawn)
            .and_then(|l| l.motors.into_iter().find(|m| m.order == order))
            .map_or_else(|| frames::letter(order), |m| m.label)
    }

    /// Why no motor can spin on this vehicle now, beyond its frame.
    fn cannot_spin(&self, vehicle: &Vehicle) -> Option<ApiError> {
        let invalid = |why: String| ApiError::new(ErrorKind::InvalidValue, why);
        let busy = |why: String| ApiError::new(ErrorKind::Busy, why);
        // Armed for a motor test that just ended is not armed by its pilot.
        let mut checked = vehicle.clone();
        if self.setup.test_armed.contains(&vehicle.id) {
            checked.armed = false;
        }
        if let Some(why) = self.cannot_set_up(&checked) {
            return Some(invalid(why));
        }
        if let Some(running) = self
            .running_setup(&vehicle.id)
            .filter(|s| s.kind != SetupKind::Motors)
        {
            return Some(busy(format!(
                "{} is running on {}. Cancel it first.",
                running.kind.words(),
                vehicle.name
            )));
        }
        if let Some(controller) = self.state.controllers.values().find(|c| {
            c.claim.is_some()
                && c.target
                    .as_deref()
                    .is_some_and(|t| t == vehicle.id || vehicle.aliases.iter().any(|a| a == t))
        }) {
            return Some(busy(format!(
                "{} is driving {}. Release it first.",
                controller.name, vehicle.name
            )));
        }
        if let Some(lease) = self.lease_of(&vehicle.id) {
            return Some(busy(format!(
                "{} has control of {} from away. Take it back first.",
                lease.name, vehicle.name
            )));
        }
        None
    }

    /// The window still holds the motor: renewed when it is due, and let
    /// go when the vehicle can no longer be told.
    fn keep_spin(&mut self, vehicle: &Vehicle) {
        let now = self.clock.now_us();
        let Some(mut held) = self.setup.spins.get(&vehicle.id).cloned() else {
            return;
        };
        held.kept_us = now;
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            self.end_spin(
                &vehicle.id,
                Ended::Failed(format!(
                    "Stopped: UXDuck can't tell {}: {why}.",
                    vehicle.name
                )),
            );
            return;
        }
        match held.sent_us {
            // Waiting for a stop that never answered: started now.
            None if self
                .setup
                .stopping
                .get(&vehicle.id)
                .is_none_or(|at| now.saturating_sub(*at) >= STOP_WAIT_US) =>
            {
                self.setup.stopping.remove(&vehicle.id);
                self.setup.spins.insert(vehicle.id.clone(), held);
                self.send_waiting_start(&vehicle.id);
                return;
            }
            Some(sent) if held.accepted && now.saturating_sub(sent) >= RENEW_AFTER_US => {
                held.sent_us = Some(now);
                let receipt_id = format!("rn{}", &self.random.token()[..16]);
                self.setup
                    .renewals
                    .insert(receipt_id.clone(), vehicle.id.clone());
                // No receipt in the journal: twice a second while held.
                self.links.command(CommandOrder {
                    receipt_id,
                    vehicle_id: vehicle.id.clone(),
                    command: test_for(super::px4::px4(vehicle), held.order, held.throttle),
                    force: false,
                });
            }
            _ => {}
        }
        self.setup.spins.insert(vehicle.id.clone(), held);
    }

    /// A hold waiting for a stop's answer: its start sent now, with the
    /// receipt its procedure carries.
    fn send_waiting_start(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let Some(setup) = self
            .current_setup(&vehicle.id)
            .filter(|s| s.kind == SetupKind::Motors && s.step == SetupStep::Starting)
        else {
            return;
        };
        let (Some(receipt_id), Some(held)) = (
            setup.receipt_id.clone(),
            self.setup.spins.get_mut(&vehicle.id),
        ) else {
            return;
        };
        if held.sent_us.is_some() {
            return;
        }
        held.sent_us = Some(self.clock.now_us());
        let command = test_for(super::px4::px4(&vehicle), held.order, held.throttle);
        let actor = held.actor.clone();
        self.send_setup_command(&vehicle, actor, receipt_id, command, Order::Start);
    }

    /// The hold ended: the motor is stopped at the vehicle if its test may
    /// still run, and the procedure says how it ended.
    fn end_spin(&mut self, vehicle_id: &str, ended: Ended) {
        let Some(held) = self.setup.spins.remove(vehicle_id) else {
            return;
        };
        let now = self.clock.now_us();
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        if held
            .sent_us
            .is_some_and(|sent| now.saturating_sub(sent) < STOP_WITHIN_US)
            && self.not_writable(&vehicle.link_id).is_none()
        {
            let receipt_id = format!("rc{}", &self.random.token()[..16]);
            self.send_setup_command(
                &vehicle,
                actor(),
                receipt_id,
                test_for(super::px4::px4(&vehicle), held.order, 0.0),
                Order::Stop,
            );
            self.setup.stopping.insert(vehicle.id.clone(), now);
        }
        let Some(mut setup) = self
            .current_setup(&vehicle.id)
            .filter(|s| s.kind == SetupKind::Motors && s.step.running())
        else {
            return;
        };
        let (step, detail) = match ended {
            Ended::Released => (SetupStep::Succeeded, None),
            Ended::Lapsed => (
                SetupStep::Cancelled,
                Some("Stopped: the press stopped reaching UXDuck.".to_owned()),
            ),
            Ended::Closed => (
                SetupStep::Cancelled,
                Some("Stopped: the window holding it closed.".to_owned()),
            ),
            Ended::Failed(why) => (SetupStep::Failed, Some(why)),
        };
        setup.step = step;
        setup.detail = detail;
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        self.save_setup(setup, true, actor());
    }

    /// The vehicle answered a motor test's start: spinning, or why not.
    /// Answered after it was let go, a refusal still says why.
    pub(super) fn motor_started(&mut self, mut setup: Setup, receipt: &Receipt) {
        let vehicle_id = setup.vehicle_id.clone();
        let accepted = receipt.status == ReceiptStatus::Accepted;
        if accepted {
            if let Some(held) = self.setup.spins.get_mut(&vehicle_id) {
                held.accepted = true;
                setup.step = SetupStep::Running;
                setup.updated_at_us = self.clock.now_us();
                self.save_setup(setup, true, actor());
            }
            return;
        }
        let name = self.vehicle_name(&vehicle_id);
        let said = self
            .setup
            .motor_said
            .remove(&vehicle_id)
            .filter(|(_, at)| self.clock.now_us().saturating_sub(*at) < super::SAID_US)
            .map(|(text, _)| text);
        let why = if self.vehicle(&vehicle_id).is_some_and(super::px4::px4) {
            self.px4_motor_refusal(&vehicle_id, receipt)
        } else {
            refusal(&name, receipt, said.as_deref())
        };
        if let Some(held) = self.setup.spins.get_mut(&vehicle_id) {
            // Refused or never sent, it never ran: there is nothing to
            // stop. Unanswered, it may have.
            if receipt.status != ReceiptStatus::Timeout {
                held.sent_us = None;
            }
            self.end_spin(&vehicle_id, Ended::Failed(why));
            return;
        }
        // Let go before the answer: it stopped, and says why it never ran.
        let now = self.clock.now_us();
        setup.step = SetupStep::Failed;
        setup.detail = Some(why);
        setup.updated_at_us = now;
        setup.finished_at_us.get_or_insert(now);
        self.save_setup(setup, true, actor());
    }

    /// The vehicle answered a stop: a hold waiting for it starts.
    pub(super) fn motor_stopped(&mut self, vehicle_id: &str) {
        self.setup.stopping.remove(vehicle_id);
        if self
            .setup
            .spins
            .get(vehicle_id)
            .is_some_and(|s| s.sent_us.is_none())
        {
            self.send_waiting_start(vehicle_id);
        }
    }

    /// A renewal ended (it has no receipt in the journal): refused by the
    /// vehicle, the hold ends with its words. True when it was one.
    pub(crate) fn renewal_done(&mut self, receipt_id: &str, outcome: &Outcome) -> bool {
        let Some(vehicle_id) = self.setup.renewals.remove(receipt_id) else {
            return false;
        };
        if let Outcome::Rejected(why) = outcome
            && self.setup.spins.contains_key(&vehicle_id)
        {
            let name = self.vehicle_name(&vehicle_id);
            let said = self
                .setup
                .motor_said
                .remove(&vehicle_id)
                .map(|(text, _)| text);
            let words = said.map_or_else(
                || format!("Stopped: {name} refused to go on ({why})."),
                |said| format!("Stopped: {name} said {said}."),
            );
            self.end_spin(&vehicle_id, Ended::Failed(words));
        }
        true
    }

    /// Once a second: holds whose keepalives stopped are let go, and a
    /// motor test left running by a UXDuck that stopped ends.
    pub fn expire_spins(&mut self, now: u64) {
        let lapsed: Vec<String> = self
            .setup
            .spins
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.kept_us) >= KEPT_US)
            .map(|(id, _)| id.clone())
            .collect();
        for vehicle_id in lapsed {
            self.end_spin(&vehicle_id, Ended::Lapsed);
        }
        self.setup
            .stopping
            .retain(|_, at| now.saturating_sub(*at) < 10 * STOP_WAIT_US);
        let orphaned: Vec<Setup> = self
            .state
            .setups
            .values()
            .filter(|s| {
                s.kind == SetupKind::Motors
                    && s.step.running()
                    && !self.setup.spins.contains_key(&s.vehicle_id)
            })
            .cloned()
            .collect();
        for mut setup in orphaned {
            setup.step = SetupStep::Cancelled;
            setup.detail = Some("Stopped: UXDuck restarted during it.".into());
            setup.updated_at_us = now;
            setup.finished_at_us = Some(now);
            self.save_setup(setup, true, actor());
        }
    }

    /// A connection closed: the motor it held stops.
    pub(crate) fn spins_disconnected(&mut self, connection: ConnectionId) {
        let held: Vec<String> = self
            .setup
            .spins
            .iter()
            .filter(|(_, s)| s.connection == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for vehicle_id in held {
            self.end_spin(&vehicle_id, Ended::Closed);
        }
    }

    /// The vehicle armed or disarmed: true when it is its own arming for a
    /// motor test, which is not a flight and no reason to refuse the next
    /// motor.
    pub(crate) fn armed_for_motor_test(&mut self, vehicle_id: &str, armed: bool) -> bool {
        if !armed {
            return self.setup.test_armed.remove(vehicle_id);
        }
        let now = self.clock.now_us();
        let testing = self.setup.spins.contains_key(vehicle_id)
            || self
                .setup
                .stopping
                .get(vehicle_id)
                .is_some_and(|at| now.saturating_sub(*at) < ARMED_FOR_TEST_US)
            || self.current_setup(vehicle_id).is_some_and(|s| {
                s.kind == SetupKind::Motors
                    && s.finished_at_us
                        .is_none_or(|at| now.saturating_sub(at) < ARMED_FOR_TEST_US)
            });
        if testing {
            self.setup.test_armed.insert(vehicle_id.to_owned());
        }
        testing
    }

    /// The vehicle is armed by itself for a motor test (S188).
    pub(crate) fn armed_by_motor_test(&self, vehicle_id: &str) -> bool {
        self.setup.test_armed.contains(vehicle_id)
    }

    /// What a vehicle says of its motor test: "starting motor test",
    /// "finished motor test", and why it refused ("Motor Test: Safety
    /// switch"), which becomes the row's words. True when it is the motor
    /// test's own and needs no notice.
    pub(super) fn motors_said(&mut self, setup: &mut Setup, text: &str) -> bool {
        let now = self.clock.now_us();
        let lower = text.trim().to_lowercase();
        let lately = setup.step.running()
            || setup
                .finished_at_us
                .is_some_and(|at| now.saturating_sub(at) < ARMED_FOR_TEST_US);
        let ours = lower.contains("motor test");
        if !lately || !ours {
            return false;
        }
        let reason = text
            .trim()
            .strip_prefix("Motor Test:")
            .map(str::trim)
            .filter(|_| !lower.starts_with("motor test: cancelled"))
            .map(str::to_owned)
            .or_else(|| (lower == "must be disarmed for motor test").then(|| "it is armed".into()));
        let Some(reason) = reason else {
            return true;
        };
        if setup.step == SetupStep::Failed {
            // Said just after its refusal: its words replace UXDuck's.
            let name = self.vehicle_name(&setup.vehicle_id);
            setup.detail = Some(format!("{name} refused the motor test: {}.", said(&reason)));
            setup.updated_at_us = now;
            self.save_setup(setup.clone(), true, actor());
        } else {
            self.setup
                .motor_said
                .insert(setup.vehicle_id.clone(), (reason, now));
        }
        true
    }
}

/// A vehicle's reason in a sentence: "Safety switch" → "the safety switch
/// is on".
fn said(reason: &str) -> String {
    let trimmed = reason.trim().trim_end_matches('.');
    match trimmed.to_lowercase().as_str() {
        "safety switch" => "its safety switch is on; press it first".into(),
        "rc not calibrated" => "its radio is not calibrated".into(),
        "vehicle not landed" => "it is not landed".into(),
        "board initialising" => "it is still starting up".into(),
        "motor emergency stopped" => "its emergency stop is on".into(),
        _ => trimmed.to_owned(),
    }
}

/// What a vehicle's refusal of a motor test means, with what it said.
fn refusal(name: &str, receipt: &Receipt, said_why: Option<&str>) -> String {
    match (receipt.status, said_why) {
        (ReceiptStatus::Rejected, Some(why)) => {
            format!("{name} refused the motor test: {}.", said(why))
        }
        (ReceiptStatus::Rejected, None) => match receipt.detail.as_deref() {
            Some("Unsupported") => format!("{name} has no motor test."),
            _ => format!(
                "{name} refused the motor test. Check it is disarmed, its safety switch is \
                 pressed and its throttle is down."
            ),
        },
        (ReceiptStatus::Timeout, _) => format!("{name} did not answer."),
        (_, _) => format!(
            "It was not sent to {name}: {}.",
            receipt.detail.as_deref().unwrap_or("not sent")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_lasts_a_second_and_a_stop_none() {
        assert_eq!(
            test_command(2, 5.0),
            Command::Long {
                command: 209,
                params: vec![2.0, 0.0, 5.0, 1.0, 0.0, 1.0],
            }
        );
        assert_eq!(
            test_command(2, 0.0),
            Command::Long {
                command: 209,
                params: vec![2.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            }
        );
        assert_eq!(test_command(1, 5.0).label(), "Spin motor A at 5%");
        assert_eq!(test_command(1, 0.0).label(), "Stop the motor test");
    }

    #[test]
    fn the_vehicles_reasons_in_words() {
        assert_eq!(
            said("Safety switch"),
            "its safety switch is on; press it first"
        );
        assert_eq!(said("RC not calibrated"), "its radio is not calibrated");
        assert_eq!(said("Throttle too high."), "Throttle too high");
    }
}

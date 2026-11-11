//! ESC calibration, all at once (S216, ADR-0037 V17), as `ArduPilot`'s
//! documentation describes it: with the propellers off and the vehicle on
//! its battery, `ESC_CALIBRATION` is set to 3, the battery is unplugged and
//! plugged in, the ESCs beep as they learn the throttle range, and the
//! vehicle sets the parameter back to 0 when it is done. UXDuck writes the
//! 3, waits for the vehicle to restart, and ends the procedure when the
//! parameters downloaded after the restart say 0.
//!
//! For copters with PWM or `OneShot` ESCs (`MOT_PWM_TYPE` 0 to 3): `DShot`
//! needs no calibration, and planes, rovers and boats calibrate their ESCs
//! by hand. The vehicle keeps no record of a calibration, so the row's
//! state is UXDuck's own history. A calibration not heard back within two
//! minutes says so; cancelling it sets `ESC_CALIBRATION` back to 0, at once
//! or once the vehicle is heard again, so a half-done calibration does not
//! wait for its next start.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::setup::{RowState, Setup as SetupDto, SetupRowState};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::history::HistoryEvent;
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::setup::{Instruction, Setup, SetupKind, SetupRow, SetupStep};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Actor;

use super::actor;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

const PARAM: &str = "ESC_CALIBRATION";
/// "Auto calibration": the vehicle calibrates every ESC at its next start.
const AUTO: f64 = 3.0;
/// Not heard back within this long after the write, the row says so.
const HEARD_WITHIN_US: u64 = 120_000_000;

/// What the ESC calibration keeps beyond the journal. Lost with UXDuck: a
/// calibration it was running then ends as cancelled when the vehicle is
/// read again.
#[derive(Debug, Default)]
pub(super) struct Escs {
    /// Writes of `ESC_CALIBRATION`, by order id: the vehicle and the value.
    writes: BTreeMap<String, (String, f64)>,
    /// Vehicles whose calibration was written and that restarted since.
    restarted: BTreeSet<String>,
    /// Vehicles to set back to 0 once they are heard again.
    reset: BTreeSet<String>,
}

/// Whether the vehicle's ESCs are calibrated this way: an `ArduPilot`
/// multicopter whose `MOT_PWM_TYPE` is 0 to 3.
pub(super) fn offered(engine: &Engine, vehicle: &Vehicle) -> bool {
    vehicle.autopilot == "Ardupilotmega"
        && vehicle.vehicle_type != "Helicopter"
        && Family::of(&vehicle.vehicle_type) == Some(Family::Copter)
        && engine
            .escs_param(&vehicle.id, "MOT_PWM_TYPE")
            .is_some_and(|t| (0.0..=3.0).contains(&t))
        && engine.escs_param(&vehicle.id, PARAM).is_some()
}

/// `setup/start` for `escs`: the propellers said off, then 3 written.
pub(super) fn start(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle: &Vehicle,
    propellers_off: bool,
) -> Result<Value, ApiError> {
    if !offered(engine, vehicle) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{}'s ESCs aren't calibrated this way: UXDuck calibrates the ESCs of copters \
                 with PWM or OneShot ESCs (MOT_PWM_TYPE 0 to 3).",
                vehicle.name
            ),
        ));
    }
    if !propellers_off {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Take the propellers off first, and say so: the motors spin at full throttle \
             while the ESCs are calibrated.",
        ));
    }
    let now = engine.clock.now_us();
    let setup = Setup {
        vehicle_id: vehicle.id.clone(),
        kind: SetupKind::Escs,
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
        motor: None,
        firmware: None,
        airspeed: None,
        started_by: Some(engine.author(connection)),
        receipt_id: None,
        started_at_us: now,
        updated_at_us: now,
        finished_at_us: None,
        version: 0,
    };
    let actor = engine.actor(connection);
    let saved = engine.save_setup(setup, true, actor.clone());
    engine.setup.escs.restarted.remove(&vehicle.id);
    engine.setup.escs.reset.remove(&vehicle.id);
    engine.write_escs(&vehicle.id, AUTO, actor, connection);
    to_value(&SetupDto::from(&saved))
}

impl Engine {
    fn escs_param(&self, vehicle_id: &str, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
    }

    fn write_escs(&mut self, vehicle_id: &str, value: f64, actor: Actor, connection: ConnectionId) {
        let Some(kind) = self
            .state
            .parameters
            .get(&parameter_id(vehicle_id, PARAM))
            .map(|p| p.kind)
        else {
            return;
        };
        let reason = if value == 0.0 {
            "ESC calibration ended"
        } else {
            "ESC calibration"
        };
        let author = self.author(connection);
        let order =
            self.write_parameter_for(actor, author, reason, (vehicle_id, PARAM, value), kind);
        self.setup
            .escs
            .writes
            .insert(order, (vehicle_id.to_owned(), value));
    }

    /// A write of `ESC_CALIBRATION` ended: 3 taken asks for the battery to
    /// be replugged; refused, the calibration failed.
    pub(crate) fn escs_written(&mut self, order_id: &str, taken: bool) {
        let Some((vehicle_id, value)) = self.setup.escs.writes.remove(order_id) else {
            return;
        };
        if value == 0.0 {
            if !taken {
                self.setup.escs.reset.insert(vehicle_id);
            }
            return;
        }
        let Some(mut setup) = self
            .current_setup(&vehicle_id)
            .filter(|s| s.kind == SetupKind::Escs && s.step.running())
        else {
            return;
        };
        let now = self.clock.now_us();
        setup.updated_at_us = now;
        if taken {
            setup.step = SetupStep::Running;
            setup.instruction = Some(Instruction::ReplugBattery);
        } else {
            let name = self.vehicle_name(&vehicle_id);
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            setup.detail = Some(format!("{name} didn't take ESC_CALIBRATION 3."));
            setup.finished_at_us = Some(now);
        }
        self.save_setup(setup, true, actor());
    }

    /// The vehicle restarted: a calibration waiting for it goes on, and is
    /// ended by the parameters read after it. True when one was waiting.
    pub(super) fn escs_restarted(&mut self, vehicle_id: &str) -> bool {
        let waiting = self
            .row_setup(vehicle_id, SetupRow::Escs)
            .is_some_and(|s| s.step == SetupStep::Running);
        if waiting {
            self.setup.escs.restarted.insert(vehicle_id.to_owned());
        }
        waiting
    }

    /// The vehicle's parameters were read, or one changed: a calibration
    /// that restarted ends when `ESC_CALIBRATION` is 0, and one cancelled
    /// while away is set back to 0.
    pub(crate) fn escs_read(&mut self, vehicle_id: &str) {
        let Some(value) = self.escs_param(vehicle_id, PARAM) else {
            return;
        };
        if value != 0.0
            && self.setup.escs.reset.contains(vehicle_id)
            && self
                .vehicle(vehicle_id)
                .is_some_and(|v| self.not_writable(&v.link_id).is_none())
        {
            self.setup.escs.reset.remove(vehicle_id);
            let author = uxduck_core::domain::history::Author {
                client_name: "UXDuck".into(),
                ..Default::default()
            };
            if let Some(kind) = self
                .state
                .parameters
                .get(&parameter_id(vehicle_id, PARAM))
                .map(|p| p.kind)
            {
                let order = self.write_parameter_for(
                    actor(),
                    author,
                    "ESC calibration cancelled",
                    (vehicle_id, PARAM, 0.0),
                    kind,
                );
                self.setup
                    .escs
                    .writes
                    .insert(order, (vehicle_id.to_owned(), 0.0));
            }
            return;
        }
        if !self.setup.escs.restarted.contains(vehicle_id) {
            return;
        }
        let Some(mut setup) = self
            .row_setup(vehicle_id, SetupRow::Escs)
            .filter(|s| s.step == SetupStep::Running)
        else {
            self.setup.escs.restarted.remove(vehicle_id);
            return;
        };
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        setup.updated_at_us = now;
        if value == 0.0 {
            self.setup.escs.restarted.remove(vehicle_id);
            setup.step = SetupStep::Succeeded;
            setup.instruction = None;
            setup.detail = None;
            setup.progress = 100;
            setup.finished_at_us = Some(now);
            let event = HistoryEvent::Calibrated {
                kind: SetupKind::Escs,
                succeeded: true,
                text: "ESCs calibrated: the vehicle set ESC_CALIBRATION back to 0".into(),
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        } else {
            // Back, but it did not calibrate: on USB power, or replugged
            // too quickly.
            setup.detail = Some(format!(
                "{} is back with ESC_CALIBRATION still {value}: power it from its battery, \
                 then unplug and plug in the battery again.",
                vehicle.name
            ));
        }
        self.save_setup(setup, true, actor());
    }

    /// A calibration not heard back within two minutes says so, and stays
    /// for the vehicle to come back or to be cancelled.
    pub(super) fn expire_escs(&mut self, now: u64) {
        let late: Vec<Setup> = self
            .state
            .setups
            .values()
            .filter(|s| s.kind == SetupKind::Escs && s.step == SetupStep::Running)
            .filter(|s| !self.setup.escs.restarted.contains(&s.vehicle_id))
            .filter(|s| s.detail.is_none())
            .filter(|s| now.saturating_sub(s.started_at_us) >= HEARD_WITHIN_US)
            .cloned()
            .collect();
        for mut setup in late {
            let name = self.vehicle_name(&setup.vehicle_id);
            setup.detail = Some(format!(
                "{name} hasn't restarted in two minutes. Unplug and plug in its battery, or \
                 cancel: UXDuck then sets ESC_CALIBRATION back to 0, now or once {name} is back."
            ));
            setup.updated_at_us = now;
            self.save_setup(setup, true, actor());
        }
    }

    /// Cancelled: `ESC_CALIBRATION` back to 0 now, or once the vehicle is
    /// heard again.
    pub(super) fn cancel_escs(
        &mut self,
        vehicle: &Vehicle,
        actor: Actor,
        connection: ConnectionId,
    ) {
        self.setup.escs.restarted.remove(&vehicle.id);
        if self
            .escs_param(&vehicle.id, PARAM)
            .is_some_and(|v| v != 0.0)
            || self
                .setup
                .escs
                .writes
                .values()
                .any(|(v, _)| *v == vehicle.id)
        {
            if self.not_writable(&vehicle.link_id).is_none()
                && vehicle.presence == uxduck_core::domain::vehicle::Presence::Online
            {
                self.write_escs(&vehicle.id, 0.0, actor, connection);
            } else {
                self.setup.escs.reset.insert(vehicle.id.clone());
            }
        }
    }

    /// The ESCs' row, for a vehicle whose ESCs are calibrated this way.
    pub(super) fn escs_row(
        &self,
        vehicle: &Vehicle,
        last: Option<&Setup>,
    ) -> Option<SetupRowState> {
        if !offered(self, vehicle) {
            return None;
        }
        let calibrated_at_us = self.calibrated_at(vehicle, SetupRow::Escs);
        let state = if last.is_some_and(|s| s.step.running()) {
            RowState::Running
        } else if calibrated_at_us.is_some() {
            RowState::Calibrated
        } else {
            RowState::NotCalibrated
        };
        Some(SetupRowState {
            row: SetupRow::Escs.into(),
            state,
            calibrated_at_us,
            detail: None,
            procedure: Some(SetupKind::Escs.into()),
        })
    }
}

//! A vehicle's gripper (S263): which actuator the operator says it is, its
//! wiring read from the vehicle's parameters, and the on-screen hold, which
//! joins a pilot route this connection already holds (ADR-0005). The data
//! plane merges held actions into that route's `MANUAL_CONTROL`; nothing
//! here starts a stream of its own, because `ArduSub` disarms when pilot
//! input stops (`FS_PILOT_INPUT`).

use serde_json::{Value, json};
use uxduck_contracts::dto::gripper::{GripperHoldParams, GripperStatus, VehicleGripperParams};
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::gripper::{
    self, GripperProblem, GripperSetup, GripperWiring, NEUTRAL_TOLERANCE_US, NEWTON_NEUTRAL_US,
};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::settings::PilotInputMethod;
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Change;

use crate::{ConnectionId, Engine};

fn not_found(what: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("{what} not found"))
}

impl Engine {
    /// A vehicle's parameter by name, as last read.
    fn vehicle_param(&self, vehicle: &Vehicle, name: &str) -> Option<f64> {
        self.state
            .parameters
            .get(&parameter_id(&vehicle.id, name))
            .map(|p| p.value)
    }

    fn has_parameters(&self, vehicle: &Vehicle) -> bool {
        let prefix = format!("{}/", vehicle.id);
        self.state
            .parameters
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(key, _)| key.starts_with(&prefix))
    }

    /// The gripper's wiring now, or why it cannot be used.
    pub(crate) fn gripper_wiring(
        &self,
        vehicle: &Vehicle,
    ) -> Result<GripperWiring, Vec<GripperProblem>> {
        if Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type) != Some(Family::Sub) {
            return Err(vec![GripperProblem::NotSub]);
        }
        let Some(setup) = vehicle.gripper else {
            return Err(Vec::new());
        };
        if !self.has_parameters(vehicle) {
            return Err(vec![GripperProblem::NoParameters]);
        }
        gripper::wiring(&setup, |name| self.vehicle_param(vehicle, name))
    }

    /// What a route to the vehicle carries for its gripper: its wiring when
    /// usable and the route sends `MANUAL_CONTROL`, which alone has buttons.
    pub(crate) fn gripper_route(
        &self,
        vehicle_id: &str,
        method: PilotInputMethod,
    ) -> Option<GripperWiring> {
        let vehicle = self.vehicle(vehicle_id)?;
        if method != PilotInputMethod::ManualControl {
            return None;
        }
        self.gripper_wiring(vehicle).ok()
    }

    /// `gripper/status`.
    pub(crate) fn gripper_status(&self, vehicle: &Vehicle) -> GripperStatus {
        let read = |name: &str| self.vehicle_param(vehicle, name);
        let (wiring, problems) = match self.gripper_wiring(vehicle) {
            Ok(wiring) => (Some(wiring), Vec::new()),
            Err(problems) => (None, problems),
        };
        let mut warnings = Vec::new();
        if let Some(wiring) = &wiring {
            let neutral = wiring.neutral_us();
            if neutral.abs_diff(NEWTON_NEUTRAL_US) > NEUTRAL_TOLERANCE_US {
                warnings.push(format!(
                    "Release returns SERVO{} to {neutral} µs, the middle of SERVO{0}_MIN and SERVO{0}_MAX. Newton stops at {NEWTON_NEUTRAL_US} µs, so it may keep moving after release",
                    wiring.output
                ));
            }
            if wiring.open_button > 15 || wiring.close_button > 15 {
                warnings.push(
                    "A gripper button slot above 15 travels in MANUAL_CONTROL's buttons2, verified with ArduSub 4.7.2. Older firmware may ignore it".into(),
                );
            }
        }
        GripperStatus {
            vehicle_id: vehicle.id.clone(),
            setup: vehicle.gripper.map(Into::into),
            wiring: wiring.as_ref().map(Into::into),
            problems: problems
                .iter()
                .map(|p| p.message(vehicle.gripper.as_ref()))
                .collect(),
            warnings,
            free_outputs: gripper::free_outputs(read),
            free_buttons: gripper::free_buttons(read),
        }
    }

    /// Routes again the controllers driving a vehicle whose gripper wiring
    /// may have changed: its setup, or its parameters. A route whose wiring
    /// is unchanged is left alone, so nothing held is let go for nothing.
    pub(crate) fn gripper_after(&mut self, change: &Change) {
        let vehicle_id = match change {
            Change::VehicleUpserted { vehicle } => vehicle.id.clone(),
            Change::ParameterChanged { parameter } => parameter.vehicle_id.clone(),
            Change::ParametersReplaced { set, .. } => set.vehicle_id.clone(),
            _ => return,
        };
        let driving: Vec<(String, Option<GripperWiring>)> = self
            .live
            .routed_gripper
            .iter()
            .filter(|(id, _)| {
                self.state
                    .controllers
                    .get(*id)
                    .and_then(|c| c.target.as_deref())
                    .and_then(|target| self.vehicle(target))
                    .is_some_and(|v| v.id == vehicle_id)
            })
            .map(|(id, wiring)| (id.clone(), wiring.clone()))
            .collect();
        for (id, before) in driving {
            let method = self.pilot_input_method(&vehicle_id);
            if self.gripper_route(&vehicle_id, method) != before {
                self.route(&id);
            }
        }
    }
}

/// `vehicle/gripper`.
pub(crate) fn set_gripper(
    engine: &mut Engine,
    connection: ConnectionId,
    VehicleGripperParams { id, gripper }: VehicleGripperParams,
) -> Result<Value, ApiError> {
    let setup: Option<GripperSetup> = gripper.map(Into::into);
    if setup.is_some_and(|s| !gripper::ACTUATORS.contains(&s.actuator)) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A gripper is on Actuator 1 to 6",
        ));
    }
    let mut vehicle = engine
        .vehicle(&id)
        .cloned()
        .ok_or_else(|| not_found(&format!("vehicle {id}")))?;
    if setup.is_some()
        && Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type) != Some(Family::Sub)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Gripper control is for ArduSub vehicles",
        ));
    }
    if vehicle.gripper != setup {
        vehicle.gripper = setup;
        vehicle.version += 1;
        engine.commit(
            connection,
            Change::VehicleUpserted {
                vehicle: vehicle.clone(),
            },
        );
    }
    serde_json::to_value(VehicleDto::from(&vehicle))
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// `gripper/status`.
pub(crate) fn status(engine: &Engine, id: &str) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(id)
        .ok_or_else(|| not_found(&format!("vehicle {id}")))?;
    serde_json::to_value(engine.gripper_status(vehicle))
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// `gripper/hold`: joins the route this connection drives the vehicle with,
/// its keyboard first, else a controller it holds. Without such a route, or
/// with a gripper that cannot be used, a hold is refused with what to do; a
/// release is always accepted.
pub(crate) fn hold(
    engine: &mut Engine,
    connection: ConnectionId,
    GripperHoldParams { vehicle_id, action }: GripperHoldParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&vehicle_id)
        .cloned()
        .ok_or_else(|| not_found(&format!("vehicle {vehicle_id}")))?;
    let keyboard = engine.live.keyboards.get(&connection);
    let mut routes: Vec<(String, bool)> = engine
        .live
        .routed_gripper
        .iter()
        .filter(|(id, _)| engine.live.claimant.get(*id) == Some(&connection))
        .filter(|(id, _)| {
            engine
                .state
                .controllers
                .get(*id)
                .and_then(|c| c.target.as_deref())
                .and_then(|t| engine.vehicle(t))
                .is_some_and(|target| target.id == vehicle.id)
        })
        .map(|(id, wiring)| (id.clone(), wiring.is_some()))
        .collect();
    routes.sort_by_key(|(id, _)| Some(id) != keyboard);
    let route = routes.into_iter().next();
    let Some((controller_id, wired)) = route else {
        if action.is_none() {
            return Ok(json!({}));
        }
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "Claim a controller for {} first (or, in a simulation, start keyboard control): the gripper joins the pilot input you send",
                vehicle.name
            ),
        ));
    };
    if !wired {
        if action.is_none() {
            return Ok(json!({}));
        }
        let problems = engine
            .gripper_wiring(&vehicle)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|p| p.message(vehicle.gripper.as_ref()))
            .collect::<Vec<_>>();
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            if problems.is_empty() {
                "Set up the gripper in the vehicle's setup first".to_owned()
            } else {
                problems.join(". ")
            },
        ));
    }
    let at_us = engine.clock.now_us();
    engine
        .links
        .controller_input(crate::controller_input::InputEvent::Gripper {
            id: controller_id,
            action: action.map(Into::into),
            at_us,
        });
    Ok(json!({}))
}

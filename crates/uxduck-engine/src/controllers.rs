//! Controllers (ADR-0005 controllers and multi-vehicle safety). The data
//! plane owns the devices and sends `MANUAL_CONTROL`; the engine decides
//! what each controller may drive and tells the data plane its route. The
//! rules are strict because this is the one path where a stale screen could
//! steer the wrong aircraft:
//!
//! - nothing drives until an operator client claims it, with its sticks
//!   centred;
//! - a controller that follows the selection moves to a newly selected
//!   vehicle only with roll, pitch and yaw centred (throttle is the
//!   operator's) and the switch confirmed;
//!   meanwhile it stays on the old one and says so;
//! - two controllers never drive one vehicle;
//! - when the claiming client goes, or the controller is unplugged, it is
//!   released and sends nothing, so the vehicle's own failsafe decides.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::command::CommandSendParams;
use uxduck_contracts::dto::controller::{
    ClientSelectParams, Controller as ControllerDto, ControllerClaimParams,
    ControllerSetTargetParams, InputMapping as MappingDto, MappingCreateParams,
    MappingUpdateParams,
};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{
    Claim, Controller, GamepadButton, InputMapping, NEUTRAL, PositionBinding, TargetMode,
};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::events::{Actor, Change};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::ControlRoute;
use crate::{ConnectionId, Engine, upsert};

/// How far below the middle a flying multicopter's throttle may be when a
/// controller takes it over: -1 is idle, 0 the middle; -0.3 is 35% input.
const HOLDING_THROTTLE: f64 = 0.3;

/// What the engine knows about controllers that is not journaled.
#[derive(Debug, Default)]
pub(crate) struct Live {
    /// Whether each controller's sticks are centred, as the data plane
    /// last said; unknown counts as not.
    neutral: BTreeMap<String, bool>,
    /// The connection holding each claimed controller.
    claimant: BTreeMap<String, ConnectionId>,
    /// Each connection's selected vehicle.
    selection: BTreeMap<ConnectionId, String>,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
    }
}

/// The built-in mappings, overridden by stored edits.
pub(crate) fn mappings(engine: &Engine) -> Vec<InputMapping> {
    let mut all: BTreeMap<String, InputMapping> = [
        InputMapping::mode_2(),
        InputMapping::mode_1(),
        InputMapping::radio(),
        InputMapping::radio_taer(),
        InputMapping::expresslrs_bluetooth(),
    ]
    .into_iter()
    .map(|m| (m.id.clone(), m))
    .collect();
    for mapping in engine.state.input_mappings.values() {
        all.insert(mapping.id.clone(), mapping.clone());
    }
    all.into_values().collect()
}

fn mapping(engine: &Engine, id: &str) -> Option<InputMapping> {
    mappings(engine).into_iter().find(|m| m.id == id)
}

/// The condition of the "claim it again" notice after a restart.
fn reclaim(controller_id: &str) -> String {
    format!("reclaim:{controller_id}")
}

fn not_neutral(controller: &Controller, doing: &str) -> ApiError {
    ApiError::new(
        ErrorKind::InvalidValue,
        format!(
            "Centre roll, pitch and yaw on {} before {doing}. Throttle can stay where it is. \
             If it isn't a known gamepad, move each mapped stick once first.",
            controller.name
        ),
    )
}

/// `a`, `a and b`, `a, b and c`.
fn list(items: &[impl AsRef<str>]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no {what}"))
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let denied = |why: &str| Err(ApiError::new(ErrorKind::CapabilityDenied, why.to_owned()));
    match request {
        ClientRequest::ClientSelect(ClientSelectParams { vehicle_id }) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            engine.select(connection, vehicle_id);
            Ok(json!({}))
        }
        ClientRequest::ControllerInputs(_) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            to_value(&engine.system.controller_inputs())
        }
        ClientRequest::MappingCreate(params) => {
            if !granted.contains(&Capability::Config) {
                return denied(&Capability::Config.denied_message());
            }
            create_mapping(engine, connection, params)
        }
        ClientRequest::MappingUpdate(params) => {
            if !granted.contains(&Capability::Config) {
                return denied(&Capability::Config.denied_message());
            }
            update_mapping(engine, connection, params)
        }
        ClientRequest::ControllerForget(IdParams { id }) => {
            if !operator || !granted.contains(&Capability::Config) {
                return denied("only an operator client with config may forget a controller");
            }
            forget(engine, connection, &id)
        }
        _ if !operator || !granted.contains(&Capability::Control) => {
            denied("only an operator client may hold a controller")
        }
        ClientRequest::ControllerClaim(params) => claim(engine, connection, params),
        ClientRequest::ControllerRelease(IdParams { id }) => {
            if !engine.state.controllers.contains_key(&id) {
                return Err(not_found(&format!("controller {id}")));
            }
            engine.release(&id, connection, None);
            Ok(json!({}))
        }
        ClientRequest::ControllerSetTarget(params) => set_target(engine, connection, params),
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a controller method",
        )),
    }
}

/// Forgets an unplugged, unheld controller. One that is plugged in would
/// come straight back, and one that is held is driving.
fn forget(engine: &mut Engine, connection: ConnectionId, id: &str) -> Result<Value, ApiError> {
    let controller = engine
        .state
        .controllers
        .get(id)
        .ok_or_else(|| not_found(&format!("controller {id}")))?;
    if controller.connected {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("Unplug {} before forgetting it.", controller.name),
        ));
    }
    if controller.claim.is_some() || engine.live.claimant.contains_key(id) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("Release {} first.", controller.name),
        ));
    }
    engine.live.neutral.remove(id);
    engine.resolve(&reclaim(id));
    engine.commit(connection, Change::ControllerRemoved { id: id.to_owned() });
    Ok(json!({}))
}

fn claim(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ControllerClaimParams,
) -> Result<Value, ApiError> {
    let mut controller = engine
        .state
        .controllers
        .get(&params.id)
        .cloned()
        .filter(|c| c.connected)
        .ok_or_else(|| not_found(&format!("connected controller {}", params.id)))?;
    if let Some(holder) = engine.live.claimant.get(&controller.id)
        && *holder != connection
    {
        let name = controller
            .claim
            .as_ref()
            .map_or("another client", |c| c.client_name.as_str());
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("{} is held by {name}", controller.name),
        ));
    }
    if let Some(mapping_id) = params.mapping_id {
        if mapping(engine, &mapping_id).is_none() {
            return Err(not_found(&format!("mapping {mapping_id}")));
        }
        controller.mapping_id = mapping_id;
    }
    let mode: TargetMode = params.mode.into();
    let target = match mode {
        TargetMode::Pinned => params.vehicle_id,
        TargetMode::FollowsSelection => params
            .vehicle_id
            .or_else(|| engine.live.selection.get(&connection).cloned()),
    };
    let target = engine.drivable(&controller.id, target)?;
    engine.require_claim_neutral(&controller, &target)?;
    engine.require_rc_channels(&controller.mapping_id, &target)?;
    let actor = engine.actor(connection);
    controller.claim = Some(Claim {
        client_id: actor.client_id.clone(),
        client_name: engine.client_name(connection),
        operator_id: actor.operator_id.clone(),
    });
    controller.mode = mode;
    controller.target = Some(target);
    controller.pending_target = None;
    controller.version += 1;
    engine
        .live
        .claimant
        .insert(controller.id.clone(), connection);
    engine.resolve(&reclaim(&controller.id));
    engine.commit_as(
        actor,
        Change::ControllerUpserted {
            controller: controller.clone(),
        },
    );
    engine.route(&controller.id);
    engine.announce_target(&controller);
    to_value(&ControllerDto::from(&controller))
}

fn set_target(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ControllerSetTargetParams,
) -> Result<Value, ApiError> {
    let mut controller = engine
        .state
        .controllers
        .get(&params.id)
        .cloned()
        .ok_or_else(|| not_found(&format!("controller {}", params.id)))?;
    if engine.live.claimant.get(&controller.id) != Some(&connection) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            format!("{} is not held by this client", controller.name),
        ));
    }
    let mode: TargetMode = params.mode.into();
    let wanted = match mode {
        TargetMode::Pinned => params.vehicle_id.or_else(|| controller.target.clone()),
        TargetMode::FollowsSelection => params
            .vehicle_id
            .or_else(|| controller.pending_target.clone())
            .or_else(|| engine.live.selection.get(&connection).cloned())
            .or_else(|| controller.target.clone()),
    };
    let target = engine.drivable(&controller.id, wanted)?;
    engine.require_rc_channels(&controller.mapping_id, &target)?;
    let moving = controller.target.as_deref() != Some(target.as_str());
    if moving {
        let stays = controller.target.as_deref().map_or_else(String::new, |t| {
            format!(": it stays on {}", engine.target_name(t))
        });
        engine.require_neutral(&controller, &format!("switching{stays}"))?;
    }
    controller.mode = mode;
    controller.target = Some(target);
    controller.pending_target = None;
    controller.version += 1;
    engine.commit(
        connection,
        Change::ControllerUpserted {
            controller: controller.clone(),
        },
    );
    if moving {
        engine.route(&controller.id);
        engine.announce_target(&controller);
    }
    to_value(&ControllerDto::from(&controller))
}

fn create_mapping(
    engine: &mut Engine,
    connection: ConnectionId,
    params: MappingCreateParams,
) -> Result<Value, ApiError> {
    let name = params.name.trim();
    if name.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a mapping needs a name",
        ));
    }
    let from = params.from.unwrap_or_else(|| InputMapping::mode_2().id);
    let source = mapping(engine, &from).ok_or_else(|| not_found(&format!("mapping {from}")))?;
    // Readable and stable: from the name, numbered when taken.
    let slug: String = name
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() {
        "mapping".to_owned()
    } else {
        slug
    };
    let taken = mappings(engine);
    let id = std::iter::once(slug.clone())
        // One of these is free: there are fewer mappings than names tried.
        .chain((2..=taken.len() + 2).map(|n| format!("{slug}-{n}")))
        .find(|id| !taken.iter().any(|m| &m.id == id))
        .unwrap_or(slug);
    let mapping = InputMapping {
        id,
        name: name.to_owned(),
        version: 1,
        ..source
    };
    engine.commit(
        connection,
        Change::InputMappingUpserted {
            mapping: mapping.clone(),
        },
    );
    to_value(&MappingDto::from(&mapping))
}

/// Every calibration must be able to scale a reading.
fn require_calibrations(params: &MappingUpdateParams) -> Result<(), ApiError> {
    let calibrations = [params.roll, params.pitch, params.yaw, params.throttle]
        .into_iter()
        .chain(params.rc_channels.iter().map(|b| b.input))
        .filter_map(|b| b.calibration);
    if calibrations
        .map(uxduck_core::domain::controller::AxisCalibration::from)
        .any(|c| !c.is_valid())
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A stick's calibration needs its ends on either side of its centre. Calibrate it again.",
        ));
    }
    Ok(())
}

fn update_mapping(
    engine: &mut Engine,
    connection: ConnectionId,
    params: MappingUpdateParams,
) -> Result<Value, ApiError> {
    let current =
        mapping(engine, &params.id).ok_or_else(|| not_found(&format!("mapping {}", params.id)))?;
    if current.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "mapping {} is at version {}, not {}",
                current.name, current.version, params.expected_version
            ),
        );
        error.data.current_version = Some(current.version);
        return Err(error);
    }
    // A change to how the sticks read would jump a flying vehicle.
    let sticks: [uxduck_core::domain::controller::AxisBinding; 4] = [
        params.roll.into(),
        params.pitch.into(),
        params.yaw.into(),
        params.throttle.into(),
    ];
    if (current.throttle_range != params.throttle_range.into()
        || sticks != [current.roll, current.pitch, current.yaw, current.throttle]
        || (current.deadzone - params.deadzone).abs() > f64::EPSILON
        || current.rc_channels
            != params
                .rc_channels
                .iter()
                .copied()
                .map(Into::into)
                .collect::<Vec<_>>())
        && engine
            .state
            .controllers
            .values()
            .any(|controller| controller.mapping_id == current.id && controller.claim.is_some())
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Release every controller using this mapping before changing its sticks, deadzone, throttle range or RC channels.",
        ));
    }
    require_calibrations(&params)?;
    let channels: std::collections::BTreeSet<_> =
        params.rc_channels.iter().map(|b| b.channel).collect();
    if channels.len() != params.rc_channels.len() || channels.iter().any(|c| !(5..=16).contains(c))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "RC channels must be unique and between 5 and 16.",
        ));
    }
    if params
        .positions
        .iter()
        .any(|p| !(-1.0..=1.0).contains(&p.at))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a position is between -1 and 1",
        ));
    }
    if params.name.trim().is_empty() || !(0.0..=0.5).contains(&params.deadzone) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a mapping needs a name and a deadzone between 0 and 0.5",
        ));
    }
    let mapping = InputMapping {
        id: current.id,
        name: params.name.trim().to_owned(),
        roll: params.roll.into(),
        pitch: params.pitch.into(),
        yaw: params.yaw.into(),
        throttle: params.throttle.into(),
        throttle_range: params.throttle_range.into(),
        deadzone: params.deadzone,
        buttons: params.buttons.into_iter().map(Into::into).collect(),
        positions: params.positions.into_iter().map(Into::into).collect(),
        rc_channels: params.rc_channels.into_iter().map(Into::into).collect(),
        version: current.version + 1,
    };
    engine.commit(
        connection,
        Change::InputMappingUpserted {
            mapping: mapping.clone(),
        },
    );
    let using: Vec<String> = engine
        .state
        .controllers
        .values()
        .filter(|c| c.mapping_id == mapping.id)
        .map(|c| c.id.clone())
        .collect();
    for id in using {
        engine.route(&id);
    }
    to_value(&MappingDto::from(&mapping))
}

impl Engine {
    fn client_name(&self, connection: ConnectionId) -> String {
        self.connections
            .get(&connection)
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "a client".into())
    }

    fn target_name(&self, vehicle_id: &str) -> String {
        self.vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.name.clone())
    }

    pub(crate) fn pilot_input_method(
        &self,
        target: &str,
    ) -> uxduck_core::domain::settings::PilotInputMethod {
        use uxduck_core::domain::{modes::Family, settings::PilotInputMethod};
        let Some(vehicle) = self.vehicle(target) else {
            return PilotInputMethod::Automatic;
        };
        let method = std::iter::once(&vehicle.id)
            .chain(vehicle.aliases.iter())
            .find_map(|id| self.state.settings.pilot_input_methods.get(id))
            .copied()
            .unwrap_or_default();
        method.effective(Family::for_vehicle(
            &vehicle.autopilot,
            &vehicle.vehicle_type,
        ))
    }

    pub(crate) fn configure_pilot_input(
        &self,
        settings: &mut uxduck_core::domain::settings::Settings,
        input: &uxduck_contracts::dto::settings::PilotInputSetting,
    ) -> Result<(), ApiError> {
        use uxduck_core::domain::{modes::Family, settings::PilotInputMethod};
        let vehicle = self
            .vehicle(&input.vehicle_id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "No such vehicle"))?;
        if !matches!(vehicle.autopilot.as_str(), "Ardupilotmega" | "Px4")
            || Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type).is_none()
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Pilot input selection requires a supported ArduPilot or PX4 vehicle.",
            ));
        }
        if self.state.controllers.values().any(|c| {
            c.claim.is_some()
                && [c.target.as_deref(), c.pending_target.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|id| id == vehicle.id || vehicle.aliases.iter().any(|alias| alias == id))
        }) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                "Release the controller before changing this vehicle's pilot input method.",
            ));
        }
        let method = PilotInputMethod::from(input.method);
        if method == PilotInputMethod::ManualControl
            && Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type) == Some(Family::Blimp)
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "ArduBlimp does not handle MANUAL_CONTROL pilot axes. Use RC_CHANNELS_OVERRIDE.",
            ));
        }
        for id in std::iter::once(&vehicle.id).chain(vehicle.aliases.iter()) {
            settings.pilot_input_methods.remove(id);
        }
        if method != PilotInputMethod::Automatic {
            settings
                .pilot_input_methods
                .insert(vehicle.id.clone(), method);
        }
        Ok(())
    }

    /// Extra RC inputs must be supported and separate from the pilot channels.
    fn require_rc_channels(&self, mapping_id: &str, target: &str) -> Result<(), ApiError> {
        let Some(mapping) = mapping(self, mapping_id) else {
            return Ok(());
        };
        if mapping.rc_channels.is_empty() {
            return Ok(());
        }
        let vehicle = self
            .state
            .vehicles
            .get(target)
            .ok_or_else(|| not_found(target))?;
        let family = uxduck_core::domain::modes::Family::for_vehicle(
            &vehicle.autopilot,
            &vehicle.vehicle_type,
        );
        if self.pilot_input_method(target)
            != uxduck_core::domain::settings::PilotInputMethod::RcChannelsOverride
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Extra RC channels require RC_CHANNELS_OVERRIDE. Change the vehicle's pilot input method or remove the extra bindings.",
            ));
        }
        let axes: &[(&str, u8)] = match family {
            Some(uxduck_core::domain::modes::Family::Sub) => {
                &[("FORWARD", 5), ("LATERAL", 6), ("THROTTLE", 3), ("YAW", 4)]
            }
            Some(uxduck_core::domain::modes::Family::Rover) => &[("ROLL", 1), ("THROTTLE", 3)],
            _ => &[("ROLL", 1), ("PITCH", 2), ("THROTTLE", 3), ("YAW", 4)],
        };
        for &(axis, fallback) in axes {
            let prefix = if family == Some(uxduck_core::domain::modes::Family::Px4) {
                "RC_MAP_"
            } else {
                "RCMAP_"
            };
            let name = format!("{prefix}{axis}");
            let channel = self
                .state
                .parameters
                .values()
                .find(|p| p.vehicle_id == target && p.name == name)
                .map(|p| p.value)
                .or(Some(f64::from(fallback)));
            if channel.is_some_and(|c| {
                mapping
                    .rc_channels
                    .iter()
                    .any(|b| (f64::from(b.channel) - c).abs() < f64::EPSILON)
            }) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("An extra RC channel overlaps {name}. Choose a different channel."),
                ));
            }
        }
        Ok(())
    }

    /// Before a claim the data plane has no mapping of the controller's, so
    /// it judges neutral by Mode 2's, where the throttle it leaves out may be
    /// this controller's pitch. Judged here through the mapping it will
    /// drive by, from its raw input now; by the data plane's word when the
    /// host reads no raw input for it.
    fn require_claim_neutral(&self, controller: &Controller, target: &str) -> Result<(), ApiError> {
        let inputs = self.system.controller_inputs();
        let raw = inputs.controllers.iter().find(|c| c.id == controller.id);
        let (Some(raw), Some(mapping)) = (raw, mapping(self, &controller.mapping_id)) else {
            return self.require_neutral(controller, "using it");
        };
        let read = |binding: &uxduck_core::domain::controller::AxisBinding| {
            let axis = binding.axis.into();
            raw.axes
                .iter()
                .find(|a| match binding.code {
                    Some(code) => a.code == Some(code),
                    None => a.axis == Some(axis),
                })
                .map(|a| a.value)
                // A radio's input is unknown until it moves (see the data
                // plane's `Device::read`); a gamepad's rests centred.
                .or_else(|| raw.layout_known.then_some(0.0))
        };
        let mut channels = vec![
            ("roll", mapping.roll),
            ("pitch", mapping.pitch),
            ("throttle", mapping.throttle),
            ("yaw", mapping.yaw),
        ];
        channels.extend(
            mapping
                .rc_channels
                .iter()
                .map(|b| ("extra RC channel", b.input)),
        );
        // Say which: a mapping that reads nothing of this controller is not
        // fixed by moving its sticks.
        let unread: Vec<&str> = channels
            .iter()
            .filter(|(_, b)| read(b).is_none())
            .map(|&(name, _)| name)
            .collect();
        if !unread.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "{} doesn't read {}'s {}. Make a mapping for {} on the Mapping page, \
                     then move each stick to set it.",
                    mapping.name,
                    controller.name,
                    list(&unread),
                    controller.name,
                ),
            ));
        }
        let sticks = mapping.sticks(|b| read(b).unwrap_or(0.0));
        let off: Vec<String> = [
            ("roll", sticks.roll),
            ("pitch", sticks.pitch),
            ("yaw", sticks.yaw),
        ]
        .iter()
        .filter(|(_, value)| value.abs() >= NEUTRAL)
        .map(|(name, value)| format!("{name} (it reads {value:.2})"))
        .collect();
        if off.is_empty() {
            self.require_throttle_to_hold(controller, target, sticks.throttle)
        } else {
            Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "Centre {} on {} before using it. Throttle can stay where it is.",
                    list(&off),
                    controller.name
                ),
            ))
        }
    }

    /// A multicopter in the air taken over with the throttle at idle
    /// descends at once, in any mode: the throttle must be near the middle,
    /// where Loiter and Alt Hold hold height and Stabilize roughly hovers.
    /// On the ground, or for a plane or rover, it may be anywhere.
    fn require_throttle_to_hold(
        &self,
        controller: &Controller,
        target: &str,
        throttle: f64,
    ) -> Result<(), ApiError> {
        let Some(vehicle) = self.vehicle(target) else {
            return Ok(());
        };
        let copter = Family::of(&vehicle.vehicle_type) == Some(Family::Copter);
        if !copter || !vehicle.armed || !self.off_the_ground(target, true) {
            return Ok(());
        }
        if throttle >= -HOLDING_THROTTLE {
            return Ok(());
        }
        Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{} is in the air. Raise the throttle on {} to about the middle before \
                 using it, or it would descend at once.",
                vehicle.name, controller.name
            ),
        ))
    }

    fn require_neutral(&self, controller: &Controller, doing: &str) -> Result<(), ApiError> {
        if self
            .live
            .neutral
            .get(&controller.id)
            .copied()
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(not_neutral(controller, doing))
        }
    }

    /// The vehicle's id, if it exists and no other controller drives it.
    fn drivable(
        &self,
        controller_id: &str,
        vehicle_id: Option<String>,
    ) -> Result<String, ApiError> {
        let Some(vehicle_id) = vehicle_id else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "select a vehicle for the controller to drive",
            ));
        };
        let vehicle = self
            .vehicle(&vehicle_id)
            .ok_or_else(|| not_found(&format!("vehicle {vehicle_id}")))?;
        if let Some(other) = self.state.controllers.values().find(|c| {
            c.id != controller_id && c.claim.is_some() && c.target.as_deref() == Some(&vehicle.id)
        }) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("{} already drives {}", other.name, vehicle.name),
            ));
        }
        Ok(vehicle.id.clone())
    }

    /// Tells the data plane what a controller drives now, if anything.
    fn route(&mut self, controller_id: &str) {
        let route = self
            .state
            .controllers
            .get(controller_id)
            .filter(|c| c.connected && c.claim.is_some())
            .and_then(|c| {
                Some(ControlRoute {
                    vehicle_id: c.target.clone()?,
                    mapping: mapping(self, &c.mapping_id)?,
                    pilot_input_method: self.pilot_input_method(c.target.as_deref()?),
                })
            });
        self.links.route_controller(controller_id, route);
    }

    fn announce_target(&mut self, controller: &Controller) {
        let Some(target) = controller.target.as_deref() else {
            return;
        };
        let name = self.target_name(target);
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: target.to_owned(),
            },
            title: format!("{} now drives {name}", controller.name),
            body: None,
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }

    /// Releases a controller: no route, so nothing more is sent.
    fn release(&mut self, id: &str, by: ConnectionId, why: Option<&str>) {
        let Some(mut controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        self.live.claimant.remove(id);
        if controller.claim.is_none() {
            return;
        }
        let target = controller.target.take();
        controller.claim = None;
        controller.pending_target = None;
        controller.version += 1;
        let actor = if why.is_some() {
            daemon()
        } else {
            self.actor(by)
        };
        self.commit_as(
            actor,
            Change::ControllerUpserted {
                controller: controller.clone(),
            },
        );
        self.route(id);
        if let (Some(why), Some(target)) = (why, target) {
            let name = self.target_name(&target);
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle { id: target },
                title: format!("{} stopped driving {name}: {why}", controller.name),
                body: Some(format!(
                    "It no longer sends stick input. {name}'s own failsafe takes over."
                )),
                actions: Vec::new(),
                condition: None,
                report: None,
            });
        }
    }

    /// A connection's selected vehicle. Its controllers that follow the
    /// selection do not move: they wait for neutral sticks and a
    /// confirmation (`controller/setTarget`).
    fn select(&mut self, connection: ConnectionId, vehicle_id: Option<String>) {
        let vehicle_id = vehicle_id.and_then(|id| self.vehicle(&id).map(|v| v.id.clone()));
        match &vehicle_id {
            Some(id) => self.live.selection.insert(connection, id.clone()),
            None => self.live.selection.remove(&connection),
        };
        let held: Vec<Controller> = self
            .state
            .controllers
            .values()
            .filter(|c| {
                c.mode == TargetMode::FollowsSelection
                    && self.live.claimant.get(&c.id) == Some(&connection)
            })
            .cloned()
            .collect();
        for mut controller in held {
            let pending = vehicle_id
                .clone()
                .filter(|id| controller.target.as_ref() != Some(id));
            if pending != controller.pending_target {
                controller.pending_target = pending;
                controller.version += 1;
                self.commit(connection, Change::ControllerUpserted { controller });
            }
        }
    }

    /// A connection closed: its controllers are released.
    pub(crate) fn release_claims_of(&mut self, connection: ConnectionId) {
        self.live.selection.remove(&connection);
        let held: Vec<String> = self
            .live
            .claimant
            .iter()
            .filter(|(_, c)| **c == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for id in held {
            self.release(&id, connection, Some("its client disconnected"));
        }
    }

    /// Controllers from before a restart are neither connected nor held.
    pub(crate) fn reset_controllers(&mut self) {
        let stale: Vec<Controller> = self
            .state
            .controllers
            .values()
            .filter(|c| c.connected || c.claim.is_some())
            .cloned()
            .collect();
        for mut controller in stale {
            if controller.claim.is_some() {
                self.raise(Raise {
                    severity: Severity::Warning,
                    kind: NotificationKind::Persistent,
                    subject: Subject::System,
                    title: format!(
                        "{} stopped driving after the restart",
                        controller.name
                    ),
                    body: Some(
                        "Manual control doesn't come back by itself. Centre the sticks, then press Use in the Controllers tab."
                            .into(),
                    ),
                    actions: Vec::new(),
                    condition: Some(reclaim(&controller.id)),
                    report: None,
                });
            }
            controller.connected = false;
            controller.claim = None;
            controller.target = None;
            controller.pending_target = None;
            controller.version += 1;
            self.commit_as(daemon(), Change::ControllerUpserted { controller });
        }
    }

    /// The data plane found a controller. One whose layout the platform
    /// does not know, such as a radio in joystick mode, starts on the radio
    /// mapping, and leaves one that reads only gamepad stick names, which
    /// read nothing of it.
    pub fn controller_connected(&mut self, id: &str, name: &str, layout_known: bool) {
        let fitting = if layout_known {
            InputMapping::mode_2().id
        } else {
            InputMapping::for_radio(name).id
        };
        let controller = match self.state.controllers.get(id).cloned() {
            Some(mut known) => {
                // The USB radio mapping was briefly every radio's, and reads
                // an ExpressLRS joystick's arm switch as its throttle.
                let usb_radio = known.mapping_id == InputMapping::radio().id;
                let refit = !layout_known
                    && fitting != known.mapping_id
                    && (usb_radio
                        || mapping(self, &known.mapping_id).is_some_and(|m| {
                            [m.roll, m.pitch, m.throttle, m.yaw]
                                .iter()
                                .all(|b| b.code.is_none())
                        }));
                if known.connected && known.name == name && !refit {
                    return;
                }
                known.connected = true;
                name.clone_into(&mut known.name);
                if refit {
                    known.mapping_id = fitting;
                }
                known.version += 1;
                known
            }
            None => Controller {
                id: id.to_owned(),
                name: name.to_owned(),
                connected: true,
                claim: None,
                mode: TargetMode::FollowsSelection,
                target: None,
                pending_target: None,
                mapping_id: fitting,
                version: 1,
            },
        };
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
    }

    /// A controller was unplugged: released, so the vehicle's own failsafe
    /// takes over.
    pub fn controller_disconnected(&mut self, id: &str) {
        let Some(holder) = self.live.claimant.get(id).copied() else {
            if let Some(mut controller) = self
                .state
                .controllers
                .get(id)
                .cloned()
                .filter(|c| c.connected)
            {
                controller.connected = false;
                controller.version += 1;
                self.commit_as(daemon(), Change::ControllerUpserted { controller });
            }
            return;
        };
        self.live.neutral.remove(id);
        if let Some(mut controller) = self.state.controllers.get(id).cloned() {
            controller.connected = false;
            controller.version += 1;
            self.commit_as(daemon(), Change::ControllerUpserted { controller });
        }
        self.release(id, holder, Some("it was unplugged"));
    }

    /// Whether a controller's sticks are centred, as the data plane sees
    /// them through its mapping.
    pub fn controller_neutral(&mut self, id: &str, neutral: bool) {
        self.live.neutral.insert(id.to_owned(), neutral);
    }

    /// A button went down: its command goes to the target, as the claiming
    /// client, through the ordinary command path.
    pub fn controller_pressed(
        &mut self,
        id: &str,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) {
        let Some(controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        let (Some(connection), Some(target)) =
            (self.live.claimant.get(id).copied(), controller.target)
        else {
            return;
        };
        let Some(binding) = mapping(self, &controller.mapping_id).and_then(|m| {
            m.buttons.into_iter().find(|b| match b.code {
                Some(wanted) => code == Some(wanted),
                None => button == Some(b.button),
            })
        }) else {
            return;
        };
        self.send_for_controller(connection, target, &binding.command);
    }

    /// An axis moved into a position: its command goes to the target, as
    /// for a button. The position is found in the controller's mapping now,
    /// so one the data plane held from an older version sends nothing.
    pub fn controller_positioned(&mut self, id: &str, position: &PositionBinding) {
        let Some(controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        let (Some(connection), Some(target)) =
            (self.live.claimant.get(id).copied(), controller.target)
        else {
            return;
        };
        let Some(found) = mapping(self, &controller.mapping_id)
            .and_then(|m| m.positions.into_iter().find(|p| p == position))
        else {
            return;
        };
        self.send_for_controller(connection, target, &found.command);
    }

    fn send_for_controller(&mut self, connection: ConnectionId, target: String, command: &Command) {
        let request = ClientRequest::CommandSend(CommandSendParams {
            vehicle_id: target,
            command: command.into(),
            confirm: false,
            force: false,
            command_id: None,
            sent_at_ms: None,
        });
        let _ = crate::commands::handle(self, connection, request, &[Capability::Control], true);
    }
}

/// The wire form of a controller or mapping change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::ControllerUpserted { controller } => vec![EntityChange {
            controller: Some(controller.into()),
            ..upsert(EntityType::Controller, &controller.id)
        }],
        Change::ControllerRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::Controller, id)
        }],
        Change::InputMappingUpserted { mapping } => vec![EntityChange {
            input_mapping: Some(mapping.into()),
            ..upsert(EntityType::InputMapping, &mapping.id)
        }],
        _ => Vec::new(),
    }
}

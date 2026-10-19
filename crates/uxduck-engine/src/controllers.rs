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
//!   operator's), without another confirmation;
//!   meanwhile it stays on the old one and says so;
//! - two controllers never drive one vehicle;
//! - when the controller is unplugged, or the claiming client goes, it is
//!   released and sends nothing, so the vehicle's own failsafe decides; a
//!   window gets a moment first, so a reload takes its controllers back.
//! - the same operator's other window on this computer may take a held
//!   controller over, still flying what it flew.

use std::collections::{BTreeMap, BTreeSet};

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
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{
    Claim, Controller, GamepadButton, InputMapping, NEUTRAL, PositionBinding, Sticks, TargetMode,
};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::events::{Actor, Change};
use uxduck_core::policy::admission::Trust;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::ControlRoute;
use crate::{ConnectionId, Engine, upsert};

/// How far below the middle a flying multicopter's throttle may be when a
/// controller takes it over: -1 is idle, 0 the middle; -0.3 is 35% input.
const HOLDING_THROTTLE: f64 = 0.3;

/// How long a window's controllers keep flying after it goes, for the
/// next window like it to take back: long enough for a reload.
pub const CLAIM_GRACE_US: u64 = 15_000_000;

/// A claim whose window went, kept for one like it to take back.
#[derive(Debug)]
struct Detached {
    connection: ConnectionId,
    record: String,
    trust: Trust,
    until_us: u64,
}

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
    /// Claims whose window went moments ago: still routed, held by no
    /// connection, until a window like it comes or the grace runs out.
    detached: BTreeMap<String, Detached>,
    /// Page-owned keyboards never inherit a physical controller's reload grace.
    keyboards: BTreeMap<ConnectionId, String>,
    /// Simulator links whose controller routes need a safe restart handover.
    restarting: BTreeSet<String>,
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

fn keyboard_mapping() -> InputMapping {
    InputMapping {
        id: "sim-keyboard".into(),
        name: "Keyboard".into(),
        deadzone: 0.0,
        buttons: Vec::new(),
        ..InputMapping::mode_2()
    }
}

fn mapping(engine: &Engine, id: &str) -> Option<InputMapping> {
    if id == "sim-keyboard" {
        return Some(keyboard_mapping());
    }
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
            if params.id == "sim-keyboard" {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "The keyboard mapping is fixed",
                ));
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
        ClientRequest::KeyboardStart(params) => keyboard_start(engine, connection, params),
        ClientRequest::KeyboardInput(params) => keyboard_input(engine, connection, params.axes),
        ClientRequest::KeyboardStop(_) => {
            engine.stop_keyboard(connection);
            Ok(json!({}))
        }
        ClientRequest::ControllerClaim(params) if params.id.starts_with("keyboard:") => {
            denied("Start keyboard control from the vehicle's Cockpit tab")
        }
        ClientRequest::ControllerSetTarget(params) if params.id.starts_with("keyboard:") => {
            denied("Release keyboard control before choosing another vehicle")
        }
        ClientRequest::ControllerTakeOver(IdParams { id }) if id.starts_with("keyboard:") => {
            denied("Keyboard control stays with the page that started it")
        }
        ClientRequest::ControllerClaim(params) => claim(engine, connection, params),
        ClientRequest::ControllerTakeOver(IdParams { id }) => take_over(engine, connection, &id),
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

/// One page, one explicitly pinned vehicle. Ordinary claims decide exclusivity.
fn keyboard_start(
    engine: &mut Engine,
    connection: ConnectionId,
    params: uxduck_contracts::dto::controller::KeyboardStartParams,
) -> Result<Value, ApiError> {
    if engine.live.keyboards.contains_key(&connection) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "Release keyboard control first",
        ));
    }
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .ok_or_else(|| not_found("vehicle"))?;
    if vehicle.presence != uxduck_core::domain::vehicle::Presence::Online {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "The vehicle is offline",
        ));
    }
    if !params.throttle.is_finite() || !(-1.0..=1.0).contains(&params.throttle) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Throttle must be between -1 and 1",
        ));
    }
    let id = format!("keyboard:{}", connection.0);
    engine.drivable(&id, Some(params.vehicle_id.clone()))?;
    engine.controller_connected(&id, "Keyboard", true);
    let controller = engine
        .state
        .controllers
        .get(&id)
        .cloned()
        .ok_or_else(|| not_found("keyboard controller"))?;
    if let Err(error) =
        engine.require_throttle_to_hold(&controller, Some(&params.vehicle_id), params.throttle)
    {
        engine.controller_disconnected(&id);
        return Err(error);
    }
    engine.controller_neutral(&id, true);
    // The start frame is neutral by construction, even if a preceding session's
    // input snapshot is still in the data plane's asynchronous queue.
    let now = engine.clock.now_us();
    // Initial values reach the data plane BEFORE its route: never briefly send
    // default or previous-session throttle while waiting for the first frame.
    engine
        .links
        .controller_input(crate::controller_input::InputEvent::Keyboard {
            id: id.clone(),
            axes: [0.0, 0.0, 0.0, params.throttle],
            at_us: now,
        });
    let result = claim(
        engine,
        connection,
        ControllerClaimParams {
            id: id.clone(),
            mode: uxduck_contracts::dto::controller::TargetMode::Pinned,
            vehicle_id: Some(params.vehicle_id),
            mapping_id: Some("sim-keyboard".into()),
        },
    );
    if result.is_ok() {
        engine.live.keyboards.insert(connection, id);
    } else {
        engine.controller_disconnected(&id);
        engine
            .links
            .controller_input(crate::controller_input::InputEvent::Disconnected { id });
    }
    result
}

fn keyboard_input(
    engine: &mut Engine,
    connection: ConnectionId,
    axes: [f64; 4],
) -> Result<Value, ApiError> {
    if axes
        .iter()
        .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Keyboard axes must be between -1 and 1",
        ));
    }
    let now = engine.clock.now_us();
    let Some(id) = engine.live.keyboards.get(&connection).cloned() else {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Keyboard input was rejected because this page has no active keyboard control session.",
        ));
    };
    if engine.live.claimant.get(&id) != Some(&connection) {
        engine.stop_keyboard(connection);
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Keyboard control ended because this page no longer holds the controller.",
        ));
    }
    engine
        .links
        .controller_input(crate::controller_input::InputEvent::Keyboard {
            id,
            axes,
            at_us: now,
        });
    Ok(json!({}))
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
    let held_elsewhere = engine
        .live
        .claimant
        .get(&controller.id)
        .is_some_and(|holder| *holder != connection)
        || engine.live.detached.contains_key(&controller.id);
    if held_elsewhere {
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
    // Following a selection that is not there yet holds the controller and
    // drives nothing until the first vehicle is selected at neutral.
    let target = match (mode, target) {
        (TargetMode::FollowsSelection, None) => None,
        (_, target) => Some(engine.drivable(&controller.id, target)?),
    };
    engine.require_claim_neutral(&controller, target.as_deref())?;
    if let Some(target) = &target {
        engine.require_rc_channels(&controller.mapping_id, target)?;
    }
    let actor = engine.actor(connection);
    controller.claim = Some(Claim {
        client_id: actor.client_id.clone(),
        client_name: engine.client_name(connection),
        operator_id: actor.operator_id.clone(),
    });
    controller.mode = mode;
    controller.target = target;
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
    to_value(&ControllerDto::from(
        &engine.state.controllers[&controller.id],
    ))
}

/// On this computer: not a phone on the network or a person through the
/// relay.
fn on_this_computer(trust: &Trust) -> bool {
    matches!(trust, Trust::LocalUser | Trust::PairedBrowser { .. })
}

/// Moves a held controller to this window, for the same operator on this
/// computer: the window holding it may be left open somewhere, or closed
/// moments ago. It keeps flying what it flew with the same mapping, so
/// nothing is checked about its sticks; only who answers for it changes.
fn take_over(engine: &mut Engine, connection: ConnectionId, id: &str) -> Result<Value, ApiError> {
    let mut controller = engine
        .state
        .controllers
        .get(id)
        .cloned()
        .ok_or_else(|| not_found(&format!("controller {id}")))?;
    let holder_trust = match (engine.live.claimant.get(id), engine.live.detached.get(id)) {
        (Some(holder), _) if *holder == connection => {
            return to_value(&ControllerDto::from(&controller));
        }
        // Windows only: agents and the CLI keep what they hold.
        (Some(holder), _) => engine.window_identity(*holder).map(|(_, trust)| trust),
        (None, Some(detached)) => Some(detached.trust.clone()),
        (None, None) => None,
    };
    let Some(claim) = controller.claim.as_mut() else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("{} isn't in use. Use it instead.", controller.name),
        ));
    };
    let actor = engine.actor(connection);
    let mine = engine
        .window_identity(connection)
        .is_some_and(|(_, trust)| on_this_computer(&trust));
    if !mine
        || !holder_trust.as_ref().is_some_and(on_this_computer)
        || claim.operator_id != actor.operator_id
    {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!(
                "{} is held by {}. Only the same operator on this computer can take it over.",
                controller.name, claim.client_name
            ),
        ));
    }
    claim.client_id.clone_from(&actor.client_id);
    claim.client_name = engine.client_name(connection);
    controller.pending_target = None;
    controller.version += 1;
    engine.live.detached.remove(id);
    engine.live.claimant.insert(id.to_owned(), connection);
    engine.commit_as(
        actor,
        Change::ControllerUpserted {
            controller: controller.clone(),
        },
    );
    to_value(&ControllerDto::from(&engine.state.controllers[id]))
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
    if moving && controller.target.is_none() {
        // Its first vehicle: judged as a claim is, the throttle included.
        engine.require_claim_neutral(&controller, Some(&target))?;
    } else if moving {
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
    to_value(&ControllerDto::from(
        &engine.state.controllers[&controller.id],
    ))
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
        stick_mode: params.stick_mode.map(Into::into).or(current.stick_mode),
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
            .map_or_else(|| "an unavailable vehicle".to_owned(), |v| v.name.clone())
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
    fn require_claim_neutral(
        &self,
        controller: &Controller,
        target: Option<&str>,
    ) -> Result<(), ApiError> {
        if controller.id.starts_with("keyboard:") {
            return self.require_neutral(controller, "using it");
        }
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
        target: Option<&str>,
        throttle: f64,
    ) -> Result<(), ApiError> {
        let Some((target, vehicle)) = target.and_then(|t| Some((t, self.vehicle(t)?))) else {
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
        // With its route removed the data plane uses its default mapping;
        // a paused Mode 1/radio must still be judged through its own mapping.
        let neutral = if controller.input_pause.is_some() {
            self.mapped_controller_input(controller)
                .is_some_and(|(sticks, _)| sticks.neutral())
        } else {
            self.live
                .neutral
                .get(&controller.id)
                .copied()
                .unwrap_or(false)
        };
        if neutral {
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

    /// Remove pilot input before closing the old simulator, retaining its claim.
    pub(crate) fn pause_controllers_for_restart(&mut self, link_id: &str) {
        self.live.restarting.insert(link_id.to_owned());
        let ids: Vec<String> = self
            .state
            .controllers
            .values()
            .filter(|c| {
                c.claim.is_some()
                    && c.target
                        .as_deref()
                        .and_then(|id| self.vehicle(id))
                        .is_some_and(|v| v.link_id == link_id)
            })
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            self.set_controller_pause(
                &id,
                Some("Controller paused while the simulation restarts.".into()),
            );
            self.links.route_controller(&id, None);
        }
    }

    fn set_controller_pause(&mut self, id: &str, pause: Option<String>) {
        let Some(mut controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        if controller.input_pause == pause {
            return;
        }
        controller.input_pause = pause;
        controller.version += 1;
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
    }

    /// Restart handover is stricter than an ordinary target switch. Read the
    /// actual mapping (including throttle range), never a cached neutral bit.
    fn controller_restart_pause(&self, id: &str) -> Option<String> {
        use uxduck_core::domain::{
            link::{LaunchStage, LinkStatus},
            vehicle::Presence,
        };
        let controller = self.state.controllers.get(id)?;
        controller.claim.as_ref()?;
        let Some(vehicle) = controller.target.as_deref().and_then(|id| self.vehicle(id)) else {
            return controller
                .input_pause
                .as_ref()
                .map(|_| "Controller paused: the simulation is unavailable.".into());
        };
        if !self.live.restarting.contains(&vehicle.link_id) {
            return None;
        }
        let Some(link) = self.state.links.get(&vehicle.link_id) else {
            return Some("Controller paused: the simulation is unavailable.".into());
        };
        if link
            .launch
            .as_ref()
            .is_some_and(|l| l.stage == LaunchStage::Failed)
        {
            return Some("Controller paused: startup failed. Start again to retry, or release the controller.".into());
        }
        if self.is_launching(&vehicle.link_id)
            || vehicle.presence != Presence::Online
            || link.status != LinkStatus::Up
        {
            return Some("Controller paused while the simulation restarts.".into());
        }
        let Some((sticks, buttons_held)) = self.mapped_controller_input(controller) else {
            return Some("Controller paused: waiting for every mapped stick input.".into());
        };
        let airborne = vehicle.armed
            && (self.off_the_ground(&vehicle.id, true)
                || link
                    .launch
                    .as_ref()
                    .is_some_and(|l| l.stage == LaunchStage::Flying));
        let throttle_safe = if airborne {
            sticks.throttle.abs() < 0.15
        } else {
            sticks.throttle <= -0.9
        };
        if !sticks.neutral() || !throttle_safe {
            return Some(if airborne {
                "Controller paused: centre roll, pitch and yaw, and set throttle near 50% to resume."
            } else {
                "Controller paused: centre roll, pitch and yaw, and lower throttle to idle to resume."
            }.into());
        }
        if buttons_held {
            return Some("Controller paused: release the buttons to resume.".into());
        }
        None
    }

    /// No missing/non-finite input may become a neutral fallback during handover.
    fn mapped_controller_input(&self, controller: &Controller) -> Option<(Sticks, bool)> {
        let inputs = self.system.controller_inputs();
        let raw = inputs.controllers.iter().find(|c| c.id == controller.id)?;
        let mapping = mapping(self, &controller.mapping_id)?;
        let read = |binding: &uxduck_core::domain::controller::AxisBinding| {
            raw.axes
                .iter()
                .find(|a| match binding.code {
                    Some(code) => a.code == Some(code),
                    None => a.axis == Some(binding.axis.into()),
                })
                .map(|a| a.value)
                .filter(|v| v.is_finite())
        };
        if [mapping.roll, mapping.pitch, mapping.yaw, mapping.throttle]
            .iter()
            .chain(mapping.rc_channels.iter().map(|b| &b.input))
            .any(|b| read(b).is_none())
        {
            return None;
        }
        Some((
            mapping.sticks(|b| read(b).unwrap_or(0.0)),
            raw.buttons.iter().any(|b| b.pressed),
        ))
    }

    /// Called by both hosts' expiry beat. Throttle-only movements do not
    /// generate neutral reports, so handover must also be retried here.
    fn resume_restart_controllers(&mut self) {
        let ids: Vec<String> = self
            .state
            .controllers
            .values()
            .filter(|c| c.claim.is_some() && c.input_pause.is_some())
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            let pause = self.controller_restart_pause(&id);
            if self
                .state
                .controllers
                .get(&id)
                .is_some_and(|c| c.input_pause != pause)
            {
                self.set_controller_pause(&id, pause);
                if self.state.controllers[&id].input_pause.is_none() {
                    self.route(&id);
                }
            }
        }
        self.live
            .restarting
            .retain(|id| self.state.links.contains_key(id));
    }

    /// Tells the data plane what a controller drives now, if anything.
    fn route(&mut self, controller_id: &str) {
        // New claims and target changes cannot bypass an automatic launch.
        if let Some(link) = self
            .state
            .controllers
            .get(controller_id)
            .and_then(|c| c.target.as_deref())
            .and_then(|id| self.vehicle(id))
            .map(|v| v.link_id.clone())
            && self.is_launching(&link)
        {
            self.live.restarting.insert(link);
        }
        let pause = self.controller_restart_pause(controller_id);
        self.set_controller_pause(controller_id, pause);
        let route = self
            .state
            .controllers
            .get(controller_id)
            .filter(|c| c.connected && c.claim.is_some() && c.input_pause.is_none())
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
        // The keyboard panel already shows which vehicle it controls.
        if controller.id.starts_with("keyboard:") {
            return;
        }
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
        self.live.detached.remove(id);
        if controller.claim.is_none() {
            return;
        }
        let target = controller.target.take();
        controller.claim = None;
        controller.input_pause = None;
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

    /// A connection's selected vehicle. Following controllers switch now
    /// at neutral, or when their sticks next return to neutral.
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
            let current = controller
                .target
                .as_deref()
                .and_then(|id| self.vehicle(id))
                .map(|v| &v.id);
            let pending = vehicle_id.clone().filter(|id| current != Some(id));
            let id = controller.id.clone();
            if pending != controller.pending_target {
                controller.pending_target = pending;
                controller.version += 1;
                self.commit(connection, Change::ControllerUpserted { controller });
            }
            self.follow_selection(&id);
        }
    }

    fn follow_selection(&mut self, id: &str) {
        let Some(controller) = self.state.controllers.get(id) else {
            return;
        };
        if controller.mode != TargetMode::FollowsSelection || controller.pending_target.is_none() {
            return;
        }
        let Some(connection) = self.live.claimant.get(id).copied() else {
            return;
        };
        // Use the explicit switch's checks and routing, including exclusive
        // targets, RC mappings and the throttle gate for a first vehicle.
        // A refused switch stays pending; the UI offers an explicit retry
        // which reports the reason, and future neutral input retries it.
        let _ = set_target(
            self,
            connection,
            ControllerSetTargetParams {
                id: id.to_owned(),
                mode: uxduck_contracts::dto::controller::TargetMode::FollowsSelection,
                vehicle_id: None,
            },
        );
    }

    /// Page-owned input is disconnected immediately, with no reload grace.
    fn stop_keyboard(&mut self, connection: ConnectionId) {
        if let Some(id) = self.live.keyboards.remove(&connection) {
            // Releasing a keyboard is not a physical device being unplugged.
            self.release(&id, connection, None);
            self.live.neutral.remove(&id);
            self.controller_disconnected(&id);
            self.links
                .controller_input(crate::controller_input::InputEvent::Disconnected { id });
        }
    }

    /// A connection closed: its controllers are released. A window's physical
    /// controllers keep flying for `CLAIM_GRACE_US` in case it is reloading.
    pub(crate) fn release_claims_of(&mut self, connection: ConnectionId) {
        self.stop_keyboard(connection);
        self.live.selection.remove(&connection);
        let held: Vec<String> = self
            .live
            .claimant
            .iter()
            .filter(|(_, c)| **c == connection)
            .map(|(id, _)| id.clone())
            .collect();
        let window = self.window_identity(connection);
        let until_us = self.clock.now_us() + CLAIM_GRACE_US;
        for id in held {
            match &window {
                Some((record, trust)) => {
                    self.live.claimant.remove(&id);
                    self.live.detached.insert(
                        id,
                        Detached {
                            connection,
                            record: record.clone(),
                            trust: trust.clone(),
                            until_us,
                        },
                    );
                }
                None => self.release(&id, connection, Some("its client disconnected")),
            }
        }
    }

    /// A window's client record and how it is trusted: the same for the
    /// page again after a reload, different for a phone or another person.
    fn window_identity(&self, connection: ConnectionId) -> Option<(String, Trust)> {
        let state = self.connections.get(&connection)?;
        let admitted = state
            .admitted
            .as_ref()
            .filter(|a| matches!(a.kind, ClientKind::OperatorUi | ClientKind::Shell))?;
        Some((admitted.record.clone(), state.trust.clone()))
    }

    /// A window was admitted: it takes back the controllers a window like
    /// it left moments ago, as after a reload, still driving what they drove.
    pub(crate) fn adopt_claims(&mut self, connection: ConnectionId) {
        let Some((record, trust)) = self.window_identity(connection) else {
            return;
        };
        let ids: Vec<String> = self
            .live
            .detached
            .iter()
            .filter(|(_, d)| d.record == record && d.trust == trust)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.live.detached.remove(&id);
            let Some(mut controller) = self.state.controllers.get(&id).cloned() else {
                continue;
            };
            let actor = self.actor(connection);
            if let Some(claim) = controller.claim.as_mut() {
                claim.client_id.clone_from(&actor.client_id);
                claim.client_name = self.client_name(connection);
            }
            controller.version += 1;
            self.live.claimant.insert(id, connection);
            self.commit_as(actor, Change::ControllerUpserted { controller });
        }
    }

    /// Releases the controllers no window took back in time.
    pub fn expire_detached_claims(&mut self, now_us: u64) {
        self.resume_restart_controllers();
        let gone: Vec<(String, ConnectionId)> = self
            .live
            .detached
            .iter()
            .filter(|(_, d)| d.until_us <= now_us)
            .map(|(id, d)| (id.clone(), d.connection))
            .collect();
        for (id, connection) in gone {
            self.release(&id, connection, Some("its window closed"));
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
                        "{} is off after the restart",
                        controller.name
                    ),
                    body: Some(if controller.id.starts_with("keyboard:") {
                        "Open the vehicle's Cockpit tab and press Fly with keyboard to enable it again.".into()
                    } else {
                        "You have to re-enable it to fly with it again: centre the sticks, then press Use in the Controllers tab.".into()
                    }),
                    actions: Vec::new(),
                    condition: Some(reclaim(&controller.id)),
                    report: None,
                });
            }
            controller.connected = false;
            controller.claim = None;
            controller.input_pause = None;
            controller.target = None;
            controller.pending_target = None;
            controller.version += 1;
            self.commit_as(daemon(), Change::ControllerUpserted { controller });
        }
    }

    /// The data plane found a controller. One whose layout the platform
    /// does not know, such as a radio in joystick mode, starts on the radio
    /// mapping, and leaves one that reads only gamepad stick names, which
    /// read nothing of it. Plugging one in says so, with the way to its tab.
    pub fn controller_connected(&mut self, id: &str, name: &str, layout_known: bool) {
        let plugged_in = self.state.controllers.get(id).is_none_or(|c| !c.connected);
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
                input_pause: None,
                pending_target: None,
                mapping_id: fitting,
                version: 1,
            },
        };
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
        if plugged_in && !id.starts_with("keyboard:") {
            self.controller_presence(id, name, true);
        }
    }

    /// Reconnects update one notice per device, including old notices without a condition.
    fn controller_presence(&mut self, id: &str, name: &str, connected: bool) {
        let condition = format!("controller-presence:{id}");
        let title = if connected {
            format!("{name} connected")
        } else if id.starts_with("web-") {
            format!("{name} input interrupted")
        } else {
            format!("{name} disconnected")
        };
        let body = (!connected).then(|| {
            if id.starts_with("web-") {
                "The browser stopped reporting this controller. Check the connection and keep this tab visible. A busy browser can interrupt input.".to_owned()
            } else {
                "Check its USB or wireless connection. Reconnect it before using it again.".to_owned()
            }
        });
        let severity = if connected {
            Severity::Info
        } else {
            Severity::Warning
        };
        let actions = vec![NotificationAction {
            label: "Open Controllers".into(),
            action: Action::OpenControllers {
                controller_id: id.to_owned(),
            },
        }];
        let mut previous: Vec<_> = self.state.notifications.values().filter(|notice| {
            notice.condition.as_ref() == Some(&condition) || notice.actions.iter().any(|action| {
                matches!(&action.action, Action::OpenControllers { controller_id } if controller_id == id)
            })
        }).cloned().collect();
        if let Some(mut notice) = previous.pop() {
            let interrupted = connected && notice.severity == Severity::Warning;
            notice.title = title;
            notice.body = body;
            notice.severity = severity;
            if interrupted {
                notice.title = format!("{name} reconnected");
                notice.body = Some(
                    "Input was interrupted. Check its connection before using it again.".to_owned(),
                );
                notice.severity = Severity::Warning;
            }
            notice.actions = actions;
            notice.condition = Some(condition);
            notice.state = NotificationState::Active;
            notice.created_at_us = self.clock.now_us();
            notice.version += 1;
            self.commit_as(
                daemon(),
                Change::NotificationUpserted {
                    notification: notice,
                },
            );
            for mut duplicate in previous {
                duplicate.state = NotificationState::Resolved;
                duplicate.actions.clear();
                duplicate.version += 1;
                self.commit_as(
                    daemon(),
                    Change::NotificationUpserted {
                        notification: duplicate,
                    },
                );
            }
        } else {
            self.raise(Raise {
                severity,
                kind: NotificationKind::Transient,
                subject: Subject::System,
                title,
                body,
                actions,
                condition: Some(condition),
                report: None,
            });
        }
    }

    /// A controller was unplugged: released, so the vehicle's own failsafe
    /// takes over.
    pub fn controller_disconnected(&mut self, id: &str) {
        let previous = self.state.controllers.get(id).cloned();
        if let Some(controller) = previous
            .as_ref()
            .filter(|c| c.connected && !id.starts_with("keyboard:"))
            && (controller.claim.is_none() || controller.target.is_none())
        {
            self.controller_presence(id, &controller.name, false);
        }
        self.live.keyboards.retain(|_, keyboard| keyboard != id);
        let holder = self.live.claimant.get(id).copied().or_else(|| {
            self.live
                .detached
                .get(id)
                .map(|detached| detached.connection)
        });
        let Some(holder) = holder else {
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
        self.release(
            id,
            holder,
            (!id.starts_with("keyboard:")).then_some(if id.starts_with("web-") {
                "browser input stopped"
            } else {
                "it was unplugged"
            }),
        );
    }

    /// Whether a controller's sticks are centred, as the data plane sees
    /// them through its mapping.
    pub fn controller_neutral(&mut self, id: &str, neutral: bool) {
        self.live.neutral.insert(id.to_owned(), neutral);
        if neutral {
            self.follow_selection(id);
        }
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
        if controller.input_pause.is_some() {
            return;
        }
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
        if controller.input_pause.is_some() {
            return;
        }
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

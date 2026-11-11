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
//! - native unplugging releases control. Missing browser input pauses output
//!   while retaining activation for fresh input from the claiming page;
//! - a departing client releases its controllers; a native window gets a
//!   moment first, so a reload takes its controllers back.
//! - the same operator's other window on this computer may take a held
//!   controller over, still flying what it flew.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use uxduck_contracts::dto::command::CommandSendParams;
use uxduck_contracts::dto::controller::{
    ClientSelectParams, Controller as ControllerDto, ControllerClaimParams,
    ControllerSetTargetParams, InputMapping as MappingDto, KeyboardStopReason, MappingCreateParams,
    MappingUpdateParams,
};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::command::{Command, ReceiptStatus};
use uxduck_core::domain::controller::{
    ActionBinding, Claim, Controller, GamepadButton, HOLD_MS, Hold, InputMapping, NEUTRAL,
    PositionBinding, StickControl, Sticks, TargetMode, centred_throttle,
};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, NotificationState, Severity, Subject,
};
use uxduck_core::events::{Actor, Change};
use uxduck_core::policy::admission::Trust;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::runtime::ControlRoute;
use crate::{ConnectionId, Engine, upsert};

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
    /// The connection holding each claimed controller.
    pub(crate) claimant: BTreeMap<String, ConnectionId>,
    /// Each connection's selected vehicle.
    selection: BTreeMap<ConnectionId, String>,
    /// Claims whose window went moments ago: still routed, held by no
    /// connection, until a window like it comes or the grace runs out.
    detached: BTreeMap<String, Detached>,
    /// Page-owned keyboards never inherit a physical controller's reload grace.
    pub(crate) keyboards: BTreeMap<ConnectionId, String>,
    /// Simulator links pressed to start again whose vehicle is not back yet.
    restarting: BTreeSet<String>,
    /// Browser input is missing, but the person has not released activation.
    /// The value is the interruption time: queued earlier samples cannot resume.
    /// No route or action may run until the claiming page supplies fresh input.
    input_lost: BTreeMap<String, u64>,
    /// Each routed controller's gripper wiring as last sent with its route
    /// (S263): which routes `gripper/hold` may join, and whether a
    /// parameter change needs a new route.
    pub(crate) routed_gripper:
        BTreeMap<String, Option<uxduck_core::domain::gripper::GripperWiring>>,
    /// Each controller's last position command still to be answered, and
    /// the position settled since (FB-67).
    position_sends: BTreeMap<String, PositionSend>,
}

/// A position's command on its way to the vehicle. A position settled
/// meanwhile goes once it is answered, never alongside it: the vehicle's
/// answer cannot say which of two mode changes it is for, and the older
/// must not land after the newer. A later one replaces it, so only where
/// the switch is now goes.
#[derive(Debug)]
struct PositionSend {
    receipt_id: String,
    next: Option<PositionBinding>,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
        identity: None,
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
        InputMapping::expresslrs_4_bluetooth(),
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
        ClientRequest::KeyboardStop(params) => {
            if params.reason == Some(KeyboardStopReason::AppUnfocused) {
                engine.say_keyboard_unfocused(connection);
            }
            engine.stop_keyboard(connection);
            Ok(json!({}))
        }
        ClientRequest::GripperHold(params) => crate::gripper::hold(engine, connection, params),
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
    // Where the throttle centres, a start away from the middle drives or
    // dives the moment the keyboard takes over, armed or not.
    if let Some(control) = centred_throttle(&vehicle.autopilot, &vehicle.vehicle_type)
        && params.throttle.abs() >= NEUTRAL
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("Start keyboard control with {control} at neutral."),
        ));
    }
    let id = format!("keyboard:{}", connection.0);
    engine.drivable(&id, Some(params.vehicle_id.clone()))?;
    engine.controller_connected(&id, "Keyboard", true);
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
    engine.resolve(&reclaim(id));
    engine.commit(connection, Change::ControllerRemoved { id: id.to_owned() });
    Ok(json!({}))
}

fn claim(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ControllerClaimParams,
) -> Result<Value, ApiError> {
    engine.ensure_identity_settled()?;
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
    // The person's mapping (maintainer, 2026-10-23): the one named, else
    // theirs for this controller if it is here, else the one it last used.
    let operator_id = engine.actor(connection).operator_id;
    let theirs = operator_id
        .as_ref()
        .and_then(|id| engine.state.operators.get(id))
        .and_then(|o| o.mapping_for(&controller.id, &controller.name))
        .filter(|id| mapping(engine, id).is_some())
        .map(str::to_owned);
    let chosen = params.mapping_id.is_some();
    if let Some(mapping_id) = params.mapping_id.or(theirs) {
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
    engine.require_mapping_reads(&controller)?;
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
    if chosen {
        engine.remember_mapping(actor.clone(), operator_id.as_deref(), &controller);
    }
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
    engine.ensure_identity_settled()?;
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
        // Its first vehicle: judged as a claim is.
        engine.require_mapping_reads(&controller)?;
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

/// Every hold must be one a pilot can make: 0.1 to 10 seconds.
/// The mapping's gripper buttons (S263), kept when the update names none: a
/// button either sends a command or holds the gripper, and only once.
fn require_actions(
    params: &MappingUpdateParams,
    current: &InputMapping,
) -> Result<Vec<ActionBinding>, ApiError> {
    let actions: Vec<ActionBinding> = params.actions.clone().map_or_else(
        || current.actions.clone(),
        |actions| actions.into_iter().map(Into::into).collect(),
    );
    if actions.iter().any(|action| {
        params
            .buttons
            .iter()
            .any(|b| action.matches(Some(b.button.into()), b.code))
            || actions
                .iter()
                .filter(|other| other.matches(Some(action.button), action.code))
                .count()
                > 1
    }) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A button either sends a command or is held for an action, once.",
        ));
    }
    Ok(actions)
}

fn require_holds(params: &MappingUpdateParams) -> Result<(), ApiError> {
    if params
        .buttons
        .iter()
        .filter_map(|b| b.hold_ms)
        .chain(params.positions.iter().filter_map(|p| p.hold_ms))
        .any(|ms| !HOLD_MS.contains(&ms))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A hold is from 0.1 to 10 seconds.",
        ));
    }
    Ok(())
}

/// Each RC channel is passed on once, on a channel ArduPilot reads as one.
fn require_rc_channels(params: &MappingUpdateParams) -> Result<(), ApiError> {
    let channels: std::collections::BTreeSet<_> =
        params.rc_channels.iter().map(|b| b.channel).collect();
    if channels.len() != params.rc_channels.len() || channels.iter().any(|c| !(5..=16).contains(c))
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "RC channels must be unique and between 5 and 16.",
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
    let mut unused: Vec<StickControl> = params.unused.iter().copied().map(Into::into).collect();
    unused.sort_unstable();
    unused.dedup();
    if (current.throttle_range != params.throttle_range.into()
        || sticks != [current.roll, current.pitch, current.yaw, current.throttle]
        || unused != current.unused
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
    require_rc_channels(&params)?;
    require_holds(&params)?;
    let actions = require_actions(&params, &current)?;
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
        unused,
        actions,
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
    /// Keeps on the operator the mapping they claimed a controller with.
    fn remember_mapping(
        &mut self,
        actor: Actor,
        operator_id: Option<&str>,
        controller: &Controller,
    ) {
        let Some(mut operator) = operator_id
            .and_then(|id| self.state.operators.get(id))
            .cloned()
        else {
            return;
        };
        if operator.choose_mapping(&controller.id, &controller.name, &controller.mapping_id) {
            operator.version += 1;
            self.commit_as(actor, Change::OperatorUpserted { operator });
        }
    }

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

    /// A controller is used with its sticks wherever they are (maintainer,
    /// 2026-10-29), but not by a mapping that reads nothing of some of its
    /// channels: it would fly on made-up values. Judged from its raw input
    /// now; one the host reads no raw input for is taken as it is.
    fn require_mapping_reads(&self, controller: &Controller) -> Result<(), ApiError> {
        if controller.id.starts_with("keyboard:") {
            return Ok(());
        }
        let inputs = self.system.controller_inputs();
        let raw = inputs.controllers.iter().find(|c| c.id == controller.id);
        let (Some(raw), Some(mapping)) = (raw, mapping(self, &controller.mapping_id)) else {
            return Ok(());
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
        let mut channels: Vec<_> = [
            ("roll", Some(StickControl::Roll), mapping.roll),
            ("pitch", Some(StickControl::Pitch), mapping.pitch),
            ("throttle", None, mapping.throttle),
            ("yaw", Some(StickControl::Yaw), mapping.yaw),
        ]
        .into_iter()
        .filter(|(_, control, _)| control.is_none_or(|c| !mapping.unused.contains(&c)))
        .map(|(name, _, binding)| (name, binding))
        .collect();
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
        Ok(())
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

    /// A claimed controller sends nothing while UXDuck starts its
    /// simulation: while it restarts, and while it takes off or is put back
    /// by itself, so the sticks cannot stop it arming. It drives the vehicle
    /// again as soon as that ends, wherever its sticks are, in any mode.
    fn controller_start_pause(&self, id: &str) -> Option<String> {
        use uxduck_core::domain::{
            link::{LaunchStage, LinkStatus},
            vehicle::Presence,
        };
        let controller = self.state.controllers.get(id)?;
        controller.claim.as_ref()?;
        let vehicle = self.vehicle(controller.target.as_deref()?)?;
        let link = self.state.links.get(&vehicle.link_id)?;
        let down = link.status != LinkStatus::Up || vehicle.presence != Presence::Online;
        if self.live.restarting.contains(&link.id) && down {
            let failed = link
                .launch
                .as_ref()
                .is_some_and(|l| l.stage == LaunchStage::Failed);
            return Some(if failed {
                "Controller paused: the simulation didn't start. Start it again, or release the controller.".into()
            } else {
                "Controller paused while the simulation restarts.".into()
            });
        }
        self.is_launching(&link.id)
            .then(|| format!("Controller paused until {} has started.", vehicle.name))
    }

    /// A controller's sticks through its mapping, and whether a button is
    /// held; none while any mapped input is missing or not finite.
    pub(crate) fn mapped_controller_input(
        &self,
        controller: &Controller,
    ) -> Option<(Sticks, bool)> {
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
        if mapping
            .stick_bindings()
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

    /// Called by both hosts' expiry beat: hands controllers back once their
    /// simulation has started, and by a take-off as it ends.
    pub(crate) fn resume_started_controllers(&mut self) {
        let ids: Vec<String> = self
            .state
            .controllers
            .values()
            .filter(|c| c.claim.is_some() && c.input_pause.is_some())
            .map(|c| c.id.clone())
            .collect();
        for id in ids {
            let pause = self.controller_start_pause(&id);
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
        let back: Vec<String> = self
            .live
            .restarting
            .iter()
            .filter(|id| self.restarted(id))
            .cloned()
            .collect();
        for id in back {
            self.live.restarting.remove(&id);
        }
    }

    /// Whether a simulator pressed to start again is up with its vehicle
    /// back, or is gone.
    fn restarted(&self, link_id: &str) -> bool {
        use uxduck_core::domain::{link::LinkStatus, vehicle::Presence};
        let Some(link) = self.state.links.get(link_id) else {
            return true;
        };
        link.status == LinkStatus::Up
            && self
                .state
                .vehicles
                .values()
                .any(|v| v.link_id == link_id && v.presence == Presence::Online)
    }

    /// Tells the data plane what a controller drives now, if anything.
    pub(crate) fn route(&mut self, controller_id: &str) {
        let pause = self.controller_start_pause(controller_id);
        self.set_controller_pause(controller_id, pause);
        let route = self
            .state
            .controllers
            .get(controller_id)
            .filter(|c| {
                c.connected
                    && c.claim.is_some()
                    && c.input_pause.is_none()
                    && !self.live.input_lost.contains_key(controller_id)
            })
            .and_then(|c| {
                let vehicle_id = c.target.clone()?;
                let pilot_input_method = self.pilot_input_method(&vehicle_id);
                Some(ControlRoute {
                    gripper: self.gripper_route(&vehicle_id, pilot_input_method),
                    vehicle_id,
                    mapping: mapping(self, &c.mapping_id)?,
                    pilot_input_method,
                })
            });
        if let Some(waiting) = self.live.position_sends.get_mut(controller_id) {
            waiting.next = None;
        }
        match &route {
            Some(route) => {
                self.live
                    .routed_gripper
                    .insert(controller_id.to_owned(), route.gripper.clone());
            }
            None => {
                self.live.routed_gripper.remove(controller_id);
            }
        }
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
        let was_interrupted = self.live.input_lost.remove(id);
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
        if was_interrupted.is_some() {
            self.controller_presence(id, &controller.name, false);
        }
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

    /// A connection's selected vehicle. Following controllers switch to it
    /// now, wherever their sticks are.
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
        // targets and RC mappings. A refused switch stays pending; the UI
        // offers an explicit retry which reports the reason.
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
    /// The page let its keyboard go because UXDuck left the front (S291):
    /// a warning, with its tone, while the vehicle is armed; else quietly.
    fn say_keyboard_unfocused(&mut self, connection: ConnectionId) {
        let Some(target) = self
            .live
            .keyboards
            .get(&connection)
            .and_then(|id| self.state.controllers.get(id))
            .and_then(|controller| controller.target.clone())
        else {
            return;
        };
        let armed = self.vehicle(&target).is_some_and(|v| v.armed);
        let name = self.target_name(&target);
        self.raise(Raise {
            severity: if armed {
                Severity::Warning
            } else {
                Severity::Info
            },
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle { id: target },
            title: format!("Keyboard control of {name} turned off: UXDuck isn't the app in front"),
            body: Some(if armed {
                format!("It no longer sends stick input. {name}'s own failsafe takes over.")
            } else {
                "Turn it on again in the Cockpit.".to_owned()
            }),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
    }

    fn stop_keyboard(&mut self, connection: ConnectionId) {
        if let Some(id) = self.live.keyboards.remove(&connection) {
            // Releasing a keyboard is not a physical device being unplugged.
            self.release(&id, connection, None);
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
            // Browser input belongs to this live page. It cannot keep or
            // inherit activation after its connection has actually closed.
            if id.starts_with("web-") || self.live.input_lost.contains_key(&id) {
                self.release(&id, connection, Some("its page closed"));
                continue;
            }
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
        self.resume_started_controllers();
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
                        "Open the vehicle's Cockpit tab and press Control with keyboard to enable it again.".into()
                    } else {
                        "To control a vehicle with it again, press Use in the vehicle's Cockpit.".into()
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
        let waiting_for_input = self.live.input_lost.contains_key(id);
        let plugged_in = self.state.controllers.get(id).is_none_or(|c| !c.connected);
        let fitting = if layout_known {
            InputMapping::mode_2().id
        } else {
            InputMapping::for_radio(name).id
        };
        let controller = match self.state.controllers.get(id).cloned() {
            Some(mut known) => {
                // The USB radio mapping was briefly every radio's, and reads
                // an ExpressLRS joystick's arm switch as its throttle. Not
                // once someone has set it up: their edits are theirs.
                let usb_radio = known.mapping_id == InputMapping::radio().id
                    && !self.state.input_mappings.contains_key(&known.mapping_id);
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
                // A connection notice precedes the complete input snapshot.
                // Keep a recovering controller paused until that snapshot.
                known.connected = !waiting_for_input;
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
                holding: None,
                pending_target: None,
                mapping_id: fitting,
                version: 1,
            },
        };
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
        if plugged_in && !waiting_for_input && !id.starts_with("keyboard:") {
            self.controller_presence(id, name, true);
        }
    }

    /// Reconnects update one notice per device, including old notices without a condition.
    fn controller_presence(&mut self, id: &str, name: &str, connected: bool) {
        let activated = self
            .state
            .controllers
            .get(id)
            .is_some_and(|c| c.claim.is_some());
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
                if activated {
                    "Input is paused. It will resume automatically when fresh readings return. Release the controller to keep it off.".to_owned()
                } else {
                    "The browser stopped reporting this controller. Check the connection and keep this tab visible. A busy browser can interrupt input.".to_owned()
                }
            } else {
                "Check its USB or wireless connection. Reconnect it before using it again.".to_owned()
            }
        });
        let severity = if connected {
            Severity::Info
        } else {
            Severity::Warning
        };
        // "Open Controllers" was clipped in the notice (maintainer, 2026-10-31).
        let actions = vec![NotificationAction {
            label: "Open".into(),
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
            if interrupted && activated {
                notice.title = format!("{name} input resumed");
                notice.body =
                    Some("Fresh controller input returned. Control resumed automatically.".into());
            } else if interrupted {
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
        self.live.input_lost.remove(id);
        // A hold ends with the controller.
        self.set_holding(id, None);
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

    /// Missing browser input pauses output, retaining an existing activation.
    /// The host clears its raw inputs and releases the vehicle's overrides first.
    pub fn controller_input_interrupted(&mut self, id: &str) {
        let Some(mut controller) = self
            .state
            .controllers
            .get(id)
            .cloned()
            .filter(|c| c.claim.is_some() && self.live.claimant.contains_key(id))
        else {
            self.controller_disconnected(id);
            return;
        };
        self.live
            .input_lost
            .insert(id.to_owned(), self.clock.now_us());
        controller.connected = false;
        controller.version += 1;
        self.commit_as(
            daemon(),
            Change::ControllerUpserted {
                controller: controller.clone(),
            },
        );
        self.links.route_controller(id, None);
        self.controller_presence(id, &controller.name, false);
    }

    /// A complete fresh browser report, after its raw inputs were installed.
    /// Only the claiming page may resume an activation. Explicit release has
    /// already removed it; an unclaimed controller simply becomes available.
    pub fn controller_input_recovered(
        &mut self,
        id: &str,
        connection: ConnectionId,
        read_at_us: u64,
    ) {
        let waiting = self.live.input_lost.get(id).copied();
        let Some(controller) = self.state.controllers.get(id) else {
            return;
        };
        if waiting.is_none() && controller.connected {
            return;
        }
        let mut controller = controller.clone();
        let sticks = if let Some(lost_at) = waiting {
            if read_at_us < lost_at || self.live.claimant.get(id) != Some(&connection) {
                return;
            }
            let Some((sticks, _)) = self.mapped_controller_input(&controller) else {
                return;
            };
            Some(sticks)
        } else {
            None
        };
        self.live.input_lost.remove(id);
        controller.connected = true;
        controller.version += 1;
        self.commit_as(
            daemon(),
            Change::ControllerUpserted {
                controller: controller.clone(),
            },
        );
        self.controller_presence(id, &controller.name, true);
        if let Some(sticks) = sticks {
            // Installing the route seeds held switches from this snapshot;
            // neither held buttons nor positions become new commands.
            self.route(id);
            self.controller_neutral(id, sticks.neutral());
        }
    }

    /// Whether a controller's sticks are centred, as the data plane sees
    /// them through its mapping. Nothing waits for it (maintainer,
    /// 2026-10-29); a centring retries a selection it could not follow yet,
    /// such as a vehicle another controller drove.
    pub fn controller_neutral(&mut self, id: &str, neutral: bool) {
        // Connected emits a provisional neutral value before Present has
        // installed the fresh axes. It must not switch a recovering target.
        if self.live.input_lost.contains_key(id) {
            return;
        }
        if neutral {
            self.follow_selection(id);
        }
    }

    /// The client a controller's commands go as, and its target, while a
    /// press or a move would send one: connected, routed, not paused.
    fn controller_sends(&self, id: &str) -> Option<(Controller, ConnectionId, String)> {
        let controller = self.state.controllers.get(id)?;
        if !controller.connected
            || self.live.input_lost.contains_key(id)
            || controller.input_pause.is_some()
        {
            return None;
        }
        let connection = self.live.claimant.get(id).copied()?;
        let target = controller.target.clone()?;
        Some((controller.clone(), connection, target))
    }

    /// A button or position bound with a hold went down. While it is held
    /// the controller says what it will send, for the app to show the hold
    /// filling; nothing is said where its command would not go.
    pub fn controller_holding(&mut self, id: &str, command: Command, hold_ms: u32) {
        if self.controller_sends(id).is_some() {
            self.set_holding(id, Some(Hold { command, hold_ms }));
        }
    }

    /// The hold under way ended, let go early or complete.
    pub fn controller_let_go(&mut self, id: &str) {
        self.set_holding(id, None);
    }

    fn set_holding(&mut self, id: &str, holding: Option<Hold>) {
        let Some(mut controller) = self.state.controllers.get(id).cloned() else {
            return;
        };
        if controller.holding == holding {
            return;
        }
        controller.holding = holding;
        controller.version += 1;
        self.commit_as(daemon(), Change::ControllerUpserted { controller });
    }

    /// A button went down: its command goes to the target, as the claiming
    /// client, through the ordinary command path.
    pub fn controller_pressed(
        &mut self,
        id: &str,
        button: Option<GamepadButton>,
        code: Option<u32>,
    ) {
        let Some((controller, connection, target)) = self.controller_sends(id) else {
            return;
        };
        let Some(binding) =
            mapping(self, &controller.mapping_id).and_then(|m| m.button(button, code).cloned())
        else {
            return;
        };
        self.send_for_controller(&controller, connection, target, &binding.command);
    }

    /// An axis settled in a position: its command goes to the target, as
    /// for a button, once the controller's last position command is
    /// answered (`PositionSend`). The position is found in the
    /// controller's mapping now, so one the data plane held from an older
    /// version sends nothing.
    pub fn controller_positioned(&mut self, id: &str, position: &PositionBinding) {
        let Some((controller, connection, target)) = self.controller_sends(id) else {
            return;
        };
        let Some(found) = mapping(self, &controller.mapping_id)
            .and_then(|m| m.positions.into_iter().find(|p| p == position))
        else {
            return;
        };
        if let Some(waiting) = self.live.position_sends.get_mut(id)
            && self
                .state
                .receipts
                .get(&waiting.receipt_id)
                .is_some_and(|r| r.status == ReceiptStatus::Sent)
        {
            waiting.next = Some(found);
            return;
        }
        self.live.position_sends.remove(id);
        if let Some(receipt_id) =
            self.send_for_controller(&controller, connection, target, &found.command)
        {
            self.live.position_sends.insert(
                id.to_owned(),
                PositionSend {
                    receipt_id,
                    next: None,
                },
            );
        }
    }

    /// A command ended: a position settled while it was on its way goes
    /// now, through every check again.
    pub(crate) fn position_answered(&mut self, receipt_id: &str) {
        let Some(id) = self
            .live
            .position_sends
            .iter()
            .find(|(_, waiting)| waiting.receipt_id == receipt_id)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let next = self.live.position_sends.remove(&id).and_then(|w| w.next);
        if let Some(position) = next {
            self.controller_positioned(&id, &position);
        }
    }

    /// Sends as the claiming client: the receipt of a command that went,
    /// still to be answered. A refusal that is no receipt, such as one
    /// asking to confirm, is said in a notice: the pilot pressed a button
    /// and must hear why nothing happened.
    fn send_for_controller(
        &mut self,
        controller: &Controller,
        connection: ConnectionId,
        target: String,
        command: &Command,
    ) -> Option<String> {
        let params = CommandSendParams {
            vehicle_id: target.clone(),
            command: command.into(),
            confirm: false,
            force: false,
            override_preflight: Vec::new(),
        };
        let error = match crate::commands::send_checked(
            self,
            connection,
            params,
            &[Capability::Control],
            true,
        ) {
            Ok((_, sent)) => return sent,
            Err(error) => error,
        };
        let name = self.target_name(&target);
        // Held back here, never refused by the vehicle: that is a receipt.
        let body = if error.data.kind == ErrorKind::ConfirmationRequired {
            format!(
                "UXDuck can't hear {name} right now, so it asks before sending this. Send it from the Cockpit to confirm."
            )
        } else {
            error.message
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle { id: target },
            title: format!(
                "UXDuck held back {} from {} to {name}",
                command.label(),
                controller.name
            ),
            body: Some(body),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
        None
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

impl Engine {
    pub(crate) fn ensure_identity_settled(&self) -> Result<(), ApiError> {
        if !self.awaiting_identity.is_empty() {
            return Err(ApiError::new(
                ErrorKind::Busy,
                "Wait for the operator or account change to finish before claiming control.",
            ));
        }
        Ok(())
    }

    /// A profile change never transfers an existing control claim, including
    /// one held through a reload grace period or another window of that actor.
    /// It never touches the account either (ADR-0033, one account per
    /// computer): only control is handed over. Becoming the operator a claim
    /// already belongs to, as a reloaded window does after taking its
    /// controllers back, hands nothing over.
    pub(crate) fn ensure_operator_handover(
        &self,
        connection: ConnectionId,
        next: Option<&str>,
    ) -> Result<(), ApiError> {
        let actor = self.actor(connection);
        if actor.operator_id.as_deref() == next {
            return Ok(());
        }
        let local_operator = self
            .connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
            .is_some_and(crate::Admitted::is_operator);
        if self.state.controllers.values().any(|controller| {
            controller.claim.as_ref().is_some_and(|claim| {
                claim.operator_id.as_deref() != next
                    && (self.live.claimant.get(&controller.id) == Some(&connection)
                        || (local_operator
                            && actor.operator_id.is_some()
                            && claim.operator_id == actor.operator_id))
            })
        }) {
            return Err(ApiError::new(
                ErrorKind::ConfirmationRequired,
                "Release your controllers before switching operators. Telemetry and recordings will continue.",
            ));
        }
        Ok(())
    }

    /// Signing in or out changes the account every operator here acts
    /// under, so an operator's window asks for every controller first.
    pub(crate) fn ensure_account_handover(&self, connection: ConnectionId) -> Result<(), ApiError> {
        self.ensure_operator_handover(connection, None)?;
        if self.actor(connection).operator_id.is_some()
            && self.account_identity.signed_in
            && self.state.controllers.values().any(|c| c.claim.is_some())
        {
            return Err(ApiError::new(
                ErrorKind::ConfirmationRequired,
                "Release the controllers before changing account sessions. Telemetry and recordings will continue.",
            ));
        }
        Ok(())
    }
}

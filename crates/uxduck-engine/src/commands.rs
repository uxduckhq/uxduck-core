//! Commands, receipts and notifications (ADR-0005 command path and
//! notifications; ADR-0004 offline behaviour). `command/send` journals a
//! receipt, hands the command to the data plane, and answers when the
//! vehicle has acknowledged it or every retry has timed out.

use serde_json::{Value, json};
use uxduck_contracts::dto::command::{
    CommandInfo, CommandListResult, CommandSendParams, ModeFamily, ModeInfo, ModeKind,
    ModeListParams, ModeListResult, Receipt as ReceiptDto,
};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::terrain::TerrainAtParams;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{
    Command, GoToHeading, MAV_CMD_DO_PAUSE_CONTINUE, MAV_CMD_DO_SET_MISSION_CURRENT,
    MAV_CMD_MISSION_START, MAV_CMD_SCRIPTING, OfflineBehaviour, PlaceAmsl, Receipt, ReceiptStatus,
    RefusalReason,
};
use uxduck_core::domain::modes::{self, AvailableMode, Family, is_vtol};
use uxduck_core::domain::notification::{
    Action, Notification, NotificationAction, NotificationKind, NotificationState, Severity,
    Subject,
};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::preflight::PreflightOverride;
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::domain::telemetry_layout::VehicleClass;
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};
use uxduck_terrain::lookup::Ground;

use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;
use crate::{ConnectionId, Engine};

/// How a command ended, as the data plane reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Rejected(String),
    Timeout,
    NotSent(String),
}

/// What to raise.
pub(crate) struct Raise {
    pub severity: Severity,
    pub kind: NotificationKind,
    pub subject: Subject,
    pub title: String,
    pub body: Option<String>,
    pub actions: Vec<NotificationAction>,
    pub condition: Option<String>,
    /// Offers "Report this" with this code, for when UXDuck itself may have
    /// gone wrong; `None` for news, questions and the vehicle's own say.
    pub report: Option<String>,
}

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
        identity: None,
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::CommandSend(params) => {
            send_checked(engine, connection, params, granted, operator).map(|(value, _)| value)
        }
        ClientRequest::CommandList(IdParams { id }) => {
            require(Capability::Read)?;
            to_value(&command_list(engine, &id)?)
        }
        ClientRequest::CommandModes(ModeListParams { vehicle_id }) => {
            require(Capability::Read)?;
            to_value(&mode_list(engine, vehicle_id.as_deref()))
        }
        ClientRequest::NotificationAcknowledge(IdParams { id }) => {
            require(Capability::Read)?;
            set_state(engine, connection, &id, NotificationState::Acknowledged)
        }
        ClientRequest::NotificationDismiss(IdParams { id }) => {
            require(Capability::Read)?;
            set_state(engine, connection, &id, NotificationState::Resolved)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a command method")),
    }
}

/// `command/send`: its checks, then the send. With what it answers, the
/// receipt of a command that went to the vehicle, still to be answered.
pub(crate) fn send_checked(
    engine: &mut Engine,
    connection: ConnectionId,
    params: CommandSendParams,
    granted: &[Capability],
    operator: bool,
) -> Result<(Value, Option<String>), ApiError> {
    if !granted.contains(&Capability::Control) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Control.denied_message(),
        ));
    }
    if params.force && !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator may force a command",
        ));
    }
    // Arming past failing preflight checks is a person's (ADR-0038
    // K4): an agent's or a plugin's arming is put to an operator.
    if !params.override_preflight.is_empty() && !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator may arm past failing preflight checks",
        ));
    }
    let params = home_under_an_armed_vehicle(engine, params)?;
    if let Some(vehicle) = engine.vehicle(&params.vehicle_id)
        && let Some(why) = unsafe_place(&params.command.clone().into(), flies(vehicle))
    {
        return Err(ApiError::new(ErrorKind::InvalidValue, why));
    }
    send(engine, connection, params, operator)
}

/// Home where an armed vehicle is goes on the ground under it, at UXDuck's
/// terrain height there: ArduPilot's own "where it is now" takes its height
/// in the air too and locks it, so it would land, and arm next time, below
/// home. A disarmed vehicle is on the ground, so it keeps its own.
fn home_under_an_armed_vehicle(
    engine: &mut Engine,
    mut params: CommandSendParams,
) -> Result<CommandSendParams, ApiError> {
    let Some(vehicle) = engine.vehicle(&params.vehicle_id) else {
        return Ok(params);
    };
    if !vehicle.armed
        || !matches!(
            Command::from(params.command.clone()),
            Command::SetHome { place: None }
        )
    {
        return Ok(params);
    }
    let name = vehicle.name.clone();
    let position = engine.telemetry(&params.vehicle_id).and_then(|telemetry| {
        telemetry
            .positions
            .iter()
            .filter(|p| {
                p.source == "fused"
                    && p.latitude_deg.is_finite()
                    && p.longitude_deg.is_finite()
                    && (p.latitude_deg != 0.0 || p.longitude_deg != 0.0)
            })
            .max_by_key(|p| p.time_us)
            .map(|p| (p.latitude_deg, p.longitude_deg))
    });
    let Some((latitude_deg, longitude_deg)) = position else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{name} hasn't said where it is, so home can't go under it. Pick home on the map"
            ),
        ));
    };
    let dataset = engine.terrain_dataset_of(&params.vehicle_id);
    let ground = engine
        .terrain_answer(
            dataset,
            &TerrainAtParams {
                latitude_deg,
                longitude_deg,
                dataset: None,
            },
        )
        .and_then(|answer| match answer.ground {
            Ok(Ground::Height(height)) => Some(height),
            Ok(Ground::Sea) => Some(0.0),
            _ => None,
        });
    engine.terrain_flush();
    let Some(altitude_amsl_m) = ground else {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "UXDuck has no ground height under {name} yet, so home can't go on the ground there. Try again in a moment, or pick home on the map"
            ),
        ));
    };
    params.command = (&Command::SetHome {
        place: Some(PlaceAmsl {
            latitude_deg,
            longitude_deg,
            altitude_amsl_m,
        }),
    })
        .into();
    Ok(params)
}

/// `DO_CHANGE_SPEED`, offered with no speed; the sender fills param 2 in. A
/// plane's airspeed (kind 0); the ground speed of the rest that go somewhere
/// in GUIDED and AUTO, as Go to does (kind 1). Throttle unchanged (-1).
fn change_speed(family: Option<Family>) -> Option<Command> {
    let kind = match family? {
        Family::Plane => 0.0,
        Family::Copter | Family::Rover | Family::Sub | Family::Px4 => 1.0,
        Family::Blimp => return None,
    };
    Some(Command::Long {
        command: 178,
        params: vec![kind, 0.0, -1.0],
    })
}

/// Home, at the vehicle or a place the sender fills in; the EKF origin,
/// advanced, offered at 0, 0 for its risk and class. PX4 takes its origin
/// another way, so only ArduPilot is offered one.
fn home_and_origin(family: Option<Family>) -> Vec<Command> {
    let Some(family) = family else {
        return Vec::new();
    };
    let mut commands = vec![Command::SetHome { place: None }];
    if family != Family::Px4 {
        commands.push(Command::SetEkfOrigin {
            place: PlaceAmsl {
                latitude_deg: 0.0,
                longitude_deg: 0.0,
                altitude_amsl_m: 0.0,
            },
        });
    }
    commands
}

/// Restart the mission, and going to one of its items, offered at item 1
/// for its risk and class; the sender picks the item (S339). Every
/// autopilot UXDuck knows flies a mission; ArduPilot's first item is 1,
/// after home, and PX4's is 0. Starting it is offered from where it is;
/// the sender may name a step. Pausing and continuing it (S340) only
/// `ArduCopter` takes: Plane, Rover and PX4 hold by changing mode.
fn mission_items(family: Option<Family>) -> Vec<Command> {
    let Some(family) = family else {
        return Vec::new();
    };
    let first = u16::from(family != Family::Px4);
    let mut commands = vec![
        Command::restart_mission(first),
        Command::go_to_mission_item(first),
    ];
    // Flying it from a step (S341): Sub and Blimp have no `MISSION_START`.
    if matches!(
        family,
        Family::Copter | Family::Plane | Family::Rover | Family::Px4
    ) {
        commands.push(Command::start_mission(None));
    }
    if family == Family::Copter {
        commands.extend([Command::pause_mission(), Command::continue_mission()]);
    }
    // Landing via the mission (S345): Copter, Plane and PX4 jump to the
    // nearest DO_LAND_START; Rover, Sub and Blimp have no landing sequence.
    if matches!(family, Family::Copter | Family::Plane | Family::Px4) {
        commands.push(Command::land_via_the_mission());
    }
    commands
}

/// The fastest speed sent: well over any small vehicle's, to catch a slip
/// of units (km/h typed as m/s) rather than to limit a fast one.
const MAX_SPEED_MPS: f32 = 150.0;

/// Something that flies: a multicopter, plane, VTOL or blimp.
fn flies(vehicle: &Vehicle) -> bool {
    matches!(
        Family::of(&vehicle.vehicle_type),
        Some(Family::Copter | Family::Plane | Family::Blimp)
    )
}

/// A place no client may send a vehicle to, from the app, the CLI, the
/// agent or a script alike: off the map, the 0,0 a missing position reads
/// as, or, for something that flies, at or below the ground its height is
/// measured from (a rover, boat or submarine keeps its own height, which
/// may be 0 or below). None when it may go.
fn unsafe_place(command: &Command, flies: bool) -> Option<String> {
    // DO_CHANGE_SPEED: an airspeed or a ground speed, never "as fast as
    // it can" (-1) or nothing, which an autopilot may read as a stop.
    if let Command::Long {
        command: 178,
        params,
    } = command
    {
        let kind = params.first().copied().unwrap_or(f32::NAN);
        let speed = params.get(1).copied().unwrap_or(f32::NAN);
        if ![0.0, 1.0].contains(&kind) {
            return Some("a speed needs to say whether it is airspeed or ground speed".into());
        }
        return (!speed.is_finite() || speed <= 0.0 || speed > MAX_SPEED_MPS)
            .then(|| format!("a speed of {speed} m/s isn't one it can be sent"));
    }
    // ArduPlane's native height command uses param1 and a MAV_FRAME in
    // param2. Apply the same ground guard as Go to, for every caller.
    if let Command::Long {
        command: 186,
        params,
    } = command
    {
        let height = params.first().copied().unwrap_or(f32::NAN);
        let frame = params.get(1).copied().unwrap_or(f32::NAN);
        if !height.is_finite() || ![0.0, 3.0, 5.0, 6.0, 10.0, 11.0].contains(&frame) {
            return Some("a height needs a supported altitude reference".into());
        }
        return (flies && [3.0, 6.0, 10.0, 11.0].contains(&frame) && height <= 0.0)
            .then(|| format!("a height of {height} m would put it on or under the ground"));
    }
    if let Command::SetHome { place: Some(place) } | Command::SetEkfOrigin { place } = command {
        return unsafe_amsl_place(place);
    }
    let Command::GoTo {
        latitude_deg,
        longitude_deg,
        altitude_m,
        altitude_frame,
        ..
    } = command
    else {
        return None;
    };
    if !(latitude_deg.is_finite() && longitude_deg.is_finite() && altitude_m.is_finite())
        || latitude_deg.abs() > 90.0
        || longitude_deg.abs() > 180.0
    {
        return Some("that place is not on the map".into());
    }
    if *latitude_deg == 0.0 && *longitude_deg == 0.0 {
        return Some("0, 0 is where a missing position reads. Choose the place again".into());
    }
    let from_ground = matches!(
        altitude_frame,
        AltitudeFrame::AboveHome | AltitudeFrame::AboveGround
    );
    (flies && from_ground && *altitude_m <= 0.0)
        .then(|| format!("a height of {altitude_m} m would put it on or under the ground"))
}

/// The lowest and highest ground on Earth, with room: a height outside
/// them is a slip, of feet for metres or a height above home for one above
/// the sea.
const AMSL_RANGE_M: std::ops::RangeInclusive<f64> = -500.0..=9_000.0;

/// Home or the EKF origin somewhere no client may put it.
fn unsafe_amsl_place(place: &PlaceAmsl) -> Option<String> {
    let PlaceAmsl {
        latitude_deg,
        longitude_deg,
        altitude_amsl_m,
    } = *place;
    if !(latitude_deg.is_finite() && longitude_deg.is_finite())
        || latitude_deg.abs() > 90.0
        || longitude_deg.abs() > 180.0
    {
        return Some("that place is not on the map".into());
    }
    if latitude_deg == 0.0 && longitude_deg == 0.0 {
        return Some("0, 0 is where a missing position reads. Choose the place again".into());
    }
    (!AMSL_RANGE_M.contains(&altitude_amsl_m)).then(|| {
        format!("{altitude_amsl_m} m above sea level is not a height on the ground anywhere")
    })
}

fn send(
    engine: &mut Engine,
    connection: ConnectionId,
    params: CommandSendParams,
    operator: bool,
) -> Result<(Value, Option<String>), ApiError> {
    let vehicle = engine.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no vehicle {}", params.vehicle_id),
        )
    })?;
    // A toggle goes, and is receipted, as what the vehicle needs.
    let command = Command::from(params.command).resolved(vehicle.armed);
    // A disarmed vehicle's take-off goes after the steps it needs, the
    // first of them now (`TakeOff`).
    let mut steps = take_off_steps(engine, &vehicle, &command).unwrap_or_default();
    let arms = command.arms_on_the_ground() || !steps.is_empty();
    let asked = command.label();
    let command = if steps.is_empty() {
        command
    } else {
        steps.remove(0)
    };
    let actor = engine.actor(connection);
    let now = engine.clock.now_us();
    let mut receipt = Receipt {
        id: format!("rc{}", &engine.random.token()[..16]),
        vehicle_id: vehicle.id.clone(),
        command: command.clone(),
        status: ReceiptStatus::Sent,
        detail: None,
        reason: None,
        preflight: Vec::new(),
        preflight_override: None,
        sent_without_recent_heartbeat: vehicle.presence != Presence::Online,
        forced: params.force,
        attempts: 0,
        client_id: actor.client_id.clone(),
        operator_id: actor.operator_id.clone(),
        sent_by: engine.sender(connection),
        remote: None,
        created_at_us: now,
        completed_at_us: None,
        version: 1,
    };

    let not_sent = |receipt: &mut Receipt, why: String| {
        receipt.status = ReceiptStatus::NotSent;
        receipt.detail = Some(why);
        receipt.completed_at_us = Some(now);
    };
    // A replay vehicle is not there to send to (ADR-0039 R3).
    if vehicle.replay {
        not_sent(&mut receipt, crate::replay::NOTHING_SENT.into());
        receipt.reason = Some(RefusalReason::Replay);
    }
    // Its link is gone, read-only, or a paused simulation's (`sim/pause`).
    if receipt.status == ReceiptStatus::Sent
        && let Some(why) = engine.not_writable(&vehicle.link_id)
    {
        not_sent(&mut receipt, why);
    }
    offline_check(
        &command,
        &vehicle,
        (params.confirm, params.force),
        &mut receipt,
    )?;
    // A real vehicle arms only for an operator who agreed to the terms
    // (ADR-0047), asked before the preflight checks.
    engine.terms_gate(&vehicle, arms, &mut receipt);
    // The preflight gate, for every client (S192), whichever step arms.
    preflight_gate(
        engine,
        &vehicle,
        (&params.override_preflight, operator),
        (arms, &asked),
        &mut receipt,
    );
    let result = ReceiptDto::from(&receipt);
    let id = receipt.id.clone();
    let sent = receipt.status == ReceiptStatus::Sent;
    let status = receipt.status;
    engine.commit_as(actor.clone(), Change::ReceiptUpserted { receipt });
    if !sent {
        if !vehicle.replay {
            engine.track_command(&command, status);
        }
        return Ok((to_value(&result)?, None));
    }
    engine.plan_run_command_sent(connection, &vehicle, &command, &id);
    engine.links.command(CommandOrder {
        receipt_id: id.clone(),
        vehicle_id: vehicle.id,
        command,
        force: params.force,
    });
    // Answered when the command ends (`command_result`).
    if let Some(waiting) = engine.replying_to.take() {
        engine.awaiting.insert(id.clone(), waiting);
    }
    if !steps.is_empty() {
        engine
            .take_offs
            .insert(id.clone(), TakeOff { actor, rest: steps });
    }
    Ok((Value::Null, Some(id)))
}

/// A command for a vehicle UXDuck can't hear: sent, confirmed first, or
/// not sent, by the command's offline behaviour (ADR-0005).
fn offline_check(
    command: &Command,
    vehicle: &Vehicle,
    (confirm, force): (bool, bool),
    receipt: &mut Receipt,
) -> Result<(), ApiError> {
    if receipt.status != ReceiptStatus::Sent || vehicle.presence == Presence::Online {
        return Ok(());
    }
    let presence = format!("{:?}", vehicle.presence).to_lowercase();
    match command.offline_behaviour() {
        OfflineBehaviour::Allow => {}
        OfflineBehaviour::Warn if confirm || force => {}
        OfflineBehaviour::Warn => {
            return Err(ApiError::new(
                ErrorKind::ConfirmationRequired,
                format!(
                    "{} needs confirming: {} is {presence}",
                    command.label(),
                    vehicle.name
                ),
            ));
        }
        OfflineBehaviour::Block if force => {}
        OfflineBehaviour::Block => {
            receipt.status = ReceiptStatus::NotSent;
            receipt.detail = Some(format!(
                "Can't send {} while UXDuck can't hear {}.",
                command.label(),
                vehicle.name
            ));
            receipt.completed_at_us = Some(receipt.created_at_us);
        }
    }
    Ok(())
}

/// Holds a command that arms on the ground while a blocking preflight check
/// does not pass: `not-sent` with reason `preflight`, listing them; or sent
/// past them by an operator naming every one, or by an approval that listed
/// them (ADR-0038 K3, K4).
fn preflight_gate(
    engine: &Engine,
    vehicle: &Vehicle,
    (named_ids, operator): (&[String], bool),
    (arms, asked): (bool, &str),
    receipt: &mut Receipt,
) {
    if receipt.status != ReceiptStatus::Sent || vehicle.armed || !arms {
        return;
    }
    let gating = engine.preflight_gating(&vehicle.id);
    if gating.is_empty() {
        return;
    }
    let named = |ids: &[String]| gating.iter().all(|c| ids.contains(&c.id));
    let by = if operator && named(named_ids) {
        Some(PreflightOverride::Operator)
    } else {
        engine
            .approving_preflight
            .as_ref()
            .filter(|(_, listed)| named(listed))
            .map(|(approval_id, _)| PreflightOverride::Approval {
                approval_id: approval_id.clone(),
            })
    };
    receipt.preflight.clone_from(&gating);
    if by.is_some() {
        receipt.preflight_override = by;
        return;
    }
    receipt.status = ReceiptStatus::NotSent;
    receipt.detail = Some(crate::preflight::refusal(
        asked,
        &vehicle.name,
        &gating,
        operator,
    ));
    receipt.completed_at_us = Some(receipt.created_at_us);
    receipt.reason = Some(RefusalReason::Preflight);
}

/// The rest of a take-off from the ground (QA 2026-10-24), sent once the
/// command before it is accepted: each its own receipt from the same
/// sender, so the history says what was done. The first refusal ends it,
/// and that command's receipt answers the request.
pub(crate) struct TakeOff {
    actor: Actor,
    rest: Vec<Command>,
}

/// What a take-off asked of a disarmed vehicle sends: `ArduPilot` lifts a
/// copter or a quadplane off only armed in GUIDED, so it is switched to
/// GUIDED and armed first, and PX4 is armed first. GUIDED climbs where it
/// stands and holds; the mission stays unstarted. None when the take-off
/// goes alone: the vehicle is armed (in the air, or on the ground in
/// another mode, which `take_off_refusal` explains), not heard, or does
/// not lift off straight up.
fn take_off_steps(engine: &Engine, vehicle: &Vehicle, command: &Command) -> Option<Vec<Command>> {
    if command.starts_mission() {
        return mission_start_steps(vehicle, command);
    }
    if !command.is_take_off()
        || vehicle.armed
        || vehicle.presence != Presence::Online
        || !lifts_off(engine, vehicle)
    {
        return None;
    }
    let ardupilot = matches!(
        Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type),
        Some(Family::Copter | Family::Plane)
    );
    let guided = vehicle
        .mode
        .as_deref()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("GUIDED"));
    let mut steps = Vec::new();
    if ardupilot && !guided {
        steps.push(Command::SetMode {
            mode: "GUIDED".into(),
        });
    }
    steps.push(Command::Arm);
    steps.push(command.clone());
    Some(steps)
}

/// What a mission's start sends (S341). ArduPilot refuses `MISSION_START`
/// with a first item, so it is made current first and the start goes as
/// `[0, 0]`; PX4 takes the item itself. Disarmed, the vehicle is armed
/// before the start, as Mission Planner's Start Mission does: from GUIDED
/// for a copter in AUTO, where `ArduCopter` refuses to arm. None when
/// the command goes as it is.
fn mission_start_steps(vehicle: &Vehicle, command: &Command) -> Option<Vec<Command>> {
    let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type)?;
    let ardupilot = family != Family::Px4;
    let mut steps = Vec::new();
    if ardupilot && let Some(seq) = command.mission_start_item().filter(|seq| *seq > 0) {
        steps.push(Command::go_to_mission_item(seq));
    }
    if !vehicle.armed && vehicle.presence == Presence::Online {
        let auto = vehicle
            .mode
            .as_deref()
            .is_some_and(|mode| mode.eq_ignore_ascii_case("AUTO"));
        if family == Family::Copter && auto {
            steps.push(Command::SetMode {
                mode: "GUIDED".into(),
            });
        }
        steps.push(Command::Arm);
    }
    let start = if ardupilot {
        Command::Long {
            command: MAV_CMD_MISSION_START,
            params: vec![0.0, 0.0],
        }
    } else {
        command.clone()
    };
    if steps.is_empty() && start == *command {
        return None;
    }
    steps.push(start);
    Some(steps)
}

/// Whether the vehicle is offered Take off: a copter, PX4's multicopters
/// and VTOLs, and a quadplane, which `ArduPlane` lifts straight up in
/// GUIDED once armed (maintainer, 2026-10-07: a VTOL on the ground had no
/// way up). A plane that can't hover takes off through a mission.
fn lifts_off(engine: &Engine, vehicle: &Vehicle) -> bool {
    let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type);
    match family {
        Some(Family::Copter) => true,
        Some(Family::Px4) => {
            matches!(Family::of(&vehicle.vehicle_type), Some(Family::Copter))
                || vehicle.vehicle_type.starts_with("Vtol")
        }
        Some(Family::Plane) => is_vtol(mode_airframe(
            engine,
            vehicle,
            family,
            engine.listed_modes(vehicle),
        )),
        _ => false,
    }
}

/// The height "Take off" climbs to, above home.
const TAKE_OFF_HEIGHT_M: f32 = 10.0;

impl Engine {
    /// A vehicle listed its own modes (`AVAILABLE_MODES`): the command list
    /// offers those from now on, under the table's names where it knows
    /// them. Kept under the vehicle's lasting id, whichever id the session
    /// heard it by.
    pub fn vehicle_modes_listed(&mut self, vehicle_id: &str, modes: Vec<AvailableMode>) {
        let id = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |vehicle| vehicle.id.clone());
        self.available_modes.insert(id, modes);
    }

    /// The modes a vehicle listed itself, if it has.
    fn listed_modes(&self, vehicle: &Vehicle) -> Option<&[AvailableMode]> {
        std::iter::once(&vehicle.id)
            .chain(vehicle.aliases.iter())
            .find_map(|id| self.available_modes.get(id))
            .map(Vec::as_slice)
    }
}

/// The airframe a vehicle's modes are offered for: its heartbeat's type,
/// or a quadplane's for a plane that reports fixed wing but has `Q_ENABLE`
/// set or lists hover modes itself. `ArduPlane` reports fixed wing for every
/// quadplane, real or simulated, while `Q_MAV_TYPE` is 0, its default
/// (`QuadPlane::get_mav_type`; maintainer, 2026-10-13: QLOITER and QLAND
/// were missing).
fn mode_airframe<'a>(
    engine: &Engine,
    vehicle: &'a Vehicle,
    family: Option<Family>,
    listed: Option<&[AvailableMode]>,
) -> &'a str {
    if family != Some(Family::Plane) || is_vtol(&vehicle.vehicle_type) {
        return &vehicle.vehicle_type;
    }
    let enabled = engine
        .state
        .parameters
        .get(&parameter_id(&vehicle.id, "Q_ENABLE"))
        .is_some_and(|p| p.value >= 1.0);
    let hovers = listed.is_some_and(|listed| {
        listed.iter().any(|m| {
            m.selectable
                && Family::Plane
                    .name(m.number)
                    .is_some_and(|name| name.starts_with('Q'))
        })
    });
    if enabled || hovers {
        "VtolQuadrotor"
    } else {
        &vehicle.vehicle_type
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the commands offered, in the menu's order"
)]
fn command_list(engine: &Engine, vehicle_id: &str) -> Result<CommandListResult, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}")))?;
    let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type);
    let mut commands = vec![Command::Arm, Command::Disarm];
    // The modes of the second, longer group of the menu.
    let mut advanced: Vec<String> = Vec::new();
    let listed = engine.listed_modes(vehicle);
    let airframe = mode_airframe(engine, vehicle, family, listed);
    if let Some(family) = family {
        // The vehicle's own list when it gave one, else the table.
        let offered: Vec<(String, bool)> = match listed {
            Some(listed) => family.offered_from(Some(airframe), listed),
            None => family
                .offered(Some(airframe))
                .into_iter()
                .map(|(mode, more)| (mode.to_owned(), more))
                .collect(),
        };
        for (mode, more) in offered {
            if more {
                advanced.push(mode.clone());
            }
            commands.push(Command::SetMode { mode });
        }
        // The mode it is in, when that is not offered (INITIALISING, or a
        // mode its airframe is not thought to have), so it is described
        // and can be chosen again.
        if let Some(mode) = &vehicle.mode
            && (family.number(mode).is_some()
                || listed.is_some_and(|l| Family::listed_number(l, mode).is_some()))
            && !commands
                .iter()
                .any(|c| matches!(c, Command::SetMode { mode: m } if m.eq_ignore_ascii_case(mode)))
        {
            commands.push(Command::SetMode { mode: mode.clone() });
            advanced.push(mode.clone());
        }
    }
    if lifts_off(engine, vehicle) {
        commands.push(Command::take_off(TAKE_OFF_HEIGHT_M));
    }
    // Go to: offered with no place, for its risk and
    // class; the sender fills the place in. Copter, Plane, Rover and Sub
    // apply DO_REPOSITION in GUIDED; PX4 holds there. Blimp acknowledges
    // it without setting a destination, so it is not offered.
    if matches!(
        family,
        Some(Family::Copter | Family::Plane | Family::Rover | Family::Sub | Family::Px4)
    ) {
        commands.push(Command::GoTo {
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            altitude_m: 0.0,
            altitude_frame: AltitudeFrame::AboveHome,
            heading: (family == Some(Family::Sub)).then_some(GoToHeading::FaceDestination),
            // Presence advertises support to interfaces paired with an older daemon.
            // This is an example radius, like the command's example position.
            loiter_radius_m: (family == Some(Family::Plane)).then_some(60.0),
        });
    }
    // Plane preserves next_WP_loc's horizontal destination itself. Copter
    // does not implement DO_CHANGE_ALTITUDE and needs a full Go to instead.
    if family == Some(Family::Plane) {
        commands.push(Command::Long {
            command: 186,
            params: vec![0.0, 3.0],
        });
    }
    commands.extend(change_speed(family));
    commands.push(Command::Rtl);
    // A PX4 rover, boat or submarine has nowhere to land.
    let keeps_down = VehicleClass::of(&vehicle.vehicle_type).never_flies();
    if matches!(family, Some(Family::Copter | Family::Plane | Family::Blimp))
        || (family == Some(Family::Px4) && !keeps_down)
    {
        commands.push(Command::Land);
    }
    commands.extend(home_and_origin(family));
    commands.extend(mission_items(family));
    commands.push(Command::Reboot);
    commands.extend(restart_scripts(engine, vehicle, family));
    Ok(CommandListResult {
        commands: commands
            .iter()
            .map(|command| CommandInfo {
                command: command.into(),
                label: command.label(),
                description: command_description(command, family, &vehicle.vehicle_type),
                advanced: matches!(command, Command::SetEkfOrigin { .. })
                    || matches!(command, Command::SetMode { mode } if advanced.contains(mode)),
                mode_kind: match (command, family) {
                    (Command::SetMode { mode }, Some(family)) => {
                        family.kind(mode).map(|kind| match kind {
                            modes::ModeKind::Automatic => ModeKind::Automatic,
                            modes::ModeKind::Assisted => ModeKind::Assisted,
                            modes::ModeKind::Manual => ModeKind::Manual,
                        })
                    }
                    _ => None,
                },
                without_gps: matches!(
                    (command, family),
                    (Command::SetMode { mode }, Some(family)) if family.without_gps(mode)
                ),
                offline_behaviour: command.offline_behaviour().into(),
                risk: command.risk(false, false).into(),
                risk_in_flight: command.risk(true, false).into(),
                forced_risk: matches!(command, Command::Arm | Command::Disarm)
                    .then(|| command.risk(false, true).into()),
                class: command.class().into(),
                max_age_ms: command.max_age_ms(),
            })
            .collect(),
        flying_above_home_m: uxduck_core::domain::command::FLYING_ABOVE_HOME_M,
    })
}

/// Restart scripts, for an ArduPilot vehicle with scripting on (S259,
/// ADR-0027 F16).
fn restart_scripts(engine: &Engine, vehicle: &Vehicle, family: Option<Family>) -> Option<Command> {
    let on = engine
        .state
        .parameters
        .get(&parameter_id(&vehicle.id, "SCR_ENABLE"))
        .is_some_and(|p| (p.value - 1.0).abs() < 0.5);
    (on && family.is_some_and(|f| f != Family::Px4)).then(Command::restart_scripts)
}

/// Descriptions shared by the command menu and the agent, for a vehicle of
/// this `MAV_TYPE`.
fn command_description(
    command: &Command,
    family: Option<Family>,
    vehicle_type: &str,
) -> Option<String> {
    // A rover, boat or submarine, of either autopilot.
    let keeps_down = VehicleClass::of(vehicle_type).never_flies();
    match (command, family) {
        (Command::SetMode { mode }, Some(family)) => {
            family.describe_for(mode, vehicle_type).map(str::to_owned)
        }
        (Command::GoTo { .. }, _) => {
            Some("Goes to a place chosen on the map and holds there".to_owned())
        }
        (Command::Long { command: 186, .. }, Some(Family::Plane)) => Some(
            "Changes the height while keeping the current horizontal destination".to_owned(),
        ),
        (Command::Long { command: 178, .. }, Some(Family::Plane)) => Some(
            "Changes the airspeed it flies at, to its destination or along its mission".to_owned(),
        ),
        (Command::Long { command: 178, .. }, _) => Some(
            "Changes the speed it goes at, to its destination or along its mission".to_owned(),
        ),
        // In plain words, for someone who has never flown
        // (maintainer, 2026-10-10): the button's tooltip and the
        // agent's reading of it. Disarmed, it is switched and armed
        // first (`take_off_steps`).
        (Command::Long { command: 22, .. }, Some(Family::Plane)) => Some(
            "Lifts off straight up and hovers. On the ground, it switches to GUIDED and arms first. It doesn't start the mission"
                .to_owned(),
        ),
        (Command::Long { command: 22, .. }, Some(Family::Px4)) => Some(
            "Climbs straight up to a set height and holds there. On the ground, it arms first. It doesn't start the mission"
                .to_owned(),
        ),
        (Command::Long { command: 22, .. }, _) => Some(
            "Climbs straight up to a set height and holds there. On the ground, it switches to GUIDED and arms first. It doesn't start the mission"
                .to_owned(),
        ),
        (Command::Arm, _) => Some("Enables the motors to spin.".to_owned()),
        (Command::Disarm, _) if keeps_down => Some("Stops the motors.".to_owned()),
        (Command::Disarm, _) => Some("Stops the motors. In the air, it falls.".to_owned()),
        (Command::Rtl, Some(Family::Plane)) => {
            Some("Flies back to where it took off and circles there".to_owned())
        }
        (Command::Rtl, Some(Family::Rover | Family::Sub)) => {
            Some("Goes back to where it started".to_owned())
        }
        (Command::Rtl, Some(Family::Px4)) if keeps_down => {
            Some("Goes back to where it started".to_owned())
        }
        (Command::Rtl, _) => {
            Some("Flies back to where it took off and lands".to_owned())
        }
        (Command::Land, Some(Family::Copter | Family::Blimp | Family::Px4)) => {
            Some("Comes down where it is, lands and stops its motors".to_owned())
        }
        // QLAND (uxduck-mavlink): one text for both kinds of
        // plane, rather than one that changes with the airframe
        // (maintainer, 2026-10-12).
        (Command::Land, Some(Family::Plane)) => Some(
            "A VTOL plane comes straight down where it is and lands. A plane that can't hover refuses: it lands through a mission"
                .to_owned(),
        ),
        (Command::SetHome { .. }, _) => Some(
            "Moves home, where it returns to and lands and its heights are measured from: to a place on the map, on the ground there, or to where it is now"
                .to_owned(),
        ),
        (Command::SetEkfOrigin { .. }, _) => Some(
            "Sets the point its position estimate is measured from, for flying without GPS. It's taken only while the vehicle has none"
                .to_owned(),
        ),
        (Command::Long { command, params }, _)
            if *command == MAV_CMD_DO_SET_MISSION_CURRENT
                && params.get(1).is_some_and(|p| *p >= 0.5) =>
        {
            Some(
                "Starts the mission again from its first step, with its repeats reset. In a mission mode it goes there now. Otherwise the mission starts there"
                    .to_owned(),
            )
        }
        (Command::Long { command, .. }, _) if *command == MAV_CMD_DO_SET_MISSION_CURRENT => Some(
            "Makes a step of the mission the current one. In a mission mode it goes on to it now. Otherwise the mission goes on from there"
                .to_owned(),
        ),
        (command @ Command::Long { .. }, _) if command.starts_mission() => Some(
            "Flies the mission from its current step, or a step you choose. On the ground, it arms first"
                .to_owned(),
        ),
        (command @ Command::Long { .. }, _) if command.lands_via_the_mission() => Some(
            "Flies to the mission's landing sequence, its nearest DO_LAND_START, and lands along it in AUTO. It needs a mission with a landing sequence"
                .to_owned(),
        ),
        (command @ Command::Long { .. }, _) if command.pauses() => Some(
            "Stops the vehicle where it is in its mission and holds there, until it's continued"
                .to_owned(),
        ),
        (Command::Long { command, .. }, _) if *command == MAV_CMD_DO_PAUSE_CONTINUE => Some(
            "Goes on with a paused mission from where it stopped".to_owned(),
        ),
        (Command::Long { command, .. }, _) if *command == MAV_CMD_SCRIPTING => Some(
            "Stops the vehicle's Lua scripts and starts them again, reading them from its scripts folder."
                .to_owned(),
        ),
        (Command::Reboot, _) => {
            // The autopilot refuses PREFLIGHT_REBOOT_SHUTDOWN while armed.
            Some("Restarts the autopilot. Works only while disarmed.".to_owned())
        }
        _ => None,
    }
}

/// Every family's modes, for a controller's mapping, which may drive any
/// vehicle: each family's whole table with a VTOL's usual modes first. With
/// a vehicle, its own family lists what the Cockpit's mode menu offers it:
/// its airframe's modes, or those it listed itself.
fn mode_list(engine: &Engine, vehicle_id: Option<&str>) -> ModeListResult {
    let vehicle = vehicle_id.and_then(|id| engine.vehicle(id));
    let own = vehicle.and_then(|vehicle| {
        let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type)?;
        let listed = engine.listed_modes(vehicle);
        Some((
            family,
            mode_airframe(engine, vehicle, Some(family), listed),
            listed,
        ))
    });
    ModeListResult {
        families: Family::ALL
            .iter()
            .map(|&family| {
                let modes = match own {
                    Some((mine, airframe, listed)) if mine == family => {
                        let offered: Vec<(String, bool)> = match listed {
                            Some(listed) => family.offered_from(Some(airframe), listed),
                            None => family
                                .offered(Some(airframe))
                                .into_iter()
                                .map(|(mode, advanced)| (mode.to_owned(), advanced))
                                .collect(),
                        };
                        offered
                            .into_iter()
                            .map(|(mode, advanced)| ModeInfo {
                                description: family
                                    .describe_for(&mode, airframe)
                                    .map(str::to_owned),
                                mode,
                                advanced,
                            })
                            .collect()
                    }
                    _ => family
                        .offered(None)
                        .into_iter()
                        .map(|(mode, advanced)| ModeInfo {
                            mode: mode.to_owned(),
                            description: family.describe(mode).map(str::to_owned),
                            advanced,
                        })
                        .collect(),
                };
                ModeFamily {
                    key: family.key().to_owned(),
                    name: family.title().to_owned(),
                    modes,
                }
            })
            .collect(),
        vehicle_family: own.map(|(family, _, _)| family.key().to_owned()),
    }
}

/// What a command's failure notice is bound to: the vehicle and the
/// command, any mode change counting as one, so the next attempt's outcome
/// replaces or clears it.
fn failure_condition(vehicle_id: &str, command: &Command) -> String {
    let what = match command {
        Command::SetMode { .. } => "set-mode".to_owned(),
        other => other.label(),
    };
    format!("command:{vehicle_id}:{what}")
}

fn set_state(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
    state: NotificationState,
) -> Result<Value, ApiError> {
    let Some(mut notification) = engine.state.notifications.get(id).cloned() else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no notification {id}"),
        ));
    };
    if notification.state != state {
        notification.state = state;
        notification.version += 1;
        engine.commit(connection, Change::NotificationUpserted { notification });
    }
    Ok(json!({}))
}

impl Engine {
    /// ArduPilot refuses a take-off with a bare "Failed" unless the vehicle
    /// is armed in GUIDED: say what it needs instead. Disarmed, it was
    /// switched and armed first (`take_off_steps`), unless it was not
    /// heard. PX4 takes off from any mode.
    fn take_off_refusal(&self, receipt: &Receipt) -> Option<String> {
        if !receipt.command.is_take_off() {
            return None;
        }
        let vehicle = self.vehicle(&receipt.vehicle_id)?;
        let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type);
        if !matches!(family, Some(Family::Copter | Family::Plane)) {
            return None;
        }
        let guided = vehicle
            .mode
            .as_deref()
            .is_some_and(|mode| mode.eq_ignore_ascii_case("GUIDED"));
        match (vehicle.armed, guided) {
            (true, true) => None,
            (false, true) => Some("It takes off once armed: arm it, then Take off.".into()),
            (true, false) => Some("It takes off from GUIDED: choose GUIDED, then Take off.".into()),
            (false, false) => Some(
                "It takes off from GUIDED once armed: choose GUIDED, arm it, then Take off.".into(),
            ),
        }
    }

    /// The data plane finished a command: complete its receipt, answer the
    /// request that sent it, and tell people when it did not go through.
    pub fn command_result(&mut self, receipt_id: &str, outcome: Outcome, attempts: u8) {
        // A motor test's renewal, which has no receipt (S188).
        if self.renewal_done(receipt_id, &outcome) {
            return;
        }
        // The response graph's message intervals, asked for one at a time (S330).
        if self.tuning_order_done(receipt_id) {
            return;
        }
        let Some(mut receipt) = self.state.receipts.get(receipt_id).cloned() else {
            return;
        };
        let was_pending = receipt.status == ReceiptStatus::Sent;
        let (status, detail) = match outcome {
            Outcome::Accepted => (ReceiptStatus::Accepted, None),
            Outcome::Rejected(why) => (
                ReceiptStatus::Rejected,
                Some(self.take_off_refusal(&receipt).unwrap_or(why)),
            ),
            Outcome::Timeout => (
                ReceiptStatus::Timeout,
                Some(format!("no answer after {attempts} tries")),
            ),
            Outcome::NotSent(why) => (ReceiptStatus::NotSent, Some(why)),
        };
        let condition = failure_condition(&receipt.vehicle_id, &receipt.command);
        // A command that went through clears an earlier refusal of it, and
        // a new failure replaces the one before: one notice, the latest.
        self.resolve(&condition);
        receipt.status = status;
        receipt.detail = detail;
        receipt.attempts = attempts;
        receipt.completed_at_us = Some(self.clock.now_us());
        receipt.version += 1;
        self.track_command(&receipt.command, status);
        if was_pending
            && self.awaiting.contains_key(receipt_id)
            && !self.is_launch_order(receipt_id)
        {
            self.track_task(
                &receipt.vehicle_id,
                uxduck_contracts::dto::analytics::EngagementTask::VehicleCommand,
                status == ReceiptStatus::Accepted,
            );
        }
        let result = ReceiptDto::from(&receipt);
        self.commit_as(
            daemon(),
            Change::ReceiptUpserted {
                receipt: receipt.clone(),
            },
        );
        if was_pending {
            self.plan_run_command_done(&receipt);
        }
        self.position_answered(receipt_id);
        // A take-off's next step, which answers in its place.
        if let Some(take_off) = self.take_offs.remove(receipt_id)
            && status == ReceiptStatus::Accepted
        {
            self.take_off_step(&receipt, take_off);
            return;
        }
        if let Some((connection, request_id)) = self.awaiting.remove(receipt_id) {
            let frame = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
            self.send(connection, &frame);
        }
        // A setup procedure's: its row says how it went (S184).
        if self.setup_order_done(&receipt) {
            return;
        }
        if self.is_launch_order(receipt_id) {
            let accepted = status == ReceiptStatus::Accepted;
            self.launch_order_done(receipt_id, accepted, receipt.detail.clone());
            return;
        }
        // Someone else's command: a simulated take-off leaves it be. The
        // simulation start's own, such as a rover's HOLD, is not.
        if status == ReceiptStatus::Accepted && receipt.client_id != crate::launch::ACTOR {
            self.hand_over_launch(&receipt.vehicle_id);
        }
        if !matches!(status, ReceiptStatus::Accepted) {
            let name = self
                .state
                .vehicles
                .get(&receipt.vehicle_id)
                .map_or_else(|| "the vehicle".to_owned(), |v| v.name.clone());
            let label = receipt.command.label();
            let (title, report) = match status {
                ReceiptStatus::Rejected => (format!("{name} refused {label}"), "command-refused"),
                ReceiptStatus::Timeout => {
                    (format!("{name} didn't answer {label}"), "command-timeout")
                }
                _ => (format!("{label} wasn't sent to {name}"), "command-not-sent"),
            };
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Transient,
                subject: Subject::Vehicle {
                    id: receipt.vehicle_id.clone(),
                },
                title,
                body: receipt.detail.clone(),
                actions: Vec::new(),
                condition: Some(condition),
                report: Some(report.into()),
            });
        }
    }

    /// Sends a take-off's next command after `after`, as the same sender.
    fn take_off_step(&mut self, after: &Receipt, take_off: TakeOff) {
        let TakeOff { actor, mut rest } = take_off;
        let command = rest.remove(0);
        let online = self
            .vehicle(&after.vehicle_id)
            .is_some_and(|v| v.presence == Presence::Online);
        let receipt = Receipt {
            id: format!("rc{}", &self.random.token()[..16]),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            sent_without_recent_heartbeat: !online,
            forced: false,
            attempts: 0,
            created_at_us: self.clock.now_us(),
            completed_at_us: None,
            version: 1,
            ..after.clone()
        };
        let id = receipt.id.clone();
        let link = self.vehicle(&after.vehicle_id).map(|v| v.link_id.clone());
        self.commit_as(actor.clone(), Change::ReceiptUpserted { receipt });
        if let Some(waiting) = self.awaiting.remove(&after.id) {
            self.awaiting.insert(id.clone(), waiting);
        }
        // Paused or gone since the step before: said as any command is.
        let why = match &link {
            Some(link) => self.not_writable(link),
            None => Some("the vehicle is gone".into()),
        };
        if let Some(why) = why {
            self.command_result(&id, Outcome::NotSent(why), 0);
            return;
        }
        if !rest.is_empty() {
            self.take_offs.insert(id.clone(), TakeOff { actor, rest });
        }
        self.links.command(CommandOrder {
            receipt_id: id,
            vehicle_id: after.vehicle_id.clone(),
            command,
            force: false,
        });
    }

    /// Raises a notification, unless one with the same condition is active.
    pub(crate) fn raise(&mut self, raise: Raise) {
        if let Some(condition) = &raise.condition
            && self
                .state
                .notifications
                .values()
                .any(|n| n.condition.as_ref() == Some(condition))
        {
            return;
        }
        let notification = Notification {
            id: format!("no{}", &self.random.token()[..16]),
            severity: raise.severity,
            kind: raise.kind,
            state: NotificationState::Active,
            subject: raise.subject,
            title: raise.title,
            body: raise.body,
            actions: raise.actions,
            condition: raise.condition,
            report: raise.report,
            created_at_us: self.clock.now_us(),
            version: 1,
        };
        self.commit_as(daemon(), Change::NotificationUpserted { notification });
    }

    /// Resolves the notifications bound to a condition that has cleared.
    pub(crate) fn resolve(&mut self, condition: &str) {
        let bound: Vec<Notification> = self
            .state
            .notifications
            .values()
            .filter(|n| n.condition.as_deref() == Some(condition))
            .cloned()
            .collect();
        for mut notification in bound {
            notification.state = NotificationState::Resolved;
            notification.version += 1;
            self.commit_as(daemon(), Change::NotificationUpserted { notification });
        }
    }

    /// A vehicle's presence changed: an alarm when it goes offline, resolved
    /// when it is heard again.
    pub(crate) fn presence_changed(&mut self, vehicle_id: &str, name: &str, presence: Presence) {
        let condition = format!("vehicle-offline:{vehicle_id}");
        // Armed when it went: its flight stays open, and a person may end it (C8).
        let actions = self
            .open_flight(vehicle_id)
            .map(|flight| NotificationAction {
                label: "End flight".into(),
                action: Action::EndFlight {
                    flight_id: flight.id.clone(),
                },
            })
            .into_iter()
            .collect();
        match presence {
            Presence::Offline => self.raise(Raise {
                severity: Severity::Alarm,
                kind: NotificationKind::Persistent,
                subject: Subject::Vehicle {
                    id: vehicle_id.to_owned(),
                },
                title: format!("{name} is offline"),
                body: Some("UXDuck hasn't heard from it for a while. Commands that work offline still go to its last known address.".into()),
                actions,
                condition: Some(condition),
                report: None,
            }),
            Presence::Online => self.resolve(&condition),
            Presence::Stale => {}
        }
    }

    /// A forward was turned off by loop detection: an alarm with the button
    /// to turn it on again.
    pub(crate) fn loop_detected(&mut self, link_id: &str, name: &str, reason: &str) {
        self.raise(Raise {
            severity: Severity::Alarm,
            kind: NotificationKind::Persistent,
            subject: Subject::Link {
                id: link_id.to_owned(),
            },
            title: format!("{name} turned off: its messages were looping back"),
            body: Some(reason.to_owned()),
            actions: vec![NotificationAction {
                label: "Turn on".into(),
                action: Action::EnableLink {
                    link_id: link_id.to_owned(),
                },
            }],
            condition: Some(format!("link-loop:{link_id}")),
            report: None,
        });
    }
}

#[cfg(test)]
mod unsafe_place_tests {
    use uxduck_core::domain::command::{Command, PlaceAmsl};
    use uxduck_core::domain::telemetry::AltitudeFrame;

    use super::unsafe_place;

    fn go(latitude_deg: f64, longitude_deg: f64, altitude_m: f64, frame: AltitudeFrame) -> Command {
        Command::GoTo {
            latitude_deg,
            longitude_deg,
            altitude_m,
            altitude_frame: frame,
            loiter_radius_m: None,
            heading: None,
        }
    }

    #[test]
    fn native_height_changes_have_the_same_ground_guard_as_go_to() {
        let height = |alt, frame| Command::Long {
            command: 186,
            params: vec![alt, frame],
        };
        for frame in [3.0, 6.0, 10.0, 11.0] {
            assert!(unsafe_place(&height(0.0, frame), true).is_some());
            assert!(unsafe_place(&height(-1.0, frame), true).is_some());
            assert_eq!(unsafe_place(&height(40.0, frame), true), None);
        }
        assert_eq!(unsafe_place(&height(-1.0, 0.0), true), None);
        assert!(unsafe_place(&height(f32::NAN, 3.0), true).is_some());
        assert!(unsafe_place(&height(40.0, 2.0), true).is_some());
        let speed = |kind: f32, mps: f32| Command::Long {
            command: 178,
            params: vec![kind, mps, -1.0],
        };
        assert_eq!(unsafe_place(&speed(0.0, 18.0), true), None);
        assert_eq!(unsafe_place(&speed(1.0, 5.0), false), None);
        for bad in [0.0, -1.0, f32::NAN, 400.0] {
            assert!(unsafe_place(&speed(0.0, bad), true).is_some(), "{bad}");
        }
        assert!(unsafe_place(&speed(2.0, 3.0), true).is_some());
        assert!(
            unsafe_place(
                &Command::Long {
                    command: 186,
                    params: vec![]
                },
                true
            )
            .is_some()
        );
    }

    #[test]
    fn a_go_to_off_the_map_at_0_0_or_into_the_ground_is_refused() {
        let ok = go(-35.36, 149.16, 30.0, AltitudeFrame::AboveHome);
        assert_eq!(unsafe_place(&ok, true), None);
        assert!(unsafe_place(&go(0.0, 0.0, 30.0, AltitudeFrame::AboveHome), true).is_some());
        assert!(unsafe_place(&go(91.0, 149.16, 30.0, AltitudeFrame::AboveHome), true).is_some());
        assert!(
            unsafe_place(&go(f64::NAN, 149.16, 30.0, AltitudeFrame::AboveHome), true).is_some()
        );
        for frame in [AltitudeFrame::AboveHome, AltitudeFrame::AboveGround] {
            assert!(unsafe_place(&go(-35.36, 149.16, 0.0, frame), true).is_some());
            assert!(unsafe_place(&go(-35.36, 149.16, -5.0, frame), true).is_some());
        }
        // Sea level and below can be a real height above the sea.
        assert_eq!(
            unsafe_place(&go(-35.36, 149.16, 0.0, AltitudeFrame::Amsl), true),
            None
        );
        // A rover, boat or submarine keeps its own height, 0 or below.
        assert_eq!(
            unsafe_place(&go(-35.36, 149.16, 0.0, AltitudeFrame::AboveHome), false),
            None
        );
        assert_eq!(
            unsafe_place(&go(-35.36, 149.16, -3.0, AltitudeFrame::AboveHome), false),
            None
        );
        // Other commands are not places.
        assert_eq!(unsafe_place(&Command::Rtl, true), None);
    }

    #[test]
    fn home_and_the_origin_go_only_on_the_ground_somewhere_on_the_map() {
        let at = |latitude_deg: f64, longitude_deg: f64, altitude_amsl_m: f64| PlaceAmsl {
            latitude_deg,
            longitude_deg,
            altitude_amsl_m,
        };
        for place in [
            at(-35.36, 149.16, 584.0),
            at(31.5, 35.5, -430.0),
            at(28.0, 86.9, 8_849.0),
        ] {
            assert_eq!(
                unsafe_place(&Command::SetHome { place: Some(place) }, true),
                None
            );
            assert_eq!(unsafe_place(&Command::SetEkfOrigin { place }, false), None);
        }
        // Where it is now has no place to judge.
        assert_eq!(unsafe_place(&Command::SetHome { place: None }, true), None);
        for place in [
            at(0.0, 0.0, 10.0),
            at(91.0, 149.16, 10.0),
            at(f64::NAN, 149.16, 10.0),
            // Feet for metres on a mountain, or no height at all.
            at(-35.36, 149.16, 12_000.0),
            at(-35.36, 149.16, -600.0),
            at(-35.36, 149.16, f64::NAN),
        ] {
            assert!(unsafe_place(&Command::SetHome { place: Some(place) }, true).is_some());
            assert!(unsafe_place(&Command::SetEkfOrigin { place }, true).is_some());
        }
    }
}

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
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{
    Command, GoToHeading, OfflineBehaviour, Receipt, ReceiptStatus, RefusalReason,
};
use uxduck_core::domain::modes::{self, AvailableMode, Family, is_vtol};
use uxduck_core::domain::notification::{
    Action, Notification, NotificationAction, NotificationKind, NotificationState, Severity,
    Subject,
};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::preflight::PreflightOverride;
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};

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
            require(Capability::Control)?;
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
            if let Some(vehicle) = engine.vehicle(&params.vehicle_id)
                && let Some(why) = unsafe_place(&params.command.clone().into(), flies(vehicle))
            {
                return Err(ApiError::new(ErrorKind::InvalidValue, why));
            }
            send(engine, connection, params, operator)
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

fn send(
    engine: &mut Engine,
    connection: ConnectionId,
    params: CommandSendParams,
    operator: bool,
) -> Result<Value, ApiError> {
    let vehicle = engine.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no vehicle {}", params.vehicle_id),
        )
    })?;
    let command: Command = params.command.into();
    // A disarmed vehicle's take-off goes after the steps it needs, the
    // first of them now (`TakeOff`).
    let mut steps = take_off_steps(engine, &vehicle, &command).unwrap_or_default();
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
    // From away (S80): who sent it, and refused if it is too old to act on.
    let remote = mark_remote(
        engine,
        connection,
        (params.command_id.as_deref(), params.sent_at_ms),
        &command,
        &mut receipt,
    );
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
    // The preflight gate, for every client (S192).
    let (ids, away) = (&params.override_preflight, remote.is_some());
    preflight_gate(engine, &vehicle, ids, (operator, away), &mut receipt);
    let result = ReceiptDto::from(&receipt);
    let id = receipt.id.clone();
    let sent = receipt.status == ReceiptStatus::Sent;
    let status = receipt.status;
    engine.commit_as(actor.clone(), Change::ReceiptUpserted { receipt });
    if let Some(by) = &remote {
        engine.remember_remote(by, &id);
    }
    if !sent {
        engine.track_command(&command, status);
        return to_value(&result);
    }
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
        engine.take_offs.insert(id, TakeOff { actor, rest: steps });
    }
    Ok(Value::Null)
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
/// past them by an operator naming every one, never from away, or by an
/// approval that listed them (ADR-0038 K3, K4).
fn preflight_gate(
    engine: &Engine,
    vehicle: &Vehicle,
    named_ids: &[String],
    (operator, away): (bool, bool),
    receipt: &mut Receipt,
) {
    if receipt.status != ReceiptStatus::Sent
        || vehicle.armed
        || !receipt.command.arms_on_the_ground()
    {
        return;
    }
    let gating = engine.preflight_gating(&vehicle.id);
    if gating.is_empty() {
        return;
    }
    let named = |ids: &[String]| gating.iter().all(|c| ids.contains(&c.id));
    let by = if away {
        None
    } else if operator && named(named_ids) {
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
    let label = receipt.command.label();
    receipt.status = ReceiptStatus::NotSent;
    receipt.detail = Some(crate::preflight::refusal(
        &label,
        &vehicle.name,
        &gating,
        operator && !away,
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

/// A command from away (S80): names who sent it on its receipt, and makes it
/// `not-sent` as `expired` when it arrived too old to act on.
fn mark_remote(
    engine: &Engine,
    connection: ConnectionId,
    (command_id, sent_at_ms): (Option<&str>, Option<u64>),
    command: &Command,
    receipt: &mut Receipt,
) -> Option<uxduck_core::domain::command::RemoteBy> {
    let (by, expired) = engine.remote_command(connection, command, command_id, sent_at_ms)?;
    receipt.remote = Some(by.clone());
    if let Some(why) = expired {
        receipt.status = ReceiptStatus::NotSent;
        receipt.detail = Some(why);
        receipt.completed_at_us = Some(receipt.created_at_us);
        receipt.reason = Some(RefusalReason::Expired);
    }
    Some(by)
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
    commands.push(Command::Rtl);
    if matches!(
        family,
        Some(Family::Copter | Family::Plane | Family::Blimp | Family::Px4)
    ) {
        commands.push(Command::Land);
    }
    commands.push(Command::Reboot);
    Ok(CommandListResult {
        commands: commands
            .iter()
            .map(|command| CommandInfo {
                command: command.into(),
                label: command.label(),
                description: command_description(command, family),
                advanced: matches!(command, Command::SetMode { mode } if advanced.contains(mode)),
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

/// Descriptions shared by the command menu and the agent.
fn command_description(command: &Command, family: Option<Family>) -> Option<String> {
    match (command, family) {
        (Command::SetMode { mode }, Some(family)) => {
            family.describe(mode).map(str::to_owned)
        }
        (Command::GoTo { .. }, _) => {
            Some("Goes to a place chosen on the map and holds there".to_owned())
        }
        (Command::Long { command: 186, .. }, Some(Family::Plane)) => Some(
            "Changes the height while keeping the current horizontal destination".to_owned(),
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
        (Command::Arm, _) => Some(
            "Arming enables the motors for takeoff. The propellers may spin.".to_owned(),
        ),
        (Command::Disarm, _) => {
            Some("Stops the motors. Only on the ground: in the air it falls".to_owned())
        }
        (Command::Rtl, Some(Family::Plane)) => {
            Some("Flies back to where it took off and circles there".to_owned())
        }
        (Command::Rtl, Some(Family::Rover | Family::Sub)) => {
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
        (Command::Reboot, _) => {
            Some("Restarts the autopilot. Available only while the vehicle is on the ground.".to_owned())
        }
        _ => None,
    }
}

/// Every family's modes, for a controller's mapping, which may drive any
/// vehicle: each family's whole table with a VTOL's usual modes first.
fn mode_list(engine: &Engine, vehicle_id: Option<&str>) -> ModeListResult {
    let vehicle_family = vehicle_id
        .and_then(|id| engine.vehicle(id))
        .and_then(|vehicle| Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type))
        .map(|family| family.key().to_owned());
    ModeListResult {
        families: Family::ALL
            .iter()
            .map(|family| ModeFamily {
                key: family.key().to_owned(),
                name: family.title().to_owned(),
                modes: family
                    .offered(None)
                    .into_iter()
                    .map(|(mode, advanced)| ModeInfo {
                        mode: mode.to_owned(),
                        description: family.describe(mode).map(str::to_owned),
                        advanced,
                    })
                    .collect(),
            })
            .collect(),
        vehicle_family,
    }
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
        if status == ReceiptStatus::Accepted {
            // Someone else's command: a simulated take-off leaves it be.
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
                condition: None,
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
    use uxduck_core::domain::command::Command;
    use uxduck_core::domain::telemetry::AltitudeFrame;

    use super::unsafe_place;

    fn go(latitude_deg: f64, longitude_deg: f64, altitude_m: f64, frame: AltitudeFrame) -> Command {
        Command::GoTo {
            latitude_deg,
            longitude_deg,
            altitude_m,
            altitude_frame: frame,
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
}

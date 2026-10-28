//! Vehicle setup (ADR-0037, S184): the Setup tab's rows, and each vehicle's
//! procedure, held here rather than by a window (V2). `setup/start` sends
//! the vehicle's own calibration as a command with a receipt; the vehicle's
//! reports move it on; one that finishes or fails goes in the vehicle's
//! history (V13). Only for a disarmed vehicle UXDuck can hear and write to,
//! one at a time, and with `config` (V3). The agent reads `setup/state` but
//! cannot start one: a person does it with the vehicle in their hands (V4).
//!
//! `ArduPilot`'s compass calibration (V5): `MAV_CMD_DO_START_MAG_CAL` for
//! every compass, with no retry, saved by the vehicle, no delay and no
//! restart; `MAG_CAL_PROGRESS` while each compass runs, and `MAG_CAL_REPORT`
//! with its result, which the vehicle repeats until it restarts. A vehicle
//! found calibrating, started by another ground station, is followed too.
//!
//! From a known heading (V6, S185): `MAV_CMD_FIXED_MAG_CAL_YAW` with the
//! vehicle's true heading, every compass, and its position given on the map
//! or, with 0, 0, its own (which needs a 3D fix). One command, answered at
//! once: the procedure is starting until the vehicle answers, then
//! succeeded or failed, with no restart. The vehicle says why it failed in
//! a `Mag…` message just after its answer, which becomes the row's words.
//!
//! The accelerometers (V7, S186): `MAV_CMD_PREFLIGHT_CALIBRATION` with its
//! accelerometer parameter 1 starts `ArduPilot`'s full calibration. The
//! vehicle then asks for each of six positions with
//! `MAV_CMD_ACCELCAL_VEHICLE_POS` (to target 0, on the link of the ground
//! station that started it), repeated once a second until answered;
//! the person holds it so and confirms (`setup/confirm`), and UXDuck answers
//! with the same command and that position (current `ArduPilot`; older
//! versions took a `COMMAND_ACK` instead, which is not sent). It ends with
//! the vehicle's success or failure, as the same command or as its
//! "Calibration successful" or "Calibration FAILED", and needs a restart.
//! Level is parameter 2: one command, answered once the vehicle has taken
//! the attitude it rests in as level.
//!
//! The radio (V8, S187) is done by UXDuck, not the vehicle: see `radio`.
//! The airspeed's zero, pitot check and ratio (V15, S190): see `airspeed`.
//! PX4's driver for the same rows (V11, S191): see `px4`.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::setup::{
    CompassInfo, HeadingParams, RowState, Setup as SetupDto, SetupConfirmParams, SetupParams,
    SetupRowState, SetupStartParams, SetupStateResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::history::{EntryKind, HistoryEvent};
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::setup::{
    CompassFailure, Fitness, Fix, HeadingFrom, Instruction, KnownHeading, SensorProgress,
    SensorState, Setup, SetupKind, SetupRow, SetupStep, VehiclePosition, fix_for, set_up, setup_id,
};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change};
use uxduck_mavlink::session::MagCal;

mod airspeed;
mod firmware;
mod motors;
mod px4;
mod radio;

pub use firmware::{
    FirmwarePort, FirmwareReport, InstallOrder, InstallSource, ReleasesFound, ReleasesOrder,
};

use crate::rpc::handlers::to_value;
use crate::runtime::CommandOrder;
use crate::{ConnectionId, Engine};

/// Starts `ArduPilot`'s compass calibration.
const MAV_CMD_DO_START_MAG_CAL: u16 = 42424;
/// Cancels it.
const MAV_CMD_DO_CANCEL_MAG_CAL: u16 = 42426;
/// Calibrates every compass from a known heading (S185).
const MAV_CMD_FIXED_MAG_CAL_YAW: u16 = 42006;
/// Calibrates the accelerometers (parameter 5: 1 the full calibration, 2
/// level) (S186).
const MAV_CMD_PREFLIGHT_CALIBRATION: u16 = 241;
/// A position asked for and answered (S186).
const MAV_CMD_ACCELCAL_VEHICLE_POS: u16 = 42429;
/// `ACCELCAL_VEHICLE_POS_SUCCESS` and `_FAILED`.
const ACCELCAL_SUCCESS: u32 = 16_777_215;
const ACCELCAL_FAILED: u32 = 16_777_216;
/// The same position asked for again this long after it was confirmed:
/// the vehicle did not take it, and wants it again. Sooner, the request
/// crossed the answer.
const ASKED_AGAIN_US: u64 = 3_000_000;
/// `GPS_FIX_TYPE_3D_FIX`: what `ArduPilot` needs to use its own position.
const GPS_3D_FIX: f64 = 3.0;
/// A vehicle's message this soon after its refusal says why.
const SAID_US: u64 = 3_000_000;
/// `MAG_CAL_STATUS`: the compass succeeded.
const MAG_CAL_SUCCESS: u8 = 4;
/// Progress is journaled at most this often; a sensor finishing, or a
/// change of step, at once.
const PROGRESS_EVERY_US: u64 = 1_000_000;
/// A procedure the vehicle has said nothing about for this long no longer
/// stops another from starting: the vehicle went away during it.
const STALE_US: u64 = 15_000_000;

/// What a command sent for a procedure was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    Start,
    Cancel,
    /// The answer to a position the vehicle asked for (S186).
    Placed,
    /// A stream's rate set back after the radio calibration (S187): its
    /// answer changes nothing.
    Rate,
    /// A motor test stopped (S188): a hold waiting for it starts.
    Stop,
}

/// What the engine keeps about procedures beyond the journal.
#[derive(Debug, Default)]
pub(crate) struct Setups {
    /// Commands sent for procedures, by receipt id: whose, and what for.
    orders: BTreeMap<String, (String, Order)>,
    /// The latest progress not journaled yet, by setup id.
    unsaved: BTreeMap<String, Setup>,
    /// The radio calibration's parameter writes, by order id: the vehicle
    /// and the parameter (S187).
    radio_writes: BTreeMap<String, (String, String)>,
    /// Each channel's reading when the radio calibration asked for the
    /// stick it waits for, by vehicle: a stick is found by how far it
    /// moved from there (S187).
    radio_from: BTreeMap<String, BTreeMap<u8, u16>>,
    /// The channel seen moved as asked, its reading and since when, by
    /// vehicle: found once held long enough (S187).
    radio_held: BTreeMap<String, (u8, u16, u64)>,
    /// Motors held, by vehicle (S188).
    spins: BTreeMap<String, motors::Spin>,
    /// Motor test renewals sent, with no receipt in the journal: the
    /// vehicle, by order id (S188).
    renewals: BTreeMap<String, String>,
    /// When a motor test's stop was sent, by vehicle, until it is
    /// answered (S188).
    stopping: BTreeMap<String, u64>,
    /// Vehicles armed by the vehicle itself for a motor test (S188).
    test_armed: std::collections::BTreeSet<String>,
    /// Why a vehicle refused its motor test, as it said it, and when, by
    /// vehicle: said before its answer came (S188).
    motor_said: BTreeMap<String, (String, u64)>,
    /// Firmware installs and the port that runs them (S189).
    firmware: firmware::Firmware,
    /// The airspeed ratio's estimates and writes (S190).
    airspeed: airspeed::Airspeeds,
    /// A PX4 airspeed calibration's offset and the pressure blown into
    /// it, pascals, by vehicle, for its history (S191).
    px4_airspeed: BTreeMap<String, (Option<i32>, Option<i32>)>,
}

fn actor() -> Actor {
    Actor {
        client_id: "setup".into(),
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
        ClientRequest::SetupState(SetupParams { vehicle_id }) => {
            require(Capability::Read)?;
            to_value(&engine.setup_state(&vehicle_id)?)
        }
        ClientRequest::SetupStart(params) => {
            require(Capability::Config)?;
            start(engine, connection, &params)
        }
        ClientRequest::SetupFirmware(params) => {
            require(Capability::Config)?;
            firmware::install(engine, connection, &params, operator)
        }
        ClientRequest::SetupFirmwareReleases(params) => {
            require(Capability::Read)?;
            firmware::releases(engine, connection, &params)
        }
        ClientRequest::SetupCancel(SetupParams { vehicle_id }) => {
            // An install is cancelled with `config`, only before erasing.
            if let Some(vehicle) = engine.vehicle(&vehicle_id).cloned()
                && engine
                    .current_setup(&vehicle.id)
                    .is_some_and(|s| s.kind == SetupKind::Firmware && s.step.running())
            {
                require(Capability::Config)?;
                return engine.cancel_install(&vehicle);
            }
            // A motor test is stopped with `control`, as it is started.
            let motors = engine
                .vehicle(&vehicle_id)
                .and_then(|v| engine.current_setup(&v.id))
                .is_some_and(|s| s.kind == SetupKind::Motors && s.step.running());
            if motors {
                require(Capability::Control)?;
                return motors::spin(
                    engine,
                    connection,
                    &uxduck_contracts::dto::setup::SetupSpinParams {
                        vehicle_id,
                        motor: 0,
                        throttle: 0.0,
                        held: false,
                        keep: false,
                        propellers_off: true,
                    },
                    true,
                );
            }
            require(Capability::Config)?;
            cancel(engine, connection, &vehicle_id)
        }
        ClientRequest::SetupSpin(params) => {
            require(Capability::Control)?;
            motors::spin(engine, connection, &params, operator)
        }
        ClientRequest::SetupConfirm(params) => {
            require(Capability::Config)?;
            if engine
                .current_setup(&params.vehicle_id)
                .or_else(|| {
                    let id = engine.vehicle(&params.vehicle_id)?.id.clone();
                    engine.current_setup(&id)
                })
                .is_some_and(|s| s.kind == SetupKind::Radio && s.step.running())
            {
                return radio::confirm(engine, connection, &params);
            }
            confirm(engine, connection, &params)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a setup method")),
    }
}

fn no_vehicle(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}"))
}

#[allow(clippy::too_many_lines, reason = "one arm per procedure")]
fn start(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SetupStartParams,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(&params.vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(&params.vehicle_id))?;
    let kind = SetupKind::from(params.kind);
    // The airspeed's zero, check and ratio (S190); the ratio in flight.
    if matches!(
        kind,
        SetupKind::AirspeedZero | SetupKind::AirspeedCheck | SetupKind::AirspeedRatio
    ) {
        return airspeed::start(engine, connection, &vehicle, kind);
    }
    if let Some(why) = engine.cannot_set_up(&vehicle) {
        return Err(ApiError::new(ErrorKind::InvalidValue, why));
    }
    if let Some(running) = engine.running_setup(&vehicle.id) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!(
                "{} is already running on {}. Cancel it first.",
                running.kind.words(),
                vehicle.name
            ),
        ));
    }
    let heading = match kind {
        SetupKind::CompassHeading => {
            let heading = known_heading(engine, &vehicle, params.heading.as_ref())?;
            if px4::px4(&vehicle)
                && let Some(why) = px4::heading_refused(&vehicle, heading.degrees)
            {
                return Err(ApiError::new(ErrorKind::InvalidValue, why));
            }
            Some(heading)
        }
        SetupKind::Radio => {
            engine.can_calibrate_radio(&vehicle)?;
            None
        }
        SetupKind::Compass | SetupKind::Accelerometer | SetupKind::Level => None,
        SetupKind::Motors => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "A motor spins while its press is held: use setup/spin.",
            ));
        }
        SetupKind::Firmware => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "Firmware is installed with setup/firmware.",
            ));
        }
        SetupKind::AirspeedZero | SetupKind::AirspeedCheck | SetupKind::AirspeedRatio => {
            unreachable!("started above")
        }
    };
    let now = engine.clock.now_us();
    let receipt_id = format!("rc{}", &engine.random.token()[..16]);
    let setup = Setup {
        vehicle_id: vehicle.id.clone(),
        kind,
        step: SetupStep::Starting,
        progress: 0,
        sensors: Vec::new(),
        instruction: None,
        detail: None,
        restart_needed: false,
        heading: heading.clone(),
        position: None,
        placed: false,
        sides: None,
        radio: (kind == SetupKind::Radio).then(uxduck_core::domain::setup::Radio::new),
        motor: None,
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
    // PX4's own commands for the compasses and the radio (S191); the
    // radio's channels are asked for as well.
    if px4::px4(&vehicle)
        && let Some(command) = px4::start_command(kind)
    {
        engine.send_setup_command(&vehicle, actor.clone(), receipt_id, command, Order::Start);
        if kind == SetupKind::Radio {
            let rate = format!("rc{}", &engine.random.token()[..16]);
            engine.send_setup_command(&vehicle, actor, rate, radio::stream(true), Order::Rate);
        }
        return to_value(&SetupDto::from(&saved));
    }
    let command = match kind {
        // Every compass, no retry, saved by the vehicle, no delay, and no
        // restart of its own: the person restarts it with a hold.
        SetupKind::Compass => Command::Long {
            command: MAV_CMD_DO_START_MAG_CAL,
            params: vec![0.0, 0.0, 1.0, 0.0, 0.0],
        },
        // The heading, every compass (mask 0), and the position given or
        // 0, 0 for the vehicle's own.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "COMMAND_LONG's parameters are floats"
        )]
        SetupKind::CompassHeading => {
            let heading = heading.unwrap_or_else(|| unreachable!("checked above"));
            Command::Long {
                command: MAV_CMD_FIXED_MAG_CAL_YAW,
                params: vec![
                    heading.degrees as f32,
                    0.0,
                    heading.latitude_deg.unwrap_or(0.0) as f32,
                    heading.longitude_deg.unwrap_or(0.0) as f32,
                ],
            }
        }
        // The full calibration, or level: parameter 5, the others 0.
        SetupKind::Accelerometer => Command::Long {
            command: MAV_CMD_PREFLIGHT_CALIBRATION,
            params: vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        },
        SetupKind::Level => Command::Long {
            command: MAV_CMD_PREFLIGHT_CALIBRATION,
            params: vec![0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0],
        },
        // The radio's channels ten times a second: its answer starts it.
        SetupKind::Radio => radio::stream(true),
        SetupKind::Motors
        | SetupKind::Firmware
        | SetupKind::AirspeedZero
        | SetupKind::AirspeedCheck
        | SetupKind::AirspeedRatio => unreachable!("refused above"),
    };
    engine.send_setup_command(&vehicle, actor, receipt_id, command, Order::Start);
    to_value(&SetupDto::from(&saved))
}

fn cancel(
    engine: &mut Engine,
    connection: ConnectionId,
    vehicle_id: &str,
) -> Result<Value, ApiError> {
    let vehicle = engine
        .vehicle(vehicle_id)
        .cloned()
        .ok_or_else(|| no_vehicle(vehicle_id))?;
    let Some(mut setup) = engine
        .current_setup(&vehicle.id)
        .filter(|s| s.step.running())
    else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("nothing is being set up on {}", vehicle.name),
        ));
    };
    // A ratio in flight turns the vehicle's automatic calibration off
    // (S190).
    if setup.kind == SetupKind::AirspeedRatio {
        let stopped = engine
            .stop_ratio(&vehicle.id, SetupStep::Cancelled, "Cancelled.")
            .unwrap_or(setup);
        return to_value(&SetupDto::from(&stopped));
    }
    let actor = engine.actor(connection);
    // Told to the vehicle, and cancelled here at once: a late report
    // changes nothing. A calibration from a heading, and level, are one
    // command the vehicle has either done or not: there is nothing to tell
    // it. ArduPilot has no command to stop its accelerometer calibration:
    // it asks for its position until it restarts, which is not followed.
    // The radio's channels go back to their usual rate.
    // PX4 stops a calibration, and ends the radio's, with 241 all 0
    // (S191); the radio's channels go back to their rate too.
    if px4::px4(&vehicle)
        && matches!(
            setup.kind,
            SetupKind::Compass
                | SetupKind::Accelerometer
                | SetupKind::Level
                | SetupKind::AirspeedZero
                | SetupKind::Radio
        )
        && engine.not_writable(&vehicle.link_id).is_none()
    {
        let receipt_id = format!("rc{}", &engine.random.token()[..16]);
        engine.send_setup_command(
            &vehicle,
            actor.clone(),
            receipt_id,
            px4::stop(),
            Order::Cancel,
        );
    }
    let command = match setup.kind {
        _ if px4::px4(&vehicle) && setup.kind != SetupKind::Radio => None,
        SetupKind::Compass => Some(Command::Long {
            command: MAV_CMD_DO_CANCEL_MAG_CAL,
            params: vec![0.0],
        }),
        SetupKind::Radio => Some(radio::stream(false)),
        SetupKind::CompassHeading
        | SetupKind::Accelerometer
        | SetupKind::Level
        | SetupKind::Motors
        | SetupKind::Firmware
        | SetupKind::AirspeedZero
        | SetupKind::AirspeedCheck
        | SetupKind::AirspeedRatio => None,
    };
    if let Some(command) = command
        && engine.not_writable(&vehicle.link_id).is_none()
    {
        let receipt_id = format!("rc{}", &engine.random.token()[..16]);
        engine.send_setup_command(&vehicle, actor.clone(), receipt_id, command, Order::Cancel);
    }
    engine.setup.radio_from.remove(&vehicle.id);
    let now = engine.clock.now_us();
    setup.step = SetupStep::Cancelled;
    setup.instruction = None;
    setup.position = None;
    setup.placed = false;
    setup.updated_at_us = now;
    setup.finished_at_us = Some(now);
    let saved = engine.save_setup(setup, true, actor);
    to_value(&SetupDto::from(&saved))
}

/// `setup/confirm` (S186): the vehicle is held in the position it asks for.
/// UXDuck tells the vehicle, which measures it and asks for the next.
fn confirm(
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
        .filter(|s| s.step.running() && s.kind == SetupKind::Accelerometer)
    else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("nothing on {} is waiting to be confirmed", vehicle.name),
        ));
    };
    if setup.sides.is_some() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "{} finds the side itself: hold it still on a side not done yet.",
                vehicle.name
            ),
        ));
    }
    let Some(asked) = setup.position.filter(|_| !setup.placed) else {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!("{} has not asked for a position yet.", vehicle.name),
        ));
    };
    let given = params.position.map(VehiclePosition::from);
    if given != Some(asked) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("{} asks to be held {} now.", vehicle.name, asked.words()),
        ));
    }
    if let Some(why) = engine.not_writable(&vehicle.link_id) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("Can't tell {}: {why}.", vehicle.name),
        ));
    }
    let actor = engine.actor(connection);
    let receipt_id = format!("rc{}", &engine.random.token()[..16]);
    #[allow(clippy::cast_precision_loss, reason = "a position number, 1 to 6")]
    let command = Command::Long {
        command: MAV_CMD_ACCELCAL_VEHICLE_POS,
        params: vec![asked.mavlink() as f32],
    };
    engine.send_setup_command(&vehicle, actor.clone(), receipt_id, command, Order::Placed);
    setup.placed = true;
    setup.instruction = Some(Instruction::HoldStill);
    setup.detail = None;
    setup.updated_at_us = engine.clock.now_us();
    let saved = engine.save_setup(setup, true, actor);
    to_value(&SetupDto::from(&saved))
}

/// What a vehicle's refusal of an accelerometer calibration or level means.
fn accel_refusal(name: &str, kind: SetupKind, receipt: &Receipt) -> String {
    let what = if kind == SetupKind::Level {
        "take this as level"
    } else {
        "start its accelerometer calibration"
    };
    match (receipt.status, receipt.detail.as_deref()) {
        (ReceiptStatus::Rejected, Some("Unsupported")) => {
            format!("{name} can't {what}: its firmware does not support it.")
        }
        (ReceiptStatus::Rejected, Some("Denied" | "TemporarilyRejected")) => {
            format!("{name} refused to {what} now. Disarm it and keep it still.")
        }
        (ReceiptStatus::Rejected, _) => format!("{name} could not {what}."),
        (ReceiptStatus::Timeout, _) => format!("{name} did not answer."),
        (_, Some(why)) => format!("It was not sent to {name}: {why}."),
        _ => format!("{name} did not {what}."),
    }
}

/// How many of the six positions the person has confirmed.
fn positions_done(setup: &Setup) -> usize {
    match setup.position {
        Some(position) if setup.placed => position.mavlink() as usize,
        Some(position) => position.mavlink() as usize - 1,
        None => 0,
    }
}

/// The heading asked for, checked: a number of degrees, a marker that is
/// there, a position on the Earth or none, and with none a 3D fix of the
/// vehicle's own.
fn known_heading(
    engine: &Engine,
    vehicle: &Vehicle,
    params: Option<&HeadingParams>,
) -> Result<KnownHeading, ApiError> {
    let invalid = |why: String| ApiError::new(ErrorKind::InvalidValue, why);
    let params = params.ok_or_else(|| {
        invalid(format!(
            "Give the heading {} faces, in degrees from true north.",
            vehicle.name
        ))
    })?;
    if !params.degrees.is_finite() {
        return Err(invalid("The heading must be a number of degrees.".into()));
    }
    let from = HeadingFrom::from(params.from);
    let marker = match (&params.marker_id, from) {
        (Some(id), HeadingFrom::Marker) => Some(
            engine
                .state
                .markers
                .get(id)
                .map(|m| m.name.clone())
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no marker {id}")))?,
        ),
        _ => None,
    };
    let (latitude_deg, longitude_deg) = match (params.latitude_deg, params.longitude_deg) {
        (Some(lat), Some(lon))
            if lat.is_finite()
                && lon.is_finite()
                && lat.abs() <= 90.0
                && lon.abs() <= 180.0
                && (lat, lon) != (0.0, 0.0) =>
        {
            (Some(lat), Some(lon))
        }
        (None, None) => (None, None),
        _ => {
            return Err(invalid(
                "Give both a latitude and a longitude for the vehicle's position, or neither."
                    .into(),
            ));
        }
    };
    // ArduPilot uses its own position only with a 3D fix.
    if latitude_deg.is_none()
        && engine
            .telemetry_value(&vehicle.id, "gps.fix_type")
            .is_none_or(|fix| fix < GPS_3D_FIX)
    {
        return Err(invalid(format!(
            "{} has no 3D GPS fix. Give its position on the map.",
            vehicle.name
        )));
    }
    Ok(KnownHeading {
        degrees: params.degrees.rem_euclid(360.0),
        from,
        marker,
        latitude_deg,
        longitude_deg,
    })
}

/// "a true heading of 270°, the bearing to Flag, at its GPS position".
fn heading_words(heading: &KnownHeading) -> String {
    let degrees = round1(heading.degrees);
    let from = match (heading.from, &heading.marker) {
        (HeadingFrom::Marker, Some(name)) => format!(", the bearing to {name}"),
        (HeadingFrom::Map, _) => ", the bearing to a point on the map".to_owned(),
        _ => String::new(),
    };
    let at = match (heading.latitude_deg, heading.longitude_deg) {
        (Some(lat), Some(lon)) => format!(", at {lat:.6}, {lon:.6} as given"),
        _ => ", at its GPS position".to_owned(),
    };
    format!("a true heading of {degrees}°{from}{at}")
}

/// What a vehicle's refusal of a calibration from a heading means.
fn heading_refusal(name: &str, receipt: &Receipt) -> String {
    match (receipt.status, receipt.detail.as_deref()) {
        (ReceiptStatus::Rejected, Some("Unsupported")) => {
            format!("{name} can't calibrate from a heading: its firmware does not support it.")
        }
        (ReceiptStatus::Rejected, Some("Failed")) => {
            format!("{name} could not calibrate from that heading.")
        }
        (ReceiptStatus::Rejected, Some(why)) => format!("{name} refused it: {why}."),
        (ReceiptStatus::Timeout, _) => format!("{name} did not answer."),
        (_, Some(why)) => format!("It was not sent to {name}: {why}."),
        _ => format!("{name} did not calibrate."),
    }
}

/// The compasses a vehicle lists in its parameters, in its order.
fn compasses_of(engine: &Engine, vehicle_id: &str) -> Vec<CompassInfo> {
    let value = |name: &str| {
        engine
            .state
            .parameters
            .get(&parameter_id(vehicle_id, name))
            .map(|p| p.value)
    };
    (0..3u8)
        .filter_map(|index| {
            let n = if index == 0 {
                String::new()
            } else {
                (index + 1).to_string()
            };
            let external = if index == 0 {
                "COMPASS_EXTERNAL".to_owned()
            } else {
                format!("COMPASS_EXTERN{n}")
            };
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a device id is a 32-bit integer parameter"
            )]
            let device_id = value(&format!("COMPASS_DEV_ID{n}")).filter(|id| *id > 0.0)? as u32;
            let offsets = ["X", "Y", "Z"]
                .iter()
                .map(|axis| value(&format!("COMPASS_OFS{n}_{axis}")))
                .collect::<Option<Vec<f64>>>()
                .and_then(|all| <[f64; 3]>::try_from(all).ok());
            Some(CompassInfo {
                index,
                device_id,
                used: value(&format!("COMPASS_USE{n}")).is_none_or(|v| v != 0.0),
                external: value(&external).map(|v| v != 0.0),
                offsets,
            })
        })
        .collect()
}

/// A sensor's results in words, for the history: "compass 1 Good (fitness
/// 3.5, offsets 42, -17, 63)".
fn sensor_words(sensor: &SensorProgress) -> String {
    let number = u16::from(sensor.index) + 1;
    let fitness = sensor
        .fitness
        .map(|f| format!("fitness {}", round1(f)))
        .unwrap_or_default();
    match (sensor.state, sensor.failure) {
        (SensorState::Succeeded, _) => {
            let quality = sensor.quality.map_or("Calibrated", Fitness::words);
            let offsets = sensor.offsets.map_or_else(String::new, |[x, y, z]| {
                format!(", offsets {}, {}, {}", round1(x), round1(y), round1(z))
            });
            format!("compass {number} {quality} ({fitness}{offsets})")
        }
        (SensorState::Failed, failure) => {
            let why = failure.unwrap_or(CompassFailure::Failed).words();
            if fitness.is_empty() {
                format!("compass {number} failed: {why}")
            } else {
                format!("compass {number} failed: {why} ({fitness})")
            }
        }
        _ => format!("compass {number} did not finish"),
    }
}

/// One decimal, without a trailing `.0`.
fn round1(value: impl Into<f64>) -> String {
    let rounded = (value.into() * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.1}")
    }
}

impl Engine {
    /// The vehicle's procedure as it stands, journaled or not yet: the one
    /// running, else the one changed last.
    pub(crate) fn current_setup(&self, vehicle_id: &str) -> Option<Setup> {
        self.setups_of(vehicle_id)
            .into_iter()
            .find(|s| s.step.running())
            .or_else(|| {
                let id = self.state.setup_latest.get(vehicle_id)?;
                self.row_setup_by_id(id)
            })
    }

    /// The vehicle's last procedure of a row, journaled or not yet (S189).
    pub(crate) fn row_setup(&self, vehicle_id: &str, row: SetupRow) -> Option<Setup> {
        self.row_setup_by_id(&setup_id(vehicle_id, row))
    }

    fn row_setup_by_id(&self, id: &str) -> Option<Setup> {
        self.setup
            .unsaved
            .get(id)
            .or_else(|| self.state.setups.get(id))
            .cloned()
    }

    /// The vehicle's last procedure of each row, in the tab's order (S189).
    pub(crate) fn setups_of(&self, vehicle_id: &str) -> Vec<Setup> {
        SetupRow::ALL
            .iter()
            .filter_map(|row| self.row_setup(vehicle_id, *row))
            .collect()
    }

    /// The vehicle's procedure if it is running and the vehicle still says
    /// something of it.
    pub(crate) fn running_setup(&self, vehicle_id: &str) -> Option<Setup> {
        let now = self.clock.now_us();
        // An accelerometer calibration waits for the person as long as it
        // takes, the vehicle asking all the while; so does the radio's,
        // which the vehicle knows nothing of (S187). A motor test runs
        // while it is held, however long (S188).
        self.current_setup(vehicle_id).filter(|s| {
            if s.kind == SetupKind::Motors {
                return s.step.running() && self.setup.spins.contains_key(vehicle_id);
            }
            // An install runs while the vehicle is silent, restarting
            // (S189).
            if s.kind == SetupKind::Firmware {
                return s.step.running();
            }
            // A ratio runs as long as the flight circles; a check has its
            // own time (S190).
            if matches!(s.kind, SetupKind::AirspeedRatio | SetupKind::AirspeedCheck) {
                return s.step.running();
            }
            s.step.running()
                && (now.saturating_sub(s.updated_at_us) < STALE_US
                    || (s.kind == SetupKind::Accelerometer && s.position.is_some())
                    || s.kind == SetupKind::Radio)
        })
    }

    /// Why a procedure cannot start on this vehicle now, in words.
    fn cannot_set_up(&self, vehicle: &Vehicle) -> Option<String> {
        if let Some(why) = self.not_writable(&vehicle.link_id) {
            return Some(format!("Can't set up {}: {why}.", vehicle.name));
        }
        if !set_up(&vehicle.autopilot) {
            return Some(format!(
                "UXDuck sets up ArduPilot and PX4 vehicles, and {} runs neither.",
                vehicle.name
            ));
        }
        if vehicle.presence != Presence::Online {
            return Some(format!("UXDuck can't hear {} right now.", vehicle.name));
        }
        if vehicle.armed {
            return Some(format!(
                "Disarm {} first: it is set up only on the ground.",
                vehicle.name
            ));
        }
        None
    }

    /// Journals a procedure now, or keeps it until progress is due. Returns
    /// it as it is now.
    fn save_setup(&mut self, mut setup: Setup, now: bool, actor: Actor) -> Setup {
        let id = setup.id();
        let saved = self.state.setups.get(&id);
        let due = now
            || saved.is_none_or(|s| {
                setup.updated_at_us.saturating_sub(s.updated_at_us) >= PROGRESS_EVERY_US
            });
        setup.version = saved.map_or(1, |s| s.version + 1);
        if due {
            self.setup.unsaved.remove(&id);
            self.commit_as(
                actor,
                Change::SetupUpdated {
                    setup: setup.clone(),
                },
            );
        } else {
            self.setup.unsaved.insert(id, setup.clone());
        }
        setup
    }

    /// Sends a procedure's command with a receipt, journaled like any.
    fn send_setup_command(
        &mut self,
        vehicle: &Vehicle,
        actor: Actor,
        receipt_id: String,
        command: Command,
        order: Order,
    ) {
        let receipt = Receipt {
            id: receipt_id.clone(),
            vehicle_id: vehicle.id.clone(),
            command: command.clone(),
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            preflight: Vec::new(),
            preflight_override: None,
            sent_without_recent_heartbeat: vehicle.presence != Presence::Online,
            forced: false,
            attempts: 0,
            client_id: actor.client_id.clone(),
            operator_id: actor.operator_id.clone(),
            remote: None,
            created_at_us: self.clock.now_us(),
            completed_at_us: None,
            version: 1,
        };
        self.commit_as(actor, Change::ReceiptUpserted { receipt });
        self.setup
            .orders
            .insert(receipt_id.clone(), (vehicle.id.clone(), order));
        self.links.command(CommandOrder {
            receipt_id,
            vehicle_id: vehicle.id.clone(),
            command,
            force: false,
        });
    }

    /// A command ended: if it was a procedure's, moves the procedure on
    /// and says true, so it is not also told as a refused command.
    pub(crate) fn setup_order_done(&mut self, receipt: &Receipt) -> bool {
        let Some((vehicle_id, order)) = self.setup.orders.remove(&receipt.id) else {
            return false;
        };
        // Cancelled here already, whatever the vehicle answers.
        if matches!(order, Order::Cancel | Order::Rate) {
            return true;
        }
        if order == Order::Stop {
            self.motor_stopped(&vehicle_id);
            return true;
        }
        if order == Order::Placed {
            self.placed_answered(&vehicle_id, receipt);
            return true;
        }
        let Some(mut setup) = self
            .setups_of(&vehicle_id)
            .into_iter()
            .find(|s| s.receipt_id.as_deref() == Some(&receipt.id))
        else {
            return true;
        };
        // A motor test's start, answered after it was let go too (S188).
        if setup.kind == SetupKind::Motors {
            self.motor_started(setup, receipt);
            return true;
        }
        // The restart into the bootloader (S189).
        if setup.kind == SetupKind::Firmware {
            self.firmware_restart_answered(&setup, receipt);
            return true;
        }
        if setup.step != SetupStep::Starting {
            return true;
        }
        // PX4 answers before it calibrates, and says the rest (S191).
        if self.vehicle(&vehicle_id).is_some_and(px4::px4)
            && matches!(
                setup.kind,
                SetupKind::Compass
                    | SetupKind::CompassHeading
                    | SetupKind::Accelerometer
                    | SetupKind::Level
                    | SetupKind::AirspeedZero
            )
        {
            self.px4_answered(setup, receipt);
            return true;
        }
        match setup.kind {
            SetupKind::CompassHeading => {
                self.heading_answered(setup, receipt);
                return true;
            }
            SetupKind::Level => {
                self.level_answered(setup, receipt);
                return true;
            }
            SetupKind::Radio => {
                self.radio_answered(setup, receipt);
                return true;
            }
            SetupKind::AirspeedZero => {
                self.zero_answered(setup, receipt);
                return true;
            }
            SetupKind::Motors
            | SetupKind::Firmware
            | SetupKind::AirspeedCheck
            | SetupKind::AirspeedRatio => return true,
            SetupKind::Compass | SetupKind::Accelerometer => {}
        }
        let now = self.clock.now_us();
        let name = self
            .vehicle(&vehicle_id)
            .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone());
        if receipt.status == ReceiptStatus::Accepted {
            setup.step = SetupStep::Running;
            // The accelerometer waits for the vehicle to ask for a position.
            setup.instruction =
                (setup.kind == SetupKind::Compass).then_some(Instruction::TurnAround);
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            setup.detail = Some(if setup.kind == SetupKind::Accelerometer {
                accel_refusal(&name, setup.kind, receipt)
            } else {
                receipt
                    .detail
                    .clone()
                    .unwrap_or_else(|| "The vehicle did not start it.".into())
            });
            setup.finished_at_us = Some(now);
        }
        setup.updated_at_us = now;
        self.save_setup(setup, true, actor());
        true
    }

    /// The vehicle answered a calibration from a heading: it is done, well
    /// or not, and in the history either way (V13).
    fn heading_answered(&mut self, mut setup: Setup, receipt: &Receipt) {
        let Some(vehicle) = self.vehicle(&setup.vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let accepted = receipt.status == ReceiptStatus::Accepted;
        let words = setup
            .heading
            .as_ref()
            .map(heading_words)
            .unwrap_or_default();
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        setup.progress = 100;
        let text = if accepted {
            setup.step = SetupStep::Succeeded;
            setup.instruction = None;
            format!("Compasses calibrated from {words}")
        } else {
            let refusal = heading_refusal(&vehicle.name, receipt);
            setup.step = SetupStep::Failed;
            // Trying again does nothing for firmware without it.
            setup.instruction =
                (receipt.detail.as_deref() != Some("Unsupported")).then_some(Instruction::TryAgain);
            let text = format!("Compass calibration from {words} failed: {refusal}");
            setup.detail = Some(refusal);
            text
        };
        // The vehicle answered: a refusal by it is a failed calibration;
        // one that never reached it is not one.
        if accepted || receipt.status == ReceiptStatus::Rejected {
            let event = HistoryEvent::Calibrated {
                kind: setup.kind,
                succeeded: accepted,
                text,
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        }
        self.save_setup(setup, true, actor());
        // ArduPilot uses the new offsets at once: what it said of them
        // before no longer stands, and its parameters have new offsets.
        if accepted {
            self.forget_prearm(&vehicle.id, SetupRow::Compass);
            self.download_parameters(&vehicle.id);
        }
    }

    /// The vehicle answered level (S186): it is done, well or not, and in
    /// the history either way (V13). What it said of the trim just before
    /// its answer ("Trim OK: roll=0.12 pitch=-0.40", "trim over maximum of
    /// 10 degrees") is in `detail` by now.
    fn level_answered(&mut self, mut setup: Setup, receipt: &Receipt) {
        let Some(vehicle) = self.vehicle(&setup.vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let accepted = receipt.status == ReceiptStatus::Accepted;
        let said = setup.detail.take();
        setup.updated_at_us = now;
        setup.finished_at_us = Some(now);
        setup.progress = 100;
        let text = if accepted {
            setup.step = SetupStep::Succeeded;
            setup.instruction = None;
            match &said {
                Some(said) => format!("Level set ({said})"),
                None => "Level set".to_owned(),
            }
        } else {
            setup.step = SetupStep::Failed;
            let refusal = match (&said, receipt.status) {
                (Some(said), ReceiptStatus::Rejected) => {
                    format!("{} could not take this as level: {said}.", vehicle.name)
                }
                _ => accel_refusal(&vehicle.name, setup.kind, receipt),
            };
            setup.instruction =
                (receipt.detail.as_deref() != Some("Unsupported")).then_some(Instruction::SetLevel);
            let text = format!("Level failed: {refusal}");
            setup.detail = Some(refusal);
            text
        };
        if accepted || receipt.status == ReceiptStatus::Rejected {
            let event = HistoryEvent::Calibrated {
                kind: setup.kind,
                succeeded: accepted,
                text,
            };
            match setup.started_by.clone() {
                Some(author) => self.record_by(&vehicle.id, event, author),
                None => self.record(&vehicle.id, event),
            }
        }
        self.save_setup(setup, true, actor());
        // Its trim parameters changed.
        if accepted {
            self.download_parameters(&vehicle.id);
        }
    }

    /// The vehicle answered a position confirmed (S186). Taken, it measures
    /// it and asks for the next; refused, the person is asked again.
    fn placed_answered(&mut self, vehicle_id: &str, receipt: &Receipt) {
        if receipt.status == ReceiptStatus::Accepted {
            return;
        }
        let Some(mut setup) = self
            .current_setup(vehicle_id)
            .filter(|s| s.step.running() && s.kind == SetupKind::Accelerometer && s.placed)
        else {
            return;
        };
        let name = self
            .vehicle(vehicle_id)
            .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone());
        setup.placed = false;
        setup.instruction = Some(Instruction::PlaceAsShown);
        setup.detail = Some(match receipt.status {
            ReceiptStatus::Timeout => format!("{name} did not answer. Press Done again."),
            ReceiptStatus::Rejected => {
                format!("{name} did not take that position. Hold it still and press Done again.")
            }
            _ => format!(
                "It was not sent to {name}: {}.",
                receipt.detail.as_deref().unwrap_or("not sent")
            ),
        });
        setup.updated_at_us = self.clock.now_us();
        self.save_setup(setup, true, actor());
    }

    /// The vehicle's accelerometer calibration asks for a position, or says
    /// it succeeded or failed (S186). Repeated once a second, so a request
    /// for the position already shown changes nothing. Asked with nothing
    /// running here (UXDuck restarted during it, or another ground station
    /// on the same link started it), it is followed from there.
    pub fn accel_cal_position(&mut self, vehicle_id: &str, position: u32) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let current = self.current_setup(&vehicle.id);
        let running = current
            .clone()
            .filter(|s| s.step.running() && s.kind == SetupKind::Accelerometer);
        let Some(asked) = VehiclePosition::from_mavlink(position) else {
            if let Some(mut setup) = running {
                match position {
                    ACCELCAL_SUCCESS => self.finish_accel(&mut setup, &vehicle, true),
                    ACCELCAL_FAILED => self.finish_accel(&mut setup, &vehicle, false),
                    _ => return,
                }
                self.save_setup(setup, true, actor());
            }
            return;
        };
        let mut setup = match (running, current) {
            (Some(setup), _) => setup,
            // Cancelled or ended here, the vehicle asks on until it
            // restarts; a procedure of another kind is left alone.
            (None, Some(other))
                if other.kind == SetupKind::Accelerometer || other.step.running() =>
            {
                return;
            }
            // Found calibrating, started here before UXDuck restarted.
            _ => Setup {
                vehicle_id: vehicle.id.clone(),
                kind: SetupKind::Accelerometer,
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
                airspeed: None,
                started_by: None,
                receipt_id: None,
                started_at_us: now,
                updated_at_us: now,
                finished_at_us: None,
                version: 0,
            },
        };
        if setup.position == Some(asked) {
            // Asked again long after it was confirmed: it did not take it.
            if !(setup.placed && now.saturating_sub(setup.updated_at_us) >= ASKED_AGAIN_US) {
                return;
            }
            setup.detail = Some(format!(
                "{} asked for this position again. Hold it still and press Done.",
                vehicle.name
            ));
        } else {
            setup.detail = None;
        }
        setup.step = SetupStep::Running;
        setup.position = Some(asked);
        setup.placed = false;
        setup.instruction = Some(Instruction::PlaceAsShown);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a percentage of six positions"
        )]
        let progress = (positions_done(&setup) * 100 / VehiclePosition::ALL.len()) as u8;
        setup.progress = progress;
        setup.updated_at_us = now;
        self.save_setup(setup, true, actor());
    }

    /// The accelerometer calibration ended (S186): succeeded, it needs a
    /// restart, as `ArduPilot` uses new accelerometer offsets only then;
    /// either way the vehicle's history says how (V13).
    fn finish_accel(&mut self, setup: &mut Setup, vehicle: &Vehicle, succeeded: bool) {
        let now = self.clock.now_us();
        let done = positions_done(setup);
        setup.finished_at_us = Some(now);
        setup.updated_at_us = now;
        setup.position = None;
        setup.placed = false;
        let text = if succeeded {
            setup.step = SetupStep::Succeeded;
            setup.progress = 100;
            setup.restart_needed = true;
            setup.instruction = Some(Instruction::Restart);
            setup.detail = None;
            "Accelerometers calibrated in six positions".to_owned()
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = Some(Instruction::TryAgain);
            let why = format!(
                "{}'s accelerometer calibration failed. Hold it still in each position, and \
                 try again.",
                vehicle.name
            );
            setup.detail = Some(why);
            format!("Accelerometer calibration failed after {done} of 6 positions")
        };
        let event = HistoryEvent::Calibrated {
            kind: setup.kind,
            succeeded,
            text,
        };
        match setup.started_by.clone() {
            Some(author) => self.record_by(&vehicle.id, event, author),
            None => self.record(&vehicle.id, event),
        }
        if succeeded {
            self.forget_prearm(&vehicle.id, SetupRow::Accelerometer);
            self.download_parameters(&vehicle.id);
        }
    }

    /// What a vehicle says of its accelerometer calibration or level
    /// (S186), while one runs or just after: its prompts ("Place vehicle
    /// level and press any key.") belong to the row, so they are kept in
    /// the log without a notice (true); its "Calibration successful" or
    /// "Calibration FAILED" ends one still running; its words on the trim
    /// become level's.
    fn accel_said(&mut self, setup: &mut Setup, vehicle_id: &str, text: &str, now: u64) -> bool {
        let lately = setup.step.running()
            || setup
                .finished_at_us
                .is_some_and(|at| now.saturating_sub(at) < SAID_US);
        if !lately {
            return false;
        }
        let lower = text.to_lowercase();
        if setup.kind == SetupKind::Level {
            if !lower.contains("trim") {
                return false;
            }
            let said = text.trim().trim_end_matches('.').to_owned();
            if setup.step == SetupStep::Starting {
                setup.detail = Some(said);
            } else if setup.step == SetupStep::Failed && !said.starts_with("Trim OK") {
                // Said just after its refusal, as SITL does: its words
                // replace UXDuck's.
                let name = self
                    .vehicle(vehicle_id)
                    .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone());
                setup.detail = Some(format!("{name} could not take this as level: {said}."));
            } else {
                return true;
            }
            setup.updated_at_us = now;
            self.save_setup(setup.clone(), true, actor());
            return true;
        }
        let ended = match lower.trim() {
            "calibration successful" => Some(true),
            "calibration failed" => Some(false),
            _ => None,
        };
        if let Some(succeeded) = ended
            && setup.step.running()
            && let Some(vehicle) = self.vehicle(vehicle_id).cloned()
        {
            self.finish_accel(setup, &vehicle, succeeded);
            self.save_setup(setup.clone(), true, actor());
        }
        ended.is_some() || lower.starts_with("place vehicle")
    }

    /// A vehicle said something: just after it refused a calibration from
    /// a heading, its `Mag…` message says why ("Mag: no position
    /// available"), and becomes the row's words. What it says of its
    /// accelerometer calibration or level goes to `accel_said`. True when
    /// the words are the procedure's own and need no notice of their own.
    pub(crate) fn setup_said(&mut self, vehicle_id: &str, text: &str) -> bool {
        // PX4 says its calibrations in its own words (S191).
        if self.vehicle(vehicle_id).is_some_and(px4::px4)
            && let Some(ours) = self.px4_said(vehicle_id, text)
        {
            return ours;
        }
        let Some(mut setup) = self.current_setup(vehicle_id) else {
            return false;
        };
        let now = self.clock.now_us();
        if matches!(setup.kind, SetupKind::Accelerometer | SetupKind::Level) {
            return self.accel_said(&mut setup, vehicle_id, text, now);
        }
        if matches!(setup.kind, SetupKind::Radio | SetupKind::Firmware) {
            return false;
        }
        if setup.kind == SetupKind::Motors {
            return self.motors_said(&mut setup, text);
        }
        if setup.kind.row() == SetupRow::Airspeed {
            return self.airspeed_said(&mut setup, text);
        }
        let just_failed = setup.kind == SetupKind::CompassHeading
            && setup.step == SetupStep::Failed
            && setup
                .finished_at_us
                .is_some_and(|at| now.saturating_sub(at) < SAID_US);
        if !just_failed || !text.starts_with("Mag") {
            return false;
        }
        let name = self
            .vehicle(vehicle_id)
            .map_or_else(|| "The vehicle".to_owned(), |v| v.name.clone());
        if text.to_lowercase().contains("no position") {
            setup.detail = Some(format!("{name} has no position to calibrate from."));
            setup.instruction = Some(Instruction::GivePosition);
        } else {
            setup.detail = Some(format!("{name} could not calibrate: {}", text.trim()));
        }
        setup.updated_at_us = now;
        self.save_setup(setup, true, actor());
        false
    }

    /// The compass calibration a report is about: the one running, or one
    /// the vehicle was found doing. None for what to ignore.
    fn calibrating(&self, vehicle_id: &str, report: &MagCal, now: u64) -> Option<Setup> {
        // Another procedure running: a stray report starts nothing.
        if self
            .current_setup(vehicle_id)
            .is_some_and(|s| s.step.running() && s.kind != SetupKind::Compass)
        {
            return None;
        }
        match self.row_setup(vehicle_id, SetupRow::Compass) {
            Some(setup) if setup.step.running() && setup.kind == SetupKind::Compass => Some(setup),
            // A finished one's result, repeated until the vehicle restarts.
            _ if matches!(report, MagCal::Report { .. }) => None,
            // What the vehicle said on its way to stopping, just after one
            // ended here.
            Some(setup)
                if setup
                    .finished_at_us
                    .is_some_and(|at| now.saturating_sub(at) < STALE_US) =>
            {
                None
            }
            // Found calibrating, started by another ground station.
            _ => Some(Setup {
                vehicle_id: vehicle_id.to_owned(),
                kind: SetupKind::Compass,
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
                airspeed: None,
                started_by: None,
                receipt_id: None,
                started_at_us: now,
                updated_at_us: now,
                finished_at_us: None,
                version: 0,
            }),
        }
    }

    /// A vehicle's compass calibration said how it goes (S184).
    pub fn mag_cal(&mut self, vehicle_id: &str, report: MagCal) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let now = self.clock.now_us();
        let Some(mut setup) = self.calibrating(&vehicle.id, &report, now) else {
            return;
        };
        let mut at_once = setup.step != SetupStep::Running || setup.sensors.is_empty();
        setup.step = SetupStep::Running;
        setup.instruction = Some(Instruction::TurnAround);
        let (compass_id, cal_mask) = match &report {
            MagCal::Progress {
                compass_id,
                cal_mask,
                ..
            }
            | MagCal::Report {
                compass_id,
                cal_mask,
                ..
            } => (*compass_id, *cal_mask | (1 << compass_id.min(&7))),
        };
        for index in (0..8u8).filter(|i| cal_mask & (1 << i) != 0) {
            if !setup.sensors.iter().any(|s| s.index == index) {
                setup.sensors.push(SensorProgress::waiting(index));
            }
        }
        setup.sensors.sort_by_key(|s| s.index);
        let Some(sensor) = setup.sensors.iter_mut().find(|s| s.index == compass_id) else {
            return;
        };
        if sensor.finished() {
            return;
        }
        match report {
            MagCal::Progress {
                attempt,
                completion_pct,
                completion_mask,
                direction,
                ..
            } => {
                sensor.state = SensorState::Running;
                sensor.progress = completion_pct.min(100);
                sensor.sections = completion_mask.to_vec();
                sensor.direction = Some(direction);
                sensor.attempt = attempt;
            }
            MagCal::Report {
                cal_status,
                autosaved,
                fitness,
                offsets,
                ..
            } => {
                sensor.progress = 100;
                sensor.direction = None;
                sensor.fitness = Some(fitness);
                sensor.quality = Some(Fitness::of(fitness));
                if cal_status == MAG_CAL_SUCCESS {
                    sensor.state = SensorState::Succeeded;
                    sensor.offsets = Some(offsets);
                    sensor.autosaved = autosaved;
                } else {
                    sensor.state = SensorState::Failed;
                    sensor.failure = Some(
                        CompassFailure::from_status(cal_status).unwrap_or(CompassFailure::Failed),
                    );
                }
                at_once = true;
            }
        }
        setup.progress = setup.overall();
        setup.updated_at_us = now;
        if setup.sensors.iter().all(SensorProgress::finished) {
            self.finish_setup(&mut setup, &vehicle);
        }
        self.save_setup(setup, at_once, actor());
    }

    /// Every sensor finished: the procedure succeeded or failed, and the
    /// vehicle's history says how (V13).
    fn finish_setup(&mut self, setup: &mut Setup, vehicle: &Vehicle) {
        let now = self.clock.now_us();
        let succeeded = setup
            .sensors
            .iter()
            .all(|s| s.state == SensorState::Succeeded);
        setup.finished_at_us = Some(now);
        setup.progress = 100;
        if succeeded {
            setup.step = SetupStep::Succeeded;
            // ArduPilot uses new offsets only once it restarts.
            setup.restart_needed = true;
            setup.instruction = Some(Instruction::Restart);
        } else {
            setup.step = SetupStep::Failed;
            setup.instruction = setup
                .sensors
                .iter()
                .find_map(|s| s.failure)
                .map_or(Some(Instruction::TryAgain), |f| Some(f.instruction()));
        }
        let results: Vec<String> = setup.sensors.iter().map(sensor_words).collect();
        let text = if succeeded {
            format!("Compasses calibrated: {}", results.join("; "))
        } else {
            format!("Compass calibration failed: {}", results.join("; "))
        };
        let event = HistoryEvent::Calibrated {
            kind: setup.kind,
            succeeded,
            text,
        };
        match setup.started_by.clone() {
            Some(author) => self.record_by(&vehicle.id, event, author),
            None => self.record(&vehicle.id, event),
        }
    }

    /// A vehicle restarted: every calibration waiting on it is in use now,
    /// and one running when it restarted stopped.
    pub(crate) fn setup_restarted(&mut self, vehicle_id: &str) {
        let mut all = self.setups_of(vehicle_id);
        // The one running is ended last, so it stays the latest.
        all.sort_by_key(|s| s.step.running());
        for mut setup in all {
            let now = self.clock.now_us();
            // Restarting is how an install ends (S189).
            if setup.kind == SetupKind::Firmware {
                continue;
            }
            // A ratio turns autocal off, which the vehicle kept (S190).
            if setup.kind == SetupKind::AirspeedRatio
                && setup.step.running()
                && self.airspeed_restarted(vehicle_id)
            {
                continue;
            }
            if setup.step.running() {
                // A motor held stops with it: its test ended as it restarted.
                if self.setup.spins.remove(vehicle_id).is_some() {
                    setup.step = SetupStep::Cancelled;
                    setup.detail = Some("Stopped: it restarted.".into());
                    setup.finished_at_us = Some(now);
                    setup.updated_at_us = now;
                    self.save_setup(setup, true, actor());
                    continue;
                }
                setup.step = SetupStep::Cancelled;
                setup.instruction = Some(Instruction::TryAgain);
                setup.detail = Some("It restarted during the calibration.".into());
                setup.finished_at_us = Some(now);
                setup.position = None;
                setup.placed = false;
                self.setup.radio_from.remove(vehicle_id);
            } else if setup.restart_needed {
                setup.restart_needed = false;
                setup.instruction = None;
            } else {
                continue;
            }
            setup.updated_at_us = now;
            self.save_setup(setup, true, actor());
        }
    }

    /// `setup/state`: the vehicle's rows, its compasses and its procedure.
    #[allow(clippy::too_many_lines, reason = "one row after another")]
    pub(crate) fn setup_state(&self, vehicle_id: &str) -> Result<SetupStateResult, ApiError> {
        let vehicle = self
            .vehicle(vehicle_id)
            .ok_or_else(|| no_vehicle(vehicle_id))?;
        let procedure = self.current_setup(&vehicle.id);
        let last = |row: SetupRow| self.row_setup(&vehicle.id, row);
        let px4 = px4::px4(vehicle);
        let compasses = if px4 {
            self.px4_compasses(&vehicle.id)
        } else {
            compasses_of(self, &vehicle.id)
        };
        let said: Vec<(String, SetupRow, Fix)> = self
            .prearm_reasons(&vehicle.id)
            .into_iter()
            .filter_map(|reason| {
                fix_for(&vehicle.autopilot, &reason).map(|(row, fix)| (reason, row, fix))
            })
            .collect();
        let says = |row: SetupRow, fix: Fix| {
            said.iter()
                .find(|(_, r, f)| *r == row && *f == fix)
                .map(|(reason, ..)| reason.clone())
        };
        // A row's procedure is offered for ArduPilot and PX4 (S191).
        let ardupilot = set_up(&vehicle.autopilot);
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
        };
        let row = |row: SetupRow, state: RowState| SetupRowState {
            row: row.into(),
            state,
            calibrated_at_us: None,
            detail: None,
            procedure: None,
        };
        let mut rows = Vec::new();
        let installing = last(SetupRow::Firmware).is_some_and(|p| p.step.running());
        rows.push(SetupRowState {
            detail: vehicle.firmware.clone(),
            procedure: ardupilot.then_some(SetupKind::Firmware.into()),
            calibrated_at_us: self.calibrated_at(vehicle, SetupRow::Firmware),
            ..row(
                SetupRow::Firmware,
                if installing {
                    RowState::Running
                } else if vehicle.firmware.is_some() {
                    RowState::Installed
                } else {
                    RowState::Unknown
                },
            )
        });
        // The accelerometer: what the vehicle says, else ArduPilot's own
        // rule, that offsets of exactly zero were never calibrated; PX4's,
        // that `CAL_ACC0_ID` is 0 (given as one zero "offset").
        let accel_offsets: Option<Vec<f64>> = if px4 {
            self.px4_accel_calibrated(&vehicle.id)
                .map(|calibrated| vec![if calibrated { 1.0 } else { 0.0 }])
        } else {
            ["X", "Y", "Z"]
                .iter()
                .map(|axis| param(&format!("INS_ACCOFFS_{axis}")))
                .collect()
        };
        rows.push(self.accel_row(
            vehicle,
            last(SetupRow::Accelerometer).as_ref(),
            accel_offsets.as_deref(),
            &says,
            ardupilot,
        ));
        rows.push(self.compass_row(
            vehicle,
            last(SetupRow::Compass).as_ref(),
            &compasses,
            &says,
            (ardupilot, !px4),
        ));
        rows.push(self.radio_row(vehicle, last(SetupRow::Radio).as_ref(), &says, ardupilot));
        // The airspeed, for a plane with a sensor enabled (S190).
        if let Some(sensor) = self
            .airspeed_sensor(vehicle)
            .filter(|s| *s != airspeed::Sensor::Absent)
        {
            rows.push(self.airspeed_row(vehicle, last(SetupRow::Airspeed).as_ref(), sensor));
        }
        // The motors, for a vehicle UXDuck tests them on (S188).
        let motors = self.motors_of(vehicle);
        if motors.is_some() {
            let running = last(SetupRow::Motors).is_some_and(|p| p.step.running());
            rows.push(SetupRowState {
                row: SetupRow::Motors.into(),
                state: if running {
                    RowState::Running
                } else {
                    RowState::Unknown
                },
                calibrated_at_us: None,
                detail: None,
                procedure: Some(SetupKind::Motors.into()),
            });
        }
        Ok(SetupStateResult {
            vehicle_id: vehicle.id.clone(),
            rows,
            compasses,
            motors: motors
                .and_then(motors::Motors::drawn)
                .as_ref()
                .map(Into::into),
            procedure: procedure.as_ref().map(Into::into),
            procedures: self.setups_of(&vehicle.id).iter().map(Into::into).collect(),
        })
    }

    /// When a procedure of `row` last succeeded in UXDuck, from the
    /// vehicle's history.
    fn calibrated_at(&self, vehicle: &Vehicle, row: SetupRow) -> Option<u64> {
        self.state
            .history
            .values()
            .filter(|e| e.vehicle_id == vehicle.id || vehicle.aliases.contains(&e.vehicle_id))
            .filter(|e| e.deleted.is_none())
            .filter(|e| {
                matches!(
                    &e.kind,
                    EntryKind::Event {
                        event: HistoryEvent::Calibrated {
                            kind,
                            succeeded: true,
                            ..
                        }
                    } if kind.row() == row
                )
            })
            .map(|e| e.happened_at_us)
            .max()
    }

    /// The accelerometer's row (S186): running, needing a restart or a
    /// calibration as the vehicle says, else by its offsets. Level counts
    /// as calibrating it only for when; it does not make an uncalibrated
    /// one calibrated.
    fn accel_row(
        &self,
        vehicle: &Vehicle,
        procedure: Option<&Setup>,
        offsets: Option<&[f64]>,
        says: &dyn Fn(SetupRow, Fix) -> Option<String>,
        ardupilot: bool,
    ) -> SetupRowState {
        let calibrated_at_us = self.calibrated_at(vehicle, SetupRow::Accelerometer);
        let accel = procedure.filter(|p| p.kind.row() == SetupRow::Accelerometer);
        let full = accel.filter(|p| p.kind == SetupKind::Accelerometer);
        let (state, detail) = if accel.is_some_and(|p| p.step.running()) {
            (RowState::Running, None)
        } else if let Some(reason) = says(SetupRow::Accelerometer, Fix::Restart) {
            (RowState::NeedsRestart, Some(reason))
        } else if full.is_some_and(|p| p.restart_needed) {
            (RowState::NeedsRestart, None)
        } else if let Some(reason) = says(SetupRow::Accelerometer, Fix::Calibrate) {
            (RowState::NotCalibrated, Some(reason))
        } else if full.is_some_and(|p| p.step == SetupStep::Succeeded) {
            (RowState::Calibrated, None)
        } else {
            match offsets {
                Some(all) if all.iter().all(|v| *v == 0.0) => (RowState::NotCalibrated, None),
                Some(_) => (RowState::Calibrated, None),
                None => (RowState::Unknown, None),
            }
        };
        SetupRowState {
            row: SetupRow::Accelerometer.into(),
            state,
            calibrated_at_us,
            detail,
            procedure: ardupilot.then_some(SetupKind::Accelerometer.into()),
        }
    }

    fn compass_row(
        &self,
        vehicle: &Vehicle,
        procedure: Option<&Setup>,
        compasses: &[CompassInfo],
        says: &dyn Fn(SetupRow, Fix) -> Option<String>,
        (offered, zero_uncalibrated): (bool, bool),
    ) -> SetupRowState {
        let ardupilot = offered;
        let calibrated_at_us = self.calibrated_at(vehicle, SetupRow::Compass);
        let compass = procedure.filter(|p| p.kind.row() == SetupRow::Compass);
        // ArduPilot's rule; PX4 keeps offsets of 0 for a compass it has
        // calibrated as needing none, and lists only those it calibrated.
        let uncalibrated = zero_uncalibrated
            && compasses.iter().any(|c| {
                c.used
                    && c.offsets
                        .is_some_and(|offsets| offsets.iter().all(|v| *v == 0.0))
            });
        let (state, detail) = if compass.is_some_and(|p| p.step.running()) {
            (RowState::Running, None)
        } else if let Some(reason) = says(SetupRow::Compass, Fix::Restart) {
            (RowState::NeedsRestart, Some(reason))
        } else if compass.is_some_and(|p| p.restart_needed) {
            (RowState::NeedsRestart, None)
        } else if let Some(reason) = says(SetupRow::Compass, Fix::Calibrate) {
            (RowState::NotCalibrated, Some(reason))
        } else if compass.is_some_and(|p| p.step == SetupStep::Succeeded) {
            // Before its parameters are read again with the new offsets.
            (RowState::Calibrated, None)
        } else if uncalibrated {
            (RowState::NotCalibrated, None)
        } else if calibrated_at_us.is_some() || compasses.iter().any(|c| c.offsets.is_some()) {
            (RowState::Calibrated, None)
        } else {
            (RowState::Unknown, None)
        };
        SetupRowState {
            row: SetupRow::Compass.into(),
            state,
            calibrated_at_us,
            detail,
            procedure: ardupilot.then_some(SetupKind::Compass.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_read_in_words() {
        let mut good = SensorProgress::waiting(0);
        good.state = SensorState::Succeeded;
        good.fitness = Some(3.54);
        good.quality = Some(Fitness::Good);
        good.offsets = Some([42.5, -17.25, 63.0]);
        assert_eq!(
            sensor_words(&good),
            "compass 1 Good (fitness 3.5, offsets 42.5, -17.3, 63)"
        );
        let mut bad = SensorProgress::waiting(1);
        bad.state = SensorState::Failed;
        bad.fitness = Some(31.5);
        bad.failure = Some(CompassFailure::BadOrientation);
        assert_eq!(
            sensor_words(&bad),
            "compass 2 failed: its orientation looks wrong (fitness 31.5)"
        );
    }
}

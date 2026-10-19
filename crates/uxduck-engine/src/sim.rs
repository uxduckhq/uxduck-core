//! Simulated vehicles (ADR-0011): `simulator` links, which the installed
//! daemon runs from a SITL build (S17) and a browser from ArduPilot's
//! WebAssembly build (S20). A simulated vehicle is its link: starting one
//! adds the link, and stopping it removes the link.

use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::link::{IdParams, Link as LinkDto, LinkAddParams};
use uxduck_contracts::dto::sim::{SimBinariesResult, SimBuildParams, SimForward, SimStartParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::{JET_FRAME, Link, LinkKind, LinkStatus, SimAutopilot, SimVehicle};
use uxduck_core::events::Change;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Simulated vehicles one installed daemon runs at once. Each is a whole
/// autopilot; the instance also sets its ports (5760 + 10 per instance).
pub const MAX_INSTANCES: u8 = 32;

/// How many simulated vehicles a browser runs at once.
pub const MAX_IN_BROWSER: usize = 2;

/// The heights a simulated vehicle may start in the air at (S33), in metres
/// above the ground.
pub const MIN_IN_AIR_M: f64 = 10.0;
pub const MAX_IN_AIR_M: f64 = 500.0;

/// `ArduPlane` frames the take-off cannot fly (docs/simulation-frames.md): a
/// glider has no motor, the stratospheric blimp floats, and a petrol
/// engine waits to be started. Each stays on the ground until 2 minutes
/// later "it did not climb" (2026-10-18).
pub const GROUND_ONLY: [&str; 3] = ["glider", "stratoblimp", "plane-ice"];

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// Whether a vehicle can start in the air at a height: one that flies, on a
/// frame the take-off can fly.
fn check_in_air(vehicle: SimVehicle, frame: Option<&str>, height_m: f64) -> Result<(), ApiError> {
    if matches!(
        vehicle,
        SimVehicle::Rover | SimVehicle::Boat | SimVehicle::Submarine | SimVehicle::Airship
    ) {
        return Err(invalid(
            "this vehicle starts at its selected location. Automatic take-off is unavailable",
        ));
    }
    if let Some(frame) = frame.filter(|f| GROUND_ONLY.contains(f)) {
        return Err(invalid(format!(
            "{frame} cannot take off by itself. Start it on the ground"
        )));
    }
    if !(MIN_IN_AIR_M..=MAX_IN_AIR_M).contains(&height_m) {
        return Err(invalid(format!(
            "a vehicle starts in the air at {MIN_IN_AIR_M} to {MAX_IN_AIR_M} m"
        )));
    }
    Ok(())
}

/// Checks a new simulator link against the host and the others. Returns
/// its default name.
pub(crate) fn check(engine: &Engine, links: &[&Link], kind: &LinkKind) -> Result<String, ApiError> {
    let LinkKind::Simulator {
        vehicle,
        autopilot,
        latitude_deg,
        longitude_deg,
        altitude_m,
        heading_deg,
        frame,
        speedup,
        in_air_m,
        binary,
        ..
    } = kind
    else {
        return Err(invalid("not a simulated vehicle"));
    };
    let capabilities = &engine.config.capabilities;
    if !capabilities.simulator {
        return Err(invalid("this UXDuck does not run simulated vehicles"));
    }
    let placed = latitude_deg.abs() <= 90.0
        && longitude_deg.abs() <= 180.0
        && altitude_m.is_finite()
        && heading_deg.is_finite();
    if !placed {
        return Err(invalid("a simulated vehicle needs a place on the map"));
    }
    if speedup.is_some_and(|s| !(1..=10).contains(&s)) {
        return Err(invalid("the speed-up is from 1 to 10 times real time"));
    }
    if let Some(height_m) = in_air_m {
        check_in_air(*vehicle, frame.as_deref(), *height_m)?;
    }
    if *autopilot == SimAutopilot::Px4 {
        if capabilities.runs_in == RunsIn::Browser {
            return Err(invalid(
                "UXDuck Web runs ArduPilot only. PX4 runs in UXDuck Desktop",
            ));
        }
        if matches!(
            vehicle,
            SimVehicle::Boat | SimVehicle::Submarine | SimVehicle::Airship | SimVehicle::Jet
        ) {
            return Err(invalid(
                "PX4 flies a multicopter, plane, VTOL or rover here: not a boat, submarine, blimp or jet",
            ));
        }
    }
    // The jet is ArduPlane's jet frame and its tuning (S143): another
    // frame would fly as something else under the jet's name and model.
    if *vehicle == SimVehicle::Jet && frame.as_deref().is_some_and(|f| f != JET_FRAME) {
        return Err(invalid(format!("a jet flies the {JET_FRAME} frame")));
    }
    // Frame names are the build's own, such as `+`, `octa-quad` or
    // `X:@ROMFS/models/Callisto.json`: short, with no spaces.
    let frame_ok = frame
        .as_deref()
        .is_none_or(|f| !f.is_empty() && f.len() <= 100 && !f.contains(char::is_whitespace));
    if !frame_ok {
        return Err(invalid("a frame is one of the build's frame names"));
    }
    let simulators = links.iter().filter(|l| l.kind.is_simulator()).count();
    if capabilities.runs_in == RunsIn::Browser {
        if binary.is_some() {
            return Err(invalid("a browser runs only its own simulator"));
        }
        // Each is a whole autopilot in this tab: two, as the first visit
        // starts (maintainer, 2026-10-02).
        if simulators >= MAX_IN_BROWSER {
            return Err(invalid(format!(
                "{MAX_IN_BROWSER} simulated vehicles are already running in this browser. Stop one first"
            )));
        }
    } else {
        if binary.as_deref().is_some_and(|b| b.trim().is_empty()) {
            return Err(invalid("a SITL build needs its path"));
        }
        if simulators >= usize::from(MAX_INSTANCES) {
            return Err(invalid(format!(
                "{MAX_INSTANCES} simulated vehicles are running. Stop one first"
            )));
        }
    }
    Ok(match vehicle {
        SimVehicle::Plane => "Simulated plane",
        SimVehicle::Multicopter => "Simulated multicopter",
        SimVehicle::Vtol => "Simulated VTOL",
        SimVehicle::Rover => "Simulated rover",
        SimVehicle::Boat => "Simulated boat",
        SimVehicle::Submarine => "Simulated submarine",
        SimVehicle::Airship => "Simulated blimp",
        SimVehicle::Jet => "Simulated jet",
    }
    .to_owned())
}

/// Gives a new simulator the lowest instance that is free in its own
/// state; an installed daemon's must also have ports this computer can open
/// (S71), so several run beside each other and beside other programs'
/// simulators. The instance sets the vehicle's system id, so two in one
/// browser are two vehicles.
pub(crate) fn assign_instance(engine: &Engine, kind: LinkKind) -> LinkKind {
    let LinkKind::Simulator {
        vehicle,
        autopilot,
        latitude_deg,
        longitude_deg,
        altitude_m,
        heading_deg,
        frame,
        speedup,
        in_air_m,
        snapshot_id,
        binary,
        wipe,
        with_mission,
        ..
    } = kind
    else {
        return kind;
    };
    let installed = engine.config.capabilities.runs_in == RunsIn::Installed;
    let taken: Vec<u8> = engine
        .state
        .links
        .values()
        .filter_map(|l| match l.kind {
            LinkKind::Simulator { instance, .. } => instance,
            _ => None,
        })
        .collect();
    let instance = Some(
        (0..MAX_INSTANCES)
            .find(|i| !taken.contains(i) && (!installed || engine.system.sim_instance_free(*i)))
            .unwrap_or(MAX_INSTANCES - 1),
    );
    LinkKind::Simulator {
        vehicle,
        autopilot,
        latitude_deg,
        longitude_deg,
        altitude_m,
        heading_deg,
        frame,
        speedup,
        in_air_m,
        snapshot_id,
        binary,
        wipe,
        with_mission,
        instance,
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
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
        ClientRequest::SimStart(mut params) => {
            require(Capability::Config)?;
            let forward = params.forward.take();
            let link = start_link(engine, connection, add_params(params), forward)?;
            to_value(&link)
        }
        ClientRequest::SimStop(IdParams { id }) => {
            require(Capability::Config)?;
            let simulated = engine
                .state
                .links
                .get(&id)
                .is_some_and(|l| l.kind.is_simulator());
            if !simulated {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no simulated vehicle on link {id}"),
                ));
            }
            engine.give_up_practice_on(&id, connection);
            crate::rpc::vehicles::remove_link(engine, connection, &id)?;
            Ok(json!({}))
        }
        ClientRequest::SimRestart(IdParams { id }) => {
            require(Capability::Config)?;
            restart(engine, connection, &id)
        }
        ClientRequest::SimBinaries(_) => {
            require(Capability::Read)?;
            to_value(&SimBinariesResult {
                binaries: engine.system.sim_binaries(),
            })
        }
        ClientRequest::SimCatalogue(params) => {
            require(Capability::Read)?;
            to_value(&engine.system.sim_catalogue(params.refresh))
        }
        ClientRequest::SimDownload(SimBuildParams { id }) => {
            require(Capability::Config)?;
            engine.system.sim_download(&id).map_err(invalid)?;
            Ok(json!({}))
        }
        ClientRequest::SimDelete(SimBuildParams { id }) => {
            require(Capability::Config)?;
            engine.system.sim_delete(&id).map_err(invalid)?;
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a sim method")),
    }
}

/// Adds a simulated vehicle's link and, when asked, a forward of its
/// traffic to another program (S102), such as another ground station to
/// test. The forward is checked first, so a refused one starts nothing; it
/// carries only this vehicle's link, both ways unless `read`, and is
/// removed with it.
pub(crate) fn start_link(
    engine: &mut Engine,
    connection: ConnectionId,
    add: LinkAddParams,
    forward: Option<SimForward>,
) -> Result<LinkDto, ApiError> {
    let forward = forward.map(|SimForward { host, port, policy }| LinkAddParams {
        name: None,
        kind: uxduck_contracts::dto::link::LinkKind::UdpForward {
            host: host.trim().to_owned(),
            port,
        },
        policy,
        topology: None,
        vehicle_filter: Vec::new(),
        from_link: None,
        keep_alive: false,
    });
    if let Some(forward) = &forward {
        if engine.config.capabilities.runs_in == RunsIn::Browser {
            return Err(invalid(
                "sending a simulated vehicle to other programs needs UXDuck Desktop",
            ));
        }
        crate::rpc::vehicles::check_kind(engine, &forward.kind.clone().into(), None)?;
        crate::forwarding::check_new_simulation(engine, &forward.kind.clone().into())?;
    }
    let actor = engine.actor(connection);
    let link = crate::rpc::vehicles::create_link(engine, actor.clone(), add)?;
    if let Some(mut forward) = forward {
        if let uxduck_contracts::dto::link::LinkKind::UdpForward { host, port } = &forward.kind {
            forward.name = Some(format!("{} to {host}:{port}", link.name));
        }
        let made =
            crate::rpc::vehicles::create_link_from(engine, actor, forward, Some(link.id.clone()));
        if let Err(error) = made {
            crate::rpc::vehicles::remove_link(engine, connection, &link.id)?;
            return Err(error);
        }
    }
    Ok(engine.state.links.get(&link.id).map_or(link, LinkDto::from))
}

/// Starts a simulated vehicle again from where it started (maintainer,
/// 2026-10-07: a beginner who crashes one needs a way back). The link is
/// closed and opened again with the same kind, so the simulator starts
/// afresh at the link's place, height and speed, while the vehicle keeps
/// its link, instance and so its identity: its name, history and the tabs
/// pinned to it. Its vehicles go offline first, as on a daemon start, so
/// the take-off waits for the new simulator instead of talking to the one
/// that is stopping.
pub(crate) fn restart(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    let Some(mut link) = engine.state.links.get(id).cloned() else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no simulated vehicle on link {id}"),
        ));
    };
    if !link.kind.is_simulator() {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no simulated vehicle on link {id}"),
        ));
    }
    // "Start again" gives up an attempt at practice on it (S100).
    engine.give_up_practice_on(id, connection);
    engine.pause_controllers_for_restart(id);
    engine.links.close(id);
    engine.drop_launch(id);
    crate::rpc::vehicles::vehicles_offline(engine, connection, id);
    engine.forget_flights_on(id);
    link.status = LinkStatus::Configured;
    link.status_detail = None;
    link.launch = None;
    link.version += 1;
    engine.commit(connection, Change::LinkUpserted { link: link.clone() });
    engine.links.open(&link);
    engine.begin_launch(id);
    let link = engine.state.links.get(id).unwrap_or(&link);
    to_value(&uxduck_contracts::dto::link::Link::from(link))
}

fn add_params(params: SimStartParams) -> LinkAddParams {
    LinkAddParams {
        name: params.name,
        kind: uxduck_contracts::dto::link::LinkKind::Simulator {
            vehicle: params.vehicle,
            autopilot: params.autopilot,
            latitude_deg: params.latitude_deg,
            longitude_deg: params.longitude_deg,
            altitude_m: params.altitude_m.unwrap_or(0.0),
            heading_deg: params.heading_deg.unwrap_or(0.0),
            frame: params.frame.map(|f| f.trim().to_owned()),
            speedup: params.speedup,
            in_air_m: params.in_air_m,
            snapshot_id: None,
            binary: params.binary.map(|b| b.trim().to_owned()),
            wipe: params.wipe,
            with_mission: params.with_mission,
            instance: None,
        },
        policy: None,
        topology: None,
        vehicle_filter: Vec::new(),
        from_link: None,
        keep_alive: false,
    }
}

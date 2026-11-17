//! Faults in a simulated vehicle's camera and gimbal (ADR-0063 Y16): a
//! person, a test or an agent switches one on to see what UXDuck does with
//! it. A fault is what the simulated hardware says on the wire, so nothing
//! here tells the rest of UXDuck of it (Y1). The link holds which are on,
//! for the simulation's page and for whoever asks.

use serde_json::Value;
use uxduck_contracts::dto::link::Link as LinkDto;
use uxduck_contracts::dto::sim::{SimFaultCommand, SimFaultParams, SimFaultState, SimFaultsResult};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::link::{
    Link, LinkKind, LinkStatus, PayloadProfile, SimAutopilot, SimFault, SimFaultKind,
};
use uxduck_core::events::Change;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A command a fault can name: its `MAV_CMD` number, what it does and its
/// MAVLink name.
type Named = (u16, &'static str, &'static str);

/// The camera's commands UXDuck sends it.
const CAMERA: [Named; 9] = [
    (2000, "Take a photo", "MAV_CMD_IMAGE_START_CAPTURE"),
    (2001, "Stop taking photos", "MAV_CMD_IMAGE_STOP_CAPTURE"),
    (2500, "Start recording", "MAV_CMD_VIDEO_START_CAPTURE"),
    (2501, "Stop recording", "MAV_CMD_VIDEO_STOP_CAPTURE"),
    (530, "Change the camera's mode", "MAV_CMD_SET_CAMERA_MODE"),
    (531, "Zoom", "MAV_CMD_SET_CAMERA_ZOOM"),
    (532, "Focus", "MAV_CMD_SET_CAMERA_FOCUS"),
    (526, "Format the card", "MAV_CMD_STORAGE_FORMAT"),
    (
        529,
        "Reset the camera's settings",
        "MAV_CMD_RESET_CAMERA_SETTINGS",
    ),
];

/// A gimbal manager's commands UXDuck sends it.
const MANAGER: [Named; 3] = [
    (
        1000,
        "Point the gimbal",
        "MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW",
    ),
    (
        1001,
        "Take or give back the gimbal",
        "MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE",
    ),
    (195, "Look at a place", "MAV_CMD_DO_SET_ROI_LOCATION"),
];

/// The simulated camera and gimbal a simulation carries.
#[derive(Clone, Copy)]
struct Carries {
    camera: bool,
    gimbal: bool,
    profile: Option<PayloadProfile>,
}

impl Carries {
    fn integrated(self) -> bool {
        self.profile.is_some_and(PayloadProfile::integrated)
    }

    /// The commands its camera and gimbal answer themselves. With ArduPilot
    /// the autopilot is the manager of a gimbal apart from the camera, and
    /// answers the manager's commands itself (Y3): only an integrated unit
    /// manages its own.
    fn commands(self) -> Vec<Named> {
        let mut commands = Vec::new();
        if self.camera {
            commands.extend(match self.profile {
                Some(PayloadProfile::StreamOnly) => &[][..],
                Some(PayloadProfile::PhotoVideo) => &CAMERA[..5],
                Some(PayloadProfile::Integrated | PayloadProfile::ThermalTracking) | None => {
                    &CAMERA[..]
                }
            });
            // A camera that only takes photos in one mode and records in
            // the other has a card to format and settings to reset too.
            if self.profile == Some(PayloadProfile::PhotoVideo) {
                commands.extend(&CAMERA[7..]);
            }
        }
        if self.integrated() {
            commands.extend(MANAGER);
        }
        commands
    }

    /// Why it cannot have a fault, as a sentence to show beside it.
    fn cannot_have(self, kind: SimFaultKind) -> Option<&'static str> {
        let stream_only = self.profile == Some(PayloadProfile::StreamOnly);
        match kind {
            SimFaultKind::CameraSilent | SimFaultKind::CardFull if !self.camera => {
                Some("This simulation carries no simulated camera.")
            }
            SimFaultKind::CardFull if stream_only => {
                Some("A camera that only sends video has no card.")
            }
            SimFaultKind::GimbalSilent
            | SimFaultKind::GimbalError
            | SimFaultKind::AutopilotHolds
                if !self.gimbal =>
            {
                Some("This simulation carries no simulated gimbal.")
            }
            SimFaultKind::GimbalSilent if self.integrated() => Some(
                "A camera and gimbal in one is a single component, so it stops talking whole. Use the camera's fault.",
            ),
            SimFaultKind::AutopilotHolds if !self.integrated() => Some(
                "ArduPilot manages this gimbal and says who holds it. Only a camera and gimbal in one, which manages its own, can say the autopilot holds it.",
            ),
            SimFaultKind::CommandRefused | SimFaultKind::CommandInProgress
                if self.commands().is_empty() =>
            {
                Some(if stream_only {
                    "A camera that only sends video takes no commands, and ArduPilot answers a gimbal's itself."
                } else {
                    "ArduPilot answers this gimbal's commands itself. The simulated gimbal takes none from a ground station."
                })
            }
            _ => None,
        }
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// The link of a simulation that carries UXDuck's simulated camera or
/// gimbal, and what it carries. Any other link is refused with the reason.
fn carrying<'a>(engine: &'a Engine, id: &str) -> Result<(&'a Link, Carries), ApiError> {
    let link = engine
        .state
        .links
        .get(id)
        .filter(|link| link.kind.is_simulator())
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no simulated vehicle on link {id}. Only a simulation's camera and gimbal have faults to switch"),
            )
        })?;
    let LinkKind::Simulator {
        autopilot,
        camera,
        gimbal,
        ardupilot_gimbal,
        payload_profile,
        ..
    } = link.kind
    else {
        return Err(invalid("not a simulation"));
    };
    // What each fault does, and which a simulation cannot have, was worked
    // out and run with ArduPilot as the gimbal's manager (S410). PX4's
    // manager is another (S413), and no fault has been run with it.
    if autopilot == SimAutopilot::Px4 && (camera || gimbal) {
        return Err(invalid(
            "Faults in the simulated camera and gimbal come with ArduPilot. With PX4 there are none to switch yet",
        ));
    }
    if ardupilot_gimbal.is_some() {
        return Err(invalid(
            "ArduPilot simulates this gimbal and its camera itself. UXDuck switches faults only in its own simulated camera and gimbal",
        ));
    }
    if !(camera || gimbal) {
        return Err(invalid(
            "This simulation carries no simulated camera or gimbal. Start one that does to switch faults",
        ));
    }
    Ok((
        link,
        Carries {
            camera,
            gimbal,
            profile: payload_profile,
        },
    ))
}

/// `sim/faults`: every fault, whether it is on, why this simulation cannot
/// have it, and the commands one can name.
pub(crate) fn list(engine: &Engine, id: &str) -> Result<SimFaultsResult, ApiError> {
    let (link, carries) = carrying(engine, id)?;
    Ok(SimFaultsResult {
        faults: SimFaultKind::ALL
            .into_iter()
            .map(|kind| {
                let on = link.faults.iter().find(|fault| fault.kind == kind);
                SimFaultState {
                    kind: kind.into(),
                    on: on.is_some(),
                    command: on.and_then(|fault| fault.command),
                    unavailable: carries.cannot_have(kind).map(str::to_owned),
                }
            })
            .collect(),
        commands: carries
            .commands()
            .into_iter()
            .map(|(command, label, name)| SimFaultCommand {
                command,
                label: label.to_owned(),
                name: name.to_owned(),
            })
            .collect(),
    })
}

/// `sim/fault`: switches one fault on or off. The data plane is told all
/// that are on, and the link says which.
pub(crate) fn switch(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SimFaultParams,
) -> Result<Value, ApiError> {
    let (link, carries) = carrying(engine, &params.id)?;
    let mut link = link.clone();
    let kind: SimFaultKind = params.kind.into();
    let before = link.faults.clone();
    link.faults.retain(|fault| fault.kind != kind);
    if params.on {
        if let Some(why) = carries.cannot_have(kind) {
            return Err(invalid(why.trim_end_matches('.')));
        }
        let command = if kind.names_a_command() {
            let Some(command) = params.command else {
                return Err(invalid(
                    "Name the command this fault is about, one of the commands that sim/faults lists",
                ));
            };
            if !carries.commands().iter().any(|named| named.0 == command) {
                return Err(invalid(format!(
                    "This simulation's camera and gimbal have no command {command}. sim/faults lists the ones they have"
                )));
            }
            Some(command)
        } else {
            None
        };
        // Its camera and gimbal are there while it runs.
        if link.status != LinkStatus::Up {
            return Err(invalid("the simulation isn't running"));
        }
        link.faults.push(SimFault { kind, command });
        link.faults.sort_by_key(|fault| fault.kind);
    }
    if link.faults != before {
        link.version += 1;
        engine.links.simulation_faults(&link.id, &link.faults);
        engine.commit(connection, Change::LinkUpserted { link: link.clone() });
    }
    to_value(&LinkDto::from(&link))
}

impl Engine {
    /// A fault ended by what happened on the wire: a full card was
    /// formatted, or the gimbal was taken from the autopilot that held it.
    /// The link stops saying it is on, and the data plane is told what
    /// still is, so the two agree whatever passed each other on the way.
    pub fn simulation_fault_ended(&mut self, link_id: &str, kind: SimFaultKind) {
        let Some(mut link) = self.state.links.get(link_id).cloned() else {
            return;
        };
        let before = link.faults.len();
        link.faults.retain(|fault| fault.kind != kind);
        if link.faults.len() == before {
            return;
        }
        link.version += 1;
        self.links.simulation_faults(link_id, &link.faults);
        self.commit_as(
            uxduck_core::events::Actor {
                client_id: "data-plane".into(),
                operator_id: None,
                identity: None,
            },
            Change::LinkUpserted { link },
        );
    }
}

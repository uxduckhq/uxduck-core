//! A controller's flight actions (S417, ADR-0005 A4): the commands a
//! pilot reaches for with hands on the sticks that depend on what the
//! vehicle is doing now. Each comes to an ordinary command here, before
//! `command/send`'s checks, so it goes, and is receipted, as that command
//! with every check the Cockpit's buttons have. And the rule that a
//! dangerous command needs a hold on a controller, kept on save and at
//! dispatch.

use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::{DANGEROUS_HOLD_MS, InputMapping};
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::parameter::parameter_id;
use uxduck_core::domain::telemetry_layout::VehicleClass;
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;

/// `MISSION_CURRENT`'s state while the mission is paused.
const MISSION_STATE_PAUSED: f64 = 4.0;

/// The last `MAV_CMD_NAV_*`: items below it fly somewhere, the rest act
/// on the way (`DO_*`) or wait (`CONDITION_*`). Waypoint steps count only
/// these, as a pilot counts waypoints.
const MAV_CMD_NAV_LAST: u16 = 95;

/// Each family's cruise speed parameters, first found first, with the
/// factor that makes them metres a second: what a vehicle flies at until
/// UXDuck tells it another speed.
const CRUISE_SPEEDS: [(Family, &str, f64); 6] = [
    (Family::Copter, "WPNAV_SPEED", 0.01),
    (Family::Sub, "WPNAV_SPEED", 0.01),
    (Family::Plane, "AIRSPEED_CRUISE", 1.0),
    (Family::Plane, "TRIM_ARSPD_CM", 0.01),
    (Family::Rover, "WP_SPEED", 1.0),
    (Family::Rover, "CRUISE_SPEED", 1.0),
];

fn refused(why: String) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, why)
}

impl Engine {
    /// The command a flight action comes to for `vehicle` now, or why it
    /// cannot be sent. Anything else is itself.
    pub(crate) fn flight_action_resolved(
        &self,
        vehicle: &Vehicle,
        command: Command,
    ) -> Result<Command, ApiError> {
        let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type);
        let name = &vehicle.name;
        match command {
            Command::PauseOrContinue => {
                if family != Some(Family::Copter) {
                    return Err(refused(format!(
                        "Only ArduCopter pauses a mission and continues it. Choose a holding mode for {name} instead, such as LOITER or Hold."
                    )));
                }
                let paused = self
                    .telemetry_value(&vehicle.id, "mission.state")
                    .is_some_and(|state| (state - MISSION_STATE_PAUSED).abs() < 0.5);
                Ok(if paused {
                    Command::continue_mission()
                } else {
                    Command::pause_mission()
                })
            }
            Command::MissionStep { steps } => self.mission_step(vehicle, family, steps),
            Command::SpeedStep { by_mps } => self.speed_step(vehicle, family, by_mps),
            Command::ModeCycle { modes } => {
                let current = vehicle.mode.as_deref().unwrap_or_default();
                let at = modes.iter().position(|m| m.eq_ignore_ascii_case(current));
                let next = at.map_or(0, |at| (at + 1) % modes.len());
                modes
                    .get(next)
                    .map(|mode| Command::SetMode { mode: mode.clone() })
                    .ok_or_else(|| refused("A mode cycle needs its modes.".into()))
            }
            other => Ok(other),
        }
    }

    /// The navigation item `steps` on from the one the vehicle flies to.
    fn mission_step(
        &self,
        vehicle: &Vehicle,
        family: Option<Family>,
        steps: i16,
    ) -> Result<Command, ApiError> {
        let name = &vehicle.name;
        let items = self.mission(&vehicle.id).items;
        // ArduPilot's item 0 is home, never flown to as a step.
        let first = usize::from(family != Some(Family::Px4));
        let waypoints: Vec<usize> = (first..items.len())
            .filter(|&seq| items[seq].command < MAV_CMD_NAV_LAST)
            .collect();
        if waypoints.is_empty() {
            return Err(refused(format!(
                "UXDuck doesn't know of a mission with waypoints on {name}. Upload one, or wait for its mission to download."
            )));
        }
        let Some(current) = self.telemetry_value(&vehicle.id, "mission.current") else {
            return Err(refused(format!(
                "{name} hasn't said which step of its mission it is on yet."
            )));
        };
        // Where it is now: the waypoint it flies to, or the first one after
        // the action it is on.
        let at = waypoints
            .iter()
            .position(|&seq| f64::from(u32::try_from(seq).unwrap_or(u32::MAX)) >= current - 0.5)
            .unwrap_or(waypoints.len());
        let to = i64::try_from(at).unwrap_or(i64::MAX) + i64::from(steps);
        let Some(&seq) = usize::try_from(to).ok().and_then(|to| waypoints.get(to)) else {
            let way = if steps < 0 { "before" } else { "after" };
            let count = match steps.unsigned_abs() {
                1 => "waypoint".to_owned(),
                n => format!("{n} waypoints"),
            };
            return Err(refused(format!(
                "{name}'s mission has no {count} {way} the one it flies to."
            )));
        };
        Ok(Command::go_to_mission_item(
            u16::try_from(seq).unwrap_or(u16::MAX),
        ))
    }

    /// The speed `by_mps` from the one it was last told.
    fn speed_step(
        &self,
        vehicle: &Vehicle,
        family: Option<Family>,
        by_mps: f64,
    ) -> Result<Command, ApiError> {
        let name = &vehicle.name;
        let Some(family) = family.filter(|f| *f != Family::Blimp) else {
            return Err(refused(format!("{name} doesn't take a change of speed.")));
        };
        let kind = if family == Family::Plane { 0.0 } else { 1.0 };
        let Some(now) = self
            .speed_last_sent(vehicle, kind)
            .or_else(|| self.cruise_speed(vehicle, family))
        else {
            return Err(refused(format!(
                "UXDuck doesn't know the speed {name} goes at yet. Set one with Change speed in the Cockpit first, then step it."
            )));
        };
        let to = now + by_mps;
        if to <= 0.0 {
            return Err(refused(format!(
                "{name} goes at {now:.1} m/s, and slower by {:.1} m/s would ask it to stop. UXDuck doesn't send a speed of 0 or less.",
                by_mps.abs()
            )));
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a speed in metres a second, which MAVLink sends as a float"
        )]
        Ok(Command::Long {
            command: 178,
            params: vec![kind, to as f32, -1.0],
        })
    }

    /// The last speed of this kind the vehicle accepted from UXDuck since
    /// it started.
    fn speed_last_sent(&self, vehicle: &Vehicle, kind: f32) -> Option<f64> {
        let boot = self.transfers.boots.get(&vehicle.id).copied().unwrap_or(0);
        self.state
            .receipts
            .values()
            .filter(|r| {
                r.vehicle_id == vehicle.id
                    && r.status == uxduck_core::domain::command::ReceiptStatus::Accepted
                    && r.created_at_us >= boot
            })
            .filter_map(|r| match &r.command {
                Command::Long {
                    command: 178,
                    params,
                } if params.first().is_some_and(|k| (*k - kind).abs() < 0.5) => params
                    .get(1)
                    .map(|speed| (r.created_at_us, f64::from(*speed))),
                _ => None,
            })
            .max_by_key(|(at, _)| *at)
            .map(|(_, speed)| speed)
    }

    /// What the vehicle's own parameters say it cruises at.
    fn cruise_speed(&self, vehicle: &Vehicle, family: Family) -> Option<f64> {
        let px4 = match VehicleClass::of(&vehicle.vehicle_type) {
            class if class.never_flies() => "GND_SPEED_TRIM",
            _ if vehicle.vehicle_type == "FixedWing" => "FW_AIRSPD_TRIM",
            _ => "MPC_XY_CRUISE",
        };
        let names: Vec<(&str, f64)> = if family == Family::Px4 {
            vec![(px4, 1.0)]
        } else {
            CRUISE_SPEEDS
                .iter()
                .filter(|(f, _, _)| *f == family)
                .map(|(_, name, factor)| (*name, *factor))
                .collect()
        };
        names.into_iter().find_map(|(name, factor)| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value * factor)
                .filter(|speed| *speed > 0.0)
        })
    }
}

/// Why a binding of `command` held for `hold_ms` may not be on a
/// controller: a dangerous command held for less than
/// `DANGEROUS_HOLD_MS`, or not at all (S417).
pub(crate) fn hold_refusal(command: &Command, hold_ms: Option<u32>) -> Option<String> {
    let reason = command.controller_hold_reason()?;
    if hold_ms.is_some_and(|ms| ms >= DANGEROUS_HOLD_MS) {
        return None;
    }
    Some(format!(
        "{} needs a hold of {} s or more on a controller, so a bumped button never sends it. {reason}",
        command.label(),
        DANGEROUS_HOLD_MS / 1_000,
    ))
}

/// Why a mode cycle may not be bound: it needs two modes or more, each
/// once, to have a next one.
fn cycle_refusal(command: &Command) -> Option<String> {
    let Command::ModeCycle { modes } = command else {
        return None;
    };
    let mut names: Vec<String> = modes.iter().map(|m| m.trim().to_uppercase()).collect();
    if names.iter().any(String::is_empty) {
        return Some("A mode cycle's modes each need a name.".into());
    }
    names.sort_unstable();
    names.dedup();
    if names.len() != modes.len() {
        return Some("A mode cycle lists each mode once.".into());
    }
    (modes.len() < 2).then(|| {
        "A mode cycle needs two modes or more. For one mode, choose that mode on its own.".into()
    })
}

/// A mapping is saved only when its flight actions keep the rules (S417):
/// refused at the first binding that breaks the hold rule or has a mode
/// cycle without a next mode.
pub(crate) fn require_flight_actions(mapping: &InputMapping) -> Result<(), ApiError> {
    mapping
        .buttons
        .iter()
        .map(|b| (&b.command, b.hold_ms))
        .chain(mapping.positions.iter().map(|p| (&p.command, p.hold_ms)))
        .find_map(|(command, hold_ms)| {
            hold_refusal(command, hold_ms).or_else(|| cycle_refusal(command))
        })
        .map_or(Ok(()), |why| Err(refused(why)))
}

impl Engine {
    /// Whether a controller's binding keeps the hold rule at dispatch: a
    /// mapping stored before it, or by a client that skipped it, sends
    /// nothing dangerous without its hold, and the pilot is told why, and
    /// feels it as a refusal's rumble (S419).
    pub(crate) fn controller_hold_kept(
        &mut self,
        controller: &uxduck_core::domain::controller::Controller,
        target: &str,
        command: &Command,
        hold_ms: Option<u32>,
    ) -> bool {
        let Some(why) = hold_refusal(command, hold_ms) else {
            return true;
        };
        let name = self.vehicle_name(target);
        self.raise(crate::commands::Raise {
            severity: uxduck_core::domain::notification::Severity::Warning,
            kind: uxduck_core::domain::notification::NotificationKind::Transient,
            subject: uxduck_core::domain::notification::Subject::Vehicle {
                id: target.to_owned(),
            },
            title: format!(
                "UXDuck held back {} from {} to {name}",
                command.label(),
                controller.name
            ),
            body: Some(format!(
                "{why} Set a hold for it in the controller's mapping."
            )),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
        self.controller_rumble(
            &controller.id,
            uxduck_core::domain::controller::Rumble::Refused,
        );
        false
    }
}

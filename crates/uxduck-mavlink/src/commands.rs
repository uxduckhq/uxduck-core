//! Commands as `COMMAND_LONG` or `COMMAND_INT` (ADR-0005 command path):
//! which `MAV_CMD` and parameters each command is on ArduPilot and on PX4,
//! and the acknowledgement's result in words.

use uxduck_core::domain::command::Command;
use uxduck_core::domain::telemetry::AltitudeFrame;

use crate::modes::{AvailableMode, Family};

/// `MAV_CMD_COMPONENT_ARM_DISARM`'s magic number that skips pre-arm checks
/// (arming) or disarms in flight.
const FORCE: f32 = 21196.0;

/// `MAV_CMD_DO_REPOSITION`: go to a place now.
const MAV_CMD_DO_REPOSITION: u16 = 192;
/// `MAV_FRAME_GLOBAL_INT`: latitude and longitude in degrees times 1e7,
/// the height above mean sea level.
const MAV_FRAME_GLOBAL_INT: u8 = 5;
/// `MAV_FRAME_GLOBAL_RELATIVE_ALT_INT`: as `GLOBAL_INT`, the height above home.
const MAV_FRAME_GLOBAL_RELATIVE_ALT_INT: u8 = 6;
/// `MAV_FRAME_GLOBAL_TERRAIN_ALT_INT`: as `GLOBAL_INT`, the height above
/// the ground, from the autopilot's terrain data.
const MAV_FRAME_GLOBAL_TERRAIN_ALT_INT: u8 = 11;
/// `MAV_DO_REPOSITION_FLAGS_CHANGE_MODE`: switch to GUIDED (PX4: Hold) if
/// the vehicle is not there.
const CHANGE_MODE: f32 = 1.0;

/// How a command goes on the wire: a `COMMAND_LONG` with seven floats, or
/// a `COMMAND_INT`, whose place is two integers in degrees times 1e7 and
/// so keeps it to the centimetre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wire {
    Long([f32; 7]),
    Int {
        /// `MAV_FRAME`.
        frame: u8,
        /// `param1` to `param4`.
        params: [f32; 4],
        /// Latitude, degrees times 1e7.
        x: i32,
        /// Longitude, degrees times 1e7.
        y: i32,
        /// The height in the frame.
        z: f32,
    },
}

impl Wire {
    /// The seven floats of a `COMMAND_LONG`; none for a `COMMAND_INT`.
    #[must_use]
    pub fn long(self) -> Option<[f32; 7]> {
        match self {
            Self::Long(params) => Some(params),
            Self::Int { .. } => None,
        }
    }
}

/// The `MAV_CMD` and how it goes on the wire.
///
/// # Errors
/// When the vehicle's firmware has no such mode or command, or a place is
/// not on Earth.
pub fn encode(
    command: &Command,
    family: Option<Family>,
    force: bool,
) -> Result<(u16, Wire), String> {
    encode_listed(command, family, &[], force)
}

/// As `encode`, with the vehicle's own list of modes, for a mode the
/// table does not know by name (a scripted one).
///
/// # Errors
/// As `encode`.
pub fn encode_listed(
    command: &Command,
    family: Option<Family>,
    listed: &[AvailableMode],
    force: bool,
) -> Result<(u16, Wire), String> {
    encode_long(command, family, listed, force)
        .map(|(command, params)| (command, Wire::Long(params)))
        .or_else(|first| match command {
            Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
            } => go_to(*latitude_deg, *longitude_deg, *altitude_m, *altitude_frame),
            _ => Err(first),
        })
}

/// `DO_REPOSITION` in a `COMMAND_INT`: the vehicle's own speed (-1),
/// changing to GUIDED if it is not there, no loiter radius, the heading
/// left as it is (NaN), and the place, its height in the frame asked for.
fn go_to(
    latitude_deg: f64,
    longitude_deg: f64,
    altitude_m: f64,
    altitude_frame: AltitudeFrame,
) -> Result<(u16, Wire), String> {
    if !(-90.0..=90.0).contains(&latitude_deg)
        || !(-180.0..=180.0).contains(&longitude_deg)
        || !altitude_m.is_finite()
    {
        return Err(
            "a place needs a latitude within 90°, a longitude within 180° and a height".into(),
        );
    }
    let frame = match altitude_frame {
        AltitudeFrame::AboveHome => MAV_FRAME_GLOBAL_RELATIVE_ALT_INT,
        AltitudeFrame::Amsl => MAV_FRAME_GLOBAL_INT,
        AltitudeFrame::AboveGround => MAV_FRAME_GLOBAL_TERRAIN_ALT_INT,
        AltitudeFrame::Ellipsoid => {
            return Err("a height above the ellipsoid cannot be flown to".into());
        }
    };
    #[allow(clippy::cast_possible_truncation, reason = "within ±1.8e9")]
    let e7 = |degrees: f64| (degrees * 1e7).round() as i32;
    Ok((
        MAV_CMD_DO_REPOSITION,
        Wire::Int {
            frame,
            params: [-1.0, CHANGE_MODE, 0.0, f32::NAN],
            x: e7(latitude_deg),
            y: e7(longitude_deg),
            #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
            z: altitude_m as f32,
        },
    ))
}

/// The `MAV_CMD` and its seven parameters, for commands that are a `COMMAND_LONG`.
fn encode_long(
    command: &Command,
    family: Option<Family>,
    listed: &[AvailableMode],
    force: bool,
) -> Result<(u16, [f32; 7]), String> {
    let force = if force { FORCE } else { 0.0 };
    let set_mode = |name: &str| {
        let number = family
            .and_then(|family| family.number(name))
            .or_else(|| Family::listed_number(listed, name))
            .ok_or_else(|| format!("{name} is not a mode of this vehicle"))?;
        // MAV_CMD_DO_SET_MODE with MAV_MODE_FLAG_CUSTOM_MODE_ENABLED; PX4
        // takes its main and sub mode apart.
        #[allow(clippy::cast_precision_loss, reason = "mode numbers are small")]
        if family == Some(Family::Px4) {
            let (main, sub) = Family::px4_parts(number);
            Ok((176, [1.0, main as f32, sub as f32, 0.0, 0.0, 0.0, 0.0]))
        } else {
            Ok((176, [1.0, number as f32, 0.0, 0.0, 0.0, 0.0, 0.0]))
        }
    };
    match command {
        Command::Arm => Ok((400, [1.0, force, 0.0, 0.0, 0.0, 0.0, 0.0])),
        Command::Disarm => Ok((400, [0.0, force, 0.0, 0.0, 0.0, 0.0, 0.0])),
        Command::SetMode { mode } => set_mode(mode),
        // MAV_CMD_NAV_RETURN_TO_LAUNCH: every ArduPilot vehicle, and PX4.
        Command::Rtl if family == Some(Family::Blimp) => set_mode("RTL"),
        Command::Rtl => Ok((20, [0.0; 7])),
        // A landing mode here and now: LAND on a copter, QLAND on a
        // quadplane. A fixed-wing plane lands through a mission.
        Command::Land => match family {
            Some(Family::Copter | Family::Blimp) => set_mode("LAND"),
            Some(Family::Px4) => set_mode("Land"),
            Some(Family::Plane) => set_mode("QLAND"),
            // A submarine comes up rather than down (S37).
            Some(Family::Sub) => set_mode("SURFACE"),
            _ => Err("this vehicle cannot land on command".into()),
        },
        // MAV_CMD_PREFLIGHT_REBOOT_SHUTDOWN: restart the autopilot.
        Command::Reboot => Ok((246, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])),
        // A `COMMAND_INT` (`go_to`), not a `COMMAND_LONG`.
        Command::GoTo { .. } => Err("a place goes as a COMMAND_INT".into()),
        Command::Long { command, params } => {
            if params.len() > 7 || !params.iter().all(|p| p.is_finite()) {
                return Err("a command has at most seven numeric parameters".into());
            }
            let mut all = [0.0; 7];
            all[..params.len()].copy_from_slice(params);
            Ok((*command, all))
        }
    }
}

/// `MAV_RESULT` in words.
#[must_use]
pub fn result_text(result: u8) -> &'static str {
    match result {
        0 => "Accepted",
        1 => "Temporarily rejected",
        2 => "Denied",
        3 => "Unsupported",
        4 => "Failed",
        5 => "In progress",
        6 => "Cancelled",
        7 => "Only as COMMAND_LONG",
        8 => "Only as COMMAND_INT",
        9 => "Unsupported frame",
        _ => "Unknown result",
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact small integers sent as floats")]
mod tests {
    use super::*;

    /// The command and its floats, for the commands that are a `COMMAND_LONG`.
    fn long(
        command: &Command,
        family: Option<Family>,
        force: bool,
    ) -> Result<(u16, [f32; 7]), String> {
        let (command, wire) = encode(command, family, force)?;
        Ok((command, wire.long().expect("a COMMAND_LONG")))
    }

    #[test]
    fn a_scripted_mode_goes_by_the_vehicles_own_list() {
        let listed = [AvailableMode::new(100, "Survey", 0)];
        let survey = Command::SetMode {
            mode: "survey".into(),
        };
        let (command, wire) = encode_listed(&survey, Some(Family::Copter), &listed, false).unwrap();
        assert_eq!((command, wire.long().unwrap()[1]), (176, 100.0));
        // The table's names still win, and without the list the mode is unknown.
        let loiter = Command::SetMode {
            mode: "LOITER".into(),
        };
        let (_, wire) = encode_listed(&loiter, Some(Family::Copter), &listed, false).unwrap();
        assert_eq!(wire.long().unwrap()[1], 5.0);
        assert!(encode(&survey, Some(Family::Copter), false).is_err());
    }

    #[test]
    fn commands_encode_per_family() {
        let copter = Some(Family::Copter);
        assert_eq!(long(&Command::Arm, copter, false).unwrap().0, 400);
        assert!((long(&Command::Disarm, copter, true).unwrap().1[1] - FORCE).abs() < f32::EPSILON);
        let (cmd, params) = long(
            &Command::SetMode {
                mode: "loiter".into(),
            },
            copter,
            false,
        )
        .unwrap();
        assert_eq!((cmd, params[1]), (176, 5.0));
        assert_eq!(long(&Command::Land, copter, false).unwrap().1[1], 9.0);
        assert_eq!(
            long(&Command::Land, Some(Family::Plane), false).unwrap().1[1],
            20.0
        );
        assert!(long(&Command::Land, Some(Family::Rover), false).is_err());
        let (command, params) = long(&Command::Land, Some(Family::Px4), false).unwrap();
        assert_eq!((command, params[1], params[2]), (176, 4.0, 6.0));
        let position = Command::SetMode {
            mode: "Position".into(),
        };
        let (_, params) = long(&position, Some(Family::Px4), false).unwrap();
        assert_eq!((params[0], params[1], params[2]), (1.0, 3.0, 0.0));
        assert!(
            long(
                &Command::SetMode {
                    mode: "WARP".into()
                },
                copter,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn a_place_goes_as_a_command_int_with_its_degrees_times_1e7() {
        let go_to = Command::GoTo {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_230,
            altitude_m: 30.0,
            altitude_frame: AltitudeFrame::AboveHome,
        };
        let (command, wire) = encode(&go_to, Some(Family::Copter), false).unwrap();
        assert_eq!(command, 192);
        let Wire::Int {
            frame,
            params,
            x,
            y,
            z,
        } = wire
        else {
            panic!("{wire:?}");
        };
        assert_eq!((frame, x, y, z), (6, -353_632_610, 1_491_652_300, 30.0));
        assert_eq!((params[0], params[1], params[2]), (-1.0, 1.0, 0.0));
        assert!(params[3].is_nan());
        // Not on Earth.
        assert!(
            encode(
                &Command::GoTo {
                    latitude_deg: 91.0,
                    longitude_deg: 0.0,
                    altitude_m: 10.0,
                    altitude_frame: AltitudeFrame::AboveHome,
                },
                None,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn a_submarine_destination_keeps_its_negative_height_and_changes_to_guided() {
        let go_to = Command::GoTo {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_230,
            altitude_m: -12.3,
            altitude_frame: AltitudeFrame::AboveHome,
        };
        let (command, wire) = encode(&go_to, Some(Family::Sub), false).unwrap();
        assert_eq!(command, MAV_CMD_DO_REPOSITION);
        let Wire::Int {
            frame,
            params,
            x,
            y,
            z,
        } = wire
        else {
            panic!("{wire:?}");
        };
        assert_eq!((frame, x, y, z), (6, -353_632_610, 1_491_652_300, -12.3));
        assert_eq!(params[1], CHANGE_MODE);
    }

    #[test]
    fn a_place_s_height_goes_in_its_own_frame() {
        let frame_of = |altitude_frame| {
            let go_to = Command::GoTo {
                latitude_deg: -35.0,
                longitude_deg: 149.0,
                altitude_m: 600.0,
                altitude_frame,
            };
            match encode(&go_to, Some(Family::Plane), false).map(|(_, wire)| wire) {
                Ok(Wire::Int { frame, z, .. }) => Ok((frame, z)),
                Ok(wire) => panic!("{wire:?}"),
                Err(error) => Err(error),
            }
        };
        assert_eq!(frame_of(AltitudeFrame::AboveHome), Ok((6, 600.0)));
        assert_eq!(frame_of(AltitudeFrame::Amsl), Ok((5, 600.0)));
        assert_eq!(frame_of(AltitudeFrame::AboveGround), Ok((11, 600.0)));
        assert!(frame_of(AltitudeFrame::Ellipsoid).is_err());
    }
    #[test]
    fn blimp_return_and_land_use_its_mode_numbers() {
        let family = Some(Family::Blimp);
        assert_eq!(
            long(&Command::Rtl, family, false).unwrap(),
            (176, [1.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(
            long(&Command::Land, family, false).unwrap(),
            (176, [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
    }
}

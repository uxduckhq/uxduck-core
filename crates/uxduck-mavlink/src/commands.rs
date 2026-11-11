//! Commands as `COMMAND_LONG` or `COMMAND_INT` (ADR-0005 command path):
//! which `MAV_CMD` and parameters each command is on ArduPilot and on PX4,
//! and the acknowledgement's result in words.

use uxduck_core::domain::command::{Command, PlaceAmsl};
use uxduck_core::domain::telemetry::AltitudeFrame;

use crate::modes::{AvailableMode, Family};

/// `MAV_CMD_COMPONENT_ARM_DISARM`'s magic number that skips pre-arm checks
/// (arming) or disarms in flight.
const FORCE: f32 = 21196.0;

/// `MAV_CMD_DO_REPOSITION`: go to a place now.
const MAV_CMD_DO_REPOSITION: u16 = 192;
/// `MAV_CMD_DO_SET_HOME`: param 1 is 1 for where the vehicle is now.
const MAV_CMD_DO_SET_HOME: u16 = 179;
/// `MAV_CMD_DO_SET_GLOBAL_ORIGIN`: the EKF origin.
const MAV_CMD_DO_SET_GLOBAL_ORIGIN: u16 = 611;
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
    if let Command::GoTo {
        loiter_radius_m: Some(radius),
        ..
    } = command
        && (family != Some(Family::Plane)
            || !radius.is_finite()
            || !(1.0..=65535.0).contains(&radius.abs()))
    {
        return Err(
            "loiter radius needs an ArduPlane and a magnitude from 1 to 65535 metres".into(),
        );
    }
    if let Command::GoTo {
        heading: Some(heading),
        ..
    } = command
    {
        if family != Some(Family::Sub) {
            return Err("heading options are supported only by ArduSub".into());
        }
        if let uxduck_core::domain::command::GoToHeading::Set { degrees } = heading
            && (!degrees.is_finite() || !(0.0..=360.0).contains(degrees))
        {
            return Err("heading must be from 0 to 360 degrees true".into());
        }
    }
    encode_long(command, family, listed, force)
        .map(|(command, params)| (command, Wire::Long(params)))
        .or_else(|first| match command {
            Command::GoTo {
                latitude_deg,
                longitude_deg,
                altitude_m,
                altitude_frame,
                loiter_radius_m,
                ..
            } => go_to(
                *latitude_deg,
                *longitude_deg,
                *altitude_m,
                *altitude_frame,
                *loiter_radius_m,
            ),
            // Where the vehicle is now: param 1, and no place.
            Command::SetHome { place: None } => Ok((
                MAV_CMD_DO_SET_HOME,
                Wire::Int {
                    frame: MAV_FRAME_GLOBAL_INT,
                    params: [1.0, 0.0, 0.0, 0.0],
                    x: 0,
                    y: 0,
                    z: 0.0,
                },
            )),
            Command::SetHome { place: Some(place) } => amsl_place(MAV_CMD_DO_SET_HOME, place),
            Command::SetEkfOrigin { place } => amsl_place(MAV_CMD_DO_SET_GLOBAL_ORIGIN, place),
            _ => Err(first),
        })
}

/// `DO_REPOSITION` in a `COMMAND_INT`: the vehicle's own speed (-1),
/// changing to GUIDED if it is not there, an optional plane loiter radius
/// and direction (otherwise 0/NaN), and the place at the requested height.
fn go_to(
    latitude_deg: f64,
    longitude_deg: f64,
    altitude_m: f64,
    altitude_frame: AltitudeFrame,
    loiter_radius_m: Option<f64>,
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
            return Err("a destination cannot be given as a height above the ellipsoid".into());
        }
    };
    #[allow(clippy::cast_possible_truncation, reason = "within ±1.8e9")]
    let e7 = |degrees: f64| (degrees * 1e7).round() as i32;
    Ok((
        MAV_CMD_DO_REPOSITION,
        Wire::Int {
            frame,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "validated ArduPlane radius"
            )]
            params: [
                -1.0,
                CHANGE_MODE,
                loiter_radius_m.map_or(0.0, |r| r.abs() as f32),
                loiter_radius_m.map_or(f32::NAN, |r| if r < 0.0 { 1.0 } else { 0.0 }),
            ],
            x: e7(latitude_deg),
            y: e7(longitude_deg),
            #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
            z: altitude_m as f32,
        },
    ))
}

/// A place above mean sea level in a `COMMAND_INT`, its other parameters
/// 0: home's or the EKF origin's. 0, 0 is refused, since ArduPilot reads
/// it as "where the vehicle is now" for home.
fn amsl_place(command: u16, place: &PlaceAmsl) -> Result<(u16, Wire), String> {
    let PlaceAmsl {
        latitude_deg,
        longitude_deg,
        altitude_amsl_m,
    } = *place;
    if !(-90.0..=90.0).contains(&latitude_deg)
        || !(-180.0..=180.0).contains(&longitude_deg)
        || !altitude_amsl_m.is_finite()
    {
        return Err(
            "a place needs a latitude within 90°, a longitude within 180° and a height".into(),
        );
    }
    if latitude_deg == 0.0 && longitude_deg == 0.0 {
        return Err("0, 0 is where a missing position reads. Choose the place again".into());
    }
    #[allow(clippy::cast_possible_truncation, reason = "within ±1.8e9")]
    let e7 = |degrees: f64| (degrees * 1e7).round() as i32;
    Ok((
        command,
        Wire::Int {
            frame: MAV_FRAME_GLOBAL_INT,
            params: [0.0; 4],
            x: e7(latitude_deg),
            y: e7(longitude_deg),
            #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
            z: altitude_amsl_m as f32,
        },
    ))
}

#[cfg(test)]
mod loiter_tests {
    use super::*;

    #[test]
    fn reposition_radius_and_direction_are_separate_and_position_stays_precise() {
        for radius in [75.0, -120.0] {
            let command = Command::GoTo {
                latitude_deg: 37.763_932_1,
                longitude_deg: -122.385_804_3,
                altitude_m: 100.0,
                altitude_frame: AltitudeFrame::AboveHome,
                loiter_radius_m: Some(radius),
                heading: None,
            };
            let (id, wire) = encode(&command, Some(Family::Plane), false).unwrap();
            let Wire::Int { params, x, y, .. } = wire else {
                panic!("Go to must retain integer position")
            };
            assert_eq!(id, 192);
            assert_eq!(
                params,
                [
                    -1.0,
                    1.0,
                    #[allow(clippy::cast_possible_truncation, reason = "75 and 120 m are exact")]
                    {
                        radius.abs() as f32
                    },
                    if radius < 0.0 { 1.0 } else { 0.0 }
                ]
            );
            assert_eq!((x, y), (377_639_321, -1_223_858_043));
            for family in [Family::Copter, Family::Rover, Family::Sub, Family::Px4] {
                assert!(encode(&command, Some(family), false).is_err());
            }
        }
    }

    #[test]
    fn invalid_radii_are_refused_before_a_packet_is_created() {
        for radius in [0.0, 0.5, -0.5, 65536.0, f64::NAN, f64::INFINITY] {
            let command = Command::GoTo {
                latitude_deg: 37.7,
                longitude_deg: -122.4,
                altitude_m: 100.0,
                altitude_frame: AltitudeFrame::AboveHome,
                loiter_radius_m: Some(radius),
                heading: None,
            };
            assert!(encode(&command, Some(Family::Plane), false).is_err());
        }
    }
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
        // The engine sends it as Arm or Disarm (`Command::resolved`).
        Command::ArmToggle => Err("a toggle goes as Arm or Disarm".into()),
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
        Command::GoTo { .. } | Command::SetHome { .. } | Command::SetEkfOrigin { .. } => {
            Err("a place goes as a COMMAND_INT".into())
        }
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
    fn plane_native_height_sends_the_height_and_reference_without_coordinates() {
        for frame in [0.0, 3.0, 10.0] {
            let command = Command::Long {
                command: 186,
                params: vec![40.0, frame],
            };
            let (number, params) = long(&command, Some(Family::Plane), false).unwrap();
            assert_eq!(number, 186);
            assert_eq!(params, [40.0, frame, 0.0, 0.0, 0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn a_place_goes_as_a_command_int_with_its_degrees_times_1e7() {
        let go_to = Command::GoTo {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_230,
            altitude_m: 30.0,
            altitude_frame: AltitudeFrame::AboveHome,
            loiter_radius_m: None,
            heading: None,
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
                    loiter_radius_m: None,
                    heading: None,
                },
                None,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn home_and_the_ekf_origin_go_as_a_command_int_above_sea_level() {
        let place = PlaceAmsl {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_230,
            altitude_amsl_m: 584.25,
        };
        for (command, id) in [
            (Command::SetHome { place: Some(place) }, 179),
            (Command::SetEkfOrigin { place }, 611),
        ] {
            for family in [Family::Copter, Family::Plane, Family::Px4] {
                let (sent, wire) = encode(&command, Some(family), false).unwrap();
                assert_eq!(sent, id);
                assert_eq!(
                    wire,
                    Wire::Int {
                        frame: 5,
                        params: [0.0; 4],
                        x: -353_632_610,
                        y: 1_491_652_300,
                        z: 584.25,
                    }
                );
            }
        }
        // Where it is now: param 1, no place.
        let (sent, wire) = encode(&Command::SetHome { place: None }, None, false).unwrap();
        assert_eq!(sent, 179);
        let Wire::Int { params, x, y, .. } = wire else {
            panic!("{wire:?}")
        };
        assert_eq!((params[0], x, y), (1.0, 0, 0));
        // 0, 0 would read as "where it is now"; off the Earth, or no height.
        for (lat, lon, alt) in [(0.0, 0.0, 10.0), (91.0, 0.0, 10.0), (10.0, 10.0, f64::NAN)] {
            let bad = PlaceAmsl {
                latitude_deg: lat,
                longitude_deg: lon,
                altitude_amsl_m: alt,
            };
            assert!(encode(&Command::SetHome { place: Some(bad) }, None, false).is_err());
            assert!(encode(&Command::SetEkfOrigin { place: bad }, None, false).is_err());
        }
    }

    #[test]
    fn a_submarine_destination_keeps_its_negative_height_and_changes_to_guided() {
        let go_to = Command::GoTo {
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_230,
            altitude_m: -12.3,
            altitude_frame: AltitudeFrame::AboveHome,
            loiter_radius_m: None,
            heading: None,
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

                loiter_radius_m: None,

                heading: None,
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

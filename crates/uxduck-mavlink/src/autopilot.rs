//! Which heartbeats are a vehicle's flight controller (UXD-222). A vehicle's
//! identity, meaning its autopilot, type, mode, armed state and presence,
//! comes from its flight controller alone. Every other component on the
//! vehicle, such as a gimbal, camera, ADS-B receiver or onboard computer,
//! sends a heartbeat of its own type, which says nothing about the vehicle.

/// `MAV_COMP_ID_AUTOPILOT1`: where MAVLink puts a system's flight controller.
pub const AUTOPILOT_COMPONENT_ID: u8 = 1;

/// `MAV_AUTOPILOT_INVALID`: sent by everything that is not a flight controller.
const MAV_AUTOPILOT_INVALID: u8 = 8;

/// `MAV_TYPE_GENERIC`.
const MAV_TYPE_GENERIC: u8 = 0;

/// Whether a `MAV_TYPE` is a kind of vehicle, which only a flight controller
/// reports. The rest of MAVLink's list is ground stations and components:
/// GCS (6), onboard controller (18), gimbal (26), ADS-B (27), camera (30),
/// charging station (31), FLARM (32), servo (33), Open Drone ID (34),
/// battery (36), parachute (37), log (38), OSD (39), IMU (40), GPS (41),
/// winch (42), illuminator (44), gripper (48) and radio (49). A type newer
/// than this list is not a vehicle until it is added here.
#[must_use]
pub const fn vehicle_type(mav_type: u8) -> bool {
    matches!(
        mav_type,
        0..=5 | 7..=17 | 19..=25 | 28 | 29 | 35 | 43 | 45..=47
    )
}

/// [`vehicle_type`] for a type as [`crate::decode`] names it.
#[must_use]
pub fn vehicle_type_named(mav_type: &str) -> bool {
    matches!(
        mav_type,
        "Generic"
            | "FixedWing"
            | "Quadrotor"
            | "Coaxial"
            | "Helicopter"
            | "AntennaTracker"
            | "Airship"
            | "FreeBalloon"
            | "Rocket"
            | "GroundRover"
            | "SurfaceBoat"
            | "Submarine"
            | "Hexarotor"
            | "Octorotor"
            | "Tricopter"
            | "FlappingWing"
            | "Kite"
            | "VtolTailsitterDuorotor"
            | "VtolTailsitterQuadrotor"
            | "VtolTiltrotor"
            | "VtolFixedrotor"
            | "VtolTailsitter"
            | "VtolTiltwing"
            | "VtolReserved5"
            | "Parafoil"
            | "Dodecarotor"
            | "Decarotor"
            | "GenericMultirotor"
            | "SpacecraftOrbiter"
            | "GroundQuadruped"
            | "VtolGyrodyne"
    )
}

/// Whether a heartbeat, by its `type`, `autopilot` and sending component as
/// [`crate::decode`] names them, is a flight controller's: an autopilot and
/// a kind of vehicle. `MAV_TYPE_GENERIC` says nothing about what sent it,
/// so it counts only from the autopilot's own component id.
#[must_use]
pub fn flight_controller(mav_type: &str, autopilot: &str, component_id: u8) -> bool {
    autopilot != "Invalid"
        && vehicle_type_named(mav_type)
        && (mav_type != "Generic" || component_id == AUTOPILOT_COMPONENT_ID)
}

/// [`flight_controller`] for a heartbeat's raw `type` and `autopilot`.
#[must_use]
pub const fn flight_controller_raw(mav_type: u8, autopilot: u8, component_id: u8) -> bool {
    autopilot != MAV_AUTOPILOT_INVALID
        && vehicle_type(mav_type)
        && (mav_type != MAV_TYPE_GENERIC || component_id == AUTOPILOT_COMPONENT_ID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mavio::dialects::ardupilotmega::enums::MavType;

    #[test]
    fn the_named_and_raw_lists_agree_for_every_type_the_dialect_knows() {
        let mut known = 0;
        for raw in 0..=u8::MAX {
            if let Ok(kind) = MavType::try_from(raw) {
                known += 1;
                assert_eq!(
                    vehicle_type_named(&format!("{kind:?}")),
                    vehicle_type(raw),
                    "{kind:?} ({raw})"
                );
            }
        }
        assert!(known >= 48, "{known}");
    }

    #[test]
    fn components_and_ground_stations_are_not_flight_controllers() {
        for (mav_type, raw) in [
            ("Gcs", 6),
            ("OnboardController", 18),
            ("Gimbal", 26),
            ("Adsb", 27),
            ("Camera", 30),
            ("ChargingStation", 31),
            ("Flarm", 32),
            ("Servo", 33),
            ("Odid", 34),
            ("Battery", 36),
            ("Parachute", 37),
            ("Log", 38),
            ("Osd", 39),
            ("Imu", 40),
            ("Gps", 41),
            ("Winch", 42),
            ("Illuminator", 44),
        ] {
            assert!(!flight_controller(mav_type, "Generic", 1), "{mav_type}");
            assert!(!flight_controller_raw(raw, 0, 1), "{mav_type}");
        }
        for raw in [48, 49, 50, 255] {
            assert!(!flight_controller_raw(raw, 0, 1), "{raw}");
        }
    }

    #[test]
    fn a_vehicle_type_with_an_autopilot_is_a_flight_controller() {
        assert!(flight_controller("Quadrotor", "Ardupilotmega", 1));
        assert!(flight_controller("FixedWing", "Px4", 1));
        assert!(flight_controller("GroundRover", "Generic", 1));
        assert!(!flight_controller("Quadrotor", "Invalid", 1));
        assert!(flight_controller_raw(2, 3, 1));
        assert!(!flight_controller_raw(2, 8, 1));
    }

    #[test]
    fn a_generic_type_is_a_flight_controller_only_at_the_autopilots_component() {
        assert!(flight_controller(
            "Generic",
            "Generic",
            AUTOPILOT_COMPONENT_ID
        ));
        assert!(!flight_controller("Generic", "Generic", 154));
        assert!(flight_controller_raw(0, 0, AUTOPILOT_COMPONENT_ID));
        assert!(!flight_controller_raw(0, 0, 154));
    }
}

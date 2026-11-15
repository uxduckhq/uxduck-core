//! A vehicle's kind in plain words, for the agent (FB-173): its autopilot
//! and airframe as the mode menu works them out, so an `ArduPlane`
//! quadplane that reports fixed wing is told as a quadplane.

use uxduck_core::domain::modes::Family;
use uxduck_core::domain::param_definitions::Firmware;
use uxduck_core::domain::vehicle::Vehicle;

use crate::Engine;
use crate::commands::mode_airframe;

impl Engine {
    /// Its autopilot and airframe in plain words, such as "an ArduPilot
    /// plane with VTOL lift motors (`ArduPlane` `QuadPlane`)" or "a PX4
    /// quadcopter". `None` when neither is known.
    #[must_use]
    pub fn vehicle_kind(&self, vehicle: &Vehicle) -> Option<String> {
        let family = Family::for_vehicle(&vehicle.autopilot, &vehicle.vehicle_type);
        let airframe = mode_airframe(self, vehicle, family, self.listed_modes(vehicle));
        let shape = shape(airframe);
        let vtol = airframe.starts_with("Vtol");
        match vehicle.autopilot.as_str() {
            "Ardupilotmega" => {
                let firmware = Firmware::of(airframe).map(|firmware| {
                    if vtol {
                        "ArduPlane QuadPlane"
                    } else {
                        firmware.name()
                    }
                });
                Some(match firmware {
                    Some(firmware) => {
                        format!("an ArduPilot {} ({firmware})", shape.unwrap_or("vehicle"))
                    }
                    None => format!("an ArduPilot {}", shape.unwrap_or("vehicle")),
                })
            }
            "Px4" => Some(format!("a PX4 {}", shape.unwrap_or("vehicle"))),
            _ => shape.map(|shape| {
                let article = if shape.starts_with(['a', 'e', 'i', 'o', 'u']) {
                    "an"
                } else {
                    "a"
                };
                format!("{article} {shape}")
            }),
        }
    }
}

/// An airframe, by its `MAV_TYPE` as decoded, in a few words.
fn shape(airframe: &str) -> Option<&'static str> {
    Some(match airframe {
        "FixedWing" => "plane",
        // A plane with lift motors of its own: an `ArduPlane` quadplane
        // (`mode_airframe`) or PX4's standard VTOL.
        "VtolQuadrotor" | "VtolFixedrotor" => "plane with VTOL lift motors",
        "VtolTiltrotor" => "tiltrotor VTOL plane",
        "VtolTailsitter" | "VtolTailsitterDuorotor" | "VtolTailsitterQuadrotor" => {
            "tailsitter VTOL plane"
        }
        "VtolTiltwing" => "tiltwing VTOL plane",
        t if t.starts_with("Vtol") => "VTOL plane",
        "Quadrotor" => "quadcopter",
        "Hexarotor" => "hexacopter",
        "Octorotor" => "octocopter",
        "Tricopter" => "tricopter",
        "Coaxial" => "coaxial copter",
        "Helicopter" => "helicopter",
        "Decarotor" => "decacopter",
        "Dodecarotor" => "dodecacopter",
        "GenericMultirotor" => "multicopter",
        "GroundRover" => "rover",
        "SurfaceBoat" => "boat",
        "Submarine" => "submarine",
        "Airship" => "blimp",
        "AntennaTracker" => "antenna tracker",
        _ => return None,
    })
}

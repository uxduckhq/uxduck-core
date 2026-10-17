//! Examples (ADR-0029 P12): a plan for each vehicle type, placed relative
//! to home. The Plans page offers them as templates (`plan/examples`), and
//! a new simulated vehicle is sent its type's as its mission, so it has
//! something to fly in Auto. An example is never added to the library.

use uxduck_core::domain::link::{LinkKind, SimAutopilot, SimVehicle};
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::plan::{
    LatLon, PatternShape, PlanAutopilot, PlanBody, PlanFrame, PlanItem, PlanPattern,
    PlanVehicleType, PlannedHome,
};
use uxduck_core::events::Actor;

use crate::Engine;

const EARTH_M: f64 = 6_371_008.8;

/// `AUTO_OPTIONS` for a simulated multicopter: arming in Auto (bit 0) and
/// taking off in it with the throttle low (bit 1), since a simulator's
/// throttle rests at the bottom with nobody on the sticks.
const AUTO_OPTIONS: f64 = 3.0;

/// The place `east_m` and `north_m` from `home`, turned by `heading_deg`
/// (so "north" is the way the vehicle faces).
fn place(home: LatLon, heading_deg: f64, east_m: f64, north_m: f64) -> LatLon {
    let (sin, cos) = heading_deg.to_radians().sin_cos();
    let east = east_m * cos + north_m * sin;
    let north = north_m * cos - east_m * sin;
    LatLon {
        latitude_deg: home.latitude_deg + (north / EARTH_M).to_degrees(),
        longitude_deg: home.longitude_deg
            + (east / (EARTH_M * home.latitude_deg.to_radians().cos().max(1e-6))).to_degrees(),
    }
}

fn item(command: u16, at: Option<LatLon>, altitude_m: f64) -> PlanItem {
    let at = at.unwrap_or(LatLon {
        latitude_deg: 0.0,
        longitude_deg: 0.0,
    });
    PlanItem {
        command,
        frame: PlanFrame::AboveHome.mav_frame(),
        params: [Some(0.0), Some(0.0), Some(0.0), None],
        latitude_deg: at.latitude_deg,
        longitude_deg: at.longitude_deg,
        altitude_m,
        autocontinue: true,
        pattern: None,
    }
}

/// The example for a vehicle type at `home`, facing `heading_deg`: a box
/// for a multicopter, a circuit with a landing sequence for a plane, a
/// take-off, a circuit and a landing for a VTOL, a loop for a rover or
/// boat, a square at depth for a submarine. Heights above home.
#[must_use]
pub fn example(
    vehicle_type: PlanVehicleType,
    autopilot: PlanAutopilot,
    home: LatLon,
    heading_deg: f64,
) -> PlanBody {
    let at = |east: f64, north: f64| Some(place(home, heading_deg, east, north));
    let waypoint = |east: f64, north: f64, altitude: f64| item(16, at(east, north), altitude);
    let (name, items, patterns) = match vehicle_type {
        PlanVehicleType::Multicopter => (
            "Example box",
            vec![
                item(22, None, 20.0),
                waypoint(0.0, 50.0, 30.0),
                waypoint(50.0, 50.0, 30.0),
                waypoint(50.0, 0.0, 30.0),
                waypoint(0.0, 0.0, 30.0),
                item(20, None, 0.0),
            ],
            Vec::new(),
        ),
        PlanVehicleType::Plane => {
            let mut take_off = item(22, None, 60.0);
            // ArduPlane: climb at 15° on the take-off.
            take_off.params[0] = Some(15.0);
            (
                "Example circuit",
                vec![
                    take_off,
                    waypoint(0.0, 600.0, 80.0),
                    waypoint(400.0, 600.0, 80.0),
                    waypoint(400.0, -700.0, 80.0),
                ],
                vec![PlanPattern {
                    id: "landing-1".into(),
                    shape: PatternShape::Landing {
                        touchdown: place(home, heading_deg, 0.0, 0.0),
                        approach_from: place(home, heading_deg, 0.0, -700.0),
                        approach_altitude_m: 50.0,
                    },
                }],
            )
        }
        PlanVehicleType::Vtol => (
            "Example VTOL circuit",
            vec![
                item(84, None, 30.0),
                waypoint(0.0, 400.0, 60.0),
                waypoint(400.0, 400.0, 60.0),
                waypoint(400.0, -300.0, 60.0),
                waypoint(0.0, -300.0, 40.0),
                item(85, at(0.0, 0.0), 0.0),
            ],
            Vec::new(),
        ),
        PlanVehicleType::Rover | PlanVehicleType::Boat => (
            "Example loop",
            vec![
                waypoint(0.0, 30.0, 0.0),
                waypoint(30.0, 30.0, 0.0),
                waypoint(30.0, 0.0, 0.0),
                waypoint(0.0, 0.0, 0.0),
            ],
            Vec::new(),
        ),
        PlanVehicleType::Submarine => (
            "Example square at depth",
            vec![
                waypoint(0.0, 0.0, -5.0),
                waypoint(0.0, 20.0, -5.0),
                waypoint(20.0, 20.0, -5.0),
                waypoint(20.0, 0.0, -5.0),
                waypoint(0.0, 0.0, -5.0),
                waypoint(0.0, 0.0, 0.0),
            ],
            Vec::new(),
        ),
    };
    let mut body = PlanBody::new(name.into(), vehicle_type, autopilot);
    body.frame = PlanFrame::AboveHome;
    body.items = items;
    body.patterns = patterns;
    // The landing's items after the circuit.
    if let Some(pattern) = body.patterns.first() {
        let mut place = item(16, None, 0.0);
        place.pattern = Some(pattern.id.clone());
        body.items.push(place);
    }
    body.planned_home = Some(PlannedHome {
        latitude_deg: home.latitude_deg,
        longitude_deg: home.longitude_deg,
        altitude_m: None,
    });
    crate::plan_patterns::expand(&mut body);
    body
}

/// The plan type a simulated vehicle flies as, if one.
fn plan_type(vehicle: SimVehicle) -> Option<PlanVehicleType> {
    match vehicle {
        SimVehicle::Multicopter => Some(PlanVehicleType::Multicopter),
        SimVehicle::Plane => Some(PlanVehicleType::Plane),
        SimVehicle::Vtol => Some(PlanVehicleType::Vtol),
        SimVehicle::Rover => Some(PlanVehicleType::Rover),
        SimVehicle::Boat => Some(PlanVehicleType::Boat),
        SimVehicle::Submarine => Some(PlanVehicleType::Submarine),
        SimVehicle::Airship => None,
    }
}

impl Engine {
    /// Sends a simulated vehicle its type's example as its mission, once per
    /// start of its simulator, when it was started with one, its mission came
    /// down empty and it was not started from a snapshot (which brings its own). A simulated
    /// multicopter also gets `AUTO_OPTIONS`, so Auto takes off from the
    /// ground. Recorded as the simulation start's; it does not fly it.
    pub(crate) fn send_example_mission(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let Some(link) = self.state.links.get(&vehicle.link_id) else {
            return;
        };
        let LinkKind::Simulator {
            vehicle: kind,
            autopilot,
            latitude_deg,
            longitude_deg,
            heading_deg,
            ref snapshot_id,
            with_mission,
            ..
        } = link.kind
        else {
            return;
        };
        // Only when asked for (Quick start asks); a simulation starts empty.
        if !with_mission {
            return;
        }
        let key = format!("{}:{}", link.id, link.version);
        if snapshot_id.is_some() || self.transfers.examples_sent.contains(&key) {
            return;
        }
        let Some(vehicle_type) = plan_type(kind) else {
            return;
        };
        self.transfers.examples_sent.insert(key);
        let px4 = autopilot == SimAutopilot::Px4;
        let body = example(
            vehicle_type,
            if px4 {
                PlanAutopilot::Px4
            } else {
                PlanAutopilot::Ardupilot
            },
            LatLon {
                latitude_deg,
                longitude_deg,
            },
            heading_deg,
        );
        let mut job = crate::plans::plan_job(self, &body, vehicle_id.to_owned());
        // A simulation's fence is left as it starts.
        job.fence_settings.clear();
        if vehicle_type == PlanVehicleType::Multicopter && !px4 {
            job.fence_settings
                .push(("AUTO_OPTIONS".into(), AUTO_OPTIONS, ParamKind::Int32));
        }
        let actor = Actor {
            client_id: crate::launch::ACTOR.into(),
            operator_id: None,
        };
        // A refusal (busy, not answering) leaves it without; nothing to say.
        let _ = self.upload_plan_mission_as(actor, job);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "heights are set exactly")]
mod tests {
    use uxduck_core::domain::plan::{distance_m, has_position};

    use super::*;

    const HOME: LatLon = LatLon {
        latitude_deg: -35.363,
        longitude_deg: 149.165,
    };

    #[test]
    fn each_type_has_an_example_near_home() {
        for vehicle_type in [
            PlanVehicleType::Multicopter,
            PlanVehicleType::Plane,
            PlanVehicleType::Vtol,
            PlanVehicleType::Rover,
            PlanVehicleType::Boat,
            PlanVehicleType::Submarine,
        ] {
            let body = example(vehicle_type, PlanAutopilot::Ardupilot, HOME, 0.0);
            assert!(body.items.len() >= 4, "{vehicle_type:?}");
            assert_eq!(body.frame, PlanFrame::AboveHome);
            for item in body.items.iter().filter(|i| has_position(i)) {
                let at = LatLon {
                    latitude_deg: item.latitude_deg,
                    longitude_deg: item.longitude_deg,
                };
                assert!(distance_m(HOME, at) < 1_000.0, "{vehicle_type:?} {item:?}");
            }
        }
    }

    #[test]
    fn a_plane_s_circuit_ends_in_a_landing_sequence() {
        let body = example(PlanVehicleType::Plane, PlanAutopilot::Ardupilot, HOME, 90.0);
        let commands: Vec<u16> = body.items.iter().map(|i| i.command).collect();
        assert_eq!(commands, [22, 16, 16, 16, 189, 16, 21]);
        // Facing east, the first leg goes east.
        assert!(body.items[1].longitude_deg > HOME.longitude_deg + 0.005);
        let land = body.items.last().unwrap();
        assert!(
            distance_m(
                HOME,
                LatLon {
                    latitude_deg: land.latitude_deg,
                    longitude_deg: land.longitude_deg
                }
            ) < 1.0
        );
    }
}

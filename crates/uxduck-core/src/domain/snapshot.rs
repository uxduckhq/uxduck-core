//! Simulation snapshots (ADR-0011 M14, S34): a simulated vehicle as it was
//! at a moment, saved to start a new simulation from later. Close, not
//! exact: the simulator's own state (physics, estimator, timers) is not in
//! it, so starting one puts a new vehicle back where the old one was, with
//! its parameters and mission, at its height, in its mode.

use serde::{Deserialize, Serialize};

use crate::domain::link::{SimAutopilot, SimVehicle};
use crate::domain::mission::MissionItem;
use crate::domain::parameter::ParamKind;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub vehicle: SimVehicle,
    /// The autopilot it ran, which a new simulation runs too.
    #[serde(default, skip_serializing_if = "SimAutopilot::is_ardupilot")]
    pub autopilot: SimAutopilot,
    /// The build's frame, such as ArduPilot's `X` or PX4's `quadx`; absent
    /// for the vehicle's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// The firmware and its version, such as `ArduCopter 4.7.0`, when the
    /// vehicle said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Ground level there, above mean sea level: where the new simulation's
    /// home is.
    pub ground_altitude_m: f64,
    /// Above that ground; 0 on the ground.
    pub height_m: f64,
    pub heading_deg: f64,
    pub ground_speed_mps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub armed: bool,
    /// Its parameters as they were. Only the ones that differ from a new
    /// vehicle's are written back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<SnapshotParameter>,
    /// Its mission, home first, as ArduPilot keeps it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mission: Vec<MissionItem>,
    /// The mission item it was flying to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_item: Option<u16>,
    /// Who saved it; `None` for everyone on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub created_at_us: u64,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotParameter {
    pub name: String,
    pub value: f64,
    pub kind: ParamKind,
}

impl Snapshot {
    /// Whether it was saved flying: armed and off the ground. A rover,
    /// boat or submarine never is, whatever its height above home.
    #[must_use]
    pub fn flying(&self) -> bool {
        !matches!(
            self.vehicle,
            SimVehicle::Rover | SimVehicle::Boat | SimVehicle::Submarine
        ) && self.armed
            && self.height_m >= super::command::FLYING_ABOVE_HOME_M
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_something_that_flies_is_saved_flying() {
        let at = |vehicle: SimVehicle| Snapshot {
            id: "s".into(),
            name: "Sim 1".into(),
            vehicle,
            autopilot: SimAutopilot::default(),
            frame: None,
            firmware: None,
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            ground_altitude_m: 0.0,
            // A rover up a hill, or a boat on a lake above home.
            height_m: 20.0,
            heading_deg: 0.0,
            ground_speed_mps: 0.0,
            mode: None,
            armed: true,
            parameters: Vec::new(),
            mission: Vec::new(),
            current_item: None,
            operator_id: None,
            created_at_us: 0,
            version: 1,
        };
        assert!(at(SimVehicle::Multicopter).flying());
        assert!(at(SimVehicle::Airship).flying());
        for vehicle in [SimVehicle::Rover, SimVehicle::Boat, SimVehicle::Submarine] {
            assert!(!at(vehicle).flying(), "{vehicle:?}");
        }
    }
}

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
    /// Whether it was saved flying: armed and off the ground.
    #[must_use]
    pub fn flying(&self) -> bool {
        self.armed && self.height_m >= super::command::FLYING_ABOVE_HOME_M
    }
}

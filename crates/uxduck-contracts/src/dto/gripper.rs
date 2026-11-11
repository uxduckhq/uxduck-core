//! A vehicle's gripper on the wire (S263).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The end of the output's range that opens the claw. Newton opens above
/// its neutral (`max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GripperEnd {
    Max,
    Min,
}

/// What the operator said about a vehicle's gripper: which `ArduSub`
/// actuator it is on and which end opens it. The output and button slots
/// are read from the vehicle's parameters (`gripper/status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GripperSetup {
    /// Actuator 1 to 6.
    pub actuator: u8,
    pub opens_at: GripperEnd,
}

/// A held gripper action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GripperAction {
    Open,
    Close,
}

/// `vehicle/gripper`: says which actuator is the vehicle's gripper, or
/// that it has none. Writes no parameter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleGripperParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Absent: it has no gripper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gripper: Option<GripperSetup>,
}

/// The gripper as the vehicle's parameters wire it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GripperWiring {
    /// The servo output, 1 to 16, the actuator drives.
    pub output: u8,
    /// `BTNn_FUNCTION` slots, 0 to 31, held to open and to close.
    pub open_button: u8,
    pub close_button: u8,
    /// `SERVOn_MIN` and `SERVOn_MAX`, in microseconds.
    pub min_us: u16,
    pub max_us: u16,
    /// Where release returns the output: the middle of the limits.
    pub neutral_us: u16,
}

/// `gripper/status`: whether a vehicle's gripper can be used, and what
/// setup could use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GripperStatus {
    pub vehicle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<GripperSetup>,
    /// Absent while a problem stops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiring: Option<GripperWiring>,
    /// What stops it, in the operator's words, in the order setup fixes them.
    pub problems: Vec<String>,
    /// What does not stop it but should be checked, such as a neutral that
    /// is not 1500 µs.
    pub warnings: Vec<String>,
    /// Outputs with no function (`SERVOn_FUNCTION` 0).
    pub free_outputs: Vec<u8>,
    /// Button slots with no function, plain or shifted.
    pub free_buttons: Vec<u8>,
}

/// `gripper/hold`: the on-screen Open or Close held now, or released. It
/// joins the pilot input this connection already sends the vehicle
/// (keyboard or a claimed controller). Resend at least every 500 ms while
/// held: a hold not refreshed for 500 ms is let go.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GripperHoldParams {
    pub vehicle_id: String,
    /// Absent: released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<GripperAction>,
}

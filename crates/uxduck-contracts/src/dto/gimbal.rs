//! A vehicle's gimbals on the wire (ADR-0063 Y6, S406): each gimbal a
//! manager answers for, what it can do, how far it turns, and who is in
//! control of it. Where it points is in the vehicle's telemetry
//! (`gimbal.pitch_deg`, `gimbal.yaw_deg` from the nose, or
//! `gimbal.heading_deg` from north).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Something a gimbal can do, as its manager's `GIMBAL_MANAGER_CAP_FLAGS`
/// say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalCapability {
    /// It tucks itself away.
    Retract,
    /// It has a neutral position.
    Neutral,
    /// It turns in roll.
    RollAxis,
    /// It turns in pitch.
    PitchAxis,
    /// It turns in yaw.
    YawAxis,
    /// Its yaw follows the vehicle's nose.
    YawFollow,
    /// Its yaw holds north while the vehicle turns.
    YawLock,
    /// Its yaw turns round and round.
    InfiniteYaw,
    /// It keeps looking at a place it is given (`look-at`).
    PointAtPlace,
}

/// How far a gimbal turns, in degrees: roll, pitch from the horizon with
/// down negative, and yaw from the nose with right positive. Each is
/// absent where its manager does not say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_min_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_max_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_min_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch_max_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_min_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_max_deg: Option<f64>,
}

/// A MAVLink system and component in control of a gimbal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalController {
    pub system_id: u8,
    pub component_id: u8,
}

/// Who has a gimbal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalHolder {
    /// Its manager has not said yet.
    Unknown,
    /// No one.
    NoOne,
    /// This UXDuck.
    Uxduck,
    /// The vehicle's autopilot.
    Autopilot,
    /// The mission the vehicle is flying, which points it.
    Mission,
    /// Another ground station or component: `primary` says which.
    Other,
}

/// What to have a gimbal do: one command of the MAVLink gimbal protocol,
/// which its manager acknowledges.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum GimbalAction {
    /// Puts UXDuck in control. UXDuck does this by itself before the
    /// first movement, so it is only for taking a gimbal without moving
    /// it.
    TakeControl,
    /// Gives control back, so that the next to ask has it. UXDuck does
    /// this by itself a while after the last movement.
    ReleaseControl,
    /// Points it: `pitchDeg` from the horizon with down negative, and
    /// `yawDeg` with right positive. The yaw is from the vehicle's nose
    /// and turns with it, or with `lock` from north and held there.
    Point {
        pitch_deg: f32,
        yaw_deg: f32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lock: bool,
    },
    /// Level and straight ahead, following the nose.
    Centre,
    /// Lets the yaw turn with the vehicle's nose, from where it points
    /// now. Needs `yaw-follow`.
    Follow,
    /// Holds the yaw from north while the vehicle turns, from where it
    /// points now. Needs `yaw-lock`.
    HoldNorth,
    /// Keeps looking at a place. Needs `point-at-place`. Without
    /// `altitudeAmslM` it looks at the ground there.
    LookAt {
        latitude_deg: f64,
        longitude_deg: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        altitude_amsl_m: Option<f64>,
    },
}

/// `gimbal/command`: has a gimbal do something.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalCommandParams {
    /// The gimbal's `id` from `gimbal/list`.
    pub gimbal_id: String,
    pub action: GimbalAction,
    /// Confirms the command for a vehicle that is not online, and taking
    /// the gimbal from whoever holds it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// `gimbal/watch`: whether this connection is looking at where a
/// vehicle's gimbals point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalWatchParams {
    pub vehicle_id: String,
    /// False when it stops looking.
    pub watch: bool,
}

/// A gimbal of a vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Gimbal {
    /// `<vehicle id>/<manager's component id>/<device id>`.
    pub id: String,
    pub vehicle_id: String,
    /// The MAVLink component of its manager: the autopilot's, or the
    /// gimbal's own when it manages itself.
    pub component_id: u8,
    /// What its manager calls it: 1 to 6 for one the manager drives
    /// itself, or the gimbal device's MAVLink component.
    pub device_id: u8,
    /// The gimbal device's own MAVLink component, when it has one that is
    /// heard: a camera's `gimbalDeviceId` names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_component_id: Option<u8>,
    /// Whether its manager's heartbeats are heard now. A gimbal that is
    /// gone is kept, and asked again when it returns.
    pub present: bool,
    /// What it can do.
    pub capabilities: Vec<GimbalCapability>,
    /// Its `GIMBAL_MANAGER_CAP_FLAGS` as sent, for a bit UXDuck has no
    /// name for.
    pub flags: u32,
    pub limits: GimbalLimits,
    /// Who has it. Pointing it takes it from them, so a person is asked
    /// first when it is the autopilot, a mission or another.
    pub holder: GimbalHolder,
    /// Who its manager says is in control, and who may also move it.
    /// Absent for no one, and before the manager has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<GimbalController>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<GimbalController>,
    /// Whether its yaw is held from north while the vehicle turns, and
    /// not from the nose. Absent until the gimbal has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaw_locked: Option<bool>,
    /// It is tucked away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retracted: bool,
    /// It is at its neutral position.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub neutral: bool,
    /// What the gimbal says is wrong with it, in the order MAVLink's
    /// `GIMBAL_DEVICE_ERROR_FLAGS` has them. Empty for nothing. A limit it
    /// was sent past is not a failure and is left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<GimbalFailure>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Something a gimbal says is wrong with it (`GIMBAL_DEVICE_ERROR_FLAGS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum GimbalFailure {
    /// An encoder.
    Encoder,
    /// Its power supply.
    Power,
    /// A motor.
    Motor,
    /// Its software.
    Software,
    /// It no longer hears its manager.
    Comms,
    /// It is still calibrating.
    Calibrating,
    /// It has no manager.
    NoManager,
}

/// `gimbal/list`: every gimbal of every vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GimbalListResult {
    pub gimbals: Vec<Gimbal>,
}

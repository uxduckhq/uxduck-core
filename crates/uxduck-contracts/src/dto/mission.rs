//! Missions on the wire (ADR-0004, ADR-0001 entity versions).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::parameter::{TransferStatus, WriteStatus};

/// One item, as `MISSION_ITEM_INT` carries it. In global frames the
/// position is in degrees and metres; other frames carry `param5` and
/// `param6` in the latitude and longitude fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionItem {
    /// `MAV_CMD`: 16 waypoint, 19 loiter for a time, 20 return to launch,
    /// 21 land, 22 take off.
    pub command: u16,
    /// `MAV_FRAME`: 0 above mean sea level, 3 above home, 10 above terrain.
    pub frame: u8,
    pub param1: f64,
    pub param2: f64,
    pub param3: f64,
    pub param4: f64,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    pub autocontinue: bool,
}

/// A vehicle's mission, keyed by `vehicleId`. Item 0 is home on ArduPilot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Mission {
    pub vehicle_id: String,
    pub items: Vec<MissionItem>,
    pub status: TransferStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_instant_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// Bumped when the items change. An upload names the version it was
    /// edited from.
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Uploads a mission edited from `expectedVersion`; refused with
/// `version-conflict` if another write came first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionUploadParams {
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    pub items: Vec<MissionItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionUploadResult {
    pub status: WriteStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The mission as it now stands.
    pub mission: Mission,
}

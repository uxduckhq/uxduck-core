//! A vehicle's mission (ADR-0004, ADR-0001 entity versions). The daemon
//! keeps the mission as last downloaded or uploaded; an upload names the
//! version it was edited from and is refused if another write came first.

use serde::{Deserialize, Serialize};

use crate::domain::parameter::TransferStatus;

/// One item, as `MISSION_ITEM_INT` carries it. In global frames the
/// position is in degrees and metres; other frames carry `param5` and
/// `param6` in the same fields, scaled the same way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionItem {
    /// `MAV_CMD`, such as 16 for a waypoint or 22 for take-off.
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

/// A vehicle's mission. Its id is the vehicle id; item 0 is home on
/// ArduPilot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mission {
    pub vehicle_id: String,
    pub items: Vec<MissionItem>,
    pub status: TransferStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_instant_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// Bumped when the items change, not when only the status does, so an
    /// editor sees a conflict only when there is one.
    pub version: u64,
}

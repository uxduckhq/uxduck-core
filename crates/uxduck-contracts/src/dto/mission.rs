//! Missions on the wire (ADR-0004, ADR-0001 entity versions).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::mission_read::ReadMissionItem;

use crate::dto::parameter::{TransferProgress, TransferStatus, WriteStatus};

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
    pub items: Vec<ReadMissionItem>,
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
    /// How far a download or upload has come while it runs (S127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
    /// Its fence and rally points, as the mission protocol carries them
    /// (S128).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence: Vec<ReadMissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rally: Vec<ReadMissionItem>,
}

/// `mission/get` (FB-256): what UXDuck holds of a vehicle's mission, fence
/// and rally points, how each part was last read from the vehicle, and the
/// fence's shapes and rally points read from those items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionGetResult {
    /// The items, fence and rally points exactly as UXDuck holds them.
    pub mission: Mission,
    /// Where each part stands: the mission's items, the fence and the rally
    /// points.
    pub items: MissionPart,
    pub fence: MissionPart,
    pub rally: MissionPart,
    /// The shapes `mission.fence` makes.
    pub fence_shapes: FenceShapes,
    /// The points `mission.rally` holds.
    pub rally_points: Vec<OnboardRallyPoint>,
    /// Fence and rally items that make no shape or point, each in words with
    /// its item number. None is left out without being named here.
    pub unreadable: Vec<String>,
}

/// Where one part of a vehicle's mission stands (FB-256).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionPart {
    pub status: MissionPartStatus,
    /// How many items UXDuck holds for it.
    pub count: u32,
    /// True only while `status` is `synced`: the last read brought every
    /// item the vehicle said it holds, and nothing was sent since.
    pub complete: bool,
    /// When it was last read from the vehicle, whether or not that read
    /// came whole.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at_us: Option<u64>,
    /// Why it stands as it does, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MissionPartStatus {
    /// Not read from the vehicle yet. Its items, if any, say nothing about
    /// what the vehicle holds.
    NeverRead,
    /// Being read now.
    Reading,
    /// Read whole. No items means the autopilot reported none.
    Synced,
    /// UXDuck is sending it, or sent it or tried to and has not read it
    /// since. Its items are what was sent.
    Sent,
    /// The last read did not come whole. Its items are from an earlier
    /// read or what was sent.
    Failed,
    /// The autopilot does not keep it through the mission protocol, or
    /// UXDuck does not read it from this kind of vehicle.
    Unsupported,
}

/// A fence's shapes, as the mission protocol carries them (FB-256).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FenceShapes {
    pub polygons: Vec<OnboardFencePolygon>,
    pub circles: Vec<OnboardFenceCircle>,
    /// Where a plane returns to on a breach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_point: Option<crate::dto::plan::LatLon>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OnboardFencePolygon {
    /// Inside is allowed. Otherwise inside is kept out of.
    pub inclusion: bool,
    /// Its first item in `mission.fence`, from 0.
    pub first_item: u32,
    pub points: Vec<crate::dto::plan::LatLon>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OnboardFenceCircle {
    /// Inside is allowed. Otherwise inside is kept out of.
    pub inclusion: bool,
    /// Its item in `mission.fence`, from 0.
    pub item: u32,
    pub center: crate::dto::plan::LatLon,
    pub radius_m: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OnboardRallyPoint {
    /// Its item in `mission.rally`, from 0.
    pub item: u32,
    /// `MAV_FRAME` of its height: 0 or 5 above mean sea level, 3 or 6 above
    /// home, 10 or 11 above terrain.
    pub frame: u8,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
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

/// What `transfer/cancel` stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TransferWhat {
    Parameters,
    Mission,
}

/// `transfer/cancel`: stops a parameter download or a mission transfer; a
/// mission is read back afterwards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransferCancelParams {
    pub vehicle_id: String,
    pub what: TransferWhat,
}

/// How a mission came to be on a vehicle (S134).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum MissionOrigin {
    /// On the vehicle when UXDuck first read it.
    Found,
    /// Uploaded from UXDuck, from a plan when one is named; `verified` once
    /// read back.
    Uploaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verified: Option<bool>,
    },
    /// A simulated vehicle's example, sent as it started.
    Example,
    /// Changed by something UXDuck did not send.
    ChangedElsewhere,
}

/// One mission a vehicle has held (S134), kept once by its content. Items
/// leave out ArduPilot's home at item 0.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionRecord {
    pub id: String,
    pub vehicle_id: String,
    pub items: Vec<ReadMissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence: Vec<ReadMissionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rally: Vec<ReadMissionItem>,
    pub origin: MissionOrigin,
    /// How it came, in words: "Uploaded from Ridge survey".
    pub origin_text: String,
    /// What it holds, in words: "38 items, a fence".
    pub counts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_seen_us: u64,
    /// When it last became the vehicle's mission.
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_seen_us: u64,
    pub times: u32,
}

/// `mission/history`: a vehicle's missions, newest first; the first is the
/// one it holds, when UXDuck knows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionHistoryResult {
    pub records: Vec<MissionRecord>,
}

/// `mission/restore` (S134): puts a mission from a vehicle's history back
/// on it, or on another vehicle, with its fence and rally points, and reads
/// it back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionRestoreParams {
    pub record_id: String,
    /// The vehicle to put it on; the one that held it when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// As `PlanUploadResult`, without a plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MissionRestoreResult {
    pub status: crate::dto::plan::PlanUploadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub notes: Vec<String>,
    pub mission: Mission,
}

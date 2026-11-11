//! Terrain heights (S23, ADR-0026): the ground at a point from UXDuck's
//! own copy of a dataset.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::plan::LatLon;

/// A terrain dataset (ADR-0026 T1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerrainDataset {
    /// JAXA's AW3D30, as ArduPilot's SRTM1 files: the default.
    #[default]
    Alos,
    /// Copernicus GLO-30, as QGroundControl serves.
    Copernicus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainAtParams {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// The dataset to read; Settings › Terrain's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<TerrainDataset>,
}

/// Why there is no height at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerrainMissing {
    /// The dataset has no height here: a void, or a place it does not cover.
    NoData,
    /// Not kept on this computer, and there is no connection to fetch it.
    Offline,
    /// The manifest or the data could not be had or did not check out;
    /// `detail` says why.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainAtResult {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub dataset: TerrainDataset,
    /// The release answered from, such as `2026-10-14`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Metres above the dataset's geoid, to the decimetre; absent when
    /// there is no height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_m: Option<f64>,
    /// The geoid heights are above: `EGM96` (ALOS) or `EGM2008`
    /// (Copernicus).
    pub geoid: String,
    /// Sea, by the tile grid: the height is 0 m.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sea: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<TerrainMissing>,
    /// Why, in words, when there is no height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainCheckParams {
    pub vehicle_id: String,
    /// A point to check; without one, home and the point under the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainStatusParams {
    pub vehicle_id: String,
}

/// What a check found (S119).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerrainVerdict {
    /// Within 2 m of the vehicle's dataset everywhere checked.
    Agrees,
    /// Off by more than 2 m, and not the other dataset either.
    Differs,
    /// Off from its dataset, but within 2 m of the other one.
    MatchesOther,
    /// A uniform difference could be ArduPilot's arming reference offset.
    /// Reports contain corrected heights, so this does not verify the files.
    OffsetUncertain,
    /// The vehicle has no terrain at the points checked.
    NoTerrain,
    /// The vehicle did not answer.
    NoReply,
    /// UXDuck has no height there to compare with.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainCheckPoint {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// The vehicle's own terrain height there, as it reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_m: Option<f64>,
    /// What the vehicle's dataset gives there on the vehicle's own grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_m: Option<f64>,
    /// What the other dataset gives there, the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_m: Option<f64>,
    /// Whether the vehicle answered for this point.
    pub answered: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainCheckResult {
    pub vehicle_id: String,
    /// The vehicle's dataset, compared with.
    pub dataset: TerrainDataset,
    pub verdict: TerrainVerdict,
    /// The largest difference from the vehicle's dataset, in metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difference_m: Option<f64>,
    pub points: Vec<TerrainCheckPoint>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub checked_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainStatusResult {
    pub vehicle_id: String,
    /// The dataset the vehicle is answered from and checked against.
    pub dataset: TerrainDataset,
    /// Whether it was chosen for this vehicle, rather than the default.
    pub chosen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<TerrainCheckResult>,
}

/// A vehicle's own dataset, chosen on its page or from a warning (S119).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainDataset {
    pub vehicle_id: String,
    /// `None` goes back to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<TerrainDataset>,
}

/// An area whose terrain is kept on this computer for use offline (S120):
/// a polygon (ADR-0051 A5), kept while it is enabled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainArea {
    pub id: String,
    pub name: String,
    /// Its corners in order; the last joins the first.
    pub points: Vec<LatLon>,
    /// Kept on this computer; a disabled area is drawn, and not kept.
    pub enabled: bool,
}

/// Areas are compared as written; their corners are never NaN.
impl Eq for TerrainArea {}

/// A polygon: at least three corners in order, the last joined to the
/// first, at most 500. An edge goes the shorter way between its corners,
/// so across the antimeridian when that is shorter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainPolygon {
    pub points: Vec<LatLon>,
}

/// Keeps a new area, or changes one kept already by its `id`: its name,
/// corners, and whether it is enabled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainKeepAreaParams {
    /// An area from `terrain/areas` to change; absent keeps a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub points: Vec<LatLon>,
    /// Absent: a new area is enabled, and a changed one stays as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// What an area would take to keep, from the default dataset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainAreaSize {
    pub chunks: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// Why the size is not known, such as being offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How keeping an area is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerrainKeeping {
    /// Being fetched.
    Fetching,
    /// All of it is on this computer.
    Kept,
    /// Some could not be fetched; `detail` says why.
    Incomplete,
    /// Disabled: drawn, and not kept.
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainAreaStatus {
    pub area: TerrainArea,
    pub state: TerrainKeeping,
    pub chunks: u32,
    pub done: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// How it stands on each vehicle it is going to or is on (ADR-0051 A8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicles: Vec<VehicleTerrainArea>,
}

/// `terrain/putArea`: writes areas onto a vehicle's card now, under
/// ADR-0051 A7's rules (never while it is armed), or removes one's blocks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainPutAreaParams {
    pub vehicle_id: String,
    /// One area from `terrain/areas`; absent: every enabled area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_id: Option<String>,
    /// Removes the blocks UXDuck wrote for the area from the vehicle
    /// instead (A10's "Remove from Alpha"); needs `areaId`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remove: bool,
}

/// How an area stands on a vehicle (ADR-0051 A8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TerrainPutState {
    /// To be written; `detail` says what it waits for (armed, an answer,
    /// its heights, offline).
    Waiting,
    Writing,
    /// On the vehicle's card: every block written and acknowledged, or
    /// skipped.
    Written,
    /// Written from a dataset or release that is no longer the vehicle's.
    Stale,
    /// Its blocks are being removed from the card.
    Removing,
    Failed,
}

/// One area on one vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainArea {
    pub area_id: String,
    /// The area's name; absent for one deleted since it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub vehicle_id: String,
    pub vehicle_name: String,
    pub state: TerrainPutState,
    /// Blocks to be on the card, and those acknowledged.
    pub blocks: u32,
    pub written: u32,
    /// Left off because a height was missing; `detail` says why.
    pub skipped: u32,
    /// Bytes this pass writes, and how fast they go.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub rate_bps: Option<u64>,
    /// When it was last written whole, microseconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub written_at_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A block of an area on the vehicle's card, for the map (A2, A8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VehicleCardBlockState {
    /// Written and acknowledged.
    Written,
    /// Being written now.
    Writing,
    /// Still to be written.
    Waiting,
    /// Left off: a height was missing.
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleCardBlock {
    /// South-west, north-west, north-east and south-east.
    pub corners: Vec<crate::dto::plan::LatLon>,
    pub state: VehicleCardBlockState,
    pub area_id: String,
}

/// What putting areas on a vehicle would take, while UXDuck waits for the
/// person to agree (A7): `terrain/putArea` agrees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainPutAsk {
    pub areas: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// At the link's pace.
    pub seconds: u32,
    /// Over a telemetry radio.
    pub radio: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainAreasResult {
    /// The dataset areas are kept from: the default.
    pub dataset: TerrainDataset,
    pub areas: Vec<TerrainAreaStatus>,
    /// When the bucket's manifest last verified, microseconds since 1970.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub manifest_checked_at_us: Option<u64>,
}

/// `terrain/vehicle`: one vehicle's terrain as UXDuck knows it (S310,
/// ADR-0051 A1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainParams {
    pub vehicle_id: String,
    /// List the vehicle's terrain folder over MAVLink FTP as well, as its
    /// page does when it opens: never while it is armed on a radio.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list_files: bool,
}

/// How a block of a vehicle's grid stands this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VehicleTerrainBlockState {
    /// Asked for; nothing sent yet (its heights may be on their way here).
    Asked,
    /// Some of the groups asked for were sent.
    Sending,
    /// Every group the vehicle asked for was sent.
    Sent,
    /// Asked for, and UXDuck has no heights to answer it with; `detail`
    /// says why.
    Unavailable,
}

/// A block of the vehicle's grid: 28 points north by 32 east at its
/// spacing, from the south-west corner it asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainBlock {
    /// South-west, north-west, north-east and south-east, as ArduPilot
    /// places the block's outer points.
    pub corners: Vec<crate::dto::plan::LatLon>,
    pub spacing_m: u16,
    pub state: VehicleTerrainBlockState,
    /// Groups of 4 × 4 points the vehicle asked for this session, of 56.
    pub groups: u8,
    /// Of those, the groups sent or no longer asked for.
    pub groups_done: u8,
    /// Why it cannot be answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the vehicle last asked for it, microseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub asked_at_us: u64,
}

/// A file in the vehicle's terrain folder: one degree of its grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainFile {
    /// Such as `N47E035.DAT`.
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub size: u64,
}

/// The vehicle's terrain folder as last listed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainFiles {
    /// `/APM/TERRAIN` on boards, `terrain` on a simulator.
    pub path: String,
    pub files: Vec<VehicleTerrainFile>,
    /// When it was listed, microseconds since 1970; absent until it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub listed_at_us: Option<u64>,
    /// Being listed now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listing: bool,
    /// Why the last listing did not happen or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One vehicle's terrain: what it asked for and was sent this session, its
/// own counts, its dataset, its card's files and its last check (A1). Also
/// `terrain/vehicleChanged`'s.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts about one vehicle's terrain"
)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTerrainResult {
    pub vehicle_id: String,
    /// Whether the vehicle uses terrain: it asked for some, said it had
    /// groups pending, or holds a mission above terrain.
    pub used: bool,
    pub dataset: TerrainDataset,
    /// Whether the dataset was chosen for this vehicle.
    pub chosen: bool,
    /// Its `TERRAIN_SPACING`, once its parameters came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spacing_m: Option<u16>,
    /// Its `TERRAIN_ENABLE`, once its parameters came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `TERRAIN_REPORT`'s groups waiting and in memory (`terrain.pending`,
    /// `terrain.loaded`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<u32>,
    /// The blocks it asked for this session, oldest first.
    pub blocks: Vec<VehicleTerrainBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<VehicleTerrainFiles>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<TerrainCheckResult>,
    /// Areas going to its card or on it (ADR-0051 A8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<VehicleTerrainArea>,
    /// Their blocks, for the map: at most 2000, the rest counted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub card_blocks: Vec<VehicleCardBlock>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub card_blocks_omitted: u32,
    /// "Keep areas off this vehicle" (A7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub areas_off: bool,
    /// Areas go to it by themselves; otherwise UXDuck asks first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub areas_automatic: bool,
    /// Waiting for the person to agree to put areas on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<TerrainPutAsk>,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

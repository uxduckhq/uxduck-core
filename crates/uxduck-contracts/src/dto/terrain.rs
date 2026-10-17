//! Terrain heights (S23, ADR-0026): the ground at a point from UXDuck's
//! own copy of a dataset.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

/// An area whose terrain is kept on this computer for use offline (S120).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainArea {
    pub id: String,
    pub name: String,
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
}

/// Areas are compared as written; their corners are never NaN.
impl Eq for TerrainArea {}

/// A rectangle, south-west to north-east, not across the antimeridian.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainBounds {
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerrainKeepAreaParams {
    pub name: String,
    #[serde(flatten)]
    pub bounds: TerrainBounds,
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

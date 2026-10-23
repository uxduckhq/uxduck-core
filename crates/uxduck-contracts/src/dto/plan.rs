//! Plans on the wire (ADR-0029): missions saved in UXDuck for a vehicle
//! type, with a fence and rally points. The shapes match
//! `uxduck_core::domain::plan` field for field; `convert::plan` moves
//! between them through serde and a test keeps them equal.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a plan is for. Helicopters plan as multicopters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanVehicleType {
    #[default]
    Multicopter,
    Plane,
    Vtol,
    Rover,
    Boat,
    Submarine,
    Blimp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanAutopilot {
    #[default]
    Ardupilot,
    Px4,
}

/// The plan's one frame: above terrain (`MAV_FRAME` 10), above home (3) or
/// above sea level (0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanFrame {
    #[default]
    AboveTerrain,
    AboveHome,
    AboveSeaLevel,
}

/// One item as `MISSION_ITEM_INT` carries it. A `null` parameter is sent
/// as NaN: "no change" or the autopilot's default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    /// `MAV_CMD`.
    pub command: u16,
    /// `MAV_FRAME`.
    pub frame: u8,
    pub params: [Option<f64>; 4],
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    pub autocontinue: bool,
    /// The pattern that made it (P11): made again from the pattern when the
    /// plan is saved. Edit the pattern, not these items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A survey, corridor, orbit or landing kept as its shape (ADR-0029 P11).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanPattern {
    pub id: String,
    pub shape: PatternShape,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PatternShape {
    /// Lanes across an area, back and forth.
    #[serde(rename_all = "camelCase")]
    Survey {
        area: Vec<LatLon>,
        spacing_m: f64,
        /// The lanes' bearing, clockwise from north.
        angle_deg: f64,
        turnaround_m: f64,
        altitude_m: f64,
        #[serde(default)]
        crosshatch: bool,
    },
    /// Lanes along a line, `widthM` wide in all.
    #[serde(rename_all = "camelCase")]
    Corridor {
        line: Vec<LatLon>,
        width_m: f64,
        spacing_m: f64,
        turnaround_m: f64,
        altitude_m: f64,
    },
    /// Circles round a point at each height in turn.
    #[serde(rename_all = "camelCase")]
    Orbit {
        center: LatLon,
        radius_m: f64,
        turns: f64,
        heights_m: Vec<f64>,
        #[serde(default)]
        counter_clockwise: bool,
    },
    /// A landing sequence start, the approach and the landing.
    #[serde(rename_all = "camelCase")]
    Landing {
        touchdown: LatLon,
        approach_from: LatLon,
        approach_altitude_m: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LatLon {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FencePolygon {
    pub inclusion: bool,
    pub points: Vec<LatLon>,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FenceCircle {
    pub inclusion: bool,
    pub center: LatLon,
    pub radius_m: f64,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FenceAction {
    Report,
    ReturnToLaunch,
    Land,
    Brake,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FenceSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<FenceAction>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Fence {
    pub polygons: Vec<FencePolygon>,
    pub circles: Vec<FenceCircle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_point: Option<LatLon>,
    pub settings: FenceSettings,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RallyPoint {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where an above-home plan expects to arm.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlannedHome {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanAuthor {
    #[default]
    Person,
    Agent,
}

/// What a plan holds: what a revision keeps and undo moves between.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanBody {
    pub name: String,
    pub vehicle_type: PlanVehicleType,
    pub autopilot: PlanAutopilot,
    pub frame: PlanFrame,
    pub items: Vec<PlanItem>,
    pub fence: Fence,
    pub rally: Vec<RallyPoint>,
    /// Surveys, corridors, orbits and landings (P11); their items are in
    /// `items`, tagged with their id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<PlanPattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_home: Option<PlannedHome>,
    /// The height the next waypoint added gets, in the plan's frame.
    pub next_altitude_m: f64,
    /// The minimum clearance over the ground the profile checks.
    pub min_clearance_m: f64,
    pub labels: Vec<String>,
    pub notes: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
}

/// An upload of the plan to a vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpload {
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub plan_version: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub verified: bool,
    /// The vehicle's mission version it became: a later one means the
    /// mission changed since.
    #[schemars(schema_with = "crate::safe::integer")]
    #[serde(default)]
    pub mission_version: u64,
}

/// A saved plan. Never uploaded by itself: it reaches a vehicle only when
/// someone uploads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: String,
    #[serde(flatten)]
    pub body: PlanBody,
    /// The nearest place's name, looked up by the app when the plan moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_at: Option<LatLon>,
    /// Where its points are; absent while it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    pub made_by: PlanAuthor,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub changed_at_us: u64,
    /// Set while in Deleted; gone 30 days later.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_us: Option<u64>,
    pub uploads: Vec<PlanUpload>,
    /// How many changes can be undone and redone.
    pub undo: u32,
    pub redo: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `plan/create`: a new plan, at once, named "Untitled plan" unless named.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_type: Option<PlanVehicleType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autopilot: Option<PlanAutopilot>,
    /// A whole body to start from (an example, a copy); `name`,
    /// `vehicleType` and `autopilot` override its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<PlanBody>,
}

/// `plan/update`: the plan's whole body as edited from `expectedVersion`.
/// Each update is one revision that undo can go back over.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    pub body: PlanBody,
}

/// `plan/undo`, `plan/redo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanStepParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    /// Back to the revision the plan had at this version (`plan/undo`
    /// only): "Back to when I opened it".
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<u64>,
}

/// `plan/place`: the nearest place's name, found by the app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanPlaceParams {
    pub id: String,
    pub name: String,
    pub at: LatLon,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanListParams {
    /// Include plans in Deleted.
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanListResult {
    pub plans: Vec<Plan>,
}

/// A file's name and text, read by the app or the CLI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanFile {
    pub name: String,
    pub text: String,
}

/// `plan/import`: files read into one plan: a QGroundControl `.plan`,
/// Mission Planner's waypoint, fence and rally files (several at once),
/// ArduPilot's fence text, or KML.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanImportParams {
    pub files: Vec<PlanFile>,
    /// The type when the files do not say (the selected vehicle's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_type: Option<PlanVehicleType>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanImportResult {
    pub plan: Plan,
    /// What could not be read, one line each.
    pub skipped: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanExportFormat {
    /// One QGroundControl `.plan` with all three parts.
    #[default]
    Qgc,
    /// Mission Planner's waypoint, fence and rally files.
    MissionPlanner,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanExportParams {
    pub id: String,
    #[serde(default)]
    pub format: PlanExportFormat,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanExportResult {
    pub files: Vec<PlanFile>,
}

/// `plan/upload`: a plan sent to a vehicle as its mission, then read back
/// and compared. Needs `control`; an agent's waits for approval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanUploadParams {
    pub plan_id: String,
    pub vehicle_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanUploadStatus {
    /// Read back the same, allowing for what the autopilot rounds.
    Verified,
    /// Accepted, but read back different: `notes` say how.
    Different,
    /// The vehicle refused it or stopped answering; it was read back.
    Failed,
    /// Cancelled; it was read back.
    Cancelled,
    /// Not sent: `detail` says why.
    NotSent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanUploadResult {
    pub status: PlanUploadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the autopilot changed or rounded: "Item 4's delay kept as 2 s".
    pub notes: Vec<String>,
    pub plan: Plan,
    pub mission: crate::dto::mission::Mission,
}

/// `plan/check`: a plan's problems (ADR-0029 P9), checked against the
/// terrain a vehicle would use when one is named.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanCheckParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PlanCheckSeverity {
    /// Stops an upload.
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanCheck {
    pub severity: PlanCheckSeverity,
    /// `below-ground`, `low-clearance`, `terrain-missing`, `outside-fence`,
    /// `home-outside-fence`, `fence-shape`, `command-unfit`, `no-take-off`,
    /// `no-end`, `no-landing-sequence`, `rally-outside-fence`, `high`,
    /// `above-ceiling`, `lands-early`.
    pub code: String,
    /// The item it is about, numbered from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanCheckResult {
    pub checks: Vec<PlanCheck>,
}

/// `plan/examples` (ADR-0029 P12): where to place the examples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanExamplesParams {
    pub at: LatLon,
    /// The way "forward" faces, clockwise from north; north when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanExamplesResult {
    /// One per vehicle type: multicopter, plane, VTOL, rover, boat, submarine, blimp.
    pub examples: Vec<PlanBody>,
}

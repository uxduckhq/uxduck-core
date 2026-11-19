//! Plans (ADR-0029): missions saved in UXDuck for a vehicle type, with a
//! fence and rally points, that reach a vehicle only when uploaded. A plan
//! is never the vehicle's mission; `mission` is what a vehicle holds.

use serde::{Deserialize, Serialize};

/// What a plan is for (P3).
/// Helicopters plan as multicopters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

impl PlanVehicleType {
    /// Whether altitude means anything for it: rovers and boats ignore it.
    #[must_use]
    pub fn flies(self) -> bool {
        matches!(
            self,
            Self::Multicopter | Self::Plane | Self::Vtol | Self::Blimp
        )
    }

    /// What it is in a few words, as the agent is told: "a VTOL".
    #[must_use]
    pub fn words(self) -> &'static str {
        match self {
            Self::Multicopter => "a multicopter",
            Self::Plane => "a plane",
            Self::Vtol => "a VTOL",
            Self::Rover => "a rover",
            Self::Boat => "a boat",
            Self::Submarine => "a submarine",
            Self::Blimp => "a blimp",
        }
    }

    /// The frame a new plan of this type starts in (P4).
    #[must_use]
    pub fn default_frame(self) -> PlanFrame {
        match self {
            Self::Multicopter | Self::Plane | Self::Vtol => PlanFrame::AboveTerrain,
            // Rovers and boats ignore heights; subs give depth as a
            // negative height above home (normally the surface).
            Self::Rover | Self::Boat | Self::Submarine | Self::Blimp => PlanFrame::AboveHome,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanAutopilot {
    #[default]
    Ardupilot,
    Px4,
}

/// One frame for the whole plan (P4). Items carry their own `MAV_FRAME`
/// only when "Choose a frame for each item" is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanFrame {
    #[default]
    AboveTerrain,
    AboveHome,
    AboveSeaLevel,
}

impl PlanFrame {
    /// The `MAV_FRAME` an item in this frame is sent with.
    #[must_use]
    pub fn mav_frame(self) -> u8 {
        match self {
            Self::AboveSeaLevel => 0,
            Self::AboveHome => 3,
            Self::AboveTerrain => 10,
        }
    }

    /// What a height in it is measured from: "above terrain".
    #[must_use]
    pub fn words(self) -> &'static str {
        match self {
            Self::AboveTerrain => "above terrain",
            Self::AboveHome => "above home",
            Self::AboveSeaLevel => "above sea level",
        }
    }

    /// The plan frame a `MAV_FRAME` belongs to, `_INT` variants included;
    /// `None` for frames that are not a height over the globe.
    #[must_use]
    pub fn from_mav_frame(frame: u8) -> Option<Self> {
        match frame {
            0 | 5 => Some(Self::AboveSeaLevel),
            3 | 6 => Some(Self::AboveHome),
            10 | 11 => Some(Self::AboveTerrain),
            _ => None,
        }
    }
}

/// One waypoint-list item, as `MISSION_ITEM_INT` carries it. A parameter
/// that is absent is sent as NaN: "no change" or the autopilot's default
/// (QGroundControl writes `null`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    /// `MAV_CMD`.
    pub command: u16,
    /// `MAV_FRAME`. In a plan without per-item frames it is the plan's.
    pub frame: u8,
    pub params: [Option<f64>; 4],
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    pub autocontinue: bool,
    /// The pattern (P11) that made it: its items are made again from the
    /// pattern whenever the plan is saved, in the place of the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A shape flown often (P11), kept as its shape and settings. The items it
/// makes are in the plan's items, tagged with its id, so the upload, the
/// profile and the checks see what is flown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanPattern {
    pub id: String,
    pub shape: PatternShape,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PatternShape {
    /// Lanes across an area, back and forth.
    #[serde(rename_all = "camelCase")]
    Survey {
        area: Vec<LatLon>,
        /// Between lanes.
        spacing_m: f64,
        /// The lanes' bearing, clockwise from north.
        angle_deg: f64,
        /// Flown past the area's edge before turning.
        turnaround_m: f64,
        /// In the plan's frame.
        altitude_m: f64,
        /// The lanes again at right angles.
        #[serde(default)]
        crosshatch: bool,
        /// Kept from the area's edge and from the plan's exclusion zones,
        /// which the lanes stop short of and the flight goes round.
        #[serde(default)]
        margin_m: f64,
        /// Where the first lane starts; as drawn when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<SurveyStart>,
        /// UXDuck picks the angle with the fewest lanes, then the shortest
        /// flight, and keeps it in `angle_deg`.
        #[serde(default)]
        shortest: bool,
    },
    /// Lanes along a line, `width_m` wide in all.
    #[serde(rename_all = "camelCase")]
    Corridor {
        line: Vec<LatLon>,
        width_m: f64,
        spacing_m: f64,
        turnaround_m: f64,
        altitude_m: f64,
    },
    /// Circles round a point, at each height in turn.
    #[serde(rename_all = "camelCase")]
    Orbit {
        center: LatLon,
        radius_m: f64,
        turns: f64,
        heights_m: Vec<f64>,
        #[serde(default)]
        counter_clockwise: bool,
    },
    /// A plane's or VTOL's landing (P11): a landing sequence start where the
    /// approach begins, the approach, and the landing at the touchdown.
    #[serde(rename_all = "camelCase")]
    Landing {
        touchdown: LatLon,
        approach_from: LatLon,
        approach_altitude_m: f64,
    },
}

/// Where a survey's first lane starts: the lane end nearest a corner of
/// the area, or nearest the plan's home.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurveyStart {
    NorthWest,
    NorthEast,
    SouthEast,
    SouthWest,
    Home,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatLon {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FencePolygon {
    /// Inside is allowed; otherwise inside is kept out of.
    pub inclusion: bool,
    pub points: Vec<LatLon>,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FenceCircle {
    pub inclusion: bool,
    pub center: LatLon,
    pub radius_m: f64,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What the vehicle does on a breach (ArduPilot's `FENCE_ACTION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FenceAction {
    Report,
    ReturnToLaunch,
    Land,
    Brake,
}

/// Fence settings (P10): written as parameters after the fence.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FenceSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<FenceAction>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fence {
    #[serde(default)]
    pub polygons: Vec<FencePolygon>,
    #[serde(default)]
    pub circles: Vec<FenceCircle>,
    /// A plane's return point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_point: Option<LatLon>,
    #[serde(default)]
    pub settings: FenceSettings,
}

impl Fence {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.polygons.is_empty() && self.circles.is_empty() && self.return_point.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RallyPoint {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: f64,
    /// What the person called it; kept in the plan only, never sent to the vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where an above-home plan expects to arm: item 0 on ArduPilot.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedHome {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Above sea level, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
}

/// Who made or last changed it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanAuthor {
    #[default]
    Person,
    Agent,
}

/// The plan's contents: what a revision keeps and undo moves between.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBody {
    pub name: String,
    pub vehicle_type: PlanVehicleType,
    pub autopilot: PlanAutopilot,
    pub frame: PlanFrame,
    #[serde(default)]
    pub items: Vec<PlanItem>,
    #[serde(default)]
    pub fence: Fence,
    #[serde(default)]
    pub rally: Vec<RallyPoint>,
    /// Patterns (P11), whose items are among `items`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<PlanPattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_home: Option<PlannedHome>,
    /// The height a waypoint added next gets, in the plan's frame.
    pub next_altitude_m: f64,
    /// The profile's minimum clearance over the ground (P9).
    pub min_clearance_m: f64,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub notes: String,
    /// The MAVLink dialects whose commands it offers, by id, beside those
    /// for every vehicle (ADR-0044 F14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialects: Vec<String>,
}

/// What an item's pattern id takes beside the item: a short text of its
/// own, such as "survey-1", in the 32 bytes an allocator gives it.
const ITEM_ID_BYTES: usize = 32;

pub const DEFAULT_NEXT_ALTITUDE_M: f64 = 40.0;
pub const DEFAULT_MIN_CLEARANCE_M: f64 = 10.0;

impl PlanBody {
    #[must_use]
    pub fn new(name: String, vehicle_type: PlanVehicleType, autopilot: PlanAutopilot) -> Self {
        Self {
            name,
            vehicle_type,
            autopilot,
            frame: vehicle_type.default_frame(),
            next_altitude_m: if vehicle_type.flies() {
                DEFAULT_NEXT_ALTITUDE_M
            } else {
                0.0
            },
            min_clearance_m: DEFAULT_MIN_CLEARANCE_M,
            ..Self::default()
        }
    }

    /// About the bytes it holds in memory, added up from its lengths and
    /// never by writing it out: what a kept revision costs
    /// ([`MAX_REVISIONS_BYTES`]). Every item counts as one a pattern made,
    /// with its pattern's id beside it, so the sum is a quarter high for
    /// items placed by hand and needs no walk through them.
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        let named = |name: &Option<String>| name.as_ref().map_or(0, String::len);
        let texts = |texts: &[String]| -> usize {
            texts
                .iter()
                .map(|text| size_of::<String>() + text.len())
                .sum()
        };
        let patterns: usize = self
            .patterns
            .iter()
            .map(|pattern| {
                size_of::<PlanPattern>()
                    + pattern.id.len()
                    + match &pattern.shape {
                        PatternShape::Survey { area, .. } => size_of_val(area.as_slice()),
                        PatternShape::Corridor { line, .. } => size_of_val(line.as_slice()),
                        PatternShape::Orbit { heights_m, .. } => size_of_val(heights_m.as_slice()),
                        PatternShape::Landing { .. } => 0,
                    }
            })
            .sum();
        let polygons: usize = self
            .fence
            .polygons
            .iter()
            .map(|polygon| {
                size_of::<FencePolygon>()
                    + size_of_val(polygon.points.as_slice())
                    + named(&polygon.name)
            })
            .sum();
        let circles: usize = self
            .fence
            .circles
            .iter()
            .map(|circle| size_of::<FenceCircle>() + named(&circle.name))
            .sum();
        let rally: usize = self
            .rally
            .iter()
            .map(|point| size_of::<RallyPoint>() + named(&point.name))
            .sum();
        size_of::<Self>()
            + self.items.len() * (size_of::<PlanItem>() + ITEM_ID_BYTES)
            + patterns
            + polygons
            + circles
            + rally
            + self.name.len()
            + self.notes.len()
            + texts(&self.labels)
            + texts(&self.dialects)
    }

    /// The points that say where the plan is: positioned items, fence and
    /// rally points. Items without a position (0°, 0°) are left out.
    #[must_use]
    pub fn points(&self) -> Vec<LatLon> {
        let mut points: Vec<LatLon> = self
            .items
            .iter()
            .filter(|item| has_position(item))
            .map(|item| LatLon {
                latitude_deg: item.latitude_deg,
                longitude_deg: item.longitude_deg,
            })
            .collect();
        for polygon in &self.fence.polygons {
            points.extend(polygon.points.iter().copied());
        }
        points.extend(self.fence.circles.iter().map(|circle| circle.center));
        points.extend(self.rally.iter().map(|rally| LatLon {
            latitude_deg: rally.latitude_deg,
            longitude_deg: rally.longitude_deg,
        }));
        if points.is_empty()
            && let Some(home) = self.planned_home
        {
            points.push(LatLon {
                latitude_deg: home.latitude_deg,
                longitude_deg: home.longitude_deg,
            });
        }
        points
    }

    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::around(&self.points())
    }
}

/// Whether an item is somewhere: its command carries a position in a global
/// frame and it is not at 0°, 0° (P6).
#[must_use]
pub fn has_position(item: &PlanItem) -> bool {
    command_has_position(item.command)
        && PlanFrame::from_mav_frame(item.frame).is_some()
        && !(item.latitude_deg == 0.0 && item.longitude_deg == 0.0)
        && item.latitude_deg.is_finite()
        && item.longitude_deg.is_finite()
}

/// `MAV_CMD`s whose latitude and longitude are a place (the `NAV_*`
/// commands with a position, the region of interest, the landing sequence
/// start and the fence and rally items).
#[must_use]
pub fn command_has_position(command: u16) -> bool {
    matches!(
        command,
        16..=19 // waypoint, loiter unlimited, turns, time
            | 21 // land
            | 22 // take-off (often without one: then 0°, 0°)
            | 30 // continue and change altitude
            | 31 // loiter to altitude
            | 82 // spline waypoint
            | 84 | 85 // VTOL take-off, VTOL land
            | 94 // payload place
            | 189 // landing sequence start (DO_LAND_START)
            | 195 // region of interest at a location
            | 201 // DO_SET_ROI
            | 5001..=5004 // fence vertices and circles
            | 5100 // rally point
    )
}

/// A `MAV_CMD` by the name the plan editor's list gives it
/// (`apps/web/src/features/plans/commands.ts`); `None` for one it has no
/// name for, such as a dialect's.
#[must_use]
pub fn command_label(command: u16) -> Option<&'static str> {
    Some(match command {
        16 => "Waypoint",
        17 => "Loiter",
        18 => "Loiter turns",
        19 => "Loiter for a time",
        20 => "Return to launch",
        21 => "Land",
        22 => "Take off",
        30 => "Continue and change height",
        31 => "Loiter to height",
        82 => "Spline waypoint",
        83 => "Wait for height",
        84 => "VTOL take off",
        85 => "VTOL land",
        92 => "Guided by companion",
        93 => "Delay",
        94 => "Place payload",
        112 => "Delay the next action",
        114 => "Act near the next waypoint",
        115 => "Turn to heading",
        177 => "Repeat from item",
        178 => "Change speed",
        179 => "Set home",
        181 => "Set relay",
        182 => "Repeat relay",
        183 => "Set servo",
        184 => "Repeat servo",
        188 => "Return path start",
        189 => "Landing sequence start",
        191 => "Go around",
        193 => "Pause or continue",
        194 => "Drive in reverse",
        195 => "Look at location",
        196 => "Look ahead with an offset",
        197 => "Stop looking",
        201 => "Look at",
        202 => "Camera settings",
        203 => "Camera control",
        205 => "Point mount",
        206 => "Camera trigger distance",
        207 => "Fence on or off",
        208 => "Parachute",
        210 => "Inverted flight",
        211 => "Gripper",
        212 => "Autotune",
        213 => "Turn and drive",
        215 => "Resume distance",
        216 => "Sprayer",
        217 => "Script message",
        218 => "Auxiliary function",
        222 => "Guided limits",
        223 => "Engine control",
        531 => "Zoom camera",
        532 => "Focus camera",
        534 => "Camera source",
        600 => "Jump tag",
        601 => "Repeat from tag",
        1000 => "Point gimbal",
        1001 => "Set gimbal control",
        2000 => "Take photos",
        2001 => "Stop photos",
        2500 => "Start video",
        2501 => "Stop video",
        3000 => "VTOL transition",
        42600 => "Winch",
        42702 => "Script control",
        42703 => "Hold an attitude",
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
}

impl Bounds {
    #[must_use]
    pub fn around(points: &[LatLon]) -> Option<Self> {
        let first = points.first()?;
        let mut bounds = Self {
            south_deg: first.latitude_deg,
            west_deg: first.longitude_deg,
            north_deg: first.latitude_deg,
            east_deg: first.longitude_deg,
        };
        for point in &points[1..] {
            bounds.south_deg = bounds.south_deg.min(point.latitude_deg);
            bounds.north_deg = bounds.north_deg.max(point.latitude_deg);
            bounds.west_deg = bounds.west_deg.min(point.longitude_deg);
            bounds.east_deg = bounds.east_deg.max(point.longitude_deg);
        }
        Some(bounds)
    }

    #[must_use]
    pub fn center(&self) -> LatLon {
        LatLon {
            latitude_deg: f64::midpoint(self.south_deg, self.north_deg),
            longitude_deg: f64::midpoint(self.west_deg, self.east_deg),
        }
    }
}

/// An upload of this plan to a vehicle (S127 fills these).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpload {
    pub vehicle_id: String,
    /// The plan's version that was uploaded.
    pub plan_version: u64,
    pub at_us: u64,
    pub verified: bool,
    /// The vehicle's mission version it became: a later one means the
    /// mission changed since.
    #[serde(default)]
    pub mission_version: u64,
}

/// What makes a plan a vehicle copy (ADR-0029 P17): a vehicle's mission
/// being changed before it is written, never listed with the plans.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleCopy {
    pub vehicle_id: String,
    /// The vehicle's mission version the copy was made from.
    pub mission_version: u64,
    /// The plan the mission came from, which Save to plan writes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

/// A saved plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: String,
    #[serde(flatten)]
    pub body: PlanBody,
    /// The nearest place's name, looked up on save (P2, S125).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_name: Option<String>,
    /// Where `place_name` was looked up, so it is redone only when the plan
    /// moves more than 1 km.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_at: Option<LatLon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    pub made_by: PlanAuthor,
    pub created_at_us: u64,
    pub changed_at_us: u64,
    /// Set when deleted; restorable for 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_us: Option<u64>,
    #[serde(default)]
    pub uploads: Vec<PlanUpload>,
    /// How many revisions can be undone and redone from here.
    pub undo: u32,
    pub redo: u32,
    pub version: u64,
    /// Set on a vehicle's copy of its mission; such a plan is not one of
    /// the library's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_copy: Option<VehicleCopy>,
}

/// Plans deleted longer ago than this are gone.
pub const DELETED_KEPT_US: u64 = 30 * 24 * 3600 * 1_000_000;

/// Distance in metres between two points (haversine on a 6371 km sphere).
#[must_use]
pub fn distance_m(a: LatLon, b: LatLon) -> f64 {
    let (la1, la2) = (a.latitude_deg.to_radians(), b.latitude_deg.to_radians());
    let dla = la2 - la1;
    let dlo = (b.longitude_deg - a.longitude_deg).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// How a saved plan's undo history moves with one change (S125).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PlanRevision {
    /// Nothing to undo: made, renamed by a lookup, deleted or restored.
    None,
    /// An edit: the body before it goes onto the undo stack, and what was
    /// undone can no longer be redone.
    Edit { previous: Box<PlanBody> },
    /// Back `steps` revisions.
    Undo { steps: u32 },
    /// Forward one.
    Redo,
}

/// A plan's undo and redo stacks, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRevisions {
    #[serde(default)]
    pub undo: Vec<PlanBody>,
    #[serde(default)]
    pub redo: Vec<PlanBody>,
}

/// Revisions kept per plan; the oldest go first.
pub const MAX_REVISIONS: usize = 200;

/// What a plan's kept revisions hold in memory at most, added up by
/// [`PlanBody::held_bytes`]. Past it too the oldest go first. A revision is
/// a whole copy of the plan and nothing counts a plan's items any more, so
/// 200 of them have no size of their own: for one survey of 57,000 items
/// they would be 1.7 GB, and a plan's notes and fence add to it without a
/// count either. This is what the ring held while a plan stopped at 5,000
/// items: 200 revisions of one are 152 MB counted this way, and the daemon
/// grew by 145 MB keeping them when measured, 0.73 MB each. So a plan of
/// that size keeps its 200 as it did, a larger one keeps fewer, and UXDuck
/// holds no more for a plan than it did before.
pub const MAX_REVISIONS_BYTES: usize = 160_000_000;

impl PlanRevisions {
    /// Moves the stacks for `revision`, where `current` is the body before
    /// the change. Returns the body it lands on for undo and redo.
    pub fn apply(&mut self, current: &PlanBody, revision: &PlanRevision) -> Option<PlanBody> {
        let landed = match revision {
            PlanRevision::None => return None,
            PlanRevision::Edit { previous } => {
                self.undo.push((**previous).clone());
                self.redo.clear();
                None
            }
            PlanRevision::Undo { steps } => {
                let mut body = current.clone();
                for _ in 0..*steps {
                    let Some(previous) = self.undo.pop() else {
                        break;
                    };
                    self.redo.push(std::mem::replace(&mut body, previous));
                }
                Some(body)
            }
            PlanRevision::Redo => {
                let next = self.redo.pop()?;
                self.undo.push(current.clone());
                Some(next)
            }
        };
        self.keep_within(MAX_REVISIONS, MAX_REVISIONS_BYTES);
        landed
    }

    /// How many revisions an edit that replaces `previous` leaves to undo:
    /// what `apply` keeps, counted without making the copy. It is fewer
    /// than the edits made once the oldest have gone.
    #[must_use]
    pub fn undo_after_edit(&self, previous: &PlanBody) -> usize {
        fitting(
            std::iter::once(previous).chain(self.undo.iter().rev()),
            MAX_REVISIONS,
            MAX_REVISIONS_BYTES,
        )
    }

    /// Drops revisions until the two stacks together are within both
    /// bounds, the oldest first. Undo and redo move revisions between the
    /// stacks and put the plan as it was in the place of the one landed
    /// on, so the pair is counted, not each. The oldest are the first of
    /// `undo`, and after them the last of `redo`, the steps nearest the
    /// plan as it is. The newest is always kept, however large: the last
    /// edit to any plan can be undone, and what an undo left can be put
    /// back.
    fn keep_within(&mut self, most: usize, most_bytes: usize) {
        let kept = fitting(
            self.redo.iter().chain(self.undo.iter().rev()),
            most,
            most_bytes,
        );
        let undone = kept.saturating_sub(self.redo.len());
        self.undo.drain(..self.undo.len() - undone);
        self.redo.truncate(kept);
    }
}

/// How many of these revisions, newest first, are within both bounds. The
/// newest always is.
fn fitting<'a>(
    newest_first: impl Iterator<Item = &'a PlanBody>,
    most: usize,
    most_bytes: usize,
) -> usize {
    let mut held = 0_usize;
    let mut kept = 0;
    for body in newest_first.take(most) {
        held = held.saturating_add(body.held_bytes());
        if kept > 0 && held > most_bytes {
            break;
        }
        kept += 1;
    }
    kept
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    clippy::unwrap_used,
    reason = "exact values in tests"
)]
mod tests {
    use super::*;

    fn item(command: u16, lat: f64, lon: f64) -> PlanItem {
        PlanItem {
            command,
            frame: 3,
            params: [None; 4],
            latitude_deg: lat,
            longitude_deg: lon,
            altitude_m: 30.0,
            autocontinue: true,
            pattern: None,
        }
    }

    #[test]
    fn items_without_a_position_are_not_points() {
        let mut body = PlanBody::new(
            "Box".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.items = vec![
            item(22, 0.0, 0.0),
            item(16, -35.0, 149.0),
            item(178, 0.0, 0.0),
            item(16, -35.1, 149.2),
            item(20, 0.0, 0.0),
        ];
        let bounds = body.bounds().unwrap();
        assert_eq!(body.points().len(), 2);
        assert_eq!(bounds.south_deg, -35.1);
        assert_eq!(bounds.east_deg, 149.2);
    }

    #[test]
    fn types_choose_their_frame() {
        assert_eq!(
            PlanVehicleType::Plane.default_frame(),
            PlanFrame::AboveTerrain
        );
        assert_eq!(
            PlanVehicleType::Submarine.default_frame(),
            PlanFrame::AboveHome
        );
        let rover = PlanBody::new(
            "Loop".into(),
            PlanVehicleType::Rover,
            PlanAutopilot::Ardupilot,
        );
        assert_eq!(rover.next_altitude_m, 0.0);
    }

    #[test]
    fn distance_is_about_right() {
        let a = LatLon {
            latitude_deg: 0.0,
            longitude_deg: 0.0,
        };
        let b = LatLon {
            latitude_deg: 0.0,
            longitude_deg: 1.0,
        };
        assert!((distance_m(a, b) - 111_195.0).abs() < 10.0);
    }

    #[test]
    fn undo_and_redo_move_between_revisions() {
        let body = |name: &str| {
            PlanBody::new(
                name.into(),
                PlanVehicleType::Multicopter,
                PlanAutopilot::Ardupilot,
            )
        };
        let mut revisions = PlanRevisions::default();
        revisions.apply(
            &body("a"),
            &PlanRevision::Edit {
                previous: Box::new(body("a")),
            },
        );
        revisions.apply(
            &body("b"),
            &PlanRevision::Edit {
                previous: Box::new(body("b")),
            },
        );
        // Now at "c": two back is "a".
        let back = revisions
            .apply(&body("c"), &PlanRevision::Undo { steps: 2 })
            .unwrap();
        assert_eq!(back.name, "a");
        assert_eq!(revisions.redo.len(), 2);
        let forward = revisions.apply(&back, &PlanRevision::Redo).unwrap();
        assert_eq!(forward.name, "b");
        revisions.apply(
            &forward,
            &PlanRevision::Edit {
                previous: Box::new(forward.clone()),
            },
        );
        assert!(revisions.redo.is_empty());
        assert_eq!(revisions.undo.len(), 2);
    }

    /// A plan of this many waypoints placed by hand, all at one place.
    fn waypoints(count: usize, name: &str) -> PlanBody {
        let mut body = PlanBody::new(
            name.into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.items = vec![item(16, -35.0, 149.0); count];
        body
    }

    /// One edit that replaces `previous`, checked against what it was said to leave.
    fn edit(revisions: &mut PlanRevisions, previous: PlanBody) {
        let said = revisions.undo_after_edit(&previous);
        let current = PlanBody::default();
        revisions.apply(
            &current,
            &PlanRevision::Edit {
                previous: Box::new(previous),
            },
        );
        assert_eq!(revisions.undo.len(), said);
    }

    fn held(revisions: &PlanRevisions) -> usize {
        revisions
            .undo
            .iter()
            .chain(&revisions.redo)
            .map(PlanBody::held_bytes)
            .sum()
    }

    fn names(revisions: &[PlanBody]) -> Vec<&str> {
        revisions.iter().map(|body| body.name.as_str()).collect()
    }

    #[test]
    fn a_large_plans_revisions_stay_within_the_bound_and_its_last_edit_is_undone() {
        // 60,000 items are 9 MB a revision: 200 of them would be 1.8 GB.
        let fit = MAX_REVISIONS_BYTES / waypoints(60_000, "").held_bytes();
        assert!((10..30).contains(&fit), "{fit}");
        let mut revisions = PlanRevisions::default();
        for change in 0..fit + 8 {
            edit(&mut revisions, waypoints(60_000, &change.to_string()));
            assert_eq!(revisions.undo.len(), (change + 1).min(fit));
            assert!(held(&revisions) <= MAX_REVISIONS_BYTES);
        }
        // The oldest went first, and the newest is there to undo.
        assert_eq!(revisions.undo[0].name, "8");
        let now = waypoints(60_000, "now");
        let back = revisions
            .apply(&now, &PlanRevision::Undo { steps: 1 })
            .unwrap();
        assert_eq!(back.name, (fit + 7).to_string());
        assert_eq!(names(&revisions.redo), ["now"]);
        assert_eq!(revisions.undo.len(), fit - 1);
    }

    #[test]
    fn an_undo_of_many_steps_keeps_undo_and_redo_together_within_the_bound() {
        let fit = MAX_REVISIONS_BYTES / waypoints(60_000, "").held_bytes();
        let mut revisions = PlanRevisions::default();
        for change in 0..fit {
            edit(&mut revisions, waypoints(60_000, &change.to_string()));
        }
        // The plan as it is has twice the items. Undone all the way, it
        // joins `redo` in the place of the oldest revision, half its size.
        let now = waypoints(120_000, "now");
        let back = revisions
            .apply(&now, &PlanRevision::Undo { steps: u32::MAX })
            .unwrap();
        assert_eq!(back.name, "0");
        assert!(revisions.undo.is_empty());
        assert!(held(&revisions) <= MAX_REVISIONS_BYTES);
        // The newest is kept. The step nearest the plan went: of what is
        // left to redo, it is the oldest.
        assert_eq!(revisions.redo.len(), fit - 1);
        assert_eq!(revisions.redo[0].name, "now");
        assert_eq!(revisions.redo[fit - 2].name, "2");
        let forward = revisions.apply(&back, &PlanRevision::Redo).unwrap();
        assert_eq!(forward.name, "2");
        assert_eq!(names(&revisions.undo), ["0"]);
        assert!(held(&revisions) <= MAX_REVISIONS_BYTES);
    }

    #[test]
    fn a_small_plan_still_keeps_its_last_200_changes() {
        let mut revisions = PlanRevisions::default();
        for change in 0..MAX_REVISIONS + 50 {
            edit(&mut revisions, waypoints(5, &change.to_string()));
        }
        assert_eq!(revisions.undo.len(), MAX_REVISIONS);
        assert_eq!(revisions.undo[0].name, "50");
        assert_eq!(revisions.undo[MAX_REVISIONS - 1].name, "249");
    }

    #[test]
    fn a_plan_of_5000_items_keeps_the_200_changes_it_kept() {
        // The most items a plan held while they were counted, with a fence
        // and rally points beside them.
        let corner = LatLon {
            latitude_deg: -35.0,
            longitude_deg: 149.0,
        };
        let mut most = waypoints(5_000, "");
        most.fence.polygons = vec![FencePolygon {
            inclusion: true,
            points: vec![corner; 100],
            name: Some("Field".into()),
        }];
        most.rally = vec![
            RallyPoint {
                latitude_deg: -35.0,
                longitude_deg: 149.0,
                altitude_m: 40.0,
                name: Some("North gate".into()),
            };
            5
        ];
        let mut revisions = PlanRevisions::default();
        for change in 0..MAX_REVISIONS + 10 {
            let mut previous = most.clone();
            previous.name = change.to_string();
            edit(&mut revisions, previous);
        }
        assert_eq!(revisions.undo.len(), MAX_REVISIONS);
        assert_eq!(revisions.undo[0].name, "10");
        // The bound is that ring's size and little more, not a number of
        // its own.
        let held = held(&revisions);
        assert!(
            held <= MAX_REVISIONS_BYTES && held > MAX_REVISIONS_BYTES / 10 * 9,
            "{held}"
        );
    }

    #[test]
    fn one_revision_is_kept_however_large_and_either_bound_drops_the_oldest() {
        let five = || PlanRevisions {
            undo: (0..5)
                .map(|change| waypoints(100, &change.to_string()))
                .collect(),
            redo: Vec::new(),
        };
        let one = waypoints(100, "0").held_bytes();
        let mut revisions = five();
        revisions.keep_within(MAX_REVISIONS, one - 1);
        assert_eq!(names(&revisions.undo), ["4"]);
        let mut revisions = five();
        revisions.keep_within(3, usize::MAX);
        assert_eq!(names(&revisions.undo), ["2", "3", "4"]);
        let mut revisions = five();
        revisions.keep_within(MAX_REVISIONS, 2 * one + 1);
        assert_eq!(names(&revisions.undo), ["3", "4"]);
        // With steps to redo, the oldest of all still go first.
        let mut revisions = five();
        revisions.redo = vec![waypoints(100, "6"), waypoints(100, "5")];
        revisions.keep_within(4, usize::MAX);
        assert_eq!(names(&revisions.undo), ["3", "4"]);
        assert_eq!(names(&revisions.redo), ["6", "5"]);
        revisions.keep_within(1, usize::MAX);
        assert!(revisions.undo.is_empty());
        assert_eq!(names(&revisions.redo), ["6"]);
    }

    /// Every allocation of a body, walked through in full.
    fn asked_for(body: &PlanBody) -> usize {
        let named = |name: &Option<String>| name.as_ref().map_or(0, String::capacity);
        let texts = |texts: &Vec<String>| -> usize {
            texts
                .iter()
                .map(|text| size_of::<String>() + text.capacity())
                .sum()
        };
        let items: usize = body
            .items
            .iter()
            .map(|item| size_of::<PlanItem>() + named(&item.pattern))
            .sum();
        let patterns: usize = body
            .patterns
            .iter()
            .map(|pattern| {
                size_of::<PlanPattern>()
                    + pattern.id.capacity()
                    + match &pattern.shape {
                        PatternShape::Survey { area, .. } => area.capacity() * size_of::<LatLon>(),
                        PatternShape::Corridor { line, .. } => {
                            line.capacity() * size_of::<LatLon>()
                        }
                        PatternShape::Orbit { heights_m, .. } => {
                            heights_m.capacity() * size_of::<f64>()
                        }
                        PatternShape::Landing { .. } => 0,
                    }
            })
            .sum();
        let polygons: usize = body
            .fence
            .polygons
            .iter()
            .map(|polygon| {
                size_of::<FencePolygon>()
                    + polygon.points.capacity() * size_of::<LatLon>()
                    + named(&polygon.name)
            })
            .sum();
        let circles: usize = body
            .fence
            .circles
            .iter()
            .map(|circle| size_of::<FenceCircle>() + named(&circle.name))
            .sum();
        let rally: usize = body
            .rally
            .iter()
            .map(|point| size_of::<RallyPoint>() + named(&point.name))
            .sum();
        size_of::<PlanBody>()
            + items
            + patterns
            + polygons
            + circles
            + rally
            + body.name.capacity()
            + body.notes.capacity()
            + texts(&body.labels)
            + texts(&body.dialects)
    }

    /// A survey as it is saved: its items tagged with its id, beside a
    /// fence, rally points, notes and labels.
    fn a_saved_survey() -> PlanBody {
        let corner = LatLon {
            latitude_deg: -35.0,
            longitude_deg: 149.0,
        };
        let mut survey = waypoints(5_000, "Ridge survey");
        for item in &mut survey.items {
            item.pattern = Some("survey-1".into());
        }
        survey.patterns = vec![
            PlanPattern {
                id: "survey-1".into(),
                shape: PatternShape::Survey {
                    area: vec![corner; 40],
                    spacing_m: 12.5,
                    angle_deg: 90.0,
                    turnaround_m: 10.0,
                    altitude_m: 60.0,
                    crosshatch: false,
                    margin_m: 5.0,
                    start: Some(SurveyStart::Home),
                    shortest: true,
                },
            },
            PlanPattern {
                id: "orbit-1".into(),
                shape: PatternShape::Orbit {
                    center: corner,
                    radius_m: 30.0,
                    turns: 2.0,
                    heights_m: vec![20.0, 40.0],
                    counter_clockwise: false,
                },
            },
        ];
        survey.fence.polygons = vec![FencePolygon {
            inclusion: false,
            points: vec![corner; 200],
            name: Some("Pond".into()),
        }];
        survey.fence.circles = vec![FenceCircle {
            inclusion: true,
            center: corner,
            radius_m: 900.0,
            name: None,
        }];
        survey.rally = vec![
            RallyPoint {
                latitude_deg: -35.0,
                longitude_deg: 149.0,
                altitude_m: 40.0,
                name: Some("North gate".into()),
            };
            3
        ];
        survey.notes = "Fly before ten, while the wind is light.".repeat(20);
        survey.labels = vec!["ridge".into(), "weekly".into()];
        survey
    }

    #[test]
    fn what_a_body_is_counted_as_holding_is_near_what_it_asks_its_allocator_for() {
        for body in [a_saved_survey(), waypoints(5_000, "By hand")] {
            // A copy holds what its lengths say and no more, as a kept revision does.
            let kept = body.clone();
            let (counted, asked) = (kept.held_bytes(), asked_for(&kept));
            assert!(
                counted >= asked && counted <= asked + asked / 2,
                "counted {counted} for {asked}"
            );
        }
    }
}

//! Plans (ADR-0029): missions saved in UXDuck for a vehicle type, with a
//! fence and rally points, that reach a vehicle only when uploaded. A plan
//! is never the vehicle's mission; `mission` is what a vehicle holds.

use serde::{Deserialize, Serialize};

/// What a plan is for (P3): the simulator's list without the blimp.
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
}

impl PlanVehicleType {
    /// Whether altitude means anything for it: rovers and boats ignore it.
    #[must_use]
    pub fn flies(self) -> bool {
        matches!(self, Self::Multicopter | Self::Plane | Self::Vtol)
    }

    /// The frame a new plan of this type starts in (P4).
    #[must_use]
    pub fn default_frame(self) -> PlanFrame {
        match self {
            Self::Multicopter | Self::Plane | Self::Vtol => PlanFrame::AboveTerrain,
            // Rovers and boats ignore heights; subs give depth as a
            // negative height above home (normally the surface).
            Self::Rover | Self::Boat | Self::Submarine => PlanFrame::AboveHome,
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
}

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

impl PlanRevisions {
    /// Moves the stacks for `revision`, where `current` is the body before
    /// the change. Returns the body it lands on for undo and redo.
    pub fn apply(&mut self, current: &PlanBody, revision: &PlanRevision) -> Option<PlanBody> {
        match revision {
            PlanRevision::None => None,
            PlanRevision::Edit { previous } => {
                self.undo.push((**previous).clone());
                if self.undo.len() > MAX_REVISIONS {
                    self.undo.remove(0);
                }
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
        }
    }
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
}

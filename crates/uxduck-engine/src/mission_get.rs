//! `mission/get` (FB-256): what UXDuck holds of a vehicle's mission, fence
//! and rally points, where each part stands, and the fence's shapes read
//! from its items. It asks the vehicle nothing.

use uxduck_contracts::dto::mission::{
    FenceShapes, Mission as MissionDto, MissionGetResult, MissionPart, MissionPartStatus,
    OnboardFenceCircle, OnboardFencePolygon, OnboardRallyPoint,
};
use uxduck_contracts::dto::plan::LatLon;
use uxduck_core::domain::mission::{Mission, MissionItem, ReadOutcome};

use crate::Engine;
use crate::replay::REPLAY_LINK;

const FENCE_RETURN_POINT: u16 = 5000;
const FENCE_POLYGON_INCLUSION: u16 = 5001;
const FENCE_POLYGON_EXCLUSION: u16 = 5002;
const FENCE_CIRCLE_INCLUSION: u16 = 5003;
const FENCE_CIRCLE_EXCLUSION: u16 = 5004;
const RALLY_POINT: u16 = 5100;

/// `MAV_FRAME`s whose position is a latitude and longitude.
const GLOBAL_FRAMES: [u8; 6] = [0, 3, 5, 6, 10, 11];

impl Engine {
    pub(crate) fn mission_get(&self, vehicle_id: &str) -> MissionGetResult {
        let mission = self.mission(vehicle_id);
        let reading = self.reading_parts(vehicle_id);
        let uploading = self.uploading_parts(vehicle_id);
        let vehicle = self.vehicle(vehicle_id);
        let replay = vehicle.is_some_and(|vehicle| {
            vehicle.link_id == REPLAY_LINK
                || vehicle
                    .link_id
                    .strip_prefix(REPLAY_LINK)
                    .is_some_and(|source| source.starts_with('-'))
        });
        let from = |part: u8| {
            if replay {
                From::Recording
            } else if part > 0 && vehicle.is_some_and(|vehicle| vehicle.vehicle_type == "Airship") {
                From::Airship
            } else {
                From::Vehicle
            }
        };
        let mut unreadable = Vec::new();
        let fence_shapes = fence_shapes(&mission.fence, &mut unreadable);
        let rally_points = rally_points(&mission.rally, &mut unreadable);
        MissionGetResult {
            mission: MissionDto::from(&mission),
            items: part(&mission, 0, [reading[0], uploading[0]], from(0)),
            fence: part(&mission, 1, [reading[1], uploading[1]], from(1)),
            rally: part(&mission, 2, [reading[2], uploading[2]], from(2)),
            fence_shapes,
            rally_points,
            unreadable,
        }
    }
}

/// Where UXDuck's copy of a part comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum From {
    Vehicle,
    /// A fence or rally points, which UXDuck does not read from an airship.
    Airship,
    /// A replay: what the recording held, never read from a vehicle here.
    Recording,
}

/// Where one part stands. `[reading, sending]` says whether a read or an
/// upload of it is running now.
fn part(mission: &Mission, kind: u8, [reading, sending]: [bool; 2], from: From) -> MissionPart {
    let count = match kind {
        0 => mission.items.len(),
        1 => mission.fence.len(),
        _ => mission.rally.len(),
    };
    let read = mission.part_read(kind);
    // Read and kept by a version that did not record how each part's read
    // ended.
    let kept_from_earlier = mission.synced_at_us.is_some()
        && mission.sent.is_empty()
        && (0..3).all(|kind| mission.part_read(kind).is_none());
    let (status, detail) = if sending {
        (
            MissionPartStatus::Sent,
            Some("UXDuck is sending it now.".to_owned()),
        )
    } else if reading {
        (
            MissionPartStatus::Reading,
            Some("UXDuck is reading it from the vehicle now.".to_owned()),
        )
    } else if mission.sent.contains(&kind) {
        (
            MissionPartStatus::Sent,
            Some("UXDuck sent it and has not read it back since.".to_owned()),
        )
    } else if let Some(read) = read {
        let status = match read.outcome {
            ReadOutcome::Read => MissionPartStatus::Synced,
            ReadOutcome::Failed => MissionPartStatus::Failed,
            ReadOutcome::Unsupported => MissionPartStatus::Unsupported,
        };
        (status, read.detail.clone())
    } else if from == From::Airship {
        (
            MissionPartStatus::Unsupported,
            Some("UXDuck does not read a fence or rally points from an airship.".to_owned()),
        )
    } else if from == From::Recording {
        (
            MissionPartStatus::NeverRead,
            Some("From the recording, not read from a vehicle.".to_owned()),
        )
    } else if kept_from_earlier {
        (
            MissionPartStatus::NeverRead,
            Some("Kept from an earlier version. Read it again to know.".to_owned()),
        )
    } else {
        (MissionPartStatus::NeverRead, None)
    };
    MissionPart {
        status,
        count: number(count),
        complete: status == MissionPartStatus::Synced,
        read_at_us: read.map(|read| read.at_us),
        detail,
    }
}

fn number(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// The item's latitude and longitude, if it has a usable one.
fn position(item: &MissionItem) -> Option<LatLon> {
    (GLOBAL_FRAMES.contains(&item.frame)
        && item.latitude_deg.is_finite()
        && item.longitude_deg.is_finite()
        && item.latitude_deg.abs() <= 90.0
        && item.longitude_deg.abs() <= 180.0)
        .then_some(LatLon {
            latitude_deg: item.latitude_deg,
            longitude_deg: item.longitude_deg,
        })
}

/// A polygon is `param1` corners of the same kind in a row, and a circle
/// carries its radius in `param1`, as `MAV_CMD_NAV_FENCE_*` says.
fn fence_shapes(items: &[MissionItem], unreadable: &mut Vec<String>) -> FenceShapes {
    let mut shapes = FenceShapes {
        polygons: Vec::new(),
        circles: Vec::new(),
        return_point: None,
    };
    let mut return_item = None;
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        match item.command {
            FENCE_RETURN_POINT => {
                match (position(item), return_item) {
                    (None, _) => unreadable.push(format!(
                        "Fence item {i} is a return point with no usable position."
                    )),
                    (Some(_), Some(first)) => unreadable.push(format!(
                        "Fence item {i} is another return point. Item {first} is the one given."
                    )),
                    (Some(at), None) => {
                        shapes.return_point = Some(at);
                        return_item = Some(i);
                    }
                }
                i += 1;
            }
            FENCE_CIRCLE_INCLUSION | FENCE_CIRCLE_EXCLUSION => {
                let radius_m = item.param1;
                match position(item) {
                    Some(center) if radius_m.is_finite() && radius_m > 0.0 => {
                        shapes.circles.push(OnboardFenceCircle {
                            inclusion: item.command == FENCE_CIRCLE_INCLUSION,
                            item: number(i),
                            center,
                            radius_m,
                        });
                    }
                    Some(_) => unreadable
                        .push(format!("Fence item {i} is a circle with no usable radius.")),
                    None => unreadable
                        .push(format!("Fence item {i} is a circle with no usable centre.")),
                }
                i += 1;
            }
            FENCE_POLYGON_INCLUSION | FENCE_POLYGON_EXCLUSION => {
                i += polygon(items, i, &mut shapes, unreadable);
            }
            command => {
                unreadable.push(format!(
                    "Fence item {i} is command {command}, which is not a fence shape."
                ));
                i += 1;
            }
        }
    }
    shapes
}

/// The polygon whose first corner is item `i`, or why its corners make none:
/// how many items it took.
fn polygon(
    items: &[MissionItem],
    i: usize,
    shapes: &mut FenceShapes,
    unreadable: &mut Vec<String>,
) -> usize {
    let item = &items[i];
    let run = items[i..]
        .iter()
        .take_while(|corner| corner.command == item.command)
        .count();
    let last = i + run - 1;
    let count = item.param1;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked to be a whole number from 3 to the corners in a row"
    )]
    let corners = (count.is_finite() && count.fract() == 0.0 && count >= 3.0)
        .then_some(count as usize)
        .filter(|corners| *corners <= run);
    let Some(corners) = corners else {
        unreadable.push(format!(
            "Fence items {i} to {last} are polygon corners that say there are {count} of them, with {run} in a row."
        ));
        return run;
    };
    #[allow(
        clippy::float_cmp,
        reason = "every corner carries the same count, exactly"
    )]
    let agree = items[i..i + corners]
        .iter()
        .all(|corner| corner.param1 == count);
    if !agree {
        unreadable.push(format!(
            "Fence items {i} to {last} are polygon corners that do not agree how many there are."
        ));
        return run;
    }
    let points: Option<Vec<LatLon>> = items[i..i + corners].iter().map(position).collect();
    if let Some(points) = points {
        shapes.polygons.push(OnboardFencePolygon {
            inclusion: item.command == FENCE_POLYGON_INCLUSION,
            first_item: number(i),
            points,
        });
    } else {
        let last = i + corners - 1;
        unreadable.push(format!(
            "Fence items {i} to {last} are a polygon with a corner that has no usable position."
        ));
    }
    corners
}

fn rally_points(items: &[MissionItem], unreadable: &mut Vec<String>) -> Vec<OnboardRallyPoint> {
    let mut points = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if item.command != RALLY_POINT {
            unreadable.push(format!(
                "Rally item {i} is command {}, which is not a rally point.",
                item.command
            ));
            continue;
        }
        match position(item) {
            Some(at) if item.altitude_m.is_finite() => points.push(OnboardRallyPoint {
                item: number(i),
                frame: item.frame,
                latitude_deg: at.latitude_deg,
                longitude_deg: at.longitude_deg,
                altitude_m: item.altitude_m,
            }),
            Some(_) => unreadable.push(format!("Rally item {i} has no usable height.")),
            None => unreadable.push(format!("Rally item {i} has no usable position.")),
        }
    }
    points
}

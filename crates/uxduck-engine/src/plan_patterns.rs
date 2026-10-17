//! Patterns (ADR-0029 P11): the items a survey, corridor, orbit or landing
//! makes. A plan keeps each pattern as its shape and settings; its items
//! sit among the plan's items, tagged with its id, and are made again from
//! the pattern whenever the plan is saved ([`expand`]), in the place of the
//! first of them. So the upload, the profile and the checks see what is
//! flown, and editing the pattern moves its items with it.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "lane and point counts are small (at most `MAX_ITEMS`), and widths over spacings are positive"
)]

use std::collections::HashSet;

use uxduck_core::domain::plan::{
    LatLon, PatternShape, PlanBody, PlanItem, PlanPattern, PlanVehicleType,
};

/// The mean Earth radius, as `distance_m` uses.
const EARTH_M: f64 = 6_371_008.8;

/// No pattern makes more items than this: a survey 1 m apart over a large
/// field would otherwise make tens of thousands, more than an autopilot holds.
pub const MAX_ITEMS: usize = 2000;

/// The narrowest spacing between lanes, so a typo of 0 makes no flood of lanes.
const MIN_SPACING_M: f64 = 1.0;

const WAYPOINT: u16 = 16;
const LOITER_TURNS: u16 = 18;
const LAND: u16 = 21;
const VTOL_LAND: u16 = 85;
const DO_LAND_START: u16 = 189;

/// A flat plane around a place, in metres east and north: good to well
/// under a metre across a few kilometres.
struct Local {
    lat0: f64,
    lon0: f64,
    cos0: f64,
}

impl Local {
    fn around(points: &[LatLon]) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        let n = points.len() as f64;
        let lat0 = points.iter().map(|p| p.latitude_deg).sum::<f64>() / n;
        let lon0 = points.iter().map(|p| p.longitude_deg).sum::<f64>() / n;
        Some(Self {
            lat0,
            lon0,
            cos0: lat0.to_radians().cos().max(1e-6),
        })
    }

    fn to(&self, p: LatLon) -> (f64, f64) {
        (
            (p.longitude_deg - self.lon0).to_radians() * EARTH_M * self.cos0,
            (p.latitude_deg - self.lat0).to_radians() * EARTH_M,
        )
    }

    fn back(&self, (east, north): (f64, f64)) -> LatLon {
        LatLon {
            latitude_deg: self.lat0 + (north / EARTH_M).to_degrees(),
            longitude_deg: self.lon0 + (east / (EARTH_M * self.cos0)).to_degrees(),
        }
    }
}

/// A lane: where it starts and ends, in local metres.
type Lane = ((f64, f64), (f64, f64));

/// Lanes across a polygon at `angle_deg` (a bearing), `spacing_m` apart,
/// each lengthened by `turnaround_m` at both ends, flown back and forth.
/// A concave area gives a scan line several pieces; each piece is a lane,
/// flown in order along the line.
fn survey_lanes(
    area: &[(f64, f64)],
    spacing_m: f64,
    angle_deg: f64,
    turnaround_m: f64,
) -> Vec<Lane> {
    if area.len() < 3 {
        return Vec::new();
    }
    let spacing = spacing_m.max(MIN_SPACING_M);
    let bearing = angle_deg.to_radians();
    // Along the lanes, and across them.
    let along = (bearing.sin(), bearing.cos());
    let across = (bearing.cos(), -bearing.sin());
    let dot = |p: (f64, f64), d: (f64, f64)| p.0 * d.0 + p.1 * d.1;
    let uv: Vec<(f64, f64)> = area
        .iter()
        .map(|&p| (dot(p, along), dot(p, across)))
        .collect();
    let v_min = uv.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let v_max = uv.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let width = v_max - v_min;
    if !width.is_finite() {
        return Vec::new();
    }
    // As many lanes as cover the width with a lane's width each (the
    // spacing), centred in the area: at least one.
    let count = ((width / spacing).ceil() as usize).max(1);
    let first = v_min + (width - (count - 1) as f64 * spacing) / 2.0;
    let mut lanes = Vec::new();
    for k in 0..count {
        let v = first + k as f64 * spacing;
        let mut crossings = Vec::new();
        for i in 0..uv.len() {
            let a = uv[i];
            let b = uv[(i + 1) % uv.len()];
            // Half-open, so a corner exactly on the line counts once.
            if (a.1 <= v && v < b.1) || (b.1 <= v && v < a.1) {
                crossings.push(a.0 + (v - a.1) / (b.1 - a.1) * (b.0 - a.0));
            }
        }
        crossings.sort_by(f64::total_cmp);
        let mut pieces: Vec<(f64, f64)> = crossings
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[enter, leave]| (enter - turnaround_m, leave + turnaround_m))
            .collect();
        // Back and forth: every other line is flown the other way.
        let backwards = k % 2 == 1;
        if backwards {
            pieces.reverse();
        }
        for (u0, u1) in pieces {
            let (from, to) = if backwards { (u1, u0) } else { (u0, u1) };
            let point = |u: f64| (u * along.0 + v * across.0, u * along.1 + v * across.1);
            lanes.push((point(from), point(to)));
        }
        if lanes.len() * 2 > MAX_ITEMS {
            break;
        }
    }
    lanes
}

/// The line moved sideways by `offset` metres (right of its direction),
/// keeping each corner's lines parallel.
fn offset_line(line: &[(f64, f64)], offset: f64) -> Vec<(f64, f64)> {
    let normal = |a: (f64, f64), b: (f64, f64)| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length = dx.hypot(dy);
        if length < 1e-9 {
            (0.0, 0.0)
        } else {
            (dy / length, -dx / length)
        }
    };
    (0..line.len())
        .map(|i| {
            let before = (i > 0).then(|| normal(line[i - 1], line[i]));
            let after = (i + 1 < line.len()).then(|| normal(line[i], line[i + 1]));
            let n = match (before, after) {
                (Some(a), Some(b)) => {
                    let sum = (a.0 + b.0, a.1 + b.1);
                    let length = sum.0.hypot(sum.1);
                    if length < 1e-9 {
                        a
                    } else {
                        let m = (sum.0 / length, sum.1 / length);
                        // The corner moves further so both sides stay
                        // `offset` away; never more than four times.
                        let cos = (m.0 * a.0 + m.1 * a.1).max(0.25);
                        (m.0 / cos, m.1 / cos)
                    }
                }
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => (0.0, 0.0),
            };
            (line[i].0 + n.0 * offset, line[i].1 + n.1 * offset)
        })
        .collect()
}

/// Lanes along a line: as many as cover `width_m` at `spacing_m`, centred
/// on it, flown back and forth, each lengthened at its ends.
fn corridor_lanes(
    line: &[(f64, f64)],
    width_m: f64,
    spacing_m: f64,
    turnaround_m: f64,
) -> Vec<Vec<(f64, f64)>> {
    if line.len() < 2 {
        return Vec::new();
    }
    let spacing = spacing_m.max(MIN_SPACING_M);
    let count = ((width_m.max(0.0) / spacing).ceil() as usize).max(1);
    let mut lanes = Vec::new();
    for k in 0..count {
        let sideways = (k as f64 - (count - 1) as f64 / 2.0) * spacing;
        let mut path = offset_line(line, sideways);
        extend(&mut path, turnaround_m);
        if k % 2 == 1 {
            path.reverse();
        }
        lanes.push(path);
        if lanes.iter().map(Vec::len).sum::<usize>() > MAX_ITEMS {
            break;
        }
    }
    lanes
}

/// Lengthens a line at both ends along its first and last legs.
fn extend(line: &mut [(f64, f64)], by: f64) {
    if line.len() < 2 || by == 0.0 {
        return;
    }
    let push = |end: (f64, f64), next: (f64, f64)| {
        let (dx, dy) = (end.0 - next.0, end.1 - next.1);
        let length = dx.hypot(dy);
        if length < 1e-9 {
            end
        } else {
            (end.0 + dx / length * by, end.1 + dy / length * by)
        }
    };
    let last = line.len() - 1;
    line[0] = push(line[0], line[1]);
    line[last] = push(line[last], line[last - 1]);
}

fn item(command: u16, frame: u8, at: LatLon, altitude_m: f64, id: &str) -> PlanItem {
    PlanItem {
        command,
        frame,
        params: [Some(0.0), Some(0.0), Some(0.0), None],
        latitude_deg: at.latitude_deg,
        longitude_deg: at.longitude_deg,
        altitude_m,
        autocontinue: true,
        pattern: Some(id.to_owned()),
    }
}

/// The items one pattern makes, in the plan's frame.
#[must_use]
pub fn items(pattern: &PlanPattern, vehicle_type: PlanVehicleType, frame: u8) -> Vec<PlanItem> {
    let id = pattern.id.as_str();
    let mut out = Vec::new();
    match &pattern.shape {
        PatternShape::Survey {
            area,
            spacing_m,
            angle_deg,
            turnaround_m,
            altitude_m,
            crosshatch,
        } => {
            let Some(local) = Local::around(area) else {
                return out;
            };
            let flat: Vec<(f64, f64)> = area.iter().map(|&p| local.to(p)).collect();
            let mut lanes = survey_lanes(&flat, *spacing_m, *angle_deg, *turnaround_m);
            if *crosshatch {
                lanes.extend(survey_lanes(
                    &flat,
                    *spacing_m,
                    angle_deg + 90.0,
                    *turnaround_m,
                ));
            }
            for (from, to) in lanes {
                out.push(item(WAYPOINT, frame, local.back(from), *altitude_m, id));
                out.push(item(WAYPOINT, frame, local.back(to), *altitude_m, id));
            }
        }
        PatternShape::Corridor {
            line,
            width_m,
            spacing_m,
            turnaround_m,
            altitude_m,
        } => {
            let Some(local) = Local::around(line) else {
                return out;
            };
            let flat: Vec<(f64, f64)> = line.iter().map(|&p| local.to(p)).collect();
            for lane in corridor_lanes(&flat, *width_m, *spacing_m, *turnaround_m) {
                for point in lane {
                    out.push(item(WAYPOINT, frame, local.back(point), *altitude_m, id));
                }
            }
        }
        PatternShape::Orbit {
            center,
            radius_m,
            turns,
            heights_m,
            counter_clockwise,
        } => {
            // ArduPilot: a negative radius circles counter-clockwise.
            let radius = if *counter_clockwise {
                -radius_m.abs()
            } else {
                radius_m.abs()
            };
            for height in heights_m {
                let mut loiter = item(LOITER_TURNS, frame, *center, *height, id);
                loiter.params = [Some(turns.max(0.0)), Some(0.0), Some(radius), Some(0.0)];
                out.push(loiter);
            }
        }
        PatternShape::Landing {
            touchdown,
            approach_from,
            approach_altitude_m,
        } => {
            out.push(item(DO_LAND_START, frame, *approach_from, 0.0, id));
            out.push(item(
                WAYPOINT,
                frame,
                *approach_from,
                *approach_altitude_m,
                id,
            ));
            let land = if vehicle_type == PlanVehicleType::Vtol {
                VTOL_LAND
            } else {
                LAND
            };
            let mut landing = item(land, frame, *touchdown, 0.0, id);
            // A plane's land: no abort altitude, no precision landing.
            landing.params = [Some(0.0), Some(0.0), Some(0.0), None];
            out.push(landing);
        }
    }
    out.truncate(MAX_ITEMS);
    out
}

/// Makes each pattern's items again, in the place of the first item it
/// made before (at the end for a new pattern); drops items of patterns no
/// longer in the plan.
pub fn expand(body: &mut PlanBody) {
    if body.patterns.is_empty() && body.items.iter().all(|i| i.pattern.is_none()) {
        return;
    }
    let frame = body.frame.mav_frame();
    let mut placed = HashSet::new();
    let mut out = Vec::with_capacity(body.items.len());
    for old in std::mem::take(&mut body.items) {
        let Some(id) = old.pattern.as_deref() else {
            out.push(old);
            continue;
        };
        if placed.contains(id) {
            continue;
        }
        if let Some(pattern) = body.patterns.iter().find(|p| p.id == id) {
            placed.insert(id.to_owned());
            out.extend(items(pattern, body.vehicle_type, frame));
        }
    }
    for pattern in &body.patterns {
        if !placed.contains(&pattern.id) {
            out.extend(items(pattern, body.vehicle_type, frame));
        }
    }
    body.items = out;
}

/// A new pattern's id, unused in the plan: `survey-1`, `orbit-2`.
#[must_use]
pub fn new_id(body: &PlanBody, kind: &str) -> String {
    // One of the first n + 1 numbers is free among n patterns.
    (1..=body.patterns.len() + 1)
        .map(|n| format!("{kind}-{n}"))
        .find(|id| body.patterns.iter().all(|p| &p.id != id))
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "heights and parameters are set exactly")]
mod tests {
    use uxduck_core::domain::plan::{PlanAutopilot, PlanFrame, distance_m};

    use super::*;

    fn at(lat: f64, lon: f64) -> LatLon {
        LatLon {
            latitude_deg: lat,
            longitude_deg: lon,
        }
    }

    /// A square about 200 m on a side.
    fn field() -> Vec<LatLon> {
        vec![
            at(-35.0, 149.0),
            at(-35.0, 149.0022),
            at(-35.0018, 149.0022),
            at(-35.0018, 149.0),
        ]
    }

    fn survey(area: Vec<LatLon>, spacing_m: f64, angle_deg: f64, turnaround_m: f64) -> PlanPattern {
        PlanPattern {
            id: "survey-1".into(),
            shape: PatternShape::Survey {
                area,
                spacing_m,
                angle_deg,
                turnaround_m,
                altitude_m: 50.0,
                crosshatch: false,
            },
        }
    }

    #[test]
    fn a_survey_covers_a_square_back_and_forth() {
        let made = items(
            &survey(field(), 20.0, 0.0, 0.0),
            PlanVehicleType::Multicopter,
            10,
        );
        // 200.6 m wide at 20 m: 11 lanes, two points each.
        assert_eq!(made.len(), 22, "{made:#?}");
        assert!(
            made.iter()
                .all(|i| i.command == 16 && i.frame == 10 && i.altitude_m == 50.0)
        );
        assert!(
            made.iter()
                .all(|i| i.pattern.as_deref() == Some("survey-1"))
        );
        // North-south lanes: each lane's two ends share a longitude, and the
        // next lane starts where the last ended (back and forth).
        let lat = |i: usize| made[i].latitude_deg;
        assert!((made[0].longitude_deg - made[1].longitude_deg).abs() < 1e-9);
        assert!(lat(0) < lat(1));
        assert!(lat(2) > lat(3));
        // Lanes about 20 m apart: the next starts beside where the last ended.
        let gap = distance_m(
            at(made[1].latitude_deg, made[1].longitude_deg),
            at(made[2].latitude_deg, made[2].longitude_deg),
        );
        assert!((gap - 20.0).abs() < 1.0, "{gap}");
    }

    #[test]
    fn turning_around_flies_past_the_edge() {
        let plain = items(
            &survey(field(), 20.0, 0.0, 0.0),
            PlanVehicleType::Multicopter,
            10,
        );
        let past = items(
            &survey(field(), 20.0, 0.0, 15.0),
            PlanVehicleType::Multicopter,
            10,
        );
        let length = |made: &[PlanItem]| {
            distance_m(
                at(made[0].latitude_deg, made[0].longitude_deg),
                at(made[1].latitude_deg, made[1].longitude_deg),
            )
        };
        assert!((length(&past) - length(&plain) - 30.0).abs() < 0.5);
    }

    #[test]
    fn a_concave_area_gives_lanes_only_over_it() {
        // A U: the middle of the top is cut out, so lines across the top
        // cross it twice.
        let u = vec![
            at(-35.0, 149.0),
            at(-35.0, 149.0008),
            at(-35.0012, 149.0008),
            at(-35.0012, 149.0014),
            at(-35.0, 149.0014),
            at(-35.0, 149.0022),
            at(-35.0018, 149.0022),
            at(-35.0018, 149.0),
        ];
        let made = items(
            &survey(u, 20.0, 90.0, 0.0),
            PlanVehicleType::Multicopter,
            10,
        );
        // East-west lanes; those through the cut-out are two pieces.
        let lanes = made.len() / 2;
        assert!(lanes > 10, "{lanes}");
        for lane in made.chunks(2) {
            let middle_lon = f64::midpoint(lane[0].longitude_deg, lane[1].longitude_deg);
            let middle_lat = lane[0].latitude_deg;
            // No lane's middle sits in the cut-out.
            assert!(
                !(middle_lat > -35.0012 && middle_lon > 149.0008 && middle_lon < 149.0014),
                "{lane:?}"
            );
        }
    }

    #[test]
    fn a_survey_never_makes_more_than_the_limit() {
        let made = items(
            &survey(field(), 0.0, 0.0, 0.0),
            PlanVehicleType::Multicopter,
            10,
        );
        assert!(made.len() <= MAX_ITEMS);
        assert!(!made.is_empty());
    }

    #[test]
    fn a_corridor_follows_its_line_on_each_side() {
        let pattern = PlanPattern {
            id: "corridor-1".into(),
            shape: PatternShape::Corridor {
                line: vec![at(-35.0, 149.0), at(-35.0, 149.002), at(-35.001, 149.003)],
                width_m: 40.0,
                spacing_m: 20.0,
                turnaround_m: 0.0,
                altitude_m: 30.0,
            },
        };
        let made = items(&pattern, PlanVehicleType::Multicopter, 3);
        // Two lanes of three points: 10 m either side of the line.
        assert_eq!(made.len(), 6);
        let first = at(made[0].latitude_deg, made[0].longitude_deg);
        assert!((distance_m(first, at(-35.0, 149.0)) - 10.0).abs() < 0.5);
        // The second lane is flown back.
        assert!(made[3].longitude_deg > made[5].longitude_deg);
    }

    #[test]
    fn an_orbit_circles_at_each_height_and_a_landing_starts_its_sequence() {
        let orbit = PlanPattern {
            id: "orbit-1".into(),
            shape: PatternShape::Orbit {
                center: at(-35.0, 149.0),
                radius_m: 30.0,
                turns: 2.0,
                heights_m: vec![20.0, 40.0],
                counter_clockwise: true,
            },
        };
        let made = items(&orbit, PlanVehicleType::Multicopter, 3);
        assert_eq!(made.len(), 2);
        assert_eq!(made[1].command, 18);
        assert_eq!(made[1].params[0], Some(2.0));
        assert_eq!(made[1].params[2], Some(-30.0));
        assert_eq!(made[1].altitude_m, 40.0);

        let landing = PlanPattern {
            id: "landing-1".into(),
            shape: PatternShape::Landing {
                touchdown: at(-35.0, 149.0),
                approach_from: at(-35.0, 149.006),
                approach_altitude_m: 60.0,
            },
        };
        let made = items(&landing, PlanVehicleType::Plane, 10);
        let commands: Vec<u16> = made.iter().map(|i| i.command).collect();
        assert_eq!(commands, [189, 16, 21]);
        assert_eq!(made[1].altitude_m, 60.0);
        let made = items(&landing, PlanVehicleType::Vtol, 10);
        assert_eq!(made[2].command, 85);
    }

    #[test]
    fn saving_makes_a_pattern_s_items_again_where_they_were() {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.frame = PlanFrame::AboveHome;
        let take_off = PlanItem {
            command: 22,
            frame: 3,
            params: [None; 4],
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            altitude_m: 30.0,
            autocontinue: true,
            pattern: None,
        };
        let mut rtl = take_off.clone();
        rtl.command = 20;
        // A new pattern's place: one item tagged with it, where it goes.
        let mut place = take_off.clone();
        place.command = 16;
        place.pattern = Some("survey-1".into());
        body.items = vec![take_off, place, rtl];
        body.patterns = vec![survey(field(), 20.0, 0.0, 0.0)];
        expand(&mut body);
        assert_eq!(body.items.len(), 24);
        assert_eq!(body.items[0].command, 22);
        assert_eq!(body.items[23].command, 20);
        assert!(body.items[1..23].iter().all(|i| i.frame == 3));

        // Wider lanes: fewer items, still between take-off and return.
        if let PatternShape::Survey { spacing_m, .. } = &mut body.patterns[0].shape {
            *spacing_m = 40.0;
        }
        expand(&mut body);
        assert_eq!(body.items.len(), 14);
        assert_eq!(body.items[13].command, 20);

        // The pattern removed: its items go.
        body.patterns.clear();
        expand(&mut body);
        assert_eq!(body.items.len(), 2);
    }
}

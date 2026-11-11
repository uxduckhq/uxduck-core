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
    LatLon, PatternShape, PlanBody, PlanItem, PlanPattern, PlanVehicleType, SurveyStart,
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

/// A point in local metres, east and north; or, across a survey, along
/// its lanes and across them.
type Point = (f64, f64);

/// The shortest piece of a lane kept: a sliver past an obstacle is not flown.
const MIN_PIECE_M: f64 = 0.1;

/// How much nearer than its margin a leg may pass an obstacle, so a lane
/// that ends exactly at the margin is not counted as too near.
const SLACK_M: f64 = 0.01;

/// No lane end or leg comes nearer an exclusion zone than this, whatever
/// the margin, so none lies on its edge, where the fence counts it inside.
const MIN_CLEAR_M: f64 = 1.0;

/// How much further out than the margin the way round an obstacle goes.
const ROUND_BY_M: f64 = 0.5;

/// The widest arc step on the way round an obstacle's corner.
const ARC_STEP_DEG: f64 = 30.0;

/// An exclusion zone of the plan's fence, which a survey keeps out of.
enum Obstacle {
    Polygon(Vec<Point>),
    Circle(Point, f64),
}

/// An obstacle and the points of a way round it, out of its margin.
struct Keep {
    shape: Obstacle,
    around: Vec<Point>,
}

fn dot(p: Point, d: Point) -> f64 {
    p.0 * d.0 + p.1 * d.1
}

fn sub(a: Point, b: Point) -> Point {
    (a.0 - b.0, a.1 - b.1)
}

fn gap(a: Point, b: Point) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| gap(w[0], w[1])).sum()
}

/// The pieces of a line across (at v) inside a polygon, along it.
fn inside_on_line(polygon: &[Point], v: f64) -> Vec<(f64, f64)> {
    let mut crossings = Vec::new();
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        // Half-open, so a corner exactly on the line counts once.
        if (a.1 <= v && v < b.1) || (b.1 <= v && v < a.1) {
            crossings.push(a.0 + (v - a.1) / (b.1 - a.1) * (b.0 - a.0));
        }
    }
    crossings.sort_by(f64::total_cmp);
    crossings
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[enter, leave]| (*enter, *leave))
        .collect()
}

/// Where a line across (at v) comes within r of a segment: one piece, as
/// the shape within r of a segment is convex.
fn near_segment_on_line(from: Point, to: Point, reach: f64, v: f64) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for end in [from, to] {
        let dv = v - end.1;
        if dv.abs() <= reach {
            let half = (reach * reach - dv * dv).sqrt();
            lo = lo.min(end.0 - half);
            hi = hi.max(end.0 + half);
        }
    }
    let (dx, dy) = sub(to, from);
    let span = dx.hypot(dy);
    if span > 1e-9 {
        let side = (-dy / span * reach, dx / span * reach);
        let band = [
            (from.0 + side.0, from.1 + side.1),
            (to.0 + side.0, to.1 + side.1),
            (to.0 - side.0, to.1 - side.1),
            (from.0 - side.0, from.1 - side.1),
        ];
        for i in 0..4 {
            let corner = band[i];
            let next = band[(i + 1) % 4];
            if (corner.1 <= v && v <= next.1) || (next.1 <= v && v <= corner.1) {
                let (u0, u1) = if (next.1 - corner.1).abs() < 1e-12 {
                    (corner.0, next.0)
                } else {
                    let u = corner.0 + (v - corner.1) / (next.1 - corner.1) * (next.0 - corner.0);
                    (u, u)
                };
                lo = lo.min(u0.min(u1));
                hi = hi.max(u0.max(u1));
            }
        }
    }
    (lo <= hi).then_some((lo, hi))
}

/// A piece of a line: from and to along it, and whether each end is at
/// the area's edge, where a lane may be lengthened to turn.
type Piece = (f64, f64, bool, bool);

/// The pieces with the cuts taken out; the ends a cut makes are at the
/// edge when edge is true.
fn cut(pieces: Vec<Piece>, mut cuts: Vec<(f64, f64)>, edge: bool) -> Vec<Piece> {
    if cuts.is_empty() {
        return pieces;
    }
    cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    for (mut from, to, mut from_edge, to_edge) in pieces {
        for &(c0, c1) in &cuts {
            if c1 <= from || c0 >= to {
                continue;
            }
            if c0 > from {
                out.push((from, c0, from_edge, edge));
            }
            from = c1;
            from_edge = edge;
        }
        if from < to {
            out.push((from, to, from_edge, to_edge));
        }
    }
    out.retain(|p| p.1 - p.0 > MIN_PIECE_M);
    out
}

/// A survey's scan lines at one angle, before they are put in order.
struct Lines {
    along: Point,
    across: Point,
    /// Each line's place across, and its pieces along it.
    scan: Vec<(f64, Vec<Piece>)>,
}

impl Lines {
    /// Lines across a polygon at a bearing, `spacing_m` apart and centred
    /// in it, `margin_m` in from its edge, cut where they come within
    /// `margin_m` (at least `MIN_CLEAR_M`) of an obstacle. A concave area or an obstacle gives a line several
    /// pieces.
    fn new(
        area: &[Point],
        spacing_m: f64,
        angle_deg: f64,
        margin_m: f64,
        keep: &[Keep],
        snap: bool,
    ) -> Self {
        let spacing = spacing_m.max(MIN_SPACING_M);
        let margin = margin_m.max(0.0);
        let bearing = angle_deg.to_radians();
        let along = (bearing.sin(), bearing.cos());
        let across = (bearing.cos(), -bearing.sin());
        let mut scan = Vec::new();
        let uv = |p: &Point| (dot(*p, along), dot(*p, across));
        let field: Vec<Point> = area.iter().map(uv).collect();
        let v_min = field.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let v_max = field.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let width = v_max - v_min - 2.0 * margin;
        if area.len() < 3 || !width.is_finite() {
            return Self {
                along,
                across,
                scan,
            };
        }
        let obstacles: Vec<Obstacle> = keep
            .iter()
            .map(|k| match &k.shape {
                Obstacle::Polygon(points) => Obstacle::Polygon(points.iter().map(uv).collect()),
                Obstacle::Circle(centre, radius) => Obstacle::Circle(uv(centre), *radius),
            })
            .collect();
        let width = width.max(0.0);
        // As many lanes as cover the width with a lane's width each (the
        // spacing), centred: at least one. Only as many are made as can be
        // flown, from the first. With `snap`, a width a whole number of
        // spacings, give or take rounding, is that many lanes.
        let lanes = width / spacing - if snap { 1e-6 } else { 0.0 };
        let count = (lanes.ceil() as usize).max(1);
        let first = v_min + margin + (width - (count - 1) as f64 * spacing) / 2.0;
        let count = count.min(MAX_ITEMS / 2 + 1);
        let clear = margin.max(MIN_CLEAR_M);
        let near_edges = |polygon: &[Point], v: f64, reach: f64| -> Vec<(f64, f64)> {
            if reach <= 0.0 {
                return Vec::new();
            }
            (0..polygon.len())
                .filter_map(|i| {
                    near_segment_on_line(polygon[i], polygon[(i + 1) % polygon.len()], reach, v)
                })
                .collect()
        };
        for k in 0..count {
            let v = first + k as f64 * spacing;
            let pieces = inside_on_line(&field, v)
                .into_iter()
                .map(|(a, b)| (a, b, true, true))
                .collect();
            let pieces = cut(pieces, near_edges(&field, v, margin), true);
            let mut cuts = Vec::new();
            for obstacle in &obstacles {
                match obstacle {
                    Obstacle::Polygon(points) => {
                        cuts.extend(inside_on_line(points, v));
                        cuts.extend(near_edges(points, v, clear));
                    }
                    Obstacle::Circle(centre, radius) => {
                        let r = radius + clear;
                        let dv = v - centre.1;
                        if dv.abs() < r {
                            let half = (r * r - dv * dv).sqrt();
                            cuts.push((centre.0 - half, centre.0 + half));
                        }
                    }
                }
            }
            scan.push((v, cut(pieces, cuts, false)));
        }
        Self {
            along,
            across,
            scan,
        }
    }

    /// The lanes' ends in the order flown, back and forth: from the last
    /// line when `reverse`, the first line flown backwards when
    /// `first_back`. Ends at the area's edge go `turnaround_m` past it.
    fn route(&self, turnaround_m: f64, reverse: bool, first_back: bool) -> Vec<Point> {
        let point = |u: f64, v: f64| {
            (
                u * self.along.0 + v * self.across.0,
                u * self.along.1 + v * self.across.1,
            )
        };
        let order: Vec<&(f64, Vec<Piece>)> = if reverse {
            self.scan.iter().rev().collect()
        } else {
            self.scan.iter().collect()
        };
        let mut out = Vec::new();
        for (j, (v, pieces)) in order.into_iter().enumerate() {
            // Back and forth: every other line is flown the other way.
            let backwards = (j % 2 == 1) != first_back;
            let mut pieces = pieces.clone();
            if backwards {
                pieces.reverse();
            }
            for (u0, u1, edge0, edge1) in pieces {
                let u0 = if edge0 { u0 - turnaround_m } else { u0 };
                let u1 = if edge1 { u1 + turnaround_m } else { u1 };
                let (from, to) = if backwards { (u1, u0) } else { (u0, u1) };
                out.push(point(from, *v));
                out.push(point(to, *v));
            }
            if out.len() > MAX_ITEMS {
                break;
            }
        }
        out
    }

    /// Of the four ways to fly the lines, the one whose first lane starts
    /// nearest near; as drawn without it.
    fn route_from(&self, turnaround_m: f64, near: Option<Point>) -> Vec<Point> {
        let Some(near) = near else {
            return self.route(turnaround_m, false, false);
        };
        let starts_from = |route: &[Point]| route.first().map_or(f64::INFINITY, |p| gap(*p, near));
        [(false, false), (false, true), (true, false), (true, true)]
            .into_iter()
            .map(|(reverse, first_back)| self.route(turnaround_m, reverse, first_back))
            .min_by(|a, b| starts_from(a).total_cmp(&starts_from(b)))
            .unwrap_or_default()
    }
}

/// A survey's settings, in local metres.
struct Survey<'a> {
    area: &'a [Point],
    spacing_m: f64,
    turnaround_m: f64,
    margin_m: f64,
    crosshatch: bool,
    /// Where the first lane starts nearest.
    start: Option<Point>,
    /// Rounding in the width is not another lane; off for a survey with
    /// none of the newer settings, which makes the items it made before.
    snap: bool,
}

impl Survey<'_> {
    /// Its lanes' ends in the order flown at an angle, before any way
    /// round an obstacle.
    fn lanes(&self, angle_deg: f64, keep: &[Keep]) -> Vec<Point> {
        let mut out = Lines::new(
            self.area,
            self.spacing_m,
            angle_deg,
            self.margin_m,
            keep,
            self.snap,
        )
        .route_from(self.turnaround_m, self.start);
        if self.crosshatch {
            // With a start chosen, the second pass starts where the first ends.
            let near = self.start.and(out.last().copied());
            out.extend(
                Lines::new(
                    self.area,
                    self.spacing_m,
                    angle_deg + 90.0,
                    self.margin_m,
                    keep,
                    self.snap,
                )
                .route_from(self.turnaround_m, near),
            );
        }
        out
    }

    /// The angle with the fewest lines across the area, then the shortest
    /// flight, of each of the area's edges and every whole degree. Lines
    /// along an edge win a tie in lines: a slant with as many lines can be
    /// shorter only by leaving corners uncovered.
    fn shortest_angle(&self, keep: &[Keep]) -> f64 {
        let n = self.area.len();
        let edges = (0..n).map(|i| {
            let (dx, dy) = sub(self.area[(i + 1) % n], self.area[i]);
            (dx.atan2(dy).to_degrees().rem_euclid(180.0), false)
        });
        edges
            .chain((0..180).map(|degree| (f64::from(degree), true)))
            .map(|(angle, slant)| {
                let across = Lines::new(
                    self.area,
                    self.spacing_m,
                    angle,
                    self.margin_m,
                    keep,
                    self.snap,
                );
                let flown = length(&self.lanes(angle, keep));
                (angle, (across.scan.len(), slant), flown)
            })
            .filter(|(_, _, flown)| *flown > 0.0)
            .min_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)))
            .map_or(0.0, |(angle, _, _)| angle)
    }
}

/// The route with a way round every obstacle a leg would pass too near;
/// none when a leg has no way round, as the plan's checks look at items,
/// not the legs between them.
fn with_ways_round(points: Vec<Point>, keep: &[Keep], margin_m: f64) -> Option<Vec<Point>> {
    if keep.is_empty() {
        return Some(points);
    }
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if let Some(&last) = out.last() {
            out.extend(way_round(last, p, keep, margin_m)?);
        }
        out.push(p);
        if out.len() > MAX_ITEMS {
            break;
        }
    }
    Some(out)
}

/// The shortest way from a to b by the points round the obstacles,
/// without a and b: empty when the leg is clear, none when no way round
/// is found.
fn way_round(a: Point, b: Point, keep: &[Keep], margin_m: f64) -> Option<Vec<Point>> {
    let blocked = |p: Point, q: Point| keep.iter().any(|k| too_near(p, q, &k.shape, margin_m));
    if !blocked(a, b) {
        return Some(Vec::new());
    }
    // By the obstacles in the way first, then by all of them.
    let in_the_way: Vec<&Keep> = keep
        .iter()
        .filter(|k| too_near(a, b, &k.shape, margin_m))
        .collect();
    for candidates in [in_the_way, keep.iter().collect()] {
        let mut nodes = vec![a, b];
        nodes.extend(candidates.iter().flat_map(|k| k.around.iter().copied()));
        if let Some(path) = shortest_path(&nodes, &blocked) {
            return Some(path);
        }
    }
    None
}

/// Dijkstra from node 0 to node 1 over the legs not blocked: the nodes
/// between, in order.
fn shortest_path(nodes: &[Point], blocked: &dyn Fn(Point, Point) -> bool) -> Option<Vec<Point>> {
    let n = nodes.len();
    let mut best = vec![f64::INFINITY; n];
    let mut previous = vec![0; n];
    let mut done = vec![false; n];
    best[0] = 0.0;
    loop {
        let here = (0..n)
            .filter(|&i| !done[i] && best[i].is_finite())
            .min_by(|&i, &j| best[i].total_cmp(&best[j]))?;
        if here == 1 {
            break;
        }
        done[here] = true;
        for next in 0..n {
            let through = best[here] + gap(nodes[here], nodes[next]);
            if !done[next] && through < best[next] && !blocked(nodes[here], nodes[next]) {
                best[next] = through;
                previous[next] = here;
            }
        }
    }
    let mut path = Vec::new();
    let mut at = previous[1];
    while at != 0 {
        path.push(nodes[at]);
        at = previous[at];
    }
    path.reverse();
    Some(path)
}

/// Whether a leg passes nearer an obstacle than the margin, or through it.
fn too_near(a: Point, b: Point, obstacle: &Obstacle, margin_m: f64) -> bool {
    match obstacle {
        Obstacle::Circle(centre, radius) => {
            point_to_segment(*centre, a, b) < radius + margin_m - SLACK_M
        }
        Obstacle::Polygon(points) => {
            let n = points.len();
            (margin_m > SLACK_M
                && (0..n).any(|i| {
                    segment_to_segment(a, b, points[i], points[(i + 1) % n]) < margin_m - SLACK_M
                }))
                || passes_inside(a, b, points)
        }
    }
}

/// Whether any stretch of a leg lies inside a polygon; touching it at an
/// end does not count.
fn passes_inside(a: Point, b: Point, polygon: &[Point]) -> bool {
    let leg = sub(b, a);
    let mut crossings = vec![0.0, 1.0];
    for i in 0..polygon.len() {
        let corner = polygon[i];
        let edge = sub(polygon[(i + 1) % polygon.len()], corner);
        let denominator = cross(leg, edge);
        if denominator.abs() < 1e-12 {
            continue;
        }
        let offset = sub(corner, a);
        let on_leg = cross(offset, edge) / denominator;
        let on_edge = cross(offset, leg) / denominator;
        if (0.0..=1.0).contains(&on_leg) && (0.0..=1.0).contains(&on_edge) {
            crossings.push(on_leg);
        }
    }
    crossings.sort_by(f64::total_cmp);
    crossings.windows(2).any(|pair| {
        let t = f64::midpoint(pair[0], pair[1]);
        pair[1] - pair[0] > 1e-9 && inside(polygon, (a.0 + leg.0 * t, a.1 + leg.1 * t))
    })
}

fn cross(u: Point, v: Point) -> f64 {
    u.0 * v.1 - u.1 * v.0
}

fn inside(polygon: &[Point], p: Point) -> bool {
    let mut is_inside = false;
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
            is_inside = !is_inside;
        }
    }
    is_inside
}

fn point_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let leg = sub(b, a);
    let span = dot(leg, leg);
    let t = if span < 1e-18 {
        0.0
    } else {
        (dot(sub(p, a), leg) / span).clamp(0.0, 1.0)
    };
    gap(p, (a.0 + leg.0 * t, a.1 + leg.1 * t))
}

fn segment_to_segment(a: Point, b: Point, c: Point, d: Point) -> f64 {
    let side = |from: Point, to: Point, point: Point| cross(sub(to, from), sub(point, from));
    if side(a, b, c) * side(a, b, d) < 0.0 && side(c, d, a) * side(c, d, b) < 0.0 {
        return 0.0;
    }
    point_to_segment(a, c, d)
        .min(point_to_segment(b, c, d))
        .min(point_to_segment(c, a, b))
        .min(point_to_segment(d, a, b))
}

/// The convex hull, anticlockwise.
fn hull(points: &[Point]) -> Vec<Point> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    sorted.dedup_by(|a, b| gap(*a, *b) < 1e-9);
    if sorted.len() < 3 {
        return sorted;
    }
    let turn = |origin: Point, a: Point, b: Point| cross(sub(a, origin), sub(b, origin));
    let half = |points: &mut dyn Iterator<Item = &Point>| {
        let mut chain: Vec<Point> = Vec::new();
        for &p in points {
            while chain.len() >= 2 && turn(chain[chain.len() - 2], chain[chain.len() - 1], p) <= 0.0
            {
                chain.pop();
            }
            chain.push(p);
        }
        chain.pop();
        chain
    };
    let mut out = half(&mut sorted.iter());
    out.extend(half(&mut sorted.iter().rev()));
    out
}

/// Points round an obstacle out of its margin: its hull grown by a little
/// more than the margin, its corners rounded in steps no wider than
/// `ARC_STEP_DEG`, so no leg between them comes within the margin.
fn way_round_points(obstacle: &Obstacle, margin_m: f64) -> Vec<Point> {
    use std::f64::consts::{FRAC_PI_2, PI, TAU};
    let clear = margin_m.max(0.0) + ROUND_BY_M;
    match obstacle {
        Obstacle::Circle(centre, radius) => {
            let r = (radius + clear) / (PI / 16.0).cos();
            (0..16)
                .map(|i| {
                    let a = f64::from(i) * PI / 8.0;
                    (centre.0 + r * a.cos(), centre.1 + r * a.sin())
                })
                .collect()
        }
        Obstacle::Polygon(points) => {
            let corners = hull(points);
            let n = corners.len();
            if n < 2 {
                return Vec::new();
            }
            let step = ARC_STEP_DEG.to_radians();
            let out_by = clear / (step / 2.0).cos();
            let outward = |a: Point, b: Point| {
                let (dx, dy) = sub(b, a);
                dy.atan2(dx) - FRAC_PI_2
            };
            let mut out = Vec::new();
            for i in 0..n {
                let corner = corners[i];
                let from = outward(corners[(i + n - 1) % n], corner);
                let mut to = outward(corner, corners[(i + 1) % n]);
                while to < from {
                    to += TAU;
                }
                let steps = (((to - from) / step).ceil() as usize).max(1);
                for s in 0..=steps {
                    let a = from + (to - from) * s as f64 / steps as f64;
                    out.push((corner.0 + out_by * a.cos(), corner.1 + out_by * a.sin()));
                }
            }
            out
        }
    }
}

/// The plan's exclusion zones in local metres, with the ways round them.
fn obstacles(body: &PlanBody, local: &Local, margin_m: f64) -> Vec<Keep> {
    let polygons = body
        .fence
        .polygons
        .iter()
        .filter(|p| !p.inclusion && p.points.len() >= 3)
        .map(|p| Obstacle::Polygon(p.points.iter().map(|&q| local.to(q)).collect()));
    let circles = body
        .fence
        .circles
        .iter()
        .filter(|c| !c.inclusion && c.radius_m > 0.0)
        .map(|c| Obstacle::Circle(local.to(c.center), c.radius_m));
    polygons
        .chain(circles)
        .map(|shape| Keep {
            around: way_round_points(&shape, margin_m),
            shape,
        })
        .collect()
}

/// Where a survey's first lane starts nearest, in local metres.
fn start_point(start: Option<SurveyStart>, area: &[Point], home: Option<Point>) -> Option<Point> {
    let fold = |of: fn(&Point) -> f64, pick: fn(f64, f64) -> f64, from: f64| {
        area.iter().map(of).fold(from, pick)
    };
    let west = fold(|p| p.0, f64::min, f64::INFINITY);
    let east = fold(|p| p.0, f64::max, f64::NEG_INFINITY);
    let south = fold(|p| p.1, f64::min, f64::INFINITY);
    let north = fold(|p| p.1, f64::max, f64::NEG_INFINITY);
    match start? {
        SurveyStart::NorthWest => Some((west, north)),
        SurveyStart::NorthEast => Some((east, north)),
        SurveyStart::SouthEast => Some((east, south)),
        SurveyStart::SouthWest => Some((west, south)),
        SurveyStart::Home => home,
    }
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

/// A survey's route in local metres around its area, and the angle it is
/// flown at: its own, or the shortest when it asks for that. The lanes
/// keep out of the plan's exclusion zones and the flight goes round them.
/// A survey's route in local metres and the angle it is flown at; the
/// route is none when a leg has no way round an exclusion zone.
fn survey_route(
    pattern: &PlanPattern,
    body: &PlanBody,
) -> Option<(Local, Option<Vec<Point>>, f64)> {
    let PatternShape::Survey {
        area,
        spacing_m,
        angle_deg,
        turnaround_m,
        crosshatch,
        margin_m,
        start,
        shortest,
        ..
    } = &pattern.shape
    else {
        return None;
    };
    let local = Local::around(area)?;
    let flat: Vec<Point> = area.iter().map(|&p| local.to(p)).collect();
    let margin = margin_m.max(0.0);
    let clear = margin.max(MIN_CLEAR_M);
    let keep = obstacles(body, &local, clear);
    let home = body.planned_home.map(|h| {
        local.to(LatLon {
            latitude_deg: h.latitude_deg,
            longitude_deg: h.longitude_deg,
        })
    });
    let survey = Survey {
        area: &flat,
        spacing_m: *spacing_m,
        turnaround_m: *turnaround_m,
        margin_m: margin,
        crosshatch: *crosshatch,
        start: start_point(*start, &flat, home),
        snap: margin > 0.0 || start.is_some() || *shortest,
    };
    let angle = if *shortest {
        survey.shortest_angle(&keep)
    } else {
        *angle_deg
    };
    let route = with_ways_round(survey.lanes(angle, &keep), &keep, clear);
    Some((local, route, angle))
}

/// The items one pattern makes, in the plan's frame.
#[must_use]
pub fn items(pattern: &PlanPattern, body: &PlanBody) -> Vec<PlanItem> {
    let id = pattern.id.as_str();
    let frame = body.frame.mav_frame();
    let mut out = Vec::new();
    match &pattern.shape {
        PatternShape::Survey { altitude_m, .. } => {
            let Some((local, Some(route), _)) = survey_route(pattern, body) else {
                return out;
            };
            for point in route {
                out.push(item(WAYPOINT, frame, local.back(point), *altitude_m, id));
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
            let land = if body.vehicle_type == PlanVehicleType::Vtol {
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

/// Why a survey cannot be saved: a leg with no way round an exclusion
/// zone, or no lane left by its margin or the zones. None for a survey
/// that makes items, and for one with no margin in a plan without zones,
/// which is kept as before.
#[must_use]
pub fn survey_refusal(pattern: &PlanPattern, body: &PlanBody) -> Option<String> {
    let PatternShape::Survey { margin_m, .. } = &pattern.shape else {
        return None;
    };
    let zones = body.fence.polygons.iter().any(|p| !p.inclusion)
        || body.fence.circles.iter().any(|c| !c.inclusion);
    match survey_route(pattern, body)? {
        (_, None, _) => Some(format!(
            "{} has no way round an exclusion zone: a lane, its turn past the edge or a leg between lanes would pass within its margin",
            pattern.id
        )),
        (_, Some(route), _) if route.is_empty() && (zones || *margin_m > 0.0) => Some(format!(
            "{} has no room for a lane: its margin or the exclusion zones cover its area",
            pattern.id
        )),
        _ => None,
    }
}

/// Makes each pattern's items again, in the place of the first item it
/// made before (at the end for a new pattern); drops items of patterns no
/// longer in the plan.
pub fn expand(body: &mut PlanBody) {
    if body.patterns.is_empty() && body.items.iter().all(|i| i.pattern.is_none()) {
        return;
    }
    // A survey set to the shortest keeps the angle it is flown at.
    for i in 0..body.patterns.len() {
        if matches!(
            body.patterns[i].shape,
            PatternShape::Survey { shortest: true, .. }
        ) && let Some((_, _, angle)) = survey_route(&body.patterns[i], body)
            && let PatternShape::Survey { angle_deg, .. } = &mut body.patterns[i].shape
        {
            *angle_deg = angle;
        }
    }
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
            out.extend(items(pattern, body));
        }
    }
    for pattern in &body.patterns {
        if !placed.contains(&pattern.id) {
            out.extend(items(pattern, body));
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
    use uxduck_core::domain::plan::{
        FenceCircle, FencePolygon, PlanAutopilot, PlanFrame, PlannedHome, distance_m,
    };

    use super::*;

    /// The items a pattern makes in a plan of this type and frame, with no fence.
    fn made_by(pattern: &PlanPattern, vehicle_type: PlanVehicleType, frame: u8) -> Vec<PlanItem> {
        let mut body = PlanBody::new("Field".into(), vehicle_type, PlanAutopilot::Ardupilot);
        body.frame = PlanFrame::from_mav_frame(frame).unwrap();
        items(pattern, &body)
    }

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
                margin_m: 0.0,
                start: None,
                shortest: false,
            },
        }
    }

    #[test]
    fn a_survey_covers_a_square_back_and_forth() {
        let made = made_by(
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
        let plain = made_by(
            &survey(field(), 20.0, 0.0, 0.0),
            PlanVehicleType::Multicopter,
            10,
        );
        let past = made_by(
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
        let made = made_by(
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
        let made = made_by(
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
        let made = made_by(&pattern, PlanVehicleType::Multicopter, 3);
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
        let made = made_by(&orbit, PlanVehicleType::Multicopter, 3);
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
        let made = made_by(&landing, PlanVehicleType::Plane, 10);
        let commands: Vec<u16> = made.iter().map(|i| i.command).collect();
        assert_eq!(commands, [189, 16, 21]);
        assert_eq!(made[1].altitude_m, 60.0);
        let made = made_by(&landing, PlanVehicleType::Vtol, 10);
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

    /// A field about 200 m square with a 40 m square exclusion zone in its
    /// middle, and a survey over it.
    fn field_with_a_pond(margin_m: f64) -> (PlanBody, PlanPattern) {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        body.fence.polygons.push(FencePolygon {
            inclusion: false,
            points: vec![
                at(-35.00072, 149.00088),
                at(-35.00072, 149.00132),
                at(-35.00108, 149.00132),
                at(-35.00108, 149.00088),
            ],
            name: Some("Pond".into()),
        });
        let mut pattern = survey(field(), 10.0, 0.0, 0.0);
        if let PatternShape::Survey { margin_m: m, .. } = &mut pattern.shape {
            *m = margin_m;
        }
        (body, pattern)
    }

    fn flat(made: &[PlanItem], local: &Local) -> Vec<Point> {
        made.iter()
            .map(|i| local.to(at(i.latitude_deg, i.longitude_deg)))
            .collect()
    }

    fn with(mut pattern: PlanPattern, change: impl FnOnce(&mut PatternShape)) -> PlanPattern {
        change(&mut pattern.shape);
        pattern
    }

    #[test]
    fn a_survey_goes_round_an_exclusion_zone() {
        let (body, pattern) = field_with_a_pond(0.0);
        let local = Local::around(&field()).unwrap();
        let route = flat(&items(&pattern, &body), &local);
        let pond = Obstacle::Polygon(
            body.fence.polygons[0]
                .points
                .iter()
                .map(|&p| local.to(p))
                .collect(),
        );
        // No leg crosses the pond: lanes beside it stop at it and go on
        // past it, and the flight between goes round.
        for leg in route.windows(2) {
            assert!(!too_near(leg[0], leg[1], &pond, 0.0), "{leg:?}");
        }
        assert!(route.len() > 21 * 2, "{}", route.len());
        // Even with no margin no point lies on the pond's edge, which the
        // plan's fence check would count as inside it.
        for leg in route.windows(2) {
            assert!(!too_near(leg[0], leg[1], &pond, MIN_CLEAR_M), "{leg:?}");
        }
        // Without the pond the same survey flies straight through it.
        let mut open = body.clone();
        open.fence.polygons.clear();
        let straight = flat(&items(&pattern, &open), &local);
        assert_eq!(straight.len(), 21 * 2);
        assert!(
            straight
                .windows(2)
                .any(|leg| too_near(leg[0], leg[1], &pond, 0.0))
        );
    }

    #[test]
    fn a_survey_with_no_way_round_a_zone_makes_nothing_and_is_refused() {
        // A zone just past the north edge, where the lanes turn 20 m out.
        let (mut body, pattern) = field_with_a_pond(0.0);
        body.fence.polygons.clear();
        body.fence.circles.push(FenceCircle {
            inclusion: false,
            center: at(-34.999_865, 149.0011),
            radius_m: 8.0,
            name: None,
        });
        let turning = with(pattern.clone(), |s| {
            if let PatternShape::Survey { turnaround_m, .. } = s {
                *turnaround_m = 20.0;
            }
        });
        assert!(items(&turning, &body).is_empty());
        assert!(
            survey_refusal(&turning, &body)
                .is_some_and(|why| why.contains("no way round an exclusion zone"))
        );
        // Without the turn past the edge the lanes keep clear of it.
        assert!(!items(&pattern, &body).is_empty());
        assert_eq!(survey_refusal(&pattern, &body), None);
    }

    #[test]
    fn a_survey_without_zones_or_a_margin_is_never_refused() {
        let mut flat_area = survey(field(), 10.0, 0.0, 0.0);
        if let PatternShape::Survey { area, .. } = &mut flat_area.shape {
            area.truncate(2);
        }
        let body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        assert!(items(&flat_area, &body).is_empty());
        assert_eq!(survey_refusal(&flat_area, &body), None);
    }

    #[test]
    fn a_margin_keeps_lanes_from_the_edge_and_from_obstacles() {
        let (mut body, pattern) = field_with_a_pond(8.0);
        body.fence.circles.push(FenceCircle {
            inclusion: false,
            center: at(-35.0004, 149.0004),
            radius_m: 6.0,
            name: Some("Tree".into()),
        });
        let local = Local::around(&field()).unwrap();
        let route = flat(&items(&pattern, &body), &local);
        let edge: Vec<Point> = field().iter().map(|&p| local.to(p)).collect();
        for &p in &route {
            let nearest = (0..4)
                .map(|i| point_to_segment(p, edge[i], edge[(i + 1) % 4]))
                .fold(f64::INFINITY, f64::min);
            assert!(nearest > 8.0 - 0.1, "{p:?} is {nearest} m from the edge");
        }
        let keep = obstacles(&body, &local, 8.0);
        assert_eq!(keep.len(), 2);
        for k in &keep {
            for leg in route.windows(2) {
                assert!(!too_near(leg[0], leg[1], &k.shape, 8.0), "{leg:?}");
            }
        }
    }

    #[test]
    fn a_survey_starts_at_the_corner_or_home_asked_for() {
        let mut body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        let first = |made: &[PlanItem]| at(made[0].latitude_deg, made[0].longitude_deg);
        let corners = [
            (SurveyStart::NorthWest, at(-35.0, 149.0)),
            (SurveyStart::NorthEast, at(-35.0, 149.0022)),
            (SurveyStart::SouthEast, at(-35.0018, 149.0022)),
            (SurveyStart::SouthWest, at(-35.0018, 149.0)),
        ];
        for (start, corner) in corners {
            let pattern = with(survey(field(), 20.0, 0.0, 0.0), |shape| {
                if let PatternShape::Survey { start: s, .. } = shape {
                    *s = Some(start);
                }
            });
            let made = items(&pattern, &body);
            let off = distance_m(first(&made), corner);
            assert!(off < 2.0, "{start:?} starts {off} m from its corner");
        }
        // As drawn, the first lane is the westmost, flown north from the south.
        let drawn = items(&survey(field(), 20.0, 0.0, 0.0), &body);
        assert!(distance_m(first(&drawn), at(-35.0018, 149.0)) < 2.0);
        // Home, by the north-east corner.
        body.planned_home = Some(PlannedHome {
            latitude_deg: -34.9999,
            longitude_deg: 149.0023,
            altitude_m: None,
        });
        let pattern = with(survey(field(), 20.0, 0.0, 0.0), |shape| {
            if let PatternShape::Survey { start, .. } = shape {
                *start = Some(SurveyStart::Home);
            }
        });
        let made = items(&pattern, &body);
        assert!(distance_m(first(&made), at(-35.0, 149.0022)) < 2.0);
    }

    #[test]
    fn the_shortest_angle_runs_lanes_along_a_long_field() {
        // A strip 400 m by 60 m, its long side at a bearing of 30°.
        let local = Local::around(&field()).unwrap();
        let (s, c) = 30f64.to_radians().sin_cos();
        let corner =
            |along: f64, across: f64| local.back((along * s + across * c, along * c - across * s));
        let strip = vec![
            corner(-200.0, -30.0),
            corner(200.0, -30.0),
            corner(200.0, 30.0),
            corner(-200.0, 30.0),
        ];
        let mut body = PlanBody::new(
            "Strip".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        let across = items(&survey(strip.clone(), 10.0, 120.0, 0.0), &body).len();
        body.patterns = vec![with(survey(strip, 10.0, 120.0, 0.0), |shape| {
            if let PatternShape::Survey { shortest, .. } = shape {
                *shortest = true;
            }
        })];
        expand(&mut body);
        let PatternShape::Survey { angle_deg, .. } = body.patterns[0].shape else {
            unreachable!()
        };
        // Saving keeps the angle it picked: along the strip.
        assert!((angle_deg - 30.0).abs() < 0.5, "{angle_deg}");
        // Six lanes along it rather than 41 across it, where a survey with
        // none of the newer settings keeps its rounding (400 m in 10 m
        // lanes comes out a hair over 40).
        assert_eq!(body.items.len(), 12);
        assert_eq!(across, 82);

        // A square field with a pond: lanes along its edges, though a slant
        // that leaves the corners is shorter.
        let (mut body, pattern) = field_with_a_pond(10.0);
        body.patterns = vec![with(pattern, |shape| {
            if let PatternShape::Survey { shortest, .. } = shape {
                *shortest = true;
            }
        })];
        expand(&mut body);
        let PatternShape::Survey { angle_deg, .. } = body.patterns[0].shape else {
            unreachable!()
        };
        assert!(
            angle_deg.rem_euclid(90.0) < 0.5 || angle_deg.rem_euclid(90.0) > 89.5,
            "{angle_deg}"
        );
    }

    #[test]
    fn a_crosshatch_with_a_start_carries_on_from_where_the_first_pass_ends() {
        let body = PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        );
        let pattern = with(survey(field(), 40.0, 0.0, 0.0), |shape| {
            if let PatternShape::Survey {
                crosshatch, start, ..
            } = shape
            {
                *crosshatch = true;
                *start = Some(SurveyStart::NorthWest);
            }
        });
        let made = items(&pattern, &body);
        let half = made.len() / 2;
        let end = at(made[half - 1].latitude_deg, made[half - 1].longitude_deg);
        let next = at(made[half].latitude_deg, made[half].longitude_deg);
        assert!(distance_m(end, next) < 2.0, "{}", distance_m(end, next));
    }
}

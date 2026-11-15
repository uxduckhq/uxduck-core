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
    reason = "lane and point counts are checked against `MOST_ITEMS` before any is made, and widths over spacings are positive"
)]

use std::cell::RefCell;
use std::collections::HashSet;

use uxduck_core::domain::plan::{
    LatLon, PatternShape, PlanBody, PlanItem, PlanPattern, PlanVehicleType, SurveyStart,
};

/// The mean Earth radius, as `distance_m` uses.
const EARTH_M: f64 = 6_371_008.8;

/// The most items one mission carries, and so the most one pattern is made
/// with. MAVLink counts a mission's items in 16 bits (`MISSION_COUNT.count`),
/// so no autopilot can be sent more, and a slip, such as a spacing of 1 m
/// over a country, would otherwise make millions and hold UXDuck up while
/// it made them. A pattern that would make more is refused before any is
/// made, with the number ([`refusal`]). It is not what an autopilot holds,
/// which is less and is said when the plan is uploaded: MAVLink has no way
/// to ask. A pattern is never cut short to fit a count: a survey that
/// covers part of its area looks complete.
pub const MOST_ITEMS: usize = 65_535;

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
    /// Whether the leg from one of those points to another is blocked,
    /// once found: 0 not looked at, 1 clear, 2 blocked. It is the same for
    /// every way round, and a survey has one for each lane a zone cuts, so
    /// it is found once, not once a lane.
    legs: RefCell<Vec<u8>>,
}

impl Keep {
    fn new(shape: Obstacle, margin_m: f64) -> Self {
        Self {
            around: way_round_points(&shape, margin_m),
            shape,
            legs: RefCell::new(Vec::new()),
        }
    }

    /// Whether the leg between two of its points round is blocked, as
    /// `blocked` finds it the first time it is asked.
    fn leg_blocked(&self, from: usize, to: usize, blocked: impl FnOnce() -> bool) -> bool {
        let count = self.around.len();
        let mut legs = self.legs.borrow_mut();
        if legs.is_empty() {
            legs.resize(count * count, 0);
        }
        let known = &mut legs[from * count + to];
        if *known == 0 {
            *known = if blocked() { 2 } else { 1 };
        }
        *known == 2
    }
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

/// A polygon's edges for lines taken across it in rising order. A line
/// looks only at the edges it can come within reach of, so it costs what
/// it meets and not the polygon's corners. Otherwise an outline of
/// thousands of corners under tens of thousands of lanes costs their
/// product, once for every angle tried.
struct Sweep {
    /// Each edge's first corner, with where across it can first and last
    /// be met, in the order they are first met.
    edges: Vec<(f64, f64, usize)>,
    next: usize,
    /// The edges the last line could meet, in the polygon's order.
    near: Vec<(usize, f64)>,
    met: Vec<usize>,
}

impl Sweep {
    /// The edges of a polygon whose points are (along, across), met from
    /// `reach` away.
    fn new(polygon: &[Point], reach: f64) -> Self {
        // Wider than the test each edge is given, so rounding leaves none out.
        let reach = reach.max(0.0) + 1e-6;
        let mut edges: Vec<(f64, f64, usize)> = (0..polygon.len())
            .map(|i| {
                let a = polygon[i].1;
                let b = polygon[(i + 1) % polygon.len()].1;
                (a.min(b) - reach, a.max(b) + reach, i)
            })
            .collect();
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));
        Self {
            edges,
            next: 0,
            near: Vec::new(),
            met: Vec::new(),
        }
    }

    /// The first corners of the edges a line across at v can meet, in the
    /// polygon's order. Each v is at or past the one before.
    fn at(&mut self, v: f64) -> &[usize] {
        let known = self.near.len();
        while let Some(&(from, to, i)) = self.edges.get(self.next)
            && from <= v
        {
            self.near.push((i, to));
            self.next += 1;
        }
        let new = self.near.len() > known;
        self.near.retain(|(_, to)| *to >= v);
        if new {
            self.near.sort_unstable_by_key(|(i, _)| *i);
        }
        self.met.clear();
        self.met.extend(self.near.iter().map(|(i, _)| *i));
        &self.met
    }
}

/// The pieces of a line across (at v) inside a polygon, along it, from
/// the edges it can meet (`Sweep::at`).
fn inside_on_line(polygon: &[Point], edges: &[usize], v: f64) -> Vec<(f64, f64)> {
    let mut crossings = Vec::new();
    for &i in edges {
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

/// The unit vectors along a bearing's lanes and across them.
fn axes(angle_deg: f64) -> (Point, Point) {
    let bearing = angle_deg.to_radians();
    (
        (bearing.sin(), bearing.cos()),
        (bearing.cos(), -bearing.sin()),
    )
}

/// Where an area starts across its lanes, and its width there less the
/// margin on both sides, never negative. None for fewer than three corners
/// or a width that is not a number.
fn span_across(area: &[Point], across: Point, margin: f64) -> Option<(f64, f64)> {
    let places = || area.iter().map(|p| dot(*p, across));
    let v_min = places().fold(f64::INFINITY, f64::min);
    let v_max = places().fold(f64::NEG_INFINITY, f64::max);
    let width = v_max - v_min - 2.0 * margin;
    (area.len() >= 3 && width.is_finite()).then_some((v_min, width.max(0.0)))
}

/// As many lines as cover a width with a lane's width each (the spacing):
/// at least one. With `snap`, a width a whole number of spacings, give or
/// take rounding, is that many lines. A float, because a slip in the
/// spacing asks for more lines than a count holds: it is compared with
/// `MOST_ITEMS` before it becomes one.
fn lines_across(width: f64, spacing: f64, snap: bool) -> f64 {
    (width / spacing - if snap { 1e-6 } else { 0.0 })
        .ceil()
        .max(1.0)
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
    /// pieces. Every line is made, however many: the caller has counted
    /// them first (`Survey::waypoints`), with a spacing above zero.
    fn new(
        area: &[Point],
        spacing_m: f64,
        angle_deg: f64,
        margin_m: f64,
        keep: &[Keep],
        snap: bool,
    ) -> Self {
        let spacing = spacing_m;
        let margin = margin_m.max(0.0);
        let (along, across) = axes(angle_deg);
        let mut scan = Vec::new();
        let uv = |p: &Point| (dot(*p, along), dot(*p, across));
        let field: Vec<Point> = area.iter().map(uv).collect();
        let Some((v_min, width)) = span_across(area, across, margin) else {
            return Self {
                along,
                across,
                scan,
            };
        };
        let obstacles: Vec<Obstacle> = keep
            .iter()
            .map(|k| match &k.shape {
                Obstacle::Polygon(points) => Obstacle::Polygon(points.iter().map(uv).collect()),
                Obstacle::Circle(centre, radius) => Obstacle::Circle(uv(centre), *radius),
            })
            .collect();
        // As many lanes as cover the width, centred in it.
        let count = lines_across(width, spacing, snap) as usize;
        let first = v_min + margin + (width - (count - 1) as f64 * spacing) / 2.0;
        let clear = margin.max(MIN_CLEAR_M);
        let near_edges =
            |polygon: &[Point], edges: &[usize], v: f64, reach: f64| -> Vec<(f64, f64)> {
                if reach <= 0.0 {
                    return Vec::new();
                }
                edges
                    .iter()
                    .filter_map(|&i| {
                        near_segment_on_line(polygon[i], polygon[(i + 1) % polygon.len()], reach, v)
                    })
                    .collect()
            };
        let mut field_edges = Sweep::new(&field, margin);
        let mut obstacle_edges: Vec<Sweep> = obstacles
            .iter()
            .map(|obstacle| match obstacle {
                Obstacle::Polygon(points) => Sweep::new(points, clear),
                Obstacle::Circle(..) => Sweep::new(&[], clear),
            })
            .collect();
        for k in 0..count {
            let v = first + k as f64 * spacing;
            let edges = field_edges.at(v);
            let pieces = inside_on_line(&field, edges, v)
                .into_iter()
                .map(|(a, b)| (a, b, true, true))
                .collect();
            let pieces = cut(pieces, near_edges(&field, edges, v, margin), true);
            let mut cuts = Vec::new();
            for (obstacle, edges) in obstacles.iter().zip(&mut obstacle_edges) {
                match obstacle {
                    Obstacle::Polygon(points) => {
                        let edges = edges.at(v);
                        cuts.extend(inside_on_line(points, edges, v));
                        cuts.extend(near_edges(points, edges, v, clear));
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
    /// How many waypoints its lanes make at an angle, two to a lane and a
    /// crosshatch's second pass with them, counted from the area's width
    /// before any is made. An exclusion zone or a concave edge that cuts a
    /// lane makes more.
    fn waypoints(&self, angle_deg: f64) -> f64 {
        let lines = |angle: f64| {
            span_across(self.area, axes(angle).1, self.margin_m).map_or(0.0, |(_, width)| {
                lines_across(width, self.spacing_m, self.snap)
            })
        };
        let second = if self.crosshatch {
            lines(angle_deg + 90.0)
        } else {
            0.0
        };
        2.0 * (lines(angle_deg) + second)
    }

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
    ///
    /// Each angle's lines are counted from the area's width, and only the
    /// angles that tie for the fewest are flown to find the shortest, so a
    /// survey of many lanes is not made once for every angle.
    ///
    /// # Errors
    ///
    /// The fewest waypoints any angle makes, when every angle makes more
    /// than `MOST_ITEMS`: no lane is made to find that out.
    fn shortest_angle(&self, keep: &[Keep]) -> Result<f64, f64> {
        let n = self.area.len();
        let edges = (0..n).map(|i| {
            let (dx, dy) = sub(self.area[(i + 1) % n], self.area[i]);
            (dx.atan2(dy).to_degrees().rem_euclid(180.0), false)
        });
        // Each angle with its lines across and whether it is a slant, in
        // the order tried: an edge first.
        let counted: Vec<(f64, (f64, bool))> = edges
            .chain((0..180).map(|degree| (f64::from(degree), true)))
            .filter_map(|(angle, slant)| {
                let (_, width) = span_across(self.area, axes(angle).1, self.margin_m)?;
                Some((
                    angle,
                    (lines_across(width, self.spacing_m, self.snap), slant),
                ))
            })
            .collect();
        let by_lines = |a: &(f64, bool), b: &(f64, bool)| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1));
        let most = MOST_ITEMS as f64;
        let mut fits: Vec<&(f64, (f64, bool))> = counted
            .iter()
            .filter(|(angle, _)| self.waypoints(*angle) <= most)
            .collect();
        if fits.is_empty() && !counted.is_empty() {
            return Err(counted
                .iter()
                .map(|(angle, _)| self.waypoints(*angle))
                .fold(f64::INFINITY, f64::min));
        }
        // A stable sort: the angles that tie stay in the order tried.
        fits.sort_by(|a, b| by_lines(&a.1, &b.1));
        let mut at = 0;
        while at < fits.len() {
            let tied = fits[at..]
                .iter()
                .take_while(|c| by_lines(&c.1, &fits[at].1).is_eq())
                .count();
            let shortest = fits[at..at + tied]
                .iter()
                .map(|(angle, _)| (*angle, length(&self.lanes(*angle, keep))))
                .filter(|(_, flown)| *flown > 0.0)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((angle, _)) = shortest {
                return Ok(angle);
            }
            at += tied;
        }
        Ok(0.0)
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
        // The obstacle each node goes round, and which of its points it is.
        let mut round: Vec<Option<(&Keep, usize)>> = vec![None, None];
        for k in candidates {
            nodes.extend(k.around.iter().copied());
            round.extend((0..k.around.len()).map(|i| Some((k, i))));
        }
        let leg_blocked = |i: usize, j: usize| match (round[i], round[j]) {
            (Some((k, from)), Some((other, to))) if std::ptr::eq(k, other) => {
                k.leg_blocked(from, to, || blocked(nodes[i], nodes[j]))
            }
            _ => blocked(nodes[i], nodes[j]),
        };
        if let Some(path) = shortest_path(&nodes, &leg_blocked) {
            return Some(path);
        }
    }
    None
}

/// Dijkstra from node 0 to node 1 over the legs not blocked, each asked
/// by its two nodes' places in the list: the nodes between, in order.
fn shortest_path(nodes: &[Point], blocked: &dyn Fn(usize, usize) -> bool) -> Option<Vec<Point>> {
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
            if !done[next] && through < best[next] && !blocked(here, next) {
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
        .map(|shape| Keep::new(shape, margin_m))
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

/// How many lanes cover a corridor's width at its spacing: at least one.
/// A float, as `lines_across` is.
fn corridor_lines(width_m: f64, spacing_m: f64) -> f64 {
    (width_m.max(0.0) / spacing_m).ceil().max(1.0)
}

/// Lanes along a line: as many as cover `width_m` at `spacing_m`, centred
/// on it, flown back and forth, each lengthened at its ends. Every lane is
/// made: the caller has counted them first, with a spacing above zero.
fn corridor_lanes(
    line: &[(f64, f64)],
    width_m: f64,
    spacing_m: f64,
    turnaround_m: f64,
) -> Vec<Vec<(f64, f64)>> {
    if line.len() < 2 {
        return Vec::new();
    }
    let count = corridor_lines(width_m, spacing_m) as usize;
    let mut lanes = Vec::new();
    for k in 0..count {
        let sideways = (k as f64 - (count - 1) as f64 / 2.0) * spacing_m;
        let mut path = offset_line(line, sideways);
        extend(&mut path, turnaround_m);
        if k % 2 == 1 {
            path.reverse();
        }
        lanes.push(path);
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

/// What a survey's settings come to.
enum Flight {
    /// Its route in local metres around its area: every lane.
    Route(Vec<Point>),
    /// A leg has no way round an exclusion zone.
    NoWayRound,
    /// Its spacing is not above zero, so it has no lanes to count.
    NoSpacing,
    /// About this many waypoints, more than `MOST_ITEMS`: none is made.
    TooMany(f64),
}

/// What a survey makes around its area, and the angle it is flown at: its
/// own, or the shortest when it asks for that. The lanes keep out of the
/// plan's exclusion zones and the flight goes round them. Its waypoints
/// are counted before any is made.
fn survey_route(pattern: &PlanPattern, body: &PlanBody) -> Option<(Local, Flight, f64)> {
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
    if spacing_m.is_nan() || *spacing_m <= 0.0 {
        return Some((local, Flight::NoSpacing, *angle_deg));
    }
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
        match survey.shortest_angle(&keep) {
            Ok(angle) => angle,
            Err(fewest) => return Some((local, Flight::TooMany(fewest), *angle_deg)),
        }
    } else {
        *angle_deg
    };
    let made = survey.waypoints(angle);
    if made > MOST_ITEMS as f64 {
        return Some((local, Flight::TooMany(made), *angle_deg));
    }
    let route = with_ways_round(survey.lanes(angle, &keep), &keep, clear)
        .map_or(Flight::NoWayRound, Flight::Route);
    Some((local, route, angle))
}

/// How many waypoints a corridor's lanes make, counted before any is made:
/// none for a spacing that is not above zero.
fn corridor_waypoints(points: usize, width_m: f64, spacing_m: f64) -> Option<f64> {
    (spacing_m > 0.0).then(|| corridor_lines(width_m, spacing_m) * points as f64)
}

/// The items one pattern makes, in the plan's frame: all of them, or none
/// when [`refusal`] says why not. Never some of them.
#[must_use]
pub fn items(pattern: &PlanPattern, body: &PlanBody) -> Vec<PlanItem> {
    let id = pattern.id.as_str();
    let frame = body.frame.mav_frame();
    let mut out = Vec::new();
    match &pattern.shape {
        PatternShape::Survey { altitude_m, .. } => {
            let Some((local, Flight::Route(route), _)) = survey_route(pattern, body) else {
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
            if corridor_waypoints(line.len(), *width_m, *spacing_m)
                .is_none_or(|made| made > MOST_ITEMS as f64)
            {
                return out;
            }
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
    out
}

/// A whole number with its thousands apart, as it is said to a person:
/// "2,400,000".
pub(crate) fn grouped(count: f64) -> String {
    let digits = format!("{count:.0}");
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Why a pattern cannot be saved, in words for the person who set it. A
/// number that means nothing: a spacing that is not above zero, or a
/// margin, a width or turns below it. More items than one mission
/// carries (`MOST_ITEMS`), counted before any is made and said with the
/// count. A survey's leg with no way round an exclusion zone, or no lane
/// left by its margin or the zones. None for a pattern that makes its
/// items, and for a survey with no margin in a plan without zones that
/// makes none, which is kept as before.
#[must_use]
pub fn refusal(pattern: &PlanPattern, body: &PlanBody) -> Option<String> {
    let flight = survey_route(pattern, body);
    said(pattern, body, flight.as_ref().map(|(_, flight, _)| flight))
}

/// As [`refusal`], of a pattern whose flight is made already when it is a
/// survey's, so saving a plan makes each survey once.
fn said(pattern: &PlanPattern, body: &PlanBody, flight: Option<&Flight>) -> Option<String> {
    let id = &pattern.id;
    let below_zero = |what: &str, value: f64| {
        (value < 0.0).then(|| format!("{id}'s {what} is {value}. It cannot be below 0"))
    };
    let no_spacing =
        |spacing_m: f64| format!("{id}'s spacing is {spacing_m} m. Lanes need a spacing above 0 m");
    let too_many = |made: f64, about: &str, smaller: &str| {
        let count = if made >= 1e12 {
            "more than 1,000,000,000,000".to_owned()
        } else {
            format!("{about}{}", grouped(made))
        };
        format!(
            "{id} would make {count} items. One mission carries {} at most, as MAVLink counts them in 16 bits. Use a wider spacing or {smaller}",
            grouped(MOST_ITEMS as f64)
        )
    };
    match &pattern.shape {
        PatternShape::Survey {
            margin_m,
            spacing_m,
            ..
        } => {
            if let Some(refusal) = below_zero("margin", *margin_m) {
                return Some(refusal);
            }
            let zones = body.fence.polygons.iter().any(|p| !p.inclusion)
                || body.fence.circles.iter().any(|c| !c.inclusion);
            match flight? {
                Flight::NoSpacing => Some(no_spacing(*spacing_m)),
                Flight::TooMany(made) => Some(too_many(*made, "about ", "a smaller area")),
                Flight::NoWayRound => Some(format!(
                    "{id} has no way round an exclusion zone: a lane, its turn past the edge or a leg between lanes would pass within its margin"
                )),
                Flight::Route(route) if route.is_empty() && (zones || *margin_m > 0.0) => {
                    Some(format!(
                        "{id} has no room for a lane: its margin or the exclusion zones cover its area"
                    ))
                }
                Flight::Route(_) => None,
            }
        }
        PatternShape::Corridor {
            line,
            width_m,
            spacing_m,
            ..
        } => below_zero("width", *width_m).or_else(|| {
            match corridor_waypoints(line.len(), *width_m, *spacing_m) {
                None => Some(no_spacing(*spacing_m)),
                Some(made) if made > MOST_ITEMS as f64 => {
                    Some(too_many(made, "", "a narrower corridor"))
                }
                Some(_) => None,
            }
        }),
        PatternShape::Orbit { turns, .. } => below_zero("number of turns", *turns),
        PatternShape::Landing { .. } => None,
    }
}

/// An earlier UXDuck stopped a pattern at this many items, and flew a
/// spacing under `ONCE_LEAST_SPACING_M` at it, without a word.
const ONCE_CUT_AT: f64 = 2000.0;
const ONCE_LEAST_SPACING_M: f64 = 1.0;

/// How many items a pattern makes now, when that is not the `held` the
/// plan has for it. A pattern's items are made again whenever its plan is
/// saved, so the two differ only in a plan last saved by an earlier
/// UXDuck, which cut a survey short and looked complete. Only a pattern
/// that one would have cut or spaced otherwise is made again to count it.
pub(crate) fn makes_now(pattern: &PlanPattern, body: &PlanBody, held: usize) -> Option<usize> {
    let asked = match &pattern.shape {
        PatternShape::Survey {
            area,
            spacing_m,
            angle_deg,
            margin_m,
            crosshatch,
            ..
        } => {
            let local = Local::around(area)?;
            let flat: Vec<Point> = area.iter().map(|&p| local.to(p)).collect();
            let survey = Survey {
                area: &flat,
                spacing_m: *spacing_m,
                turnaround_m: 0.0,
                margin_m: margin_m.max(0.0),
                crosshatch: *crosshatch,
                start: None,
                snap: false,
            };
            (*spacing_m, survey.waypoints(*angle_deg))
        }
        PatternShape::Corridor {
            line,
            width_m,
            spacing_m,
            ..
        } => (
            *spacing_m,
            corridor_waypoints(line.len(), *width_m, spacing_m.max(ONCE_LEAST_SPACING_M))?,
        ),
        PatternShape::Orbit { .. } | PatternShape::Landing { .. } => return None,
    };
    // NaN and a spacing of nothing were flown at 1 m too.
    if asked.0 >= ONCE_LEAST_SPACING_M && asked.1 < ONCE_CUT_AT {
        return None;
    }
    let makes = items(pattern, body).len();
    (makes != held).then_some(makes)
}

/// Makes each pattern's items again, in the place of the first item it
/// made before (at the end for a new pattern); drops items of patterns no
/// longer in the plan. Answers why the first pattern that makes none
/// cannot be saved ([`refusal`]), when one cannot.
pub fn expand(body: &mut PlanBody) -> Option<String> {
    if body.patterns.is_empty() && body.items.iter().all(|i| i.pattern.is_none()) {
        return None;
    }
    // Each pattern is made once: its items, why it has none, and the angle
    // a survey is flown at, which one set to the shortest keeps.
    let frame = body.frame.mav_frame();
    let mut made: Vec<Option<Vec<PlanItem>>> = Vec::with_capacity(body.patterns.len());
    let mut refused = None;
    for i in 0..body.patterns.len() {
        let pattern = &body.patterns[i];
        let (items, why, angle) = match survey_route(pattern, body) {
            Some((local, flight, angle)) => {
                let why = said(pattern, body, Some(&flight));
                let items = match (flight, &pattern.shape) {
                    (Flight::Route(route), PatternShape::Survey { altitude_m, .. }) => route
                        .into_iter()
                        .map(|point| {
                            item(WAYPOINT, frame, local.back(point), *altitude_m, &pattern.id)
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                (items, why, Some(angle))
            }
            None => (items(pattern, body), said(pattern, body, None), None),
        };
        refused = refused.or(why);
        made.push(Some(items));
        if let Some(angle) = angle
            && let PatternShape::Survey {
                shortest: true,
                angle_deg,
                ..
            } = &mut body.patterns[i].shape
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
        if let Some(at) = body.patterns.iter().position(|p| p.id == id) {
            placed.insert(id.to_owned());
            out.extend(made[at].take().unwrap_or_default());
        }
    }
    for (pattern, items) in body.patterns.iter().zip(&mut made) {
        if !placed.contains(&pattern.id) {
            out.extend(items.take().unwrap_or_default());
        }
    }
    body.items = out;
    refused
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

    /// A square `side_m` on a side, its south-west corner at 35° S, 149° E.
    fn square(side_m: f64) -> Vec<LatLon> {
        let local = Local::around(&[at(-35.0, 149.0)]).unwrap();
        [(0.0, 0.0), (side_m, 0.0), (side_m, side_m), (0.0, side_m)]
            .into_iter()
            .map(|corner| local.back(corner))
            .collect()
    }

    fn body() -> PlanBody {
        PlanBody::new(
            "Field".into(),
            PlanVehicleType::Multicopter,
            PlanAutopilot::Ardupilot,
        )
    }

    #[test]
    fn a_large_survey_covers_the_whole_area() {
        // 5 km across at 2 m: 2,500 lanes and 5,000 waypoints, where a
        // survey used to stop at 2,000 and leave three fifths of it out.
        let area = square(5_000.0);
        let pattern = survey(area.clone(), 2.0, 0.0, 0.0);
        let made = items(&pattern, &body());
        assert_eq!(refusal(&pattern, &body()), None);
        // North-south lanes from the west edge to the east edge, a
        // spacing apart all the way: none is left out anywhere.
        let local = Local::around(&area).unwrap();
        let easts: Vec<f64> = flat(&made, &local)
            .chunks(2)
            .map(|lane| lane[0].0)
            .collect();
        let (west, east) = (local.to(area[0]).0, local.to(area[1]).0);
        assert!(easts.len() >= 2_500, "{}", easts.len());
        assert_eq!(easts.len(), ((east - west) / 2.0).ceil() as usize);
        assert_eq!(made.len(), 2 * easts.len());
        assert!((easts[0] - west).abs() <= 2.0, "{} from {west}", easts[0]);
        assert!(
            (easts[easts.len() - 1] - east).abs() <= 2.0,
            "the last lane is at {} and the east edge at {east}",
            easts[easts.len() - 1]
        );
        for pair in easts.windows(2) {
            assert!((pair[1] - pair[0] - 2.0).abs() < 1e-6, "{pair:?}");
        }
    }

    #[test]
    fn a_spacing_is_used_as_it_was_typed() {
        // Half a metre apart is half a metre apart: it used to be flown at
        // 1 m without a word.
        let pattern = survey(square(20.0), 0.5, 0.0, 0.0);
        let made = items(&pattern, &body());
        // 40 lanes, or 41 by the rounding of the square's own corners.
        assert!([80, 82].contains(&made.len()), "{}", made.len());
        let local = Local::around(&square(20.0)).unwrap();
        let lanes = flat(&made, &local);
        for pair in lanes.chunks(2).collect::<Vec<_>>().windows(2) {
            assert!((pair[1][0].0 - pair[0][0].0 - 0.5).abs() < 1e-6, "{pair:?}");
        }
        assert_eq!(refusal(&pattern, &body()), None);

        let corridor = PlanPattern {
            id: "corridor-1".into(),
            shape: PatternShape::Corridor {
                line: vec![at(-35.0, 149.0), at(-35.0, 149.002)],
                width_m: 2.0,
                spacing_m: 0.25,
                turnaround_m: 0.0,
                altitude_m: 30.0,
            },
        };
        assert_eq!(items(&corridor, &body()).len(), 2 * 8);
    }

    #[test]
    fn a_spacing_of_nothing_is_refused_in_words_and_makes_no_items() {
        for spacing in [0.0, -3.0] {
            let pattern = survey(field(), spacing, 0.0, 0.0);
            assert!(items(&pattern, &body()).is_empty());
            assert_eq!(
                refusal(&pattern, &body()),
                Some(format!(
                    "survey-1's spacing is {spacing} m. Lanes need a spacing above 0 m"
                ))
            );
        }
        let corridor = PlanPattern {
            id: "corridor-1".into(),
            shape: PatternShape::Corridor {
                line: vec![at(-35.0, 149.0), at(-35.0, 149.002)],
                width_m: 40.0,
                spacing_m: 0.0,
                turnaround_m: 0.0,
                altitude_m: 30.0,
            },
        };
        assert!(items(&corridor, &body()).is_empty());
        assert_eq!(
            refusal(&corridor, &body()).as_deref(),
            Some("corridor-1's spacing is 0 m. Lanes need a spacing above 0 m")
        );
    }

    #[test]
    fn a_survey_of_more_waypoints_than_a_mission_carries_is_refused_before_any_is_made() {
        // The slip: 1 m apart over 300 km, 600,000 waypoints. It is
        // counted, said and not made, each of the three ways a survey is
        // set, and no part of it is kept.
        for change in [
            (|_| {}) as fn(&mut PatternShape),
            |shape| {
                if let PatternShape::Survey { shortest, .. } = shape {
                    *shortest = true;
                }
            },
            |shape| {
                if let PatternShape::Survey {
                    margin_m, start, ..
                } = shape
                {
                    *margin_m = 5.0;
                    *start = Some(SurveyStart::NorthWest);
                }
            },
        ] {
            let pattern = with(survey(square(300_000.0), 1.0, 0.0, 0.0), change);
            assert!(items(&pattern, &body()).is_empty());
            let why = refusal(&pattern, &body()).unwrap();
            assert!(
                why.starts_with("survey-1 would make about 6")
                    && why.ends_with(
                        " items. One mission carries 65,535 at most, as MAVLink counts them in 16 bits. Use a wider spacing or a smaller area"
                    ),
                "{why}"
            );
        }
        // A crosshatch counts both passes.
        let twice = with(survey(square(20_000.0), 1.0, 0.0, 0.0), |shape| {
            if let PatternShape::Survey { crosshatch, .. } = shape {
                *crosshatch = true;
            }
        });
        assert!(
            refusal(&twice, &body()).is_some_and(|why| why.contains("about 80,0")),
            "{:?}",
            refusal(&twice, &body())
        );
        // Saving the plan leaves the survey's place empty and its angle alone.
        let mut plan = body();
        plan.patterns = vec![with(survey(square(300_000.0), 1.0, 12.0, 0.0), |shape| {
            if let PatternShape::Survey { shortest, .. } = shape {
                *shortest = true;
            }
        })];
        expand(&mut plan);
        assert!(plan.items.is_empty());
        assert!(matches!(
            plan.patterns[0].shape,
            PatternShape::Survey { angle_deg, .. } if angle_deg == 12.0
        ));
    }

    #[test]
    fn a_survey_is_whole_right_up_to_what_a_mission_carries() {
        // The field is about 200.6 m across. At 6.3 mm that is some 31,840
        // lanes, under 65,535 waypoints: every one is made. At 6 mm it is
        // some 33,430 lanes, over it: none is.
        let whole = survey(field(), 0.0063, 0.0, 0.0);
        let made = items(&whole, &body());
        assert!(
            made.len() > 60_000 && made.len() <= MOST_ITEMS,
            "{}",
            made.len()
        );
        assert_eq!(refusal(&whole, &body()), None);
        let local = Local::around(&field()).unwrap();
        let lanes = flat(&made, &local);
        let east = local.to(field()[1]).0;
        assert!((lanes[lanes.len() - 1].0 - east).abs() < 0.01);

        let over = survey(field(), 0.006, 0.0, 0.0);
        assert!(items(&over, &body()).is_empty());
        assert!(refusal(&over, &body()).is_some_and(|why| why.contains("would make about 66,")));
    }

    #[test]
    fn a_wide_corridor_keeps_every_lane() {
        // 5 km wide at 2 m: 2,500 lanes of two points, where a corridor
        // used to stop after 1,001.
        let line = vec![at(-35.0, 149.0), at(-35.0, 149.01)];
        let corridor = |width_m: f64, spacing_m: f64| PlanPattern {
            id: "corridor-1".into(),
            shape: PatternShape::Corridor {
                line: line.clone(),
                width_m,
                spacing_m,
                turnaround_m: 0.0,
                altitude_m: 30.0,
            },
        };
        let made = items(&corridor(5_000.0, 2.0), &body());
        assert_eq!(made.len(), 5_000);
        // The outermost lanes are 4,998 m apart, either side of the line.
        let first = at(made[0].latitude_deg, made[0].longitude_deg);
        let last = &made[made.len() - 1];
        let across = distance_m(first, at(last.latitude_deg, last.longitude_deg));
        assert!((across - 4_998.0).abs() < 5.0, "{across}");
        // More than a mission carries: counted exactly, said, and not made.
        let slip = corridor(100_000.0, 1.0);
        assert!(items(&slip, &body()).is_empty());
        assert_eq!(
            refusal(&slip, &body()).as_deref(),
            Some(
                "corridor-1 would make 200,000 items. One mission carries 65,535 at most, as MAVLink counts them in 16 bits. Use a wider spacing or a narrower corridor"
            )
        );
    }

    #[test]
    fn a_margin_a_width_and_turns_below_zero_are_refused_in_words() {
        let margin = with(survey(field(), 20.0, 0.0, 0.0), |shape| {
            if let PatternShape::Survey { margin_m, .. } = shape {
                *margin_m = -4.0;
            }
        });
        assert_eq!(
            refusal(&margin, &body()).as_deref(),
            Some("survey-1's margin is -4. It cannot be below 0")
        );
        let corridor = PlanPattern {
            id: "corridor-2".into(),
            shape: PatternShape::Corridor {
                line: vec![at(-35.0, 149.0), at(-35.0, 149.002)],
                width_m: -1.0,
                spacing_m: 20.0,
                turnaround_m: 0.0,
                altitude_m: 30.0,
            },
        };
        assert_eq!(
            refusal(&corridor, &body()).as_deref(),
            Some("corridor-2's width is -1. It cannot be below 0")
        );
        let orbit = PlanPattern {
            id: "orbit-1".into(),
            shape: PatternShape::Orbit {
                center: at(-35.0, 149.0),
                radius_m: 30.0,
                turns: -2.0,
                heights_m: vec![20.0],
                counter_clockwise: false,
            },
        };
        assert_eq!(
            refusal(&orbit, &body()).as_deref(),
            Some("orbit-1's number of turns is -2. It cannot be below 0")
        );
    }

    #[test]
    fn a_way_round_is_the_same_remembered_or_found_afresh() {
        // A round pond of 60 corners and a shed beside it, and 80 lanes
        // through them: the legs between the points round an obstacle are
        // remembered from one lane's way round to the next.
        let zones = || {
            let pond: Vec<Point> = (0..60)
                .map(|i| {
                    let a = f64::from(i) / 60.0 * std::f64::consts::TAU;
                    (20.0 * a.cos(), 20.0 * a.sin())
                })
                .collect();
            let shed = vec![(24.0, -6.0), (34.0, -6.0), (34.0, 6.0), (24.0, 6.0)];
            vec![
                Keep::new(Obstacle::Polygon(pond), 3.0),
                Keep::new(Obstacle::Polygon(shed), 3.0),
                Keep::new(Obstacle::Circle((0.0, 40.0), 5.0), 3.0),
            ]
        };
        let remembered = zones();
        let mut round = 0;
        for lane in 0..80 {
            let v = -30.0 + f64::from(lane) * 0.9;
            for (a, b) in [((-60.0, v), (60.0, v)), ((v, -60.0), (v + 7.0, 70.0))] {
                let way = way_round(a, b, &remembered, 3.0);
                assert_eq!(way, way_round(a, b, &zones(), 3.0), "from {a:?} to {b:?}");
                round += usize::from(way.is_some_and(|w| !w.is_empty()));
            }
        }
        assert!(round > 60, "{round} legs went round");
    }

    #[test]
    fn a_line_meets_the_same_edges_swept_as_looked_at_one_by_one() {
        // A star of 400 corners: concave, and most of its edges are far
        // from any one line.
        let n = 400;
        let star: Vec<Point> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let r = if i % 2 == 0 { 100.0 } else { 60.0 };
                (r * a.cos(), r * a.sin())
            })
            .collect();
        let all: Vec<usize> = (0..n).collect();
        for reach in [0.0, 3.0] {
            // Lines every 0.37 m, and through and a reach either side of
            // every corner, where an edge starts or stops being met.
            let mut lines: Vec<f64> = (0..600).map(|k| -111.0 + f64::from(k) * 0.37).collect();
            for corner in &star {
                lines.extend([corner.1 - reach, corner.1, corner.1 + reach]);
            }
            lines.sort_by(f64::total_cmp);
            let mut sweep = Sweep::new(&star, reach);
            let mut fewest = n;
            for v in lines {
                let met = sweep.at(v).to_vec();
                fewest = fewest.min(met.len());
                assert_eq!(
                    inside_on_line(&star, &met, v),
                    inside_on_line(&star, &all, v),
                    "inside at {v}"
                );
                let near = |edges: &[usize]| -> Vec<(f64, f64)> {
                    edges
                        .iter()
                        .filter_map(|&i| near_segment_on_line(star[i], star[(i + 1) % n], reach, v))
                        .collect()
                };
                assert_eq!(near(&met), near(&all), "near at {v}");
            }
            assert!(fewest < n / 10, "a line looks at {fewest} of {n} edges");
        }
    }

    /// The angle a survey set to the shortest is flown at, found the plain
    /// way: every angle's lanes made and measured.
    fn shortest_by_flying_every_angle(survey: &Survey<'_>, keep: &[Keep]) -> f64 {
        let n = survey.area.len();
        let edges = (0..n).map(|i| {
            let (dx, dy) = sub(survey.area[(i + 1) % n], survey.area[i]);
            (dx.atan2(dy).to_degrees().rem_euclid(180.0), false)
        });
        edges
            .chain((0..180).map(|degree| (f64::from(degree), true)))
            .map(|(angle, slant)| {
                let across = Lines::new(
                    survey.area,
                    survey.spacing_m,
                    angle,
                    survey.margin_m,
                    keep,
                    survey.snap,
                );
                let flown = length(&survey.lanes(angle, keep));
                (angle, (across.scan.len(), slant), flown)
            })
            .filter(|(_, _, flown)| *flown > 0.0)
            .min_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)))
            .map_or(0.0, |(angle, _, _)| angle)
    }

    #[test]
    fn the_shortest_angle_is_the_one_flying_every_angle_finds() {
        let (pond, _) = field_with_a_pond(0.0);
        let local = Local::around(&field()).unwrap();
        let shapes: [Vec<Point>; 4] = [
            field().iter().map(|&p| local.to(p)).collect(),
            // A strip at a slant, a triangle, and an L.
            vec![
                (-180.0, -90.0),
                (170.0, 110.0),
                (150.0, 145.0),
                (-200.0, -55.0),
            ],
            vec![(0.0, 0.0), (310.0, 40.0), (120.0, 260.0)],
            vec![
                (0.0, 0.0),
                (300.0, 0.0),
                (300.0, 80.0),
                (90.0, 80.0),
                (90.0, 240.0),
                (0.0, 240.0),
            ],
        ];
        for area in &shapes {
            for (spacing_m, margin_m, crosshatch, start, zones) in [
                (20.0, 0.0, false, None, false),
                (7.0, 6.0, false, Some((0.0, 0.0)), true),
                (33.0, 0.0, true, None, true),
                (2.5, 3.0, true, Some((300.0, 240.0)), false),
            ] {
                let survey = Survey {
                    area,
                    spacing_m,
                    turnaround_m: 5.0,
                    margin_m,
                    crosshatch,
                    start,
                    snap: true,
                };
                let keep = if zones {
                    obstacles(&pond, &local, margin_m.max(MIN_CLEAR_M))
                } else {
                    Vec::new()
                };
                assert_eq!(
                    survey.shortest_angle(&keep),
                    Ok(shortest_by_flying_every_angle(&survey, &keep)),
                    "{area:?} at {spacing_m} m"
                );
            }
        }
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
            refusal(&turning, &body)
                .is_some_and(|why| why.contains("no way round an exclusion zone"))
        );
        // Without the turn past the edge the lanes keep clear of it.
        assert!(!items(&pattern, &body).is_empty());
        assert_eq!(refusal(&pattern, &body), None);
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
        assert_eq!(refusal(&flat_area, &body), None);
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

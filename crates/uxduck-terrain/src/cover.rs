//! Which chunks cover an area or a mission (S120, ADR-0026 T6): areas kept
//! offline are polygons (ADR-0051 A5), covered by every chunk they touch;
//! a mission needs every chunk within 1 km of any leg, sampled along each
//! leg, not only at its waypoints, so a long leg over unseen ground is
//! covered.

use std::collections::BTreeSet;

use crate::dataset::{CHUNKS_PER_SIDE, ChunkKey, Tile};

/// Metres in a degree of latitude, near enough for picking chunks.
const METRES_PER_DEGREE: f64 = 111_320.0;
/// Samples along a leg, at most this far apart: well under a chunk's
/// eighth of a degree (about 14 km).
const SAMPLE_M: f64 = 250.0;
/// A mission's legs are covered this far to either side (T6).
pub const LEG_BUFFER_M: f64 = 1000.0;
/// No area or mission asks for more chunks than this (about 6° square).
pub const MAX_CHUNKS: usize = 2500;

/// A rectangle in degrees, west to east without crossing the antimeridian.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

impl Bounds {
    /// Whether it is a rectangle on the globe.
    #[must_use]
    pub fn valid(&self) -> bool {
        (-90.0..=90.0).contains(&self.south)
            && (-90.0..=90.0).contains(&self.north)
            && (-180.0..=180.0).contains(&self.west)
            && (-180.0..=180.0).contains(&self.east)
            && self.south < self.north
            && self.west < self.east
    }

    /// A square `side_m` across, centred on a point.
    #[must_use]
    pub fn around(lat: f64, lon: f64, side_m: f64) -> Self {
        let half_lat = side_m / 2.0 / METRES_PER_DEGREE;
        let half_lon = half_lat / lat.to_radians().cos().max(0.01);
        Self {
            south: (lat - half_lat).max(-90.0),
            north: (lat + half_lat).min(90.0),
            west: (lon - half_lon).max(-180.0),
            east: (lon + half_lon).min(180.0),
        }
    }
}

/// The chunks a rectangle touches.
#[must_use]
pub fn chunks_in(bounds: Bounds) -> BTreeSet<ChunkKey> {
    let mut keys = BTreeSet::new();
    if !bounds.valid() {
        return keys;
    }
    let step = 1.0 / f64::from(CHUNKS_PER_SIDE);
    // Eighths of a degree, south to north and west to east.
    #[allow(clippy::cast_possible_truncation, reason = "within ±1440")]
    let (s, n) = (
        (bounds.south / step).floor() as i32,
        ((bounds.north / step).ceil() as i32 - 1).min(719),
    );
    #[allow(clippy::cast_possible_truncation, reason = "within ±1440")]
    let (w, e) = (
        (bounds.west / step).floor() as i32,
        ((bounds.east / step).ceil() as i32 - 1).min(1439),
    );
    for y in s..=n {
        for x in w..=e {
            keys.insert(chunk(y, x));
            if keys.len() > MAX_CHUNKS {
                return keys;
            }
        }
    }
    keys
}

/// Eighths of a degree round the globe.
const EIGHTHS_AROUND: i32 = 2880;
/// A polygon's bounding box is looked through only this many chunks.
const MAX_BOX_CHUNKS: i64 = 160_000;
/// No area has more corners than this.
pub const MAX_POINTS: usize = 500;

/// Why a polygon cannot be an area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolygonProblem {
    /// Fewer than three corners, or more than `MAX_POINTS`.
    Corners,
    /// A corner outside ±90° and ±180°, or not a number.
    OutOfRange,
    /// It goes all the way round the globe, as round a pole.
    AroundTheGlobe,
    /// It touches more than `MAX_CHUNKS`.
    TooLarge,
}

/// A polygon on the globe: its corners in order, as (latitude, longitude),
/// the last joined to the first. Each edge is the shorter way between its
/// corners, so one across the antimeridian goes across it; its longitudes
/// are kept unwrapped, continuous from the first corner.
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    points: Vec<(f64, f64)>,
}

impl Polygon {
    /// # Errors
    /// When the corners do not make an area on the globe.
    pub fn new(corners: &[(f64, f64)]) -> Result<Self, PolygonProblem> {
        if corners.len() < 3 || corners.len() > MAX_POINTS {
            return Err(PolygonProblem::Corners);
        }
        if corners
            .iter()
            .any(|(lat, lon)| !(-90.0..=90.0).contains(lat) || !(-180.0..=180.0).contains(lon))
        {
            return Err(PolygonProblem::OutOfRange);
        }
        let shorter = |delta: f64| delta - 360.0 * (delta / 360.0).round();
        let mut points: Vec<(f64, f64)> = Vec::with_capacity(corners.len());
        for &(lat, lon) in corners {
            let lon = match points.last() {
                Some(&(_, last)) => last + shorter(lon - last),
                None => lon,
            };
            points.push((lat, lon));
        }
        let (first, last) = (points[0].1, points[points.len() - 1].1);
        if (last + shorter(first - last) - first).abs() > 1.0 {
            return Err(PolygonProblem::AroundTheGlobe);
        }
        Ok(Self { points })
    }

    /// The corners, longitudes unwrapped.
    #[must_use]
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }

    fn edges(&self) -> impl Iterator<Item = ((f64, f64), (f64, f64))> + '_ {
        self.points
            .iter()
            .copied()
            .zip(self.points.iter().copied().cycle().skip(1))
    }

    /// Whether a point, its longitude on the polygon's own unwrapped
    /// range, is inside (even-odd, so a concave or crossed polygon too).
    fn contains(&self, lat: f64, lon: f64) -> bool {
        let mut inside = false;
        for ((lat0, lon0), (lat1, lon1)) in self.edges() {
            if (lat0 > lat) != (lat1 > lat)
                && lon < lon0 + (lat - lat0) / (lat1 - lat0) * (lon1 - lon0)
            {
                inside = !inside;
            }
        }
        inside
    }
}

/// Whether a segment passes through the inside of a box, south-west to
/// north-east (Liang–Barsky): running along its edge is not, so an area
/// drawn on a chunk's edge does not take the chunk beside it.
fn crosses_box(
    from: (f64, f64),
    to: (f64, f64),
    south_west: (f64, f64),
    north_east: (f64, f64),
) -> bool {
    let (d_lat, d_lon) = (to.0 - from.0, to.1 - from.1);
    let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-d_lat, from.0 - south_west.0),
        (d_lat, north_east.0 - from.0),
        (-d_lon, from.1 - south_west.1),
        (d_lon, north_east.1 - from.1),
    ] {
        if p == 0.0 {
            if q <= 0.0 {
                return false;
            }
        } else if p < 0.0 {
            enter = enter.max(q / p);
        } else {
            leave = leave.min(q / p);
        }
    }
    enter < leave
}

/// The chunks a polygon touches: under it, or crossed by its edges.
///
/// # Errors
/// `TooLarge` past `MAX_CHUNKS`.
pub fn chunks_under(polygon: &Polygon) -> Result<BTreeSet<ChunkKey>, PolygonProblem> {
    let step = 1.0 / f64::from(CHUNKS_PER_SIDE);
    let (mut south, mut west) = (f64::INFINITY, f64::INFINITY);
    let (mut north, mut east) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for &(lat, lon) in polygon.points() {
        (south, north) = (south.min(lat), north.max(lat));
        (west, east) = (west.min(lon), east.max(lon));
    }
    // Eighths of a degree, south to north and west to east.
    #[allow(clippy::cast_possible_truncation, reason = "within ±2880")]
    let (rows, cols) = (
        ((south / step).floor() as i32).max(-720)..=((north / step).floor() as i32).min(719),
        (west / step).floor() as i32..=(east / step).floor() as i32,
    );
    let height = i64::from(rows.end() - rows.start() + 1);
    if height * i64::from(cols.end() - cols.start() + 1) > MAX_BOX_CHUNKS {
        return Err(PolygonProblem::TooLarge);
    }
    let mut keys = BTreeSet::new();
    for y in rows {
        for x in cols.clone() {
            let south_west = (f64::from(y) * step, f64::from(x) * step);
            let north_east = (south_west.0 + step, south_west.1 + step);
            let touches = polygon
                .edges()
                .any(|(a, b)| crosses_box(a, b, south_west, north_east))
                || polygon.contains(south_west.0 + step / 2.0, south_west.1 + step / 2.0);
            if !touches {
                continue;
            }
            let x = (x + EIGHTHS_AROUND / 2).rem_euclid(EIGHTHS_AROUND) - EIGHTHS_AROUND / 2;
            keys.insert(chunk(y, x));
            if keys.len() > MAX_CHUNKS {
                return Err(PolygonProblem::TooLarge);
            }
        }
    }
    Ok(keys)
}

/// The chunk at eighths of a degree `y` north and `x` east.
fn chunk(y: i32, x: i32) -> ChunkKey {
    let per = i32::from(CHUNKS_PER_SIDE);
    #[allow(clippy::cast_possible_truncation, reason = "whole degrees")]
    let tile = Tile {
        lat: y.div_euclid(per) as i16,
        lon: x.div_euclid(per) as i16,
    };
    // Row 0 is the tile's north edge.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0 to 7"
    )]
    ChunkKey {
        tile,
        row: (per - 1 - y.rem_euclid(per)) as u8,
        col: x.rem_euclid(per) as u8,
    }
}

/// The chunks within `buffer_m` of any leg between the points, sampled
/// along each leg.
#[must_use]
pub fn along_legs(points: &[(f64, f64)], buffer_m: f64) -> BTreeSet<ChunkKey> {
    let mut keys = BTreeSet::new();
    let mut add = |lat: f64, lon: f64| {
        keys.extend(chunks_in(Bounds::around(lat, lon, 2.0 * buffer_m)));
    };
    for leg in points.windows(2) {
        let ((lat0, lon0), (lat1, lon1)) = (leg[0], leg[1]);
        let north = (lat1 - lat0) * METRES_PER_DEGREE;
        let east = (lon1 - lon0) * METRES_PER_DEGREE * f64::midpoint(lat0, lat1).to_radians().cos();
        let length = north.hypot(east);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a leg's samples"
        )]
        let steps = ((length / SAMPLE_M).ceil() as usize).max(1);
        for i in 0..=steps {
            #[allow(clippy::cast_precision_loss, reason = "a leg's samples")]
            let t = i as f64 / steps as f64;
            add(lat0 + (lat1 - lat0) * t, lon0 + (lon1 - lon0) * t);
        }
    }
    if let [(lat, lon)] = points {
        add(*lat, *lon);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(keys: &BTreeSet<ChunkKey>) -> Vec<String> {
        let mut names: Vec<String> = keys.iter().map(|k| k.file()).collect();
        names.sort();
        names
    }

    #[test]
    fn a_rectangle_takes_the_chunks_it_touches() {
        // Inside one chunk: N53W005's row 7 (south), column 7 (east).
        let one = chunks_in(Bounds {
            south: 53.01,
            west: -4.1,
            north: 53.1,
            east: -4.01,
        });
        assert_eq!(names(&one), ["N53W005/77.uxdt"]);
        // Across a tile's corner: four chunks of four tiles.
        let four = chunks_in(Bounds {
            south: 52.99,
            west: -4.01,
            north: 53.01,
            east: -3.99,
        });
        assert_eq!(
            names(&four),
            [
                "N52W004/00.uxdt",
                "N52W005/07.uxdt",
                "N53W004/70.uxdt",
                "N53W005/77.uxdt"
            ]
        );
        assert!(
            chunks_in(Bounds {
                south: 1.0,
                west: 0.0,
                north: 0.0,
                east: 1.0
            })
            .is_empty()
        );
    }

    #[test]
    fn a_long_leg_is_covered_between_its_ends() {
        // 20 km due east at 53° N: about 0.3°, so chunks between the ends.
        let keys = along_legs(&[(53.05, -4.4), (53.05, -4.1)], LEG_BUFFER_M);
        let cols: BTreeSet<u8> = keys.iter().filter(|k| k.row == 7).map(|k| k.col).collect();
        assert_eq!(cols, [4, 5, 6, 7].into(), "{:?}", names(&keys));
        // Only the waypoints would miss columns 5 and 6.
        let ends = chunks_in(Bounds::around(53.05, -4.4, 2000.0))
            .union(&chunks_in(Bounds::around(53.05, -4.1, 2000.0)))
            .count();
        assert!(keys.len() > ends);
    }

    fn polygon(corners: &[(f64, f64)]) -> BTreeSet<ChunkKey> {
        chunks_under(&Polygon::new(corners).unwrap()).unwrap()
    }

    #[test]
    fn a_rectangle_as_a_polygon_takes_what_the_rectangle_did() {
        let tile = polygon(&[(53.0, -5.0), (53.0, -4.0), (54.0, -4.0), (54.0, -5.0)]);
        let rectangle = chunks_in(Bounds {
            south: 53.0,
            west: -5.0,
            north: 54.0,
            east: -4.0,
        });
        assert_eq!(tile.len(), 64);
        assert_eq!(tile, rectangle);
        // Inside one chunk, and across a tile's corner, as above.
        let one = polygon(&[(53.01, -4.1), (53.01, -4.01), (53.1, -4.05)]);
        assert_eq!(names(&one), ["N53W005/77.uxdt"]);
        let four = polygon(&[
            (52.99, -4.01),
            (52.99, -3.99),
            (53.01, -3.99),
            (53.01, -4.01),
        ]);
        assert_eq!(four.len(), 4);
    }

    #[test]
    fn a_concave_polygon_leaves_out_the_chunks_in_its_notch() {
        // A U over N53W005, a degree across: its notch, the middle half of
        // the tile's columns over its northern three quarters, is open.
        let u = polygon(&[
            (53.0, -5.0),
            (53.0, -4.0),
            (54.0, -4.0),
            (54.0, -4.25),
            (53.25, -4.25),
            (53.25, -4.75),
            (54.0, -4.75),
            (54.0, -5.0),
        ]);
        // Columns 2 to 5 of rows 0 to 5 are in the notch: 24 chunks out.
        assert_eq!(u.len(), 64 - 24, "{:?}", names(&u));
        assert!(u.iter().all(|k| k.row >= 6 || k.col < 2 || k.col > 5));
        // A thin diagonal: only the chunks it crosses, not its box's.
        let line = polygon(&[(53.0, -5.0), (53.01, -5.0), (54.0, -4.01), (53.99, -4.0)]);
        assert!(line.len() < 64 / 2, "{}", line.len());
        assert!(line.len() >= 8, "{}", line.len());
    }

    #[test]
    fn a_polygon_across_the_antimeridian_goes_the_short_way() {
        // Fiji's Taveuni, from 179.8° E to 179.8° W: the chunks either side,
        // not the 359° the other way round.
        let keys = polygon(&[
            (-16.9, 179.8),
            (-16.9, -179.8),
            (-16.7, -179.8),
            (-16.7, 179.8),
        ]);
        let mut lons: Vec<i16> = keys.iter().map(|k| k.tile.lon).collect();
        lons.dedup();
        assert_eq!(lons, [-180, 179], "{:?}", names(&keys));
        // Three eighths of a degree south to north, two either side.
        assert_eq!(keys.len(), 12, "{:?}", names(&keys));
        assert_eq!(
            Polygon::new(&[(80.0, -120.0), (80.0, 0.0), (80.0, 120.0)]),
            Err(PolygonProblem::AroundTheGlobe)
        );
    }

    #[test]
    fn a_polygon_needs_three_corners_on_the_globe_and_a_size_to_keep() {
        assert_eq!(
            Polygon::new(&[(1.0, 1.0), (2.0, 2.0)]),
            Err(PolygonProblem::Corners)
        );
        assert_eq!(
            Polygon::new(&[(1.0, 1.0), (91.0, 2.0), (1.0, 3.0)]),
            Err(PolygonProblem::OutOfRange)
        );
        assert_eq!(
            Polygon::new(&[(1.0, f64::NAN), (2.0, 2.0), (1.0, 3.0)]),
            Err(PolygonProblem::OutOfRange)
        );
        let europe = Polygon::new(&[(35.0, -10.0), (35.0, 30.0), (60.0, 30.0), (60.0, -10.0)]);
        assert_eq!(
            chunks_under(&europe.unwrap()),
            Err(PolygonProblem::TooLarge)
        );
    }

    #[test]
    fn a_square_around_a_point_is_as_wide_as_asked() {
        let b = Bounds::around(53.0, -4.0, 10_000.0);
        assert!(((b.north - b.south) * METRES_PER_DEGREE - 10_000.0).abs() < 1.0);
        let wide = (b.east - b.west) * METRES_PER_DEGREE * 53.0f64.to_radians().cos();
        assert!((wide - 10_000.0).abs() < 1.0);
    }
}

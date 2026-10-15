//! Which chunks cover an area or a mission (S120, ADR-0026 T6): areas kept
//! offline are rectangles; a mission needs every chunk within 1 km of any
//! leg, sampled along each leg, not only at its waypoints, so a long leg
//! over unseen ground is covered.

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
            let key = ChunkKey {
                tile,
                row: (per - 1 - y.rem_euclid(per)) as u8,
                col: x.rem_euclid(per) as u8,
            };
            keys.insert(key);
            if keys.len() > MAX_CHUNKS {
                return keys;
            }
        }
    }
    keys
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

    #[test]
    fn a_square_around_a_point_is_as_wide_as_asked() {
        let b = Bounds::around(53.0, -4.0, 10_000.0);
        assert!(((b.north - b.south) * METRES_PER_DEGREE - 10_000.0).abs() < 1.0);
        let wide = (b.east - b.west) * METRES_PER_DEGREE * 53.0f64.to_radians().cos();
        assert!((wide - 10_000.0).abs() < 1.0);
    }
}

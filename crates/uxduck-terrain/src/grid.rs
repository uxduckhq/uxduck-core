//! ArduPilot's terrain grid (`AP_Terrain`, `Location`), ported so UXDuck
//! puts heights where the vehicle expects them (T3).
//!
//! The vehicle keeps blocks of 28 × 32 points, `grid_spacing` metres
//! apart, whose south-west corners step from each whole degree by 24 and
//! 28 points (one point of overlap). A block travels as 56 groups of 4 × 4
//! points; a group's bit (`gridbit`) counts east first, then north. `x` is
//! north and `y` east throughout, as in ArduPilot.
//!
//! ArduPilot's own maths uses `float` on flight controllers and `double` in
//! SITL; the two differ by a centimetre at most. This port works in `f64`
//! where `offset` does and in `f32` where `get_distance_NE` does, and its
//! tests compare it with both (`tests/ap_grid.rs`).

/// Points along each side of a group sent in one `TERRAIN_DATA`.
pub const GROUP: u8 = 4;
/// Groups to the north (`x`) and to the east (`y`) in a block.
pub const GROUPS_X: u8 = 7;
pub const GROUPS_Y: u8 = 8;
/// Points in a block: 28 north by 32 east.
pub const BLOCK_X: u8 = GROUP * GROUPS_X;
pub const BLOCK_Y: u8 = GROUP * GROUPS_Y;
/// Points between one block's corner and the next.
pub const SPACING_X: u8 = (GROUPS_X - 1) * GROUP;
pub const SPACING_Y: u8 = (GROUPS_Y - 1) * GROUP;
/// Every group of a block: the mask of a request for all of it.
pub const ALL_GROUPS: u64 = (1 << (GROUPS_X * GROUPS_Y)) - 1;

/// `LATLON_TO_M` and its inverse, as the `float` constants ArduPilot keeps.
#[allow(
    clippy::excessive_precision,
    reason = "ArduPilot's constants as written"
)]
const SCALING: f32 = 0.011_131_884_502_145_034;
#[allow(
    clippy::excessive_precision,
    reason = "ArduPilot's constants as written"
)]
const SCALING_INV: f32 = 89.832_049_533_689_22;

/// `Location::longitude_scale`: how much shorter a degree of longitude is
/// than one of latitude here.
#[must_use]
pub fn longitude_scale(lat_e7: i32) -> f64 {
    let scale = (f64::from(lat_e7) * (1.0e-7 * (std::f64::consts::PI / 180.0))).cos();
    scale.max(0.01)
}

/// `Location::diff_longitude`: `a - b`, wrapping at ±180°.
#[must_use]
pub fn diff_longitude(a: i32, b: i32) -> i32 {
    if (a < 0) == (b < 0) {
        return a.wrapping_sub(b);
    }
    let mut d = i64::from(a) - i64::from(b);
    if d > 1_800_000_000 {
        d -= 3_600_000_000;
    } else if d < -1_800_000_000 {
        d += 3_600_000_000;
    }
    #[allow(clippy::cast_possible_truncation, reason = "within ±180°")]
    let d = d as i32;
    d
}

/// `Location::wrap_longitude`.
#[must_use]
pub fn wrap_longitude(lon: i64) -> i32 {
    let lon = if lon > 1_800_000_000 {
        lon - 3_600_000_000
    } else if lon < -1_800_000_000 {
        lon + 3_600_000_000
    } else {
        lon
    };
    #[allow(clippy::cast_possible_truncation, reason = "as ArduPilot")]
    let lon = lon as i32;
    lon
}

/// `Location::limit_lattitude`: over a pole and back down.
#[must_use]
pub fn limit_latitude(lat: i32) -> i32 {
    if lat > 900_000_000 {
        1_800_000_000 - lat
    } else if lat < -900_000_000 {
        -(1_800_000_000 + lat)
    } else {
        lat
    }
}

/// `Location::offset`: a point moved north and east by metres.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "C's truncation, as ArduPilot"
)]
pub fn offset(lat_e7: i32, lon_e7: i32, north_m: f64, east_m: f64) -> (i32, i32) {
    let dlat = (north_m * f64::from(SCALING_INV)) as i32;
    let dlon = ((east_m * f64::from(SCALING_INV)) / longitude_scale(lat_e7 + dlat / 2)) as i64;
    let lat = limit_latitude(lat_e7.saturating_add(dlat));
    (lat, wrap_longitude(dlon + i64::from(lon_e7)))
}

/// `Location::get_distance_NE`: metres north and east from the first point
/// to the second, in `float` as ArduPilot keeps it.
#[must_use]
#[allow(clippy::cast_possible_truncation, reason = "ArduPilot's Vector2f")]
#[allow(clippy::cast_precision_loss, reason = "ArduPilot's float")]
pub fn distance_ne(lat: i32, lon: i32, lat2: i32, lon2: i32) -> (f32, f32) {
    let north = (lat2.wrapping_sub(lat)) as f32 * SCALING;
    let mid = i32::try_from(i64::midpoint(i64::from(lat), i64::from(lat2))).unwrap_or(lat);
    let east =
        (f64::from(diff_longitude(lon2, lon) as f32 * SCALING) * longitude_scale(mid)) as f32;
    (north, east)
}

/// Where a point falls in the vehicle's grid (`calculate_grid_info`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridInfo {
    /// The whole degree the grid steps from (south-west).
    pub lat_degrees: i16,
    pub lon_degrees: i16,
    /// The block's south-west corner, 1e-7 degrees.
    pub grid_lat: i32,
    pub grid_lon: i32,
    /// Which block from the degree: north, east.
    pub grid_idx_x: u32,
    pub grid_idx_y: u32,
    /// The point's square in the block: north, east.
    pub idx_x: u8,
    pub idx_y: u8,
    /// How far across that square, 0 to 1.
    pub frac_x: f32,
    pub frac_y: f32,
}

/// `AP_Terrain::calculate_grid_info`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "C's conversions, as ArduPilot"
)]
#[allow(clippy::cast_precision_loss, reason = "ArduPilot's float")]
pub fn grid_info(lat_e7: i32, lon_e7: i32, spacing: u16) -> GridInfo {
    let degrees = |v: i32| -> i32 {
        if v < 0 {
            (v - 9_999_999) / 10_000_000
        } else {
            v / 10_000_000
        }
    };
    let lat_degrees = degrees(lat_e7);
    let lon_degrees = degrees(lon_e7);
    let ref_lat = lat_degrees * 10_000_000;
    let ref_lon = lon_degrees * 10_000_000;
    let (north, east) = distance_ne(ref_lat, ref_lon, lat_e7, lon_e7);
    let spacing_f = f32::from(spacing);
    let idx_x = (north / spacing_f) as u32;
    let idx_y = (east / spacing_f) as u32;
    let grid_idx_x = idx_x / u32::from(SPACING_X);
    let grid_idx_y = idx_y / u32::from(SPACING_Y);
    let frac_x = (north - idx_x as f32 * spacing_f) / spacing_f;
    let frac_y = (east - idx_y as f32 * spacing_f) / spacing_f;
    let (grid_lat, grid_lon) = offset(
        ref_lat,
        ref_lon,
        f64::from(grid_idx_x as f32 * f32::from(SPACING_X) * spacing_f),
        f64::from(grid_idx_y as f32 * f32::from(SPACING_Y) * spacing_f),
    );
    GridInfo {
        lat_degrees: lat_degrees as i16,
        lon_degrees: lon_degrees as i16,
        grid_lat,
        grid_lon,
        grid_idx_x,
        grid_idx_y,
        idx_x: (idx_x % u32::from(SPACING_X)) as u8,
        idx_y: (idx_y % u32::from(SPACING_Y)) as u8,
        frac_x,
        frac_y,
    }
}

/// `AP_Terrain::grid_bitnum`: the group a point of a block is in.
#[must_use]
pub fn gridbit(idx_x: u8, idx_y: u8) -> u8 {
    (idx_y / GROUP) + GROUPS_Y * (idx_x / GROUP)
}

/// The first point of a group: north, east (`handle_terrain_data`).
#[must_use]
pub fn group_origin(gridbit: u8) -> (u8, u8) {
    ((gridbit / GROUPS_Y) * GROUP, (gridbit % GROUPS_Y) * GROUP)
}

/// Where the 16 heights of a group belong, in `TERRAIN_DATA`'s order
/// (`data[x * 4 + y]`, `x` north): each point moved from the block's
/// corner by whole spacings, as `create_terrain.py` fills a block.
#[must_use]
pub fn group_points(block_lat: i32, block_lon: i32, spacing: u16, gridbit: u8) -> [(i32, i32); 16] {
    let (x0, y0) = group_origin(gridbit);
    let mut points = [(0, 0); 16];
    for x in 0..GROUP {
        for y in 0..GROUP {
            let north = f64::from(x0 + x) * f64::from(spacing);
            let east = f64::from(y0 + y) * f64::from(spacing);
            points[usize::from(x * GROUP + y)] = offset(block_lat, block_lon, north, east);
        }
    }
    points
}

/// `AP_Terrain::height_amsl`'s interpolation of the four points around a
/// spot, given them as `[h00, h01, h10, h11]` (`h10` one north of `h00`).
#[must_use]
pub fn interpolate(info: &GridInfo, heights: [f32; 4]) -> f32 {
    let [h00, h01, h10, h11] = heights;
    let avg1 = (1.0 - info.frac_x) * h00 + info.frac_x * h10;
    let avg2 = (1.0 - info.frac_x) * h01 + info.frac_x * h11;
    (1.0 - info.frac_y) * avg1 + info.frac_y * avg2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_count_east_then_north() {
        assert_eq!(gridbit(0, 0), 0);
        assert_eq!(gridbit(0, 4), 1);
        assert_eq!(gridbit(4, 0), 8);
        assert_eq!(gridbit(27, 31), 55);
        assert_eq!(group_origin(55), (24, 28));
        assert_eq!(group_origin(9), (4, 4));
        for bit in 0..56 {
            let (x, y) = group_origin(bit);
            assert_eq!(gridbit(x, y), bit);
        }
        assert_eq!(ALL_GROUPS.count_ones(), 56);
        assert_eq!((BLOCK_X, BLOCK_Y, SPACING_X, SPACING_Y), (28, 32, 24, 28));
    }

    #[test]
    fn longitude_wraps_and_latitude_folds() {
        assert_eq!(diff_longitude(-1_799_999_999, 1_799_999_999), 2);
        assert_eq!(wrap_longitude(1_800_000_001), -1_799_999_999);
        assert_eq!(limit_latitude(900_000_010), 899_999_990);
    }

    #[test]
    fn a_group_starts_at_the_block_corner() {
        let points = group_points(530_646_790, -40_798_165, 100, 0);
        assert_eq!(points[0], (530_646_790, -40_798_165));
        // One spacing east, then one north.
        assert!(points[1].1 > points[0].1 && points[1].0 == points[0].0);
        assert!(points[4].0 > points[0].0 && points[4].1 == points[0].1);
    }
}

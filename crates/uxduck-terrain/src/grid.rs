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
    let (lat_degrees, lon_degrees) = (lat_degrees as i16, lon_degrees as i16);
    let (grid_lat, grid_lon) =
        block_corner(lat_degrees, lon_degrees, grid_idx_x, grid_idx_y, spacing);
    GridInfo {
        lat_degrees,
        lon_degrees,
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

/// The south-west corner of a block from its degree and indices, as
/// `calculate_grid_info` finds it.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "ArduPilot's float")]
pub fn block_corner(
    lat_degrees: i16,
    lon_degrees: i16,
    grid_idx_x: u32,
    grid_idx_y: u32,
    spacing: u16,
) -> (i32, i32) {
    let spacing = f32::from(spacing);
    offset(
        i32::from(lat_degrees) * 10_000_000,
        i32::from(lon_degrees) * 10_000_000,
        f64::from(grid_idx_x as f32 * f32::from(SPACING_X) * spacing),
        f64::from(grid_idx_y as f32 * f32::from(SPACING_Y) * spacing),
    )
}

/// Where a point of a block is to ArduPilot when it reads the block's
/// heights back: whole spacings north and east of the degree's corner, the
/// frame `calculate_grid_info` measures in, as `terrain_gen.py` fills a
/// block. `x` north and `y` east within the block.
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "a point's parts, as ArduPilot keeps them"
)]
pub fn block_point(
    lat_degrees: i16,
    lon_degrees: i16,
    grid_idx_x: u32,
    grid_idx_y: u32,
    x: u8,
    y: u8,
    spacing: u16,
) -> (i32, i32) {
    let spacing = f64::from(spacing);
    offset(
        i32::from(lat_degrees) * 10_000_000,
        i32::from(lon_degrees) * 10_000_000,
        f64::from(grid_idx_x * u32::from(SPACING_X) + u32::from(x)) * spacing,
        f64::from(grid_idx_y * u32::from(SPACING_Y) + u32::from(y)) * spacing,
    )
}

/// `AP_Terrain::east_blocks`: blocks per row of a degree's file, the
/// degree's width in blocks and two more.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "C's conversion, as ArduPilot"
)]
#[allow(clippy::cast_precision_loss, reason = "ArduPilot's float")]
pub fn east_blocks(lat_degrees: i16, lon_degrees: i16, spacing: u16) -> u32 {
    let lat = i32::from(lat_degrees) * 10_000_000;
    let lon = i32::from(lon_degrees) * 10_000_000;
    let (lat2, lon2) = offset(
        lat,
        (i32::from(lon_degrees) + 1) * 10_000_000,
        0.0,
        f64::from(2 * u32::from(spacing) * u32::from(BLOCK_Y)),
    );
    let (_, east) = distance_ne(lat, lon, lat2, lon2);
    (east / (u32::from(spacing) * u32::from(SPACING_Y)) as f32) as u32
}

/// A block's degree and indices from the corner a vehicle names it by:
/// `(lat_degrees, lon_degrees, grid_idx_x, grid_idx_y)`. A row's last
/// block may start just past its degree's east edge and still belong to
/// it, so the degree to the west is tried too; the one whose corner comes
/// back nearest the vehicle's is taken.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "block indices"
)]
pub fn block_place(block_lat: i32, block_lon: i32, spacing: u16) -> (i16, i16, u32, u32) {
    let info = grid_info(block_lat, block_lon, spacing);
    let west = if info.lon_degrees <= -180 {
        179
    } else {
        info.lon_degrees - 1
    };
    let size_x = f32::from(spacing) * f32::from(SPACING_X);
    let size_y = f32::from(spacing) * f32::from(SPACING_Y);
    [info.lon_degrees, west]
        .into_iter()
        .map(|lon_degrees| {
            let (north, east) = distance_ne(
                i32::from(info.lat_degrees) * 10_000_000,
                i32::from(lon_degrees) * 10_000_000,
                block_lat,
                block_lon,
            );
            let x = (north / size_x).round().max(0.0) as u32;
            let y = (east / size_y).round().max(0.0) as u32;
            let (lat, lon) = block_corner(info.lat_degrees, lon_degrees, x, y, spacing);
            let off = i64::from(lat.abs_diff(block_lat))
                + i64::from(diff_longitude(lon, block_lon).unsigned_abs());
            (off, (info.lat_degrees, lon_degrees, x, y))
        })
        .min_by_key(|&(off, _)| off)
        .map_or((info.lat_degrees, info.lon_degrees, 0, 0), |(_, place)| {
            place
        })
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
/// (`data[x * 4 + y]`, `x` north): where the vehicle reads them back,
/// [`block_point`], for the block it named by its corner.
#[must_use]
pub fn group_points(block_lat: i32, block_lon: i32, spacing: u16, gridbit: u8) -> [(i32, i32); 16] {
    let (lat_degrees, lon_degrees, grid_idx_x, grid_idx_y) =
        block_place(block_lat, block_lon, spacing);
    let (x0, y0) = group_origin(gridbit);
    let mut points = [(0, 0); 16];
    for x in 0..GROUP {
        for y in 0..GROUP {
            points[usize::from(x * GROUP + y)] = block_point(
                lat_degrees,
                lon_degrees,
                grid_idx_x,
                grid_idx_y,
                x0 + x,
                y0 + y,
                spacing,
            );
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
        // One spacing east, then one north, each measured from the
        // degree's corner as ArduPilot measures.
        let degree = (530_000_000, -50_000_000);
        let (n0, e0) = distance_ne(degree.0, degree.1, points[0].0, points[0].1);
        let (n1, e1) = distance_ne(degree.0, degree.1, points[1].0, points[1].1);
        let (n4, e4) = distance_ne(degree.0, degree.1, points[4].0, points[4].1);
        assert!(points[1].0 == points[0].0 && (e1 - e0 - 100.0).abs() < 0.02);
        assert!((n4 - n0 - 100.0).abs() < 0.02 && (e4 - e0).abs() < 0.02);
        assert!((n1 - n0).abs() < 0.02);
    }
}

//! ArduPilot's terrain files on a vehicle's card (ADR-0051 A6): blocks of
//! its grid written whole, so a vehicle reads them instead of asking.
//!
//! ArduPilot keeps one file per whole degree, `/APM/TERRAIN/N47E035.DAT`,
//! of 2048-byte blocks (`union grid_io_block` in `AP_Terrain.h`). A block
//! sits `east_blocks × grid_idx_x + grid_idx_y` blocks into its file.
//! ArduPilot uses a block it reads only if its corner matches (within
//! `TERRAIN_MARGIN`), its spacing is the vehicle's, its `version` is 1,
//! its bitmap is not empty and its CRC holds; a `version_minor` below 1
//! raises a pre-arm warning that the data has expired. Little-endian,
//! packed:
//!
//! | Bytes      | Field                                                   |
//! | ---------- | ------------------------------------------------------- |
//! | 0..8       | `bitmap`: the 56 groups held                            |
//! | 8..16      | `lat`, `lon`: the south-west corner, 1e-7 degrees       |
//! | 16..18     | `crc`: CRC-16/XMODEM of bytes 0..1821 with this zero    |
//! | 18..22     | `version`, `spacing`                                    |
//! | 22..1814   | `height[28][32]`: metres, `x` north                     |
//! | 1814..1821 | `grid_idx_x`, `grid_idx_y`, `lon_degrees`, `lat_degrees` |
//! | 1821       | `version_minor`, outside the CRC                        |
//! | 1822..2048 | zero                                                    |
//!
//! Each height is taken where ArduPilot reads it back from
//! (`grid::block_point`: whole spacings from the degree's corner, as
//! ArduPilot's `terrain_gen.py` places them) and rounded to the nearest
//! metre, as the live answers are (T3). The format and those points are
//! tested against `terrain_gen.py` (`tests/ardupilot_card.rs`).

use std::collections::{BTreeMap, BTreeSet};

use crate::cache::Unavailable;
use crate::grid::{self, ALL_GROUPS, BLOCK_X, BLOCK_Y, SPACING_X, SPACING_Y};
use crate::lookup::{Ground, nearest_metre};

/// Bytes in a block on the card.
pub const BLOCK_BYTES: usize = 2048;
/// Bytes the CRC covers: everything before `version_minor`.
const CRC_BYTES: usize = 1821;
/// `TERRAIN_GRID_FORMAT_VERSION`.
pub const VERSION: u16 = 1;
/// `version_minor`: at least `TERRAIN_VERSION_MINOR_MIN`.
pub const VERSION_MINOR: u8 = 1;
/// Metres in a degree of latitude, to find the degrees near an area.
const METRES_PER_DEGREE: f64 = 111_319.5;

/// A whole degree: the south-west corner of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Degree {
    pub lat: i8,
    pub lon: i16,
}

impl Degree {
    /// The file's name, as `AP_Terrain::open_file` makes it.
    #[must_use]
    pub fn file_name(self) -> String {
        format!(
            "{}{:02}{}{:03}.DAT",
            if self.lat < 0 { 'S' } else { 'N' },
            self.lat.unsigned_abs().min(99),
            if self.lon < 0 { 'W' } else { 'E' },
            self.lon.unsigned_abs().min(999),
        )
    }
}

/// A block of the vehicle's grid as it sits in a file. Blocks of one
/// degree and spacing sort as they lie in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CardBlock {
    pub degree: Degree,
    pub spacing: u16,
    /// Which block from the degree's corner: north, east.
    pub grid_idx_x: u16,
    pub grid_idx_y: u16,
    /// The south-west corner, 1e-7 degrees.
    pub lat: i32,
    pub lon: i32,
}

impl CardBlock {
    /// The block a point is in.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, reason = "ArduPilot's own types")]
    pub fn containing(lat_e7: i32, lon_e7: i32, spacing: u16) -> Self {
        let info = grid::grid_info(lat_e7, lon_e7, spacing);
        Self {
            degree: Degree {
                lat: info.lat_degrees as i8,
                lon: info.lon_degrees,
            },
            spacing,
            grid_idx_x: info.grid_idx_x as u16,
            grid_idx_y: info.grid_idx_y as u16,
            lat: info.grid_lat,
            lon: info.grid_lon,
        }
    }

    /// The block at these indices of a degree.
    #[must_use]
    pub fn at(degree: Degree, spacing: u16, grid_idx_x: u16, grid_idx_y: u16) -> Self {
        let (lat, lon) = grid::block_corner(
            i16::from(degree.lat),
            degree.lon,
            u32::from(grid_idx_x),
            u32::from(grid_idx_y),
            spacing,
        );
        Self {
            degree,
            spacing,
            grid_idx_x,
            grid_idx_y,
            lat,
            lon,
        }
    }

    /// Where the block starts in its file, in bytes (`seek_offset`).
    #[must_use]
    pub fn offset(&self) -> u64 {
        let stride = grid::east_blocks(i16::from(self.degree.lat), self.degree.lon, self.spacing);
        (u64::from(stride) * u64::from(self.grid_idx_x) + u64::from(self.grid_idx_y))
            * BLOCK_BYTES as u64
    }

    /// The whole block from its heights, `heights[x][y]` with `x` north:
    /// every group held, the CRC ArduPilot checks.
    #[must_use]
    pub fn encode(&self, heights: &[[i16; BLOCK_Y as usize]; BLOCK_X as usize]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(BLOCK_BYTES);
        bytes.extend_from_slice(&ALL_GROUPS.to_le_bytes());
        bytes.extend_from_slice(&self.lat.to_le_bytes());
        bytes.extend_from_slice(&self.lon.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.spacing.to_le_bytes());
        for height in heights.as_flattened() {
            bytes.extend_from_slice(&height.to_le_bytes());
        }
        bytes.extend_from_slice(&self.grid_idx_x.to_le_bytes());
        bytes.extend_from_slice(&self.grid_idx_y.to_le_bytes());
        bytes.extend_from_slice(&self.degree.lon.to_le_bytes());
        bytes.extend_from_slice(&self.degree.lat.to_le_bytes());
        debug_assert_eq!(bytes.len(), CRC_BYTES);
        let crc = crc(&bytes);
        bytes[16..18].copy_from_slice(&crc.to_le_bytes());
        bytes.push(VERSION_MINOR);
        bytes.resize(BLOCK_BYTES, 0);
        bytes
    }
}

/// `crc16_ccitt(buf, len, 0)` as ArduPilot computes it: CRC-16/XMODEM.
#[must_use]
pub fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

/// Why a block was not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The dataset has no height there: a void, or no tile.
    NoData,
    Unavailable(Unavailable),
}

/// A block left off the card, with the first point that lacked a height.
#[derive(Clone, Debug, PartialEq)]
pub struct Skipped {
    pub block: CardBlock,
    pub why: Missing,
    pub lat: f64,
    pub lon: f64,
}

/// Blocks ready to write, those skipped, and those whose heights are still
/// being read or fetched: ask again after the cache's next `arrived`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filled {
    pub blocks: Vec<(CardBlock, Vec<u8>)>,
    pub skipped: Vec<Skipped>,
    pub waiting: Vec<CardBlock>,
}

/// Fills blocks from a terrain source. `ground` answers a point, in
/// degrees, as [`Terrain::at`](crate::Terrain::at)'s ground does, and
/// `None` while it is still being read or fetched. A block with any point
/// lacking a height is skipped: heights are never guessed (T4).
pub fn fill(
    blocks: impl IntoIterator<Item = CardBlock>,
    mut ground: impl FnMut(f64, f64) -> Option<Result<Ground, Unavailable>>,
) -> Filled {
    let mut filled = Filled::default();
    'blocks: for block in blocks {
        let mut heights = [[0i16; BLOCK_Y as usize]; BLOCK_X as usize];
        let mut waiting = false;
        for (x, row) in (0u8..).zip(&mut heights) {
            for (y, height) in (0u8..).zip(row) {
                let (lat, lon) = grid::block_point(
                    i16::from(block.degree.lat),
                    block.degree.lon,
                    u32::from(block.grid_idx_x),
                    u32::from(block.grid_idx_y),
                    x,
                    y,
                    block.spacing,
                );
                let (lat, lon) = (f64::from(lat) * 1e-7, f64::from(lon) * 1e-7);
                let why = match ground(lat, lon) {
                    None => {
                        waiting = true;
                        continue;
                    }
                    Some(Ok(Ground::Height(h))) => {
                        *height = nearest_metre(h);
                        continue;
                    }
                    Some(Ok(Ground::Sea)) => {
                        *height = 0;
                        continue;
                    }
                    Some(Ok(Ground::NoData)) => Missing::NoData,
                    Some(Err(why)) => Missing::Unavailable(why),
                };
                filled.skipped.push(Skipped {
                    block,
                    why,
                    lat,
                    lon,
                });
                continue 'blocks;
            }
        }
        if waiting {
            filled.waiting.push(block);
        } else {
            filled.blocks.push((block, block.encode(&heights)));
        }
    }
    filled
}

/// The blocks of the vehicle's grid at `spacing` that touch an area or lie
/// within one block of it, by file and in file order. The area is its
/// points in degrees, latitude then longitude, in order: a polygon from
/// three, a line from two, a point from one. It may cross the antimeridian.
#[must_use]
#[allow(clippy::cast_possible_truncation, reason = "whole degrees")]
pub fn blocks_covering(points: &[(f64, f64)], spacing: u16) -> BTreeMap<Degree, Vec<CardBlock>> {
    let mut blocks: BTreeMap<Degree, BTreeSet<CardBlock>> = BTreeMap::new();
    if points.is_empty() || spacing == 0 {
        return BTreeMap::new();
    }
    // Longitudes unwrapped along the area, so one across the antimeridian
    // spans a few degrees rather than all of them.
    let mut area: Vec<(f64, f64)> = Vec::with_capacity(points.len());
    for &(lat, lon) in points {
        let lon = match area.last() {
            Some(&(_, last)) => last + (lon - last + 180.0).rem_euclid(360.0) - 180.0,
            None => lon,
        };
        area.push((lat.clamp(-90.0, 90.0), lon));
    }
    let (mut south, mut north, mut west, mut east) = (90.0f64, -90.0f64, f64::MAX, f64::MIN);
    for &(lat, lon) in &area {
        south = south.min(lat);
        north = north.max(lat);
        west = west.min(lon);
        east = east.max(lon);
    }
    // Two blocks around the area: a degree with no block near it adds
    // nothing.
    let block_m = f64::from(spacing) * f64::from(SPACING_Y);
    let margin_lat = 2.0 * block_m / METRES_PER_DEGREE;
    let widest = (south.abs().max(north.abs()) + margin_lat).min(90.0);
    let margin_lon = margin_lat / widest.to_radians().cos().max(0.01);
    let first_lat = (south - margin_lat).floor().max(-90.0) as i32;
    let last_lat = (north + margin_lat).floor().min(89.0) as i32;
    let first_lon = (west - margin_lon).floor() as i32;
    let last_lon = ((east + margin_lon).floor() as i32).min(first_lon + 359);
    for lat in first_lat..=last_lat {
        for lon in first_lon..=last_lon {
            let degree = Degree {
                lat: lat as i8,
                lon: ((lon + 180).rem_euclid(360) - 180) as i16,
            };
            let found = blocks_of_degree(degree, spacing, &area);
            if !found.is_empty() {
                blocks.entry(degree).or_default().extend(found);
            }
        }
    }
    blocks
        .into_iter()
        .map(|(degree, set)| (degree, set.into_iter().collect()))
        .collect()
}

/// A degree's blocks within one block of the area, found in ArduPilot's
/// frame for the degree: metres north and east of its corner.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "block indices"
)]
fn blocks_of_degree(degree: Degree, spacing: u16, area: &[(f64, f64)]) -> Vec<CardBlock> {
    let ref_lat = i32::from(degree.lat) * 10_000_000;
    let ref_lon = i32::from(degree.lon) * 10_000_000;
    let area: Vec<(f64, f64)> = area
        .iter()
        .map(|&(lat, lon)| {
            let lat = (lat * 1e7).round() as i32;
            let lon = grid::wrap_longitude((lon * 1e7).round() as i64);
            let (n, e) = grid::distance_ne(ref_lat, ref_lon, lat, lon);
            (f64::from(n), f64::from(e))
        })
        .collect();
    let size_x = f64::from(spacing) * f64::from(SPACING_X);
    let size_y = f64::from(spacing) * f64::from(SPACING_Y);
    let (mut lo_n, mut hi_n, mut lo_e, mut hi_e) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &(n, e) in &area {
        lo_n = lo_n.min(n);
        hi_n = hi_n.max(n);
        lo_e = lo_e.min(e);
        hi_e = hi_e.max(e);
    }
    // The last row, and each row's last block, holding a point of the
    // degree.
    let top = ref_lat + 10_000_000 - 1;
    let east_edge = ref_lon + 10_000_000 - 1;
    let height_m = f64::from(grid::distance_ne(ref_lat, ref_lon, top, ref_lon).0);
    let first_x = ((lo_n / size_x).floor() - 1.0).max(0.0) as i32;
    let last_x = ((hi_n / size_x).floor() + 1.0).min((height_m / size_x).floor()) as i32;
    let mut found = Vec::new();
    for x in first_x..=last_x {
        let row = [
            f64::from(x) * size_x,
            (f64::from(x + 1) * size_x).min(height_m),
        ];
        let width_m = row
            .into_iter()
            .map(|north| {
                let lat = grid::offset(ref_lat, ref_lon, north, 0.0).0.min(top);
                grid::distance_ne(ref_lat, ref_lon, lat, east_edge).1
            })
            .fold(0.0f32, f32::max);
        let first_y = ((lo_e / size_y).floor() - 1.0).max(0.0) as i32;
        let last_y =
            ((hi_e / size_y).floor() + 1.0).min((f64::from(width_m) / size_y).floor()) as i32;
        for y in first_y..=last_y {
            let near = Rect {
                south: f64::from(x - 1) * size_x,
                north: f64::from(x + 2) * size_x,
                west: f64::from(y - 1) * size_y,
                east: f64::from(y + 2) * size_y,
            };
            if near.touches(&area) {
                found.push(CardBlock::at(degree, spacing, x as u16, y as u16));
            }
        }
    }
    found
}

/// A rectangle in metres north and east of a degree's corner.
struct Rect {
    south: f64,
    north: f64,
    west: f64,
    east: f64,
}

impl Rect {
    /// Whether an area, its points in the same frame, touches it.
    fn touches(&self, area: &[(f64, f64)]) -> bool {
        match area {
            [] => false,
            [point] => self.clips(*point, *point),
            [a, b] => self.clips(*a, *b),
            _ => {
                area.iter()
                    .zip(area.iter().cycle().skip(1))
                    .any(|(&a, &b)| self.clips(a, b))
                    || self.centre_inside(area)
            }
        }
    }

    /// Liang-Barsky: whether a segment reaches into the rectangle.
    fn clips(&self, (n0, e0): (f64, f64), (n1, e1): (f64, f64)) -> bool {
        let (dn, de) = (n1 - n0, e1 - e0);
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        for (p, q) in [
            (-dn, n0 - self.south),
            (dn, self.north - n0),
            (-de, e0 - self.west),
            (de, self.east - e0),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
                continue;
            }
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
            if t0 > t1 {
                return false;
            }
        }
        true
    }

    /// Even-odd: whether the rectangle's centre is inside a polygon.
    fn centre_inside(&self, area: &[(f64, f64)]) -> bool {
        let n = f64::midpoint(self.south, self.north);
        let e = f64::midpoint(self.west, self.east);
        let mut inside = false;
        for (&(n0, e0), &(n1, e1)) in area.iter().zip(area.iter().cycle().skip(1)) {
            if (n0 > n) != (n1 > n) && e < e0 + (n - n0) / (n1 - n0) * (e1 - e0) {
                inside = !inside;
            }
        }
        inside
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e7(v: f64) -> i32 {
        #[allow(clippy::cast_possible_truncation, reason = "test points")]
        let v = (v * 1e7).round() as i32;
        v
    }

    #[test]
    fn the_crc_is_ardupilots() {
        // CRC-16/XMODEM's check value.
        assert_eq!(crc(b"123456789"), 0x31C3);
        assert_eq!(crc(&[]), 0);
    }

    #[test]
    fn a_block_is_laid_out_as_ardupilot_reads_it() {
        let block = CardBlock::at(Degree { lat: -34, lon: 151 }, 100, 3, 5);
        let mut heights = [[0i16; BLOCK_Y as usize]; BLOCK_X as usize];
        for (x, row) in (0i16..).zip(&mut heights) {
            for (y, h) in (0i16..).zip(row) {
                *h = x * 100 - y;
            }
        }
        let bytes = block.encode(&heights);
        assert_eq!(bytes.len(), BLOCK_BYTES);
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let i32_at = |at: usize| {
            i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        assert_eq!(
            u64::from_le_bytes(bytes[0..8].try_into().unwrap_or_default()),
            ALL_GROUPS
        );
        assert_eq!((i32_at(8), i32_at(12)), (block.lat, block.lon));
        assert_eq!((u16_at(18), u16_at(20)), (1, 100));
        // East first, then north: height[x][y] at 22 + 2 × (32x + y).
        assert_eq!(u16_at(22 + 2 * (32 * 2 + 7)).cast_signed(), 193);
        assert_eq!(u16_at(22 + 2 * (32 * 27 + 31)).cast_signed(), 2669);
        assert_eq!((u16_at(1814), u16_at(1816)), (3, 5));
        assert_eq!(u16_at(1818).cast_signed(), 151);
        assert_eq!(bytes[1820].cast_signed(), -34);
        assert_eq!(bytes[1821], VERSION_MINOR);
        assert!(bytes[1822..].iter().all(|&b| b == 0));
        let mut zeroed = bytes[..CRC_BYTES].to_vec();
        zeroed[16..18].fill(0);
        assert_eq!(u16_at(16), crc(&zeroed));
    }

    #[test]
    fn files_are_named_and_signed_as_ardupilot_names_them() {
        let name = |lat, lon| Degree { lat, lon }.file_name();
        assert_eq!(name(53, -5), "N53W005.DAT");
        assert_eq!(name(-34, 151), "S34E151.DAT");
        assert_eq!(name(0, 0), "N00E000.DAT");
        assert_eq!(name(-1, -1), "S01W001.DAT");
        assert_eq!(name(-18, -180), "S18W180.DAT");
        // Just south and west of 0°: the degree below, as ArduPilot rounds.
        let block = CardBlock::containing(-1, -1, 100);
        assert_eq!(block.degree, Degree { lat: -1, lon: -1 });
        let block = CardBlock::containing(e7(-17.5), e7(179.999), 100);
        assert_eq!(block.degree.file_name(), "S18E179.DAT");
        let block = CardBlock::containing(e7(-17.5), e7(-179.999), 100);
        assert_eq!(block.degree.file_name(), "S18W180.DAT");
        assert_eq!(block.grid_idx_y, 0);
    }

    #[test]
    fn rows_narrow_towards_the_poles() {
        // A degree's width plus two blocks, in blocks of 28 spacings.
        assert_eq!(grid::east_blocks(0, 0, 100), 42);
        assert_eq!(grid::east_blocks(60, 10, 100), 22);
        assert_eq!(grid::east_blocks(80, 10, 100), 9);
        assert_eq!(grid::east_blocks(89, 10, 100), 2);
        assert_eq!(grid::east_blocks(-89, -180, 30), 4);
        assert_eq!(grid::east_blocks(-89, 179, 30), 4);
        let block = CardBlock::at(Degree { lat: 80, lon: 10 }, 100, 5, 3);
        assert_eq!(block.offset(), (9 * 5 + 3) * 2048);
        let point = CardBlock::containing(block.lat + 10, block.lon + 10, 100);
        assert_eq!(point, block);
    }

    #[test]
    fn a_block_with_a_missing_height_is_skipped_and_says_why() {
        let degree = Degree { lat: 47, lon: 8 };
        // Two apart, as neighbours share a column of points.
        let blocks = [0, 2, 4, 6].map(|y| CardBlock::at(degree, 100, 4, y));
        let void = blocks[1];
        let offline = blocks[2];
        let loading = blocks[3];
        let in_block =
            |b: CardBlock, lat: f64, lon: f64| CardBlock::containing(e7(lat), e7(lon), 100) == b;
        let mut asked = 0;
        let filled = fill(blocks, |lat, lon| {
            asked += 1;
            if in_block(void, lat, lon) {
                Some(Ok(Ground::NoData))
            } else if in_block(offline, lat, lon) {
                Some(Err(Unavailable::Offline))
            } else if in_block(loading, lat, lon) {
                None
            } else if lat > 47.1 {
                Some(Ok(Ground::Sea))
            } else {
                Some(Ok(Ground::Height(412.6)))
            }
        });
        assert_eq!(filled.blocks.len(), 1);
        let (block, bytes) = &filled.blocks[0];
        assert_eq!(*block, blocks[0]);
        let heights: Vec<i16> = bytes[22..1814]
            .chunks(2)
            .map(|h| i16::from_le_bytes([h[0], h[1]]))
            .collect();
        assert!(heights.iter().all(|&h| h == 413 || h == 0));
        assert!(heights.contains(&0) && heights.contains(&413));
        let why: Vec<_> = filled
            .skipped
            .iter()
            .map(|s| (s.block, s.why.clone()))
            .collect();
        assert_eq!(
            why,
            [
                (void, Missing::NoData),
                (offline, Missing::Unavailable(Unavailable::Offline))
            ]
        );
        assert!(in_block(void, filled.skipped[0].lat, filled.skipped[0].lon));
        assert_eq!(filled.waiting, [loading]);
        // A skipped block stops at its first missing height; a loading one
        // asks for every point, so all it needs is fetched at once.
        assert!(asked < 4 * 896 && asked > 2 * 896);
    }

    #[test]
    fn an_area_covers_its_blocks_and_one_more_around() {
        let spacing = 100;
        // A triangle about 6 km across, well inside N47E008.
        let area = [(47.40, 8.40), (47.44, 8.48), (47.38, 8.47)];
        let files = blocks_covering(&area, spacing);
        assert_eq!(
            files.keys().copied().collect::<Vec<_>>(),
            [Degree { lat: 47, lon: 8 }]
        );
        let found = &files[&Degree { lat: 47, lon: 8 }];
        assert!(found.windows(2).all(|w| w[0].offset() < w[1].offset()));
        // The blocks with a point of the triangle, sampled finely.
        let mut inside = BTreeSet::new();
        for i in 0..=400 {
            for j in 0..=400 {
                let (a, b) = (f64::from(i) / 400.0, f64::from(j) / 400.0);
                if a + b > 1.0 {
                    continue;
                }
                let lat = area[0].0 + a * (area[1].0 - area[0].0) + b * (area[2].0 - area[0].0);
                let lon = area[0].1 + a * (area[1].1 - area[0].1) + b * (area[2].1 - area[0].1);
                let block = CardBlock::containing(e7(lat), e7(lon), spacing);
                inside.insert((block.grid_idx_x, block.grid_idx_y));
            }
        }
        let near = |x: u16, y: u16| {
            inside
                .iter()
                .any(|&(ix, iy)| ix.abs_diff(x) <= 1 && iy.abs_diff(y) <= 1)
        };
        for &(x, y) in &inside {
            assert!(found.iter().any(|b| (b.grid_idx_x, b.grid_idx_y) == (x, y)));
        }
        for block in found {
            assert!(
                near(block.grid_idx_x, block.grid_idx_y),
                "{block:?} is not near"
            );
            assert_eq!(
                *block,
                CardBlock::at(block.degree, spacing, block.grid_idx_x, block.grid_idx_y)
            );
        }
        // Every neighbour of a block inside, the triangle's corners not cut.
        let neighbours = (0..=200u16)
            .flat_map(|x| (0..=200u16).map(move |y| (x, y)))
            .filter(|&(x, y)| near(x, y))
            .count();
        assert_eq!(found.len(), neighbours);
    }

    #[test]
    fn a_point_takes_its_block_and_those_around_it_across_degrees() {
        // At a degree's south-west corner, the blocks around it lie in
        // four files.
        let files = blocks_covering(&[(-33.0001, 151.0001)], 100);
        let names: Vec<String> = files.keys().map(|d| d.file_name()).collect();
        assert_eq!(
            names,
            ["S34E150.DAT", "S34E151.DAT", "S33E150.DAT", "S33E151.DAT"]
        );
        let here = CardBlock::containing(e7(-33.0001), e7(151.0001), 100);
        assert_eq!(here.degree, Degree { lat: -34, lon: 151 });
        assert!(files[&here.degree].contains(&here));
        // A point inside a degree: its block and the eight around it.
        let files = blocks_covering(&[(47.5, 8.5)], 30);
        let blocks = &files[&Degree { lat: 47, lon: 8 }];
        assert_eq!(files.len(), 1);
        assert_eq!(blocks.len(), 9);
        assert_eq!(blocks[4], CardBlock::containing(e7(47.5), e7(8.5), 30));
    }

    #[test]
    fn an_area_across_the_antimeridian_takes_both_sides() {
        let area = [
            (-17.52, 179.97),
            (-17.52, -179.97),
            (-17.48, -179.97),
            (-17.48, 179.97),
        ];
        let files = blocks_covering(&area, 100);
        let names: Vec<String> = files.keys().map(|d| d.file_name()).collect();
        assert_eq!(names, ["S18W180.DAT", "S18E179.DAT"]);
        let west = &files[&Degree {
            lat: -18,
            lon: -180,
        }];
        let east = &files[&Degree { lat: -18, lon: 179 }];
        assert!(west.iter().any(|b| b.grid_idx_y == 0));
        let last = grid::east_blocks(-18, 179, 100) - 3;
        assert!(east.iter().all(|b| u32::from(b.grid_idx_y) + 2 >= last));
        // About 6.4 km east to west: two or three blocks a side and margin.
        assert!(west.len() + east.len() < 40);
        let here = CardBlock::containing(e7(-17.5), e7(-179.99), 100);
        assert!(west.contains(&here));
        assert!(blocks_covering(&[], 100).is_empty());
        assert!(blocks_covering(&area, 0).is_empty());
    }
}

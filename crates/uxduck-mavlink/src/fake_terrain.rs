//! The fake vehicle's terrain (S118), when it is told to have some: like
//! ArduPilot with an empty terrain folder, it asks a ground station for the
//! block under it every two seconds until it has every group, keeps what
//! it is sent, and reports the height under it from that, interpolated as
//! `AP_Terrain` does. Without it the fake vehicle reports fixed terrain.

use std::collections::BTreeMap;

use uxduck_terrain::grid::{self, BLOCK_X, BLOCK_Y, GROUP, GridInfo};

use crate::terrain::Block;

/// ArduPilot's default `TERRAIN_SPACING`.
pub const SPACING: u16 = 100;
/// How often it asks again for what is missing.
const ASK_EVERY_S: f64 = 2.0;

/// One block as the vehicle keeps it: which groups came, and the heights.
#[derive(Clone, Debug)]
struct Kept {
    bitmap: u64,
    heights: [[i16; BLOCK_Y as usize]; BLOCK_X as usize],
}

#[derive(Clone, Debug, Default)]
pub struct FakeTerrain {
    blocks: BTreeMap<Block, Kept>,
    asked_at_s: Option<f64>,
    /// Metres added to every height it keeps, as if its card held another
    /// dataset (S119).
    pub shift_m: i16,
}

impl FakeTerrain {
    fn block_at(lat_e7: i32, lon_e7: i32) -> (Block, GridInfo) {
        let info = grid::grid_info(lat_e7, lon_e7, SPACING);
        (
            Block {
                lat: info.grid_lat,
                lon: info.grid_lon,
                spacing: SPACING,
            },
            info,
        )
    }

    /// What to ask for now, if anything: the block under the vehicle and
    /// the groups of it still missing.
    pub fn ask(&mut self, lat_e7: i32, lon_e7: i32, t_s: f64) -> Option<(Block, u64)> {
        if self.asked_at_s.is_some_and(|at| t_s - at < ASK_EVERY_S) {
            return None;
        }
        let (block, _) = Self::block_at(lat_e7, lon_e7);
        let have = self.blocks.get(&block).map_or(0, |k| k.bitmap);
        let mask = grid::ALL_GROUPS & !have;
        if mask == 0 {
            return None;
        }
        self.asked_at_s = Some(t_s);
        Some((block, mask))
    }

    /// Keeps a `TERRAIN_DATA` for a block it asked about.
    pub fn keep(&mut self, block: Block, gridbit: u8, data: [i16; 16]) {
        if block.spacing != SPACING || gridbit >= 56 {
            return;
        }
        let kept = self.blocks.entry(block).or_insert(Kept {
            bitmap: 0,
            heights: [[0; BLOCK_Y as usize]; BLOCK_X as usize],
        });
        let (x0, y0) = grid::group_origin(gridbit);
        for x in 0..GROUP {
            for y in 0..GROUP {
                kept.heights[usize::from(x0 + x)][usize::from(y0 + y)] =
                    data[usize::from(x * GROUP + y)].saturating_add(self.shift_m);
            }
        }
        kept.bitmap |= 1 << gridbit;
    }

    /// Groups still missing around it and groups it holds, as
    /// `TERRAIN_REPORT`'s `pending` and `loaded`.
    #[must_use]
    pub fn counts(&self, lat_e7: i32, lon_e7: i32) -> (u16, u16) {
        let (block, _) = Self::block_at(lat_e7, lon_e7);
        let loaded: u32 = self.blocks.values().map(|k| k.bitmap.count_ones()).sum();
        let here = self.blocks.get(&block).map_or(0, |k| k.bitmap.count_ones());
        let pending = 56 - here;
        (
            u16::try_from(pending).unwrap_or(u16::MAX),
            u16::try_from(loaded).unwrap_or(u16::MAX),
        )
    }

    /// The terrain's height under a point, if it holds the four points
    /// around it.
    #[must_use]
    pub fn height(&self, lat_e7: i32, lon_e7: i32) -> Option<f32> {
        let (block, info) = Self::block_at(lat_e7, lon_e7);
        let kept = self.blocks.get(&block)?;
        let (x, y) = (info.idx_x, info.idx_y);
        let has = |x: u8, y: u8| kept.bitmap & (1 << grid::gridbit(x, y)) != 0;
        if !(has(x, y) && has(x, y + 1) && has(x + 1, y) && has(x + 1, y + 1)) {
            return None;
        }
        let at = |x: u8, y: u8| f32::from(kept.heights[usize::from(x)][usize::from(y)]);
        Some(grid::interpolate(
            &info,
            [at(x, y), at(x, y + 1), at(x + 1, y), at(x + 1, y + 1)],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_asks_until_it_has_the_block_and_reports_from_it() {
        let (lat, lon) = (-353_632_610, 1_491_652_300);
        let mut terrain = FakeTerrain::default();
        let (block, mask) = terrain.ask(lat, lon, 0.0).unwrap();
        assert_eq!(mask, grid::ALL_GROUPS);
        assert_eq!(terrain.ask(lat, lon, 1.0), None, "not again so soon");
        assert_eq!(terrain.height(lat, lon), None);
        assert_eq!(terrain.counts(lat, lon), (56, 0));
        for gridbit in 0..56 {
            terrain.keep(block, gridbit, [584; 16]);
        }
        assert_eq!(terrain.ask(lat, lon, 5.0), None, "it has it all");
        assert_eq!(terrain.counts(lat, lon), (0, 56));
        assert_eq!(terrain.height(lat, lon), Some(584.0));
    }
}

//! A height at a point (T3): bilinear between the four samples around it,
//! wherever they are kept.
//!
//! The four may lie in up to four chunks of up to four tiles, and the rows
//! above and below a point may have different numbers of columns
//! (Copernicus thins its columns towards the poles), so each of the two
//! rows is interpolated along its own columns first, then the two rows
//! along the meridian. Where the rows match this is plain bilinear.
//!
//! A sample with no weight is never read, so a point on a sample needs
//! only that sample. Any void among the samples it does need is no data:
//! UXDuck never guesses a height (T4).

use crate::chunk::Chunk;
use crate::dataset::{CHUNKS_PER_SIDE, Cell, ChunkKey, Tile, wrap_degrees};

/// Samples per degree assumed for a tile that is all sea.
const SEA_SAMPLES: u16 = 3600;

/// The ground at a point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ground {
    /// Metres above the dataset's geoid.
    Height(f64),
    /// Sea, by the tile grid: 0 m.
    Sea,
    /// A void, or a tile the source does not cover.
    NoData,
}

/// What a lookup needs before it can answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Need {
    Cell(Cell),
    Chunk(ChunkKey),
}

/// A tile as its cell index describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileGrid {
    Data { rows: u16, cols: u16 },
    Sea,
    NoData,
}

/// Where the lookup reads tiles and chunks.
pub trait Source {
    /// # Errors
    /// What is needed to know the tile.
    fn tile(&self, tile: Tile) -> Result<TileGrid, Need>;
    /// The chunk, or `None` for one with no data (not stored).
    ///
    /// # Errors
    /// The chunk itself, when it is not here.
    fn chunk(&self, key: ChunkKey) -> Result<Option<&Chunk>, Need>;
}

enum Value {
    Height(f64),
    Sea,
    NoData,
}

/// The ground at a point, or what must be fetched first.
///
/// # Errors
/// The cells and chunks to fetch.
pub fn ground(source: &impl Source, lat: f64, lon: f64) -> Result<Ground, Vec<Need>> {
    let Some(tile) = Tile::containing(lat, lon) else {
        return Ok(Ground::NoData);
    };
    let grid = source.tile(tile).map_err(|need| vec![need])?;
    let rows = match grid {
        TileGrid::Data { rows, .. } => rows,
        TileGrid::Sea => SEA_SAMPLES,
        TileGrid::NoData => return Ok(Ground::NoData),
    };
    let y = (f64::from(tile.lat) + 1.0 - lat) * f64::from(rows);
    let r0 = y.floor().clamp(0.0, f64::from(rows - 1));
    let t = (y - r0).clamp(0.0, 1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped above"
    )]
    let r0 = r0 as u16;

    let mut needs = Vec::new();
    let mut values = Vec::with_capacity(4);
    let mut rows_wanted = vec![(1.0 - t, tile, r0)];
    if t > 0.0 {
        if r0 + 1 < rows {
            rows_wanted.push((t, tile, r0 + 1));
        } else if let Some(south) = tile.south() {
            rows_wanted.push((t, south, 0));
        }
    }
    let mut height = 0.0;
    for (weight, row_tile, row) in rows_wanted {
        if weight == 0.0 {
            continue;
        }
        match along_row(source, row_tile, row, lon, &mut needs) {
            Some(Value::Height(h)) => {
                height += weight * h;
                values.push(false);
            }
            Some(Value::Sea) => values.push(true),
            Some(Value::NoData) => return Ok(Ground::NoData),
            None => {}
        }
    }
    if !needs.is_empty() {
        needs.sort();
        needs.dedup();
        return Err(needs);
    }
    if values.iter().all(|&sea| sea) {
        Ok(Ground::Sea)
    } else {
        Ok(Ground::Height(height))
    }
}

/// The height along one row of samples at a longitude; `None` when
/// something is needed first.
fn along_row(
    source: &impl Source,
    tile: Tile,
    row: u16,
    lon: f64,
    needs: &mut Vec<Need>,
) -> Option<Value> {
    let grid = match source.tile(tile) {
        Ok(grid) => grid,
        Err(need) => {
            needs.push(need);
            return None;
        }
    };
    let cols = match grid {
        TileGrid::Data { cols, .. } => cols,
        TileGrid::Sea => SEA_SAMPLES,
        TileGrid::NoData => return Some(Value::NoData),
    };
    let mut x = (wrap_degrees(lon) - f64::from(tile.lon)) * f64::from(cols);
    if x < 0.0 {
        // The antimeridian: a point at 180° east is the west edge.
        x += 360.0 * f64::from(cols);
    }
    let c0 = x.floor().clamp(0.0, f64::from(cols - 1));
    let s = (x - c0).clamp(0.0, 1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped above"
    )]
    let c0 = c0 as u16;
    let mut wanted = vec![(1.0 - s, tile, row, c0)];
    if s > 0.0 {
        if c0 + 1 < cols {
            wanted.push((s, tile, row, c0 + 1));
        } else {
            wanted.push((s, tile.east(), row, 0));
        }
    }
    let mut height = 0.0;
    let mut sea = true;
    let mut missing = false;
    for (weight, tile, row, col) in wanted {
        if weight == 0.0 {
            continue;
        }
        match sample(source, tile, row, col) {
            Ok(Value::Height(h)) => {
                height += weight * h;
                sea = false;
            }
            Ok(Value::Sea) => {}
            Ok(Value::NoData) => return Some(Value::NoData),
            Err(need) => {
                needs.push(need);
                missing = true;
            }
        }
    }
    if missing {
        None
    } else if sea {
        Some(Value::Sea)
    } else {
        Some(Value::Height(height))
    }
}

/// One sample of a tile, by its row and column.
fn sample(source: &impl Source, tile: Tile, row: u16, col: u16) -> Result<Value, Need> {
    let (rows, cols) = match source.tile(tile)? {
        TileGrid::Data { rows, cols } => (rows, cols),
        TileGrid::Sea => return Ok(Value::Sea),
        TileGrid::NoData => return Ok(Value::NoData),
    };
    if row >= rows || col >= cols {
        // A neighbour on another grid: no sample lines up.
        return Ok(Value::NoData);
    }
    let per = u16::from(CHUNKS_PER_SIDE);
    let (chunk_rows, chunk_cols) = (rows / per, cols / per);
    #[allow(clippy::cast_possible_truncation, reason = "under eight")]
    let key = ChunkKey {
        tile,
        row: (row / chunk_rows) as u8,
        col: (col / chunk_cols) as u8,
    };
    match source.chunk(key)? {
        None => Ok(Value::NoData),
        Some(chunk) => Ok(chunk
            .height(row % chunk_rows, col % chunk_cols)
            .map_or(Value::NoData, Value::Height)),
    }
}

/// Rounded to the nearest metre, as sent to a vehicle: truncating would
/// read the ground up to a metre low, the unsafe way (T3).
#[must_use]
#[allow(clippy::cast_possible_truncation, reason = "heights fit easily")]
pub fn nearest_metre(height: f64) -> i16 {
    height
        .round()
        .clamp(f64::from(i16::MIN + 1), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::chunk::VOID;

    /// Tiles of 16 by 16 samples (two per chunk), each sample's height its
    /// global row and column, so interpolation is easy to check.
    #[derive(Default)]
    struct Fake {
        tiles: HashMap<Tile, TileGrid>,
        chunks: HashMap<ChunkKey, Option<Chunk>>,
    }

    impl Fake {
        fn land(&mut self, name: &str, rows: u16, cols: u16, f: impl Fn(u16, u16) -> i16) {
            let tile = Tile::parse(name).unwrap();
            self.tiles.insert(tile, TileGrid::Data { rows, cols });
            let (cr, cc) = (rows / 8, cols / 8);
            for i in 0..8u8 {
                for j in 0..8u8 {
                    let mut samples = Vec::new();
                    for r in 0..cr {
                        for c in 0..cc {
                            samples.push(f(u16::from(i) * cr + r, u16::from(j) * cc + c));
                        }
                    }
                    let key = ChunkKey {
                        tile,
                        row: i,
                        col: j,
                    };
                    self.chunks
                        .insert(key, Some(Chunk::new(cr, cc, 10, samples).unwrap()));
                }
            }
        }
    }

    impl Source for Fake {
        fn tile(&self, tile: Tile) -> Result<TileGrid, Need> {
            self.tiles
                .get(&tile)
                .copied()
                .ok_or(Need::Cell(tile.cell()))
        }
        fn chunk(&self, key: ChunkKey) -> Result<Option<&Chunk>, Need> {
            self.chunks
                .get(&key)
                .map(Option::as_ref)
                .ok_or(Need::Chunk(key))
        }
    }

    fn close(ground: Ground, want: f64) {
        match ground {
            Ground::Height(h) => assert!((h - want).abs() < 1e-6, "{h} not {want}"),
            other => panic!("{other:?} not {want}"),
        }
    }

    #[test]
    fn bilinear_inside_a_chunk_and_on_a_sample() {
        let mut fake = Fake::default();
        fake.land("N10E020", 16, 16, |r, c| (r * 10 + c).cast_signed());
        // Row 2.5 from the north, column 3.25 from the west.
        close(
            ground(&fake, 11.0 - 2.5 / 16.0, 20.0 + 3.25 / 16.0).unwrap(),
            25.0 + 3.25,
        );
        // Exactly on a sample.
        close(
            ground(&fake, 11.0 - 4.0 / 16.0, 20.0 + 5.0 / 16.0).unwrap(),
            45.0,
        );
    }

    #[test]
    fn across_chunk_and_tile_edges() {
        let mut fake = Fake::default();
        fake.land("N10E020", 16, 16, |r, c| (r * 10 + c).cast_signed());
        fake.land("N09E020", 16, 16, |r, c| ((r + 16) * 10 + c).cast_signed());
        fake.land("N10E021", 16, 16, |r, c| (r * 10 + c + 16).cast_signed());
        fake.land("N09E021", 16, 16, |r, c| {
            ((r + 16) * 10 + c + 16).cast_signed()
        });
        // Between chunks 0 and 1 (rows 1 and 2).
        close(ground(&fake, 11.0 - 1.5 / 16.0, 20.0).unwrap(), 15.0);
        // Between the last row of N10 and the first of N09, and the last
        // column of E020 and the first of E021.
        close(
            ground(&fake, 10.0 + 0.5 / 16.0, 21.0 - 0.5 / 16.0).unwrap(),
            155.0 + 15.5,
        );
    }

    #[test]
    fn a_point_names_what_it_needs() {
        let mut fake = Fake::default();
        fake.land("N10E020", 16, 16, |_, _| 5);
        let key = ChunkKey {
            tile: Tile::parse("N10E020").unwrap(),
            row: 0,
            col: 0,
        };
        fake.chunks.remove(&key);
        assert_eq!(ground(&fake, 10.99, 20.01), Err(vec![Need::Chunk(key)]));
        // Its south neighbour's cell is not known yet.
        let needs = ground(&fake, 10.0 + 0.5 / 16.0, 20.01).unwrap_err();
        assert_eq!(
            needs,
            vec![Need::Cell(Tile::parse("N09E020").unwrap().cell())]
        );
    }

    #[test]
    fn voids_and_missing_tiles_are_no_data_and_sea_is_zero() {
        let mut fake = Fake::default();
        fake.land(
            "N10E020",
            16,
            16,
            |r, c| if (r, c) == (3, 3) { VOID } else { 100 },
        );
        fake.tiles
            .insert(Tile::parse("N09E020").unwrap(), TileGrid::Sea);
        fake.tiles
            .insert(Tile::parse("N10E021").unwrap(), TileGrid::NoData);
        assert_eq!(
            ground(&fake, 11.0 - 3.5 / 16.0, 20.0 + 3.5 / 16.0),
            Ok(Ground::NoData)
        );
        // A sample away from the void is fine.
        close(ground(&fake, 11.0 - 8.0 / 16.0, 20.5).unwrap(), 100.0);
        // Half way to the sea: half the height.
        close(ground(&fake, 10.0 + 0.5 / 16.0, 20.5).unwrap(), 50.0);
        assert_eq!(ground(&fake, 9.5, 20.5), Ok(Ground::Sea));
        // Next to a tile with no data.
        assert_eq!(ground(&fake, 10.5, 21.0 - 0.5 / 16.0), Ok(Ground::NoData));
    }

    #[test]
    fn rows_with_fewer_columns_interpolate_along_their_own() {
        let mut fake = Fake::default();
        // North of the band edge: 8 columns; south: 16.
        fake.land("N50E000", 16, 8, |_, c| (c * 10).cast_signed());
        fake.land("N49E000", 16, 16, |_, c| (c * 4).cast_signed());
        let lon = 1.25 / 8.0;
        // On the band edge: N49's row 0 alone, 2.5 of its columns along.
        close(ground(&fake, 50.0, lon).unwrap(), 10.0);
        // Half a row north: N50's last row, 1.25 of its columns along
        // (12.5 m), and N49's first (10 m).
        close(ground(&fake, 50.0 + 0.5 / 16.0, lon).unwrap(), 11.25);
    }

    #[test]
    fn rounding_is_to_the_nearest_metre() {
        assert_eq!(nearest_metre(583.5), 584);
        assert_eq!(nearest_metre(583.49), 583);
        assert_eq!(nearest_metre(-1.6), -2);
    }
}

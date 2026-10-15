//! Making a release (T2): a source tile cut into chunks, and the indexes
//! that name them. The importer (`cargo xtask terrain-import`), the test
//! fixtures and the tests use the same code, so what is published is what
//! the cache reads.

use std::collections::BTreeMap;

use crate::chunk::{Chunk, VOID};
use crate::dataset::{CELL_DEGREES, CHUNKS_PER_SIDE, Cell, ChunkKey, Dataset, Tile};
use crate::manifest::{self, CellIndex, FileRef, Release, SCHEMA, SourceFile, TileEntry};

/// One source tile's samples, at its own resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileSamples {
    pub tile: Tile,
    /// Samples per degree north to south (its north edge kept, its south
    /// edge left to the tile below).
    pub rows: u16,
    /// Samples per degree west to east (its west edge kept).
    pub cols: u16,
    /// Metres per unit, in decimetres.
    pub unit_dm: u8,
    /// Row-major from the north-west; [`VOID`] for no data.
    pub samples: Vec<i16>,
}

/// A tile's chunks: each with its file, or `None` when it has no data.
#[derive(Clone, Debug)]
pub struct Cut {
    pub tile: Tile,
    pub rows: u16,
    pub cols: u16,
    pub chunks: Vec<(ChunkKey, Option<Vec<u8>>)>,
}

impl TileSamples {
    /// Cuts the tile into its 64 chunks.
    ///
    /// # Errors
    /// When the samples do not fill the tile, or it does not divide into
    /// eight.
    pub fn cut(&self) -> Result<Cut, String> {
        let per = u16::from(CHUNKS_PER_SIDE);
        if self.rows == 0
            || self.cols == 0
            || !self.rows.is_multiple_of(per)
            || !self.cols.is_multiple_of(per)
        {
            return Err(format!(
                "{} is {} by {}, which does not cut into {per} by {per}",
                self.tile, self.rows, self.cols
            ));
        }
        if self.samples.len() != usize::from(self.rows) * usize::from(self.cols) {
            return Err(format!("{}'s samples do not fill it", self.tile));
        }
        let (chunk_rows, chunk_cols) = (self.rows / per, self.cols / per);
        let mut chunks = Vec::with_capacity(64);
        for row in 0..CHUNKS_PER_SIDE {
            for col in 0..CHUNKS_PER_SIDE {
                let mut samples =
                    Vec::with_capacity(usize::from(chunk_rows) * usize::from(chunk_cols));
                for r in 0..chunk_rows {
                    let start = (usize::from(row) * usize::from(chunk_rows) + usize::from(r))
                        * usize::from(self.cols)
                        + usize::from(col) * usize::from(chunk_cols);
                    samples
                        .extend_from_slice(&self.samples[start..start + usize::from(chunk_cols)]);
                }
                let chunk = Chunk::new(chunk_rows, chunk_cols, self.unit_dm, samples)?;
                let key = ChunkKey {
                    tile: self.tile,
                    row,
                    col,
                };
                chunks.push((key, (!chunk.is_void()).then(|| chunk.encode())));
            }
        }
        Ok(Cut {
            tile: self.tile,
            rows: self.rows,
            cols: self.cols,
            chunks,
        })
    }

    /// Whether every sample is no data.
    #[must_use]
    pub fn is_void(&self) -> bool {
        self.samples.iter().all(|&s| s == VOID)
    }
}

impl Cut {
    /// Its entry in a cell index.
    #[must_use]
    pub fn entry(&self, source: SourceFile) -> TileEntry {
        TileEntry {
            rows: self.rows,
            cols: self.cols,
            source,
            chunks: self
                .chunks
                .iter()
                .map(|(_, bytes)| bytes.as_deref().map(manifest::sha256).unwrap_or_default())
                .collect(),
            sizes: self
                .chunks
                .iter()
                .map(|(_, bytes)| {
                    bytes
                        .as_ref()
                        .map_or(0, |b| u32::try_from(b.len()).unwrap_or(u32::MAX))
                })
                .collect(),
        }
    }
}

/// A cell's index from its tiles' entries and its sea tiles.
#[must_use]
pub fn cell_index(
    dataset: Dataset,
    release: &str,
    cell: Cell,
    tiles: BTreeMap<String, TileEntry>,
    sea: Vec<String>,
) -> CellIndex {
    CellIndex {
        schema: SCHEMA,
        dataset,
        release: release.into(),
        cell: cell.name(),
        tiles,
        sea,
    }
}

/// One release as it is made: tiles go in, files come out through the
/// `keep` callback (chunks as each tile is added, cell indexes at the end),
/// so a whole dataset never sits in memory.
#[derive(Debug)]
pub struct ReleaseBuilder {
    release: Release,
    cells: BTreeMap<Cell, (BTreeMap<String, TileEntry>, Vec<String>)>,
}

impl ReleaseBuilder {
    #[must_use]
    pub fn new(dataset: Dataset, id: &str, imported_at: &str, source: &str) -> Self {
        Self {
            release: Release {
                dataset,
                id: id.into(),
                imported_at: imported_at.into(),
                source: source.into(),
                heights: dataset.geoid().into(),
                cells: BTreeMap::new(),
                sea_cells: Vec::new(),
            },
            cells: BTreeMap::new(),
        }
    }

    /// Cuts a tile and keeps its chunks.
    ///
    /// # Errors
    /// As [`TileSamples::cut`].
    pub fn add_tile(
        &mut self,
        samples: &TileSamples,
        source: SourceFile,
        keep: &mut dyn FnMut(String, Vec<u8>),
    ) -> Result<(), String> {
        self.add_cut(samples.cut()?, source, keep);
        Ok(())
    }

    /// Keeps a tile already cut, so the cutting (most of the work) can
    /// happen on other threads.
    pub fn add_cut(&mut self, cut: Cut, source: SourceFile, keep: &mut dyn FnMut(String, Vec<u8>)) {
        let entry = cut.entry(source);
        let tile = cut.tile;
        for (key, bytes) in cut.chunks {
            if let Some(bytes) = bytes {
                keep(self.release.chunk_path(key), bytes);
            }
        }
        self.cells
            .entry(tile.cell())
            .or_default()
            .0
            .insert(tile.name(), entry);
    }

    /// A tile that is all sea.
    pub fn add_sea_tile(&mut self, tile: Tile) {
        self.cells
            .entry(tile.cell())
            .or_default()
            .1
            .push(tile.name());
    }

    /// Writes each cell's index and returns the release for the manifest.
    /// A cell with only sea tiles, all 25 of them, is a sea cell with no
    /// index.
    #[must_use]
    pub fn finish(mut self, keep: &mut dyn FnMut(String, Vec<u8>)) -> Release {
        for (cell, (tiles, mut sea)) in std::mem::take(&mut self.cells) {
            sea.sort();
            sea.dedup();
            let all = usize::try_from(CELL_DEGREES * CELL_DEGREES).unwrap_or(25);
            if tiles.is_empty() && sea.len() == all {
                self.release.sea_cells.push(cell.name());
                continue;
            }
            let index = cell_index(self.release.dataset, &self.release.id, cell, tiles, sea);
            let bytes = serde_json::to_vec(&index).unwrap_or_default();
            self.release.cells.insert(cell.name(), file_ref(&bytes));
            keep(self.release.cell_path(cell), bytes);
        }
        self.release.sea_cells.sort();
        self.release
    }
}

/// A file's reference in the manifest.
#[must_use]
pub fn file_ref(bytes: &[u8]) -> FileRef {
    FileRef {
        sha256: manifest::sha256(bytes),
        size: bytes.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_cuts_into_sixty_four_and_void_chunks_are_not_kept() {
        let tile = Tile::parse("N53W005").unwrap();
        let mut samples = vec![VOID; 16 * 24];
        // One sample in the north-east chunk.
        samples[23] = 12;
        let cut = TileSamples {
            tile,
            rows: 16,
            cols: 24,
            unit_dm: 10,
            samples,
        }
        .cut()
        .unwrap();
        assert_eq!(cut.chunks.len(), 64);
        let kept: Vec<_> = cut.chunks.iter().filter(|(_, b)| b.is_some()).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!((kept[0].0.row, kept[0].0.col), (0, 7));
        let chunk = Chunk::decode(kept[0].1.as_ref().unwrap()).unwrap();
        assert_eq!((chunk.rows, chunk.cols), (2, 3));
        assert_eq!(chunk.height(0, 2), Some(12.0));
        let entry = cut.entry(SourceFile {
            name: "x".into(),
            sha256: String::new(),
        });
        assert_eq!(entry.chunks.iter().filter(|c| !c.is_empty()).count(), 1);
    }

    #[test]
    fn a_tile_that_does_not_divide_is_refused() {
        let tile = Tile::parse("N53W005").unwrap();
        let samples = TileSamples {
            tile,
            rows: 10,
            cols: 16,
            unit_dm: 10,
            samples: vec![0; 160],
        };
        assert!(samples.cut().is_err());
    }
}

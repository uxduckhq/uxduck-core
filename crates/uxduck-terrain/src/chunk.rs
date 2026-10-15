//! One chunk: an eighth of a degree square of heights at its dataset's own
//! resolution, never resampled (T2).
//!
//! The file is gzip around a small header and the samples:
//!
//! | Bytes | What |
//! | ----- | ---- |
//! | 4 | `UXDT` |
//! | 1 | format, 1 |
//! | 1 | the unit in decimetres: 10 for whole metres (ALOS), 5 for half metres (Copernicus) |
//! | 2 | rows, little-endian |
//! | 2 | columns, little-endian |
//! | 2 × rows × columns | the samples as `i16` little-endian, each the difference from the one before it in its row (the first in a row from the first of the row above), wrapping |
//!
//! Row 0 is the chunk's north edge and column 0 its west edge; a sample is
//! a point, not an area (SRTM's and Copernicus's own convention). A tile
//! keeps its north and west edges and leaves its south and east edges to
//! its neighbours, so no sample is stored twice. [`VOID`] is no data.
//! Differences make neighbouring heights small numbers, which deflate
//! packs well: Snowdonia's hilliest ALOS chunk is 95 kB, under 4 bits a
//! sample.

use std::io::{Read as _, Write as _};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// No data: a void in the source.
pub const VOID: i16 = i16::MIN;
const MAGIC: &[u8; 4] = b"UXDT";
const FORMAT: u8 = 1;
const HEADER: usize = 10;
/// Larger than any chunk of a 1″ grid.
const MAX_SIDE: u16 = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub rows: u16,
    pub cols: u16,
    /// Metres per unit, in decimetres.
    pub unit_dm: u8,
    samples: Vec<i16>,
}

impl Chunk {
    /// A chunk from its samples, row-major from the north-west.
    ///
    /// # Errors
    /// When the samples do not fill `rows` by `cols`, or the unit is zero.
    pub fn new(rows: u16, cols: u16, unit_dm: u8, samples: Vec<i16>) -> Result<Self, String> {
        if rows == 0 || cols == 0 || rows > MAX_SIDE || cols > MAX_SIDE {
            return Err(format!("a chunk cannot be {rows} by {cols}"));
        }
        if unit_dm == 0 {
            return Err("a chunk's unit cannot be zero".into());
        }
        if samples.len() != usize::from(rows) * usize::from(cols) {
            return Err(format!(
                "{} samples do not fill {rows} by {cols}",
                samples.len()
            ));
        }
        Ok(Self {
            rows,
            cols,
            unit_dm,
            samples,
        })
    }

    /// The height at a sample in metres, or `None` for no data.
    #[must_use]
    pub fn height(&self, row: u16, col: u16) -> Option<f64> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let raw = self.samples[usize::from(row) * usize::from(self.cols) + usize::from(col)];
        (raw != VOID).then(|| f64::from(raw) * f64::from(self.unit_dm) / 10.0)
    }

    /// The raw samples, row-major from the north-west.
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    /// Whether every sample is no data, so the chunk need not be stored.
    #[must_use]
    pub fn is_void(&self) -> bool {
        self.samples.iter().all(|&s| s == VOID)
    }

    /// The file's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let cols = usize::from(self.cols);
        let mut raw = Vec::with_capacity(HEADER + self.samples.len() * 2);
        raw.extend_from_slice(MAGIC);
        raw.push(FORMAT);
        raw.push(self.unit_dm);
        raw.extend_from_slice(&self.rows.to_le_bytes());
        raw.extend_from_slice(&self.cols.to_le_bytes());
        let mut row_start = 0i16;
        for row in self.samples.chunks(cols) {
            let mut before = row_start;
            row_start = row[0];
            for &sample in row {
                raw.extend_from_slice(&sample.wrapping_sub(before).to_le_bytes());
                before = sample;
            }
        }
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        // Writing to a Vec cannot fail.
        let _ = encoder.write_all(&raw);
        encoder.finish().unwrap_or_default()
    }

    /// Reads a chunk's file.
    ///
    /// # Errors
    /// When it is not one.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let limit = HEADER + 2 * usize::from(MAX_SIDE) * usize::from(MAX_SIDE);
        let mut raw = Vec::new();
        GzDecoder::new(bytes)
            .take(limit as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| format!("not a terrain chunk: {e}"))?;
        if raw.len() < HEADER || &raw[..4] != MAGIC {
            return Err("not a terrain chunk".into());
        }
        if raw[4] != FORMAT {
            return Err(format!(
                "terrain chunk format {} is not one this build reads",
                raw[4]
            ));
        }
        let unit_dm = raw[5];
        let rows = u16::from_le_bytes([raw[6], raw[7]]);
        let cols = u16::from_le_bytes([raw[8], raw[9]]);
        let body = &raw[HEADER..];
        if body.len() != 2 * usize::from(rows) * usize::from(cols) {
            return Err("a terrain chunk is cut short".into());
        }
        let mut samples = Vec::with_capacity(body.len() / 2);
        let mut row_start = 0i16;
        for (i, pair) in body.as_chunks::<2>().0.iter().enumerate() {
            let delta = i16::from_le_bytes([pair[0], pair[1]]);
            let sample = if i % usize::from(cols.max(1)) == 0 {
                let first = row_start.wrapping_add(delta);
                row_start = first;
                first
            } else {
                samples.last().copied().unwrap_or(0i16).wrapping_add(delta)
            };
            samples.push(sample);
        }
        Self::new(rows, cols, unit_dm, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hill(rows: u16, cols: u16) -> Vec<i16> {
        let mut samples = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                let d = (f64::from(r) - 20.0).hypot(f64::from(c) - 30.0);
                #[allow(clippy::cast_possible_truncation, reason = "a small hill")]
                samples.push((900.0 - d * 7.0) as i16);
            }
        }
        samples
    }

    #[test]
    fn a_chunk_survives_its_file() {
        let mut samples = hill(45, 60);
        samples[7] = VOID;
        samples[60] = i16::MAX;
        samples[61] = -400;
        let chunk = Chunk::new(45, 60, 10, samples).unwrap();
        let bytes = chunk.encode();
        assert_eq!(Chunk::decode(&bytes).unwrap(), chunk);
        assert_eq!(chunk.height(0, 7), None);
        assert_eq!(chunk.height(1, 1), Some(-400.0));
        assert_eq!(chunk.height(45, 0), None, "outside");
    }

    #[test]
    fn half_metres_read_as_half_metres() {
        let chunk = Chunk::new(1, 2, 5, vec![1171, -3]).unwrap();
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back.height(0, 0), Some(585.5));
        assert_eq!(back.height(0, 1), Some(-1.5));
    }

    #[test]
    fn differences_pack_a_hill_small() {
        let chunk = Chunk::new(450, 450, 10, hill(450, 450)).unwrap();
        let bytes = chunk.encode();
        assert!(bytes.len() < 450 * 450 / 4, "{} bytes", bytes.len());
    }

    #[test]
    fn what_is_not_a_chunk_is_refused() {
        assert!(Chunk::decode(b"nonsense").is_err());
        let mut bytes = Chunk::new(2, 2, 10, vec![1, 2, 3, 4]).unwrap().encode();
        let last = bytes.len() - 9;
        bytes[last] ^= 0xff;
        assert!(Chunk::decode(&bytes).is_err(), "gzip's CRC catches damage");
        assert!(Chunk::new(2, 2, 10, vec![1]).is_err());
        assert!(Chunk::new(2, 2, 0, vec![1, 2, 3, 4]).is_err());
        assert!(Chunk::new(2, 2, 10, vec![VOID; 4]).unwrap().is_void());
    }
}

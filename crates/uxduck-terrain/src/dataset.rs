//! The two datasets (ADR-0026 T1), and how the world is cut up: 1° tiles,
//! as the sources publish them, eight by eight chunks in each tile, and 5°
//! cells that index the tiles.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Chunks along each side of a tile: a chunk is an eighth of a degree.
pub const CHUNKS_PER_SIDE: u8 = 8;
/// A cell's side in degrees.
pub const CELL_DEGREES: i16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dataset {
    /// JAXA's AW3D30, as ArduPilot's SRTM1 files: the default.
    Alos,
    /// Copernicus GLO-30, as QGroundControl serves.
    Copernicus,
}

impl Dataset {
    pub const ALL: [Self; 2] = [Self::Alos, Self::Copernicus];

    /// Its name in the bucket and the manifest.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Alos => "alos",
            Self::Copernicus => "copernicus",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.id() == id)
    }

    /// Its name for people.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Alos => "ALOS",
            Self::Copernicus => "Copernicus",
        }
    }

    /// The geoid its heights are above (T1: never converted).
    #[must_use]
    pub const fn geoid(self) -> &'static str {
        match self {
            Self::Alos => "EGM96",
            Self::Copernicus => "EGM2008",
        }
    }

    /// The credit its licence asks for, in the map's Data sources and About.
    #[must_use]
    pub const fn credit(self) -> &'static str {
        match self {
            Self::Alos => {
                "Original data provided by JAXA (ALOS World 3D 30 m), as ArduPilot's SRTM1 files."
            }
            // The licence's Article 6(b): the data are re-gridded and rounded.
            Self::Copernicus => {
                "produced using Copernicus WorldDEM-30 © DLR e.V. 2010-2014 and © Airbus Defence and Space GmbH 2014-2018 provided under COPERNICUS by the European Union and ESA; all rights reserved"
            }
        }
    }

    /// The liability sentence its licence asks for beside the credit, if any
    /// (Copernicus's Article 6(c)).
    #[must_use]
    pub const fn disclaimer(self) -> Option<&'static str> {
        match self {
            Self::Alos => None,
            Self::Copernicus => Some(
                "The organisations in charge of the Copernicus programme by law or by delegation do not incur any liability for any use of the Copernicus WorldDEM-30",
            ),
        }
    }
}

impl fmt::Display for Dataset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// `N53W005`: the hemisphere letters and the south-west corner's whole
/// degrees, as SRTM names its files.
fn corner_name(lat: i16, lon: i16) -> String {
    format!(
        "{}{:02}{}{:03}",
        if lat < 0 { 'S' } else { 'N' },
        lat.unsigned_abs(),
        if lon < 0 { 'W' } else { 'E' },
        lon.unsigned_abs()
    )
}

fn parse_corner(name: &str) -> Option<(i16, i16)> {
    let bytes = name.as_bytes();
    if bytes.len() != 7 || !name.is_ascii() {
        return None;
    }
    let lat: i16 = name[1..3].parse().ok()?;
    let lon: i16 = name[4..7].parse().ok()?;
    let lat = match bytes[0] {
        b'N' => lat,
        b'S' => -lat,
        _ => return None,
    };
    let lon = match bytes[3] {
        b'E' => lon,
        b'W' => -lon,
        _ => return None,
    };
    Some((lat, lon))
}

/// A 1° square, by its south-west corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tile {
    /// -90 to 89.
    pub lat: i16,
    /// -180 to 179.
    pub lon: i16,
}

impl Tile {
    /// The tile a point is in; `None` off the globe.
    #[must_use]
    pub fn containing(lat_deg: f64, lon_deg: f64) -> Option<Self> {
        if !(-90.0..=90.0).contains(&lat_deg) || !lat_deg.is_finite() || !lon_deg.is_finite() {
            return None;
        }
        let lon = wrap_degrees(lon_deg);
        // The north pole belongs to the last row of tiles.
        #[allow(clippy::cast_possible_truncation, reason = "within ±180")]
        let lat = (lat_deg.floor() as i16).min(89);
        #[allow(clippy::cast_possible_truncation, reason = "within ±180")]
        let lon = (lon.floor() as i16).clamp(-180, 179);
        Some(Self { lat, lon })
    }

    #[must_use]
    pub fn name(self) -> String {
        corner_name(self.lat, self.lon)
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let (lat, lon) = parse_corner(name)?;
        ((-90..=89).contains(&lat) && (-180..=179).contains(&lon)).then_some(Self { lat, lon })
    }

    /// The cell that indexes it.
    #[must_use]
    pub fn cell(self) -> Cell {
        Cell {
            lat: self.lat.div_euclid(CELL_DEGREES) * CELL_DEGREES,
            lon: self.lon.div_euclid(CELL_DEGREES) * CELL_DEGREES,
        }
    }

    /// The tile to the south; none below -90.
    #[must_use]
    pub fn south(self) -> Option<Self> {
        (self.lat > -90).then_some(Self {
            lat: self.lat - 1,
            lon: self.lon,
        })
    }

    /// The tile to the east, across the antimeridian.
    #[must_use]
    pub fn east(self) -> Self {
        Self {
            lat: self.lat,
            lon: if self.lon == 179 { -180 } else { self.lon + 1 },
        }
    }
}

impl fmt::Display for Tile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// A 5° square of tiles, by its south-west corner: what one cell index
/// lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell {
    pub lat: i16,
    pub lon: i16,
}

impl Cell {
    #[must_use]
    pub fn name(self) -> String {
        corner_name(self.lat, self.lon)
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let (lat, lon) = parse_corner(name)?;
        let cell = Self { lat, lon };
        (Tile { lat, lon }.cell() == cell).then_some(cell)
    }
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// One chunk of a tile: row 0 is the north, column 0 the west.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkKey {
    pub tile: Tile,
    pub row: u8,
    pub col: u8,
}

impl ChunkKey {
    /// Its index in a tile's list of chunks: row-major from the north-west.
    #[must_use]
    pub fn index(self) -> usize {
        usize::from(self.row) * usize::from(CHUNKS_PER_SIDE) + usize::from(self.col)
    }

    /// Its path under a release.
    #[must_use]
    pub fn file(self) -> String {
        format!("{}/{}{}.uxdt", self.tile.name(), self.row, self.col)
    }
}

/// A longitude in [-180, 180).
#[must_use]
pub fn wrap_degrees(lon: f64) -> f64 {
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped >= 180.0 { -180.0 } else { wrapped }
}

/// Whether a release id or a file name from the manifest is a plain name
/// that cannot leave its folder.
#[must_use]
pub fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_are_named_as_srtm_names_them() {
        let snowdon = Tile::containing(53.0685, -4.0763).unwrap();
        assert_eq!(snowdon.name(), "N53W005");
        assert_eq!(Tile::parse("N53W005"), Some(snowdon));
        assert_eq!(Tile::containing(-0.5, -0.5).unwrap().name(), "S01W001");
        assert_eq!(Tile::containing(0.0, 0.0).unwrap().name(), "N00E000");
        assert_eq!(Tile::containing(90.0, 180.0).unwrap().name(), "N89W180");
        assert_eq!(
            Tile::containing(-33.8688, 151.2093).unwrap().name(),
            "S34E151"
        );
        assert!(Tile::containing(91.0, 0.0).is_none());
        assert!(Tile::parse("N90E000").is_none());
        assert!(Tile::parse("X53W005").is_none());
    }

    #[test]
    fn cells_hold_five_degrees() {
        let tile = Tile::parse("N53W005").unwrap();
        assert_eq!(tile.cell().name(), "N50W005");
        assert_eq!(Tile::parse("S01W001").unwrap().cell().name(), "S05W005");
        assert_eq!(Tile::parse("N04E004").unwrap().cell().name(), "N00E000");
        assert!(Cell::parse("N53W005").is_none(), "not a cell's corner");
        assert_eq!(Cell::parse("N50W005"), Some(tile.cell()));
    }

    #[test]
    fn neighbours_wrap_at_the_antimeridian() {
        let tile = Tile::parse("N10E179").unwrap();
        assert_eq!(tile.east().name(), "N10W180");
        assert_eq!(Tile::parse("S90E000").unwrap().south(), None);
        assert!((wrap_degrees(181.0) - -179.0).abs() < 1e-9);
        assert!((wrap_degrees(180.0) - -180.0).abs() < 1e-9);
    }

    #[test]
    fn chunk_files_name_their_place() {
        let key = ChunkKey {
            tile: Tile::parse("N53W005").unwrap(),
            row: 7,
            col: 0,
        };
        assert_eq!(key.file(), "N53W005/70.uxdt");
        assert_eq!(key.index(), 56);
        assert!(plain("2026-10-14") && plain("2024_1"));
        assert!(!plain("../x") && !plain("") && !plain("a/b"));
    }
}

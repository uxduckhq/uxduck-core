//! The signed manifest and the cell indexes it names (T2).
//!
//! The manifest is signed with the release key, as the simulator and
//! parameter catalogues are; an unsigned or unverifiable one is an error.
//! It names each dataset's current release, every release's cell indexes
//! with their SHA-256, and the withdrawn releases. A cell index names each
//! of its tiles' source file and chunks with their SHA-256, so every byte
//! UXDuck reads is checked against the signature.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dataset::{CHUNKS_PER_SIDE, Cell, ChunkKey, Dataset, Tile, plain};

/// The manifest schema this build reads.
pub const SCHEMA: u32 = 1;
/// The manifest's path in the bucket.
pub const MANIFEST: &str = "manifest.json";
/// Its signature's.
pub const SIGNATURE: &str = "manifest.json.minisig";
/// Where the bucket is served (ADR-0026 T2).
pub const DEFAULT_BASE_URL: &str = "https://terrain.uxduck.com";
/// A manifest or cell index far larger than this is not one.
pub const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema: u32,
    /// RFC 3339; a manifest older than one already seen is refused.
    pub published_at: String,
    /// Each dataset's current release, by dataset id.
    pub current: BTreeMap<String, String>,
    pub releases: Vec<Release>,
    /// Releases that must not be used, and are deleted where kept.
    #[serde(default)]
    pub withdrawn: Vec<Withdrawn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub dataset: Dataset,
    /// The date of its import, or the source's own version
    /// (`2026-10-14`, `2024_1`).
    pub id: String,
    pub imported_at: String,
    /// Where it was imported from.
    pub source: String,
    /// The geoid its heights are above.
    pub heights: String,
    /// Each cell index, by cell name.
    pub cells: BTreeMap<String, FileRef>,
    /// Cells that are all sea: no index, every height 0 m.
    #[serde(default)]
    pub sea_cells: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Withdrawn {
    pub dataset: Dataset,
    pub id: String,
    pub reason: String,
}

/// A 5° cell's tiles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellIndex {
    pub schema: u32,
    pub dataset: Dataset,
    pub release: String,
    pub cell: String,
    /// Tiles with data, by tile name.
    pub tiles: BTreeMap<String, TileEntry>,
    /// Tiles that are sea: every height 0 m. A tile in neither list has no
    /// data.
    #[serde(default)]
    pub sea: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TileEntry {
    /// Samples per degree north to south, and west to east.
    pub rows: u16,
    pub cols: u16,
    /// The file it was made from.
    pub source: SourceFile,
    /// The 64 chunks' SHA-256, row-major from the north-west; empty for a
    /// chunk with no data, which is not stored.
    pub chunks: Vec<String>,
    /// Their sizes in bytes, 0 for one not stored, so a download's size is
    /// known before it starts (S120).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFile {
    pub name: String,
    pub sha256: String,
}

impl Manifest {
    /// A dataset's current release.
    #[must_use]
    pub fn current(&self, dataset: Dataset) -> Option<&Release> {
        let id = self.current.get(dataset.id())?;
        self.releases
            .iter()
            .find(|r| r.dataset == dataset && &r.id == id)
    }

    #[must_use]
    pub fn is_withdrawn(&self, dataset: Dataset, id: &str) -> bool {
        self.withdrawn
            .iter()
            .any(|w| w.dataset == dataset && w.id == id)
    }
}

impl Release {
    /// Its folder in the bucket.
    #[must_use]
    pub fn prefix(&self) -> String {
        format!("{}/{}/", self.dataset.id(), self.id)
    }

    /// A cell index's path.
    #[must_use]
    pub fn cell_path(&self, cell: Cell) -> String {
        format!("{}cells/{}.json", self.prefix(), cell.name())
    }

    /// A chunk's path.
    #[must_use]
    pub fn chunk_path(&self, key: ChunkKey) -> String {
        format!("{}{}", self.prefix(), key.file())
    }

    #[must_use]
    pub fn is_sea_cell(&self, cell: Cell) -> bool {
        let name = cell.name();
        self.sea_cells.contains(&name)
    }
}

/// What a tile is, by its cell index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileKind<'a> {
    Data(&'a TileEntry),
    Sea,
    NoData,
}

impl CellIndex {
    #[must_use]
    pub fn tile(&self, tile: Tile) -> TileKind<'_> {
        let name = tile.name();
        if let Some(entry) = self.tiles.get(&name) {
            TileKind::Data(entry)
        } else if self.sea.contains(&name) {
            TileKind::Sea
        } else {
            TileKind::NoData
        }
    }
}

/// Lowercase hex SHA-256.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Verifies a manifest against its minisign signature and reads it.
///
/// # Errors
/// A bad key, signature or document, a schema this build does not read,
/// or a current release that is missing or withdrawn.
pub fn verify(bytes: &[u8], signature: &str, public_key: &str) -> Result<Manifest, String> {
    let key = minisign_verify::PublicKey::from_base64(public_key.trim())
        .map_err(|e| format!("the public key: {e}"))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|e| format!("the terrain manifest's signature: {e}"))?;
    key.verify(bytes, &signature, false)
        .map_err(|e| format!("the terrain manifest's signature does not verify: {e}"))?;
    read(bytes)
}

/// Reads a manifest whose signature was checked.
///
/// # Errors
/// As [`verify`], less the signature.
pub fn read(bytes: &[u8]) -> Result<Manifest, String> {
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|e| format!("the terrain manifest is not valid: {e}"))?;
    if manifest.schema != SCHEMA {
        return Err(format!(
            "terrain manifest schema {} is not one this build reads ({SCHEMA})",
            manifest.schema
        ));
    }
    for release in &manifest.releases {
        if !plain(&release.id) {
            return Err(format!(
                "the terrain release {:?} is not a plain name",
                release.id
            ));
        }
        for (cell, file) in &release.cells {
            if Cell::parse(cell).is_none() || !hex64(&file.sha256) {
                return Err(format!("the terrain cell {cell:?} is not valid"));
            }
        }
    }
    for (dataset, id) in &manifest.current {
        let dataset = Dataset::from_id(dataset)
            .ok_or_else(|| format!("the terrain manifest names an unknown dataset {dataset:?}"))?;
        if manifest.current(dataset).is_none() {
            return Err(format!(
                "{dataset}'s current release {id} is not in the manifest"
            ));
        }
        if manifest.is_withdrawn(dataset, id) {
            return Err(format!("{dataset}'s current release {id} is withdrawn"));
        }
    }
    Ok(manifest)
}

/// Reads a cell index whose bytes matched the manifest.
///
/// # Errors
/// When it is not the index the manifest named.
pub fn read_cell(bytes: &[u8], release: &Release, cell: Cell) -> Result<CellIndex, String> {
    let index: CellIndex = serde_json::from_slice(bytes)
        .map_err(|e| format!("the terrain cell {cell} is not valid: {e}"))?;
    if index.schema != SCHEMA
        || index.dataset != release.dataset
        || index.release != release.id
        || index.cell != cell.name()
    {
        return Err(format!(
            "the terrain cell {cell} is not the one the manifest names"
        ));
    }
    let chunks = usize::from(CHUNKS_PER_SIDE) * usize::from(CHUNKS_PER_SIDE);
    for (name, tile) in &index.tiles {
        let fits = Tile::parse(name).is_some_and(|t| t.cell() == cell)
            && tile.rows % u16::from(CHUNKS_PER_SIDE) == 0
            && tile.cols % u16::from(CHUNKS_PER_SIDE) == 0
            && tile.rows > 0
            && tile.cols > 0
            && tile.chunks.len() == chunks
            && tile.chunks.iter().all(|c| c.is_empty() || hex64(c))
            && (tile.sizes.is_empty() || tile.sizes.len() == chunks);
        if !fits {
            return Err(format!("the terrain tile {name} in {cell} is not valid"));
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> Release {
        Release {
            dataset: Dataset::Alos,
            id: "2026-10-14".into(),
            imported_at: "2026-10-14T00:00:00Z".into(),
            source: "https://terrain.ardupilot.org/SRTM1/".into(),
            heights: "EGM96".into(),
            cells: BTreeMap::from([(
                "N50W005".into(),
                FileRef {
                    sha256: "a".repeat(64),
                    size: 10,
                },
            )]),
            sea_cells: vec!["N30W040".into()],
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            schema: SCHEMA,
            published_at: "2026-10-14T00:00:00Z".into(),
            current: BTreeMap::from([("alos".into(), "2026-10-14".into())]),
            releases: vec![release()],
            withdrawn: vec![],
        }
    }

    #[test]
    fn paths_stay_in_their_release() {
        let release = release();
        let tile = Tile::parse("N53W005").unwrap();
        assert_eq!(
            release.cell_path(tile.cell()),
            "alos/2026-10-14/cells/N50W005.json"
        );
        assert_eq!(
            release.chunk_path(ChunkKey {
                tile,
                row: 3,
                col: 4
            }),
            "alos/2026-10-14/N53W005/34.uxdt"
        );
        assert!(release.is_sea_cell(Cell::parse("N30W040").unwrap()));
    }

    #[test]
    fn a_manifest_names_live_releases_only() {
        let bytes = serde_json::to_vec(&manifest()).unwrap();
        assert_eq!(
            read(&bytes).unwrap().current(Dataset::Alos).unwrap().id,
            "2026-10-14"
        );

        let mut withdrawn = manifest();
        withdrawn.withdrawn.push(Withdrawn {
            dataset: Dataset::Alos,
            id: "2026-10-14".into(),
            reason: "bad".into(),
        });
        let error = read(&serde_json::to_vec(&withdrawn).unwrap()).unwrap_err();
        assert!(error.contains("withdrawn"), "{error}");

        let mut escaping = manifest();
        escaping.releases[0].id = "../x".into();
        assert!(read(&serde_json::to_vec(&escaping).unwrap()).is_err());

        let mut newer = manifest();
        newer.schema = 2;
        assert!(
            read(&serde_json::to_vec(&newer).unwrap())
                .unwrap_err()
                .contains("schema")
        );
    }

    #[test]
    fn a_cell_index_must_be_the_one_named() {
        let release = release();
        let cell = Cell::parse("N50W005").unwrap();
        let mut index = CellIndex {
            schema: SCHEMA,
            dataset: Dataset::Alos,
            release: "2026-10-14".into(),
            cell: "N50W005".into(),
            tiles: BTreeMap::from([(
                "N53W005".into(),
                TileEntry {
                    rows: 3600,
                    cols: 3600,
                    source: SourceFile {
                        name: "N53W005.hgt.zip".into(),
                        sha256: "b".repeat(64),
                    },
                    chunks: vec![String::new(); 64],
                    sizes: Vec::new(),
                },
            )]),
            sea: vec!["N50W009".into()],
        };
        let read = read_cell(&serde_json::to_vec(&index).unwrap(), &release, cell).unwrap();
        assert!(matches!(
            read.tile(Tile::parse("N53W005").unwrap()),
            TileKind::Data(_)
        ));
        assert_eq!(read.tile(Tile::parse("N50W009").unwrap()), TileKind::Sea);
        assert_eq!(read.tile(Tile::parse("N54W001").unwrap()), TileKind::NoData);

        index.cell = "N45W005".into();
        assert!(read_cell(&serde_json::to_vec(&index).unwrap(), &release, cell).is_err());
    }

    #[test]
    fn hashes_are_lowercase_hex() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(hex64(&sha256(b"x")) && !hex64(&"A".repeat(64)));
    }
}

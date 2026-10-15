//! Terrain heights (ADR-0026 T2, T3): UXDuck's own copy of two datasets,
//! ALOS (ArduPilot's SRTM1) and Copernicus GLO-30, published as small
//! chunks under a signed manifest.
//!
//! Nothing here does I/O. [`Terrain`] says what it needs as [`Order`]s and
//! the daemon (files in the data folder) or the browser (the Origin Private
//! File System) answers through [`Terrain::arrived`]. The same checks run in
//! both: the manifest's signature, each file's SHA-256, withdrawn releases.
//!
//! Layout of the bucket, relative to its base URL:
//!
//! - `manifest.json` and `manifest.json.minisig`: every release, the
//!   current one per dataset, the withdrawn ones, and each cell index's
//!   SHA-256.
//! - `<dataset>/<release>/cells/<cell>.json`: a 5° cell's tiles, each with
//!   its source file's SHA-256 and its 64 chunks' SHA-256.
//! - `<dataset>/<release>/<tile>/<row><col>.uxdt`: one chunk, an eighth of
//!   a degree square at the dataset's own resolution ([`chunk`]).

pub mod cache;
pub mod chunk;
pub mod cover;
pub mod dataset;
pub mod grid;
pub mod lookup;
pub mod manifest;
pub mod publish;

pub use cache::{Answer, Arrived, FetchError, Order, Problem, Terrain, Unavailable};
pub use dataset::{Cell, ChunkKey, Dataset, Tile};

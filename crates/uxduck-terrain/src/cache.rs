//! The cache (T3): what UXDuck knows of the manifest, the cell indexes and
//! the chunks, and what it asks its host for.
//!
//! The host keeps files by key (the bucket's paths) and fetches them from
//! the bucket; it never checks anything. Everything read from it or from
//! the bucket is checked here against the signed manifest before it is
//! used or kept: a file that does not match is refused, and one fetched
//! that does not match is a problem to show.
//!
//! The manifest is read from the host's store and fetched at start, and
//! fetched again every few hours while online. The newer verified one is
//! used and kept, so offline the last one seen still serves, and an older
//! signed manifest (one that might still list a withdrawn release) is
//! refused. Releases the manifest withdraws are removed from the store.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::chunk::Chunk;
use crate::dataset::{Cell, ChunkKey, Dataset, Tile};
use crate::lookup::{self, Ground, Need, Source, TileGrid};
use crate::manifest::{
    self, CellIndex, MANIFEST, MAX_INDEX_BYTES, Manifest, Release, SIGNATURE, TileKind,
};

/// How often the manifest is fetched again while online.
pub const RECHECK_US: u64 = 6 * 3600 * 1_000_000;
/// How soon a failed fetch may be tried again.
pub const RETRY_US: u64 = 30 * 1_000_000;
/// Chunks kept in memory; each is at most a few hundred kilobytes.
pub const CHUNKS_IN_MEMORY: usize = 96;

/// What the cache asks of its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Order {
    /// Read a kept file; answered with [`Arrived::Read`].
    Read { key: String },
    /// Fetch a file from the bucket; answered with [`Arrived::Fetched`].
    Fetch { key: String },
    /// Keep a file that was checked.
    Write { key: String, bytes: Vec<u8> },
    /// Remove every kept file under a prefix (a withdrawn release).
    Remove { prefix: String },
}

/// Why a fetch failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchError {
    /// No connection, as opposed to the server refusing.
    pub offline: bool,
    pub message: String,
}

/// What the host answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrived {
    /// A kept file, or `None` when there is none.
    Read { key: String, bytes: Option<Vec<u8>> },
    Fetched {
        key: String,
        result: Result<Vec<u8>, FetchError>,
    },
}

/// Why there is no height to give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Not kept here, and offline.
    Offline,
    /// The manifest or a file could not be had or did not check out.
    Failed(String),
}

/// The answer for a point.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub dataset: Dataset,
    /// The release answered from, when there is a manifest.
    pub release: Option<String>,
    pub ground: Result<Ground, Unavailable>,
}

/// Something people should hear about, as a notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The same condition is raised once.
    pub condition: String,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug)]
enum Slot<T> {
    Reading,
    Fetching,
    Ready(T),
    Failed { why: Unavailable, at_us: u64 },
}

#[derive(Debug, Default)]
struct Pair {
    manifest: Option<Vec<u8>>,
    signature: Option<Vec<u8>>,
    /// Both halves answered (a missing half is `None` above).
    manifest_in: bool,
    signature_in: bool,
}

#[derive(Debug, Default)]
struct Fetch {
    manifest: Option<Result<Vec<u8>, FetchError>>,
    signature: Option<Result<Vec<u8>, FetchError>>,
}

/// The terrain cache. One per daemon.
#[derive(Debug)]
pub struct Terrain {
    key: Option<String>,
    manifest: Option<Arc<Manifest>>,
    /// When the bucket's manifest last verified.
    checked_at_us: Option<u64>,
    /// Why there is no manifest, or why the latest fetch was refused.
    manifest_error: Option<Unavailable>,
    reading: bool,
    stored: Pair,
    fetching: Option<u64>,
    /// When the manifest was last fetched, whatever came of it.
    last_try_us: Option<u64>,
    fetched: Fetch,
    cells: HashMap<String, Slot<Arc<CellIndex>>>,
    chunks: HashMap<String, Slot<Arc<Chunk>>>,
    /// Ready chunks, oldest first, for eviction.
    loaded: VecDeque<String>,
    /// Chunks being kept for an area or a mission (S120), by path; never
    /// held in memory.
    keeping: HashMap<String, Slot<u32>>,
    jobs: std::collections::BTreeMap<u64, keep::Job>,
    next_job: u64,
    orders: Vec<Order>,
    problems: Vec<Problem>,
    resolved_problems: Vec<String>,
}

impl Terrain {
    /// A cache that verifies the manifest with `public_key` (minisign,
    /// base64). Without a key nothing verifies, and every answer says so.
    #[must_use]
    pub fn new(public_key: Option<String>) -> Self {
        Self {
            key: public_key.filter(|k| !k.trim().is_empty()),
            manifest: None,
            checked_at_us: None,
            manifest_error: None,
            reading: false,
            stored: Pair::default(),
            fetching: None,
            last_try_us: None,
            fetched: Fetch::default(),
            cells: HashMap::new(),
            chunks: HashMap::new(),
            loaded: VecDeque::new(),
            keeping: HashMap::new(),
            jobs: std::collections::BTreeMap::new(),
            next_job: 0,
            orders: Vec::new(),
            problems: Vec::new(),
            resolved_problems: Vec::new(),
        }
    }

    /// Reads the kept manifest and fetches the bucket's.
    pub fn start(&mut self, now_us: u64) {
        if self.key.is_none() {
            // Said by every answer, not as a notice: development builds
            // have no release key.
            self.manifest_error = Some(Unavailable::Failed(NO_KEY.into()));
            return;
        }
        self.reading = true;
        self.stored = Pair::default();
        self.orders.push(Order::Read {
            key: MANIFEST.into(),
        });
        self.orders.push(Order::Read {
            key: SIGNATURE.into(),
        });
        self.fetch_manifest(now_us);
    }

    fn fetch_manifest(&mut self, now_us: u64) {
        self.fetching = Some(now_us);
        self.fetched = Fetch::default();
        self.orders.push(Order::Fetch {
            key: MANIFEST.into(),
        });
        self.orders.push(Order::Fetch {
            key: SIGNATURE.into(),
        });
    }

    /// Fetches the manifest again when it is due: every few hours, or soon
    /// after a failed fetch.
    pub fn refresh_if_due(&mut self, now_us: u64) {
        if self.key.is_none() || self.fetching.is_some() {
            return;
        }
        let due = match (self.checked_at_us, &self.manifest_error) {
            (_, Some(_)) => self
                .last_try_us
                .is_none_or(|at| now_us.saturating_sub(at) >= RETRY_US),
            (Some(at), None) => now_us.saturating_sub(at) >= RECHECK_US,
            (None, None) => true,
        };
        if due {
            self.fetch_manifest(now_us);
        }
    }

    /// What the host must do; drained by each call.
    pub fn take_orders(&mut self) -> Vec<Order> {
        std::mem::take(&mut self.orders)
    }

    /// Problems to show once each.
    pub fn take_problems(&mut self) -> Vec<Problem> {
        std::mem::take(&mut self.problems)
    }

    /// Previously reported conditions whose data has now verified.
    pub fn take_resolved_problems(&mut self) -> Vec<String> {
        std::mem::take(&mut self.resolved_problems)
    }

    /// When the bucket's manifest last verified, in the host clock's
    /// microseconds.
    #[must_use]
    pub fn checked_at_us(&self) -> Option<u64> {
        self.checked_at_us
    }

    /// Whether the manifest is still being read or fetched for the first
    /// time.
    #[must_use]
    pub fn loading(&self) -> bool {
        self.manifest.is_none() && (self.reading || self.fetching.is_some())
    }

    /// The manifest in use.
    #[must_use]
    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_deref()
    }

    /// The host answered.
    pub fn arrived(&mut self, arrived: Arrived, now_us: u64) {
        self.arrived_inner(arrived, now_us);
        self.advance_jobs(now_us);
    }

    fn arrived_inner(&mut self, arrived: Arrived, now_us: u64) {
        match arrived {
            Arrived::Read { key, bytes } => {
                if key == MANIFEST {
                    self.stored.manifest = bytes;
                    self.stored.manifest_in = true;
                    self.stored_manifest();
                } else if key == SIGNATURE {
                    self.stored.signature = bytes;
                    self.stored.signature_in = true;
                    self.stored_manifest();
                } else {
                    self.file_read(&key, bytes);
                }
            }
            Arrived::Fetched { key, result } => {
                if key == MANIFEST {
                    self.fetched.manifest = Some(result);
                    self.fetched_manifest(now_us);
                } else if key == SIGNATURE {
                    self.fetched.signature = Some(result);
                    self.fetched_manifest(now_us);
                } else {
                    self.file_fetched(&key, result, now_us);
                }
            }
        }
    }

    fn stored_manifest(&mut self) {
        if !(self.stored.manifest_in && self.stored.signature_in) {
            return;
        }
        self.reading = false;
        let (Some(bytes), Some(signature), Some(key)) =
            (&self.stored.manifest, &self.stored.signature, &self.key)
        else {
            return;
        };
        // A kept manifest that no longer verifies is ignored; the fetched
        // one replaces it.
        if let Ok(manifest) = manifest::verify(bytes, &String::from_utf8_lossy(signature), key) {
            self.adopt(manifest);
        }
        self.stored = Pair::default();
    }

    fn fetched_manifest(&mut self, now_us: u64) {
        let (Some(bytes), Some(signature)) = (&self.fetched.manifest, &self.fetched.signature)
        else {
            return;
        };
        self.fetching = None;
        self.last_try_us = Some(now_us);
        let result = match (bytes, signature) {
            (Ok(bytes), Ok(signature)) => {
                let key = self.key.clone().unwrap_or_default();
                manifest::verify(bytes, &String::from_utf8_lossy(signature), &key)
                    .map(|m| (m, bytes.clone(), signature.clone()))
                    .map_err(Unavailable::Failed)
            }
            (Err(e), _) | (_, Err(e)) if e.offline => Err(Unavailable::Offline),
            (Err(e), _) | (_, Err(e)) => Err(Unavailable::Failed(format!(
                "the terrain manifest could not be fetched: {}",
                e.message
            ))),
        };
        self.fetched = Fetch::default();
        match result {
            Ok((fresh, bytes, signature)) => {
                if let Some(seen) = &self.manifest
                    && fresh.published_at < seen.published_at
                {
                    let why = format!(
                        "the terrain manifest was published at {}, before one already seen ({}). It is ignored",
                        fresh.published_at, seen.published_at
                    );
                    self.refuse_manifest(why);
                    return;
                }
                self.orders.push(Order::Write {
                    key: MANIFEST.into(),
                    bytes,
                });
                self.orders.push(Order::Write {
                    key: SIGNATURE.into(),
                    bytes: signature,
                });
                self.checked_at_us = Some(now_us);
                self.manifest_error = None;
                self.resolved_problems.push("terrain:manifest".into());
                self.adopt(fresh);
            }
            Err(Unavailable::Offline) => {
                if self.manifest.is_none() {
                    self.manifest_error = Some(Unavailable::Offline);
                }
            }
            Err(Unavailable::Failed(why)) => self.refuse_manifest(why),
        }
    }

    fn refuse_manifest(&mut self, why: String) {
        self.problems.push(Problem {
            condition: "terrain:manifest".into(),
            title: "UXDuck could not check its terrain data".into(),
            body: format!(
                "{why}. Saved terrain heights remain available. Terrain for new places is unavailable until this is fixed."
            ),
        });
        self.manifest_error = Some(Unavailable::Failed(why));
    }

    /// Uses a verified manifest, if it is at least as new as the one in use.
    fn adopt(&mut self, manifest: Manifest) {
        if let Some(seen) = &self.manifest
            && manifest.published_at < seen.published_at
        {
            return;
        }
        for withdrawn in &manifest.withdrawn {
            let prefix = format!("{}/{}/", withdrawn.dataset.id(), withdrawn.id);
            self.cells.retain(|key, _| !key.starts_with(&prefix));
            self.chunks.retain(|key, _| !key.starts_with(&prefix));
            self.loaded.retain(|key| !key.starts_with(&prefix));
            self.orders.push(Order::Remove { prefix });
        }
        self.manifest = Some(Arc::new(manifest));
    }

    fn file_read(&mut self, key: &str, bytes: Option<Vec<u8>>) {
        self.keep_read(key, bytes.as_deref());
        if let Some(Slot::Reading) = self.cells.get(key) {
            match bytes.and_then(|b| self.check_cell(key, &b).ok()) {
                Some(index) => {
                    self.cells.insert(key.into(), Slot::Ready(Arc::new(index)));
                }
                None => self.fetch(key, true),
            }
        } else if let Some(Slot::Reading) = self.chunks.get(key) {
            match bytes.and_then(|b| self.check_chunk(key, &b).ok()) {
                Some(chunk) => self.keep_chunk(key, chunk),
                None => self.fetch(key, false),
            }
        }
    }

    fn fetch(&mut self, key: &str, cell: bool) {
        if cell {
            fetching(&mut self.cells, key);
        } else {
            fetching(&mut self.chunks, key);
        }
        self.orders.push(Order::Fetch { key: key.into() });
    }

    fn file_fetched(&mut self, key: &str, result: Result<Vec<u8>, FetchError>, now_us: u64) {
        self.keep_fetched(key, &result, now_us);
        let cell = matches!(self.cells.get(key), Some(Slot::Fetching));
        let chunk = matches!(self.chunks.get(key), Some(Slot::Fetching));
        if !cell && !chunk {
            return;
        }
        let checked = match result {
            Ok(bytes) => {
                let outcome = if cell {
                    self.check_cell(key, &bytes).map(|index| {
                        self.cells.insert(key.into(), Slot::Ready(Arc::new(index)));
                    })
                } else {
                    self.check_chunk(key, &bytes)
                        .map(|chunk| self.keep_chunk(key, chunk))
                };
                match outcome {
                    Ok(()) => {
                        self.resolved_problems.push(format!("terrain:file:{key}"));
                        self.orders.push(Order::Write {
                            key: key.into(),
                            bytes,
                        });
                        return;
                    }
                    Err(why) => {
                        self.problems.push(Problem {
                            condition: format!("terrain:file:{key}"),
                            title: "Terrain data did not match its manifest".into(),
                            body: format!("{why}. It was not used."),
                        });
                        Unavailable::Failed(why)
                    }
                }
            }
            Err(e) if e.offline => Unavailable::Offline,
            Err(e) => Unavailable::Failed(e.message),
        };
        if cell {
            self.cells.insert(
                key.into(),
                Slot::Failed {
                    why: checked,
                    at_us: now_us,
                },
            );
        } else {
            self.chunks.insert(
                key.into(),
                Slot::Failed {
                    why: checked,
                    at_us: now_us,
                },
            );
        }
    }

    fn release_for(&self, key: &str) -> Option<&Release> {
        let manifest = self.manifest.as_deref()?;
        manifest
            .releases
            .iter()
            .find(|r| key.starts_with(&r.prefix()))
    }

    fn check_cell(&self, key: &str, bytes: &[u8]) -> Result<CellIndex, String> {
        let release = self
            .release_for(key)
            .ok_or_else(|| format!("{key} is in no release of the manifest"))?;
        let name = key
            .strip_prefix(&format!("{}cells/", release.prefix()))
            .and_then(|rest| rest.strip_suffix(".json"))
            .ok_or_else(|| format!("{key} is not a cell index"))?;
        let cell = Cell::parse(name).ok_or_else(|| format!("{key} is not a cell index"))?;
        let file = release
            .cells
            .get(name)
            .ok_or_else(|| format!("{key} is not in the manifest"))?;
        if bytes.len() > MAX_INDEX_BYTES || manifest::sha256(bytes) != file.sha256 {
            return Err(format!("{key} does not match the manifest's checksum"));
        }
        manifest::read_cell(bytes, release, cell)
    }

    fn check_chunk(&self, key: &str, bytes: &[u8]) -> Result<Chunk, String> {
        let release = self
            .release_for(key)
            .ok_or_else(|| format!("{key} is in no release of the manifest"))?;
        let rest = key
            .strip_prefix(&release.prefix())
            .ok_or_else(|| format!("{key} is not a chunk"))?;
        let (tile, file) = rest
            .split_once('/')
            .ok_or_else(|| format!("{key} is not a chunk"))?;
        let tile = Tile::parse(tile).ok_or_else(|| format!("{key} is not a chunk"))?;
        let digits = file.strip_suffix(".uxdt").unwrap_or_default().as_bytes();
        let [row @ b'0'..=b'7', col @ b'0'..=b'7'] = digits else {
            return Err(format!("{key} is not a chunk"));
        };
        let chunk_key = ChunkKey {
            tile,
            row: row - b'0',
            col: col - b'0',
        };
        let cell_key = release.cell_path(tile.cell());
        let Some(Slot::Ready(index)) = self.cells.get(&cell_key) else {
            return Err(format!("{key}'s cell index is not here"));
        };
        let TileKind::Data(entry) = index.tile(tile) else {
            return Err(format!("{key} is not in its cell index"));
        };
        let want = &entry.chunks[chunk_key.index()];
        if want.is_empty() || manifest::sha256(bytes) != *want {
            return Err(format!("{key} does not match the manifest's checksum"));
        }
        let chunk = Chunk::decode(bytes)?;
        let per = u16::from(crate::dataset::CHUNKS_PER_SIDE);
        if chunk.rows != entry.rows / per || chunk.cols != entry.cols / per {
            return Err(format!("{key} is not the size its cell index says"));
        }
        Ok(chunk)
    }

    fn keep_chunk(&mut self, key: &str, chunk: Chunk) {
        self.chunks.insert(key.into(), Slot::Ready(Arc::new(chunk)));
        self.loaded.push_back(key.into());
        while self.loaded.len() > CHUNKS_IN_MEMORY {
            if let Some(old) = self.loaded.pop_front() {
                // Read again from the store when next wanted.
                self.chunks.remove(&old);
            }
        }
    }

    /// The ground at a point from a dataset's current release, or `None`
    /// while what it needs is read or fetched: ask again after the next
    /// [`arrived`](Self::arrived).
    pub fn at(&mut self, dataset: Dataset, lat: f64, lon: f64, now_us: u64) -> Option<Answer> {
        self.refresh_if_due(now_us);
        let answer = |release: Option<String>, ground| {
            Some(Answer {
                dataset,
                release,
                ground,
            })
        };
        let Some(manifest) = self.manifest.clone() else {
            if self.loading() {
                return None;
            }
            let why = self
                .manifest_error
                .clone()
                .unwrap_or(Unavailable::Failed("there is no terrain manifest".into()));
            return answer(None, Err(why));
        };
        let Some(release) = manifest.current(dataset) else {
            return answer(
                None,
                Err(Unavailable::Failed(format!(
                    "the manifest has no {dataset} release"
                ))),
            );
        };
        let needs = match lookup::ground(
            &View {
                terrain: self,
                release,
            },
            lat,
            lon,
        ) {
            Ok(ground) => return answer(Some(release.id.clone()), Ok(ground)),
            Err(needs) => needs,
        };
        let mut waiting = false;
        for need in needs {
            let (key, cell) = match need {
                Need::Cell(cell) => (release.cell_path(cell), true),
                Need::Chunk(chunk) => (release.chunk_path(chunk), false),
            };
            let wanted = if cell {
                want(&mut self.cells, key, &mut self.orders, now_us)
            } else {
                want(&mut self.chunks, key, &mut self.orders, now_us)
            };
            match wanted {
                Wanted::Waiting => waiting = true,
                Wanted::Failed(why) => return answer(Some(release.id.clone()), Err(why)),
                Wanted::Ready => {}
            }
        }
        if waiting {
            None
        } else {
            answer(
                Some(release.id.clone()),
                Err(Unavailable::Failed("terrain lookup did not settle".into())),
            )
        }
    }
}

fn fetching<T>(slots: &mut HashMap<String, Slot<T>>, key: &str) {
    if let Some(slot) = slots.get_mut(key) {
        *slot = Slot::Fetching;
    }
}

enum Wanted {
    Waiting,
    Failed(Unavailable),
    /// Here after all; a ready file is never needed.
    Ready,
}

/// Asks for a file a lookup needs, unless it is on its way or failed
/// lately.
fn want<T>(
    slots: &mut HashMap<String, Slot<T>>,
    key: String,
    orders: &mut Vec<Order>,
    now_us: u64,
) -> Wanted {
    match slots.get(&key) {
        Some(Slot::Reading | Slot::Fetching) => Wanted::Waiting,
        Some(Slot::Ready(_)) => Wanted::Ready,
        Some(Slot::Failed { why, at_us }) if now_us.saturating_sub(*at_us) < RETRY_US => {
            Wanted::Failed(why.clone())
        }
        None | Some(Slot::Failed { .. }) => {
            slots.insert(key.clone(), Slot::Reading);
            orders.push(Order::Read { key });
            Wanted::Waiting
        }
    }
}

#[path = "keep.rs"]
mod keep;
pub use keep::JobStatus;

/// Why this build answers no terrain.
pub const NO_KEY: &str = "This build has no public key to check the terrain manifest with, so it answers no terrain heights";

/// The cache as the lookup reads it, for one release.
struct View<'a> {
    terrain: &'a Terrain,
    release: &'a Release,
}

impl Source for View<'_> {
    fn tile(&self, tile: Tile) -> Result<TileGrid, Need> {
        let cell = tile.cell();
        if self.release.is_sea_cell(cell) {
            return Ok(TileGrid::Sea);
        }
        if !self.release.cells.contains_key(&cell.name()) {
            return Ok(TileGrid::NoData);
        }
        match self.terrain.cells.get(&self.release.cell_path(cell)) {
            Some(Slot::Ready(index)) => Ok(match index.tile(tile) {
                TileKind::Data(entry) => TileGrid::Data {
                    rows: entry.rows,
                    cols: entry.cols,
                },
                TileKind::Sea => TileGrid::Sea,
                TileKind::NoData => TileGrid::NoData,
            }),
            _ => Err(Need::Cell(cell)),
        }
    }

    fn chunk(&self, key: ChunkKey) -> Result<Option<&Chunk>, Need> {
        let path = self.release.cell_path(key.tile.cell());
        if let Some(Slot::Ready(index)) = self.terrain.cells.get(&path)
            && let TileKind::Data(entry) = index.tile(key.tile)
            && entry.chunks[key.index()].is_empty()
        {
            return Ok(None);
        }
        match self.terrain.chunks.get(&self.release.chunk_path(key)) {
            Some(Slot::Ready(chunk)) => Ok(Some(chunk)),
            _ => Err(Need::Chunk(key)),
        }
    }
}

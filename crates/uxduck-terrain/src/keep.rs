//! Keeping terrain for later (S120, ADR-0026 T6): an area kept offline, or
//! the chunks along a mission's legs, fetched and checked like any other
//! and written to the host's store, but never held in memory. A job counts
//! what is done; a chunk already kept and still matching is done at once.

use std::collections::{BTreeMap, BTreeSet};

use super::{Order, Slot, Terrain, Unavailable};
use crate::dataset::{ChunkKey, Dataset, Tile};
use crate::manifest::{self, TileKind};

/// What a job is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStatus {
    /// Chunks with data to keep, once the cells say which have data.
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// Bytes of the chunks done.
    pub bytes: u64,
    /// Whether every chunk is done or failed.
    pub finished: bool,
    /// Why the first that failed did.
    pub why: Option<Unavailable>,
}

#[derive(Debug)]
pub(super) struct Job {
    dataset: Dataset,
    keys: Vec<ChunkKey>,
    done: BTreeMap<ChunkKey, u32>,
    failed: BTreeMap<ChunkKey, Unavailable>,
    /// Chunks with no data to keep (void, sea or not covered).
    empty: BTreeSet<ChunkKey>,
    /// Only these files determine whether this job is saved for later.
    files: BTreeSet<String>,
    release: Option<String>,
}

/// What a chunk needs.
enum State {
    /// Nothing to keep there.
    Empty,
    Done(u32),
    Failed(Unavailable),
    Waiting,
}

impl Terrain {
    /// Keeps chunks of a dataset's current release in the host's store.
    /// Returns the job's id, to follow with [`job`](Self::job).
    pub fn keep(
        &mut self,
        dataset: Dataset,
        keys: impl IntoIterator<Item = ChunkKey>,
        now_us: u64,
    ) -> u64 {
        self.next_job += 1;
        self.jobs.insert(
            self.next_job,
            Job {
                dataset,
                keys: keys.into_iter().collect(),
                done: BTreeMap::new(),
                failed: BTreeMap::new(),
                empty: BTreeSet::new(),
                files: BTreeSet::from([manifest::MANIFEST.into(), manifest::SIGNATURE.into()]),
                release: None,
            },
        );
        self.advance_jobs(now_us);
        self.next_job
    }

    /// How a job is going.
    #[must_use]
    pub fn job(&self, id: u64) -> Option<JobStatus> {
        let job = self.jobs.get(&id)?;
        let total = job.keys.len() - job.empty.len();
        let storage_error = self
            .storage_errors
            .iter()
            .find(|(key, _)| job.files.contains(*key))
            .map(|(_, why)| why);
        Some(JobStatus {
            total,
            done: job.done.len(),
            failed: job.failed.len().max(usize::from(storage_error.is_some())),
            bytes: job.done.values().map(|&b| u64::from(b)).sum(),
            finished: job.done.len() + job.failed.len() + job.empty.len() == job.keys.len()
                && !self
                    .pending_writes
                    .keys()
                    .any(|key| job.files.contains(key)),
            why: storage_error
                .map(|why| Unavailable::Failed(why.clone()))
                .or_else(|| job.failed.values().next().cloned()),
        })
    }

    /// Forgets a job; what it kept stays kept.
    pub fn forget_job(&mut self, id: u64) {
        self.jobs.remove(&id);
        if self.jobs.is_empty() {
            self.keeping.clear();
        }
    }

    /// Chunks and bytes a set of chunks would take, or `None` while the
    /// cells that say so are on their way.
    ///
    /// # Errors
    /// When there is no manifest or no cell index to go by.
    pub fn size(
        &mut self,
        dataset: Dataset,
        keys: &BTreeSet<ChunkKey>,
        now_us: u64,
    ) -> Option<Result<(usize, u64), Unavailable>> {
        let manifest = self.manifest.clone()?;
        let Some(release) = manifest.current(dataset) else {
            return Some(Err(Unavailable::Failed(format!(
                "the manifest has no {dataset} release"
            ))));
        };
        let mut chunks = 0;
        let mut bytes = 0u64;
        let mut waiting = false;
        for cell in keys.iter().map(|k| k.tile.cell()).collect::<BTreeSet<_>>() {
            if release.is_sea_cell(cell) || !release.cells.contains_key(&cell.name()) {
                continue;
            }
            let path = release.cell_path(cell);
            match self.cells.get(&path) {
                Some(Slot::Ready(index)) => {
                    for key in keys.iter().filter(|k| k.tile.cell() == cell) {
                        if let TileKind::Data(entry) = index.tile(key.tile)
                            && !entry.chunks[key.index()].is_empty()
                        {
                            chunks += 1;
                            bytes += u64::from(entry.sizes.get(key.index()).copied().unwrap_or(0));
                        }
                    }
                }
                Some(Slot::Failed { why, .. }) => return Some(Err(why.clone())),
                _ => {
                    waiting = true;
                    super::want(&mut self.cells, path, &mut self.orders, now_us);
                }
            }
        }
        (!waiting).then_some(Ok((chunks, bytes)))
    }

    /// What one chunk of a job needs, asking for it if need be.
    fn state(&mut self, dataset: Dataset, key: ChunkKey, now_us: u64) -> State {
        let Some(manifest) = self.manifest.clone() else {
            return State::Waiting;
        };
        let Some(release) = manifest.current(dataset) else {
            return State::Failed(Unavailable::Failed(format!(
                "the manifest has no {dataset} release"
            )));
        };
        let cell = key.tile.cell();
        if release.is_sea_cell(cell) || !release.cells.contains_key(&cell.name()) {
            return State::Empty;
        }
        let cell_path = release.cell_path(cell);
        let index = match self.cells.get(&cell_path) {
            Some(Slot::Ready(index)) => index.clone(),
            Some(Slot::Failed { why, .. }) => return State::Failed(why.clone()),
            _ => {
                super::want(&mut self.cells, cell_path, &mut self.orders, now_us);
                return State::Waiting;
            }
        };
        let TileKind::Data(entry) = index.tile(key.tile) else {
            return State::Empty;
        };
        if entry.chunks[key.index()].is_empty() {
            return State::Empty;
        }
        let path = release.chunk_path(key);
        match self.keeping.get(&path) {
            Some(Slot::Ready(size)) => State::Done(*size),
            Some(Slot::Failed { why, .. }) => State::Failed(why.clone()),
            Some(Slot::Reading | Slot::Fetching) => State::Waiting,
            None => {
                self.keeping.insert(path.clone(), Slot::Reading);
                self.orders.push(Order::Read { key: path });
                State::Waiting
            }
        }
    }

    /// Moves every job on as far as what is here allows.
    pub(super) fn advance_jobs(&mut self, now_us: u64) {
        let ids: Vec<u64> = self.jobs.keys().copied().collect();
        for id in ids {
            let Some(job) = self.jobs.get_mut(&id) else {
                continue;
            };
            let dataset = job.dataset;
            if let Some(release) = self.manifest.as_ref().and_then(|m| m.current(dataset))
                && job.release.as_deref() != Some(release.prefix().as_str())
            {
                job.done.clear();
                job.failed.clear();
                job.empty.clear();
                job.files = BTreeSet::from([manifest::MANIFEST.into(), manifest::SIGNATURE.into()]);
                for key in &job.keys {
                    job.files.insert(release.cell_path(key.tile.cell()));
                    job.files.insert(release.chunk_path(*key));
                }
                job.release = Some(release.prefix());
            }
            let open: Vec<ChunkKey> = job
                .keys
                .iter()
                .filter(|k| {
                    !job.done.contains_key(k)
                        && !job.failed.contains_key(k)
                        && !job.empty.contains(k)
                })
                .copied()
                .collect();
            for key in open {
                let state = self.state(dataset, key, now_us);
                let Some(job) = self.jobs.get_mut(&id) else {
                    break;
                };
                match state {
                    State::Empty => {
                        job.empty.insert(key);
                    }
                    State::Done(size) => {
                        job.done.insert(key, size);
                    }
                    State::Failed(why) => {
                        job.failed.insert(key, why);
                    }
                    State::Waiting => {}
                }
            }
        }
    }

    /// The SHA-256 a chunk's file must have, from its cell index.
    fn chunk_hash(&self, path: &str) -> Option<String> {
        let release = self.release_for(path)?;
        let rest = path.strip_prefix(&release.prefix())?;
        let (tile, file) = rest.split_once('/')?;
        let tile = Tile::parse(tile)?;
        let digits = file.strip_suffix(".uxdt")?.as_bytes();
        let [row @ b'0'..=b'7', col @ b'0'..=b'7'] = digits else {
            return None;
        };
        let key = ChunkKey {
            tile,
            row: row - b'0',
            col: col - b'0',
        };
        let Some(Slot::Ready(index)) = self.cells.get(&release.cell_path(tile.cell())) else {
            return None;
        };
        let TileKind::Data(entry) = index.tile(tile) else {
            return None;
        };
        Some(entry.chunks[key.index()].clone()).filter(|h| !h.is_empty())
    }

    /// A kept chunk read back: done if it still matches, else fetched.
    pub(super) fn keep_read(&mut self, path: &str, bytes: Option<&[u8]>) {
        if !matches!(self.keeping.get(path), Some(Slot::Reading)) {
            return;
        }
        let want = self.chunk_hash(path);
        match bytes {
            Some(bytes) if want.as_deref() == Some(manifest::sha256(bytes).as_str()) => {
                let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                self.keeping.insert(path.into(), Slot::Ready(size));
            }
            _ => {
                self.keeping.insert(path.into(), Slot::Fetching);
                self.orders.push(Order::Fetch { key: path.into() });
            }
        }
    }

    /// A chunk fetched for keeping: checked, then kept.
    pub(super) fn keep_fetched(
        &mut self,
        path: &str,
        result: &Result<Vec<u8>, super::FetchError>,
        now_us: u64,
    ) {
        if !matches!(self.keeping.get(path), Some(Slot::Fetching)) {
            return;
        }
        let want = self.chunk_hash(path);
        let slot = match result {
            Ok(bytes) if want.as_deref() == Some(manifest::sha256(bytes).as_str()) => {
                // Written only when no lookup is fetching it too, which
                // writes it itself.
                if !matches!(self.chunks.get(path), Some(Slot::Fetching)) {
                    self.write(path.into(), bytes.clone());
                }
                // A download is only kept once the host confirms the write.
                Slot::Fetching
            }
            Ok(_) => Slot::Failed {
                why: Unavailable::Failed(format!("{path} does not match the manifest's checksum")),
                at_us: now_us,
            },
            Err(e) if e.offline => Slot::Failed {
                why: Unavailable::Offline,
                at_us: now_us,
            },
            Err(e) => Slot::Failed {
                why: Unavailable::Failed(e.message.clone()),
                at_us: now_us,
            },
        };
        self.keeping.insert(path.into(), slot);
    }
}

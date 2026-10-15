//! Where each message is in the links' tlogs, for `mavlink/history`'s
//! received messages: per file, the offsets of its records by system id and
//! message id, so a page is a few small reads at known places rather than a
//! scan.
//!
//! The data plane indexes its current file of each link as it writes it
//! ([`open`], [`indexed`]), which costs a push per frame. Earlier files, from
//! previous runs or before the link last reopened, are indexed the first
//! time a page reaches them: one sequential read of the file's last
//! [`SCAN_BYTES`] (about a quarter of a second for 256 MB from the disk
//! cache, a few seconds from a slow disk), held in the engine's call, then
//! kept among the last [`CACHED_FILES`] read.
//!
//! Memory: at most [`MAX_OFFSETS`] offsets of 4 bytes per file (16 MB, some
//! eleven hours of a vehicle sending 100 messages a second), for each open
//! link's file and the cached ones. Past that a file's oldest offsets go,
//! about a quarter at a time, and its pages end sooner: older ones continue
//! with the file before it. Offsets past 4 GiB from where indexing started
//! are not kept.
//!
//! A cursor is `t<seconds>.<offset>`: the file's start time from its name
//! (`<link id>-<unix seconds>.tlog`) and the offset of the last record the
//! page held. It stays valid while the files exist: once its file is gone,
//! the pages continue with older files.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use uxduck_mavlink::framing::{self, Header};

/// Offsets kept per file.
const MAX_OFFSETS: usize = 4 << 20;
/// Message types (system id and message id) indexed per file; frames of
/// others past this are not.
const MAX_KEYS: usize = 4096;
/// Earlier files' indexes kept; the one read least recently goes first.
const CACHED_FILES: usize = 4;
/// How much of an earlier file's end is indexed.
const SCAN_BYTES: u64 = 256 << 20;
/// The longest a MAVLink frame can be: a v2 header, 255 bytes of payload,
/// its checksum and a signature.
const MAX_FRAME: usize = 10 + 255 + 2 + 13;

/// A message type from one system.
type Key = (u8, u32);

/// One file's records by type, each list in file order.
#[derive(Debug)]
pub struct Index {
    /// Where offsets count from: the start of the file, or for an earlier
    /// file longer than [`SCAN_BYTES`], the first record indexed.
    base: u64,
    offsets: HashMap<Key, Vec<u32>>,
    total: usize,
    /// Offsets below it were dropped.
    floor: u32,
    cap: usize,
}

impl Index {
    fn new(base: u64, cap: usize) -> Self {
        Self {
            base,
            offsets: HashMap::new(),
            total: 0,
            floor: 0,
            cap,
        }
    }

    /// The record at `offset` in the file holds a frame of `key`. Records
    /// come in file order.
    fn add(&mut self, key: Key, offset: u64) {
        let Some(at) = offset
            .checked_sub(self.base)
            .and_then(|at| u32::try_from(at).ok())
        else {
            return;
        };
        if self.offsets.len() >= MAX_KEYS && !self.offsets.contains_key(&key) {
            return;
        }
        self.offsets.entry(key).or_default().push(at);
        self.total += 1;
        if self.total > self.cap {
            self.trim(at);
        }
    }

    /// Drops the oldest offsets until a quarter of the cap is free: each
    /// round raises the floor halfway to the newest.
    fn trim(&mut self, newest: u32) {
        while self.total > self.cap / 4 * 3 && self.floor < newest {
            self.floor += (newest - self.floor).div_ceil(2);
            let floor = self.floor;
            let mut total = 0;
            self.offsets.retain(|_, list| {
                let cut = list.partition_point(|&at| at < floor);
                list.drain(..cut);
                total += list.len();
                !list.is_empty()
            });
            self.total = total;
        }
    }

    /// Up to `n` records of `key` before `before`, newest first, and
    /// whether more come before the last of them.
    fn before(&self, key: Key, before: u64, n: usize) -> (Vec<u64>, bool) {
        let Some(list) = self.offsets.get(&key) else {
            return (Vec::new(), false);
        };
        let bound = before.saturating_sub(self.base);
        let end = list.partition_point(|&at| u64::from(at) < bound);
        let start = end.saturating_sub(n);
        let found = list[start..end]
            .iter()
            .rev()
            .map(|&at| self.base + u64::from(at))
            .collect();
        (found, start > 0)
    }
}

#[derive(Debug, Default)]
struct Registry {
    /// Each open link's current file, indexed as it is written.
    current: HashMap<String, (PathBuf, Index)>,
    /// Earlier files' indexes, the one read most recently last.
    earlier: Vec<(PathBuf, Arc<Index>)>,
}

impl Registry {
    fn keep(&mut self, path: PathBuf, index: Arc<Index>) {
        self.earlier.retain(|(kept, _)| *kept != path);
        if self.earlier.len() >= CACHED_FILES {
            self.earlier.remove(0);
        }
        self.earlier.push((path, index));
    }

    fn earlier(&mut self, path: &Path) -> Option<Arc<Index>> {
        let at = self.earlier.iter().position(|(kept, _)| kept == path)?;
        let entry = self.earlier.remove(at);
        let index = entry.1.clone();
        self.earlier.push(entry);
        Some(index)
    }
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A link's new current file, empty: the one before it, if any, is kept
/// among the earlier files, whole.
pub fn open(link_id: &str, path: &Path) {
    let mut registry = registry();
    if let Some((old, index)) = registry.current.remove(link_id) {
        registry.keep(old, Arc::new(index));
    }
    registry.current.insert(
        link_id.to_owned(),
        (path.to_owned(), Index::new(0, MAX_OFFSETS)),
    );
}

/// The link closed: its file joins the earlier ones.
pub fn close(link_id: &str) {
    let mut registry = registry();
    if let Some((old, index)) = registry.current.remove(link_id) {
        registry.keep(old, Arc::new(index));
    }
}

/// Records written to the link's current file and flushed: each frame's
/// header and the offset of its record.
pub fn indexed(link_id: &str, records: &[(Header, u64)]) {
    let mut registry = registry();
    if let Some((_, index)) = registry.current.get_mut(link_id) {
        for (header, offset) in records {
            index.add((header.system_id, header.message_id), *offset);
        }
    }
}

/// A received message read back from a tlog.
#[derive(Debug, PartialEq, Eq)]
pub struct Found {
    pub at_us: u64,
    pub system_id: u8,
    pub component_id: u8,
    pub payload: Vec<u8>,
}

/// A `before` this module did not write.
#[derive(Debug, PartialEq, Eq)]
pub struct BadCursor;

fn cursor(seconds: u64, offset: u64) -> String {
    format!("t{seconds}.{offset}")
}

fn parse_cursor(text: &str) -> Result<(u64, u64), BadCursor> {
    let digits = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    let (seconds, offset) = text
        .strip_prefix('t')
        .and_then(|rest| rest.split_once('.'))
        .filter(|(s, o)| digits(s) && digits(o))
        .ok_or(BadCursor)?;
    Ok((
        seconds.parse().map_err(|_| BadCursor)?,
        offset.parse().map_err(|_| BadCursor)?,
    ))
}

/// The link's tlogs in `dir`, newest first, with the time each started.
fn files(dir: &Path, link_id: &str) -> Vec<(u64, PathBuf)> {
    let prefix = format!("{link_id}-");
    let mut files: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let seconds = name
                .to_str()?
                .strip_prefix(&prefix)?
                .strip_suffix(".tlog")?;
            if seconds.is_empty() || !seconds.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((seconds.parse().ok()?, entry.path()))
        })
        .collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    files
}

/// Up to `limit` messages of `message_id` from `system_id`, newest first,
/// from the link's tlogs in `dir`, before `before` (a previous page's
/// cursor), and the cursor for the page after them.
///
/// # Errors
/// When `before` is not a cursor this module wrote.
pub fn page(
    dir: &Path,
    link_id: &str,
    system_id: u8,
    message_id: u32,
    before: Option<&str>,
    limit: usize,
) -> Result<(Vec<Found>, Option<String>), BadCursor> {
    let before = before.map(parse_cursor).transpose()?;
    let key = (system_id, message_id);
    let files = files(dir, link_id);
    let mut found = Vec::new();
    for (n, (seconds, path)) in files.iter().enumerate() {
        let mut bound = match before {
            Some((from, _)) if *seconds > from => continue,
            Some((from, offset)) if *seconds == from => offset,
            _ => u64::MAX,
        };
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        loop {
            let (offsets, more) = offsets(path, key, bound, limit - found.len());
            for offset in offsets {
                bound = offset;
                if let Some(record) = read(&mut file, offset, key) {
                    found.push(record);
                    if found.len() == limit {
                        let older = (more || n + 1 < files.len()).then(|| cursor(*seconds, offset));
                        return Ok((found, older));
                    }
                }
            }
            if !more {
                break;
            }
        }
    }
    Ok((found, None))
}

/// Up to `n` offsets of `key` in the file at `path` before `before`, newest
/// first, from the current file's index, a cached one, or one made now.
fn offsets(path: &Path, key: Key, before: u64, n: usize) -> (Vec<u64>, bool) {
    let index = {
        let mut registry = registry();
        if let Some((_, index)) = registry.current.values().find(|(p, _)| p == path) {
            return index.before(key, before, n);
        }
        registry.earlier(path)
    };
    let index = index.unwrap_or_else(|| {
        // Read outside the lock, so the data plane keeps writing.
        let index = Arc::new(scan(path, SCAN_BYTES).unwrap_or_else(|_| Index::new(0, MAX_OFFSETS)));
        registry().keep(path.to_owned(), index.clone());
        index
    });
    index.before(key, before, n)
}

/// The record at `offset`, if it holds a frame of `key`.
fn read(file: &mut File, offset: u64, key: Key) -> Option<Found> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut record = [0u8; 8 + MAX_FRAME];
    file.read_exact(&mut record[..8 + 3]).ok()?;
    let len = framing::frame_len(&record[8..])?;
    file.read_exact(&mut record[8 + 3..8 + len]).ok()?;
    let frame = &record[8..8 + len];
    let header = framing::header(frame)?;
    if (header.system_id, header.message_id) != key {
        return None;
    }
    let mut stamp = [0u8; 8];
    stamp.copy_from_slice(&record[..8]);
    Some(Found {
        at_us: u64::from_be_bytes(stamp),
        system_id: header.system_id,
        component_id: header.component_id,
        payload: framing::payload(frame)?.to_vec(),
    })
}

/// Indexes an earlier file, or its last `tail` bytes: there the first whole
/// record is found by three in a row reading as records.
fn scan(path: &Path, tail: u64) -> std::io::Result<Index> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut start = len.saturating_sub(tail);
    if start > 0 {
        let mut window = vec![0u8; 64 * 1024];
        file.seek(SeekFrom::Start(start))?;
        let got = read_up_to(&mut file, &mut window)?;
        let Some(at) = (0..got).find(|&at| records_at(&window[..got], at, 3)) else {
            return Ok(Index::new(0, MAX_OFFSETS));
        };
        start += at as u64;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut index = Index::new(start, MAX_OFFSETS);
    let mut record = [0u8; 8 + MAX_FRAME];
    let mut offset = start;
    loop {
        if reader.read_exact(&mut record[..8 + 3]).is_err() {
            break;
        }
        let Some(len) = framing::frame_len(&record[8..]) else {
            break;
        };
        if reader.read_exact(&mut record[8 + 3..8 + len]).is_err() {
            break;
        }
        if let Some(header) = framing::header(&record[8..8 + len]) {
            index.add((header.system_id, header.message_id), offset);
        }
        offset += 8 + len as u64;
    }
    Ok(index)
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// Whether `count` whole records start at `at`, each a frame after a time
/// within an hour of the first's.
fn records_at(bytes: &[u8], mut at: usize, count: usize) -> bool {
    const HOUR_US: u64 = 3_600_000_000;
    let mut first = None;
    for _ in 0..count {
        let Some(stamp) = bytes.get(at..at + 8) else {
            return false;
        };
        let mut raw = [0u8; 8];
        raw.copy_from_slice(stamp);
        let stamp = u64::from_be_bytes(raw);
        if stamp.abs_diff(*first.get_or_insert(stamp)) > HOUR_US {
            return false;
        }
        let Some(len) = bytes.get(at + 8..).and_then(framing::frame_len) else {
            return false;
        };
        if at + 8 + len > bytes.len() {
            return false;
        }
        at += 8 + len;
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use uxduck_mavlink::sim;

    fn key_of(frame: &[u8]) -> Key {
        let header = framing::header(frame).unwrap();
        (header.system_id, header.message_id)
    }

    #[test]
    fn pages_come_newest_first_from_known_offsets() {
        let mut index = Index::new(0, MAX_OFFSETS);
        for n in 0..10u64 {
            index.add((1, 0), n * 20);
            index.add((1, 33), n * 20 + 10);
        }
        assert_eq!(
            index.before((1, 0), u64::MAX, 3),
            (vec![180, 160, 140], true)
        );
        assert_eq!(index.before((1, 0), 140, 3), (vec![120, 100, 80], true));
        assert_eq!(index.before((1, 0), 40, 3), (vec![20, 0], false));
        assert_eq!(index.before((2, 0), u64::MAX, 3), (vec![], false));
    }

    #[test]
    fn the_oldest_offsets_go_past_the_cap() {
        let mut index = Index::new(0, 100);
        for n in 0..1000u64 {
            index.add((1, u32::try_from(n % 3).unwrap()), n * 10);
            assert!(index.total <= 100);
        }
        // The newest are all kept.
        let (newest, _) = index.before((1, 0), u64::MAX, 10);
        assert_eq!(newest[0], 9990);
        assert_eq!(newest[9], 9720);
        let kept: usize = index.offsets.values().map(Vec::len).sum();
        assert_eq!(kept, index.total);
    }

    #[test]
    fn too_many_types_are_not_indexed() {
        let mut index = Index::new(0, MAX_OFFSETS);
        for id in 0..u32::try_from(MAX_KEYS).unwrap() + 10 {
            index.add((1, id), u64::from(id));
        }
        assert_eq!(index.offsets.len(), MAX_KEYS);
    }

    #[test]
    fn cursors_round_trip_and_others_are_refused() {
        assert_eq!(
            parse_cursor(&cursor(1_790_000_000, 81_920)),
            Ok((1_790_000_000, 81_920))
        );
        for bad in ["", "t", "t1", "t.1", "t1.", "m12", "t1.2.3", "t-1.2"] {
            assert_eq!(parse_cursor(bad), Err(BadCursor), "{bad}");
        }
    }

    #[test]
    fn an_earlier_file_is_scanned_whole_or_from_a_record_found_in_its_tail() {
        let a = sim::heartbeat_frame(1, 1, 0);
        let b = sim::autopilot_version_frame(1, 1, 1, 42, 0);
        let mut log = Vec::new();
        let mut offsets = Vec::new();
        for n in 0..40u64 {
            let frame = if n % 2 == 0 { &a } else { &b };
            offsets.push(log.len() as u64);
            uxduck_mavlink::tlog::record(&mut log, 1_000_000 + n, frame).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lk-1.tlog");
        std::fs::write(&path, &log).unwrap();

        let whole = scan(&path, SCAN_BYTES).unwrap();
        assert_eq!(whole.total, 40);
        let (newest, more) = whole.before(key_of(&a), u64::MAX, 2);
        assert_eq!((newest, more), (vec![offsets[38], offsets[36]], true));

        // Its last part only, starting mid-record: the first whole record
        // after that is found.
        let tail = log.len() as u64 - offsets[30] + 5;
        let part = scan(&path, tail).unwrap();
        assert_eq!(part.base, offsets[30]);
        assert_eq!(part.total, 10);
        let (oldest, more) = part.before(key_of(&b), offsets[32], 5);
        assert_eq!((oldest, more), (vec![offsets[31]], false));

        let mut file = File::open(&path).unwrap();
        let found = read(&mut file, offsets[2], key_of(&a)).unwrap();
        assert_eq!(found.at_us, 1_000_002);
        assert_eq!(found.payload, framing::payload(&a).unwrap());
        assert_eq!(
            read(&mut file, offsets[2], key_of(&b)),
            None,
            "not its type"
        );
    }

    #[test]
    fn detailed_pages_keep_wire_bytes_filter_ranges_and_report_partial_tails() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        let mut expected = Vec::new();
        for n in 0..6u8 {
            let ours = sim::heartbeat_frame(1, 1, n);
            let others = sim::heartbeat_frame(2, 1, n);
            let version = sim::autopilot_version_frame(1, 1, n, 42, 0);
            for frame in [&ours, &others, &version] {
                uxduck_mavlink::tlog::record(&mut log, 100_000_000 + u64::from(n), frame).unwrap();
            }
            expected.push(ours);
        }
        log.extend_from_slice(&[0, 1, 2]);
        std::fs::write(dir.path().join("detail-100.tlog"), log).unwrap();
        let range = (100_000_001, 100_000_004);
        let first = packet_page(dir.path(), "detail", Some(1), Some(0), range, None, 2).unwrap();
        assert_eq!(
            first.records.iter().map(|r| r.at_us).collect::<Vec<_>>(),
            [100_000_004, 100_000_003]
        );
        assert_eq!(first.records[0].frame, expected[4]);
        assert!(first.truncated);
        assert!(
            first
                .notes
                .iter()
                .any(|n| n.contains("partial or malformed"))
        );
        let second = packet_page(
            dir.path(),
            "detail",
            Some(1),
            Some(0),
            range,
            first.older.as_deref(),
            2,
        )
        .unwrap();
        assert_eq!(
            second.records.iter().map(|r| r.at_us).collect::<Vec<_>>(),
            [100_000_002, 100_000_001]
        );
        assert!(second.older.is_none());
        let all = packet_page(dir.path(), "detail", None, None, range, None, 20).unwrap();
        assert_eq!(all.records.len(), 12);
    }

    #[test]
    fn pages_continue_into_earlier_files_and_leave_other_systems_out() {
        let dir = tempfile::tempdir().unwrap();
        let ours = |n: u8| sim::heartbeat_frame(1, 1, n);
        let theirs = sim::heartbeat_frame(2, 1, 0);
        // An earlier run: 3 of ours. Not current, so read the first time a
        // page reaches it.
        let mut earlier = Vec::new();
        for n in 0..3u8 {
            uxduck_mavlink::tlog::record(&mut earlier, 100 + u64::from(n), &ours(n)).unwrap();
        }
        std::fs::write(dir.path().join("lk-page-100.tlog"), &earlier).unwrap();
        // Another link's file, and something else, are not read.
        std::fs::write(dir.path().join("lk-page-x-200.tlog"), &earlier).unwrap();
        std::fs::write(dir.path().join("lk-page-200.txt"), b"").unwrap();
        // This run's file: indexed as written.
        let path = dir.path().join("lk-page-200.tlog");
        open("lk-page", &path);
        let mut current = Vec::new();
        let mut records = Vec::new();
        for n in 0..4u8 {
            for frame in [ours(n), theirs.clone()] {
                records.push((framing::header(&frame).unwrap(), current.len() as u64));
                uxduck_mavlink::tlog::record(&mut current, 200 + u64::from(n), &frame).unwrap();
            }
        }
        std::fs::write(&path, &current).unwrap();
        indexed("lk-page", &records);

        let at = |found: &[Found]| found.iter().map(|f| f.at_us).collect::<Vec<_>>();
        let (first, older) = page(dir.path(), "lk-page", 1, 0, None, 3).unwrap();
        assert_eq!(at(&first), [203, 202, 201]);
        let (second, older) = page(dir.path(), "lk-page", 1, 0, older.as_deref(), 3).unwrap();
        assert_eq!(at(&second), [200, 102, 101]);
        let (third, older) = page(dir.path(), "lk-page", 1, 0, older.as_deref(), 3).unwrap();
        assert_eq!(at(&third), [100]);
        assert_eq!(older, None);
        // The other system's, apart.
        let (theirs, _) = page(dir.path(), "lk-page", 2, 0, None, 10).unwrap();
        assert_eq!(at(&theirs), [203, 202, 201, 200]);
        assert!(theirs.iter().all(|f| f.system_id == 2));
        assert_eq!(
            page(dir.path(), "lk-page", 1, 0, Some("m3"), 3).unwrap_err(),
            BadCursor
        );
        // Closed, the file's index is kept with the earlier ones.
        close("lk-page");
        let (all, older) = page(dir.path(), "lk-page", 1, 0, None, 10).unwrap();
        assert_eq!(at(&all), [203, 202, 201, 200, 102, 101, 100]);
        assert_eq!(older, None);
    }
}

/// A bounded detailed query, read with the same MAVLink/tlog decoder as replay.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PacketRecord {
    pub seconds: u64,
    pub offset: u64,
    pub at_us: u64,
    pub frame: Vec<u8>,
}

pub struct PacketPage {
    pub records: Vec<PacketRecord>,
    pub older: Option<String>,
    pub available_from_us: Option<u64>,
    pub available_to_us: Option<u64>,
    pub truncated: bool,
    pub notes: Vec<String>,
}

/// Bounded to 32 MiB across at most 64 files per call. Peers are not present
/// in legacy tlogs and must not be reconstructed from current configuration.
pub fn packet_page(
    dir: &Path,
    link: &str,
    system: Option<u8>,
    message: Option<u32>,
    range: (u64, u64),
    before: Option<&str>,
    limit: usize,
) -> std::io::Result<PacketPage> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    const BYTES: u64 = 32 << 20;
    let before = before
        .map(parse_cursor)
        .transpose()
        .map_err(|_| std::io::Error::other("Invalid tlog packet cursor"))?;
    let mut left = BYTES;
    let mut heap = BinaryHeap::<Reverse<PacketRecord>>::new();
    let mut result = PacketPage {
        records: Vec::new(),
        older: None,
        available_from_us: None,
        available_to_us: None,
        truncated: false,
        notes: Vec::new(),
    };
    let list = files(dir, link);
    let mut scanned = 0;
    for (seconds, path) in &list {
        if *seconds > range.1 / 1_000_000 || before.is_some_and(|(s, _)| *seconds > s) {
            continue;
        }
        if scanned >= 64 || left == 0 {
            result.truncated = true;
            break;
        }
        scanned += 1;
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(left);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(len - start).read_to_end(&mut bytes)?;
        left = left.saturating_sub(bytes.len() as u64);
        let aligned = if start == 0 {
            0
        } else {
            result.truncated = true;
            (0..bytes.len())
                .find(|&at| records_at(&bytes, at, 3))
                .unwrap_or(bytes.len())
        };
        let mut offset = start + aligned as u64;
        let records = uxduck_mavlink::tlog::read(&bytes[aligned..]);
        for (at_us, frame) in records {
            result.available_from_us =
                Some(result.available_from_us.map_or(at_us, |old| old.min(at_us)));
            result.available_to_us =
                Some(result.available_to_us.map_or(at_us, |old| old.max(at_us)));
            let position = offset;
            offset += 8 + frame.len() as u64;
            let Some(header) = framing::header(frame) else {
                continue;
            };
            if at_us < range.0
                || at_us > range.1
                || before.is_some_and(|b| (*seconds, position) >= b)
                || system.is_some_and(|id| id != header.system_id)
                || message.is_some_and(|id| id != header.message_id)
            {
                continue;
            }
            heap.push(Reverse(PacketRecord {
                seconds: *seconds,
                offset: position,
                at_us,
                frame: frame.to_vec(),
            }));
            if heap.len() > limit + 1 {
                heap.pop();
            }
        }
        if offset < len {
            result.truncated = true;
            result
                .notes
                .push("A partial or malformed tlog tail was not read.".into());
        }
        // Older files cannot normally contain a requested time after a newer
        // file's creation. Continue scanning within the explicit byte bound.
    }
    let mut packets = heap.into_iter().map(|v| v.0).collect::<Vec<_>>();
    packets.sort_by(|a, b| b.cmp(a));
    if packets.len() > limit {
        packets.truncate(limit);
        result.older = packets.last().map(|p| cursor(p.seconds, p.offset));
    }
    result.records = packets;
    if result.truncated {
        result
            .notes
            .push("The bounded tlog scan may not cover the whole requested interval.".into());
    }
    Ok(result)
}

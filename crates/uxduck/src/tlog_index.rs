//! What the links' tlogs hold, read back: for `mavlink/history`'s received
//! messages ([`page`]), for a report's flight data and for a detailed
//! diagnostic query ([`packet_page`]).
//!
//! A log is read backwards from its end, a part at a time ([`Backward`]),
//! so the newest comes first and none of a log is held between calls. A page
//! of one message type reads back until it has what was asked for, through
//! this run's file and every earlier one of the link, however long they are.
//! One call reads [`LOOK_BYTES`] at most, because it runs in the engine's
//! call, and answers with what it has and a cursor that goes on from there.
//!
//! Until 2026-11-15 this module kept every record's offset by system and
//! message id, and cut in four places without a word to anyone: a file's
//! oldest offsets went past 4,194,304 of them, a message type past the
//! 4,096th of a file was not indexed, only the last 256 MB of an earlier
//! file was, and nothing past 4 GiB of a file. Stepping back in the
//! Inspector then jumped over part of a flight or stopped short. It keeps
//! no index now, so there is nothing to cap.
//!
//! A cursor is `t<seconds>.<offset>`: the file's start time from its name
//! (`<link id>-<unix seconds>.tlog`) and the offset of the record the
//! next page reads back from. It stays valid while the files exist: once its
//! file is gone, the pages continue with older files.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use uxduck_mavlink::framing;

/// How much of a log is read at a time, going back.
const PART: u64 = 1 << 20;
/// How much of the logs one page reads before it answers. Not a limit on
/// what is read back: the answer's cursor goes on from where it stopped. A
/// page is read in the engine's call, which this keeps short: a page that
/// read all of this and found nothing took 150 to 170 ms in a debug build,
/// from the disk cache (measured 2026-11-15 on a 1.2 GiB log, 4 cores).
const LOOK_BYTES: u64 = 32 << 20;
/// The longest a MAVLink frame can be: a v2 header, 255 bytes of payload,
/// its checksum and a signature. The protocol fixes it.
const MAX_FRAME: usize = 10 + 255 + 2 + 13;
/// The longest record: a stamp and the longest frame.
const MAX_RECORD: usize = 8 + MAX_FRAME;

/// Each open link's current file, and where its latest record begins.
static CURRENT: LazyLock<Mutex<HashMap<String, (PathBuf, u64)>>> = LazyLock::new(Mutex::default);

fn current() -> std::sync::MutexGuard<'static, HashMap<String, (PathBuf, u64)>> {
    CURRENT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A link's new current file, empty.
pub fn open(link_id: &str, path: &Path) {
    current().insert(link_id.to_owned(), (path.to_owned(), 0));
}

/// The file a link writes now, by name, and where its latest record
/// begins: where a flight beginning now starts in it (ADR-0039 R10).
pub fn position(link_id: &str) -> Option<(String, u64)> {
    let current = current();
    let (path, latest) = current.get(link_id)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some((name, *latest))
}

/// The link closed.
pub fn close(link_id: &str) {
    current().remove(link_id);
}

/// Records were written to the link's current file and flushed: where the
/// latest of them begins.
pub fn written(link_id: &str, latest: u64) {
    if let Some((_, at)) = current().get_mut(link_id) {
        *at = latest;
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

/// Why a page could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    /// A `before` this module did not write.
    BadCursor,
    /// A log that could not be read, in words for the person stepping back.
    Unreadable(String),
}

fn cursor(seconds: u64, offset: u64) -> String {
    format!("t{seconds}.{offset}")
}

fn parse_cursor(text: &str) -> Result<(u64, u64), PageError> {
    let digits = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    let (seconds, offset) = text
        .strip_prefix('t')
        .and_then(|rest| rest.split_once('.'))
        .filter(|(s, o)| digits(s) && digits(o))
        .ok_or(PageError::BadCursor)?;
    Ok((
        seconds.parse().map_err(|_| PageError::BadCursor)?,
        offset.parse().map_err(|_| PageError::BadCursor)?,
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

/// The link's tlogs in `dir`, newest first.
pub(crate) fn logs(dir: &Path, link_id: &str) -> Vec<PathBuf> {
    files(dir, link_id)
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

/// A tlog read backwards, a part at a time, so a reader that wants the end
/// of a log holds [`PART`] of it however long it is.
pub(crate) struct Backward {
    file: File,
    /// What is before this offset is still to read. Always where a record
    /// begins, or the file's end.
    end: u64,
    /// `end` is the file's end, where the last record may not be whole:
    /// a file still being written, or one a crash cut short.
    tail: bool,
    buf: Vec<u8>,
}

impl Backward {
    /// The log at `path`, from its end, or from the record at `before`.
    ///
    /// # Errors
    /// When the file cannot be opened.
    pub(crate) fn open(path: &Path, before: Option<u64>) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let (end, tail) = match before {
            Some(at) if at < len => (at, false),
            _ => (len, true),
        };
        Ok(Self {
            file,
            end,
            tail,
            buf: Vec::new(),
        })
    }

    /// The part before the last one given: where it begins in the file,
    /// and its whole records in file order. None at the file's start.
    ///
    /// # Errors
    /// When the file cannot be read, or what is there is not records that
    /// end where the part does: a damaged log, or a cursor that points
    /// into the middle of a record.
    pub(crate) fn part(&mut self) -> std::io::Result<Option<(u64, &[u8])>> {
        if self.end == 0 {
            return Ok(None);
        }
        let from = self.end.saturating_sub(PART);
        let len = usize::try_from(self.end - from).unwrap_or(usize::MAX);
        self.buf.resize(len, 0);
        self.file.seek(SeekFrom::Start(from))?;
        self.file.read_exact(&mut self.buf)?;
        let Some((skip, stop)) = whole(&self.buf, from == 0, self.tail) else {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("no MAVLink record ends at byte {}", self.end),
            ));
        };
        self.end = from + skip as u64;
        self.tail = false;
        Ok(Some((self.end, &self.buf[skip..stop])))
    }
}

/// The record at `at`, if a whole one starts there: its stamp and its
/// length with the stamp.
fn record_at(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let start = at.checked_add(8)?;
    let stamp = u64::from_be_bytes(bytes.get(at..start)?.try_into().ok()?);
    let len = framing::frame_len(bytes.get(start..)?)?;
    (start + len <= bytes.len()).then_some((stamp, 8 + len))
}

/// Where whole records run from and to in bytes read from the middle of a
/// tlog that end where a record ends: the first offset from which records
/// follow each other exactly to the end. At the file's start that is 0. At
/// its `tail` the last record may be cut short, and the records stop
/// before it. None when the bytes are not that.
///
/// A record's stamp is eight bytes of anything, so bytes inside the record
/// that the part begins in can read as a record that leads on to the true
/// ones. One is taken only when its stamp is within an hour of the next
/// record's, which four bytes of a payload match by chance once in some
/// thousand million.
fn whole(bytes: &[u8], file_start: bool, tail: bool) -> Option<(usize, usize)> {
    const HOUR_US: u64 = 3_600_000_000;
    let lands = |mut at: usize| loop {
        match record_at(bytes, at) {
            Some((_, len)) => at += len,
            None if at == bytes.len() => return Some(at),
            // The rest is the start of one record: its stamp and frame
            // header, or all of it but its end.
            None if tail
                && bytes
                    .get(at + 8..)
                    .is_none_or(|rest| framing::frame_len(rest).is_some() || rest.len() < 3) =>
            {
                return Some(at);
            }
            None => return None,
        }
    };
    if file_start {
        return lands(0).map(|stop| (0, stop));
    }
    (0..=bytes.len().min(MAX_RECORD)).find_map(|at| {
        let (stamp, len) = record_at(bytes, at)?;
        if let Some((next, _)) = record_at(bytes, at + len)
            && stamp.abs_diff(next) > HOUR_US
        {
            return None;
        }
        lands(at).map(|stop| (at, stop))
    })
}

/// Up to `limit` messages of `message_id` from `system_id`, newest first,
/// from the link's tlogs in `dir`, before `before` (a previous page's
/// cursor), and the cursor for the page after them.
///
/// A page that has read [`LOOK_BYTES`] answers with what it has, which may
/// be nothing, and a cursor: the next page goes on from there. No cursor
/// means the link's logs hold nothing earlier.
///
/// # Errors
/// When `before` is not a cursor this module wrote, or a log cannot be
/// read.
pub fn page(
    dir: &Path,
    link_id: &str,
    system_id: u8,
    message_id: u32,
    before: Option<&str>,
    limit: usize,
) -> Result<(Vec<Found>, Option<String>), PageError> {
    let before = before.map(parse_cursor).transpose()?;
    let files = files(dir, link_id);
    let mut found = Vec::new();
    let mut left = LOOK_BYTES;
    for (n, (seconds, path)) in files.iter().enumerate() {
        let bound = match before {
            Some((from, _)) if *seconds > from => continue,
            Some((from, offset)) if *seconds == from => Some(offset),
            _ => None,
        };
        let unreadable = |error: std::io::Error| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            PageError::Unreadable(format!("UXDuck could not read its log {name}: {error}"))
        };
        let mut log = match Backward::open(path, bound) {
            Ok(log) => log,
            // Removed since the folder was listed: the pages go on.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(unreadable(error)),
        };
        let older_files = n + 1 < files.len();
        while let Some((start, bytes)) = log.part().map_err(unreadable)? {
            left = left.saturating_sub(bytes.len() as u64);
            let mut offset = start;
            let mut part = Vec::new();
            for (at_us, frame) in uxduck_mavlink::tlog::records(bytes) {
                let at = offset;
                offset += 8 + frame.len() as u64;
                let Some(header) = framing::header(frame) else {
                    continue;
                };
                if (header.system_id, header.message_id) == (system_id, message_id)
                    && let Some(payload) = framing::payload(frame)
                {
                    part.push((at, at_us, header.component_id, payload));
                }
            }
            for (at, at_us, component_id, payload) in part.into_iter().rev() {
                found.push(Found {
                    at_us,
                    system_id,
                    component_id,
                    payload: payload.to_vec(),
                });
                if found.len() == limit {
                    let older = (at > 0 || older_files).then(|| cursor(*seconds, at));
                    return Ok((found, older));
                }
            }
            if left == 0 && (start > 0 || older_files) {
                return Ok((found, Some(cursor(*seconds, start))));
            }
        }
    }
    Ok((found, None))
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
/// `(system, sender)`: only frames from `system`, if given, and none from
/// `sender`, a system and component: UXDuck's own, which its tlogs hold
/// beside what it received (ADR-0050 L1).
pub fn packet_page(
    dir: &Path,
    link: &str,
    (system, sender): (Option<u8>, Option<(u8, u8)>),
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
                || sender == Some((header.system_id, header.component_id))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use std::io::{BufWriter, Write};

    use super::*;
    use uxduck_mavlink::sim;

    /// A MAVLink 2 frame of `message` from `system`, its payload `fill` over
    /// and over. Its checksum is not one: nothing here checks it.
    fn frame(system: u8, message: u32, payload: usize, fill: u8) -> Vec<u8> {
        let id = message.to_le_bytes();
        let mut frame = vec![
            0xFD,
            u8::try_from(payload).unwrap(),
            0,
            0,
            0,
            system,
            1,
            id[0],
            id[1],
            id[2],
        ];
        frame.resize(10 + payload + 2, fill);
        frame
    }

    /// Every page of one message type, newest first, until the logs hold
    /// nothing earlier, and how many pages it took.
    fn all_pages(dir: &Path, link: &str, system: u8, message: u32) -> (Vec<Found>, usize) {
        let (mut all, mut pages, mut before) = (Vec::new(), 0, None::<String>);
        loop {
            let (found, older) = page(dir, link, system, message, before.as_deref(), 50).unwrap();
            pages += 1;
            all.extend(found);
            match older {
                Some(older) => before = Some(older),
                None => return (all, pages),
            }
        }
    }

    #[test]
    fn cursors_round_trip_and_others_are_refused() {
        assert_eq!(
            parse_cursor(&cursor(1_790_000_000, 81_920)),
            Ok((1_790_000_000, 81_920))
        );
        for bad in ["", "t", "t1", "t.1", "t1.", "m12", "t1.2.3", "t-1.2"] {
            assert_eq!(parse_cursor(bad), Err(PageError::BadCursor), "{bad}");
        }
    }

    /// Read backwards a part at a time, a log gives the records it gives
    /// read from its start, each at its own offset: also with a last
    /// record that is not whole, and with payloads that hold records of
    /// their own, as a log sent over MAVLink FTP does.
    #[test]
    fn a_log_read_backwards_gives_every_record_at_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lk-1.tlog");
        let mut log = Vec::new();
        let mut inner = Vec::new();
        for n in 0..6u64 {
            uxduck_mavlink::tlog::record(&mut inner, 1_000 + n, &sim::heartbeat_frame(9, 1, 0))
                .unwrap();
        }
        let mut n = 0u64;
        while log.len() < usize::try_from(3 * PART).unwrap() + 12_345 {
            let frame = match n % 4 {
                0 => sim::heartbeat_frame(1, 1, 0),
                1 => frame(1, 33, 28, 0xFD),
                2 => {
                    let mut carrier = frame(1, 110, 0, 0);
                    carrier[1] = u8::try_from(inner.len()).unwrap();
                    carrier.truncate(10);
                    carrier.extend_from_slice(&inner);
                    carrier.extend_from_slice(&[0, 0]);
                    carrier
                }
                _ => frame(2, 30, 251, 0xFE),
            };
            uxduck_mavlink::tlog::record(&mut log, 1_790_000_000_000_000 + n, &frame).unwrap();
            n += 1;
        }
        let whole_records = log.len();
        // A crash, or a write under way: the last record is cut short.
        log.extend_from_slice(&1_790_000_000_999_999u64.to_be_bytes());
        log.extend_from_slice(&[0xFD, 200, 0, 0, 0]);
        std::fs::write(&path, &log).unwrap();

        let mut expected = Vec::new();
        let mut at = 0u64;
        for (stamp, frame) in uxduck_mavlink::tlog::records(&log) {
            expected.push((at, stamp, frame.to_vec()));
            at += 8 + frame.len() as u64;
        }
        assert_eq!(usize::try_from(at).unwrap(), whole_records);

        let mut back = Backward::open(&path, None).unwrap();
        let mut read = Vec::new();
        let mut parts = 0;
        while let Some((start, bytes)) = back.part().unwrap() {
            parts += 1;
            let mut at = start;
            let mut part = Vec::new();
            for (stamp, frame) in uxduck_mavlink::tlog::records(bytes) {
                part.push((at, stamp, frame.to_vec()));
                at += 8 + frame.len() as u64;
            }
            part.extend(read);
            read = part;
        }
        assert!(parts >= 4, "{parts} parts");
        assert_eq!(read.len(), expected.len());
        assert!(read == expected, "the same records at the same offsets");

        // From a record on: only what is before it.
        let (offset, _, _) = expected[expected.len() / 2];
        let mut back = Backward::open(&path, Some(offset)).unwrap();
        let (start, bytes) = back.part().unwrap().unwrap();
        let last = uxduck_mavlink::tlog::records(bytes).last().unwrap();
        assert_eq!(last.0, expected[expected.len() / 2 - 1].1);
        assert!(start < offset);
    }

    /// Until 2026-11-15 a message type past the 4,096th of a log was not
    /// indexed, and stepping back through it said nothing earlier was kept.
    #[test]
    fn every_message_type_of_a_log_is_read_back_however_many_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        let mut at_us = 1_790_000_000_000_000u64;
        // 6,000 types: 20 systems, 300 messages each, three times over.
        for round in 0..3u8 {
            for system in 1..=20u8 {
                for message in 0..300u32 {
                    at_us += 1;
                    uxduck_mavlink::tlog::record(
                        &mut log,
                        at_us,
                        &frame(system, 1000 + message, 4, round),
                    )
                    .unwrap();
                }
            }
        }
        std::fs::write(dir.path().join("lk-100.tlog"), &log).unwrap();
        let (last, pages) = all_pages(dir.path(), "lk", 20, 1299);
        assert_eq!(pages, 1);
        assert_eq!(
            last.iter().map(|f| f.payload[0]).collect::<Vec<_>>(),
            [2, 1, 0],
            "the 6,000th type's three messages, newest first"
        );
        let (first, _) = all_pages(dir.path(), "lk", 1, 1000);
        assert_eq!(first.len(), 3);
    }

    /// Until 2026-11-15 a log's oldest offsets went once it held more than
    /// 4,194,304 records, and stepping back jumped over them to the log
    /// before, or stopped.
    #[test]
    fn a_log_of_more_records_than_were_once_indexed_is_read_back_to_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = BufWriter::new(File::create(dir.path().join("lk-200.tlog")).unwrap());
        let filler = frame(1, 30, 0, 0);
        let rare = |n: u8| frame(1, 253, 1, n);
        let t0 = 1_790_000_000_000_000u64;
        let records = 4_400_000u64;
        // Two of the rare message where the log starts, one at its end.
        for n in 0..records {
            let frame = match n {
                0 => rare(1),
                1 => rare(2),
                n if n == records - 1 => rare(3),
                _ => filler.clone(),
            };
            uxduck_mavlink::tlog::record(&mut log, t0 + n, &frame).unwrap();
        }
        log.flush().unwrap();
        // The log before it: the pages go on into it.
        let mut earlier = Vec::new();
        uxduck_mavlink::tlog::record(&mut earlier, t0 - 10, &rare(0)).unwrap();
        std::fs::write(dir.path().join("lk-100.tlog"), &earlier).unwrap();

        let (found, pages) = all_pages(dir.path(), "lk", 1, 253);
        assert_eq!(
            found
                .iter()
                .map(|f| (f.at_us - (t0 - 10), f.payload[0]))
                .collect::<Vec<_>>(),
            [(records + 9, 3), (11, 2), (10, 1), (0, 0)],
            "every one of them, newest first, none jumped over"
        );
        // 88 MB, 32 MB a page: a page that found nothing new still said
        // where the next goes on from.
        assert!(pages >= 3, "{pages} pages");
    }

    /// Until 2026-11-15 only the last 256 MB of an earlier log was indexed:
    /// what a flight held before that was not there to step back to.
    #[test]
    fn an_earlier_log_longer_than_256_mb_is_read_back_to_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = BufWriter::new(File::create(dir.path().join("lk-100.tlog")).unwrap());
        let filler = frame(1, 30, 255, 7);
        let t0 = 1_790_000_000_000_000u64;
        uxduck_mavlink::tlog::record(&mut log, t0, &frame(1, 253, 1, 42)).unwrap();
        let mut written = 0usize;
        let mut n = 0u64;
        while written < (256 << 20) + (4 << 20) {
            n += 1;
            uxduck_mavlink::tlog::record(&mut log, t0 + n, &filler).unwrap();
            written += 8 + filler.len();
        }
        log.flush().unwrap();
        // A later run's log, so the long one is an earlier file.
        let mut later = Vec::new();
        uxduck_mavlink::tlog::record(&mut later, t0 + n + 1, &filler).unwrap();
        std::fs::write(dir.path().join("lk-200.tlog"), &later).unwrap();

        let (found, pages) = all_pages(dir.path(), "lk", 1, 253);
        assert_eq!(found.len(), 1, "the message 260 MB before the log's end");
        assert_eq!((found[0].at_us, found[0].payload[0]), (t0, 42));
        assert!(pages >= 8, "{pages} pages of 32 MB");
    }

    /// A log that is not records all the way is said, by name, where a
    /// page once went on to the log before it without a word.
    #[test]
    fn a_damaged_log_and_a_cursor_into_a_record_are_said() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        for n in 0..10u64 {
            uxduck_mavlink::tlog::record(&mut log, 100 + n, &sim::heartbeat_frame(1, 1, 0))
                .unwrap();
        }
        let good = log.len();
        log.extend_from_slice(&[0x55; 64]);
        uxduck_mavlink::tlog::record(&mut log, 200, &sim::heartbeat_frame(1, 1, 0)).unwrap();
        std::fs::write(dir.path().join("lk-100.tlog"), &log).unwrap();
        let damaged = page(dir.path(), "lk", 1, 0, None, 50).unwrap_err();
        let PageError::Unreadable(why) = damaged else {
            panic!("{damaged:?}");
        };
        assert!(
            why.starts_with("UXDuck could not read its log lk-100.tlog: "),
            "{why}"
        );
        // Not where a record begins.
        let inside = cursor(100, good as u64 - 5);
        assert!(matches!(
            page(dir.path(), "lk", 1, 0, Some(&inside), 50),
            Err(PageError::Unreadable(_))
        ));
        // Where one does: the ten before the damage.
        let (found, older) =
            page(dir.path(), "lk", 1, 0, Some(&cursor(100, good as u64)), 50).unwrap();
        assert_eq!((found.len(), older), (10, None));
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
        let first = packet_page(
            dir.path(),
            "detail",
            (Some(1), None),
            Some(0),
            range,
            None,
            2,
        )
        .unwrap();
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
            (Some(1), None),
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
        let all = packet_page(dir.path(), "detail", (None, None), None, range, None, 20).unwrap();
        assert_eq!(all.records.len(), 12);
    }

    /// ADR-0050 L1: a link's tlog holds what UXDuck sent beside what it
    /// heard, and a page of received packets leaves UXDuck's own out.
    #[test]
    fn a_page_of_received_packets_leaves_out_what_uxduck_sent() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Vec::new();
        for n in 0..4u8 {
            let heard = sim::heartbeat_frame(1, 1, n);
            let sent = sim::gcs_heartbeat_frame(255, 190, n);
            let other = sim::gcs_heartbeat_frame(254, 190, n);
            for frame in [&heard, &sent, &other] {
                uxduck_mavlink::tlog::record(&mut log, 100_000_000 + u64::from(n), frame).unwrap();
            }
        }
        std::fs::write(dir.path().join("both-100.tlog"), log).unwrap();
        let range = (0, u64::MAX);
        let everything = packet_page(dir.path(), "both", (None, None), None, range, None, 20);
        assert_eq!(everything.unwrap().records.len(), 12);
        let received = packet_page(
            dir.path(),
            "both",
            (None, Some((255, 190))),
            None,
            range,
            None,
            20,
        )
        .unwrap();
        let from: Vec<u8> = received
            .records
            .iter()
            .map(|r| framing::header(&r.frame).unwrap().system_id)
            .collect();
        assert_eq!(from.len(), 8, "{from:?}");
        assert!(!from.contains(&255), "{from:?}");
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
        // This run's file, still open.
        let path = dir.path().join("lk-page-200.tlog");
        open("lk-page", &path);
        let mut current = Vec::new();
        let mut latest = 0;
        for n in 0..4u8 {
            for frame in [ours(n), theirs.clone()] {
                latest = current.len() as u64;
                uxduck_mavlink::tlog::record(&mut current, 200 + u64::from(n), &frame).unwrap();
            }
        }
        std::fs::write(&path, &current).unwrap();
        written("lk-page", latest);
        assert_eq!(
            position("lk-page"),
            Some(("lk-page-200.tlog".to_owned(), latest))
        );

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
            PageError::BadCursor
        );
        // Closed, the file is read back as before.
        close("lk-page");
        assert_eq!(position("lk-page"), None);
        let (all, older) = page(dir.path(), "lk-page", 1, 0, None, 10).unwrap();
        assert_eq!(at(&all), [203, 202, 201, 200, 102, 101, 100]);
        assert_eq!(older, None);
    }
}

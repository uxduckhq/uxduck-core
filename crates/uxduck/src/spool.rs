//! The spool (S82, ADR-0003 C2, "Durable telemetry"): what is waiting to be
//! stored in the account. The data plane writes one tlog per link per run;
//! the spool cuts what was written since the last cut into chunks, every
//! 30 s or 1 MB, on record boundaries, compressed with zstd. A chunk's file
//! is written and fsynced before its row, and its row and the tlog's cursor
//! commit together, so a crash at any point leaves either a file with no row
//! (deleted at start, and cut again) or a row with its file. A row whose
//! file is missing sends its tlog's cursor back to where it began.
//!
//! The tlogs themselves are the computer's record and are never touched
//! here; a chunk's file is deleted once the account acknowledged it.
//!
//! Recordings (S83, ADR-0003 C14, "Durable path: segmented originals") are
//! cut along their fragments into segments of about 10 s, the init segment
//! first. A segment is a byte range of its recording, not a copy: its row
//! names the file and the range, and its bytes are read from there when it
//! goes. A recording deleted before its segments went loses them; that
//! range is `lost`, and the caller journals it.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Cut at least this often while a tlog grows.
pub const CUT_EVERY_US: u64 = 30_000_000;
/// Or once this much is waiting.
pub const CUT_BYTES: u64 = 1 << 20;
/// A recording's segments last about this long (ADR-0003 C14).
pub const SEGMENT_US: u64 = 10_000_000;
/// Segments cut per recording per round, so one recording of hours does
/// not hold the others up.
const SEGMENTS_PER_ROUND: usize = 6;

/// A chunk's state on its way to the account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// On disk here, not sent.
    Written,
    /// Sent; the account has not said it keeps it yet.
    Uploaded,
    /// Kept by the account; its file here is gone.
    Acknowledged,
    /// Its recording was deleted before it went (S83).
    Lost,
}

impl State {
    const fn word(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Uploaded => "uploaded",
            Self::Acknowledged => "acknowledged",
            Self::Lost => "lost",
        }
    }
}

/// One chunk, as the account names and checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// `<recording>/<seq>-<sha256>.tlog.zst`
    pub name: String,
    /// The tlog it came from, by its file stem.
    pub recording: String,
    pub seq: u32,
    pub size: u64,
    pub sha256: String,
    pub first_us: u64,
    pub last_us: u64,
}

/// A recording the spool may cut (S83): its file, when it began, and
/// whether it ended (only then is its last part cut).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    pub id: String,
    pub path: PathBuf,
    pub started_at_us: u64,
    pub ended: bool,
}

/// What a deleted recording took with it before it reached the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lost {
    pub recording: String,
    pub from_us: u64,
    pub to_us: u64,
}

pub struct Spool {
    db: Connection,
    tlogs: PathBuf,
    dir: PathBuf,
}

/// SQLite keeps integers as `i64`; times and sizes here are never that big.
fn i(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Spool {
    /// Opens the spool in `data_dir`, reading tlogs from `tlogs`, and
    /// reconciles it with what is on disk.
    ///
    /// # Errors
    /// The database cannot be opened.
    pub fn open(data_dir: &Path, tlogs: PathBuf) -> rusqlite::Result<Self> {
        let dir = data_dir.join("spool");
        let _ = std::fs::create_dir_all(&dir);
        let db = Connection::open(data_dir.join("spool.sqlite"))?;
        db.execute_batch(
            "create table if not exists cursor (
                recording text primary key,
                offset integer not null,
                cut_at_us integer not null,
                seq integer not null
            );
            create table if not exists chunk (
                name text primary key,
                recording text not null,
                seq integer not null,
                size integer not null,
                sha256 text not null,
                first_us integer not null,
                last_us integer not null,
                start_offset integer not null,
                end_offset integer not null,
                state text not null
            );",
        )?;
        // S83: a segment of a recording is a range of `source`, not a file here.
        let has_source = db
            .prepare("select 1 from pragma_table_info('chunk') where name = 'source'")?
            .exists([])?;
        if !has_source {
            db.execute_batch("alter table chunk add column source text")?;
        }
        let mut spool = Self { db, tlogs, dir };
        spool.reconcile()?;
        Ok(spool)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// After a crash: files with no row go; rows that are waiting but whose
    /// file is gone send their tlog back to where they began.
    fn reconcile(&mut self) -> rusqlite::Result<()> {
        let waiting: Vec<(String, String, i64, i64)> = {
            let mut query = self.db.prepare(
                "select name, recording, seq, start_offset from chunk
                 where state not in ('acknowledged', 'lost') and source is null",
            )?;
            query
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let known: std::collections::HashSet<String> =
            waiting.iter().map(|w| w.0.clone()).collect();
        // Files a crash left before their row.
        for recording in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(recording.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = format!(
                    "{}/{}",
                    recording.file_name().to_string_lossy(),
                    file.file_name().to_string_lossy()
                );
                if !known.contains(&name) {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        // Rows whose file is gone: cut again from where they began.
        let tx = self.db.transaction()?;
        for (name, recording, seq, start) in waiting {
            if self.dir.join(&name).exists() {
                continue;
            }
            tx.execute(
                "delete from chunk where recording = ?1 and seq >= ?2 and state not in ('acknowledged', 'lost')",
                params![recording, seq],
            )?;
            tx.execute(
                "update cursor set offset = min(offset, ?2), seq = min(seq, ?3 - 1) where recording = ?1",
                params![recording, start, seq],
            )?;
        }
        tx.commit()
    }

    /// Cuts the chunks that are due: from every tlog, what was written since
    /// the last cut, once it is `every_us` old (30 s, `CUT_EVERY_US`), 1 MB,
    /// or its tlog stopped growing.
    /// Returns how many were cut.
    ///
    /// # Errors
    /// The database or a chunk's file cannot be written.
    pub fn cut_every(
        &mut self,
        now_us: u64,
        idle_us: u64,
        every_us: u64,
    ) -> std::io::Result<usize> {
        let mut cut = 0;
        let tlogs: Vec<PathBuf> = std::fs::read_dir(&self.tlogs)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "tlog"))
            .collect();
        for tlog in tlogs {
            let Some(recording) = tlog.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let (offset, cut_at, seq): (u64, u64, u32) = self
                .db
                .query_row(
                    "select offset, cut_at_us, seq from cursor where recording = ?1",
                    [&recording],
                    |r| Ok((u(r.get(0)?), u(r.get(1)?), r.get(2)?)),
                )
                .optional()
                .map_err(std::io::Error::other)?
                .map_or_else(
                    || {
                        // First seen: its 30 s start now.
                        self.db
                            .execute(
                                "insert into cursor values (?1, 0, ?2, 0)",
                                params![recording, i(now_us)],
                            )
                            .map(|_| (0, now_us, 0))
                    },
                    Ok,
                )
                .map_err(std::io::Error::other)?;
            let bytes = std::fs::read(&tlog)?;
            let Some(fresh) = bytes.get(usize::try_from(offset).unwrap_or(usize::MAX)..) else {
                continue;
            };
            let records = uxduck_mavlink::tlog::read(fresh);
            let consumed: usize = records.iter().map(|(_, frame)| 8 + frame.len()).sum();
            if consumed == 0 {
                continue;
            }
            let modified_us = std::fs::metadata(&tlog)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(now_us, |d| u64::try_from(d.as_micros()).unwrap_or(now_us));
            let due = consumed as u64 >= CUT_BYTES
                || now_us.saturating_sub(cut_at) >= every_us
                || now_us.saturating_sub(modified_us) >= idle_us;
            if !due {
                continue;
            }
            let slice = &fresh[..consumed];
            let compressed = zstd::bulk::compress(slice, 3)?;
            let sha256 = hex(&Sha256::digest(&compressed));
            let next = seq + 1;
            let name = format!("{recording}/{next:06}-{sha256}.tlog.zst");
            let path = self.path(&name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            {
                use std::io::Write as _;
                let mut file = std::fs::File::create(&path)?;
                file.write_all(&compressed)?;
                file.sync_all()?;
            }
            let first_us = records.first().map_or(0, |r| r.0);
            let last_us = records.last().map_or(0, |r| r.0);
            let end = offset + consumed as u64;
            let tx = self.db.transaction().map_err(std::io::Error::other)?;
            tx.execute(
                "insert or replace into chunk values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'written', null)",
                params![
                    name,
                    recording,
                    next,
                    i(compressed.len() as u64),
                    sha256,
                    i(first_us),
                    i(last_us),
                    i(offset),
                    i(end)
                ],
            )
            .map_err(std::io::Error::other)?;
            tx.execute(
                "insert into cursor values (?1, ?2, ?3, ?4)
                 on conflict(recording) do update set offset = ?2, cut_at_us = ?3, seq = ?4",
                params![recording, i(end), i(now_us), next],
            )
            .map_err(std::io::Error::other)?;
            tx.commit().map_err(std::io::Error::other)?;
            cut += 1;
        }
        Ok(cut)
    }

    /// Cuts the segments that are due from `recordings` (S83): the init
    /// segment, then whole fragments once they make about 10 s, and what is
    /// left once a recording ended. Returns how many were cut.
    ///
    /// # Errors
    /// The database or a recording cannot be read.
    pub fn cut_video(&mut self, recordings: &[Recording], now_us: u64) -> std::io::Result<usize> {
        let mut cut = 0;
        for recording in recordings {
            let Ok(mut file) = std::fs::File::open(&recording.path) else {
                continue;
            };
            let end = file.metadata()?.len();
            let (offset, seq): (u64, u32) = self
                .db
                .query_row(
                    "select offset, seq from cursor where recording = ?1",
                    [&recording.id],
                    |r| Ok((u(r.get(0)?), r.get(1)?)),
                )
                .optional()
                .map_err(std::io::Error::other)?
                .unwrap_or((0, 0));
            if offset >= end {
                continue;
            }
            let (init_end, fragments) = uxduck_media::mp4::layout_from(&mut file, offset, end)?;
            let mut pieces: Vec<Piece> = Vec::new();
            if offset == 0 {
                if init_end == 0 {
                    continue;
                }
                pieces.push(Piece {
                    start: 0,
                    end: init_end,
                    first_us: recording.started_at_us,
                    last_us: recording.started_at_us,
                    ext: "mp4",
                });
            }
            let mut group: Vec<uxduck_media::mp4::Fragment> = Vec::new();
            for (i, fragment) in fragments.iter().enumerate() {
                group.push(*fragment);
                let span: u64 = group.iter().map(|f| f.duration_us).sum();
                let last = i + 1 == fragments.len();
                if span >= SEGMENT_US || (last && recording.ended) {
                    pieces.push(Piece::of(&group, recording.started_at_us));
                    group.clear();
                }
                if pieces.len() >= SEGMENTS_PER_ROUND {
                    break;
                }
            }
            let mut seq = seq;
            for piece in pieces {
                seq += 1;
                self.cut_range(recording, &piece, seq, now_us)?;
                cut += 1;
            }
        }
        Ok(cut)
    }

    fn cut_range(
        &mut self,
        recording: &Recording,
        piece: &Piece,
        seq: u32,
        now_us: u64,
    ) -> std::io::Result<()> {
        let size = piece.end - piece.start;
        let bytes = read_range(&recording.path, piece.start, size)?;
        let sha256 = hex(&Sha256::digest(&bytes));
        let name = format!("{}/{seq:06}-{sha256}.{}", recording.id, piece.ext);
        let tx = self.db.transaction().map_err(std::io::Error::other)?;
        tx.execute(
            "insert or replace into chunk values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'written', ?10)",
            params![
                name,
                recording.id,
                seq,
                i(size),
                sha256,
                i(piece.first_us),
                i(piece.last_us),
                i(piece.start),
                i(piece.end),
                recording.path.to_string_lossy(),
            ],
        )
        .map_err(std::io::Error::other)?;
        tx.execute(
            "insert into cursor values (?1, ?2, ?3, ?4)
             on conflict(recording) do update set offset = ?2, cut_at_us = ?3, seq = ?4",
            params![recording.id, i(piece.end), i(now_us), seq],
        )
        .map_err(std::io::Error::other)?;
        tx.commit().map_err(std::io::Error::other)
    }

    /// Segments whose recording is gone, or shorter than they need: lost,
    /// each with its range. Called every round, so a recording deleted by
    /// hand or by Settings › Storage is reported too.
    ///
    /// # Errors
    /// The database cannot be written.
    pub fn find_lost(&mut self) -> rusqlite::Result<Vec<Lost>> {
        let waiting: Vec<(String, String, String, i64, i64, i64)> = {
            let mut query = self.db.prepare(
                "select name, recording, source, end_offset, first_us, last_us from chunk
                 where source is not null and state not in ('acknowledged', 'lost')",
            )?;
            query
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut lost: Vec<Lost> = Vec::new();
        for (name, recording, source, end, first_us, last_us) in waiting {
            let have = std::fs::metadata(&source).map_or(0, |m| m.len());
            if have >= u(end) {
                continue;
            }
            self.mark(&name, State::Lost)?;
            match lost.iter_mut().find(|l| l.recording == recording) {
                Some(l) => {
                    l.from_us = l.from_us.min(u(first_us));
                    l.to_us = l.to_us.max(u(last_us));
                }
                None => lost.push(Lost {
                    recording,
                    from_us: u(first_us),
                    to_us: u(last_us),
                }),
            }
        }
        Ok(lost)
    }

    /// What of a recording has not reached the account: from its first
    /// segment still waiting (or where cutting stopped) to `ended_at_us`.
    /// `None` when all of it has.
    ///
    /// # Errors
    /// The database cannot be read.
    pub fn unstored(
        &self,
        recording: &Recording,
        ended_at_us: u64,
    ) -> rusqlite::Result<Option<(u64, u64)>> {
        let (waiting_from, cut_to, offset): (Option<i64>, Option<i64>, Option<i64>) =
            self.db.query_row(
                "select
                    (select min(first_us) from chunk where recording = ?1 and state not in ('acknowledged', 'lost')),
                    (select max(last_us) from chunk where recording = ?1),
                    (select offset from cursor where recording = ?1)",
                [&recording.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let size = std::fs::metadata(&recording.path).map_or(0, |m| m.len());
        let all_cut = offset.is_some_and(|o| u(o) >= size);
        Ok(match (waiting_from, all_cut) {
            (Some(from), _) => Some((u(from), ended_at_us)),
            (None, true) => None,
            (None, false) => Some((cut_to.map_or(recording.started_at_us, u), ended_at_us)),
        })
    }

    /// Under disk pressure (S83, ADR-0003 "Eviction"): deletes ended
    /// recordings, oldest first and those wholly in the account before any
    /// other, until `need` bytes are free. Returns what went before it
    /// reached the account, which the caller journals and tells.
    ///
    /// # Errors
    /// The database cannot be read or written.
    pub fn evict(&mut self, ended: &[(Recording, u64)], need: u64) -> rusqlite::Result<Vec<Lost>> {
        let mut order: Vec<(Option<(u64, u64)>, &Recording)> = Vec::new();
        for (recording, ended_at_us) in ended {
            if recording.path.exists() {
                order.push((self.unstored(recording, *ended_at_us)?, recording));
            }
        }
        order.sort_by_key(|(unstored, r)| (unstored.is_some(), r.started_at_us));
        let mut freed = 0;
        let mut lost = Vec::new();
        for (unstored, recording) in order {
            if freed >= need {
                break;
            }
            let size = std::fs::metadata(&recording.path).map_or(0, |m| m.len());
            if std::fs::remove_file(&recording.path).is_err() {
                continue;
            }
            let _ = std::fs::remove_file(recording.path.with_extension("klv"));
            freed += size;
            self.db.execute(
                "update chunk set state = 'lost'
                 where recording = ?1 and state not in ('acknowledged', 'lost')",
                [&recording.id],
            )?;
            if let Some((from_us, to_us)) = unstored {
                lost.push(Lost {
                    recording: recording.id.clone(),
                    from_us,
                    to_us,
                });
            }
        }
        Ok(lost)
    }

    /// The oldest chunks not acknowledged yet, up to `limit`.
    ///
    /// # Errors
    /// The database cannot be read.
    pub fn waiting(&self, limit: usize) -> rusqlite::Result<Vec<(Chunk, State)>> {
        let mut query = self.db.prepare(
            "select name, recording, seq, size, sha256, first_us, last_us, state from chunk
             where state not in ('acknowledged', 'lost') order by first_us, seq limit ?1",
        )?;
        query
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                let state: String = r.get(7)?;
                Ok((
                    Chunk {
                        name: r.get(0)?,
                        recording: r.get(1)?,
                        seq: r.get(2)?,
                        size: u(r.get(3)?),
                        sha256: r.get(4)?,
                        first_us: u(r.get(5)?),
                        last_us: u(r.get(6)?),
                    },
                    if state == "uploaded" {
                        State::Uploaded
                    } else {
                        State::Written
                    },
                ))
            })?
            .collect()
    }

    /// How much is waiting: chunks and bytes.
    ///
    /// # Errors
    /// The database cannot be read.
    pub fn backlog(&self) -> rusqlite::Result<(u64, u64)> {
        self.db.query_row(
            "select count(*), coalesce(sum(size), 0) from chunk where state not in ('acknowledged', 'lost')",
            [],
            |r| Ok((u(r.get(0)?), u(r.get(1)?))),
        )
    }

    /// A chunk's bytes, as they go to the account.
    ///
    /// # Errors
    /// Its file is gone.
    pub fn bytes(&self, chunk: &Chunk) -> std::io::Result<Vec<u8>> {
        let range: Option<(String, i64)> = self
            .db
            .query_row(
                "select source, start_offset from chunk where name = ?1 and source is not null",
                [&chunk.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(std::io::Error::other)?;
        let Some((source, start)) = range else {
            return std::fs::read(self.path(&chunk.name));
        };
        read_range(Path::new(&source), u(start), chunk.size)
    }

    /// Moves a chunk on; acknowledged, its file goes.
    ///
    /// # Errors
    /// The database cannot be written.
    pub fn mark(&self, name: &str, state: State) -> rusqlite::Result<()> {
        self.db.execute(
            "update chunk set state = ?2 where name = ?1",
            params![name, state.word()],
        )?;
        if state == State::Acknowledged {
            let _ = std::fs::remove_file(self.path(name));
        }
        Ok(())
    }
}

/// A segment about to be cut: its byte range and when it plays.
struct Piece {
    start: u64,
    end: u64,
    first_us: u64,
    last_us: u64,
    ext: &'static str,
}

impl Piece {
    fn of(group: &[uxduck_media::mp4::Fragment], started_at_us: u64) -> Self {
        let first = group[0];
        let last = group[group.len() - 1];
        Self {
            start: first.offset,
            end: last.offset + last.len,
            first_us: started_at_us + first.start_us,
            last_us: started_at_us + last.start_us + last.duration_us,
            ext: "m4s",
        }
    }
}

fn read_range(path: &Path, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(path)?;
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut bytes = vec![0; usize::try_from(len).map_err(std::io::Error::other)?];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlog_with(dir: &Path, name: &str, records: u64, from_us: u64) {
        let mut out = Vec::new();
        for i in 0..records {
            let frame = uxduck_mavlink::sim::heartbeat_frame(1, 1, u8::try_from(i % 256).unwrap());
            uxduck_mavlink::tlog::record(&mut out, from_us + i * 1_000_000, &frame).unwrap();
        }
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.tlog"));
        let mut existing = std::fs::read(&path).unwrap_or_default();
        existing.extend(out);
        std::fs::write(path, existing).unwrap();
    }

    #[test]
    fn a_growing_tlog_is_cut_into_chunks_that_reassemble_it() {
        let data = tempfile::tempdir().unwrap();
        let tlogs = data.path().join("tlogs");
        tlog_with(&tlogs, "udp-1", 10, 1_000_000);
        let mut spool = Spool::open(data.path(), tlogs.clone()).unwrap();
        // Not due: fresh, small and still growing.
        assert_eq!(
            spool.cut_every(2_000_000, u64::MAX, CUT_EVERY_US).unwrap(),
            0
        );
        // 30 s on: cut.
        assert_eq!(
            spool
                .cut_every(2_000_000 + CUT_EVERY_US, u64::MAX, CUT_EVERY_US)
                .unwrap(),
            1
        );
        tlog_with(&tlogs, "udp-1", 5, 20_000_000);
        // It stopped growing: cut what is left.
        assert_eq!(
            spool
                .cut_every(3_000_000 + CUT_EVERY_US, 0, CUT_EVERY_US)
                .unwrap(),
            1
        );
        let waiting = spool.waiting(10).unwrap();
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].0.seq, 1);
        assert_eq!(
            (waiting[0].0.first_us, waiting[0].0.last_us),
            (1_000_000, 10_000_000)
        );
        assert!(waiting[0].0.name.starts_with("udp-1/000001-"));
        // Together they are the tlog, byte for byte.
        let mut joined = Vec::new();
        for (chunk, _) in &waiting {
            joined.extend(zstd::bulk::decompress(&spool.bytes(chunk).unwrap(), 1 << 24).unwrap());
        }
        assert_eq!(joined, std::fs::read(tlogs.join("udp-1.tlog")).unwrap());
        assert_eq!(spool.backlog().unwrap().0, 2);

        spool.mark(&waiting[0].0.name, State::Acknowledged).unwrap();
        assert!(spool.bytes(&waiting[0].0).is_err(), "its file is gone");
        assert_eq!(spool.backlog().unwrap().0, 1);
    }

    #[test]
    fn a_crash_between_file_and_row_is_cut_again() {
        let data = tempfile::tempdir().unwrap();
        let tlogs = data.path().join("tlogs");
        tlog_with(&tlogs, "udp-1", 4, 1_000_000);
        let mut spool = Spool::open(data.path(), tlogs.clone()).unwrap();
        spool.cut_every(1, 0, CUT_EVERY_US).unwrap();
        let first = spool.waiting(1).unwrap()[0].0.clone();
        // A file with no row, as a crash before the commit leaves.
        let stray = data.path().join("spool/udp-1/000002-dead.tlog.zst");
        std::fs::write(&stray, b"half").unwrap();
        // A row whose file vanished.
        std::fs::remove_file(data.path().join("spool").join(&first.name)).unwrap();
        drop(spool);
        let mut spool = Spool::open(data.path(), tlogs).unwrap();
        assert!(!stray.exists(), "a file with no row goes");
        assert_eq!(spool.backlog().unwrap().0, 0, "the row with no file goes");
        // And the tlog is cut again from where that chunk began.
        spool.cut_every(2, 0, CUT_EVERY_US).unwrap();
        let again = spool.waiting(1).unwrap()[0].0.clone();
        assert_eq!((again.seq, again.first_us), (1, first.first_us));
        assert_eq!(again.sha256, first.sha256, "the same bytes, the same name");
    }

    /// A recording as the recorder writes it: the init segment, then a
    /// fragment of one second per keyframe.
    fn recording_with(path: &Path, seconds: u32) {
        use uxduck_media::mp4::{ParameterSets, Sample, fragment, init_segment};
        use uxduck_media::nal::Codec;
        let sps = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x10, 0x00, 0x00,
            0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x19, 0x60,
        ];
        let key = [
            &[0, 0, 0, 1][..],
            &sps,
            &[0, 0, 0, 1, 0x68, 0xeb, 0xe3],
            &[0, 0, 0, 1, 0x65, 1, 2, 3],
        ]
        .concat();
        let sets = ParameterSets::of(Codec::H264, &key);
        let mut file = init_segment(Codec::H264, &sets, 1280, 720);
        for i in 0..seconds {
            file.extend(fragment(
                i + 1,
                u64::from(i) * 90_000,
                &[Sample {
                    annex_b: &key,
                    duration: 90_000,
                    key: true,
                }],
            ));
        }
        std::fs::write(path, file).unwrap();
    }

    #[test]
    fn a_recording_is_cut_into_segments_of_its_own_bytes_and_its_tail_once_it_ends() {
        let data = tempfile::tempdir().unwrap();
        let video = data.path().join("video");
        std::fs::create_dir_all(&video).unwrap();
        let path = video.join("vr1.mp4");
        recording_with(&path, 25);
        let mut spool = Spool::open(data.path(), data.path().join("tlogs")).unwrap();
        let mut recording = Recording {
            id: "vr1".into(),
            path: path.clone(),
            started_at_us: 1_000_000_000,
            ended: false,
        };
        // Recording: the init segment and two of 10 s; the last 5 s wait.
        assert_eq!(spool.cut_video(&[recording.clone()], 1).unwrap(), 3);
        assert_eq!(spool.cut_video(&[recording.clone()], 2).unwrap(), 0);
        recording.ended = true;
        assert_eq!(spool.cut_video(&[recording.clone()], 3).unwrap(), 1);
        assert_eq!(spool.cut_video(&[recording.clone()], 4).unwrap(), 0);
        let waiting = spool.waiting(10).unwrap();
        let names: Vec<_> = waiting.iter().map(|(c, _)| c.name.clone()).collect();
        assert!(names[0].starts_with("vr1/000001-") && names[0].contains(".mp4"));
        assert!(names[1..].iter().all(|n| n.contains(".m4s")));
        let times: Vec<_> = waiting
            .iter()
            .map(|(c, _)| (c.first_us - 1_000_000_000, c.last_us - 1_000_000_000))
            .collect();
        assert_eq!(
            times,
            [
                (0, 0),
                (0, 10_000_000),
                (10_000_000, 20_000_000),
                (20_000_000, 25_000_000)
            ]
        );
        // Their bytes, in order, are the recording; nothing was copied.
        let joined: Vec<u8> = waiting
            .iter()
            .flat_map(|(c, _)| spool.bytes(c).unwrap())
            .collect();
        assert_eq!(joined, std::fs::read(&path).unwrap());
        assert!(!data.path().join("spool/vr1").exists());
        // Restarted, the same segments are not cut again.
        drop(spool);
        let mut spool = Spool::open(data.path(), data.path().join("tlogs")).unwrap();
        assert_eq!(spool.cut_video(&[recording], 5).unwrap(), 0);
        assert_eq!(spool.backlog().unwrap().0, 4);
    }

    #[test]
    fn a_recording_deleted_before_it_went_is_lost_with_its_range_and_eviction_keeps_it_for_last() {
        let data = tempfile::tempdir().unwrap();
        let video = data.path().join("video");
        std::fs::create_dir_all(&video).unwrap();
        let mut spool = Spool::open(data.path(), data.path().join("tlogs")).unwrap();
        let mut recordings = Vec::new();
        for (n, start) in [(1, 100_000_000), (2, 200_000_000), (3, 300_000_000)] {
            let path = video.join(format!("vr{n}.mp4"));
            recording_with(&path, 12);
            recordings.push(Recording {
                id: format!("vr{n}"),
                path,
                started_at_us: start,
                ended: true,
            });
        }
        spool.cut_video(&recordings, 1).unwrap();
        // The oldest is wholly in the account; the second only in part.
        for (chunk, _) in spool.waiting(100).unwrap() {
            if chunk.recording == "vr1" || chunk.seq == 1 && chunk.recording == "vr2" {
                spool.mark(&chunk.name, State::Acknowledged).unwrap();
            }
        }
        let ended: Vec<_> = recordings
            .iter()
            .map(|r| (r.clone(), r.started_at_us + 12_000_000))
            .collect();
        // Room for one: the one already stored goes, and nothing is lost.
        let one = std::fs::metadata(&recordings[0].path).unwrap().len();
        assert_eq!(spool.evict(&ended, one).unwrap(), []);
        assert!(!recordings[0].path.exists());
        // Room for another: the oldest of the rest, with what it had not sent.
        let lost = spool.evict(&ended, one).unwrap();
        assert_eq!(
            lost,
            [Lost {
                recording: "vr2".into(),
                from_us: 200_000_000,
                to_us: 212_000_000
            }]
        );
        assert!(recordings[2].path.exists());
        // Deleted by hand: found lost on the next round, once.
        std::fs::remove_file(&recordings[2].path).unwrap();
        let lost = spool.find_lost().unwrap();
        assert_eq!(
            lost,
            [Lost {
                recording: "vr3".into(),
                from_us: 300_000_000,
                to_us: 312_000_000
            }]
        );
        assert_eq!(spool.find_lost().unwrap(), []);
        assert_eq!(spool.backlog().unwrap(), (0, 0));
    }
}

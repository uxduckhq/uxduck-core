//! The event log file: one JSON event per line, appended in `seq` order
//! (ADR-0007). Writes reach the OS at the end of every batch, so a crash or
//! `kill -9` loses nothing handed over; `fsync` runs on an interval, so a
//! power cut loses at most that interval.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use uxduck_core::events::Event;

use super::finite;

#[derive(Debug)]
pub struct EventLog {
    writer: BufWriter<File>,
    unsynced: bool,
}

/// What reading the log found.
#[derive(Debug, Default)]
pub struct Read {
    pub events: Vec<Event>,
    /// Bytes of a torn last line (a write cut short) that were removed.
    pub truncated_bytes: u64,
    /// Floats read as 0 that an older daemon wrote as `null` (a NaN or an
    /// infinity), rather than ending the log there.
    pub repaired_floats: usize,
    /// Whole lines this build cannot read, such as an event written before
    /// a field it requires existed. They stay in the file, so a build that
    /// reads them still can; replay stops at the first one it needs.
    pub unreadable: Vec<Unreadable>,
}

/// A whole line of the log that does not read as an event.
#[derive(Debug)]
pub struct Unreadable {
    /// From 1.
    pub line: usize,
    pub error: String,
}

impl EventLog {
    /// Reads every event, removes a torn last line, and opens for appending.
    /// Only a last line without its newline is torn: a whole line that
    /// does not read is kept and reported, never cut off with everything
    /// after it.
    ///
    /// # Errors
    /// When the file cannot be read or opened.
    pub fn open(path: &Path) -> std::io::Result<(Self, Read)> {
        let mut read = Read::default();
        if path.exists() {
            let mut file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut reader = BufReader::new(&mut file);
            let mut good_len = 0u64;
            let mut line = Vec::new();
            loop {
                line.clear();
                let n = reader.read_until(b'\n', &mut line)?;
                // A line without its newline can only be the last write cut
                // short.
                if n == 0 || line.last() != Some(&b'\n') {
                    break;
                }
                good_len += n as u64;
                let parsed = std::str::from_utf8(&line)
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        finite::read::<Event>(text.trim_end()).map_err(|e| e.to_string())
                    });
                match parsed {
                    Ok((event, _, repaired)) => {
                        read.events.push(event);
                        read.repaired_floats += repaired;
                    }
                    Err(error) => read.unreadable.push(Unreadable {
                        line: read.events.len() + read.unreadable.len() + 1,
                        error,
                    }),
                }
            }
            let len = file.seek(SeekFrom::End(0))?;
            if len > good_len {
                read.truncated_bytes = len - good_len;
                file.set_len(good_len)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok((
            Self {
                writer: BufWriter::new(file),
                unsynced: false,
            },
            read,
        ))
    }

    /// Buffers one event.
    ///
    /// # Errors
    /// When the event cannot be written.
    pub fn append(&mut self, event: &Event) -> std::io::Result<()> {
        let line =
            finite::write(event, &format!("event {}", event.seq)).map_err(std::io::Error::other)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.unsynced = true;
        Ok(())
    }

    /// Hands buffered events to the OS.
    ///
    /// # Errors
    /// When the write fails.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    /// Whether anything was written since the last sync.
    #[must_use]
    pub fn unsynced(&self) -> bool {
        self.unsynced
    }

    /// Flushes and fsyncs, if anything was written since the last sync.
    ///
    /// # Errors
    /// When the write or sync fails.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        if self.unsynced {
            self.writer.get_ref().sync_data()?;
            self.unsynced = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use uxduck_core::events::{Actor, Change};

    use super::*;

    fn event(seq: u64) -> Event {
        Event {
            seq,
            ts_us: seq,
            actor: Actor::default(),
            change: Change::OperatorRemoved {
                id: format!("o{seq}"),
            },
        }
    }

    #[test]
    fn appends_survive_reopen_and_a_torn_line_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.ndjson");
        {
            let (mut log, read) = EventLog::open(&path).unwrap();
            assert!(read.events.is_empty());
            log.append(&event(1)).unwrap();
            log.append(&event(2)).unwrap();
            log.flush().unwrap();
        }
        // A write cut short by a crash.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"seq":3,"tsUs""#)
            .unwrap();

        let (mut log, read) = EventLog::open(&path).unwrap();
        assert_eq!(
            read.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(read.truncated_bytes > 0);
        log.append(&event(3)).unwrap();
        log.sync().unwrap();
        drop(log);
        let (_, read) = EventLog::open(&path).unwrap();
        assert_eq!(
            read.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(read.truncated_bytes, 0);
    }

    /// A whole line can fail to read for reasons other than a crash: a
    /// newer build's event, or an older one's missing a field this build
    /// requires (beta.2 cut off a 40 MB log at one). It is kept, and so is
    /// everything after it.
    #[test]
    fn a_whole_line_that_does_not_read_is_kept_with_everything_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.ndjson");
        let line = |seq| format!("{}\n", serde_json::to_string(&event(seq)).unwrap());
        let mut bytes = line(1).into_bytes();
        bytes.extend_from_slice(
            b"{\"seq\":2,\"tsUs\":2,\"change\":{\"type\":\"fromANewerBuild\"}}\n",
        );
        bytes.extend_from_slice(b"\xff not text\n");
        bytes.extend_from_slice(line(4).as_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let (_, read) = EventLog::open(&path).unwrap();
        assert_eq!(
            read.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 4]
        );
        assert_eq!(
            read.unreadable.iter().map(|u| u.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(read.truncated_bytes, 0);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

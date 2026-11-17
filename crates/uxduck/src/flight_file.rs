//! A flight as one file (ADR-0062): `<name>.uxflight`, an ordinary zip
//! whose files are kept as they are, without compression. Any computer opens
//! a zip, so a person can always take the flight elsewhere: renamed to
//! `.zip`, it is a telemetry log Mission Planner and QGroundControl replay,
//! the camera's own video files and UXDuck's details in `flight.json`. With
//! nothing compressed, each file sits whole at a known place in the zip, so
//! UXDuck plays a video straight from the disk and seeks in it.
//!
//! Zip64 throughout, since a flight's video passes 4 GB. Written by UXDuck's
//! own code, a few hundred lines, as its log readers are (ADR-0039 R15):
//! the reader also takes deflated files, so a zip a person made again with
//! another program still opens, and says so when a video in one cannot be
//! played from where it is.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uxduck_core::domain::video::PackedVideo;
use uxduck_engine::runtime::{FlightFile, OffloadFlight, OffloadJob, ReplayRead, VideoAt};

/// The file's extension.
pub const EXTENSION: &str = "uxflight";
/// `flight.json`'s `format`.
pub const FORMAT: &str = "uxduck.flight/1";
/// UXDuck's details of the flight.
pub const MANIFEST: &str = "flight.json";
/// The flight's telemetry log, cut to its replay.
pub const TELEMETRY: &str = "telemetry.tlog";
/// What the file is, for whoever opens it without UXDuck.
pub const README: &str = "README.txt";

/// What every `.uxflight` says of itself, in `README.txt`.
pub const README_TEXT: &str = "\
This is a flight saved by UXDuck (uxduck.com).

A .uxflight file is an ordinary zip file. Nothing in it is compressed or
locked. Rename it to .zip to open it with any program that opens zip files.

Inside:

telemetry.tlog
  The flight's telemetry log, in the MAVLink .tlog format that Mission
  Planner, QGroundControl and MAVProxy replay.

video/
  The camera's recordings. A .ts file is the camera's original as UXDuck
  received it, with any KLV metadata. VLC plays it. The .mp4 beside it has
  the same pictures and sound in a file every video player plays. A video
  added to the flight from elsewhere is kept as it was.

flight.json
  UXDuck's details of the flight: its name, vehicle, times, who flew it,
  the facts in the Logbook and when each video starts on the flight's
  clock. Other apps do not need it to play the log or the videos.

Open the file in UXDuck to replay the flight with its map, telemetry and
video in step.
";

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const END64: u32 = 0x0606_4b50;
const LOCATOR64: u32 = 0x0706_4b50;
/// Zip64's extra field.
const ZIP64: u16 = 0x0001;
/// The version a zip64 reader needs, 4.5.
const NEEDS: u16 = 45;
/// Made on Unix (3), to version 4.5, so permissions are read.
const MADE_BY: u16 = (3 << 8) | NEEDS;
/// Names are UTF-8.
const UTF8: u16 = 1 << 11;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
/// A local header before its name: the 30 bytes zip fixes.
const LOCAL_LEN: u64 = 30;
/// The local header's zip64 field: its id, its size and two sizes.
const LOCAL_EXTRA: u16 = 4 + 16;
/// How much is copied at a time.
const CHUNK: usize = 1 << 20;
/// The longest an end record and its comment are, where the reader looks
/// for it: zip fixes a comment at 65,535 bytes.
const END_SEARCH: u64 = 22 + 0xFFFF;

/// A file in a `.uxflight`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub crc: u32,
    /// Its size as kept, and once read.
    pub stored_size: u64,
    pub size: u64,
    /// Where its bytes start in the file.
    pub data_offset: u64,
    pub method: u16,
}

impl Entry {
    /// Kept as it is, so its bytes can be read from where they are.
    #[must_use]
    pub fn whole(&self) -> bool {
        self.method == STORED
    }
}

struct Written {
    name: String,
    offset: u64,
    crc: u32,
    size: u64,
}

/// Where one file of a `.uxflight` is written: its checksum and size kept
/// as it goes.
struct Sink<'a> {
    file: std::io::BufWriter<&'a mut File>,
    crc: crc32fast::Hasher,
    size: u64,
    progress: &'a mut dyn FnMut(u64),
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.crc.update(&buf[..n]);
        self.size += n as u64;
        (self.progress)(self.size);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Writes a `.uxflight`, one file after another, each without compression.
pub struct Writer {
    file: File,
    written: Vec<Written>,
    /// DOS time and date for each file: the flight's.
    time: u16,
    date: u16,
}

impl Writer {
    /// Starts the file at `path`, dated `at_us` (Unix microseconds, UTC).
    ///
    /// # Errors
    /// It cannot be created.
    pub fn create(path: &Path, at_us: u64) -> std::io::Result<Self> {
        let (time, date) = dos_time(at_us);
        Ok(Self {
            file: File::create(path)?,
            written: Vec::new(),
            time,
            date,
        })
    }

    /// Adds `bytes` as `name`.
    ///
    /// # Errors
    /// The disk refuses it.
    #[cfg(test)]
    pub fn add_bytes(&mut self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.add_with(name, &mut |out| out.write_all(bytes), &mut |_| {})
            .map(|_| ())
    }

    /// Adds what `write` writes as `name`, as it writes it, so nothing of
    /// it is held in memory: a flight's telemetry is written this way
    /// however long it is. Says after each part how many bytes it has
    /// written of this file, and returns them all.
    ///
    /// # Errors
    /// `write` fails or the disk refuses it.
    pub fn add_with(
        &mut self,
        name: &str,
        write: &mut dyn FnMut(&mut dyn Write) -> std::io::Result<()>,
        progress: &mut dyn FnMut(u64),
    ) -> std::io::Result<u64> {
        let name_len = u16::try_from(name.len()).map_err(|_| {
            std::io::Error::other(format!("a name in the file is too long: {name}"))
        })?;
        let offset = self.file.stream_position()?;
        let mut header = Vec::with_capacity(64 + name.len());
        put32(&mut header, LOCAL);
        put16(&mut header, NEEDS);
        put16(&mut header, UTF8);
        put16(&mut header, STORED);
        put16(&mut header, self.time);
        put16(&mut header, self.date);
        put32(&mut header, 0);
        put32(&mut header, u32::MAX);
        put32(&mut header, u32::MAX);
        put16(&mut header, name_len);
        put16(&mut header, LOCAL_EXTRA);
        header.extend_from_slice(name.as_bytes());
        put16(&mut header, ZIP64);
        put16(&mut header, 16);
        put64(&mut header, 0);
        put64(&mut header, 0);
        self.file.write_all(&header)?;
        let (crc, size) = {
            let mut sink = Sink {
                file: std::io::BufWriter::with_capacity(CHUNK, &mut self.file),
                crc: crc32fast::Hasher::new(),
                size: 0,
                progress,
            };
            write(&mut sink)?;
            sink.file.flush()?;
            (sink.crc.finalize(), sink.size)
        };
        let end = self.file.stream_position()?;
        // The header goes before the bytes, so it is finished once they are.
        self.file.seek(SeekFrom::Start(offset + 14))?;
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.seek(SeekFrom::Start(
            offset + LOCAL_LEN + u64::from(name_len) + 4,
        ))?;
        let mut sizes = Vec::with_capacity(16);
        put64(&mut sizes, size);
        put64(&mut sizes, size);
        self.file.write_all(&sizes)?;
        self.file.seek(SeekFrom::Start(end))?;
        self.written.push(Written {
            name: name.to_owned(),
            offset,
            crc,
            size,
        });
        Ok(size)
    }

    /// Writes the zip's directory and puts the file on the disk, so it is
    /// whole once this returns.
    ///
    /// # Errors
    /// The disk refuses it.
    pub fn finish(mut self) -> std::io::Result<()> {
        let start = self.file.stream_position()?;
        let mut out = Vec::new();
        for entry in &self.written {
            put32(&mut out, CENTRAL);
            put16(&mut out, MADE_BY);
            put16(&mut out, NEEDS);
            put16(&mut out, UTF8);
            put16(&mut out, STORED);
            put16(&mut out, self.time);
            put16(&mut out, self.date);
            put32(&mut out, entry.crc);
            put32(&mut out, u32::MAX);
            put32(&mut out, u32::MAX);
            put16(
                &mut out,
                u16::try_from(entry.name.len()).unwrap_or(u16::MAX),
            );
            put16(&mut out, 4 + 24);
            put16(&mut out, 0);
            put16(&mut out, 0);
            put16(&mut out, 0);
            // A file anyone may read (0644).
            put32(&mut out, 0o100_644 << 16);
            put32(&mut out, u32::MAX);
            out.extend_from_slice(entry.name.as_bytes());
            put16(&mut out, ZIP64);
            put16(&mut out, 24);
            put64(&mut out, entry.size);
            put64(&mut out, entry.size);
            put64(&mut out, entry.offset);
        }
        let directory = out.len() as u64;
        let end64 = start + directory;
        let count = self.written.len() as u64;
        put32(&mut out, END64);
        put64(&mut out, 44);
        put16(&mut out, MADE_BY);
        put16(&mut out, NEEDS);
        put32(&mut out, 0);
        put32(&mut out, 0);
        put64(&mut out, count);
        put64(&mut out, count);
        put64(&mut out, directory);
        put64(&mut out, start);
        put32(&mut out, LOCATOR64);
        put32(&mut out, 0);
        put64(&mut out, end64);
        put32(&mut out, 1);
        put32(&mut out, END);
        put16(&mut out, 0);
        put16(&mut out, 0);
        put16(&mut out, u16::MAX);
        put16(&mut out, u16::MAX);
        put32(&mut out, u32::MAX);
        put32(&mut out, u32::MAX);
        put16(&mut out, 0);
        self.file.write_all(&out)?;
        self.file.sync_all()
    }
}

/// The files in the `.uxflight` (or any zip) at `path`.
///
/// # Errors
/// It cannot be read or is not a zip.
pub fn entries(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut file = File::open(path)?;
    entries_of(&mut file)
}

fn bad(why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("not a .uxflight file: {why}"),
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the end record, zip64's and the directory, read in the order a zip keeps them"
)]
fn entries_of(file: &mut File) -> std::io::Result<Vec<Entry>> {
    let len = file.seek(SeekFrom::End(0))?;
    let tail_len = len.min(END_SEARCH);
    file.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; usize::try_from(tail_len).unwrap_or(0)];
    file.read_exact(&mut tail)?;
    let at = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| get32(&tail, i) == END)
        .ok_or_else(|| bad("it has no zip directory"))?;
    let end_at = len - tail_len + at as u64;
    let mut count = u64::from(get16(&tail, at + 10));
    let mut size = u64::from(get32(&tail, at + 12));
    let mut start = u64::from(get32(&tail, at + 16));
    if count == 0xFFFF || size == u64::from(u32::MAX) || start == u64::from(u32::MAX) {
        let locator = end_at
            .checked_sub(20)
            .ok_or_else(|| bad("its zip64 directory is missing"))?;
        let mut record = [0u8; 20];
        file.seek(SeekFrom::Start(locator))?;
        file.read_exact(&mut record)?;
        if get32(&record, 0) != LOCATOR64 {
            return Err(bad("its zip64 directory is missing"));
        }
        let mut end64 = [0u8; 56];
        file.seek(SeekFrom::Start(get64(&record, 8)))?;
        file.read_exact(&mut end64)?;
        if get32(&end64, 0) != END64 {
            return Err(bad("its zip64 directory is damaged"));
        }
        count = get64(&end64, 32);
        size = get64(&end64, 40);
        start = get64(&end64, 48);
    }
    if start.checked_add(size).is_none_or(|end| end > len) {
        return Err(bad("its directory is past its end"));
    }
    file.seek(SeekFrom::Start(start))?;
    let mut directory =
        vec![0u8; usize::try_from(size).map_err(|_| bad("its directory is too large"))?];
    file.read_exact(&mut directory)?;
    let mut entries = Vec::new();
    let mut at = 0usize;
    for _ in 0..count {
        if at + 46 > directory.len() || get32(&directory, at) != CENTRAL {
            return Err(bad("its directory is damaged"));
        }
        let method = get16(&directory, at + 10);
        let crc = get32(&directory, at + 16);
        let mut stored_size = u64::from(get32(&directory, at + 20));
        let mut size = u64::from(get32(&directory, at + 24));
        let name_len = usize::from(get16(&directory, at + 28));
        let extra_len = usize::from(get16(&directory, at + 30));
        let comment_len = usize::from(get16(&directory, at + 32));
        let mut offset = u64::from(get32(&directory, at + 42));
        let name_at = at + 46;
        let extra_at = name_at + name_len;
        let next = extra_at + extra_len + comment_len;
        if next > directory.len() {
            return Err(bad("its directory is damaged"));
        }
        let name = String::from_utf8_lossy(&directory[name_at..extra_at]).into_owned();
        // Zip64 gives, in this order, each size or offset too large for
        // its 32-bit field.
        let mut field = extra_at;
        while field + 4 <= extra_at + extra_len {
            let (id, len) = (
                get16(&directory, field),
                usize::from(get16(&directory, field + 2)),
            );
            let body = field + 4;
            if id == ZIP64 {
                let mut value = body;
                for slot in [&mut size, &mut stored_size, &mut offset] {
                    if *slot == u64::from(u32::MAX) && value + 8 <= body + len {
                        *slot = get64(&directory, value);
                        value += 8;
                    }
                }
            }
            field = body + len;
        }
        entries.push(Entry {
            name,
            crc,
            stored_size,
            size,
            data_offset: offset,
            method,
        });
        at = next;
    }
    for entry in &mut entries {
        let mut local = [0u8; 30];
        file.seek(SeekFrom::Start(entry.data_offset))?;
        file.read_exact(&mut local)?;
        if get32(&local, 0) != LOCAL {
            return Err(bad(&format!("{} is damaged", entry.name)));
        }
        entry.data_offset +=
            LOCAL_LEN + u64::from(get16(&local, 26)) + u64::from(get16(&local, 28));
        if entry
            .data_offset
            .checked_add(entry.stored_size)
            .is_none_or(|end| end > len)
        {
            return Err(bad(&format!("{} is cut short", entry.name)));
        }
    }
    Ok(entries)
}

/// Reads one of the file's entries as it is kept or once inflated.
///
/// # Errors
/// It cannot be read, or another program compressed it in a way zip does
/// not name as one of the two kinds UXDuck reads.
pub fn reader(path: &Path, entry: &Entry) -> std::io::Result<Box<dyn Read + Send>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(entry.data_offset))?;
    let kept = BufReader::with_capacity(CHUNK, file).take(entry.stored_size);
    match entry.method {
        STORED => Ok(Box::new(kept)),
        DEFLATED => Ok(Box::new(flate2::read::DeflateDecoder::new(kept))),
        other => Err(std::io::Error::other(format!(
            "{} was compressed by another program with method {other}, which UXDuck does not read",
            entry.name
        ))),
    }
}

/// The entry named `name` in the file's `entries`.
#[must_use]
pub fn find<'a>(entries: &'a [Entry], name: &str) -> Option<&'a Entry> {
    entries.iter().find(|e| e.name == name)
}

/// Reads the entry named `name` whole.
///
/// # Errors
/// It is not in the file or cannot be read.
#[cfg(test)]
pub fn read(path: &Path, name: &str) -> std::io::Result<Vec<u8>> {
    let entries = entries(path)?;
    let entry = find(&entries, name).ok_or_else(|| bad(&format!("it has no {name}")))?;
    let mut out = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
    reader(path, entry)?.read_to_end(&mut out)?;
    Ok(out)
}

/// Reads every file in it back from the disk and checks each against what
/// the zip says it holds, saying after each part how many bytes it has read
/// of them all. A disk that lost a write fails here, before anything is
/// deleted on the strength of it.
///
/// # Errors
/// A file that does not read back as it was written, in words.
pub fn verify(path: &Path, progress: &mut dyn FnMut(u64)) -> std::io::Result<()> {
    let entries = entries(path)?;
    let mut buffer = vec![0u8; CHUNK];
    let mut done = 0u64;
    for entry in &entries {
        let mut from = reader(path, entry)?;
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = from.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            crc.update(&buffer[..n]);
            size += n as u64;
            done += n as u64;
            progress(done);
        }
        if size != entry.size || crc.finalize() != entry.crc {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} did not read back as it was written", entry.name),
            ));
        }
    }
    Ok(())
}

/// `flight.json`: UXDuck's details of the flight, in the shapes UXDuck keeps
/// them, and where each video is in the file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format: String,
    flight: uxduck_core::domain::flight::Flight,
    #[serde(default)]
    recordings: Vec<ManifestRecording>,
    #[serde(default)]
    written_by: String,
    #[serde(default)]
    written_at_us: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestRecording {
    #[serde(flatten)]
    recording: uxduck_core::domain::video::VideoRecording,
    /// The original as recorded, and the copy every player plays.
    entry: String,
    playable: String,
}

/// A `.uxflight`'s `flight.json`, read with its directory: what the engine
/// adds to the Logbook (ADR-0062).
///
/// # Errors
/// In words: it cannot be read, is not a `.uxflight`, or is from a newer
/// UXDuck.
pub fn manifest(path: &Path) -> Result<FlightFile, String> {
    let entries = entries(path).map_err(|e| e.to_string())?;
    let entry = find(&entries, MANIFEST).ok_or("it has no flight.json")?;
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
    reader(path, entry)
        .and_then(|mut r| r.read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("its flight.json is not UXDuck's: {e}"))?;
    if manifest.format != FORMAT {
        return Err(format!(
            "it is {}, and this UXDuck reads {FORMAT}. Update UXDuck to open it.",
            manifest.format
        ));
    }
    let file = path.display().to_string();
    let recordings = manifest
        .recordings
        .into_iter()
        .filter(|r| find(&entries, &r.playable).is_some())
        .map(|r| {
            (
                r.recording,
                PackedVideo {
                    path: file.clone(),
                    entry: r.entry,
                    playable: r.playable,
                },
            )
        })
        .collect();
    Ok(FlightFile {
        flight: manifest.flight,
        recordings,
        telemetry: find(&entries, TELEMETRY).is_some(),
    })
}

/// `<stem>.uxflight` in `folder`, or `<stem> 2.uxflight` and on when taken.
/// The engine numbers the names it shows the same way, so this adds a
/// number only for a file that came meanwhile.
fn free_name(folder: &Path, stem: &str) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stem}.{EXTENSION}")
        } else {
            format!("{stem} {n}.{EXTENSION}")
        };
        let path = folder.join(name);
        if !path.exists() {
            return path;
        }
        n += 1;
    }
}

/// Where a flight's file is written before it is whole: named for the
/// flight, so one left by a try that stopped (the disk unplugged or full)
/// is this flight's to remove, and never takes another file's name.
fn part_of(path: &Path, flight_id: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{flight_id}.part"));
    path.with_file_name(name)
}

/// Removes what earlier tries at this flight left in `folder`.
fn remove_stale_parts(folder: &Path, flight_id: &str) {
    let ending = format!(".{flight_id}.part");
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().ends_with(&ending) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// This flight's file already in the folder holds every file this one
/// would, and reads back whole: it is kept, rather than written again.
fn reusable(path: &Path, flight: &OffloadFlight, progress: &mut dyn FnMut(u64, u64)) -> bool {
    let Ok(entries) = entries(path) else {
        return false;
    };
    let wanted = flight
        .videos
        .iter()
        .flat_map(|v| [v.entry.as_str(), v.playable.as_str()])
        .chain(flight.telemetry.as_ref().map(|_| TELEMETRY))
        .chain([MANIFEST]);
    if !wanted
        .into_iter()
        .all(|name| find(&entries, name).is_some())
    {
        return false;
    }
    let total: u64 = entries.iter().map(|e| e.size).sum();
    verify(path, &mut |read| progress(read, total)).is_ok()
}

/// Where a file in the flight comes from.
enum From {
    Bytes(Vec<u8>),
    File(PathBuf),
    /// A file in another `.uxflight`.
    Packed(PathBuf, Entry),
    /// The flight's telemetry, written as it is read from the links' logs,
    /// with its size measured first.
    Telemetry(PathBuf, ReplayRead, u64),
    /// A camera's MPEG-TS, put into MP4 as it is written, so its copy is
    /// never made on this computer's disk, with its original's size for
    /// the progress.
    Converted(PathBuf, u64),
}

impl From {
    fn size(&self) -> std::io::Result<u64> {
        Ok(match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File(path) => std::fs::metadata(path)?.len(),
            Self::Packed(_, entry) => entry.size,
            Self::Telemetry(_, _, size) | Self::Converted(_, size) => *size,
        })
    }

    fn write(&self, out: &mut dyn Write) -> std::io::Result<()> {
        let mut from: Box<dyn Read + Send + '_> = match self {
            Self::Bytes(bytes) => Box::new(&bytes[..]),
            Self::File(path) => Box::new(BufReader::with_capacity(CHUNK, File::open(path)?)),
            Self::Packed(path, entry) => reader(path, entry)?,
            Self::Telemetry(tlogs, read, _) => {
                return crate::replay::stream(tlogs, read, out)
                    .map(|_| ())
                    .map_err(std::io::Error::other);
            }
            Self::Converted(ts, _) => {
                return crate::video::playback::convert_into(ts, out).map(|_| ());
            }
        };
        std::io::copy(&mut from, out).map(|_| ())
    }
}

/// What a flight's file needs from the computer.
pub struct Places {
    /// The links' tlogs.
    pub tlogs: PathBuf,
    /// The media folder.
    pub media: PathBuf,
    /// "UXDuck <version>".
    pub written_by: String,
    pub now_us: u64,
}

/// Writes one flight as `<stem>.uxflight` in `folder` (ADR-0062): to a
/// `.part` file, put on the disk, read back whole against each file's
/// checksum, and only then renamed into place. Says after each part how
/// many bytes it has written and read back of all it will. The file.
///
/// # Errors
/// In words: its telemetry or a video cannot be read, or the disk refuses
/// it. Nothing is left behind.
#[allow(
    clippy::too_many_lines,
    reason = "a flight's files gathered, then written and read back, in that order"
)]
pub fn write_flight(
    folder: &Path,
    flight: &OffloadFlight,
    places: &Places,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
    if let Some(existing) = flight.reuse.as_deref().map(PathBuf::from)
        && reusable(&existing, flight, progress)
    {
        return Ok(existing);
    }
    let mut files: Vec<(String, From)> = vec![(README.to_owned(), From::Bytes(README_TEXT.into()))];
    let manifest = Manifest {
        format: FORMAT.to_owned(),
        flight: flight.flight.clone(),
        recordings: flight
            .videos
            .iter()
            .map(|v| ManifestRecording {
                recording: uxduck_core::domain::video::VideoRecording {
                    packed: None,
                    ..v.recording.clone()
                },
                entry: v.entry.clone(),
                playable: v.playable.clone(),
            })
            .collect(),
        written_by: places.written_by.clone(),
        written_at_us: places.now_us,
    };
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    files.push((MANIFEST.to_owned(), From::Bytes(json)));
    if let Some(read) = &flight.telemetry {
        // Measured first, for the progress, and so a log that is not there
        // stops it before anything is written. Never held in memory.
        let size = crate::replay::stream(&places.tlogs, read, &mut std::io::sink())
            .map_err(|why| format!("Its telemetry could not be read. {why}"))?;
        files.push((
            TELEMETRY.to_owned(),
            From::Telemetry(places.tlogs.clone(), read.clone(), size),
        ));
    }
    let cache = crate::video::playback::cache_dir(&places.media);
    for video in &flight.videos {
        let name = |what: &str| format!("Its video {what} could not be read");
        match &video.from {
            VideoAt::Media { file } => {
                let original = places.media.join(file);
                files.push((video.entry.clone(), From::File(original.clone())));
                if video.playable != video.entry {
                    // A copy already made for playback is used. Otherwise
                    // the copy is made into the file as it is written, never
                    // on this computer's disk, which Move is there to free.
                    let mp4 = crate::video::playback::cached(&cache, &video.recording.id);
                    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
                    let fresh = matches!(
                        (modified(&mp4), modified(&original)),
                        (Some(copy), Some(made)) if copy >= made
                    );
                    let from = if fresh {
                        From::File(mp4)
                    } else {
                        let size = std::fs::metadata(&original)
                            .map_err(|e| format!("{}: {e}.", name(file)))?
                            .len();
                        From::Converted(original.clone(), size)
                    };
                    files.push((video.playable.clone(), from));
                }
            }
            VideoAt::Added { path } => {
                files.push((video.entry.clone(), From::File(PathBuf::from(path))));
            }
            VideoAt::Packed(packed) => {
                let from = PathBuf::from(&packed.path);
                let entries =
                    entries(&from).map_err(|e| format!("{}: {e}.", name(&packed.path)))?;
                for wanted in [&video.entry, &video.playable] {
                    if files.iter().any(|(n, _)| n == wanted) {
                        continue;
                    }
                    let at = if *wanted == video.entry {
                        &packed.entry
                    } else {
                        &packed.playable
                    };
                    let entry = find(&entries, at)
                        .ok_or_else(|| format!("{}: it has no {at}.", name(&packed.path)))?;
                    files.push((wanted.clone(), From::Packed(from.clone(), entry.clone())));
                }
            }
        }
    }
    let mut sizes = Vec::with_capacity(files.len());
    for (name, from) in &files {
        sizes.push(
            from.size()
                .map_err(|e| format!("{name} could not be read: {e}."))?,
        );
    }
    // Written, then read back.
    let total = sizes.iter().sum::<u64>().saturating_mul(2);
    remove_stale_parts(folder, &flight.flight.id);
    let path = free_name(folder, &flight.stem);
    let part = part_of(&path, &flight.flight.id);
    let written = (|| {
        let mut writer = Writer::create(&part, flight.flight.armed_at_us)?;
        let mut done = 0u64;
        let mut total = total;
        for ((name, from), size) in files.iter().zip(&sizes) {
            let at = done;
            let written = writer.add_with(name, &mut |out| from.write(out), &mut |so_far| {
                progress(at + so_far, total.max(2 * (at + so_far)));
            })?;
            // A copy made as it is written comes to its own size.
            total = total.saturating_sub(2 * size) + 2 * written;
            done += written;
        }
        writer.finish()?;
        verify(&part, &mut |read| progress(done + read, total))?;
        std::fs::rename(&part, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{} was not written: {e}.", path.display()));
    }
    Ok(path)
}

/// Writes the flights the engine asks for (ADR-0062), one job after another
/// on a thread of their own, telling it how each goes about once a second
/// and when each is done.
pub async fn work(
    mut jobs: tokio::sync::mpsc::UnboundedReceiver<OffloadJob>,
    places: std::sync::Arc<dyn Fn() -> Places + Send + Sync>,
    engine: crate::daemon::engine_task::EngineHandle,
) {
    while let Some(job) = jobs.recv().await {
        let (job_id, engine, places) = (job.id, engine.clone(), places.clone());
        let posting = engine.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let folder = PathBuf::from(&job.folder);
            for flight in &job.flights {
                let id = flight.flight.id.clone();
                let mut told = std::time::Instant::now();
                let mut progress = |done: u64, total: u64| {
                    if told.elapsed() >= std::time::Duration::from_secs(1) {
                        told = std::time::Instant::now();
                        let (job, id) = (job.id, id.clone());
                        engine.post(move |e| e.offload_progress(job, &id, done, total));
                    }
                };
                let written = write_flight(&folder, flight, &places(), &mut progress)
                    .map(|path| path.display().to_string());
                if let Err(why) = &written {
                    tracing::warn!(%why, flight = %id, "a flight was not written to its disk");
                }
                let job = job.id;
                engine.post(move |e| e.offload_written(job, &id, written));
            }
        })
        .await;
        posting.post(move |e| e.offload_done(job_id));
    }
}

/// DOS's time and date, in UTC, as zip keeps them: two-second steps, and
/// 1980 for anything before it.
fn dos_time(at_us: u64) -> (u16, u16) {
    let Ok(at) = jiff::Timestamp::from_microsecond(i64::try_from(at_us).unwrap_or(0)) else {
        return (0, (1 << 5) | 1);
    };
    let civil = at.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    if civil.year() < 1980 {
        return (0, (1 << 5) | 1);
    }
    let field = |v: i8| u16::try_from(v).unwrap_or(0);
    let time =
        (field(civil.hour()) << 11) | (field(civil.minute()) << 5) | (field(civil.second()) / 2);
    let year = u16::try_from(civil.year() - 1980).unwrap_or(0).min(127);
    let date = (year << 9) | (field(civil.month()) << 5) | field(civil.day());
    (time, date)
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn get16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn get64(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, files: &[(&str, &[u8])]) {
        let mut writer = Writer::create(path, 1_763_300_000_000_000).unwrap();
        for (name, bytes) in files {
            writer.add_bytes(name, bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    /// A flight whose log is longer than a replay holds is still written
    /// whole: a file streams its telemetry, nothing of it in memory. Until
    /// 2026-11-17 the file read it as a replay does, so a 12.6-hour session
    /// on Ben's Mac was refused (`this flight's log is over 1 GB`). Its
    /// replay keeps the refusal, now with the size measured.
    #[test]
    fn a_log_longer_than_a_replay_holds_is_written_whole_and_its_replay_refused() {
        let dir = tempfile::tempdir().unwrap();
        let tlogs = dir.path().join("tlogs");
        std::fs::create_dir_all(&tlogs).unwrap();
        let t0 = 1_763_300_000u64;
        let mut log = Vec::new();
        for i in 0..5_000u64 {
            let frame = uxduck_mavlink::sim::heartbeat_frame(1, 1, 0);
            uxduck_mavlink::tlog::record(&mut log, t0 * 1_000_000 + i * 10_000, &frame).unwrap();
        }
        std::fs::write(tlogs.join(format!("li1-{t0}.tlog")), &log).unwrap();
        let read = ReplayRead::Link {
            link_id: "li1".into(),
            from_us: t0 * 1_000_000,
            to_us: u64::MAX,
            start: None,
        };
        let flight = OffloadFlight {
            flight: serde_json::from_value(serde_json::json!({
                "id": "fl1",
                "vehicleId": "v1",
                "vehicleName": "ISR1",
                "vehicleType": "Quadrotor",
                "armedAtUs": t0 * 1_000_000,
                "end": "interrupted",
                "version": 1,
            }))
            .unwrap(),
            stem: "ISR1 2026-11-16 08.00".into(),
            telemetry: Some(read.clone()),
            videos: Vec::new(),
            reuse: None,
        };
        let places = Places {
            tlogs: tlogs.clone(),
            media: dir.path().join("media"),
            written_by: "UXDuck test".into(),
            now_us: t0 * 1_000_000,
        };
        let out = dir.path().join("disk");
        std::fs::create_dir_all(&out).unwrap();
        // A replay of a tenth of the log at most.
        let most = log.len() as u64 / 10;
        let path = crate::replay::with_most(most, || {
            write_flight(&out, &flight, &places, &mut |_, _| {})
        })
        .expect("written whole");
        assert_eq!(read_entry(&path, TELEMETRY), log);
        verify(&path, &mut |_| {}).unwrap();
        let refused = crate::replay::with_most(most, || crate::replay::read(&tlogs, &read))
            .expect_err("a replay of it is refused");
        assert_eq!(
            refused,
            "This flight's log is 1 MB. A replay is held in memory and takes 1 MB at most."
        );
        // Its file opened is refused the same way, and its log is still
        // there to write to another disk.
        let packed = ReplayRead::Packed {
            path: path.display().to_string(),
            entry: TELEMETRY.into(),
        };
        let opened = crate::replay::with_most(most, || crate::replay::read(&tlogs, &packed));
        assert_eq!(opened.unwrap_err(), refused);
        let mut again = Vec::new();
        crate::replay::stream(&tlogs, &packed, &mut again).unwrap();
        assert_eq!(again, log);
    }

    /// A camera's MPEG-TS is put into MP4 as the file is written: no copy
    /// is made in the playback cache on this computer's disk, which Move is
    /// there to free. Until 2026-11-17 each was made there first, and Copy
    /// of 186 flights took 2 GB of a nearly full SSD.
    #[test]
    fn a_recordings_mp4_copy_is_made_into_the_file_and_not_on_this_computer() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let ts = media.join("cam.ts");
        std::fs::write(&ts, crate::video::playback::tests::three_seconds()).unwrap();
        let recording: uxduck_core::domain::video::VideoRecording =
            serde_json::from_value(serde_json::json!({
                "id": "vr1",
                "sourceId": "cam",
                "vehicleId": "v1",
                "startedAtUs": 1_763_300_000_000_000u64,
                "durationUs": 3_000_000,
                "bytes": std::fs::metadata(&ts).unwrap().len(),
                "state": "done",
                "trigger": "armed",
                "fileName": "cam.ts",
                "version": 1,
            }))
            .unwrap();
        let flight = OffloadFlight {
            flight: serde_json::from_value(serde_json::json!({
                "id": "fl1",
                "vehicleId": "v1",
                "vehicleName": "Alpha",
                "vehicleType": "Quadrotor",
                "armedAtUs": 1_763_300_000_000_000u64,
                "end": "disarmed",
                "version": 1,
            }))
            .unwrap(),
            stem: "Alpha".into(),
            telemetry: None,
            videos: vec![uxduck_engine::runtime::OffloadVideo {
                recording,
                entry: "video/cam.ts".into(),
                playable: "video/cam.mp4".into(),
                from: VideoAt::Media {
                    file: "cam.ts".into(),
                },
            }],
            reuse: None,
        };
        let places = Places {
            tlogs: dir.path().join("tlogs"),
            media: media.clone(),
            written_by: "UXDuck test".into(),
            now_us: 0,
        };
        let disk = dir.path().join("disk");
        std::fs::create_dir_all(&disk).unwrap();
        let mut last = (0, 0);
        let path = write_flight(&disk, &flight, &places, &mut |done, total| {
            last = (done, total);
        })
        .unwrap();
        let cache = crate::video::playback::cache_dir(&media);
        assert!(
            std::fs::read_dir(&cache).map_or(true, |mut d| d.next().is_none()),
            "a copy was made in {}",
            cache.display()
        );
        let mut made = Vec::new();
        crate::video::playback::convert_into(&ts, &mut made).unwrap();
        assert_eq!(read_entry(&path, "video/cam.mp4"), made);
        assert_eq!(
            read_entry(&path, "video/cam.ts"),
            std::fs::read(&ts).unwrap()
        );
        // The progress ends at its total, the copy measured as written.
        assert_eq!(last.0, last.1);
        verify(&path, &mut |_| {}).unwrap();
    }

    /// A Move after a Copy to the same folder keeps the file there, read
    /// back first, rather than writing `Alpha 2.uxflight`. What a try that
    /// stopped left (its disk unplugged or full) is removed on the next,
    /// and never takes the file's name. Until 2026-11-17 a second try wrote
    /// `Alpha 2` beside the leftover `Alpha.uxflight.part`.
    #[test]
    fn a_file_there_already_is_kept_and_a_stopped_try_leaves_nothing_in_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let flight = |reuse: Option<String>| OffloadFlight {
            flight: serde_json::from_value(serde_json::json!({
                "id": "fl1",
                "vehicleId": "v1",
                "vehicleName": "Alpha",
                "vehicleType": "Quadrotor",
                "armedAtUs": 1_763_300_000_000_000u64,
                "end": "disarmed",
                "version": 1,
            }))
            .unwrap(),
            stem: "Alpha".into(),
            telemetry: None,
            videos: Vec::new(),
            reuse,
        };
        let places = Places {
            tlogs: dir.path().join("tlogs"),
            media: dir.path().join("media"),
            written_by: "UXDuck test".into(),
            now_us: 0,
        };
        let disk = dir.path().join("disk");
        std::fs::create_dir_all(&disk).unwrap();
        let stale = part_of(&disk.join("Alpha.uxflight"), "fl1");
        std::fs::write(&stale, b"half").unwrap();
        let path = write_flight(&disk, &flight(None), &places, &mut |_, _| {}).unwrap();
        assert_eq!(path, disk.join("Alpha.uxflight"));
        assert!(!stale.exists());
        let kept = write_flight(
            &disk,
            &flight(Some(path.display().to_string())),
            &places,
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(kept, path);
        assert_eq!(std::fs::read_dir(&disk).unwrap().count(), 1);
        // One that does not read back is written again beside it.
        std::fs::write(&path, b"not a zip").unwrap();
        let written = write_flight(
            &disk,
            &flight(Some(path.display().to_string())),
            &places,
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(written, disk.join("Alpha 2.uxflight"));
    }

    fn read_entry(path: &Path, name: &str) -> Vec<u8> {
        read(path, name).unwrap()
    }

    #[test]
    fn files_written_are_read_back_whole_at_the_place_the_zip_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.uxflight");
        let video: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        write(
            &path,
            &[
                (MANIFEST, b"{}"),
                (TELEMETRY, b"tlog"),
                ("video/cam.mp4", &video),
            ],
        );
        let entries = entries(&path).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [MANIFEST, TELEMETRY, "video/cam.mp4"]
        );
        assert_eq!(read(&path, TELEMETRY).unwrap(), b"tlog");
        let cam = find(&entries, "video/cam.mp4").unwrap();
        assert!(cam.whole());
        let bytes = std::fs::read(&path).unwrap();
        let at = usize::try_from(cam.data_offset).unwrap();
        assert_eq!(&bytes[at..at + video.len()], &video[..]);
        verify(&path, &mut |_| {}).unwrap();
    }

    #[test]
    fn a_file_changed_after_it_was_written_does_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.uxflight");
        write(&path, &[(TELEMETRY, b"0123456789")]);
        let at = usize::try_from(entries(&path).unwrap()[0].data_offset).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[at + 3] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();
        let why = verify(&path, &mut |_| {}).unwrap_err();
        assert!(why.to_string().contains("did not read back"), "{why}");
    }

    #[test]
    fn a_file_that_is_not_a_zip_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.uxflight");
        std::fs::write(&path, b"not a zip at all").unwrap();
        let why = entries(&path).unwrap_err();
        assert!(why.to_string().starts_with("not a .uxflight file"), "{why}");
    }

    #[test]
    #[ignore = "writes 4 GB to the disk"]
    fn a_file_over_4_gb_is_named_by_zip64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.uxflight");
        let mut writer = Writer::create(&path, 0).unwrap();
        writer.add_bytes(MANIFEST, b"{}").unwrap();
        // 4 GiB and a little more, read as zeros without holding them.
        let size = (1u64 << 32) + 10;
        writer
            .add_with(
                "video/big.ts",
                &mut |out| std::io::copy(&mut std::io::repeat(0).take(size), out).map(|_| ()),
                &mut |_| {},
            )
            .unwrap();
        writer.add_bytes(TELEMETRY, b"after").unwrap();
        writer.finish().unwrap();
        let entries = entries(&path).unwrap();
        let big = find(&entries, "video/big.ts").unwrap();
        assert_eq!(big.size, size);
        let after = find(&entries, TELEMETRY).unwrap();
        assert!(after.data_offset > size);
        assert_eq!(read(&path, TELEMETRY).unwrap(), b"after");
    }

    #[test]
    fn a_deflated_file_from_another_program_is_read_too() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.uxflight");
        write(&path, &[(TELEMETRY, b"placeholder")]);
        let mut deflated =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        deflated.write_all(b"hello hello hello").unwrap();
        let deflated = deflated.finish().unwrap();
        let entry = Entry {
            method: DEFLATED,
            stored_size: deflated.len() as u64,
            size: 17,
            ..entries(&path).unwrap()[0].clone()
        };
        let mut bytes = std::fs::read(&path).unwrap();
        let at = usize::try_from(entry.data_offset).unwrap();
        bytes.splice(at..at + deflated.len(), deflated.iter().copied());
        std::fs::write(&path, bytes).unwrap();
        let mut out = String::new();
        reader(&path, &entry)
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "hello hello hello");
    }

    #[test]
    fn its_dates_are_the_flights_in_dos_time() {
        // 2025-11-16 13:33:20 UTC.
        let (time, date) = dos_time(1_763_300_000_000_000);
        assert_eq!(time, (13 << 11) | (33 << 5) | 0b1010);
        assert_eq!(date, (45 << 9) | (11 << 5) | 16);
    }
}

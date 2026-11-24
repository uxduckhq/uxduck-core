//! The vehicle's SD card plugged into this computer (ADR-0039 R15, S212):
//! noticed by its logs, ArduPilot's `.BIN` in `APM/LOGS` or PX4's `.ulg` in
//! `log/<date>/`, and those copied into this computer's logs
//! (`<data>/logs-from-cards/<card>-<date>/`), where Flights lists them to
//! replay. Nothing is fetched from a vehicle: people pull the
//! card, since nothing reads a log over MAVLink quickly (maintainer,
//! 2026-10-28). Each copy is read once for the vehicle and the time it
//! covers (S426), so the Logbook says which flight it belongs to.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use uxduck_contracts::dto::replay::{
    CopiedLog, FailedLog, ImportedLog, InspectedLog, LogBoard, LogCard, LogCardsResult, LogSpan,
    LogsImportResult,
};
use uxduck_engine::runtime::LogSource;

/// Where the copies go.
#[must_use]
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs-from-cards")
}

/// The places a card or a disk is mounted on this platform, each volume.
#[allow(clippy::vec_init_then_push, reason = "each platform adds its own")]
pub(crate) fn mount_roots() -> Vec<PathBuf> {
    #[allow(unused_mut, reason = "each platform adds its own")]
    let mut roots: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    roots.push(PathBuf::from("/Volumes"));
    #[cfg(target_os = "linux")]
    {
        if let Ok(user) = std::env::var("USER") {
            roots.push(PathBuf::from("/media").join(&user));
            roots.push(PathBuf::from("/run/media").join(&user));
        }
        roots.push(PathBuf::from("/media"));
        roots.push(PathBuf::from("/mnt"));
    }
    #[allow(unused_mut, reason = "Windows adds its drives")]
    let mut volumes: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| std::fs::read_dir(root).into_iter().flatten().flatten())
        .map(|e| e.path())
        .collect();
    #[cfg(windows)]
    volumes.extend((b'D'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))));
    volumes
}

/// A folder in `dir`, whatever its letters' case.
fn folder(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// The files in a folder with one of these extensions, whatever their case,
/// leaving out hidden ones such as the `._` files macOS writes beside each
/// file it copies to a card.
fn files_in(folder: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
                && p.extension()
                    .is_some_and(|e| extensions.iter().any(|x| e.eq_ignore_ascii_case(x)))
        })
        .collect();
    files.sort();
    files
}

/// A card's logs, each with the name its copy takes: ArduPilot's in
/// `APM/LOGS`, and PX4's in `log` and the folders in it, by day
/// (`2026-11-13/18_31_07.ulg` as `2026-11-13_18_31_07.ulg`) or by session
/// (`sess001/log001.ulg`) when it had no GPS time.
fn card_logs(volume: &Path) -> Vec<(PathBuf, String)> {
    let named = |files: Vec<PathBuf>, prefix: &str| {
        files
            .into_iter()
            .filter_map(|file| {
                let name = file.file_name()?.to_string_lossy().into_owned();
                Some((file, format!("{prefix}{name}")))
            })
            .collect::<Vec<_>>()
    };
    let mut logs = Vec::new();
    if let Some(apm) = folder(volume, "APM").and_then(|apm| folder(&apm, "LOGS")) {
        logs.extend(named(files_in(&apm, &["bin"]), ""));
    }
    if let Some(px4) = folder(volume, "log") {
        logs.extend(named(files_in(&px4, &["ulg"]), ""));
        let mut days: Vec<PathBuf> = std::fs::read_dir(&px4)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        days.sort();
        for day in days {
            let prefix = day
                .file_name()
                .map(|n| format!("{}_", n.to_string_lossy()))
                .unwrap_or_default();
            logs.extend(named(files_in(&day, &["ulg"]), &prefix));
        }
    }
    logs
}

/// Cards plugged in with logs on them, from `volumes` (the mount points).
#[must_use]
pub fn cards_in(volumes: &[PathBuf]) -> Vec<LogCard> {
    volumes
        .iter()
        .filter_map(|volume| {
            let files: Vec<PathBuf> = card_logs(volume).into_iter().map(|(f, _)| f).collect();
            (!files.is_empty()).then(|| LogCard {
                path: volume.display().to_string(),
                name: volume.file_name().map_or_else(
                    || volume.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                ),
                logs: u32::try_from(files.len()).unwrap_or(u32::MAX),
                bytes: files
                    .iter()
                    .filter_map(|f| std::fs::metadata(f).ok())
                    .map(|m| m.len())
                    .sum(),
            })
        })
        .collect()
}

/// The logs copied off cards before, newest first.
#[must_use]
pub fn copied(data_dir: &Path) -> Vec<CopiedLog> {
    let mut logs: Vec<(std::time::SystemTime, CopiedLog)> = std::fs::read_dir(dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|copy| {
            let card = copy.file_name().to_string_lossy().into_owned();
            files_in(&copy.path(), &["bin", "ulg"])
                .into_iter()
                .map(move |file| (card.clone(), file))
        })
        .filter_map(|(card, file)| {
            let meta = std::fs::metadata(&file).ok()?;
            Some((
                meta.modified().ok()?,
                CopiedLog {
                    path: file.display().to_string(),
                    name: file.file_name()?.to_string_lossy().into_owned(),
                    card,
                    bytes: meta.len(),
                    ..CopiedLog::default()
                },
            ))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
    logs.into_iter().map(|(_, log)| log).collect()
}

/// The card being copied, how many done of how many, and why it stopped.
type Progress = (String, u32, u32, Option<String>);

/// Copies a card's logs on a thread of its own; how it goes is in the
/// shared state that `logs/cards` reads.
#[derive(Clone, Debug, Default)]
pub struct Copying(Arc<Mutex<Option<Progress>>>);

impl Copying {
    #[must_use]
    pub fn now(&self) -> Option<Progress> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts copying the logs of the card at `card` (a mount point).
    ///
    /// # Errors
    /// When it is not a card with logs, or one is being copied.
    pub fn start(&self, data_dir: &Path, card: &str, today: &str) -> Result<(), String> {
        let volume = PathBuf::from(card);
        let files = card_logs(&volume);
        if files.is_empty() {
            return Err("This card has no ArduPilot or PX4 logs.".into());
        }
        {
            let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if now
                .as_ref()
                .is_some_and(|(_, done, of, why)| done < of && why.is_none())
            {
                return Err("A card is being copied already.".into());
            }
            *now = Some((
                card.to_owned(),
                0,
                u32::try_from(files.len()).unwrap_or(u32::MAX),
                None,
            ));
        }
        let name = volume
            .file_name()
            .map_or_else(|| "card".to_owned(), |n| n.to_string_lossy().into_owned());
        let into = dir(data_dir).join(format!("{name}-{today}"));
        let state = self.0.clone();
        std::thread::spawn(move || {
            let copy = || -> Result<(), String> {
                std::fs::create_dir_all(&into).map_err(|e| e.to_string())?;
                for (done, (file, name)) in files.iter().enumerate() {
                    let target = into.join(name);
                    let same = std::fs::metadata(&target)
                        .ok()
                        .zip(std::fs::metadata(file).ok())
                        .is_some_and(|(a, b)| a.len() == b.len());
                    if !same {
                        let part = target.with_extension("part");
                        std::fs::copy(file, &part)
                            .and_then(|_| std::fs::rename(&part, &target))
                            .map_err(|e| format!("{}: {e}", file.display()))?;
                    }
                    if let Some(now) = state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_mut()
                    {
                        now.1 = u32::try_from(done + 1).unwrap_or(u32::MAX);
                    }
                }
                Ok(())
            };
            if let Err(why) = copy()
                && let Some(now) = state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
            {
                now.3 = Some(why);
            }
        });
        Ok(())
    }
}

/// What a read keeps now: 1 since the armings (S459), 2 since the flight
/// controller's id. A log read by a UXDuck from before is read again once,
/// keeping its span meanwhile.
const READ_VERSION: u32 = 2;

/// What a copied log covers, as read once, with the size it had then.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Read {
    bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    span: Option<LogSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unread: Option<String>,
    #[serde(default)]
    version: u32,
    /// As its heartbeats say it (S459).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vehicle_type: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    simulated: bool,
    /// Each arming, with what the Logbook shows of it (S459).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    armings: Vec<uxduck_mavlink::autopilot_log::Arming>,
}

#[derive(Debug, Default)]
struct SpansNow {
    loaded: bool,
    reading: bool,
    /// By path.
    read: BTreeMap<String, Read>,
}

/// What each copied log covers (S426), read once on a thread of its own,
/// since a log can be hundreds of megabytes, and kept in
/// `<data>/logs-from-cards/spans.json` so it is not read again.
#[derive(Clone, Debug, Default)]
pub struct Spans(Arc<Mutex<SpansNow>>);

fn spans_file(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("spans.json")
}

impl Read {
    fn from(span: uxduck_mavlink::autopilot_log::Span, bytes: u64) -> Self {
        Self {
            bytes,
            span: Some(LogSpan {
                system_id: span.system_id,
                firmware: span.firmware,
                gps_time: span.gps_time,
                from_us: span.from_us,
                to_us: span.to_us,
                board: span.board.map(|board| LogBoard {
                    name: board.name,
                    uid: board.uid,
                }),
            }),
            unread: None,
            version: READ_VERSION,
            vehicle_type: span.vehicle_type,
            simulated: span.simulated,
            armings: span.armings,
        }
    }

    fn unread(why: String, bytes: u64) -> Self {
        Self {
            bytes,
            span: None,
            unread: Some(why),
            version: READ_VERSION,
            vehicle_type: None,
            simulated: false,
            armings: Vec::new(),
        }
    }
}

fn read_span(path: &str, bytes: u64) -> Read {
    let read = std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|log| uxduck_mavlink::autopilot_log::span(&log));
    match read {
        Ok(span) => Read::from(span, bytes),
        Err(why) => Read::unread(why, bytes),
    }
}

/// What was kept of the logs read before, loaded once.
fn load(now: &mut SpansNow, data_dir: &Path) {
    if !now.loaded {
        now.loaded = true;
        now.read = std::fs::read(spans_file(data_dir))
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_default();
    }
}

fn save(now: &SpansNow, file: &Path) {
    let saved = serde_json::to_vec(&now.read)
        .map_err(|e| e.to_string())
        .and_then(|json| std::fs::write(file, json).map_err(|e| e.to_string()));
    if let Err(error) = saved {
        tracing::warn!(%error, "logs from cards: what each covers could not be kept");
    }
}

impl Spans {
    /// What a copy covers: as kept when it has not changed, else read now
    /// and kept (S457). Reads the whole log, so off the engine's task.
    fn read(&self, data_dir: &Path, path: &str, bytes: u64) -> Read {
        {
            let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            load(&mut now, data_dir);
            if let Some(read) = now
                .read
                .get(path)
                .filter(|read| read.bytes == bytes && read.version >= READ_VERSION)
            {
                return read.clone();
            }
        }
        let read = read_span(path, bytes);
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        now.read.insert(path.to_owned(), read.clone());
        save(&now, &spans_file(data_dir));
        read
    }

    /// Each log's span or why it could not be read, from what is known. The
    /// logs not read yet, or changed since, are read on a thread, and have
    /// neither until it is done.
    pub fn fill(&self, data_dir: &Path, logs: &mut [CopiedLog]) {
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        load(&mut now, data_dir);
        let mut unread = Vec::new();
        for log in logs.iter_mut() {
            match now
                .read
                .get(&log.path)
                .filter(|read| read.bytes == log.bytes)
            {
                Some(read) => {
                    log.span.clone_from(&read.span);
                    log.unread.clone_from(&read.unread);
                    if read.version < READ_VERSION {
                        unread.push((log.path.clone(), log.bytes));
                    }
                }
                None => unread.push((log.path.clone(), log.bytes)),
            }
        }
        if unread.is_empty() || now.reading {
            return;
        }
        now.reading = true;
        let state = self.0.clone();
        let file = spans_file(data_dir);
        std::thread::spawn(move || {
            for (path, bytes) in unread {
                let read = read_span(&path, bytes);
                let mut now = state.lock().unwrap_or_else(PoisonError::into_inner);
                now.read.insert(path, read);
                save(&now, &file);
            }
            state.lock().unwrap_or_else(PoisonError::into_inner).reading = false;
        });
    }
}

impl Spans {
    /// The logs of `logs` read as this UXDuck reads them, each with the
    /// armings it records (S459). One not read yet, or read by a UXDuck
    /// from before, is left out until it is.
    #[must_use]
    pub fn reads(&self, logs: &[CopiedLog]) -> Vec<uxduck_engine::runtime::AutopilotLogRead> {
        let now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        logs.iter()
            .filter_map(|log| {
                let read = now
                    .read
                    .get(&log.path)
                    .filter(|read| read.bytes == log.bytes && read.version >= READ_VERSION)?;
                Some(uxduck_engine::runtime::AutopilotLogRead {
                    path: log.path.clone(),
                    span: read.span.clone()?,
                    vehicle_type: read.vehicle_type.clone(),
                    simulated: read.simulated,
                    armings: read.armings.clone(),
                })
            })
            .collect()
    }
}

/// The logs copied off cards, newest first, with what each covers once
/// read (S426).
#[must_use]
pub fn copied_with_spans(data_dir: &Path, spans: &Spans) -> Vec<CopiedLog> {
    let mut logs = copied(data_dir);
    spans.fill(data_dir, &mut logs);
    logs
}

/// What `logs/cards` answers.
#[must_use]
pub fn status(data_dir: &Path, copying: &Copying, spans: &Spans) -> LogCardsResult {
    let now = copying.now();
    LogCardsResult {
        cards: cards_in(&mount_roots()),
        copied: copied_with_spans(data_dir, spans),
        copying: now.as_ref().map(|(card, _, _, _)| card.clone()),
        done: now.as_ref().map_or(0, |n| n.1),
        of: now.as_ref().map_or(0, |n| n.2),
        problem: now.and_then(|n| n.3),
    }
}

/// A read log as `logs/inspect` and `logs/import` give it (S457).
fn inspected(path: Option<String>, name: String, read: Read) -> InspectedLog {
    InspectedLog {
        path,
        name,
        bytes: read.bytes,
        span: read.span,
        unread: read.unread,
        ..InspectedLog::default()
    }
}

/// Whether a file's name is an ArduPilot `.bin` or a PX4 `.ulg`.
fn log_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("ulg"))
}

/// Whether a log's first bytes are a DataFlash or a ULog header.
fn log_head(head: &[u8]) -> bool {
    uxduck_mavlink::dataflash::is_dataflash(head) || uxduck_mavlink::ulog::is_ulog(head)
}

/// A log given by its path, its bytes or an upload, with its name: only
/// the file name of one given by bytes or uploaded, so nothing is written
/// outside the logs.
enum Given {
    Path(PathBuf),
    Bytes(Vec<u8>),
    /// Kept where the upload is, read as a path but no original.
    Upload(PathBuf),
}

impl Given {
    /// The file it is on this computer, when it is one.
    fn file(&self) -> Option<&Path> {
        match self {
            Self::Path(path) | Self::Upload(path) => Some(path),
            Self::Bytes(_) => None,
        }
    }
}

/// Only the file name of a name given, so nothing is written outside the
/// logs.
fn file_name_of(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("log.bin")
        .to_owned()
}

fn given(source: &LogSource) -> Result<(String, Given), (String, String)> {
    match source {
        LogSource::Path(path) => {
            let path = PathBuf::from(path);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| (path.display().to_string(), "This is not a file.".to_owned()))?;
            Ok((name, Given::Path(path)))
        }
        LogSource::Uploaded { name, path } => Ok((file_name_of(name), Given::Upload(path.into()))),
        LogSource::Bytes { name, data } => {
            let name = file_name_of(name);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| {
                    (
                        name.clone(),
                        "It did not arrive whole. Try again.".to_owned(),
                    )
                })?;
            Ok((name, Given::Bytes(bytes)))
        }
    }
}

/// Whether a file holds exactly these bytes, read a piece at a time.
fn same_bytes(file: &Path, other: &mut dyn std::io::Read) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(file) else {
        return false;
    };
    let (mut a, mut b) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let Ok(n) = file.read(&mut a) else {
            return false;
        };
        if n == 0 {
            return other.read(&mut b[..1]).is_ok_and(|m| m == 0);
        }
        if other.read_exact(&mut b[..n]).is_err() || a[..n] != b[..n] {
            return false;
        }
    }
}

/// The name a copy takes in `into`: its own, else `00000042 (2).BIN` and
/// on, the first not taken.
fn free_name(into: &Path, name: &str) -> PathBuf {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let extension = path
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let mut candidate = into.join(name);
    let mut n = 2u64;
    while candidate.exists() {
        candidate = into.join(format!("{stem} ({n}){extension}"));
        n += 1;
    }
    candidate
}

/// Writes a copy to `<target>.part` with `write`, checks it is `bytes` long,
/// then names it `target`. On any failure the part is removed, so a log
/// is in the logs whole or not at all.
fn copy_checked(
    write: impl FnOnce(&Path) -> std::io::Result<()>,
    target: &Path,
    bytes: u64,
) -> Result<(), String> {
    let part = PathBuf::from(format!("{}.part", target.display()));
    let copied = write(&part).map_err(|e| e.to_string()).and_then(|()| {
        let made = std::fs::metadata(&part).map_err(|e| e.to_string())?.len();
        if made != bytes {
            return Err(format!(
                "The copy came out {made} bytes, not {bytes}, so it was not kept."
            ));
        }
        std::fs::rename(&part, target).map_err(|e| e.to_string())
    });
    if copied.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    copied?;
    let kept = std::fs::metadata(target).map_err(|e| e.to_string())?.len();
    if kept == bytes {
        Ok(())
    } else {
        Err(format!("The copy is {kept} bytes, not {bytes}."))
    }
}

/// A given log's first bytes, enough to tell a log, and its size.
fn head_and_bytes(given: &Given) -> std::io::Result<(Vec<u8>, u64)> {
    match given {
        Given::Bytes(data) => Ok((data.get(..16).unwrap_or(data).to_vec(), data.len() as u64)),
        Given::Path(path) | Given::Upload(path) => {
            use std::io::Read as _;
            let bytes = std::fs::metadata(path)?.len();
            let mut head = Vec::new();
            std::fs::File::open(path)?.take(16).read_to_end(&mut head)?;
            Ok((head, bytes))
        }
    }
}

/// The copied log that is this one already: the same size and bytes,
/// whatever it was named.
fn already_copied(data_dir: &Path, given: &Given, bytes: u64) -> Option<CopiedLog> {
    copied(data_dir)
        .into_iter()
        .filter(|copy| copy.bytes == bytes)
        .find(|copy| match given {
            Given::Bytes(data) => same_bytes(Path::new(&copy.path), &mut data.as_slice()),
            Given::Path(path) | Given::Upload(path) => std::fs::File::open(path)
                .is_ok_and(|mut file| same_bytes(Path::new(&copy.path), &mut file)),
        })
}

/// What was given, as a failed or imported log says it: its path, else its
/// name.
fn given_as(source: &LogSource) -> String {
    match source {
        LogSource::Path(path) => path.clone(),
        LogSource::Bytes { name, .. } | LogSource::Uploaded { name, .. } => name.clone(),
    }
}

/// Imports logs into `<data>/logs-from-cards/Imported-<today>/` (S457), where
/// S426 reads every copied log. A log already among the copies, the same
/// size and bytes whatever it was named, is not copied again. Another of
/// the same name is kept beside it under a new name.
#[must_use]
pub fn import(
    data_dir: &Path,
    today: &str,
    sources: &[LogSource],
    spans: &Spans,
) -> LogsImportResult {
    let mut result = LogsImportResult::default();
    for source in sources {
        let from = given_as(source);
        let fail = |name: String, why: String| FailedLog {
            from: from.clone(),
            name,
            why,
        };
        let (name, given) = match given(source) {
            Ok(given) => given,
            Err((name, why)) => {
                result.failed.push(fail(name, why));
                continue;
            }
        };
        if !log_name(&name) {
            result.failed.push(fail(
                name,
                "Only ArduPilot .bin and PX4 .ulg logs are imported.".into(),
            ));
            continue;
        }
        let (head, bytes) = match head_and_bytes(&given) {
            Ok(read) => read,
            Err(e) => {
                result
                    .failed
                    .push(fail(name, format!("UXDuck couldn't read it: {e}")));
                continue;
            }
        };
        if !log_head(&head) {
            result.failed.push(fail(
                name,
                "This is not an ArduPilot .bin or a PX4 .ulg log.".into(),
            ));
            continue;
        }
        let original = match &given {
            Given::Path(path) => Some(path.clone()),
            Given::Bytes(_) | Given::Upload(_) => None,
        };
        if let Some(copy) = already_copied(data_dir, &given, bytes) {
            // The copy itself, imported again, is no original to move.
            let itself = original.as_ref().is_some_and(|path| {
                std::fs::canonicalize(path).ok() == std::fs::canonicalize(&copy.path).ok()
            });
            result.skipped.push(ImportedLog {
                from: from.clone(),
                original: original
                    .filter(|_| !itself)
                    .map(|p| p.display().to_string()),
                log: inspected(
                    Some(copy.path.clone()),
                    copy.name.clone(),
                    spans.read(data_dir, &copy.path, copy.bytes),
                ),
            });
            continue;
        }
        let into = dir(data_dir).join(format!("Imported-{today}"));
        if let Err(e) = std::fs::create_dir_all(&into) {
            result
                .failed
                .push(fail(name, format!("UXDuck couldn't make its folder: {e}")));
            continue;
        }
        let target = free_name(&into, &name);
        let copied = copy_checked(
            |part| match &given {
                Given::Path(path) | Given::Upload(path) => std::fs::copy(path, part).map(|_| ()),
                Given::Bytes(data) => std::fs::write(part, data),
            },
            &target,
            bytes,
        );
        if let Err(why) = copied {
            let why = match crate::attachments::short_of_space(&into, bytes) {
                Some(short) => format!("{why} {short}"),
                None => why,
            };
            result.failed.push(fail(name, why));
            continue;
        }
        let path = target.display().to_string();
        let kept_name = target
            .file_name()
            .map_or(name, |n| n.to_string_lossy().into_owned());
        result.imported.push(ImportedLog {
            from: from.clone(),
            original: original.map(|p| p.display().to_string()),
            log: inspected(
                Some(path.clone()),
                kept_name,
                spans.read(data_dir, &path, bytes),
            ),
        });
    }
    result
}

/// Autopilot logs from anywhere on this computer (S457): read without
/// copying for `logs/inspect`, and imported for `logs/import`.
#[derive(Clone, Debug)]
pub struct Logs {
    data_dir: PathBuf,
    spans: Spans,
}

impl Logs {
    #[must_use]
    pub fn new(data_dir: &Path, spans: &Spans) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            spans: spans.clone(),
        }
    }
}

impl uxduck_engine::runtime::AutopilotLogs for Logs {
    fn inspect(&self, source: &LogSource) -> InspectedLog {
        match given(source) {
            Err((name, why)) => InspectedLog {
                name,
                unread: Some(why),
                ..InspectedLog::default()
            },
            Ok((name, Given::Bytes(data))) => {
                let read = match uxduck_mavlink::autopilot_log::span(&data) {
                    Ok(span) => Read::from(span, data.len() as u64),
                    Err(why) => Read::unread(why, data.len() as u64),
                };
                inspected(None, name, read)
            }
            Ok((name, given)) => {
                let path = given
                    .file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let read = match std::fs::metadata(&path) {
                    Ok(meta) => read_span(&path, meta.len()),
                    Err(e) => Read::unread(format!("UXDuck couldn't read it: {e}"), 0),
                };
                // Where an upload is kept is UXDuck's own business.
                let shown = matches!(given, Given::Path(_)).then_some(path);
                inspected(shown, name, read)
            }
        }
    }

    fn import(&self, sources: &[LogSource]) -> LogsImportResult {
        use uxduck_engine::runtime::Clock as _;
        let today = crate::analytics::utc_day(crate::adapters::system::SystemClock.now_us());
        import(&self.data_dir, &today, sources, &self.spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_is_known_by_its_logs_folder_and_its_logs_are_copied() {
        let volumes = tempfile::tempdir().unwrap();
        let card = volumes.path().join("NO NAME");
        std::fs::create_dir_all(card.join("APM").join("LOGS")).unwrap();
        std::fs::write(card.join("APM/LOGS/00000001.BIN"), b"one").unwrap();
        std::fs::write(card.join("APM/LOGS/00000002.bin"), b"two!").unwrap();
        std::fs::write(card.join("APM/LOGS/LASTLOG.TXT"), b"2").unwrap();
        // A stick with no logs folder is no card.
        std::fs::create_dir_all(volumes.path().join("PHOTOS/DCIM")).unwrap();
        let cards = cards_in(&[card.clone(), volumes.path().join("PHOTOS")]);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            (cards[0].name.as_str(), cards[0].logs, cards[0].bytes),
            ("NO NAME", 2, 7)
        );

        let data = tempfile::tempdir().unwrap();
        let copying = Copying::default();
        copying
            .start(data.path(), &card.display().to_string(), "2026-10-28")
            .unwrap();
        for _ in 0..200 {
            if copying.now().is_some_and(|n| n.1 == 2) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let logs = copied(data.path());
        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|l| l.card == "NO NAME-2026-10-28"));
    }

    /// S426: each copy is read once, on a thread, for its vehicle and its
    /// time on GPS time, and what was read is kept for the next start.
    #[test]
    fn each_copy_is_read_once_for_what_it_covers_and_kept() {
        let data = tempfile::tempdir().unwrap();
        let into = dir(data.path()).join("NO NAME-2026-11-20");
        std::fs::create_dir_all(&into).unwrap();
        std::fs::write(
            into.join("00000001.BIN"),
            uxduck_mavlink::dataflash::fixture::flight(),
        )
        .unwrap();
        std::fs::write(into.join("00000002.BIN"), b"not a log").unwrap();
        let spans = Spans::default();
        let first = copied_with_spans(data.path(), &spans);
        assert!(first.iter().all(|l| l.span.is_none() && l.unread.is_none()));
        let mut logs = first;
        for _ in 0..500 {
            logs = copied_with_spans(data.path(), &spans);
            if logs.iter().all(|l| l.span.is_some() || l.unread.is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let log = |name: &str| logs.iter().find(|l| l.name == name).unwrap().clone();
        let span = log("00000001.BIN").span.unwrap();
        assert!(span.gps_time && span.from_us < span.to_us, "{span:?}");
        assert_eq!(
            log("00000002.BIN").unread.as_deref(),
            Some("This is not an ArduPilot .bin or a PX4 .ulg log.")
        );
        // Kept: a new start knows them at once, without reading them.
        let again = copied_with_spans(data.path(), &Spans::default());
        assert_eq!(again, logs);
    }

    /// S457: a log from anywhere is copied into Imported-<day>, where the
    /// cards' copies are; the same log again is not copied twice, even
    /// given by its bytes; another of the same name is kept beside it; and
    /// a file that is no log is refused with why.
    #[test]
    fn a_log_from_anywhere_is_imported_once_and_kept_beside_another_of_its_name() {
        let data = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let log = uxduck_mavlink::dataflash::fixture::flight();
        let original = downloads.path().join("00000046.BIN");
        std::fs::write(&original, &log).unwrap();
        let spans = Spans::default();
        let by_path = |path: &Path| LogSource::Path(path.display().to_string());

        let first = import(data.path(), "2026-11-22", &[by_path(&original)], &spans);
        assert!(
            first.failed.is_empty() && first.skipped.is_empty(),
            "{first:?}"
        );
        let copy = &first.imported[0];
        assert_eq!(copy.original.as_deref(), Some(original.to_str().unwrap()));
        let copied_to = PathBuf::from(copy.log.path.clone().unwrap());
        assert_eq!(
            copied_to,
            dir(data.path())
                .join("Imported-2026-11-22")
                .join("00000046.BIN")
        );
        assert_eq!(std::fs::read(&copied_to).unwrap(), log);
        assert_eq!(copy.log.bytes, log.len() as u64);
        assert!(
            copy.log.span.as_ref().is_some_and(|s| s.gps_time),
            "{copy:?}"
        );
        // The original stays where it was: moving it is the page's.
        assert!(original.exists());
        // S426 lists it with the cards' copies, already read.
        let listed = copied_with_spans(data.path(), &spans);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].card, "Imported-2026-11-22");
        assert!(listed[0].span.is_some());

        // Again, the next day and by its bytes: not copied twice.
        let again = import(
            data.path(),
            "2026-11-23",
            &[
                by_path(&original),
                LogSource::Bytes {
                    name: "whatsapp/00000046.BIN".into(),
                    data: base64::engine::general_purpose::STANDARD.encode(&log),
                },
            ],
            &spans,
        );
        assert!(again.imported.is_empty(), "{again:?}");
        assert_eq!(again.skipped.len(), 2);
        assert!(again.skipped.iter().all(|s| s.log.path == copy.log.path));
        assert_eq!(again.skipped[0].original, copy.original);
        assert_eq!(again.skipped[1].original, None);
        // The copy itself is no original to move to the Trash.
        let itself = import(data.path(), "2026-11-22", &[by_path(&copied_to)], &spans);
        assert_eq!(itself.skipped.len(), 1);
        assert_eq!(itself.skipped[0].original, None);

        // Another log of the same name, given by its bytes, is kept beside it.
        let mut other = log.clone();
        other.extend_from_slice(&log[..64]);
        let beside = import(
            data.path(),
            "2026-11-22",
            &[LogSource::Bytes {
                name: "00000046.BIN".into(),
                data: base64::engine::general_purpose::STANDARD.encode(&other),
            }],
            &spans,
        );
        assert_eq!(beside.imported.len(), 1, "{beside:?}");
        assert_eq!(beside.imported[0].log.name, "00000046 (2).BIN");
        assert_eq!(beside.imported[0].original, None);
        assert_eq!(copied(data.path()).len(), 2);

        // Not a log, not a log's name, and a file that is not there.
        let notes = downloads.path().join("notes.bin");
        std::fs::write(&notes, b"shopping list").unwrap();
        let photo = downloads.path().join("photo.jpg");
        std::fs::write(&photo, &log).unwrap();
        let refused = import(
            data.path(),
            "2026-11-22",
            &[
                by_path(&notes),
                by_path(&photo),
                by_path(&downloads.path().join("gone.BIN")),
            ],
            &spans,
        );
        assert!(refused.imported.is_empty() && refused.skipped.is_empty());
        let why: Vec<&str> = refused.failed.iter().map(|f| f.why.as_str()).collect();
        assert_eq!(why[0], "This is not an ArduPilot .bin or a PX4 .ulg log.");
        assert_eq!(
            why[1],
            "Only ArduPilot .bin and PX4 .ulg logs are imported."
        );
        assert!(why[2].starts_with("UXDuck couldn't read it"), "{why:?}");
        assert_eq!(copied(data.path()).len(), 2);
    }

    /// S457: a copy that fails on the way, or comes out the wrong size,
    /// leaves no part behind and no log under its name.
    #[test]
    fn a_copy_that_fails_leaves_no_part_behind() {
        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("00000046.BIN");
        let part = into.path().join("00000046.BIN.part");
        let half = copy_checked(
            |part| {
                std::fs::write(part, b"half a log")?;
                Err(std::io::Error::other("the disk went away"))
            },
            &target,
            20,
        );
        assert_eq!(half.unwrap_err(), "the disk went away");
        assert!(!part.exists() && !target.exists());
        let short = copy_checked(|part| std::fs::write(part, b"short"), &target, 20);
        assert_eq!(
            short.unwrap_err(),
            "The copy came out 5 bytes, not 20, so it was not kept."
        );
        assert!(!part.exists() && !target.exists());
        copy_checked(|part| std::fs::write(part, b"0123456789"), &target, 10).unwrap();
        assert!(!part.exists() && target.exists());
    }

    /// S457: `logs/inspect` reads a log where it is, by path or bytes,
    /// and copies nothing.
    #[test]
    fn a_log_is_inspected_where_it_is() {
        use uxduck_engine::runtime::AutopilotLogs as _;
        let data = tempfile::tempdir().unwrap();
        let downloads = tempfile::tempdir().unwrap();
        let log = uxduck_mavlink::dataflash::fixture::flight();
        let original = downloads.path().join("00000046.BIN");
        std::fs::write(&original, &log).unwrap();
        let logs = Logs::new(data.path(), &Spans::default());
        let read = logs.inspect(&LogSource::Path(original.display().to_string()));
        assert_eq!(read.name, "00000046.BIN");
        assert_eq!(read.bytes, log.len() as u64);
        assert!(read.span.is_some_and(|s| s.gps_time));
        let bytes = logs.inspect(&LogSource::Bytes {
            name: "00000047.BIN".into(),
            data: base64::engine::general_purpose::STANDARD.encode(b"not a log"),
        });
        assert_eq!(bytes.path, None);
        assert_eq!(
            bytes.unread.as_deref(),
            Some("This is not an ArduPilot .bin or a PX4 .ulg log.")
        );
        // An upload is read where it is kept, which the page is not told.
        let uploaded = logs.inspect(&LogSource::Uploaded {
            name: "log44.BIN".into(),
            path: original.display().to_string(),
        });
        assert_eq!(uploaded.name, "log44.BIN");
        assert_eq!(uploaded.path, None);
        assert!(
            uploaded
                .span
                .is_some_and(|s| s.gps_time && s.board.is_some())
        );
        assert!(copied(data.path()).is_empty());
    }

    /// S457: an upload is copied into the logs under its own name, with no
    /// original to move, and a full disk says what it measured.
    #[test]
    fn an_upload_is_imported_by_its_name() {
        let data = tempfile::tempdir().unwrap();
        let kept = tempfile::tempdir().unwrap();
        let log = uxduck_mavlink::dataflash::fixture::flight();
        let upload = kept.path().join("whatever-the-upload-is-called");
        std::fs::write(&upload, &log).unwrap();
        let done = import(
            data.path(),
            "2026-11-22",
            &[LogSource::Uploaded {
                name: "phone/log44.BIN".into(),
                path: upload.display().to_string(),
            }],
            &Spans::default(),
        );
        assert_eq!(done.imported.len(), 1, "{done:?}");
        assert_eq!(done.imported[0].from, "phone/log44.BIN");
        assert_eq!(done.imported[0].original, None);
        assert_eq!(done.imported[0].log.name, "log44.BIN");
        assert_eq!(
            std::fs::read(done.imported[0].log.path.as_ref().unwrap()).unwrap(),
            log
        );
        let short = crate::attachments::short_of_space(data.path(), u64::MAX).unwrap();
        assert!(
            short.starts_with(
                "It needs 18446744073.71 GB and the disk UXDuck keeps its files on has "
            ) && short.ends_with(" free."),
            "{short}"
        );
        assert_eq!(crate::attachments::short_of_space(data.path(), 0), None);
    }

    /// S459: each copy's armings are kept with its span, and one read by a
    /// UXDuck from before keeps its span while it is read again for them.
    #[test]
    fn a_copy_read_before_is_read_again_once_for_its_armings() {
        let data = tempfile::tempdir().unwrap();
        let into = dir(data.path()).join("Imported-2026-11-22");
        std::fs::create_dir_all(&into).unwrap();
        let bin = uxduck_mavlink::dataflash::fixture::flight();
        let path = into.join("00000047.BIN");
        std::fs::write(&path, &bin).unwrap();
        let path = path.display().to_string();
        let old = serde_json::json!({
            path.clone(): {
                "bytes": bin.len(),
                "span": { "systemId": 7, "gpsTime": true, "fromUs": 1, "toUs": 2 }
            }
        });
        std::fs::write(spans_file(data.path()), old.to_string()).unwrap();
        let spans = Spans::default();
        let first = copied_with_spans(data.path(), &spans);
        assert_eq!(
            first[0].span.as_ref().map(|s| s.to_us),
            Some(2),
            "kept meanwhile"
        );
        assert!(spans.reads(&first).is_empty(), "no armings from before");
        let mut reads = Vec::new();
        for _ in 0..500 {
            let logs = copied_with_spans(data.path(), &spans);
            reads = spans.reads(&logs);
            if !reads.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let [read] = reads.as_slice() else {
            panic!("read again: {reads:?}");
        };
        assert_eq!(read.path, path);
        assert_eq!(read.armings.len(), 1, "{read:?}");
        assert_eq!(read.vehicle_type.as_deref(), Some("Quadrotor"));
        let again = Spans::default();
        let logs = copied_with_spans(data.path(), &again);
        assert_eq!(again.reads(&logs).len(), 1, "kept for the next start");
    }

    #[test]
    fn a_px4_card_is_known_by_its_ulogs_and_each_day_is_kept_in_the_name() {
        let volumes = tempfile::tempdir().unwrap();
        let card = volumes.path().join("PX4");
        for (path, bytes) in [
            ("log/2026-11-13/18_31_07.ulg", &b"one"[..]),
            ("log/2026-11-14/18_31_07.ulg", b"two!"),
            ("log/sess001/log001.ulg", b"three"),
            ("log/2026-11-13/notes.txt", b"not a log"),
            ("log/2026-11-13/._18_31_07.ulg", b"macOS's own"),
            ("etc/extras.txt", b"not a log"),
        ] {
            let file = card.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, bytes).unwrap();
        }
        // A stick whose log folder holds no ULog is no card.
        let stick = volumes.path().join("STICK");
        std::fs::create_dir_all(stick.join("log")).unwrap();
        std::fs::write(stick.join("log/data.bin"), b"x").unwrap();
        let cards = cards_in(&[card.clone(), stick]);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            (cards[0].name.as_str(), cards[0].logs, cards[0].bytes),
            ("PX4", 3, 12)
        );

        let data = tempfile::tempdir().unwrap();
        let copying = Copying::default();
        copying
            .start(data.path(), &card.display().to_string(), "2026-11-14")
            .unwrap();
        for _ in 0..200 {
            if copying.now().is_some_and(|n| n.1 == 3) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut names: Vec<String> = copied(data.path()).into_iter().map(|l| l.name).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "2026-11-13_18_31_07.ulg",
                "2026-11-14_18_31_07.ulg",
                "sess001_log001.ulg"
            ]
        );
        let empty = volumes.path().join("EMPTY");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            Copying::default()
                .start(data.path(), &empty.display().to_string(), "2026-11-14")
                .is_err()
        );
    }
}

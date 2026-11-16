//! Flights from other ground stations (ADR-0039 R18, S215): the tlogs
//! QGroundControl and Mission Planner keep on this computer, and folders
//! the person adds, looked through for arm edges when Flights opens and
//! each minute after, on a thread of their own. Nothing is copied, moved or
//! written: a flight replays straight from the other app's file. A log is
//! read a record at a time, never whole, so no size of log is left out.
//!
//! An added folder is looked into on its own, its top level only, or, with
//! all subfolders on, through its whole tree (a custom ground
//! station's folders, a disk of logs; maintainer, 2026-10-29): hidden
//! folders and links skipped, on a thread of its own, and walked every ten
//! minutes rather than every one. All of the tree is read, however large:
//! the folder says how many entries a walk has read so far, and how many
//! it read in how long. Until 2026-11-15 a walk stopped at 250,000 entries.
//! An app's own folder is looked through its whole tree each minute. Until
//! 2026-11-15 a log more than four folders down was passed over.
//!
//! What a look cannot read is kept with its folder and told in
//! `flights/found`, each with why: a folder that cannot be listed, a log
//! that cannot be opened or read, a log that stops being records before
//! its end, and a file named as a log that is not one. Until 2026-11-15
//! each was passed over without a word. An answer carries how many there
//! are and one page of them ([`NOT_READ_PAGE`]), and the rest is read on
//! from an offset, since a disk decides how many there are.
//!
//! The folders, checked against each app's own source:
//! - QGroundControl: its save path, by default `Documents/QGroundControl`,
//!   and its `Telemetry` folder (`AppSettings::telemetryDirectory`).
//! - Mission Planner: `Documents/Mission Planner/logs` (its
//!   `Settings.GetDefaultLogDir`), with `<vehicle type>/<system id>/`
//!   folders beneath; under Mono on a Mac or Linux without that folder,
//!   the local application data folder's `Mission Planner/logs`; older
//!   Windows installs, `logs` in its program folder.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uxduck_contracts::dto::replay::{
    FlightFolder, FlightFolderParams, FolderWalk, FoundFlight, FoundFlightsParams,
    FoundFlightsResult, NotRead, NotReadFrom,
};

/// How often the folders are looked through while Flights asks.
const LOOK_EVERY: Duration = Duration::from_mins(1);
/// A file written this recently is still being recorded.
const GROWING: Duration = Duration::from_secs(10);
/// A folder with all subfolders on is walked this often after its last walk
/// ended: a disk takes a while, and its logs replay from where they are, so
/// one found ten minutes late is not missed.
const DEEP_EVERY: Duration = Duration::from_mins(10);
/// A walk says how far it has got each time it has read this many entries.
const TELL_EVERY: u64 = 1024;
/// How much of a log is read from the disk at a time.
const READ_AT_A_TIME: usize = 256 * 1024;
/// How many of what a folder could not read one answer carries. A disk
/// UXDuck does not control decides how many there are: a Visual Studio
/// build writes `.tlog` files of its own into every build folder, so a
/// folder of source trees holds thousands that are not telemetry logs.
/// The window asks every two seconds while a look runs, the Logbook each
/// minute after, and Settings draws every one it is given: 5,000 took
/// Chrome 2.0 s (2026-11-15). So an answer holds a page, with how many
/// there are, and the rest is read on from an offset. None is left out.
const NOT_READ_PAGE: usize = 100;
/// The longest record: a stamp and the longest MAVLink frame (a v2 header,
/// 255 bytes of payload, its checksum and a signature).
const MAX_RECORD: usize = 8 + 10 + 255 + 2 + 13;
/// `MAV_TYPE_GCS`: a ground station's heartbeat, never a flight.
const GCS: u8 = 6;

/// The person's choices, kept in `flight-folders.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Choices {
    /// Known folders turned off, by id.
    #[serde(default)]
    off: Vec<String>,
    /// Folders added, by path.
    #[serde(default)]
    added: Vec<String>,
    /// Added folders looked through with all their subfolders, by path.
    #[serde(default)]
    deep: Vec<String>,
}

/// One file as last looked through.
#[derive(Clone, Debug)]
struct Seen {
    size: u64,
    modified: SystemTime,
    flights: Vec<FoundFlight>,
    /// The byte where it stops being records, when that is before its end.
    stopped: Option<u64>,
}

/// A deep folder's last walk: when it ended, its files, how many entries
/// it read in how long, and the folders it could not list.
#[derive(Clone, Debug)]
struct Walk {
    at: Instant,
    files: Vec<PathBuf>,
    entries: u64,
    took: Duration,
    not_read: Vec<NotRead>,
}

#[derive(Debug, Default)]
struct State {
    looked_at: Option<Instant>,
    scanning: bool,
    files: HashMap<PathBuf, Seen>,
    /// Deep folders' walks, by folder id.
    walks: HashMap<String, Walk>,
    /// Walks under way, by folder id: the entries each has read so far.
    walking: HashMap<String, u64>,
    /// A walk ended while a look was under way: the look's files are not
    /// that walk's, so the next ask looks again.
    walked_since: bool,
    /// What the last look could not read, by folder id.
    not_read: HashMap<String, Unread>,
}

/// What a look could not read in one folder.
#[derive(Debug, Default)]
struct Unread {
    /// Those that may hold flights, then the files that are not logs, each
    /// by path: the order a person looks for one in, with what matters to
    /// the Logbook first.
    told: Vec<NotRead>,
    /// How many at the end of `told` are files named as a log that do not
    /// begin with a record.
    not_logs: u64,
}

impl Unread {
    fn of(mut told: Vec<NotRead>, mut not_logs: Vec<NotRead>) -> Self {
        told.sort_by(|a, b| a.path.cmp(&b.path));
        not_logs.sort_by(|a, b| a.path.cmp(&b.path));
        let count = not_logs.len() as u64;
        told.append(&mut not_logs);
        Self {
            told,
            not_logs: count,
        }
    }
}

/// The folders, what was found in them, and the look under way.
#[derive(Clone, Debug)]
pub struct Found {
    data_dir: PathBuf,
    state: Arc<Mutex<State>>,
}

fn documents() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(|home| PathBuf::from(home).join("Documents"))
}

/// The apps' own folders on this platform, by id and name.
#[must_use]
pub fn known_folders() -> Vec<(String, String, PathBuf)> {
    let mut folders = Vec::new();
    if let Some(documents) = documents() {
        folders.push((
            "qgroundcontrol".to_owned(),
            "QGroundControl".to_owned(),
            documents.join("QGroundControl").join("Telemetry"),
        ));
        let mission_planner = documents.join("Mission Planner");
        let mut logs = mission_planner.join("logs");
        // Under Mono with no Documents folder of its own, the local data one.
        if !cfg!(windows)
            && !mission_planner.exists()
            && let Some(home) = std::env::var_os("HOME")
        {
            let local = if cfg!(target_os = "macos") {
                PathBuf::from(&home).join(".local/share")
            } else {
                std::env::var_os("XDG_DATA_HOME")
                    .map_or_else(|| PathBuf::from(&home).join(".local/share"), PathBuf::from)
            };
            logs = local.join("Mission Planner").join("logs");
        }
        folders.push((
            "mission-planner".to_owned(),
            "Mission Planner".to_owned(),
            logs,
        ));
    }
    #[cfg(windows)]
    for program_files in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(dir) = std::env::var_os(program_files) {
            let logs = PathBuf::from(dir).join("Mission Planner").join("logs");
            if logs.exists() {
                folders.push((
                    "mission-planner-install".to_owned(),
                    "Mission Planner".to_owned(),
                    logs,
                ));
                break;
            }
        }
    }
    folders
}

/// Something a look could not read, with the system's words for why.
fn unread(path: &Path, what: &str, error: &std::io::Error) -> NotRead {
    NotRead {
        path: path.display().to_string(),
        why: format!("UXDuck could not {what}: {error}."),
    }
}

/// Every `.tlog` under a folder: at its top level only, or through its
/// whole `tree`, hidden folders and links skipped. With them, how many
/// entries were read. A folder that cannot be listed goes in `not_read`.
/// `go_on` hears how many entries have been read every [`TELL_EVERY`] of
/// them, and ends the walk by answering false.
fn walk(
    folder: &Path,
    tree: bool,
    not_read: &mut Vec<NotRead>,
    mut go_on: impl FnMut(u64) -> bool,
) -> (Vec<PathBuf>, u64) {
    let mut files = Vec::new();
    let mut entries = 0u64;
    let mut folders = vec![folder.to_owned()];
    while let Some(folder) = folders.pop() {
        let listed = match std::fs::read_dir(&folder) {
            Ok(listed) => listed,
            Err(error) => {
                not_read.push(unread(&folder, "list it", &error));
                continue;
            }
        };
        for entry in listed {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    // The rest of this folder is not listed.
                    not_read.push(unread(&folder, "list all of it", &error));
                    break;
                }
            };
            entries += 1;
            if entries.is_multiple_of(TELL_EVERY) && !go_on(entries) {
                return (files, entries);
            }
            let path = entry.path();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    not_read.push(unread(&path, "tell what it is", &error));
                    continue;
                }
            };
            // Nothing hidden (a disk's `.Trashes`), nothing through a link.
            if kind.is_symlink() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if kind.is_dir() {
                if tree {
                    folders.push(path);
                }
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tlog"))
            {
                files.push(path);
            }
        }
    }
    (files, entries)
}

/// What comes next in a log.
enum Next {
    /// A record: its stamp and how long its frame is, which follows the
    /// stamp's 8 bytes.
    Record(u64, usize),
    /// The log's end, also inside a record: one still being written, or
    /// one a crash cut short.
    End,
    /// Bytes that begin no MAVLink frame.
    NotARecord,
}

/// Reads the log's next record into `record`.
fn next_record(log: &mut impl Read, record: &mut [u8; MAX_RECORD]) -> std::io::Result<Next> {
    // A stamp and the three bytes that say how long its frame is.
    const HEAD: usize = 8 + 3;
    let whole = |read: std::io::Result<()>| match read {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    };
    if !whole(log.read_exact(&mut record[..HEAD]))? {
        return Ok(Next::End);
    }
    let Some(len) = uxduck_mavlink::framing::frame_len(&record[8..HEAD]) else {
        return Ok(Next::NotARecord);
    };
    if !whole(log.read_exact(&mut record[HEAD..8 + len]))? {
        return Ok(Next::End);
    }
    let mut stamp = [0u8; 8];
    stamp.copy_from_slice(&record[..8]);
    Ok(Next::Record(u64::from_be_bytes(stamp), len))
}

/// A tlog's flights: each vehicle's arm to disarm, by its heartbeats.
///
/// The log is read a record at a time and none of it is held, so its size
/// does not matter. Until 2026-11-14 a log was read whole into memory, and
/// one over 2 GB was skipped without a word to anyone. Records end at the
/// first that is not whole, as [`uxduck_mavlink::tlog::records`] do.
///
/// With the flights, the byte where the log stops being records when that
/// is not its end: a replay stops there too, and flights after it are not
/// found, which the caller says. A log that ends inside a record is one
/// still being written or cut short by a crash, and nothing to say.
///
/// # Errors
/// When the log cannot be read. Its end is no error.
pub fn flights_in(
    mut log: impl Read,
    folder: &str,
    path: &Path,
) -> std::io::Result<(Vec<FoundFlight>, Option<u64>)> {
    let mut armed: HashMap<u8, (u64, String)> = HashMap::new();
    let mut flights = Vec::new();
    let mut last = 0u64;
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut record = [0u8; MAX_RECORD];
    let mut at = 0u64;
    let stopped = loop {
        let (stamp, len) = match next_record(&mut log, &mut record)? {
            Next::Record(stamp, len) => (stamp, len),
            Next::End => break None,
            Next::NotARecord => break Some(at),
        };
        at += 8 + len as u64;
        let frame = &record[8..8 + len];
        last = stamp;
        let (Some(header), Some(payload)) = (
            uxduck_mavlink::framing::header(frame),
            uxduck_mavlink::framing::payload(frame),
        ) else {
            continue;
        };
        // HEARTBEAT: custom_mode, type, autopilot, base_mode.
        if header.message_id != 0 || payload.len() < 7 || payload[4] == GCS {
            continue;
        }
        let is_armed = payload[6] & 0x80 != 0;
        let system = header.system_id;
        match (armed.contains_key(&system), is_armed) {
            (false, true) => {
                armed.insert(system, (stamp, kind_name(payload[4])));
            }
            (true, false) => {
                if let Some((from, kind)) = armed.remove(&system) {
                    flights.push(FoundFlight {
                        folder: folder.to_owned(),
                        path: path.display().to_string(),
                        file: file.clone(),
                        system_id: system,
                        vehicle_type: kind,
                        armed_at_us: from,
                        ended_at_us: Some(stamp),
                        recording: false,
                    });
                }
            }
            _ => {}
        }
    };
    // Still armed where the log ends.
    for (system, (from, kind)) in armed {
        flights.push(FoundFlight {
            folder: folder.to_owned(),
            path: path.display().to_string(),
            file: file.clone(),
            system_id: system,
            vehicle_type: kind,
            armed_at_us: from,
            ended_at_us: Some(last),
            recording: false,
        });
    }
    Ok((flights, stopped))
}

/// `MAV_TYPE` by name, for the vehicles UXDuck draws.
fn kind_name(kind: u8) -> String {
    match kind {
        1 => "FixedWing",
        2 => "Quadrotor",
        3 => "Coaxial",
        4 => "Helicopter",
        10 => "GroundRover",
        11 => "SurfaceBoat",
        12 => "Submarine",
        13 => "Hexarotor",
        14 => "Octorotor",
        15 => "Tricopter",
        7 => "Airship",
        19..=25 => "VtolFixedRotor",
        _ => "Generic",
    }
    .to_owned()
}

impl Found {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_owned(),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn choices_path(&self) -> PathBuf {
        self.data_dir.join("flight-folders.json")
    }

    fn choices(&self) -> Choices {
        std::fs::read(self.choices_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, choices: &Choices) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(choices).map_err(|e| e.to_string())?;
        std::fs::write(self.choices_path(), bytes).map_err(|e| e.to_string())
    }

    /// The folders looked in or not, the known first, each with how many
    /// the last look could not read in it and a page of them: from the
    /// start, or `from` an offset for the folder that names.
    #[must_use]
    pub fn folders(&self, from: Option<&NotReadFrom>) -> Vec<FlightFolder> {
        let choices = self.choices();
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let told = |folder: &mut FlightFolder| {
            let Some(unread) = state.not_read.get(&folder.id) else {
                return;
            };
            let offset = from
                .filter(|from| from.folder == folder.id)
                .map_or(0, |from| usize::try_from(from.offset).unwrap_or(usize::MAX))
                .min(unread.told.len());
            folder.not_read = unread.told[offset..]
                .iter()
                .take(NOT_READ_PAGE)
                .cloned()
                .collect();
            folder.not_read_total = unread.told.len() as u64;
            folder.not_read_offset = offset as u64;
            folder.not_logs = unread.not_logs;
        };
        let mut folders: Vec<FlightFolder> = known_folders()
            .into_iter()
            .map(|(id, name, path)| FlightFolder {
                on: !choices.off.contains(&id),
                exists: path.is_dir(),
                path: path.display().to_string(),
                id,
                name,
                ..FlightFolder::default()
            })
            .collect();
        for added in &choices.added {
            let path = PathBuf::from(added);
            // A walk under way first: its last one is being replaced.
            let walk = state
                .walking
                .get(added)
                .map(|&entries| FolderWalk {
                    entries,
                    took_ms: None,
                })
                .or_else(|| {
                    state.walks.get(added).map(|walk| FolderWalk {
                        entries: walk.entries,
                        took_ms: Some(u64::try_from(walk.took.as_millis()).unwrap_or(u64::MAX)),
                    })
                });
            folders.push(FlightFolder {
                id: added.clone(),
                name: path
                    .file_name()
                    .map_or_else(|| added.clone(), |n| n.to_string_lossy().into_owned()),
                on: !choices.off.contains(added),
                exists: path.is_dir(),
                path: added.clone(),
                added: true,
                deep: choices.deep.contains(added),
                walk: walk.filter(|_| choices.deep.contains(added)),
                ..FlightFolder::default()
            });
        }
        folders.iter_mut().for_each(told);
        folders
    }

    /// What was found, newest first; a look starts when the last is a
    /// minute old.
    #[must_use]
    pub fn found(&self, params: &FoundFlightsParams) -> FoundFlightsResult {
        let folders = self.folders(params.not_read.as_ref());
        let due = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            !state.scanning && state.looked_at.is_none_or(|at| at.elapsed() >= LOOK_EVERY)
        };
        if due {
            self.look(folders.clone());
        }
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let on: Vec<&FlightFolder> = folders.iter().filter(|f| f.on).collect();
        let mut flights: Vec<FoundFlight> = state
            .files
            .iter()
            .filter(|(path, _)| on.iter().any(|f| path.starts_with(&f.path)))
            .flat_map(|(_, seen)| seen.flights.clone())
            .collect();
        flights.sort_by(|a, b| b.armed_at_us.cmp(&a.armed_at_us).then(a.path.cmp(&b.path)));
        FoundFlightsResult {
            folders,
            flights,
            scanning: state.scanning || !state.walking.is_empty(),
        }
    }

    /// Starts a walk through a folder's whole tree on a thread of its own,
    /// unless one is under way or its last ended less than [`DEEP_EVERY`]
    /// ago. A walk may take minutes, and the other folders are not kept
    /// waiting for it: a look reads the files its last walk found.
    fn walk_when_due(state: &Arc<Mutex<State>>, folder: &FlightFolder) {
        {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            let fresh = state
                .walks
                .get(&folder.id)
                .is_some_and(|walk| walk.at.elapsed() < DEEP_EVERY);
            if fresh || state.walking.contains_key(&folder.id) {
                return;
            }
            state.walking.insert(folder.id.clone(), 0);
        }
        let (state, id, path) = (
            state.clone(),
            folder.id.clone(),
            PathBuf::from(&folder.path),
        );
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut not_read = Vec::new();
            let (files, entries) = walk(&path, true, &mut not_read, |entries| {
                // Ended when the folder is removed or changed meanwhile.
                state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .walking
                    .get_mut(&id)
                    .map(|so_far| *so_far = entries)
                    .is_some()
            });
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.walking.remove(&id).is_none() {
                return;
            }
            state.walks.insert(
                id,
                Walk {
                    at: Instant::now(),
                    files,
                    entries,
                    took: started.elapsed(),
                    not_read,
                },
            );
            // The next ask looks through what it found.
            state.looked_at = None;
            state.walked_since = true;
        });
    }

    /// Looks through the folders on a thread of its own: only files new
    /// or changed since the last look are read.
    fn look(&self, folders: Vec<FlightFolder>) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.scanning = true;
            state.walked_since = false;
        }
        let state = self.state.clone();
        std::thread::spawn(move || {
            let mut files = Vec::new();
            // By folder id: those that may hold flights, and the files
            // that are not logs.
            let mut not_read: HashMap<String, (Vec<NotRead>, Vec<NotRead>)> = HashMap::new();
            for folder in folders.iter().filter(|f| f.on && f.exists) {
                let unread = &mut not_read.entry(folder.id.clone()).or_default().0;
                let found = if folder.deep {
                    Self::walk_when_due(&state, folder);
                    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
                    state.walks.get(&folder.id).map_or_else(Vec::new, |walk| {
                        unread.extend(walk.not_read.iter().cloned());
                        walk.files.clone()
                    })
                } else {
                    // An app's folder through its whole tree, as deep as
                    // its logs are; an added one at its top level only
                    // (maintainer, 2026-10-29).
                    walk(Path::new(&folder.path), !folder.added, unread, |_| true).0
                };
                files.extend(found.into_iter().map(|p| (folder.id.clone(), p)));
            }
            let mut fresh: HashMap<PathBuf, Seen> = HashMap::new();
            for (folder, path) in files {
                let (unread, not_logs) = not_read.entry(folder.clone()).or_default();
                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(error) => {
                        unread.push(self::unread(&path, "open it", &error));
                        continue;
                    }
                };
                let (size, modified) = (
                    meta.len(),
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                );
                let growing = modified.elapsed().is_ok_and(|age| age < GROWING);
                let known = state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .files
                    .get(&path)
                    .filter(|seen| seen.size == size && seen.modified == modified)
                    .cloned();
                let mut seen = if let Some(seen) = known {
                    seen
                } else {
                    // Read through, however long it is. One that cannot be
                    // read is said, and tried again at the next look.
                    let read = File::open(&path).and_then(|file| {
                        let log = BufReader::with_capacity(READ_AT_A_TIME, file);
                        flights_in(log, &folder, &path)
                    });
                    match read {
                        Ok((flights, stopped)) => Seen {
                            size,
                            modified,
                            flights,
                            stopped,
                        },
                        Err(error) => {
                            unread.push(self::unread(&path, "read it", &error));
                            continue;
                        }
                    }
                };
                match seen.stopped {
                    // Nothing of a log, such as a build tool's `.tlog`:
                    // told, and apart from those that may hold flights. A
                    // log whose first bytes are damaged is among them.
                    Some(0) => not_logs.push(NotRead {
                        path: path.display().to_string(),
                        why: "It is not a telemetry log. It does not begin with a record."
                            .to_owned(),
                    }),
                    Some(at) => unread.push(NotRead {
                        path: path.display().to_string(),
                        why: format!(
                            "It stops being a telemetry log at byte {at} of {size}. Flights after that are not listed."
                        ),
                    }),
                    None => {}
                }
                for flight in &mut seen.flights {
                    flight.recording = growing;
                }
                fresh.insert(path, seen);
            }
            // In the order a person looks for one, not the walk's.
            let not_read = not_read
                .into_iter()
                .map(|(folder, (told, not_logs))| (folder, Unread::of(told, not_logs)))
                .collect();
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.files = fresh;
            state.not_read = not_read;
            state.looked_at = (!state.walked_since).then(Instant::now);
            state.scanning = false;
        });
    }

    /// A folder on or off, added or removed, or looked through with all its
    /// subfolders or not; the next ask looks again.
    ///
    /// # Errors
    /// A folder added that is not one, removing one of the apps', or all
    /// subfolders of one of the apps'.
    pub fn change(&self, params: &FlightFolderParams) -> Result<FoundFlightsResult, String> {
        let mut choices = self.choices();
        let known = known_folders().iter().any(|(id, _, _)| *id == params.id);
        if params.remove {
            if known {
                return Err("An app's own folder can only be turned off.".into());
            }
            choices.added.retain(|p| *p != params.id);
            choices.off.retain(|p| *p != params.id);
            choices.deep.retain(|p| *p != params.id);
        } else {
            if !known && !choices.added.contains(&params.id) {
                if !Path::new(&params.id).is_dir() {
                    return Err(format!("{} is not a folder on this computer.", params.id));
                }
                choices.added.push(params.id.clone());
            }
            match params.on {
                Some(false) if !choices.off.contains(&params.id) => {
                    choices.off.push(params.id.clone());
                }
                Some(true) => choices.off.retain(|p| *p != params.id),
                _ => {}
            }
            match params.deep {
                Some(true) if known => {
                    return Err("An app's own folder is looked through as the app keeps it.".into());
                }
                Some(true) if !choices.deep.contains(&params.id) => {
                    choices.deep.push(params.id.clone());
                }
                Some(false) => choices.deep.retain(|p| *p != params.id),
                _ => {}
            }
        }
        self.save(&choices)?;
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.looked_at = None;
            // Its tree is walked afresh, now or never again, and a walk of
            // it under way ends.
            state.walks.remove(&params.id);
            state.walking.remove(&params.id);
        }
        Ok(self.found(&FoundFlightsParams::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_mavlink::sim;

    const T0: u64 = 1_790_000_000_000_000;

    fn log(flights: &[(u8, u64, u64)], end: u64) -> Vec<u8> {
        let mut log = Vec::new();
        for second in 0..end {
            for &(system, from, to) in flights {
                let armed = (from..to).contains(&second);
                let frame = sim::armed_heartbeat_frame(system, 1, 0, armed, 4);
                uxduck_mavlink::tlog::record(&mut log, T0 + second * 1_000_000, &frame).unwrap();
            }
            // Another ground station's heartbeat is never a flight.
            let gcs = sim::gcs_heartbeat_frame(255, 190, 0);
            uxduck_mavlink::tlog::record(&mut log, T0 + second * 1_000_000, &gcs).unwrap();
        }
        log
    }

    #[test]
    fn a_tlogs_flights_are_each_vehicles_arm_to_disarm() {
        let bytes = log(&[(1, 10, 40), (2, 20, 70)], 60);
        let (mut flights, stopped) = flights_in(
            &bytes[..],
            "qgroundcontrol",
            Path::new("/x/2026-10-28 14-02-17.tlog"),
        )
        .unwrap();
        assert_eq!(stopped, None);
        flights.sort_by_key(|f| f.system_id);
        assert_eq!(flights.len(), 2);
        assert_eq!(flights[0].armed_at_us, T0 + 10_000_000);
        assert_eq!(flights[0].ended_at_us, Some(T0 + 40_000_000));
        assert_eq!(flights[0].vehicle_type, "Quadrotor");
        assert_eq!(flights[0].file, "2026-10-28 14-02-17.tlog");
        // Still armed where the log ends.
        assert_eq!(flights[1].ended_at_us, Some(T0 + 59_000_000));
    }

    #[test]
    fn the_folders_are_looked_through_and_a_folder_added_and_turned_off() {
        let data = tempfile::tempdir().unwrap();
        let mavproxy = tempfile::tempdir().unwrap();
        // MAVProxy's way: its logs at the folder's top.
        std::fs::write(
            mavproxy.path().join("2026-10-28 14-02-17.tlog"),
            log(&[(1, 5, 15)], 20),
        )
        .unwrap();
        let found = Found::new(data.path());
        let path = mavproxy.path().display().to_string();
        let changed = found
            .change(&FlightFolderParams {
                id: path.clone(),
                on: Some(true),
                remove: false,
                deep: None,
            })
            .unwrap();
        assert!(changed.folders.iter().any(|f| f.added && f.path == path));
        // This computer's own QGroundControl and Mission Planner folders
        // may hold flights too: only the added folder's are counted.
        let mut flights = Vec::new();
        for _ in 0..500 {
            let result = found.found(&FoundFlightsParams::default());
            flights = result
                .flights
                .into_iter()
                .filter(|f| f.folder == path)
                .collect();
            if !flights.is_empty() && !result.scanning {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(flights.len(), 1);
        assert_eq!(flights[0].folder, path);
        // Written just now: still being recorded, as far as anyone can tell.
        assert!(flights[0].recording);
        // Turned off: not listed.
        let off = found
            .change(&FlightFolderParams {
                id: path.clone(),
                on: Some(false),
                remove: false,
                deep: None,
            })
            .unwrap();
        assert!(off.flights.iter().all(|f| f.folder != path));
        assert!(
            found
                .change(&FlightFolderParams {
                    id: "/no/such/folder".into(),
                    on: Some(true),
                    remove: false,
                    deep: None,
                })
                .is_err()
        );
    }

    /// A log is read a record at a time: it gives the flights reading it
    /// whole did, also one that ends inside a record, one with bytes that
    /// are no record after its last, and one read a few bytes at a time.
    #[test]
    fn a_log_read_a_record_at_a_time_gives_the_flights_of_one_read_whole() {
        /// Gives at most seven bytes a read, so every record spans reads.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(7);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        /// A disk that stops answering.
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the disk went away"))
            }
        }
        /// The flights as they were found before: every record at once.
        fn whole(bytes: &[u8]) -> Vec<(u8, u64, Option<u64>)> {
            let mut armed: HashMap<u8, u64> = HashMap::new();
            let mut flights = Vec::new();
            let mut last = 0;
            for (stamp, frame) in uxduck_mavlink::tlog::read(bytes) {
                last = stamp;
                let header = uxduck_mavlink::framing::header(frame).unwrap();
                let payload = uxduck_mavlink::framing::payload(frame).unwrap();
                if header.message_id != 0 || payload[4] == GCS {
                    continue;
                }
                let system = header.system_id;
                if payload[6] & 0x80 == 0 {
                    if let Some(from) = armed.remove(&system) {
                        flights.push((system, from, Some(stamp)));
                    }
                } else {
                    armed.entry(system).or_insert(stamp);
                }
            }
            flights.extend(armed.into_iter().map(|(s, from)| (s, from, Some(last))));
            flights.sort_unstable();
            flights
        }
        let found = |log: &[u8]| {
            let mut flights: Vec<(u8, u64, Option<u64>)> =
                flights_in(Trickle(log), "f", Path::new("/x/a.tlog"))
                    .unwrap()
                    .0
                    .into_iter()
                    .map(|f| (f.system_id, f.armed_at_us, f.ended_at_us))
                    .collect();
            flights.sort_unstable();
            flights
        };
        let bytes = log(&[(1, 10, 40), (2, 20, 70)], 60);
        assert_eq!(found(&bytes), whole(&bytes));
        assert_eq!(found(&bytes).len(), 2);
        // The log ends inside a record: the flights up to it.
        let cut = &bytes[..bytes.len() - 5];
        assert_eq!(found(cut), whole(cut));
        assert_eq!(found(cut).len(), 2);
        // Bytes that are no record end it, as they do a log read whole.
        let mut noise = log(&[(1, 10, 40)], 30);
        noise.extend_from_slice(&[0u8; 64]);
        noise.extend_from_slice(&log(&[(3, 1, 5)], 10));
        assert_eq!(found(&noise), whole(&noise));
        assert_eq!(found(&noise).len(), 1);
        // A log that cannot be read is an error, never an empty log.
        assert!(flights_in(Broken, "f", Path::new("/x/a.tlog")).is_err());
    }

    /// A log over 2 GB was skipped without a word until 2026-11-14. Its
    /// length is a hole in the file here, not 2 GB written to the disk.
    #[test]
    fn a_log_over_2_gb_is_looked_through_like_any_other() {
        let data = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let long = logs.path().join("three-days.tlog");
        std::fs::write(&long, log(&[(1, 5, 15), (2, 8, 30)], 20)).unwrap();
        File::options()
            .write(true)
            .open(&long)
            .unwrap()
            .set_len((2 << 30) + 4096)
            .unwrap();
        assert!(std::fs::metadata(&long).unwrap().len() > 2 << 30);
        std::fs::write(logs.path().join("short.tlog"), log(&[(3, 5, 15)], 20)).unwrap();
        let found = Found::new(data.path());
        let path = logs.path().display().to_string();
        found
            .change(&FlightFolderParams {
                id: path.clone(),
                on: Some(true),
                remove: false,
                deep: None,
            })
            .unwrap();
        let mut flights: Vec<(String, u8)> = settled(&found, &path)
            .into_iter()
            .map(|f| (f.file, f.system_id))
            .collect();
        flights.sort();
        assert_eq!(
            flights,
            [
                ("short.tlog".to_owned(), 3),
                ("three-days.tlog".to_owned(), 1),
                ("three-days.tlog".to_owned(), 2),
            ]
        );
    }

    /// Waits for a look to finish and gives the added folder's flights.
    fn settled(found: &Found, folder: &str) -> Vec<FoundFlight> {
        for _ in 0..500 {
            let result = found.found(&FoundFlightsParams::default());
            let looked = found
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .looked_at
                .is_some();
            if !result.scanning && looked {
                return result
                    .flights
                    .into_iter()
                    .filter(|f| f.folder == folder)
                    .collect();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the look never finished");
    }

    #[test]
    fn a_folder_of_ones_own_is_walked_whole_only_with_all_subfolders_on() {
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        // One level down, six levels down, one hidden, one at the top.
        let deep = disk.path().join("a/b/c/d/e/f");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.tlog"), log(&[(1, 5, 15)], 20)).unwrap();
        let hidden = disk.path().join(".Trashes");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("binned.tlog"), log(&[(1, 5, 15)], 20)).unwrap();
        std::fs::write(disk.path().join("top.tlog"), log(&[(2, 5, 15)], 20)).unwrap();
        std::fs::create_dir_all(disk.path().join("2026")).unwrap();
        std::fs::write(disk.path().join("2026/near.tlog"), log(&[(3, 5, 15)], 20)).unwrap();
        let found = Found::new(data.path());
        let path = disk.path().display().to_string();
        let added = |deep: Option<bool>| FlightFolderParams {
            id: path.clone(),
            on: Some(true),
            remove: false,
            deep,
        };
        found.change(&added(None)).unwrap();
        let shallow = settled(&found, &path);
        assert_eq!(shallow.len(), 1, "{shallow:?}");
        assert_eq!(shallow[0].file, "top.tlog");
        // All subfolders: the deep one too, never the hidden one.
        let changed = found.change(&added(Some(true))).unwrap();
        assert!(changed.folders.iter().any(|f| f.id == path && f.deep));
        let mut whole: Vec<String> = settled(&found, &path).into_iter().map(|f| f.file).collect();
        whole.sort();
        assert_eq!(whole, ["deep.tlog", "near.tlog", "top.tlog"]);
        assert!(
            found
                .found(&FoundFlightsParams::default())
                .folders
                .iter()
                .any(|f| f.id == path && !f.cut)
        );
        // Off again: back to the top.
        found.change(&added(Some(false))).unwrap();
        assert_eq!(settled(&found, &path).len(), 1);
        // The apps' own folders have no such switch: they are always
        // looked through as the apps keep them, however deep.
        assert!(
            found
                .change(&FlightFolderParams {
                    id: "qgroundcontrol".into(),
                    on: None,
                    remove: false,
                    deep: Some(true),
                })
                .is_err()
        );
    }

    /// The added folder as `flights/found` gives it.
    fn folder(found: &Found, id: &str) -> FlightFolder {
        found
            .found(&FoundFlightsParams::default())
            .folders
            .into_iter()
            .find(|f| f.id == id)
            .unwrap()
    }

    fn add(found: &Found, path: &str, deep: Option<bool>) {
        found
            .change(&FlightFolderParams {
                id: path.to_owned(),
                on: Some(true),
                remove: false,
                deep,
            })
            .unwrap();
    }

    /// A log that stops being records before its end gives the flights up
    /// to there, and the folder says where it stopped. Until 2026-11-15
    /// nothing said that the rest of the log was not looked through.
    #[test]
    fn a_log_that_stops_being_records_is_told_with_its_folder_and_keeps_its_earlier_flights() {
        let data = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let mut damaged = log(&[(1, 5, 15)], 20);
        let at = damaged.len();
        damaged.extend_from_slice(&[0u8; 64]);
        damaged.extend_from_slice(&log(&[(3, 1, 5)], 10));
        std::fs::write(logs.path().join("damaged.tlog"), &damaged).unwrap();
        std::fs::write(logs.path().join("whole.tlog"), log(&[(2, 5, 15)], 20)).unwrap();
        // One that ends inside a record is still being written, or was cut
        // short by a crash: nothing to say.
        let short = log(&[(4, 5, 15)], 20);
        std::fs::write(logs.path().join("short.tlog"), &short[..short.len() - 5]).unwrap();
        // A file with the name of a log and none of one.
        std::fs::write(logs.path().join("notes.tlog"), b"not a telemetry log").unwrap();
        let found = Found::new(data.path());
        let path = logs.path().display().to_string();
        add(&found, &path, None);
        let mut systems: Vec<u8> = settled(&found, &path).iter().map(|f| f.system_id).collect();
        systems.sort_unstable();
        assert_eq!(
            systems,
            [1, 2, 4],
            "the flight after the damage is not found"
        );
        // The one that may hold flights, then the one that is no log.
        let told = [
            NotRead {
                path: logs.path().join("damaged.tlog").display().to_string(),
                why: format!(
                    "It stops being a telemetry log at byte {at} of {}. Flights after that are not listed.",
                    damaged.len()
                ),
            },
            NotRead {
                path: logs.path().join("notes.tlog").display().to_string(),
                why: "It is not a telemetry log. It does not begin with a record.".to_owned(),
            },
        ];
        let folder_told = folder(&found, &path);
        assert_eq!(folder_told.not_read, told);
        assert_eq!((folder_told.not_read_total, folder_told.not_logs), (2, 1));
        // The next look reads none of them again, and still says it.
        found
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .looked_at = None;
        assert_eq!(settled(&found, &path).len(), 3);
        assert_eq!(folder(&found, &path).not_read, told);
        // Mended, and the other gone: nothing to say.
        std::fs::write(logs.path().join("damaged.tlog"), log(&[(1, 5, 15)], 20)).unwrap();
        std::fs::remove_file(logs.path().join("notes.tlog")).unwrap();
        found
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .looked_at = None;
        settled(&found, &path);
        assert_eq!(folder(&found, &path).not_read, []);
    }

    /// A folder says how many a look could not read and gives a page of
    /// them, and the next page goes on where the one before stopped. A
    /// disk decides how many there are (a build tool writes `.tlog` files
    /// of its own), so no answer carries them all and none is left out.
    /// Those that may hold flights come first, and the files that are not
    /// logs are counted apart, for the Logbook's note.
    #[test]
    fn what_a_folder_could_not_read_is_told_a_page_at_a_time_with_how_many_there_are() {
        let data = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        // A page and a half of a build tool's files, named as logs.
        let builds = NOT_READ_PAGE + NOT_READ_PAGE / 2;
        let build = |n: usize| logs.path().join(format!("{n:04}.tlog"));
        for n in 0..builds {
            std::fs::write(build(n), b"^C:\\SRC\\MAIN.CPP").unwrap();
        }
        let mut damaged = log(&[(1, 5, 15)], 20);
        damaged.extend_from_slice(&[0u8; 64]);
        std::fs::write(logs.path().join("damaged.tlog"), &damaged).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("notes.tlog"), b"not a telemetry log").unwrap();
        let found = Found::new(data.path());
        let (path, other) = (
            logs.path().display().to_string(),
            other.path().display().to_string(),
        );
        add(&found, &other, None);
        settled(&found, &other);
        add(&found, &path, None);
        assert_eq!(settled(&found, &path).len(), 1);
        let total = builds as u64 + 1;
        let from = |offset: u64| {
            found
                .found(&FoundFlightsParams {
                    not_read: Some(NotReadFrom {
                        folder: path.clone(),
                        offset,
                    }),
                })
                .folders
        };
        let of = |folders: &[FlightFolder], id: &str| {
            let folder = folders.iter().find(|f| f.id == id).unwrap();
            (
                folder.not_read.len(),
                folder.not_read_offset,
                folder.not_read_total,
                folder.not_logs,
            )
        };

        let first = folder(&found, &path);
        assert_eq!(
            of(std::slice::from_ref(&first), &path),
            (NOT_READ_PAGE, 0, total, builds as u64)
        );
        let page = NOT_READ_PAGE as u64;
        let second = from(page);
        assert_eq!(
            of(&second, &path),
            (builds + 1 - NOT_READ_PAGE, page, total, builds as u64)
        );
        // Another folder is given from its start, whatever is asked of this.
        assert_eq!(of(&second, &other), (1, 0, 1, 1));
        // The two pages are every one of them once: the log that may hold
        // flights first, though the build files' names come before its
        // own, then the build files by path.
        let second = second.into_iter().find(|f| f.id == path).unwrap();
        let told: Vec<&str> = first
            .not_read
            .iter()
            .chain(&second.not_read)
            .map(|told| told.path.as_str())
            .collect();
        let mut all = vec![logs.path().join("damaged.tlog").display().to_string()];
        all.extend((0..builds).map(|n| build(n).display().to_string()));
        assert_eq!(told, all);
        // Past the end there is nothing more, and still how many there are.
        assert_eq!(of(&from(total), &path), (0, total, total, builds as u64));
        assert_eq!(of(&from(u64::MAX), &path), (0, total, total, builds as u64));
    }

    /// A folder that cannot be listed is told with the tree it is in, and
    /// the rest of the tree is looked through. Until 2026-11-15 it was
    /// passed over without a word.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_listed_is_told_with_why() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let locked = disk.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("unseen.tlog"), log(&[(1, 5, 15)], 20)).unwrap();
        std::fs::create_dir_all(disk.path().join("open")).unwrap();
        std::fs::write(disk.path().join("open/seen.tlog"), log(&[(2, 5, 15)], 20)).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root lists any folder: nothing to tell there.
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let found = Found::new(data.path());
        let path = disk.path().display().to_string();
        add(&found, &path, Some(true));
        let flights = settled(&found, &path);
        let told = folder(&found, &path).not_read;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(flights.len(), 1, "{flights:?}");
        assert_eq!(flights[0].file, "seen.tlog");
        assert_eq!(
            told,
            [NotRead {
                path: locked.display().to_string(),
                why: "UXDuck could not list it: Permission denied (os error 13).".to_owned(),
            }]
        );
    }

    /// A tree is walked to its bottom, as an app's own folder is. Until
    /// 2026-11-15 an app's folder was looked into four folders down, and a
    /// log below that was passed over without a word.
    #[test]
    fn a_tree_is_walked_to_its_bottom_and_says_how_far_it_has_got() {
        let disk = tempfile::tempdir().unwrap();
        let deep = disk.path().join("a/b/c/d/e/f/g/h");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.tlog"), []).unwrap();
        std::fs::write(disk.path().join("top.tlog"), []).unwrap();
        std::fs::create_dir_all(disk.path().join(".hidden")).unwrap();
        std::fs::write(disk.path().join(".hidden/unseen.tlog"), []).unwrap();
        let names = |files: Vec<PathBuf>| {
            let mut names: Vec<String> = files
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let mut not_read = Vec::new();
        let (files, entries) = walk(disk.path(), true, &mut not_read, |_| true);
        assert_eq!(names(files), ["deep.tlog", "top.tlog"]);
        assert_eq!(entries, 11, "eight folders, two logs and the hidden folder");
        let (top, entries) = walk(disk.path(), false, &mut not_read, |_| true);
        assert_eq!(names(top), ["top.tlog"]);
        assert_eq!(entries, 3);
        assert_eq!(not_read, []);

        // A walk says how many entries it has read as it goes, and ends
        // when it is told to: the folder was removed meanwhile.
        let many = disk.path().join("many");
        std::fs::create_dir_all(&many).unwrap();
        for n in 0..4_000 {
            std::fs::write(many.join(format!("{n}.bin")), []).unwrap();
        }
        let mut heard = Vec::new();
        let (_, entries) = walk(&many, true, &mut not_read, |entries| {
            heard.push(entries);
            entries < 3 * TELL_EVERY
        });
        assert_eq!(heard, [1024, 2048, 3072]);
        assert_eq!(entries, 3072);
    }

    /// A tree of more entries than a walk once read is walked whole, and
    /// its folder says how many it read. Until 2026-11-15 a walk stopped
    /// at 250,000 entries. Kept to Linux, where making a quarter of a
    /// million files takes a few seconds.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_tree_of_more_entries_than_a_walk_once_read_is_walked_whole() {
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        for folder in 0..251 {
            let dir = disk.path().join(format!("{folder:03}"));
            std::fs::create_dir(&dir).unwrap();
            for file in 0..1_000 {
                File::create(dir.join(format!("{file}.bin"))).unwrap();
            }
        }
        std::fs::write(disk.path().join("last.tlog"), log(&[(1, 5, 15)], 20)).unwrap();
        let found = Found::new(data.path());
        let path = disk.path().display().to_string();
        add(&found, &path, Some(true));
        let flights = settled(&found, &path);
        let folder = folder(&found, &path);
        assert!(!folder.cut, "the walk did not stop short");
        assert_eq!(flights.len(), 1, "{flights:?}");
        let walk = folder.walk.unwrap();
        assert_eq!(walk.entries, 251 + 251_000 + 1);
        assert!(walk.took_ms.is_some(), "the walk has ended");
        assert_eq!(folder.not_read, []);
    }
}

//! Flights from other ground stations (ADR-0039 R18, S215): the tlogs
//! QGroundControl and Mission Planner keep on this computer, and folders
//! the person adds, looked through for arm edges when Flights opens and
//! each minute after, on a thread of their own. Nothing is copied, moved or
//! written: a flight replays straight from the other app's file.
//!
//! An added folder is looked into on its own, its top level only, or, with
//! all subfolders on, through its whole tree (a custom ground
//! station's folders, a disk of logs; maintainer, 2026-10-29): hidden
//! folders and links skipped, at most `MAX_ENTRIES` a look, after which the
//! look stops and the folder says so, and walked every ten minutes rather
//! than every one.
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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use uxduck_contracts::dto::replay::{
    FlightFolder, FlightFolderParams, FoundFlight, FoundFlightsResult,
};

/// How often the folders are looked through while Flights asks.
const LOOK_EVERY: Duration = Duration::from_mins(1);
/// A file written this recently is still being recorded.
const GROWING: Duration = Duration::from_secs(10);
/// How deep a folder is looked into: Mission Planner's type and system.
const DEPTH: usize = 4;
/// A folder with all subfolders on is walked this often: a disk takes a while.
const DEEP_EVERY: Duration = Duration::from_mins(10);
/// The most entries one look reads through a folder with all subfolders on.
const MAX_ENTRIES: usize = 250_000;
/// The largest file looked through.
const MAX_FILE: u64 = 2 << 30;
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
}

/// A deep folder's last walk: its files, and whether it stopped short.
#[derive(Clone, Debug)]
struct Walk {
    at: Instant,
    files: Vec<PathBuf>,
    cut: bool,
}

#[derive(Debug, Default)]
struct State {
    looked_at: Option<Instant>,
    scanning: bool,
    files: HashMap<PathBuf, Seen>,
    /// Deep folders' walks, by folder id.
    walks: HashMap<String, Walk>,
}

/// What a walk may still read; `cut` once it read all it may.
struct Budget {
    left: usize,
    cut: bool,
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

/// Every `.tlog` under a folder, a few levels deep.
fn tlogs_in(folder: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let mut budget = Budget {
        left: usize::MAX,
        cut: false,
    };
    walk(folder, Some(depth), out, &mut budget);
}

/// Every `.tlog` through a folder's whole tree, hidden folders and links
/// skipped, up to the budget. Whether the walk stopped short.
fn tlogs_under(folder: &Path, out: &mut Vec<PathBuf>) -> bool {
    let mut budget = Budget {
        left: MAX_ENTRIES,
        cut: false,
    };
    walk(folder, None, out, &mut budget);
    budget.cut
}

fn walk(folder: &Path, depth: Option<usize>, out: &mut Vec<PathBuf>, budget: &mut Budget) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        if budget.left == 0 {
            budget.cut = true;
            return;
        }
        budget.left -= 1;
        let path = entry.path();
        // Nothing hidden (a disk's `.Trashes`), nothing through a link.
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let link = entry.file_type().is_ok_and(|t| t.is_symlink());
        if hidden || link {
            continue;
        }
        if path.is_dir() {
            match depth {
                Some(0) => {}
                Some(left) => walk(&path, Some(left - 1), out, budget),
                None => walk(&path, None, out, budget),
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("tlog"))
        {
            out.push(path);
        }
    }
}

/// A tlog's flights: each vehicle's arm to disarm, by its heartbeats.
#[must_use]
pub fn flights_in(bytes: &[u8], folder: &str, path: &Path) -> Vec<FoundFlight> {
    let mut armed: HashMap<u8, (u64, String)> = HashMap::new();
    let mut flights = Vec::new();
    let mut last = 0u64;
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (stamp, frame) in uxduck_mavlink::tlog::read(bytes) {
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
    }
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
    flights
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

    /// The folders looked in or not, the known first.
    #[must_use]
    pub fn folders(&self) -> Vec<FlightFolder> {
        let choices = self.choices();
        let mut folders: Vec<FlightFolder> = known_folders()
            .into_iter()
            .map(|(id, name, path)| FlightFolder {
                on: !choices.off.contains(&id),
                exists: path.is_dir(),
                path: path.display().to_string(),
                id,
                name,
                added: false,
                deep: false,
                cut: false,
            })
            .collect();
        let walks = &self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .walks;
        for added in &choices.added {
            let path = PathBuf::from(added);
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
                cut: walks.get(added).is_some_and(|w| w.cut),
            });
        }
        folders
    }

    /// What was found, newest first; a look starts when the last is a
    /// minute old.
    #[must_use]
    pub fn found(&self) -> FoundFlightsResult {
        let folders = self.folders();
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
            scanning: state.scanning,
        }
    }

    /// Looks through the folders on a thread of its own: only files new
    /// or changed since the last look are read.
    fn look(&self, folders: Vec<FlightFolder>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .scanning = true;
        let state = self.state.clone();
        std::thread::spawn(move || {
            let mut files = Vec::new();
            for folder in folders.iter().filter(|f| f.on && f.exists) {
                let mut found = Vec::new();
                if folder.deep {
                    // A whole tree is walked every ten minutes, not every one.
                    let fresh = state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .walks
                        .get(&folder.id)
                        .filter(|w| w.at.elapsed() < DEEP_EVERY)
                        .cloned();
                    if let Some(walk) = fresh {
                        found = walk.files;
                    } else {
                        let cut = tlogs_under(Path::new(&folder.path), &mut found);
                        state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .walks
                            .insert(
                                folder.id.clone(),
                                Walk {
                                    at: Instant::now(),
                                    files: found.clone(),
                                    cut,
                                },
                            );
                    }
                } else {
                    // The apps' folders as the apps keep them; an added one
                    // at its top level only (maintainer, 2026-10-29).
                    let depth = if folder.added { 0 } else { DEPTH };
                    tlogs_in(Path::new(&folder.path), depth, &mut found);
                }
                files.extend(found.into_iter().map(|p| (folder.id.clone(), p)));
            }
            let mut fresh: HashMap<PathBuf, Seen> = HashMap::new();
            for (folder, path) in files {
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
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
                let mut seen = match known {
                    Some(seen) => seen,
                    None if size > MAX_FILE => continue,
                    None => {
                        let Ok(bytes) = std::fs::read(&path) else {
                            continue;
                        };
                        Seen {
                            size,
                            modified,
                            flights: flights_in(&bytes, &folder, &path),
                        }
                    }
                };
                for flight in &mut seen.flights {
                    flight.recording = growing;
                }
                fresh.insert(path, seen);
            }
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.files = fresh;
            state.looked_at = Some(Instant::now());
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
            // Its tree is walked afresh, now or never again.
            state.walks.remove(&params.id);
        }
        Ok(self.found())
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
        let mut flights = flights_in(
            &bytes,
            "qgroundcontrol",
            Path::new("/x/2026-10-28 14-02-17.tlog"),
        );
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
            let result = found.found();
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

    /// Waits for a look to finish and gives the added folder's flights.
    fn settled(found: &Found, folder: &str) -> Vec<FoundFlight> {
        for _ in 0..500 {
            let result = found.found();
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
        assert!(found.found().folders.iter().any(|f| f.id == path && !f.cut));
        // Off again: back to the top.
        found.change(&added(Some(false))).unwrap();
        assert_eq!(settled(&found, &path).len(), 1);
        // The apps' own folders are never walked whole.
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
}

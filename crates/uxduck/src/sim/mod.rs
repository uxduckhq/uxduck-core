//! The simulation manager (S17, ADR-0011): runs ArduPilot's SITL builds for
//! `simulator` links, as `sim_vehicle.py` would but with no Python or
//! MAVProxy, and PX4's SITL with its built-in SIH physics on macOS and
//! Linux. It finds and probes the builds, gives each vehicle a working
//! directory for its parameters and logs, and starts the build under a
//! guard (`uxduck dev sim-guard`) that ends it when the daemon's end of a
//! pipe closes, so a stopped or killed daemon leaves no process behind.
//! The data plane then connects to it: ArduPilot over TCP, PX4 over UDP.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use uxduck_contracts::dto::link::{SimAutopilot as SimAutopilotDto, SimVehicle as SimVehicleDto};
use uxduck_contracts::dto::sim::{CatalogueBuild, SimBinary};
use uxduck_core::domain::link::{JET_FRAME, Link, LinkKind, SimAutopilot, SimVehicle};

#[cfg(debug_assertions)]
mod cache;
pub mod catalogue;
mod profiles;

/// A catalogue build's description, beside its files (S18).
pub const BUILD_FILE: &str = "build.json";
/// For a build older than its embedded frame table, the table shipped
/// beside it, with its default parameter files under `defaults/`.
const FRAME_TABLE: &str = "vehicleinfo.json";

/// SITL's first TCP port; each instance adds 10 (`--instance`).
pub const BASE_PORT: u16 = 5760;

/// SITL's RC input, a UDP port; each instance adds 10.
pub const RC_IN_PORT: u16 = 5501;

/// PX4's MAVLink port for a ground station, UDP; each instance adds 1.
pub const PX4_GCS_PORT: u16 = 18570;

/// PX4's MAVLink port for an onboard computer, UDP; each instance adds 1.
const PX4_OFFBOARD_PORT: u16 = 14580;

/// Whether an instance's ports can be opened here now (S71): ArduPilot's
/// serial port over TCP and RC input over UDP, and PX4's ground station and
/// onboard ports over UDP, on every address and on loopback, as the
/// builds open them. One instance number serves either autopilot, so its
/// system id is never shared. Nothing is kept open.
#[must_use]
pub fn instance_free(instance: u8) -> bool {
    let offset = 10 * u16::from(instance);
    let tcp = BASE_PORT + offset;
    let udp = [
        RC_IN_PORT + offset,
        PX4_GCS_PORT + u16::from(instance),
        PX4_OFFBOARD_PORT + u16::from(instance),
    ];
    ["0.0.0.0", "127.0.0.1"].iter().all(|host| {
        std::net::TcpListener::bind((*host, tcp)).is_ok()
            && udp
                .iter()
                .all(|port| std::net::UdpSocket::bind((*host, *port)).is_ok())
    })
}

/// Where a running simulated vehicle's MAVLink is, on this computer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// ArduPilot's SERIAL0, a TCP server.
    Tcp(u16),
    /// PX4's ground station port. PX4 answers whoever spoke to it last,
    /// and sends to port 14550 until someone has.
    Udp(u16),
}
/// The programs the manager runs, and what they fly.
const PROGRAMS: [(&str, &[SimVehicleDto]); 6] = [
    ("arducopter", &[SimVehicleDto::Multicopter]),
    ("arducopter-heli", &[SimVehicleDto::Multicopter]),
    // S143: the jet is ArduPlane's `plane-jet` frame.
    (
        "arduplane",
        &[
            SimVehicleDto::Plane,
            SimVehicleDto::Vtol,
            SimVehicleDto::Jet,
        ],
    ),
    // S37: a rover and a boat are ArduRover's frames, a submarine ArduSub.
    ("ardurover", &[SimVehicleDto::Rover, SimVehicleDto::Boat]),
    ("ardusub", &[SimVehicleDto::Submarine]),
    ("blimp", &[SimVehicleDto::Airship]),
];
/// PX4's program: one build flies every vehicle, its airframe chosen when
/// it starts. A build is a folder with `bin/px4` and `etc/`, as PX4's own
/// build leaves it (`scripts/px4-sitl.sh`).
pub const PX4: &str = "px4";

/// How long a build has to describe itself.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The builds the daemon knows, probed once when it starts, again when a
/// catalogue build is installed or deleted, each time they are asked for
/// while none is found, and for a build of the user's own when it starts.
#[derive(Debug)]
pub struct Simulators {
    data_dir: PathBuf,
    /// Held across launches and log cleanup; released only after the guard exits.
    active: Arc<Mutex<HashSet<String>>>,
    binaries: Mutex<Vec<SimBinary>>,
    /// Set once the builds have been probed; a link reopened as the daemon
    /// starts waits for it.
    probed: std::sync::OnceLock<()>,
    /// One scan at a time: a rescan after an install waits for a slower
    /// scan already running, so the older list never lands last.
    scanning: Mutex<()>,
}

impl Simulators {
    #[must_use]
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            data_dir: data_dir.to_owned(),
            active: Arc::default(),
            binaries: Mutex::new(Vec::new()),
            probed: std::sync::OnceLock::new(),
            scanning: Mutex::new(()),
        })
    }

    /// Where catalogue builds are installed, one folder each (S18).
    #[must_use]
    pub fn catalogue_dir(&self) -> PathBuf {
        self.data_dir.join("simulators").join("catalogue")
    }

    /// Where builds are found, in order: in a development build
    /// `UXDUCK_SITL_DIR`, then the data directory's `simulators/bin/`, the
    /// catalogue's installed builds (S18), and in a development build
    /// `.cache/simulators/native/` (or `UXDUCK_SIM_CACHE_DIR`), with legacy
    /// `target/sitl/bin/` as fallback. A release reads nothing from the
    /// checkout: it downloads its builds as everyone's does. The first build
    /// of a program in the plain folders is used; every catalogue build is
    /// listed.
    fn directories(&self) -> Vec<(PathBuf, bool)> {
        let mut directories: Vec<(PathBuf, bool)> = std::env::var_os("UXDUCK_SITL_DIR")
            .filter(|_| cfg!(debug_assertions))
            .map(|dirs| std::env::split_paths(&dirs).map(|d| (d, false)).collect())
            .unwrap_or_default();
        directories.push((self.data_dir.join("simulators").join("bin"), false));
        let mut installed: Vec<PathBuf> = std::fs::read_dir(self.catalogue_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.join(BUILD_FILE).is_file())
            .collect();
        // Newest first, so the default for a vehicle is its newest version.
        installed.sort_by(|a, b| b.cmp(a));
        directories.extend(installed.into_iter().map(|d| (d, true)));
        #[cfg(debug_assertions)]
        directories.extend(cache::directories().into_iter().map(|d| (d, false)));
        directories
    }

    /// Probes every build in the directories, once; later callers wait for
    /// the first. Run off the engine's thread.
    pub fn probe_all(&self) {
        self.probed.get_or_init(|| self.probe());
    }

    /// Probes again, after a catalogue build came or went.
    pub fn rescan(&self) {
        self.probe();
        let _ = self.probed.set(());
    }

    /// Refreshes the list requested by a client, including builds copied
    /// in or removed after another program was already found.
    #[must_use]
    pub fn refresh_binaries(&self) -> Vec<SimBinary> {
        self.rescan();
        self.lock().clone()
    }

    fn probe(&self) {
        let _scanning = self.scanning.lock().unwrap_or_else(PoisonError::into_inner);
        let mut found: Vec<SimBinary> = Vec::new();
        for (directory, catalogue) in self.directories() {
            let programs = PROGRAMS.iter().map(|(program, _)| *program);
            for program in programs.chain([PX4]) {
                let Some(path) = find_program(&directory, program) else {
                    continue;
                };
                if !catalogue
                    && found
                        .iter()
                        .any(|b| b.program == program && b.catalogue_id.is_none())
                {
                    continue;
                }
                match probe(&path) {
                    Ok(binary) => found.push(binary),
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "not a SITL build");
                    }
                }
            }
        }
        tracing::info!(builds = found.len(), "found SITL builds");
        *self.lock() = found;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<SimBinary>> {
        self.binaries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What `sim/binaries` answers, once the builds are probed. With none
    /// found it looks again, so a build made or copied in after the daemon
    /// started is found without a restart; with still none, that is only a
    /// few folders read.
    #[must_use]
    pub fn binaries(&self) -> Vec<SimBinary> {
        self.probe_all();
        if self.lock().is_empty() {
            self.probe();
        }
        self.lock().clone()
    }

    /// Starts a simulator link's build under its guard. Returns the running
    /// process and where its MAVLink is.
    ///
    /// # Errors
    /// Why it cannot start, for the link's status: no build for the
    /// vehicle, a build that is not SITL, or a failed start.
    pub fn launch(&self, link: &Link) -> Result<(Running, Endpoint), String> {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active.contains(&link.id) {
            return Err("This simulation is still stopping. Try again once it has stopped.".into());
        }
        let (mut running, endpoint) = self.launch_inner(link)?;
        active.insert(link.id.clone());
        running.active = Some(ActiveSimulation {
            active: Arc::clone(&self.active),
            id: link.id.clone(),
        });
        Ok((running, endpoint))
    }

    /// Whether a build of this link's is still running or not yet reaped.
    pub fn is_active(&self, id: &str) -> bool {
        self.with_active(|active| active.contains(id))
    }

    /// Runs a storage operation while preventing a simulator from starting.
    pub fn with_active<T>(&self, operation: impl FnOnce(&HashSet<String>) -> T) -> T {
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        operation(&active)
    }

    fn launch_inner(&self, link: &Link) -> Result<(Running, Endpoint), String> {
        let LinkKind::Simulator {
            vehicle,
            autopilot,
            latitude_deg,
            longitude_deg,
            altitude_m,
            heading_deg,
            ref frame,
            speedup,
            ref binary,
            wipe,
            instance,
            // The take-off is the engine's, over MAVLink (S33).
            in_air_m: _,
            snapshot_id: _,
            // The example mission is the engine's too, once it is up (S131).
            with_mission: _,
        } = link.kind
        else {
            return Err("not a simulated vehicle".into());
        };
        let wanted = dto_vehicle(vehicle);
        // A build of the user's own is whatever it is; otherwise the first
        // build of the asked-for autopilot that flies the vehicle.
        let build = match binary {
            Some(path) => probe(Path::new(path))?,
            None => self
                .binaries()
                .into_iter()
                .find(|b| {
                    b.autopilot == dto_autopilot(autopilot)
                        && b.vehicles.contains(&wanted)
                        && frame.as_deref().is_none_or(|frame| {
                            b.frames.iter().any(|candidate| candidate == frame)
                                || frame.contains(':')
                                || launcher_base(b, frame).is_some()
                        })
                })
                .ok_or_else(|| missing(autopilot, vehicle))?,
        };
        if !build.vehicles.contains(&wanted) {
            return Err(format!("{} does not fly this vehicle", build.program));
        }
        let frame = frame
            .clone()
            .unwrap_or_else(|| default_frame(vehicle, &build));
        let instance = instance.unwrap_or(0);
        if build.program == PX4 {
            if px4_vehicle(&frame) != Some(wanted) || !build.frames.contains(&frame) {
                return Err(format!("PX4 has no {frame} for this vehicle"));
            }
            return self.launch_px4(link, &build, &frame, instance);
        }
        let launcher = launcher_base(&build, &frame);
        if !build.frames.contains(&frame) && !frame.contains(':') && launcher.is_none() {
            return Err(format!("{} has no frame {frame}", build.program));
        }
        // A model file of the user's own is whatever it is. A jet is
        // ArduPlane's jet frame, which flies as a plane (S143).
        let flies_as = if wanted == SimVehicleDto::Jet {
            SimVehicleDto::Plane
        } else {
            wanted
        };
        if !frame.contains(':')
            && ardupilot_vehicle(&build.program, launcher.unwrap_or(&frame))
                .is_some_and(|flies| flies != flies_as)
        {
            return Err(format!("{frame} is not a {} frame", vehicle_name(vehicle)));
        }
        let port = BASE_PORT + 10 * u16::from(instance);
        let directory = self.data_dir.join("simulators").join(&link.id);
        std::fs::create_dir_all(&directory)
            .map_err(|e| format!("cannot make {}: {e}", directory.display()))?;
        let (model, defaults) = model_and_defaults(&build, &frame, launcher);
        let mut args = vec![
            format!("--model={model}"),
            format!("--home={latitude_deg},{longitude_deg},{altitude_m},{heading_deg}"),
            format!("--speedup={}", speedup.unwrap_or(1)),
            format!("--instance={instance}"),
            format!("--sysid={}", instance + 1),
            // SITL waits for the daemon's connection, so start-up is the
            // same every time; the other ports would listen for nobody.
            "--serial0=tcp:0:wait".into(),
            "--serial1=none".into(),
            "--serial2=none".into(),
        ];
        if wipe {
            args.push("--wipe".into());
        }
        let defaults = profiles::with_defaults(&directory, &build.program, &frame, defaults)?;
        if let Some(defaults) = defaults {
            args.push(format!("--defaults={defaults}"));
        }
        let running = Running::start(Path::new(&build.path), &args, &directory, &[])?;
        tracing::info!(link = %link.id, program = %build.program, %frame, port, "started a simulated vehicle");
        Ok((running, Endpoint::Tcp(port)))
    }

    /// Starts PX4 with SIH, its own physics, in the link's working
    /// directory, where it keeps its parameters, mission store and flight
    /// logs. PX4 sets its system id to the instance + 1 itself. SIH always
    /// starts facing north, so the heading is not used.
    fn launch_px4(
        &self,
        link: &Link,
        build: &SimBinary,
        frame: &str,
        instance: u8,
    ) -> Result<(Running, Endpoint), String> {
        let LinkKind::Simulator {
            latitude_deg,
            longitude_deg,
            altitude_m,
            speedup,
            wipe,
            ..
        } = link.kind
        else {
            return Err("not a simulated vehicle".into());
        };
        let program = Path::new(&build.path);
        let etc = program
            .parent()
            .and_then(Path::parent)
            .map(|rootfs| rootfs.join("etc"))
            .ok_or("PX4's build has no etc folder")?;
        let directory = self.data_dir.join("simulators").join(&link.id);
        if wipe {
            for name in ["parameters.bson", "parameters_backup.bson", "dataman"] {
                let _ = std::fs::remove_file(directory.join(name));
            }
        }
        std::fs::create_dir_all(&directory)
            .map_err(|e| format!("cannot make {}: {e}", directory.display()))?;
        let root = px4_root(&directory, &link.id)?;
        let args = vec![
            // No interactive shell: nothing reads its stdin.
            "-d".to_owned(),
            "-i".into(),
            instance.to_string(),
            "-w".into(),
            root.display().to_string(),
            etc.display().to_string(),
        ];
        let mut env = vec![
            // PX4's scripts take their root from `pwd`, which gives PWD
            // when it names the working directory (POSIX).
            ("PWD".to_owned(), root.display().to_string()),
            ("PX4_SIMULATOR".into(), "sihsim".to_owned()),
            ("PX4_SIM_MODEL".into(), format!("sihsim_{frame}")),
            ("PX4_HOME_LAT".into(), latitude_deg.to_string()),
            ("PX4_HOME_LON".into(), longitude_deg.to_string()),
            ("PX4_HOME_ALT".into(), altitude_m.to_string()),
        ];
        if let Some(speedup) = speedup.filter(|s| *s > 1) {
            env.push(("PX4_SIM_SPEED_FACTOR".into(), speedup.to_string()));
        }
        let running = Running::start(program, &args, &directory, &env)?;
        let port = PX4_GCS_PORT + u16::from(instance);
        tracing::info!(link = %link.id, program = PX4, %frame, port, "started a simulated vehicle");
        Ok((running, Endpoint::Udp(port)))
    }
}

/// The path PX4 runs in. Its start-up scripts use their root unquoted
/// (`. ${R}etc/init.d/rc.vtol_defaults`), so a space in it, as in macOS's
/// `Application Support`, fails every start. Such a directory is reached
/// through a link in the temporary directory instead.
fn px4_root(directory: &Path, link_id: &str) -> Result<PathBuf, String> {
    let spaced = |path: &Path| path.to_string_lossy().contains(char::is_whitespace);
    if !spaced(directory) {
        return Ok(directory.to_owned());
    }
    #[cfg(not(unix))]
    return Err(format!(
        "PX4 runs on macOS and Linux only, not for {link_id}"
    ));
    #[cfg(unix)]
    {
        let root = std::env::temp_dir().join(format!("uxduck-px4-{link_id}"));
        if spaced(&root) {
            return Err(format!(
                "PX4 cannot run in {}: its start-up scripts fail on a path with a space, and so does the temporary directory",
                directory.display()
            ));
        }
        let _ = std::fs::remove_file(&root);
        std::os::unix::fs::symlink(directory, &root).map_err(|e| {
            format!(
                "cannot link {} to {}: {e}",
                root.display(),
                directory.display()
            )
        })?;
        Ok(root)
    }
}

fn missing(autopilot: SimAutopilot, vehicle: SimVehicle) -> String {
    if autopilot == SimAutopilot::Px4 {
        return "no PX4 build: put one in the simulators folder (scripts/px4-sitl.sh), or choose your own build".into();
    }
    let program = match vehicle {
        SimVehicle::Multicopter => "arducopter",
        SimVehicle::Plane | SimVehicle::Vtol | SimVehicle::Jet => "arduplane",
        SimVehicle::Rover | SimVehicle::Boat => "ardurover",
        SimVehicle::Submarine => "ardusub",
        SimVehicle::Airship => "blimp",
    };
    format!("no {program} build: put one in the simulators folder, or choose your own build")
}

fn vehicle_name(vehicle: SimVehicle) -> &'static str {
    match vehicle {
        SimVehicle::Multicopter => "multicopter",
        SimVehicle::Plane => "plane",
        SimVehicle::Vtol => "VTOL",
        SimVehicle::Rover => "rover",
        SimVehicle::Boat => "boat",
        SimVehicle::Submarine => "submarine",
        SimVehicle::Airship => "blimp",
        SimVehicle::Jet => "jet",
    }
}

fn dto_autopilot(autopilot: SimAutopilot) -> SimAutopilotDto {
    match autopilot {
        SimAutopilot::Ardupilot => SimAutopilotDto::Ardupilot,
        SimAutopilot::Px4 => SimAutopilotDto::Px4,
    }
}

fn dto_vehicle(vehicle: SimVehicle) -> SimVehicleDto {
    match vehicle {
        SimVehicle::Multicopter => SimVehicleDto::Multicopter,
        SimVehicle::Plane => SimVehicleDto::Plane,
        SimVehicle::Vtol => SimVehicleDto::Vtol,
        SimVehicle::Rover => SimVehicleDto::Rover,
        SimVehicle::Boat => SimVehicleDto::Boat,
        SimVehicle::Submarine => SimVehicleDto::Submarine,
        SimVehicle::Airship => SimVehicleDto::Airship,
        SimVehicle::Jet => SimVehicleDto::Jet,
    }
}

/// PX4's usual airframe per vehicle, where a build has it: a standard
/// VTOL before the tailsitter.
const PX4_DEFAULTS: [&str; 4] = ["quadx", "airplane", "standard_vtol", "rover_ackermann"];

/// The vehicle's frame when none is chosen: the build's own default, a
/// quadplane for a VTOL, a motorboat for a boat, the jet frame for a jet; for PX4, its usual
/// airframe for the vehicle, or else its first.
fn default_frame(vehicle: SimVehicle, build: &SimBinary) -> String {
    if build.program == PX4 {
        let wanted = dto_vehicle(vehicle);
        let mut frames = build
            .frames
            .iter()
            .filter(|frame| px4_vehicle(frame) == Some(wanted));
        let usual = frames
            .clone()
            .find(|frame| PX4_DEFAULTS.contains(&frame.as_str()));
        return usual
            .or_else(|| frames.next())
            .cloned()
            .unwrap_or_else(|| build.default_frame.clone());
    }
    match vehicle {
        SimVehicle::Vtol => "quadplane".into(),
        SimVehicle::Boat => "motorboat".into(),
        SimVehicle::Jet => JET_FRAME.into(),
        SimVehicle::Multicopter
        | SimVehicle::Plane
        | SimVehicle::Rover
        | SimVehicle::Submarine
        | SimVehicle::Airship => build.default_frame.clone(),
    }
}

/// A program in a folder: `arducopter`, `arducopter.exe`, or a Cygwin
/// build's `ArduCopter.elf` as ArduPilot publishes them for Windows. PX4 is
/// `px4/bin/px4` in a folder of builds, or `bin/px4` in a build's own.
fn find_program(directory: &Path, program: &str) -> Option<PathBuf> {
    if program == PX4 {
        return [directory.join("px4"), directory.to_owned()]
            .into_iter()
            .map(|rootfs| rootfs.join("bin").join(PX4))
            .find(|path| path.is_file());
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_ascii_lowercase)
                .is_some_and(|name| {
                    name == program
                        || name == format!("{program}.exe")
                        || name == format!("{program}.elf")
                })
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Runs `--list-models` and reads the frame table built into the binary.
///
/// # Errors
/// When it does not run, or does not answer as SITL does.
pub fn probe(path: &Path) -> Result<SimBinary, String> {
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name == PX4 {
        return probe_px4(path);
    }
    let (program, vehicles) = PROGRAMS
        .iter()
        .filter(|(program, _)| name.starts_with(program))
        .max_by_key(|(program, _)| program.len())
        .map(|(program, vehicles)| (*program, vehicles.to_vec()))
        .ok_or_else(|| {
            format!(
                "{} is not arducopter, arduplane, ardurover, ardusub, blimp or px4",
                path.display()
            )
        })?;
    let text = describe(path, "--list-models")?;
    let folder = path.parent().unwrap_or(Path::new("."));
    // An older build has no table inside; the catalogue ships its own.
    let (frames, default_frame) = frames(&text, program)
        .or_else(|| {
            frames(
                &std::fs::read_to_string(folder.join(FRAME_TABLE)).ok()?,
                program,
            )
        })
        .ok_or_else(|| {
            format!(
                "{} is not a SITL build with its frame table",
                path.display()
            )
        })?;
    let build: Option<CatalogueBuild> = std::fs::read(folder.join(BUILD_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    Ok(SimBinary {
        path: path.display().to_string(),
        autopilot: SimAutopilotDto::Ardupilot,
        program: program.to_owned(),
        vehicles,
        frames,
        default_frame,
        version: build.as_ref().map(|b| b.version.clone()),
        catalogue_id: build.map(|b| b.id),
    })
}

/// Runs a build with one argument and returns what it printed, or an error
/// when it does not run or does not finish in time.
fn describe(path: &Path, argument: &str) -> Result<String, String> {
    let mut child = command(path)
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", path.display()))?;
    let mut stdout = child.stdout.take().ok_or("no output")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    while child.try_wait().ok().flatten().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} did not describe itself", path.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(reader.join().unwrap_or_default())
}

/// A PX4 build: `px4 -h` shows it runs here, and its frames are the SIH
/// airframes in `etc/init.d-posix/airframes`, such as `10040_sihsim_quadx`,
/// named without the number and prefix. Gazebo's and the other
/// simulators' airframes are left out: UXDuck runs PX4 with SIH only.
fn probe_px4(path: &Path) -> Result<SimBinary, String> {
    let rootfs = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} is not in a PX4 build", path.display()))?;
    let usage = describe(path, "-h")?;
    if !usage.contains("px4") {
        return Err(format!("{} is not PX4", path.display()));
    }
    let airframes = std::fs::read_dir(rootfs.join("etc/init.d-posix/airframes"))
        .map_err(|e| format!("{} has no airframes: {e}", rootfs.display()))?;
    let mut names: Vec<String> = airframes
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    let frames = px4_frames(&names);
    let mut vehicles = Vec::new();
    for vehicle in frames.iter().filter_map(|frame| px4_vehicle(frame)) {
        if !vehicles.contains(&vehicle) {
            vehicles.push(vehicle);
        }
    }
    let default_frame = frames
        .iter()
        .find(|frame| px4_vehicle(frame) == Some(SimVehicleDto::Multicopter))
        .or_else(|| frames.first())
        .cloned()
        .ok_or_else(|| format!("{} has no SIH airframes", rootfs.display()))?;
    let build: Option<CatalogueBuild> = std::fs::read(rootfs.join(BUILD_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    // A local build's version, from `scripts/px4-sitl.sh`.
    let version = build.as_ref().map(|b| b.version.clone()).or_else(|| {
        std::fs::read_to_string(rootfs.join("version.txt"))
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    });
    Ok(SimBinary {
        path: path.display().to_string(),
        autopilot: SimAutopilotDto::Px4,
        program: PX4.to_owned(),
        vehicles,
        frames,
        default_frame,
        version,
        catalogue_id: build.map(|b| b.id),
    })
}

/// The SIH frames among PX4's airframe files, in their order: the part
/// after `sihsim_`, for files named `<number>_sihsim_<frame>` (not the
/// `.post` scripts beside some of them).
fn px4_frames(files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|name| !name.contains('.'))
        .filter_map(|name| {
            let (number, model) = name.split_once('_')?;
            let frame = model.strip_prefix("sihsim_")?;
            (number.chars().all(|c| c.is_ascii_digit()) && !frame.is_empty())
                .then(|| frame.to_owned())
        })
        .filter(|frame| px4_vehicle(frame).is_some())
        .collect()
}

/// What a PX4 SIH frame flies, from its name: `quadx`, `hex`; `airplane`;
/// `xvert`, `standard_vtol`; `rover_ackermann`. An unknown one is not
/// offered.
fn px4_vehicle(frame: &str) -> Option<SimVehicleDto> {
    if frame.starts_with("rover") {
        Some(SimVehicleDto::Rover)
    } else if frame.starts_with("boat") {
        Some(SimVehicleDto::Boat)
    } else if frame.contains("vtol") || frame == "xvert" || frame.contains("tailsitter") {
        Some(SimVehicleDto::Vtol)
    } else if frame.contains("plane") {
        Some(SimVehicleDto::Plane)
    } else if ["quad", "hex", "octo"].iter().any(|p| frame.starts_with(p)) {
        Some(SimVehicleDto::Multicopter)
    } else {
        None
    }
}

/// What an `ArduPlane` or `ArduRover` frame flies, from its name, for the
/// two vehicles each build carries: a quadplane, `FireFly6` or tailsitter
/// is a VTOL, the rest of `ArduPlane`'s a plane; a motorboat or sailboat is
/// a boat, the rest of `ArduRover`'s a rover. A frame without lift motors
/// refuses a VTOL take-off. None for any other build.
pub fn ardupilot_vehicle(program: &str, frame: &str) -> Option<SimVehicleDto> {
    match program {
        "arduplane" => Some(
            if frame.starts_with("quadplane") || frame == "firefly" || frame.contains("tailsitter")
            {
                SimVehicleDto::Vtol
            } else {
                SimVehicleDto::Plane
            },
        ),
        "ardurover" => Some(
            if frame.starts_with("motorboat") || frame.starts_with("sailboat") {
                SimVehicleDto::Boat
            } else {
                SimVehicleDto::Rover
            },
        ),
        _ => None,
    }
}

/// Frames SITL cannot fly on its own, never offered: another simulator
/// (Gazebo, `JSBSim`, `CRRCSim`, `last_letter`, SCRIMMAGE, `AirSim`, ROS), a
/// peripheral process on its CAN bus or PPP link, or a Lua motor mixer in
/// its scripts folder (docs/simulation-frames.md, "Needs"). Started anyway,
/// `jsbsim` panics and never speaks.
const NEEDS_MORE: [&str; 20] = [
    "dotriaconta_octaquad_x",
    "hexadeca-octa",
    "hexadeca-octa-cwx",
    "IrisRos",
    "gazebo-iris",
    "airsim-copter",
    "scrimmage-copter",
    "quad-can",
    "copter-PPP",
    "copter-periph-compass",
    "quadplane-can",
    "quadplane-PPP",
    "gazebo-zephyr",
    "last_letter",
    "CRRCSim",
    "jsbsim",
    "scrimmage-plane",
    "gazebo-rover",
    "airsim-rover",
    "gazebo-bluerov2",
];

/// The frames whose build is `program`, and its default, from SITL's
/// `--list-models` (ArduPilot's `vehicleinfo.json`).
fn frames(list: &str, program: &str) -> Option<(Vec<String>, String)> {
    let table: Value = serde_json::from_str(list).ok()?;
    let target = format!("bin/{program}");
    let mut frames = Vec::new();
    let mut default_frame = None;
    for vehicle in table.as_object()?.values() {
        let Some(all) = vehicle.get("frames").and_then(Value::as_object) else {
            continue;
        };
        let before = frames.len();
        for (name, frame) in all {
            if frame.get("waf_target").and_then(Value::as_str) == Some(target.as_str())
                && !NEEDS_MORE.contains(&name.as_str())
            {
                frames.push(name.clone());
            }
        }
        if frames.len() > before && default_frame.is_none() {
            default_frame = vehicle
                .get("default_frame")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    let default_frame = default_frame.or_else(|| frames.first().cloned())?;
    Some((frames, default_frame))
}

/// The launchers SITL's plane model reads from the end of a frame's name
/// (`SIM_Plane.cpp`): a catapult, a bungee, or a throw by hand. None is in
/// the frame table. Servo output 7 above 1700 µs releases the plane.
pub const LAUNCHERS: [&str; 3] = ["catapult", "bungee", "throw"];

/// The listed frame under a launcher, `plane-elevon` for
/// `plane-elevon-catapult`: only `ArduPlane`'s fixed-wing `plane` frames
/// have one; a tailsitter takes off standing up.
fn launcher_base<'a>(build: &SimBinary, frame: &'a str) -> Option<&'a str> {
    let (base, launcher) = frame.rsplit_once('-')?;
    (build.program == "arduplane"
        && LAUNCHERS.contains(&launcher)
        && base.starts_with("plane")
        && base != "plane-tailsitter"
        && build.frames.iter().any(|f| f == base))
    .then_some(base)
}

/// What a frame runs as `--model`, and the defaults to pass with it. As
/// `sim_vehicle.py` does, a frame the table gives a physics model of its
/// own runs that: `freestyle` is `X:@ROMFS/models/freestyle.json`, which
/// SITL also finds the frame's defaults by. SITL finds defaults by the
/// exact name only, so a plane on a launcher is given its listed frame's.
fn model_and_defaults(
    build: &SimBinary,
    frame: &str,
    launcher: Option<&str>,
) -> (String, Option<String>) {
    let path = Path::new(&build.path);
    let folder = path.parent().unwrap_or(Path::new("."));
    let read = |text: Option<String>| text.and_then(|t| serde_json::from_str::<Value>(&t).ok());
    // The binary's own table, or for an older build the catalogue's.
    let embedded = read(describe(path, "--list-models").ok());
    let shipped = read(std::fs::read_to_string(folder.join(FRAME_TABLE)).ok());
    let model = [&embedded, &shipped]
        .into_iter()
        .flatten()
        .find_map(|table| {
            frame_entry(table, &build.program, frame)?
                .get("model")?
                .as_str()
        })
        .unwrap_or(frame)
        .to_owned();
    let defaults = match launcher {
        Some(base) => embedded
            .as_ref()
            .and_then(|table| embedded_defaults(table, &build.program, base))
            .or_else(|| {
                let table = shipped.as_ref()?;
                shipped_defaults(table, folder, &build.program, base)
            }),
        None => shipped
            .as_ref()
            .and_then(|table| shipped_defaults(table, folder, &build.program, frame)),
    };
    (model, defaults)
}

/// A frame's default parameter files from the table built into the
/// binary, as `--defaults` takes them from its ROMFS.
fn embedded_defaults(table: &Value, program: &str, frame: &str) -> Option<String> {
    let files: Vec<String> = default_files(table, program, frame)?
        .iter()
        .map(|name| format!("@ROMFS/{name}"))
        .collect();
    (!files.is_empty()).then(|| files.join(","))
}

/// For an older build, its frame's default parameter files from the table
/// the catalogue shipped beside it, as `--defaults` takes them.
fn shipped_defaults(table: &Value, folder: &Path, program: &str, frame: &str) -> Option<String> {
    let paths: Vec<String> = default_files(table, program, frame)?
        .iter()
        .map(|name| {
            folder
                .join("defaults")
                .join(Path::new(name).file_name().unwrap_or_default())
        })
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .collect();
    (!paths.is_empty()).then(|| paths.join(","))
}

/// A frame's entry in a frame table, for this build.
fn frame_entry<'a>(table: &'a Value, program: &str, frame: &str) -> Option<&'a Value> {
    let target = format!("bin/{program}");
    table.as_object()?.values().find_map(|vehicle| {
        let entry = vehicle.get("frames")?.get(frame)?;
        (entry.get("waf_target")?.as_str()? == target).then_some(entry)
    })
}

/// A frame's default parameter files in a frame table, as named there.
fn default_files(table: &Value, program: &str, frame: &str) -> Option<Vec<String>> {
    match frame_entry(table, program, frame)?.get("default_params_filename")? {
        Value::String(one) => Some(vec![one.clone()]),
        Value::Array(many) => Some(
            many.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// A build's command: on Windows its folder goes first on `PATH`, where a
/// Cygwin build keeps `cygwin1.dll`.
fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    if cfg!(windows)
        && let Some(folder) = path.parent()
    {
        let mut paths = vec![folder.to_owned()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("PATH", joined);
        }
    }
    command
}

/// Starts a console program without a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A simulated vehicle's guard process. Dropping it closes the guard's
/// stdin, and the guard stops the build.
#[derive(Debug)]
pub struct Running {
    active: Option<ActiveSimulation>,
    guard: Option<Child>,
    stdin: Option<ChildStdin>,
    /// The build's output, for a line on why it stopped.
    log: PathBuf,
}

/// Keeps cleanup away from a process until it has actually exited.
#[derive(Debug)]
struct ActiveSimulation {
    active: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for ActiveSimulation {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl Running {
    /// `env` reaches the build through the guard; PX4 takes its airframe,
    /// place and speed that way.
    fn start(
        program: &Path,
        args: &[String],
        directory: &Path,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let me = std::env::current_exe().map_err(|e| format!("cannot find uxduck: {e}"))?;
        let log = directory.join("sitl.log");
        let mut guard = Command::new(me);
        guard
            // An airframe set by number would win over the chosen frame.
            .env_remove("PX4_SYS_AUTOSTART")
            .envs(env.iter().map(|(k, v)| (k, v)));
        // Its own process group, so Ctrl-C in the daemon's terminal does
        // not end the build before the daemon lets go of it: a build that
        // stops on its own is reported as stopped.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            guard.process_group(0);
        }
        // On Windows, a console of its own with no window, which the build
        // shares: a daemon with no console would otherwise give each one a
        // window, and Ctrl-C in a daemon's terminal does not reach it.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            guard.creation_flags(CREATE_NO_WINDOW);
        }
        let mut guard = guard
            .args(["dev", "sim-guard", "--log"])
            .arg(&log)
            .arg("--")
            .arg(program)
            .args(args)
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
        let stdin = guard.stdin.take();
        Ok(Self {
            active: None,
            guard: Some(guard),
            stdin,
            log,
        })
    }

    /// Whether the build has stopped on its own, without being asked: its
    /// guard's exit status, whose code is the build's. Checked without
    /// waiting.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        let guard = self.guard.as_mut()?;
        match guard.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "cannot check a simulator's guard");
                None
            }
        }
    }

    /// The link's words for a build that stopped on its own (see
    /// [`stopped_detail`]), with the last line of its output.
    #[must_use]
    pub fn stopped_detail(&self, started: bool, status: std::process::ExitStatus) -> String {
        stopped_detail(started, status.code(), last_line(&self.log).as_deref())
    }
}

/// The longest status detail that a line of the build's output is added to.
const STOPPED_DETAIL_MAX: usize = 160;

/// What a simulator link says when its build stopped on its own: that it
/// did not start, if it never connected, or that it stopped, with its exit
/// code and, when that keeps it to a short line, the last thing it printed.
#[must_use]
pub fn stopped_detail(started: bool, code: Option<i32>, last_line: Option<&str>) -> String {
    let what = if started {
        "The simulator stopped"
    } else {
        "The simulator did not start"
    };
    let plain = match code {
        Some(code) => format!("{what} (exit code {code})"),
        None => what.to_owned(),
    };
    match last_line.map(str::trim).filter(|line| !line.is_empty()) {
        Some(line) if plain.len() + 2 + line.len() <= STOPPED_DETAIL_MAX => {
            format!("{plain}: {line}")
        }
        _ => format!("{plain}."),
    }
}

/// The last line with something on it in a build's output, read from the
/// end of the file only.
fn last_line(log: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    const TAIL: u64 = 4096;
    let mut file = std::fs::File::open(log).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL)))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let text = String::from_utf8_lossy(&tail);
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty())?;
    Some(
        line.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
    )
}

impl Running {
    /// Freezes the build where it is, or lets it run again (`sim/pause`),
    /// through its guard: SITL's clock is its own, so a build that runs no
    /// code runs no time, and catches up at most a tenth of a second when
    /// it runs again.
    ///
    /// # Errors
    /// When the guard has gone.
    pub fn pause(&mut self, paused: bool) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "the simulator has stopped".to_owned())?;
        stdin
            .write_all(if paused { PAUSE } else { RESUME })
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("the simulator's guard has stopped: {e}"))
    }

    /// Stops the build and waits until it is gone, so another can take its
    /// ports: a simulation started again on the same link (`sim/restart`).
    /// The guard reaps the build before it exits; a guard that does not
    /// exit within a few seconds is killed.
    pub fn stop(mut self) {
        drop(self.stdin.take());
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match guard.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = guard.kill();
                    let _ = guard.wait();
                    return;
                }
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // The guard stops the build when its stdin closes, then exits; it
        // is reaped off the data plane's task.
        drop(self.stdin.take());
        if let Some(mut guard) = self.guard.take() {
            let active = self.active.take();
            std::thread::spawn(move || {
                let _ = guard.wait();
                drop(active);
            });
        }
    }
}

/// What the daemon writes to a guard's stdin to freeze its build, and to
/// let it run again.
const PAUSE: &[u8] = b"p";
const RESUME: &[u8] = b"r";

/// What a guard hears on its stdin.
enum Order {
    Pause,
    Resume,
    Closed,
}

/// `uxduck dev sim-guard`: runs a build and stops it when stdin closes,
/// which happens when the daemon lets go of it or dies; a paused build is
/// stopped all the same. [`PAUSE`] and [`RESUME`] on stdin freeze the
/// build and let it run again. Returns the exit
/// code to end with: the build's own when it stops by itself (128 plus the
/// signal when one ended it), 0 when it was stopped, 1 when it could not
/// start, with why at the end of the log.
#[must_use]
pub fn guard(log: &Path, program: &Path, args: &[String]) -> i32 {
    let output = std::fs::File::create(log).and_then(|out| Ok((out.try_clone()?, out)));
    let Ok((stdout, stderr)) = output else {
        return 1;
    };
    let mut child = match command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            if let Ok(mut out) = std::fs::OpenOptions::new().append(true).open(log) {
                let name = program.file_name().unwrap_or(program.as_os_str());
                let _ = writeln!(out, "Cannot run {}: {error}", name.to_string_lossy());
            }
            return 1;
        }
    };
    let (orders, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 64];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut buffer)
            && n > 0
        {
            for byte in &buffer[..n] {
                let order = match *byte {
                    b'p' => Order::Pause,
                    b'r' => Order::Resume,
                    _ => continue,
                };
                let _ = orders.send(order);
            }
        }
        let _ = orders.send(Order::Closed);
    });
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return exit_code(status);
        }
        match heard.recv_timeout(Duration::from_millis(200)) {
            Ok(Order::Closed) => {
                // A stopped build is killed all the same.
                let _ = child.kill();
                let _ = child.wait();
                return 0;
            }
            Ok(order @ (Order::Pause | Order::Resume)) => {
                let paused = matches!(order, Order::Pause);
                if let Err(error) = suspend(&child, paused)
                    && let Ok(mut out) = std::fs::OpenOptions::new().append(true).open(log)
                {
                    let what = if paused { "pause" } else { "resume" };
                    let _ = writeln!(out, "Cannot {what} the simulator: {error}");
                }
            }
            Err(_) => {}
        }
    }
}

/// Stops a build's process where it is, or lets it run on.
#[cfg(unix)]
fn suspend(child: &Child, paused: bool) -> std::io::Result<()> {
    use rustix::process::{Pid, Signal, kill_process};
    let signal = if paused { Signal::STOP } else { Signal::CONT };
    kill_process(Pid::from_child(child), signal).map_err(Into::into)
}

/// Suspends or resumes each thread of a build's process: Windows has no
/// signal that stops a whole process.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Win32 thread enumeration and suspension require raw handles and a sized entry"
)]
fn suspend(child: &Child, paused: bool) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
    };
    let pid = child.id();
    // SAFETY: the snapshot is checked before use; thread entry pointers refer to
    // initialized local storage with dwSize set. Only handles returned by Win32
    // are used, and every successfully opened handle is closed before returning.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(u32::MAX);
        let mut found = Thread32First(snapshot, &raw mut entry) != 0;
        let mut result = Ok(());
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    result = Err(std::io::Error::last_os_error());
                } else {
                    let count = if paused {
                        SuspendThread(thread)
                    } else {
                        ResumeThread(thread)
                    };
                    if count == u32::MAX {
                        result = Err(std::io::Error::last_os_error());
                    }
                    CloseHandle(thread);
                }
            }
            found = Thread32Next(snapshot, &raw mut entry) != 0;
        }
        CloseHandle(snapshot);
        result
    }
}

/// A build's exit status as an exit code to pass on: its own, or as a
/// shell gives it, 128 plus the signal that ended it.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn refreshing_finds_a_new_program_and_drops_a_removed_build() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("data directory");
        let bin = dir.path().join("simulators/bin");
        std::fs::create_dir_all(&bin).expect("build directory");
        let put = |program: &str| {
            let path = bin.join(program);
            let table = serde_json::json!({
                "vehicle": {
                    "default_frame": "test",
                    "frames": { "test": { "waf_target": format!("bin/{program}") } }
                }
            });
            std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{table}'\n"))
                .expect("test build");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
            path.display().to_string()
        };
        let copter = put("arducopter");
        let simulators = Simulators::new(dir.path());
        assert!(simulators.binaries().iter().any(|b| b.path == copter));
        let plane = put("arduplane");
        assert!(
            simulators
                .refresh_binaries()
                .iter()
                .any(|b| b.path == plane)
        );
        std::fs::remove_file(&plane).expect("remove test build");
        assert!(
            !simulators
                .refresh_binaries()
                .iter()
                .any(|b| b.path == plane)
        );
    }

    #[test]
    fn a_build_that_stops_by_itself_says_so_plainly() {
        assert_eq!(
            stopped_detail(false, Some(3), None),
            "The simulator did not start (exit code 3)."
        );
        assert_eq!(
            stopped_detail(true, Some(139), Some("  ")),
            "The simulator stopped (exit code 139)."
        );
        assert_eq!(stopped_detail(true, None, None), "The simulator stopped.");
        assert_eq!(
            stopped_detail(false, Some(1), Some("bind failed: Address in use ")),
            "The simulator did not start (exit code 1): bind failed: Address in use"
        );
        let too_long = "x".repeat(200);
        assert_eq!(
            stopped_detail(true, Some(1), Some(&too_long)),
            "The simulator stopped (exit code 1)."
        );

        let dir = tempfile::tempdir().expect("log directory");
        let log = dir.path().join("sitl.log");
        assert_eq!(last_line(&log), None);
        let mut text = "earlier\n".repeat(1000);
        text.push_str("Smoothing reset\tat 0\n\n  \n");
        std::fs::write(&log, text).expect("log");
        assert_eq!(last_line(&log).as_deref(), Some("Smoothing reset at 0"));
    }

    #[test]
    fn an_instance_whose_port_is_held_is_not_free() {
        // The last instance, which nothing else here is likely to use.
        let instance: u8 = 31;
        let port = BASE_PORT + 10 * u16::from(instance);
        let _held = std::net::TcpListener::bind(("0.0.0.0", port)).expect("the port is free");
        assert!(!instance_free(instance));
    }

    #[test]
    fn px4_offers_its_sih_airframes_by_vehicle() {
        let files: Vec<String> = [
            "10040_sihsim_quadx",
            "10041_sihsim_airplane",
            "10042_sihsim_xvert",
            "10043_sihsim_standard_vtol",
            "10044_sihsim_hex",
            "10045_sihsim_rover_ackermann",
            "10045_sihsim_rover_ackermann.post",
            "4001_gz_x500",
            "1100_rc_quad_x_sih.hil",
            "rc.mc_defaults",
        ]
        .map(str::to_owned)
        .to_vec();
        let frames = px4_frames(&files);
        assert_eq!(
            frames,
            [
                "quadx",
                "airplane",
                "xvert",
                "standard_vtol",
                "hex",
                "rover_ackermann"
            ]
        );
        let vehicles: Vec<_> = frames.iter().map(|f| px4_vehicle(f)).collect();
        assert_eq!(
            vehicles,
            [
                Some(SimVehicleDto::Multicopter),
                Some(SimVehicleDto::Plane),
                Some(SimVehicleDto::Vtol),
                Some(SimVehicleDto::Vtol),
                Some(SimVehicleDto::Multicopter),
                Some(SimVehicleDto::Rover),
            ]
        );
        assert_eq!(px4_vehicle("spacecraft"), None);
    }

    #[test]
    fn a_vtol_flies_only_frames_with_lift_motors() {
        let plane = |frame| ardupilot_vehicle("arduplane", frame);
        for frame in [
            "quadplane",
            "quadplane-tilt",
            "quadplane-copter_tailsitter",
            "plane-tailsitter",
            "firefly",
        ] {
            assert_eq!(plane(frame), Some(SimVehicleDto::Vtol), "{frame}");
        }
        // A jet has no lift motors: its VTOL take-off is refused (2026-10-18).
        for frame in ["plane", "plane-jet", "plane-3d", "glider"] {
            assert_eq!(plane(frame), Some(SimVehicleDto::Plane), "{frame}");
        }
        let rover = |frame| ardupilot_vehicle("ardurover", frame);
        assert_eq!(rover("motorboat-skid"), Some(SimVehicleDto::Boat));
        assert_eq!(rover("sailboat"), Some(SimVehicleDto::Boat));
        assert_eq!(rover("balancebot"), Some(SimVehicleDto::Rover));
        assert_eq!(ardupilot_vehicle("arducopter", "X"), None);
    }

    #[test]
    fn px4_defaults_to_its_first_frame_for_the_vehicle() {
        let build = SimBinary {
            path: "px4/bin/px4".into(),
            autopilot: SimAutopilotDto::Px4,
            program: PX4.into(),
            vehicles: vec![SimVehicleDto::Multicopter, SimVehicleDto::Plane],
            frames: vec![
                "quadx".into(),
                "airplane".into(),
                "xvert".into(),
                "standard_vtol".into(),
            ],
            default_frame: "quadx".into(),
            version: None,
            catalogue_id: None,
        };
        assert_eq!(default_frame(SimVehicle::Multicopter, &build), "quadx");
        assert_eq!(default_frame(SimVehicle::Plane, &build), "airplane");
        assert_eq!(default_frame(SimVehicle::Vtol, &build), "standard_vtol");
    }

    /// A jet (S143) is `ArduPlane`'s jet frame, whatever the build's own
    /// default frame is.
    #[test]
    fn a_jet_defaults_to_arduplanes_jet_frame() {
        let plane = SimBinary {
            path: "arduplane".into(),
            autopilot: SimAutopilotDto::Ardupilot,
            program: "arduplane".into(),
            vehicles: vec![
                SimVehicleDto::Plane,
                SimVehicleDto::Vtol,
                SimVehicleDto::Jet,
            ],
            frames: vec!["plane".into(), "plane-jet".into(), "quadplane".into()],
            default_frame: "plane".into(),
            version: None,
            catalogue_id: None,
        };
        assert_eq!(default_frame(SimVehicle::Jet, &plane), "plane-jet");
        assert_eq!(default_frame(SimVehicle::Plane, &plane), "plane");
        let arduplane = PROGRAMS.iter().find(|(name, _)| *name == "arduplane");
        assert!(arduplane.is_some_and(|(_, flies)| flies.contains(&SimVehicleDto::Jet)));
        assert!(missing(SimAutopilot::Ardupilot, SimVehicle::Jet).contains("arduplane"));
    }

    #[test]
    fn a_fixed_wing_plane_can_start_on_a_launcher_with_its_frames_defaults() {
        let plane = SimBinary {
            path: "arduplane".into(),
            autopilot: SimAutopilotDto::Ardupilot,
            program: "arduplane".into(),
            vehicles: vec![SimVehicleDto::Plane, SimVehicleDto::Vtol],
            frames: vec![
                "plane".into(),
                "plane-elevon".into(),
                "plane-tailsitter".into(),
                "quadplane".into(),
                "glider".into(),
            ],
            default_frame: "plane".into(),
            version: None,
            catalogue_id: None,
        };
        assert_eq!(launcher_base(&plane, "plane-catapult"), Some("plane"));
        assert_eq!(
            launcher_base(&plane, "plane-elevon-bungee"),
            Some("plane-elevon")
        );
        assert_eq!(launcher_base(&plane, "plane-throw"), Some("plane"));
        assert_eq!(launcher_base(&plane, "plane-elevon"), None);
        assert_eq!(launcher_base(&plane, "plane-tailsitter-catapult"), None);
        assert_eq!(launcher_base(&plane, "quadplane-catapult"), None);
        assert_eq!(launcher_base(&plane, "glider-catapult"), None);
        assert_eq!(launcher_base(&plane, "plane-vtail-catapult"), None);
        let copter = SimBinary {
            program: "arducopter".into(),
            frames: vec!["plane".into()],
            ..plane
        };
        assert_eq!(launcher_base(&copter, "plane-catapult"), None);

        let table = serde_json::json!({ "ArduPlane": { "frames": {
            "plane": { "waf_target": "bin/arduplane", "default_params_filename": "models/plane.parm" },
            "plane-elevon": { "waf_target": "bin/arduplane", "default_params_filename":
                ["models/plane.parm", "default_params/plane-elevons.parm"] }
        } } });
        assert_eq!(
            default_files(&table, "arduplane", "plane-elevon"),
            Some(vec![
                "models/plane.parm".into(),
                "default_params/plane-elevons.parm".into()
            ])
        );
        assert_eq!(default_files(&table, "arducopter", "plane"), None);
    }

    #[test]
    fn a_frame_with_its_own_physics_model_runs_that_model() {
        let table = serde_json::json!({ "ArduCopter": { "frames": {
            "X": { "waf_target": "bin/arducopter" },
            "freestyle": { "waf_target": "bin/arducopter",
                "model": "X:@ROMFS/models/freestyle.json" }
        } } });
        let model = |frame| {
            frame_entry(&table, "arducopter", frame)
                .and_then(|entry| entry.get("model"))
                .and_then(Value::as_str)
        };
        assert_eq!(model("freestyle"), Some("X:@ROMFS/models/freestyle.json"));
        assert_eq!(model("X"), None);
    }

    #[test]
    fn a_build_lists_its_own_frames_and_default() {
        let list = r#"{
            "ArduCopter": { "default_frame": "quad", "frames": {
                "+": { "waf_target": "bin/arducopter" },
                "quad": { "model": "+", "waf_target": "bin/arducopter" },
                "hexax": { "waf_target": "bin/arducopter" }
            } },
            "Helicopter": { "default_frame": "heli", "frames": {
                "heli": { "waf_target": "bin/arducopter-heli" }
            } },
            "ArduPlane": { "default_frame": "plane", "frames": {
                "plane": { "waf_target": "bin/arduplane" },
                "quadplane": { "waf_target": "bin/arduplane" },
                "jsbsim": { "waf_target": "bin/arduplane" },
                "quadplane-can": { "waf_target": "bin/arduplane" }
            } }
        }"#;
        let (copter, default) = frames(list, "arducopter").unwrap_or_default();
        assert_eq!(copter, ["+", "hexax", "quad"]);
        assert_eq!(default, "quad");
        let (plane, default) = frames(list, "arduplane").unwrap_or_default();
        // Not those that need another simulator or process.
        assert_eq!(plane, ["plane", "quadplane"]);
        assert_eq!(default, "plane");
        assert!(frames(list, "ardusub").is_none());
        assert!(frames("not json", "arducopter").is_none());
    }
}

#[cfg(all(test, unix))]
mod active_storage_tests {
    use super::*;

    #[test]
    fn dropped_simulator_stays_protected_until_its_process_exits() {
        let active = Arc::new(Mutex::new(HashSet::from(["simulation".into()])));
        let mut guard = Command::new("sh")
            .args(["-c", "cat >/dev/null; sleep 0.2"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = guard.stdin.take();
        let running = Running {
            guard: Some(guard),
            stdin,
            log: PathBuf::new(),
            active: Some(ActiveSimulation {
                active: Arc::clone(&active),
                id: "simulation".into(),
            }),
        };
        drop(running);
        assert!(
            active.lock().unwrap().contains("simulation"),
            "closing stdin does not mean the process has exited"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while active.lock().unwrap().contains("simulation") {
            assert!(
                std::time::Instant::now() < deadline,
                "process exit must release log protection"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

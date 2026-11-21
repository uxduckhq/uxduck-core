//! The engine's `System` port on a real machine: port checks, local
//! addresses, serial ports and the wake lock (ADR-0004).

use std::process::{Child, Command, Stdio};

use uxduck_engine::runtime::{PowerSaving, SerialPortInfo, System};

use crate::ports;

/// The wake lock is the platform's own assertion, which ends with the
/// daemon: on macOS a thread holds a user-initiated, latency-critical
/// activity, so the system stays awake and the daemon is neither napped nor
/// has its timers coalesced (FB-254), on Linux a child process holds
/// `systemd-inhibit` around `tail --pid`, on Windows a thread holds
/// `SetThreadExecutionState` and keeps the daemon out of power throttling. The display is held the same way while a
/// vehicle is armed: `caffeinate -d` on macOS, `ES_DISPLAY_REQUIRED` on
/// Windows; Linux desktops keep their own screen lock.
#[derive(Debug)]
pub struct Host {
    wake: Option<Hold>,
    display: Option<Hold>,
    /// power-profiles-daemon's Power Saver profile is on, for each of its
    /// names (FB-254).
    #[cfg(target_os = "linux")]
    power_saver: std::sync::Arc<[std::sync::atomic::AtomicBool; 2]>,
    data_dir: std::path::PathBuf,
    simulators: std::sync::Arc<crate::sim::Simulators>,
    catalogue: std::sync::Arc<crate::sim::catalogue::Catalogue>,
    desktop_app: std::sync::Arc<crate::desktop_app::DesktopApp>,
    /// A card's logs being copied (S212).
    card_copying: crate::card_logs::Copying,
    /// Flights in other apps' tlogs (S215).
    found_flights: crate::found_flights::Found,
    /// Where work on a replay's log goes to be done off the engine's task
    /// ([`crate::replay::work`]).
    replay_work: tokio::sync::mpsc::UnboundedSender<uxduck_engine::runtime::ReplayWork>,
    /// Where a camera's definition file is fetched from an address, off
    /// the engine's task ([`crate::camera_definitions::work`]).
    camera_definitions: tokio::sync::mpsc::UnboundedSender<crate::camera_definitions::Fetch>,
    /// Where flights to write to a disk go (ADR-0062,
    /// [`crate::flight_file::work`]).
    offload: tokio::sync::mpsc::UnboundedSender<uxduck_engine::runtime::OffloadJob>,
    /// Where attached pictures go to be made into the AI model's JPEGs
    /// (FB-248, [`crate::pictures::work`]).
    pictures: tokio::sync::mpsc::UnboundedSender<(
        uxduck_engine::runtime::PictureOrder,
        std::path::PathBuf,
    )>,
}

impl Host {
    #[must_use]
    #[allow(clippy::too_many_arguments, reason = "the daemon's work channels")]
    pub fn new(
        data_dir: &std::path::Path,
        simulators: std::sync::Arc<crate::sim::Simulators>,
        catalogue: std::sync::Arc<crate::sim::catalogue::Catalogue>,
        desktop_app: std::sync::Arc<crate::desktop_app::DesktopApp>,
        replay_work: tokio::sync::mpsc::UnboundedSender<uxduck_engine::runtime::ReplayWork>,
        camera_definitions: tokio::sync::mpsc::UnboundedSender<crate::camera_definitions::Fetch>,
        offload: tokio::sync::mpsc::UnboundedSender<uxduck_engine::runtime::OffloadJob>,
        pictures: tokio::sync::mpsc::UnboundedSender<(
            uxduck_engine::runtime::PictureOrder,
            std::path::PathBuf,
        )>,
    ) -> Self {
        Self {
            wake: None,
            display: None,
            #[cfg(target_os = "linux")]
            power_saver: watch_power_saver(),
            data_dir: data_dir.to_owned(),
            simulators,
            catalogue,
            desktop_app,
            card_copying: crate::card_logs::Copying::default(),
            found_flights: crate::found_flights::Found::new(data_dir),
            replay_work,
            camera_definitions,
            offload,
            pictures,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.display_awake(false);
        self.wake_lock(false);
    }
}

/// An assertion held until released.
#[derive(Debug)]
enum Hold {
    Child(Child),
    /// The thread that holds the execution state; dropping the sender ends it.
    #[cfg(windows)]
    Thread(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>),
    /// The thread that holds the activity; dropping the sender ends it, and
    /// it has ended once the thread has.
    #[cfg(target_os = "macos")]
    Activity(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>),
}

impl Hold {
    fn release(self) {
        match self {
            Self::Child(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            #[cfg(windows)]
            Self::Thread(stop, thread) => {
                drop(stop);
                let _ = thread.join();
            }
            #[cfg(target_os = "macos")]
            Self::Activity(stop, thread) => {
                drop(stop);
                let _ = thread.join();
            }
        }
    }
}

/// What the activity says it is for, in `pmset -g assertions` and
/// Activity Monitor.
#[cfg(target_os = "macos")]
const ACTIVITY_REASON: &str = "A vehicle is connected to UXDuck";

/// Holds a user-initiated, latency-critical activity until the returned
/// hold is released: macOS then keeps the system from idle sleep and does
/// not nap the daemon or coalesce its timers, which heartbeats, links and
/// controllers need (FB-254). The activity stays on the thread that began
/// it, and this returns once it has begun.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "objc2 marks NSProcessInfo's endActivity unsafe")]
fn hold_activity() -> std::io::Result<Hold> {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let (begun, wait_begun) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("wake lock".into())
        .spawn(move || {
            let info = NSProcessInfo::processInfo();
            let activity = info.beginActivityWithOptions_reason(
                NSActivityOptions::UserInteractive,
                &NSString::from_str(ACTIVITY_REASON),
            );
            let _ = begun.send(());
            let _ = stopped.recv();
            // SAFETY: ends the activity this thread began, once.
            unsafe { info.endActivity(&activity) };
        })?;
    if wait_begun.recv().is_err() {
        let _ = thread.join();
        return Err(std::io::Error::other(
            "the wake lock's thread stopped before it began the activity",
        ));
    }
    Ok(Hold::Activity(stop, thread))
}

/// Holds `flags` on a thread of its own until it is released, and has let
/// go once released: `SetThreadExecutionState` lasts while its thread does. With
/// `full_speed`, the daemon is also kept out of power throttling meanwhile.
#[cfg(windows)]
#[allow(unsafe_code, reason = "SetThreadExecutionState is a plain Win32 call")]
fn hold_execution_state(flags: u32, full_speed: bool) -> Hold {
    use windows_sys::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        // SAFETY: sets and clears this thread's own execution state.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | flags) };
        if full_speed {
            power_throttling(false);
        }
        let _ = stopped.recv();
        if full_speed {
            power_throttling(true);
        }
        // SAFETY: as above.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    });
    Hold::Thread(stop, thread)
}

/// Whether Windows may throttle the daemon. Unless told otherwise, it runs
/// a process with no visible window as `EcoQoS` on battery: at the CPU's most
/// efficient frequency, on its efficient cores (FB-254). Naming execution
/// speed in the control mask and leaving it off in the state mask asks for
/// full speed, and an empty control mask hands the choice back to Windows.
/// Timer resolution is left alone: the daemon never raises it.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "SetProcessInformation is a plain Win32 call on a structure on the stack"
)]
#[allow(clippy::cast_possible_truncation, reason = "the structure is 12 bytes")]
fn power_throttling(allowed: bool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: if allowed {
            0
        } else {
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED
        },
        StateMask: 0,
    };
    // SAFETY: the structure outlives the call, which only reads it.
    let set = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if set == 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            allowed,
            "could not change whether Windows may slow UXDuck down"
        );
    }
}

/// Whether Windows energy saver is on, battery saver before Windows 11
/// 24H2: `GetSystemPowerStatus` says so in `SystemStatusFlag`.
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "GetSystemPowerStatus fills a structure on the stack"
)]
fn energy_saver() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: fills the structure it is given, which outlives the call.
    let read = unsafe { GetSystemPowerStatus(&raw mut status) } != 0;
    read && status.SystemStatusFlag == 1
}

/// Starts `command` as a hold, quietly.
fn spawn_hold(mut command: Command) -> std::io::Result<Hold> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Hold::Child)
}

/// Where Move or copy writes for `folder` (ADR-0062): a Log folder as it
/// is, a disk's `UXDuck Flights` folder, or any other folder, which then
/// becomes a Log folder. It must be there and writable.
fn offload_target(
    found: &crate::found_flights::Found,
    folder: &str,
) -> Result<uxduck_engine::runtime::OffloadTarget, String> {
    use std::path::{Path, PathBuf};
    let given = Path::new(folder);
    if folder.trim().is_empty() || !given.is_absolute() {
        return Err("Choose a folder by its whole path.".into());
    }
    let folders = found.folders(None);
    let added = |path: &Path| {
        folders
            .iter()
            .find(|f| f.added && Path::new(&f.path) == path)
    };
    // A disk by where it is mounted, or by the folder `flights/found` names
    // for it, which may not be made yet.
    let disk = crate::found_flights::disks(&folders)
        .into_iter()
        .find(|d| Path::new(&d.path) == given || Path::new(&d.folder) == given);
    let (target, deep, on_disk) = match (added(given), disk) {
        (Some(f), _) => (PathBuf::from(&f.path), f.deep, false),
        (None, Some(disk)) => (PathBuf::from(&disk.folder), true, true),
        (None, None) => (given.to_owned(), false, false),
    };
    let log_folder = added(&target).is_some();
    let place = uxduck_contracts::dto::flight::place_of_folder(&target.display().to_string());
    // A disk's own folder is made when first written, on the disk plugged in.
    let there = if target.is_dir() {
        target.clone()
    } else if let Some(volume) = on_disk.then(|| target.parent()).flatten()
        && !target.exists()
    {
        volume.to_owned()
    } else if log_folder {
        return Err(format!(
            "{place} is not plugged in. Plug it in, or choose another folder."
        ));
    } else {
        return Err(format!(
            "{} is not a folder on this computer.",
            target.display()
        ));
    };
    let probe = there.join(format!(".uxduck-write-check-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::ReadOnlyFilesystem {
                // macOS reads NTFS disks but never writes to them.
                format!(
                    "{place} can only be read, so nothing can be written to it. A Mac reads a disk formatted as NTFS but cannot write to it."
                )
            } else {
                format!("UXDuck cannot write to {}: {e}.", there.display())
            }
        })?;
    let names = std::fs::read_dir(&target)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .filter(|n| n.ends_with(".uxflight"))
        .collect();
    Ok(uxduck_engine::runtime::OffloadTarget {
        folder: target.display().to_string(),
        free_bytes: fs4::available_space(&there).ok(),
        log_folder,
        deep,
        names,
        max_file: max_file(&there),
    })
}

/// The largest file the disk at `place` keeps, and its format: FAT32,
/// which many external disks come formatted as, keeps files of 4 GB less
/// a byte, a bound of its own format. None for formats without such a
/// bound (exFAT, APFS, NTFS, ext4), and where the format cannot be read:
/// on Windows a FAT32 disk refuses the file as it is written instead.
fn max_file(place: &std::path::Path) -> Option<(u64, String)> {
    const FAT_LARGEST: u64 = u32::MAX as u64;
    #[cfg(target_os = "linux")]
    {
        let fs = rustix::fs::statfs(place).ok()?;
        // MSDOS_SUPER_MAGIC, for vfat and msdos alike.
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            clippy::unnecessary_cast,
            reason = "the kernel's magic number, whatever the field's type"
        )]
        let magic = fs.f_type as u64;
        return (magic == 0x4d44).then(|| (FAT_LARGEST, "FAT32".to_owned()));
    }
    #[cfg(target_os = "macos")]
    {
        let fs = rustix::fs::statfs(place).ok()?;
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "a C string's bytes"
        )]
        let format: String = fs
            .f_fstypename
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| char::from(*c as u8))
            .collect();
        return (format == "msdos").then(|| (FAT_LARGEST, "FAT32".to_owned()));
    }
    #[allow(unreachable_code, reason = "Linux and macOS return above")]
    {
        let _ = (place, FAT_LARGEST);
        None
    }
}

/// The wake lock: the system stays awake, the display may sleep.
fn wake_hold() -> Option<std::io::Result<Hold>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::ES_SYSTEM_REQUIRED;
        return Some(Ok(hold_execution_state(ES_SYSTEM_REQUIRED, true)));
    }
    #[cfg(target_os = "macos")]
    return Some(hold_activity());
    #[allow(unreachable_code, reason = "Windows and macOS return above")]
    wake_command().map(spawn_hold)
}

/// The display stays on.
fn display_hold() -> Option<std::io::Result<Hold>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
        return Some(Ok(hold_execution_state(
            ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
            false,
        )));
    }
    #[allow(unreachable_code, reason = "Windows returns above")]
    if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        command.args(["-d", "-w", &std::process::id().to_string()]);
        Some(spawn_hold(command))
    } else {
        None
    }
}

/// Whether power-profiles-daemon's Power Saver profile is on, one flag for
/// each of its D-Bus names, kept up to date by a thread that is told when
/// the profile changes and when the service comes or goes (FB-254), so it
/// never wakes to ask. Version 0.20 and later answer on both names, earlier
/// ones only on the second. Where nothing offers power profiles, both stay
/// off until something does.
#[cfg(target_os = "linux")]
fn watch_power_saver() -> std::sync::Arc<[std::sync::atomic::AtomicBool; 2]> {
    use std::sync::atomic::AtomicBool;

    const SERVICES: [(&str, &str); 2] = [
        (
            "org.freedesktop.UPower.PowerProfiles",
            "/org/freedesktop/UPower/PowerProfiles",
        ),
        ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
    ];
    let saving = std::sync::Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
    let kept = saving.clone();
    let watch = move || {
        futures::executor::block_on(async {
            let Ok(connection) = zbus::Connection::system().await else {
                tracing::debug!("no system bus, so no power profiles");
                return;
            };
            let watches = SERVICES
                .iter()
                .zip(kept.iter())
                .map(|(&(name, path), on)| watch_profile(&connection, name, path, on));
            futures::future::join_all(watches).await;
        });
    };
    if let Err(error) = std::thread::Builder::new()
        .name("power profile".into())
        .spawn(watch)
    {
        tracing::warn!(%error, "could not watch the power profile");
    }
    saving
}

/// Follows one power-profiles-daemon name into `on`. Its profile is read
/// at the start, when the service says it changed and when a service takes
/// the name, since a restarted one announces no change. It is off while
/// nobody holds the name, and after the bus connection drops. Nothing is
/// cached, so nothing read goes stale.
#[cfg(target_os = "linux")]
async fn watch_profile(
    connection: &zbus::Connection,
    name: &str,
    path: &str,
    on: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;

    use futures::StreamExt as _;

    enum Heard {
        Owner(bool),
        Changed,
    }
    let Ok(interface) = zbus::names::InterfaceName::try_from(name) else {
        return;
    };
    let built = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(name)
        .path(path)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await;
    let Ok(properties) = built else {
        tracing::debug!(service = name, "cannot ask about power profiles");
        return;
    };
    let (Ok(owners), Ok(changes)) = (
        properties.inner().receive_owner_changed().await,
        properties.receive_properties_changed().await,
    ) else {
        tracing::debug!(service = name, "cannot follow power profiles");
        return;
    };
    // Never starts the service: a name nobody holds is not D-Bus-activated,
    // so a power-profiles-daemon the person stopped stays stopped.
    let saver = async || {
        properties
            .inner()
            .call_with_flags::<_, _, zbus::zvariant::OwnedVariant>(
                "Get",
                zbus::proxy::MethodFlags::NO_AUTO_START,
                &(interface.clone(), "ActiveProfile"),
            )
            .await
            .ok()
            .flatten()
            .and_then(|profile| String::try_from(profile).ok())
            .is_some_and(|profile| profile == "power-saver")
    };
    on.store(saver().await, Ordering::Relaxed);
    let heard = futures::stream::select(
        owners.map(|owner| Heard::Owner(owner.is_some())),
        changes.map(|_| Heard::Changed),
    );
    let mut heard = std::pin::pin!(heard);
    while let Some(event) = heard.next().await {
        let saving = match event {
            Heard::Owner(false) => false,
            Heard::Owner(true) | Heard::Changed => saver().await,
        };
        on.store(saving, Ordering::Relaxed);
    }
    on.store(false, Ordering::Relaxed);
}

fn wake_command() -> Option<Command> {
    let pid = std::process::id().to_string();
    if cfg!(target_os = "linux") {
        let mut command = Command::new("systemd-inhibit");
        command.args([
            "--what=idle:sleep",
            "--who=UXDuck",
            "--why=A vehicle is connected to UXDuck",
            "--mode=block",
            "tail",
            &format!("--pid={pid}"),
            "-f",
            "/dev/null",
        ]);
        Some(command)
    } else {
        None
    }
}

impl System for Host {
    fn process_usage(&self) -> Option<uxduck_contracts::dto::settings::CoreProcessUsage> {
        super::process_usage::snapshot(&self.simulators.guard_pids())
    }

    fn start_at_login(
        &mut self,
        on: bool,
    ) -> Result<uxduck_contracts::dto::settings::LoginItem, String> {
        crate::adapters::login_item::set(on)
    }

    fn wakeups(&self) -> (u64, Vec<(String, u64)>) {
        crate::wakes::snapshot()
    }

    fn controller_inputs(&self) -> uxduck_contracts::dto::controller::ControllerInputsResult {
        crate::daemon::controllers::inputs()
    }

    fn mavlink_inspect(
        &self,
        vehicle_ids: &[String],
    ) -> uxduck_contracts::dto::inspect::MavlinkInspectResult {
        use uxduck_engine::runtime::Clock as _;
        crate::inspect::snapshot(vehicle_ids, super::system::SystemClock.now_us())
    }

    fn vehicle_heartbeat_us(&self, vehicle_ids: &[String]) -> Option<u64> {
        crate::inspect::heartbeat_us(vehicle_ids)
    }

    fn replay_read(
        &mut self,
        what: &uxduck_engine::runtime::ReplayRead,
    ) -> Result<Vec<u8>, String> {
        crate::replay::read(&self.data_dir.join("tlogs"), what)
    }

    fn replay_available(&self, what: &uxduck_engine::runtime::ReplayRead) -> Option<bool> {
        match what {
            uxduck_engine::runtime::ReplayRead::Packed { path, .. } => {
                Some(std::path::Path::new(path).is_file())
            }
            _ => None,
        }
    }

    fn file_present(&self, path: &str) -> Option<bool> {
        Some(std::path::Path::new(path).is_file())
    }

    fn telemetry_bytes(&self, read: &uxduck_engine::runtime::ReplayRead) -> Option<(u64, u64)> {
        crate::replay::telemetry_bytes(&self.data_dir.join("tlogs"), read)
    }

    fn offload_target(
        &self,
        folder: &str,
    ) -> Result<uxduck_engine::runtime::OffloadTarget, String> {
        offload_target(&self.found_flights, folder)
    }

    fn offload(&mut self, job: uxduck_engine::runtime::OffloadJob) -> Result<(), String> {
        let folder = std::path::Path::new(&job.folder);
        std::fs::create_dir_all(folder)
            .map_err(|e| format!("UXDuck could not make {}: {e}.", folder.display()))?;
        if let Some(deep) = job.add_folder {
            self.found_flights
                .change(&uxduck_contracts::dto::replay::FlightFolderParams {
                    id: job.folder.clone(),
                    on: Some(true),
                    remove: false,
                    deep: deep.then_some(true),
                    logbook: None,
                })?;
        }
        self.offload
            .send(job)
            .map_err(|_| "UXDuck is stopping. Try again once it has started.".to_owned())
    }

    fn flight_file(&mut self, path: &str) -> Result<uxduck_engine::runtime::FlightFile, String> {
        let file = std::path::Path::new(path);
        crate::flight_file::manifest(file)
            .map_err(|why| format!("UXDuck could not open {}: {why}", file.display()))
    }

    fn found_flights(
        &mut self,
        params: &uxduck_contracts::dto::replay::FoundFlightsParams,
    ) -> Result<uxduck_contracts::dto::replay::FoundFlightsResult, String> {
        Ok(self.found_flights.found(params))
    }

    fn flight_folder(
        &mut self,
        params: &uxduck_contracts::dto::replay::FlightFolderParams,
    ) -> Result<uxduck_contracts::dto::replay::FoundFlightsResult, String> {
        self.found_flights.change(params)
    }

    fn reveal_flight_folder(&mut self, id: &str) -> Result<String, String> {
        let folder = self
            .found_flights
            .folders(None)
            .into_iter()
            .find(|f| f.id == id)
            .ok_or("UXDuck does not look in that folder.")?;
        let path = std::path::Path::new(&folder.path);
        if !path.is_dir() {
            return Err(format!("{} is not on this computer.", folder.path));
        }
        super::browser::reveal(path, None)
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(folder.path)
    }

    fn probe_video(&mut self, path: &str) -> Result<uxduck_engine::runtime::VideoProbe, String> {
        let path = std::path::Path::new(path);
        let size = std::fs::metadata(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?
            .len();
        let probed = crate::video::created::probe(path)?;
        // A copy the browser can play, made now rather than when first
        // played (S208): a long MPEG-2 file takes a while.
        if probed.play != crate::video::created::Play::Direct
            && let Some(hub) = crate::video::hub()
        {
            let cache = crate::video::playback::cache_dir(hub.dir());
            crate::video::playback::prepare_soon(&cache, path, probed.play);
        }
        Ok(uxduck_engine::runtime::VideoProbe {
            created_us: probed.created_us,
            duration_us: probed.duration_us,
            klv_start_us: probed.klv_start_us,
            codec: probed.codec,
            bytes: size,
        })
    }

    fn attachment_write(
        &mut self,
        id: &str,
        _name: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        crate::attachments::write(&self.data_dir.join("attachments"), id, offset, bytes)
    }

    fn attachment_finish(&mut self, id: &str, name: &str, keep: bool) -> Result<(), String> {
        crate::attachments::finish(&self.data_dir.join("attachments"), id, name, keep)
    }

    fn attachment_read(&mut self, id: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        crate::attachments::read(&self.data_dir.join("attachments"), id, offset, len)
    }

    fn attachment_path(&self, id: &str) -> Result<String, String> {
        crate::attachments::path(&self.data_dir.join("attachments"), id)
    }

    fn attachment_picture(
        &mut self,
        order: uxduck_engine::runtime::PictureOrder,
    ) -> Result<(), String> {
        let path = crate::attachments::path(&self.data_dir.join("attachments"), &order.attachment)?;
        self.pictures
            .send((order, path.into()))
            .map_err(|_| "UXDuck is stopping.".to_owned())
    }

    fn own_model_keep(&mut self, file: &str, bytes: &[u8]) -> Result<(), String> {
        crate::own_models::keep(&self.data_dir.join("vehicle-models"), file, bytes)
    }

    fn own_model_read(&mut self, file: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        crate::own_models::read(&self.data_dir.join("vehicle-models"), file, offset, len)
    }

    fn own_model_remove(&mut self, file: &str) -> Result<(), String> {
        crate::own_models::remove(&self.data_dir.join("vehicle-models"), file)
    }

    fn camera_definition_kept(&mut self, key: &str, version: u16) -> Option<Vec<u8>> {
        crate::camera_definitions::kept(&self.data_dir.join("camera-definitions"), key, version)
    }

    fn camera_definition_keep(&mut self, key: &str, version: u16, bytes: &[u8]) {
        let root = self.data_dir.join("camera-definitions");
        if let Err(error) = crate::camera_definitions::keep(&root, key, version, bytes) {
            tracing::warn!(%error, "cannot keep a camera's definition file");
        }
    }

    fn camera_definition_fetch(&mut self, camera_id: &str, uri: &str) -> Result<(), String> {
        self.camera_definitions
            .send(crate::camera_definitions::Fetch {
                camera_id: camera_id.to_owned(),
                uri: uri.to_owned(),
            })
            .map_err(|_| "UXDuck is stopping.".to_owned())
    }

    fn keep_log(&mut self, recording: &str, bytes: &[u8]) -> Result<(), String> {
        let mut spool = crate::spool::Spool::open(&self.data_dir, self.data_dir.join("tlogs"))
            .map_err(|e| e.to_string())?;
        spool
            .keep_bytes(recording, bytes)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn log_cards(&mut self) -> Result<uxduck_contracts::dto::replay::LogCardsResult, String> {
        Ok(crate::card_logs::status(&self.data_dir, &self.card_copying))
    }

    fn copy_card_logs(&mut self, card: &str) -> Result<(), String> {
        use uxduck_engine::runtime::Clock as _;
        let today = crate::analytics::utc_day(super::system::SystemClock.now_us());
        self.card_copying.start(&self.data_dir, card, &today)
    }

    fn replay_clock(&mut self, running: bool) {
        crate::replay::running(running);
    }

    fn replay_work(
        &mut self,
        work: uxduck_engine::runtime::ReplayWork,
    ) -> Option<uxduck_engine::runtime::ReplayWorked> {
        // Nobody takes it once the daemon is stopping: done here then.
        match self.replay_work.send(work) {
            Ok(()) => None,
            Err(back) => Some(back.0.run()),
        }
    }

    fn tlog_position(&self, link_id: &str) -> Option<(String, u64)> {
        crate::tlog_index::position(link_id)
    }

    fn link_logs(&self, link_id: &str) -> Vec<uxduck_engine::runtime::LinkLog> {
        crate::replay::link_logs(&self.data_dir.join("tlogs"), link_id)
    }

    fn remove_link_log(&mut self, link_id: &str, name: &str) -> Result<(), String> {
        crate::replay::remove_link_log(&self.data_dir.join("tlogs"), link_id, name)
    }

    fn mavlink_history(
        &self,
        vehicle: &uxduck_engine::runtime::HistoryOf<'_>,
        params: &uxduck_contracts::dto::inspect::MavlinkHistoryParams,
    ) -> Result<
        uxduck_contracts::dto::inspect::MavlinkHistoryResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::inspect::history(
            &self.data_dir.join("tlogs"),
            vehicle.vehicle_ids,
            vehicle.link_id,
            vehicle.system_id,
            params,
        )
    }

    fn transport_diagnostics(
        &self,
        link: &str,
    ) -> Option<uxduck_contracts::dto::diagnostics::TransportDiagnostics> {
        crate::diagnostics::transport_of(link)
    }

    fn last_router_incident(
        &self,
        link: &str,
    ) -> Option<uxduck_contracts::dto::diagnostics::RouterEvent> {
        crate::diagnostics::last_incident(link)
    }
    fn packet_history(
        &self,
        link: &str,
        system: Option<u8>,
        gcs_system_id: u8,
        params: &uxduck_contracts::dto::inspect::MavlinkHistoryParams,
        now: u64,
    ) -> Result<
        uxduck_contracts::dto::inspect::MavlinkHistoryResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::diagnostics::history(
            &self.data_dir.join("tlogs"),
            link,
            (system, gcs_system_id),
            params,
            now,
        )
    }
    fn router_events(
        &self,
        params: &uxduck_contracts::dto::diagnostics::RouterEventsParams,
        now: u64,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::RouterEventsResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::diagnostics::events(params, now)
    }
    fn socket_inspect(
        &self,
        params: &uxduck_contracts::dto::diagnostics::SocketInspectParams,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::SocketInspectResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::ports::inspect(params)
    }
    fn daemon_logs(
        &self,
        params: &uxduck_contracts::dto::diagnostics::DaemonLogsParams,
        now: u64,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::DaemonLogsResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::logs::query(params, now)
    }
    fn check_udp_bind(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(), uxduck_core::domain::link_failure::BindRefused> {
        ports::check_udp_bind(host, port)
    }

    fn sim_instance_free(&self, instance: u8) -> bool {
        crate::sim::claim_instance(instance)
    }

    fn sim_build_ready(&self, kind: &uxduck_core::domain::link::LinkKind) -> Result<(), String> {
        self.simulators.check_build(kind)
    }

    fn processors(&self) -> Option<usize> {
        std::thread::available_parallelism()
            .ok()
            .map(std::num::NonZero::get)
    }

    fn is_local_address(&self, host: &str) -> bool {
        ports::is_local_address(host)
    }

    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        uxduck_mavlink::links::serial_ports()
            .into_iter()
            .map(|p| SerialPortInfo {
                device: p.device,
                usb: p.usb,
                description: p.description,
            })
            .collect()
    }

    fn power_saving(&self) -> Option<PowerSaving> {
        #[cfg(target_os = "macos")]
        let on = objc2::rc::autoreleasepool(|_| {
            objc2_foundation::NSProcessInfo::processInfo().isLowPowerModeEnabled()
        });
        #[cfg(target_os = "macos")]
        return on.then_some(PowerSaving::LowPowerMode);
        #[cfg(windows)]
        return energy_saver().then_some(PowerSaving::EnergySaver);
        #[cfg(target_os = "linux")]
        return self
            .power_saver
            .iter()
            .any(|on| on.load(std::sync::atomic::Ordering::Relaxed))
            .then_some(PowerSaving::PowerSaver);
        #[allow(unreachable_code, reason = "macOS, Windows and Linux return above")]
        None
    }

    fn wake_lock(&mut self, hold: bool) {
        if hold && self.wake.is_none() {
            match wake_hold() {
                Some(Ok(assertion)) => {
                    tracing::info!("holding the wake lock while a vehicle is connected");
                    self.wake = Some(assertion);
                }
                Some(Err(error)) => tracing::warn!(%error, "cannot hold the wake lock"),
                None => tracing::debug!("no wake lock on this platform yet"),
            }
        } else if !hold && let Some(assertion) = self.wake.take() {
            assertion.release();
            tracing::info!("released the wake lock");
        }
    }

    fn display_awake(&mut self, hold: bool) {
        if hold && self.display.is_none() {
            match display_hold() {
                Some(Ok(assertion)) => {
                    tracing::info!("keeping the display on while a vehicle is armed");
                    self.display = Some(assertion);
                }
                Some(Err(error)) => tracing::warn!(%error, "cannot keep the display on"),
                None => tracing::debug!("the desktop keeps its own screen lock here"),
            }
        } else if !hold && let Some(assertion) = self.display.take() {
            assertion.release();
            tracing::info!("let the display sleep again");
        }
    }

    fn storage(&self) -> uxduck_engine::runtime::StorageReport {
        super::storage::report(&self.data_dir)
    }

    fn reveal_storage(
        &mut self,
        category: Option<uxduck_core::domain::settings::StorageCategory>,
    ) -> Result<String, String> {
        let (folder, file) = match category {
            None => (self.data_dir.clone(), None),
            Some(category) => super::storage::place_to_reveal(&self.data_dir, category)
                .ok_or("Nothing is kept there yet.")?,
        };
        super::browser::reveal(&folder, file.as_deref())
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(folder.display().to_string())
    }

    fn delete_older_than(
        &mut self,
        category: uxduck_core::domain::settings::StorageCategory,
        cutoff_us: u64,
    ) -> uxduck_engine::runtime::Deleted {
        if category == uxduck_core::domain::settings::StorageCategory::SimulatorLogs {
            return self.simulators.with_active(
                |active| match super::storage::delete_simulator_logs(
                    &self.data_dir,
                    active,
                    Some(cutoff_us),
                ) {
                    Ok(deleted) => deleted,
                    Err(error) => {
                        tracing::error!(%error, "simulator log retention failed");
                        uxduck_engine::runtime::Deleted::default()
                    }
                },
            );
        }
        super::storage::delete_older(&self.data_dir, category, cutoff_us)
    }

    fn simulator_logs(
        &self,
    ) -> Result<Vec<uxduck_contracts::dto::settings::SimulatorLogUsage>, String> {
        self.simulators
            .with_active(|active| super::storage::simulator_logs(&self.data_dir, active))
    }

    fn limit_simulator_logs(&mut self, max_bytes: u64) -> uxduck_engine::runtime::Deleted {
        self.simulators.with_active(|active| {
            match super::storage::limit_simulator_logs(&self.data_dir, active, max_bytes) {
                Ok(deleted) => deleted,
                Err(error) => {
                    tracing::error!(%error, "simulator log size cleanup failed");
                    uxduck_engine::runtime::Deleted::default()
                }
            }
        })
    }

    fn clear_simulator_logs(&mut self) -> Result<(), String> {
        self.simulators.with_active(|active| {
            super::storage::delete_simulator_logs(&self.data_dir, active, None).map(|_| ())
        })
    }

    fn sim_binaries(&self) -> Vec<uxduck_contracts::dto::sim::SimBinary> {
        self.simulators.refresh_binaries()
    }

    fn sim_frames(
        &self,
        params: &uxduck_contracts::dto::sim::SimFramesParams,
    ) -> Result<uxduck_contracts::dto::sim::SimFramesResult, String> {
        self.simulators.read_frames(params)
    }

    fn sim_inspect(
        &self,
        path: &str,
    ) -> Result<uxduck_contracts::dto::sim::SimInspectResult, String> {
        crate::sim::inspect(std::path::Path::new(path))
    }

    fn sim_inspect_model(
        &self,
        path: &str,
    ) -> Result<uxduck_contracts::dto::sim::SimInspectModelResult, String> {
        crate::sim::inspect_model(path)
    }

    fn sim_own_build(
        &self,
        params: &uxduck_contracts::dto::sim::SimOwnBuildParams,
    ) -> Result<uxduck_contracts::dto::sim::SimOwnBuildResult, String> {
        crate::sim::own_build(std::path::Path::new(&params.binary))
    }

    fn sim_catalogue(&mut self, refresh: bool) -> uxduck_contracts::dto::sim::SimCatalogueResult {
        self.catalogue.result(refresh)
    }

    fn sim_download(&mut self, id: &str) -> Result<(), String> {
        self.catalogue.download(id)
    }

    fn sim_delete(&mut self, id: &str) -> Result<(), String> {
        self.catalogue.delete(id)
    }

    fn sim_pictures(&mut self, token: &str) -> Result<(), String> {
        crate::sim::picture::pictures().allow(token);
        Ok(())
    }

    fn desktop_app(&mut self, refresh: bool) -> uxduck_contracts::dto::app::DesktopAppStatus {
        self.desktop_app.status(refresh)
    }

    fn desktop_app_download(&mut self) -> Result<(), String> {
        self.desktop_app.download()
    }

    fn desktop_app_remove(&mut self) -> Result<(), String> {
        self.desktop_app.remove()
    }

    fn desktop_app_path(&mut self) -> Result<String, String> {
        self.desktop_app.path()
    }

    fn desktop_app_open(&mut self, path: &str, args: &[String]) -> Result<(), String> {
        self.desktop_app.open(path, args)
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;

    /// Whether `pmset` lists this process holding the activity.
    fn activity_listed() -> bool {
        let output = Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        let this = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim_start().starts_with(&this) && line.contains(ACTIVITY_REASON))
    }

    #[test]
    fn the_wake_lock_is_an_activity_macos_lists_until_it_is_released() {
        assert!(!activity_listed());
        let hold = wake_hold()
            .expect("macOS has a wake lock")
            .expect("it holds");
        assert!(matches!(hold, Hold::Activity(..)), "{hold:?}");
        assert!(activity_listed());
        hold.release();
        assert!(!activity_listed());
    }
}

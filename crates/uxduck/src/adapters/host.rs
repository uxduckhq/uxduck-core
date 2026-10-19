//! The engine's `System` port on a real machine: port checks, local
//! addresses, serial ports and the wake lock (ADR-0004).

use std::process::{Child, Command, Stdio};

use uxduck_engine::runtime::{SerialPortInfo, System};

use crate::ports;

/// The wake lock is a child process that holds the platform's own
/// assertion and exits with the daemon: `caffeinate -i -w <pid>` on macOS,
/// `systemd-inhibit` around `tail --pid` on Linux; on Windows a thread holds
/// `SetThreadExecutionState`. The display is held the same way while a
/// vehicle is armed: `caffeinate -d` on macOS, `ES_DISPLAY_REQUIRED` on
/// Windows; Linux desktops keep their own screen lock.
#[derive(Debug)]
pub struct Host {
    wake: Option<Hold>,
    display: Option<Hold>,
    data_dir: std::path::PathBuf,
    simulators: std::sync::Arc<crate::sim::Simulators>,
    catalogue: std::sync::Arc<crate::sim::catalogue::Catalogue>,
    desktop_app: std::sync::Arc<crate::desktop_app::DesktopApp>,
}

impl Host {
    #[must_use]
    pub fn new(
        data_dir: &std::path::Path,
        simulators: std::sync::Arc<crate::sim::Simulators>,
        catalogue: std::sync::Arc<crate::sim::catalogue::Catalogue>,
        desktop_app: std::sync::Arc<crate::desktop_app::DesktopApp>,
    ) -> Self {
        Self {
            wake: None,
            display: None,
            data_dir: data_dir.to_owned(),
            simulators,
            catalogue,
            desktop_app,
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
    Thread(std::sync::mpsc::Sender<()>),
}

impl Hold {
    fn release(self) {
        match self {
            Self::Child(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            #[cfg(windows)]
            Self::Thread(stop) => drop(stop),
        }
    }
}

/// Holds `flags` on a thread of its own until the returned sender drops:
/// `SetThreadExecutionState` lasts while its thread does.
#[cfg(windows)]
#[allow(unsafe_code, reason = "SetThreadExecutionState is a plain Win32 call")]
fn hold_execution_state(flags: u32) -> Hold {
    use windows_sys::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // SAFETY: sets and clears this thread's own execution state.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | flags) };
        let _ = stopped.recv();
        // SAFETY: as above.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    });
    Hold::Thread(stop)
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

/// The wake lock: the system stays awake, the display may sleep.
fn wake_hold() -> Option<std::io::Result<Hold>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::ES_SYSTEM_REQUIRED;
        return Some(Ok(hold_execution_state(ES_SYSTEM_REQUIRED)));
    }
    #[allow(unreachable_code, reason = "Windows returns above")]
    wake_command().map(spawn_hold)
}

/// The display stays on.
fn display_hold() -> Option<std::io::Result<Hold>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};
        return Some(Ok(hold_execution_state(
            ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
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

fn wake_command() -> Option<Command> {
    let pid = std::process::id().to_string();
    if cfg!(target_os = "macos") {
        let mut command = Command::new("caffeinate");
        // Idle sleep only: a closed lid on battery still sleeps, as macOS insists.
        command.args(["-i", "-w", &pid]);
        Some(command)
    } else if cfg!(target_os = "linux") {
        let mut command = Command::new("systemd-inhibit");
        command.args([
            "--what=idle:sleep",
            "--who=UXDuck",
            "--why=A vehicle is online",
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
        params: &uxduck_contracts::dto::inspect::MavlinkHistoryParams,
        now: u64,
    ) -> Result<
        uxduck_contracts::dto::inspect::MavlinkHistoryResult,
        uxduck_contracts::errors::ApiError,
    > {
        crate::diagnostics::history(&self.data_dir.join("tlogs"), link, system, params, now)
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
    fn check_udp_bind(&self, host: &str, port: u16) -> Result<(), String> {
        ports::check_udp_bind(host, port)
    }

    fn sim_instance_free(&self, instance: u8) -> bool {
        crate::sim::instance_free(instance)
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

    fn wake_lock(&mut self, hold: bool) {
        if hold && self.wake.is_none() {
            match wake_hold() {
                Some(Ok(assertion)) => {
                    tracing::info!("holding the wake lock while a vehicle is online");
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

    fn reveal_data_dir(&mut self) -> Result<String, String> {
        super::browser::reveal(&self.data_dir, None)
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(self.data_dir.display().to_string())
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

    fn sim_catalogue(&mut self, refresh: bool) -> uxduck_contracts::dto::sim::SimCatalogueResult {
        self.catalogue.result(refresh)
    }

    fn sim_download(&mut self, id: &str) -> Result<(), String> {
        self.catalogue.download(id)
    }

    fn sim_delete(&mut self, id: &str) -> Result<(), String> {
        self.catalogue.delete(id)
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

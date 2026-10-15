//! The engine's `System` port on a real machine: port checks, local
//! addresses, serial ports and the wake lock (ADR-0004).

use std::process::{Child, Command, Stdio};

use uxduck_engine::runtime::{SerialPortInfo, System};

use crate::ports;

/// The wake lock is a child process that holds the platform's own
/// assertion and exits with the daemon: `caffeinate -i -w <pid>` on macOS,
/// `systemd-inhibit` around `tail --pid` on Linux. Windows has no such tool
/// without an API call, so it is not held there yet.
#[derive(Debug)]
pub struct Host {
    wake: Option<Child>,
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
            data_dir: data_dir.to_owned(),
            simulators,
            catalogue,
            desktop_app,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.wake_lock(false);
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
            match wake_command().map(|mut c| {
                c.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            }) {
                Some(Ok(child)) => {
                    tracing::info!("holding the wake lock while a vehicle is online");
                    self.wake = Some(child);
                }
                Some(Err(error)) => tracing::warn!(%error, "cannot hold the wake lock"),
                None => tracing::debug!("no wake lock on this platform yet"),
            }
        } else if !hold && let Some(mut child) = self.wake.take() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::info!("released the wake lock");
        }
    }

    fn storage(&self) -> uxduck_engine::runtime::StorageReport {
        super::storage::report(&self.data_dir)
    }

    fn delete_older_than(
        &mut self,
        category: uxduck_core::domain::settings::StorageCategory,
        cutoff_us: u64,
    ) -> uxduck_engine::runtime::Deleted {
        super::storage::delete_older(&self.data_dir, category, cutoff_us)
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

//! Restarts, settings and storage (ADR-0007 state restoration and storage
//! policy).
//!
//! A restart is a final checkpoint and the same program started again; the
//! composition root does the restarting when `restart_requested` says so.
//! The safety rules apply on every start, planned or not: commands still
//! waiting for their vehicle end as `interrupted`, transfers in flight are
//! marked failed, and controllers are released and ask to be claimed again.

use serde_json::{Value, json};
use uxduck_contracts::convert::settings::settings_dto;
use uxduck_contracts::dto::inspect::{MavlinkHistoryParams, MavlinkInspectParams};
use uxduck_contracts::dto::settings::{
    AutoDelete, CategoryUsage, DaemonAbout, DaemonRestartParams, LoginItem, LoginItemState,
    Settings as SettingsDto, SettingsUpdateParams, StorageUsage, TaskWakeups, Wakeups,
};
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::command::ReceiptStatus;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::parameter::TransferStatus;
use uxduck_core::domain::settings::{ClosePolicy, Settings, StorageCategory};
use uxduck_core::domain::vehicle::Presence;
use uxduck_core::events::{Actor, Change};
use uxduck_core::policy::admission::Trust;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, OnLastClientClosed, upsert};

/// Below this much free space the daemon warns: 1 GiB, or 5 % of the disk
/// if that is less.
const LOW_DISK_BYTES: u64 = 1 << 30;
const LOW_DISK_CONDITION: &str = "low-disk";
const DAY_US: u64 = 86_400_000_000;

fn daemon() -> Actor {
    Actor {
        client_id: "daemon".into(),
        operator_id: None,
    }
}

#[allow(clippy::too_many_lines, reason = "one arm per request")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let denied = |why: &str| Err(ApiError::new(ErrorKind::CapabilityDenied, why.to_owned()));
    match request {
        ClientRequest::DaemonStop(_) if !operator => {
            denied("Only the UXDuck app on this computer can stop UXDuck.")
        }
        ClientRequest::DaemonStop(_) => {
            engine.stop = true;
            Ok(json!({}))
        }
        ClientRequest::DebugPanic(_) if !operator => {
            denied("only an operator client may make the daemon panic")
        }
        // A release build never panics on request.
        ClientRequest::DebugPanic(_) if !cfg!(debug_assertions) => Err(ApiError::new(
            ErrorKind::NotFound,
            "debug/panic exists in development builds only",
        )),
        ClientRequest::DebugPanic(_) => {
            engine.panic = true;
            Ok(json!({}))
        }
        ClientRequest::DaemonRestart(_) if !operator => {
            denied("only an operator client may restart the daemon")
        }
        ClientRequest::DaemonRestart(DaemonRestartParams { confirm }) => {
            let armed: Vec<String> = engine
                .state
                .vehicles
                .values()
                .filter(|v| v.armed && v.presence != Presence::Offline)
                .map(|v| v.name.clone())
                .collect();
            if !armed.is_empty() && !confirm {
                return Err(ApiError::new(
                    ErrorKind::ConfirmationRequired,
                    format!(
                        "{} {} armed. Restarting drops controllers and cuts the connection for about a second. Restart anyway?",
                        armed.join(", "),
                        if armed.len() == 1 { "is" } else { "are" }
                    ),
                ));
            }
            engine.restart = true;
            Ok(json!({}))
        }
        ClientRequest::SettingsUpdate(params) => {
            if !granted.contains(&Capability::Config) {
                return denied(&Capability::Config.denied_message());
            }
            update_settings(engine, connection, params)
        }
        ClientRequest::StorageUsage(_) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            to_value(&engine.storage_usage()?)
        }
        ClientRequest::StorageReveal(_) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            let folder = engine
                .system
                .reveal_data_dir()
                .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
            to_value(&uxduck_contracts::dto::video::VideoRevealResult { folder })
        }
        ClientRequest::StorageClearSimulatorLogs(_) => {
            if !granted.contains(&Capability::Config) {
                return denied(&Capability::Config.denied_message());
            }
            engine
                .system
                .clear_simulator_logs()
                .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
            to_value(&engine.storage_usage()?)
        }
        ClientRequest::DaemonWakeups(_) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            let (since_us, mut tasks) = engine.system.wakeups();
            tasks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            to_value(&Wakeups {
                since_us,
                tasks: tasks
                    .into_iter()
                    .map(|(name, count)| TaskWakeups { name, count })
                    .collect(),
            })
        }
        ClientRequest::DaemonAbout(_) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            to_value(&about(engine))
        }
        request @ (ClientRequest::MavlinkInspect(_) | ClientRequest::MavlinkHistory(_)) => {
            if !granted.contains(&Capability::Read) {
                return denied(&Capability::Read.denied_message());
            }
            inspector(engine, &request)
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a restart method")),
    }
}

/// Which UXDuck this is (S87): the handshake's version and stage, and what
/// the host said of its build.
fn about(engine: &Engine) -> DaemonAbout {
    let config = &engine.config;
    DaemonAbout {
        version: config.daemon_version.clone(),
        runs_in: config.capabilities.runs_in,
        protocol_version: uxduck_contracts::version::PROTOCOL_VERSION,
        release_stage: uxduck_contracts::version::ReleaseStage::Flight,
        commit: config.about.commit.clone(),
        built_at: config.about.built_at.clone(),
        target: config.about.target.clone(),
        dev: cfg!(debug_assertions),
        data_dir: config.about.data_dir.clone(),
        port: config.about.port,
        uptime_us: engine.clock.now_us().saturating_sub(engine.started_us),
    }
}

/// The MAVLink inspector's calls, for a known vehicle under its id and its
/// earlier ones: the data plane may have heard it under one, such as its
/// provisional id before its UID came.
fn inspector(engine: &Engine, request: &ClientRequest) -> Result<Value, ApiError> {
    if let ClientRequest::MavlinkHistory(params) = request
        && params.detailed()
    {
        crate::diagnostics::page_size(params.limit)?;
        // A diagnostic cursor freezes the end time; the host validates it.
        if params.before.is_none() || params.to_us.is_some() {
            crate::diagnostics::window(params.from_us, params.to_us, engine.clock.now_us())?;
        }
        let vehicle = if params.vehicle_id.is_empty() {
            None
        } else {
            Some(
                engine
                    .vehicle(&params.vehicle_id)
                    .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "No such vehicle"))?,
            )
        };
        let link_id = params
            .link_id
            .as_deref()
            .or_else(|| vehicle.map(|v| v.link_id.as_str()))
            .ok_or_else(|| {
                crate::diagnostics::invalid("Supply vehicleId or linkId for packet history.")
            })?;
        if link_id.is_empty()
            || link_id.len() > 128
            || !link_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(crate::diagnostics::invalid("Not a valid connection id."));
        }
        if vehicle.is_some_and(|v| v.link_id != link_id) {
            return Err(crate::diagnostics::invalid(
                "That vehicle is not on the selected connection.",
            ));
        }
        return to_value(&engine.system.packet_history(
            link_id,
            vehicle.map(|v| v.system_id),
            params,
            engine.clock.now_us(),
        )?);
    }
    let (ClientRequest::MavlinkInspect(MavlinkInspectParams { vehicle_id })
    | ClientRequest::MavlinkHistory(MavlinkHistoryParams { vehicle_id, .. })) = request
    else {
        return Err(ApiError::new(
            ErrorKind::Internal,
            "not an inspector method",
        ));
    };
    let vehicle = engine
        .vehicle(vehicle_id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {vehicle_id}")))?;
    let ids: Vec<String> = std::iter::once(&vehicle.id)
        .chain(&vehicle.aliases)
        .cloned()
        .collect();
    match request {
        ClientRequest::MavlinkHistory(params) => {
            let of = crate::runtime::HistoryOf {
                vehicle_ids: &ids,
                link_id: &vehicle.link_id,
                system_id: vehicle.system_id,
            };
            to_value(&engine.system.mavlink_history(&of, params)?)
        }
        _ => to_value(&engine.system.mavlink_inspect(&ids)),
    }
}

/// The tray and starting at login (S64), and which vehicle messages notify
/// (S26).
fn desktop(
    engine: &mut Engine,
    settings: &mut uxduck_core::domain::settings::Settings,
    params: &SettingsUpdateParams,
) -> Result<(), ApiError> {
    let (show_in_menu_bar, start_at_login) = (params.show_in_menu_bar, params.start_at_login);
    if let Some(level) = params.message_notify_severity {
        if level > 7 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Choose a severity from 0 (emergency) to 7 (debug).",
            ));
        }
        settings.message_notify_severity = level;
    }
    if let Some(on) = show_in_menu_bar {
        settings.show_in_menu_bar = on;
    }
    if let Some(action) = params.launch_action {
        settings.launch_action = action.into();
    }
    if settings.launch_action == uxduck_core::domain::settings::LaunchAction::MenuBar {
        if params.show_in_menu_bar == Some(false) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Choose a window or browser before hiding UXDuck from the menu bar.",
            ));
        }
        settings.show_in_menu_bar = true;
    }
    // Registered first: the setting says what is true. Asked again even
    // when it looks the same, as the system may have changed it.
    if let Some(on) = start_at_login {
        let item = engine
            .system
            .start_at_login(on)
            .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
        settings.start_at_login = on;
        engine.login_item = Some(item);
    }
    Ok(())
}

/// "Allow remote control from my account" (S80): only at the computer, so
/// someone away cannot let themselves in; the supervision grace period
/// (S81); and the ground station system id.
fn remote_control(
    engine: &Engine,
    connection: ConnectionId,
    params: &SettingsUpdateParams,
    settings: &mut uxduck_core::domain::settings::Settings,
) -> Result<(), ApiError> {
    // Flights in the account (S82).
    if let Some(on) = params.store_flights {
        settings.store_flights = on;
    }
    if let Some(on) = params.upload_while_armed {
        settings.upload_while_armed = on;
    }
    if let Some(on) = params.upload_video_while_flying {
        settings.upload_video_while_flying = on;
    }
    if let Some(grace) = params.supervision_grace_s {
        if !(5..=120).contains(&grace) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the supervision grace period is 5 to 120 seconds",
            ));
        }
        settings.supervision_grace_s = grace;
    }
    if let Some(id) = params.gcs_system_id {
        if id == 0 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "a ground station system id is 1 to 255",
            ));
        }
        settings.gcs_system_id = id;
    }
    let Some(classes) = &params.remote_control else {
        return Ok(());
    };
    if engine.remote_sender(connection).is_some() {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Remote control can only be turned on or off at the computer itself.",
        ));
    }
    let mut classes: Vec<uxduck_core::domain::command::CommandClass> =
        classes.iter().map(|c| (*c).into()).collect();
    classes.sort();
    classes.dedup();
    settings.remote_control = classes;
    Ok(())
}

/// "Phones and tablets on this network" (S97): selected IPv4 addresses, or off. Only at this computer: not from a phone on the
/// network, nor from away, so nobody lets themselves in.
fn network_address(
    engine: &Engine,
    connection: ConnectionId,
    params: &SettingsUpdateParams,
    settings: &mut uxduck_core::domain::settings::Settings,
) -> Result<(), ApiError> {
    if params.network_address.is_none() && params.network_addresses.is_none() {
        return Ok(());
    }
    let here = engine
        .connections
        .get(&connection)
        .is_some_and(|c| matches!(c.trust, Trust::LocalUser | Trust::PairedBrowser { .. }));
    if !here {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "Phones and tablets can only be let in at the computer itself.",
        ));
    }
    if params.network_address.is_some() && params.network_addresses.is_some() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Use networkAddresses or networkAddress, not both.",
        ));
    }
    let requested = params.network_addresses.clone().unwrap_or_else(|| {
        params
            .network_address
            .iter()
            .filter(|a| !a.trim().is_empty())
            .cloned()
            .collect()
    });
    let mut addresses = Vec::new();
    for address in requested {
        let address = address.trim();
        let ip = address
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .filter(|ip| {
                !ip.is_loopback()
                    && !ip.is_unspecified()
                    && !ip.is_multicast()
                    && !ip.is_link_local()
                    && !ip.is_broadcast()
            })
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("{address} isn't one of this computer's addresses on a network."),
                )
            })?;
        let address = ip.to_string();
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    settings.network_address = addresses.first().cloned();
    settings.network_addresses = addresses;
    Ok(())
}

/// A new ground station system id goes to the data plane, and to a search
/// under way, whose probes say hello from it.
fn gcs_system_id_changed(engine: &mut Engine, before: u8, after: u8) {
    if before != after {
        engine.links.gcs_system_id(after);
        engine.relook();
    }
}

/// Video ports as asked for, without repeats: none 0, and not too many.
fn video_ports(ports: Vec<u16>) -> Result<Vec<u16>, ApiError> {
    let mut unique: Vec<u16> = Vec::new();
    for port in ports {
        if port == 0 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "a video port cannot be 0",
            ));
        }
        if !unique.contains(&port) {
            unique.push(port);
        }
    }
    if unique.len() > crate::video::MAX_VIDEO_PORTS {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("at most {} video ports", crate::video::MAX_VIDEO_PORTS),
        ));
    }
    Ok(unique)
}

/// Days to keep each category, only for those deleted automatically.
fn auto_delete_days(
    list: Vec<AutoDelete>,
) -> Result<std::collections::BTreeMap<StorageCategory, u32>, ApiError> {
    let mut days = std::collections::BTreeMap::new();
    for entry in list {
        let category: StorageCategory = entry.category.into();
        if !category.deletable() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("{category:?} can't be deleted automatically."),
            ));
        }
        if entry.days == 0 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Choose at least 1 day.",
            ));
        }
        days.insert(category, entry.days);
    }
    Ok(days)
}

/// Apply independent age and size policies without changing unrelated settings.
fn storage_policies(
    settings: &mut Settings,
    params: &SettingsUpdateParams,
) -> Result<(), ApiError> {
    if let Some(list) = &params.auto_delete {
        let mut days = auto_delete_days(list.clone())?;
        settings.simulator_log_retention_days = days.remove(&StorageCategory::SimulatorLogs);
        settings.auto_delete_days = days;
    }
    if let Some(bytes) = params.simulator_log_max_bytes {
        if bytes != 0 && bytes < 1_000_000 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Choose at least 1 MB, or no size limit.",
            ));
        }
        settings.simulator_log_max_bytes = bytes;
    }
    Ok(())
}

fn update_channel(
    engine: &Engine,
    connection: ConnectionId,
    include_beta: Option<bool>,
    settings: &mut Settings,
) -> Result<(), ApiError> {
    if let Some(on) = include_beta {
        // Update policy belongs to this computer, never a remote account.
        if !engine
            .connections
            .get(&connection)
            .is_some_and(|c| matches!(c.trust, Trust::LocalUser | Trust::PairedBrowser { .. }))
        {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "Change beta updates on this computer.",
            ));
        }
        settings.include_beta = on;
    }
    Ok(())
}

fn update_settings(
    engine: &mut Engine,
    connection: ConnectionId,
    params: SettingsUpdateParams,
) -> Result<Value, ApiError> {
    let mut settings = engine.state.settings.clone();
    if settings.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "the settings are at version {}, not {}",
                settings.version, params.expected_version
            ),
        );
        error.data.current_version = Some(settings.version);
        return Err(error);
    }
    storage_policies(&mut settings, &params)?;
    if let Some(input) = &params.pilot_input {
        engine.configure_pilot_input(&mut settings, input)?;
    }
    if let Some(policy) = params.close_policy {
        settings.close_policy = Some(policy.into());
    }
    if let Some(path) = params.shell_path.clone() {
        settings.shell_path = if path.is_empty() {
            None
        } else if std::path::Path::new(&path).is_absolute() {
            Some(path)
        } else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "the shell's path must be absolute",
            ));
        };
    }
    update_channel(engine, connection, params.include_beta, &mut settings)?;
    if let Some(on) = params.usage_analytics {
        settings.usage_analytics = on;
    }
    if let Some(on) = params.connect_to_account {
        settings.connect_to_account = on;
    }
    // Turned off, nobody away may watch: this computer's watch links end (S95, W9).
    let ends_watch_links = params.watch_from_account == Some(false) && settings.watch_from_account;
    if let Some(on) = params.watch_from_account {
        settings.watch_from_account = on;
    }

    remote_control(engine, connection, &params, &mut settings)?;
    network_address(engine, connection, &params, &mut settings)?;
    desktop(engine, &mut settings, &params)?;
    if let Some(on) = params.share_conversations {
        settings.share_conversations = on;
    }
    if let Some(on) = params.recreate_simulations {
        settings.recreate_simulations = on;
    }
    if let Some(dataset) = params.terrain_dataset {
        settings.terrain_dataset = dataset.into();
    }
    // S119: one vehicle's own dataset, or back to the default.
    if let Some(choice) = &params.vehicle_terrain {
        match choice.dataset {
            Some(dataset) => {
                settings
                    .vehicle_terrain_datasets
                    .insert(choice.vehicle_id.clone(), dataset.into());
            }
            None => {
                settings.vehicle_terrain_datasets.remove(&choice.vehicle_id);
            }
        }
    }
    if let Some(on) = params.share_flight_data {
        settings.share_flight_data = on;
    }
    if let Some(policy) = params.video_recording {
        settings.video_recording = policy.into();
    }
    if let Some(ports) = params.video_ports {
        settings.video_ports = video_ports(ports)?;
    }
    settings.version += 1;
    let before = engine.state.settings.clone();
    engine.commit(
        connection,
        Change::SettingsChanged {
            settings: settings.clone(),
        },
    );
    engine.app_update_settings_changed(before.include_beta);
    engine.analytics_settings_changed(&before);
    engine.listen_video_ports(&before.video_ports, &settings.video_ports);
    gcs_system_id_changed(engine, before.gcs_system_id, settings.gcs_system_id);
    if before.video_recording != settings.video_recording {
        engine.review_recordings();
    }
    if ends_watch_links {
        engine.end_watch_links();
    }
    if let Some(choice) = &params.vehicle_terrain {
        engine.terrain_dataset_chosen(&choice.vehicle_id);
    }
    if before.simulator_log_max_bytes != settings.simulator_log_max_bytes {
        engine.check_storage();
    }
    to_value(&engine.settings_dto(&settings))
}

impl Engine {
    /// Whether a client asked the daemon to stop.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop
    }

    /// Whether a client asked a development build to panic (S32).
    #[must_use]
    pub fn panic_requested(&self) -> bool {
        self.panic
    }

    /// Whether a client asked for a restart; the composition root then
    /// stops, checkpoints and starts the program again.
    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.restart
    }

    /// The close policy the command line gave, used until one is set.
    fn default_close(&self) -> ClosePolicy {
        match self.shutdown.policy {
            OnLastClientClosed::Quit => ClosePolicy::Quit,
            OnLastClientClosed::KeepRunning => ClosePolicy::KeepRunning,
        }
    }

    pub(crate) fn close_policy(&self) -> ClosePolicy {
        self.state
            .settings
            .close_policy
            .unwrap_or_else(|| self.default_close())
    }

    pub(crate) fn settings_dto(&self, settings: &Settings) -> SettingsDto {
        let mut dto = settings_dto(settings, self.default_close());
        dto.login_item.clone_from(&self.login_item);
        dto
    }

    /// What the system says of opening at login (S108), asked now and then
    /// by the host, and after each change: the setting follows it, so a
    /// switch turned off in System Settings or Task Manager shows off here.
    pub fn login_item_changed(&mut self, item: LoginItem) {
        if self.login_item.as_ref() == Some(&item) {
            return;
        }
        let first = self.login_item.is_none();
        let wish = matches!(
            item.state,
            LoginItemState::On | LoginItemState::NeedsApproval
        );
        let follows =
            item.state != LoginItemState::Unavailable && wish != self.state.settings.start_at_login;
        self.login_item = Some(item);
        if first && !follows {
            self.push_tray();
            return;
        }
        let mut settings = self.state.settings.clone();
        if follows {
            settings.start_at_login = wish;
        }
        settings.version += 1;
        self.commit_as(daemon(), Change::SettingsChanged { settings });
        self.push_tray();
    }

    /// Opening at login as the system last said, when it was asked.
    #[must_use]
    pub fn login_item(&self) -> Option<&LoginItem> {
        self.login_item.as_ref()
    }

    pub(crate) fn settings_change(&self, settings: &Settings) -> EntityChange {
        EntityChange {
            settings: Some(self.settings_dto(settings)),
            ..upsert(EntityType::Settings, "daemon")
        }
    }

    fn storage_usage(&self) -> Result<StorageUsage, ApiError> {
        let report = self.system.storage();
        let mut simulator_logs = self
            .system
            .simulator_logs()
            .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        for sim in &mut simulator_logs {
            sim.name = self
                .state
                .links
                .get(&sim.simulation_id)
                .map(|link| link.name.clone())
                .or_else(|| {
                    self.state
                        .vehicles
                        .values()
                        .find(|v| v.link_id == sim.simulation_id)
                        .map(|v| v.name.clone())
                });
        }
        Ok(StorageUsage {
            data_dir: report.data_dir,
            simulator_logs,
            categories: StorageCategory::ALL
                .iter()
                .map(|category| {
                    let (bytes, files) = report
                        .categories
                        .iter()
                        .find(|(c, _, _)| c == category)
                        .map_or((0, 0), |(_, bytes, files)| (*bytes, *files));
                    CategoryUsage {
                        category: (*category).into(),
                        bytes,
                        files,
                        deletable: category.deletable(),
                    }
                })
                .collect(),
            free_bytes: report.free_bytes,
            total_bytes: report.total_bytes,
            upload: self.upload_status.clone(),
        })
    }

    /// Checks free space, and applies the auto-delete policy. The host
    /// calls this now and then.
    pub fn check_storage(&mut self) {
        let policy: Vec<(StorageCategory, u32)> = self
            .state
            .settings
            .auto_delete_days
            .iter()
            .map(|(c, d)| (*c, *d))
            .chain(
                self.state
                    .settings
                    .simulator_log_retention_days
                    .map(|days| (StorageCategory::SimulatorLogs, days)),
            )
            .collect();
        let now = self.clock.now_us();
        let (mut files, mut bytes) = (0, 0);
        for (category, days) in policy {
            let deleted = self
                .system
                .delete_older_than(category, now.saturating_sub(u64::from(days) * DAY_US));
            files += deleted.files;
            bytes += deleted.bytes;
        }
        let limited = self
            .system
            .limit_simulator_logs(self.state.settings.simulator_log_max_bytes);
        files += limited.files;
        bytes += limited.bytes;
        if files > 0 {
            self.raise(Raise {
                severity: Severity::Info,
                kind: NotificationKind::Transient,
                subject: Subject::System,
                title: format!("Deleted {files} files ({})", megabytes(bytes)),
                body: Some(
                    "Your storage settings remove files by age or size. You can change that in Settings."
                        .into(),
                ),
                actions: Vec::new(),
                condition: None,
                report: None,
            });
        }
        let report = self.system.storage();
        let low = match (report.free_bytes, report.total_bytes) {
            (Some(free), Some(total)) => free < LOW_DISK_BYTES.min(total / 20),
            (Some(free), None) => free < LOW_DISK_BYTES,
            _ => false,
        };
        if low {
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Persistent,
                subject: Subject::System,
                title: format!(
                    "Low disk space: {} left",
                    megabytes(report.free_bytes.unwrap_or(0))
                ),
                body: Some(
                    "Recording stops when the disk is full. Settings, Storage shows what uses it."
                        .into(),
                ),
                actions: Vec::new(),
                condition: Some(LOW_DISK_CONDITION.into()),
                report: None,
            });
        } else {
            self.resolve(LOW_DISK_CONDITION);
        }
    }

    /// The restart safety rules (ADR-0007), applied on every start.
    pub(crate) fn restore_after_restart(&mut self) {
        let now = self.clock.now_us();
        let waiting: Vec<_> = self
            .state
            .receipts
            .values()
            .filter(|r| r.status == ReceiptStatus::Sent)
            .cloned()
            .collect();
        for mut receipt in waiting {
            receipt.status = ReceiptStatus::Interrupted;
            receipt.detail = Some("the daemon restarted before the vehicle answered".into());
            receipt.completed_at_us = Some(now);
            receipt.version += 1;
            self.commit_as(daemon(), Change::ReceiptUpserted { receipt });
        }
        let sets: Vec<_> = self
            .state
            .parameter_sets
            .values()
            .filter(|s| s.status == TransferStatus::Downloading)
            .cloned()
            .collect();
        for mut set in sets {
            set.status = TransferStatus::Failed;
            set.detail = Some(
                "interrupted by a restart. It downloads again when the vehicle is heard".into(),
            );
            self.commit_as(daemon(), Change::ParameterSetUpserted { set });
        }
        let missions: Vec<_> = self
            .state
            .missions
            .values()
            .filter(|m| {
                matches!(
                    m.status,
                    TransferStatus::Downloading | TransferStatus::Uploading
                )
            })
            .cloned()
            .collect();
        for mut mission in missions {
            mission.detail = Some(if mission.status == TransferStatus::Uploading {
                "the daemon restarted during the upload: what the vehicle holds is unknown".into()
            } else {
                "interrupted by a restart".into()
            });
            mission.status = TransferStatus::Failed;
            self.commit_as(daemon(), Change::MissionUpserted { mission });
        }
    }
}

fn megabytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "shown rounded")]
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else {
        format!("{mb:.0} MB")
    }
}

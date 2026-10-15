//! What each composition root provides to the engine: time and randomness.
//! The native daemon uses the system clock and the OS random source; the
//! browser worker (S20) uses `performance.now()` and `crypto.getRandomValues`.

/// Wall-clock time.
pub trait Clock: Send {
    /// Microseconds since the Unix epoch.
    fn now_us(&self) -> u64;
}

/// Unpredictable values for tokens and pairing codes.
pub trait Random: Send {
    /// Fills `bytes` from a cryptographically secure source.
    fn fill(&mut self, bytes: &mut [u8]);

    /// 32 random bytes as 64 lowercase hex characters.
    fn token(&mut self) -> String {
        let mut bytes = [0u8; 32];
        self.fill(&mut bytes);
        bytes
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                use std::fmt::Write;
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// A six-digit decimal code.
    fn code(&mut self) -> String {
        let mut bytes = [0u8; 4];
        self.fill(&mut bytes);
        format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
    }
}

/// Where the engine writes what must survive a restart (ADR-0007). Calls
/// return at once; the implementation batches, group-commits and fsyncs on
/// its own schedule, so a power cut can lose the last few hundred
/// milliseconds but a crash or `kill -9` loses nothing already handed over.
pub trait Persistence: Send {
    /// Appends one event to the log, in `seq` order.
    fn append(&mut self, event: &uxduck_core::events::Event);
    /// Records the whole state at `state.seq`.
    fn checkpoint(&mut self, state: &crate::state::State);
    /// Records a browser session by the SHA-256 of its token; the token
    /// itself is never stored.
    fn save_session(&mut self, token_hash: &str, session: &crate::pairing::Session);
}

/// Persistence that keeps nothing, for tests and for the browser simulator
/// before it has storage.
#[derive(Debug, Default)]
pub struct Forget;

impl Persistence for Forget {
    fn append(&mut self, _: &uxduck_core::events::Event) {}
    fn checkpoint(&mut self, _: &crate::state::State) {}
    fn save_session(&mut self, _: &str, _: &crate::pairing::Session) {}
}

/// A turn for the agent host: run the agent on a thread's new message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRequest {
    /// Unique per daemon run; the host reports it back when the turn ends.
    pub turn: u64,
    /// The vehicle the thread is about, as `(id, name)`.
    pub vehicle: Option<(String, String)>,
    pub thread_id: String,
    pub text: String,
    pub model: String,
    /// The provider's conversation so far, if any.
    pub previous_response_id: Option<String>,
    /// Whether the hosted model may keep this chat's turns: the
    /// conversations switch, and the chat's own "Don't use this chat"
    /// (ADR-0013 P12, S29).
    pub share: bool,
}

/// What a turn used and how it went, from the agent host, for the
/// `agent_turn` event (S29). Numbers only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnStats {
    pub rounds: u32,
    pub tool_errors: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub first_token_ms: Option<u64>,
    pub retries: u32,
    pub fell_back: bool,
    pub compacted: bool,
    pub thinking_dropped: u32,
}

/// Where the engine hands agent turns (ADR-0009, local host). The host runs
/// them outside the engine and reports back through the engine's `agent_*`
/// methods; its tool calls are ordinary requests on its own connection.
pub trait AgentPort: Send {
    fn start_turn(&mut self, turn: TurnRequest);
    fn cancel_turn(&mut self, thread_id: &str);
}

/// The daemon's secrets (ADR-0009: the key lives in the daemon, never in the
/// browser).
pub trait Credentials: Send {
    /// Stores or, with an empty value, removes a secret.
    ///
    /// # Errors
    /// When the store cannot be written; the message is shown to the operator.
    fn set(&mut self, name: &str, value: &str) -> Result<(), String>;
    fn has(&self, name: &str) -> bool;
}

/// No agent: turns are dropped. For tests and hosts without one.
#[derive(Debug, Default)]
pub struct NoAgent;

impl AgentPort for NoAgent {
    fn start_turn(&mut self, _: TurnRequest) {}
    fn cancel_turn(&mut self, _: &str) {}
}

/// Credentials kept in memory, for tests.
#[derive(Debug, Default)]
pub struct MemoryCredentials(pub std::collections::BTreeMap<String, String>);

impl Credentials for MemoryCredentials {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        if value.is_empty() {
            self.0.remove(name);
        } else {
            self.0.insert(name.to_owned(), value.to_owned());
        }
        Ok(())
    }
    fn has(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

/// Where the engine asks for links to be opened and closed (ADR-0004). The
/// data plane reports back through `Engine::link_status` and
/// `Engine::vehicle_observed`.
pub trait LinkPort: Send {
    fn open(&mut self, link: &uxduck_core::domain::link::Link);
    fn close(&mut self, link_id: &str);
    /// A link changed (`link/update`): the same id with a new kind, policy
    /// or filter. A data plane that can keeps the transport and changes
    /// the link's role in place; the default reopens it.
    fn update(&mut self, link: &uxduck_core::domain::link::Link) {
        self.close(&link.id);
        self.open(link);
    }
    /// Sends a command to a vehicle; the data plane reports how it ended
    /// through `Engine::command_result`.
    fn command(&mut self, order: CommandOrder);
    /// Starts a parameter or mission transfer; the data plane reports back
    /// through the engine's `parameters_downloaded`, `parameter_written`,
    /// `mission_downloaded` and `mission_uploaded`.
    fn transfer(&mut self, order: TransferOrder) {
        let _ = order;
    }
    /// What a controller drives now: `None` stops its `MANUAL_CONTROL` at
    /// once. The data plane reports inputs through the engine's
    /// `controller_*` methods.
    fn route_controller(&mut self, controller_id: &str, route: Option<ControlRoute>) {
        let _ = (controller_id, route);
    }
    /// Sends a plugin's message by name. Nothing comes back: a message is
    /// not acknowledged as a command is.
    fn message(&mut self, order: MessageOrder) {
        let _ = order;
    }
    /// UXDuck's 1 Hz ground station heartbeat to a vehicle (S81): on as it
    /// starts; off when nobody supervises the vehicle, so its own lost
    /// ground station failsafe acts (ADR-0003 C16).
    fn ground_heartbeat(&mut self, vehicle_id: &str, on: bool) {
        let _ = (vehicle_id, on);
    }
    /// The MAVLink system id UXDuck sends as and takes replies for, from
    /// its settings: told before any link opens, and again when it changes.
    fn gcs_system_id(&mut self, id: u8) {
        let _ = id;
    }
}

/// Where a claimed controller's sticks go, and how they are read.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlRoute {
    pub pilot_input_method: uxduck_core::domain::settings::PilotInputMethod,
    pub vehicle_id: String,
    pub mapping: uxduck_core::domain::controller::InputMapping,
}

/// A parameter or mission transfer for the data plane.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferOrder {
    DownloadParameters {
        vehicle_id: String,
    },
    SetParameter {
        order_id: String,
        vehicle_id: String,
        name: String,
        value: f64,
        kind: uxduck_core::domain::parameter::ParamKind,
        /// Together with the others asked for now, as one file (S121).
        by_file: bool,
    },
    DownloadMission {
        vehicle_id: String,
    },
    UploadMission {
        order_id: String,
        vehicle_id: String,
        items: Vec<uxduck_core::domain::mission::MissionItem>,
    },
    /// A job on a vehicle's files over MAVLink FTP (S121), reported under
    /// `id` by `Engine::file_reported`.
    File {
        vehicle_id: String,
        id: String,
        op: FileOp,
    },
    /// Stops a file job, closing what it has open on the vehicle.
    CancelFile {
        vehicle_id: String,
        id: String,
    },
    /// Terrain answers for a block of a vehicle's grid (S118), sent at
    /// `per_second`. Answers queued for the block stay only if `mask`, the
    /// vehicle's latest request, still asks for them.
    TerrainData {
        vehicle_id: String,
        block: uxduck_mavlink::terrain::Block,
        groups: Vec<uxduck_mavlink::terrain::Group>,
        mask: u64,
        per_second: u8,
    },
    /// Asks a vehicle for its own terrain height at a point (S119),
    /// reported through `Engine::terrain_checked`.
    TerrainCheck {
        vehicle_id: String,
        lat: i32,
        lon: i32,
    },
}

/// What a file job does (S121).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
    List {
        path: String,
    },
    /// The file's bytes from `from` on.
    Read {
        path: String,
        from: u64,
    },
    Write {
        path: String,
        bytes: Vec<u8>,
    },
    Remove {
        path: String,
    },
    RemoveDir {
        path: String,
    },
    MakeDir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

/// A command for the data plane to carry out.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandOrder {
    pub receipt_id: String,
    pub vehicle_id: String,
    pub command: uxduck_core::domain::command::Command,
    pub force: bool,
}

/// No data plane: links stay `configured`. For tests.
#[derive(Debug, Default)]
pub struct NoLinks;

impl LinkPort for NoLinks {
    fn open(&mut self, _: &uxduck_core::domain::link::Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
}

/// A vehicle as `mavlink/history` finds its messages: its ids (its own
/// first, then its earlier ones), the link it is on and its system id,
/// which pick its frames out of the link's tlogs.
#[derive(Clone, Copy, Debug)]
pub struct HistoryOf<'a> {
    pub vehicle_ids: &'a [String],
    pub link_id: &'a str,
    pub system_id: u8,
}

/// What only the host can answer: binding ports, its own addresses, serial
/// devices, keeping the machine awake (ADR-0004 port ownership, wake lock).
pub trait System: Send {
    fn last_router_incident(
        &self,
        link: &str,
    ) -> Option<uxduck_contracts::dto::diagnostics::RouterEvent> {
        let _ = link;
        None
    }
    /// Exact wire packet queries.
    /// # Errors
    /// Invalid queries, unavailable capture, or unreadable recordings.
    fn packet_history(
        &self,
        link_id: &str,
        system_id: Option<u8>,
        params: &uxduck_contracts::dto::inspect::MavlinkHistoryParams,
        now_us: u64,
    ) -> Result<
        uxduck_contracts::dto::inspect::MavlinkHistoryResult,
        uxduck_contracts::errors::ApiError,
    > {
        let _ = (link_id, system_id, params, now_us);
        Err(uxduck_contracts::errors::ApiError::new(
            uxduck_contracts::errors::ErrorKind::InvalidValue,
            "Packet queries by link/time require UXDuck installed on a computer.",
        ))
    }
    /// Recent routing decisions and preserved loop evidence.
    /// # Errors
    /// Invalid or expired cursors and invalid bounds.
    fn router_events(
        &self,
        params: &uxduck_contracts::dto::diagnostics::RouterEventsParams,
        now_us: u64,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::RouterEventsResult,
        uxduck_contracts::errors::ApiError,
    > {
        let (from, to) = crate::diagnostics::window(params.from_us, params.to_us, now_us)?;
        Ok(uxduck_contracts::dto::diagnostics::RouterEventsResult {
            events: Vec::new(),
            older: None,
            coverage: uxduck_contracts::dto::diagnostics::DiagnosticCoverage {
                requested_from_us: from,
                requested_to_us: to,
                source: "unavailable".into(),
                notes: vec!["This host does not capture router evidence.".into()],
                ..Default::default()
            },
        })
    }
    /// Visible local socket ownership.
    /// # Errors
    /// Invalid endpoints or inspection failures.
    fn socket_inspect(
        &self,
        params: &uxduck_contracts::dto::diagnostics::SocketInspectParams,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::SocketInspectResult,
        uxduck_contracts::errors::ApiError,
    > {
        Ok(uxduck_contracts::dto::diagnostics::SocketInspectResult {
            protocol: params.protocol,
            port: params.port,
            owners: Vec::new(),
            available: false,
            complete: false,
            notes: vec!["Local socket inspection requires UXDuck installed on a computer.".into()],
        })
    }
    /// Bounded and redacted current-daemon log records.
    /// # Errors
    /// Invalid cursors, invalid bounds, or unavailable log capture.
    fn daemon_logs(
        &self,
        params: &uxduck_contracts::dto::diagnostics::DaemonLogsParams,
        now_us: u64,
    ) -> Result<
        uxduck_contracts::dto::diagnostics::DaemonLogsResult,
        uxduck_contracts::errors::ApiError,
    > {
        let _ = (params, now_us);
        Err(uxduck_contracts::errors::ApiError::new(
            uxduck_contracts::errors::ErrorKind::InvalidValue,
            "Daemon log queries require UXDuck installed on a computer.",
        ))
    }
    /// Whether a UDP listener could bind here now. `Err` names the owner
    /// when the platform says, such as "UDP port 14550 is in use by
    /// `MissionPlanner` (pid 812)".
    ///
    /// # Errors
    /// When the port is taken or the address is not usable.
    fn check_udp_bind(&self, host: &str, port: u16) -> Result<(), String>;
    /// Opens UXDuck at login, or stops it, where the system lists such
    /// things (S108, ADR-0025 D5), and says what the system has now.
    ///
    /// # Errors
    /// Why it could not, in words.
    fn start_at_login(
        &mut self,
        on: bool,
    ) -> Result<uxduck_contracts::dto::settings::LoginItem, String> {
        let _ = on;
        Err("Opening at login needs UXDuck installed on a computer.".into())
    }
    /// Whether this address is one of this machine's own.
    fn is_local_address(&self, host: &str) -> bool;
    fn serial_ports(&self) -> Vec<SerialPortInfo>;
    /// Holds or releases the system wake lock.
    fn wake_lock(&mut self, hold: bool);
    /// How often each task has woken since `since_us`, by name (S24).
    fn wakeups(&self) -> (u64, Vec<(String, u64)>) {
        (0, Vec::new())
    }
    /// Every plugged-in controller's raw input, for `controller/inputs`.
    /// Nothing by default: a host that reads no gamepads.
    fn controller_inputs(&self) -> uxduck_contracts::dto::controller::ControllerInputsResult {
        uxduck_contracts::dto::controller::ControllerInputsResult::default()
    }
    /// The latest message of each MAVLink type heard from a vehicle and
    /// sent to it, for the inspector, under its id and its earlier ones
    /// (`vehicle_ids`, its id first), from the root's
    /// [`crate::mavlink_inspect::Inspector`]. Nothing, undecoded, by
    /// default: a host that keeps no such table.
    fn mavlink_inspect(
        &self,
        vehicle_ids: &[String],
    ) -> uxduck_contracts::dto::inspect::MavlinkInspectResult {
        let _ = vehicle_ids;
        uxduck_contracts::dto::inspect::MavlinkInspectResult::default()
    }
    /// Latest received vehicle heartbeat under its lasting or earlier ids.
    /// Kept live by the host, without journaling every unchanged heartbeat.
    /// None when this host does not keep a MAVLink inspector.
    fn vehicle_heartbeat_us(&self, vehicle_ids: &[String]) -> Option<u64> {
        let _ = vehicle_ids;
        None
    }
    /// Earlier messages of one type, newest first, for the inspector's
    /// `mavlink/history`: from the vehicle's tlogs for what it sent where
    /// the host writes them, else from the root's
    /// [`crate::mavlink_inspect::Inspector`]. An empty page by default.
    ///
    /// # Errors
    /// When `params.before` is not a cursor this host wrote.
    fn mavlink_history(
        &self,
        vehicle: &HistoryOf<'_>,
        params: &uxduck_contracts::dto::inspect::MavlinkHistoryParams,
    ) -> Result<
        uxduck_contracts::dto::inspect::MavlinkHistoryResult,
        uxduck_contracts::errors::ApiError,
    > {
        let _ = (vehicle, params);
        Ok(uxduck_contracts::dto::inspect::MavlinkHistoryResult::default())
    }
    /// Disk space by category, and what is free (ADR-0007).
    fn storage(&self) -> StorageReport {
        StorageReport::default()
    }
    /// Deletes a category's files last written before `cutoff_us`, except
    /// any in use.
    fn delete_older_than(
        &mut self,
        category: uxduck_core::domain::settings::StorageCategory,
        cutoff_us: u64,
    ) -> Deleted {
        let _ = (category, cutoff_us);
        Deleted::default()
    }
    /// Whether a SITL instance's ports can be opened on this computer now
    /// (S71): another UXDuck's simulator, or another ground station's,
    /// may hold them. Free by default.
    fn sim_instance_free(&self, instance: u8) -> bool {
        let _ = instance;
        true
    }
    /// The SITL builds this host can run, probed (S17). None by default.
    fn sim_binaries(&self) -> Vec<uxduck_contracts::dto::sim::SimBinary> {
        Vec::new()
    }
    /// The simulator catalogue for this platform as last fetched, fetching
    /// it again first when `refresh` (S18). None by default.
    fn sim_catalogue(&mut self, refresh: bool) -> uxduck_contracts::dto::sim::SimCatalogueResult {
        let _ = refresh;
        uxduck_contracts::dto::sim::SimCatalogueResult {
            state: "failed".into(),
            detail: Some("this UXDuck has no simulator catalogue".into()),
            builds: Vec::new(),
        }
    }
    /// Where the desktop app stands (S110, ADR-0025 D6), asking the
    /// release channel the first time and again when `refresh`. `chosen`
    /// is left for the engine to fill in.
    fn desktop_app(&mut self, refresh: bool) -> uxduck_contracts::dto::app::DesktopAppStatus {
        let _ = refresh;
        uxduck_contracts::dto::app::DesktopAppStatus {
            state: uxduck_contracts::dto::app::DesktopAppState::Unavailable,
            chosen: false,
            version: None,
            size: None,
            unpacked_size: None,
            received: None,
            installed_version: None,
            detail: Some("This UXDuck has no desktop app.".into()),
        }
    }
    /// Starts downloading the desktop app; `desktop_app` shows how it goes.
    ///
    /// # Errors
    /// When none is offered here, or one is already downloading.
    fn desktop_app_download(&mut self) -> Result<(), String> {
        Err("This UXDuck has no desktop app.".into())
    }
    /// Deletes the desktop app.
    ///
    /// # Errors
    /// When it cannot be deleted.
    fn desktop_app_remove(&mut self) -> Result<(), String> {
        Err("This UXDuck has no desktop app.".into())
    }
    /// The installed desktop app's executable, checked (its code signature
    /// on macOS), for `shell_path`.
    ///
    /// # Errors
    /// When none is installed, or it fails its checks.
    fn desktop_app_path(&mut self) -> Result<String, String> {
        Err("This UXDuck has no desktop app.".into())
    }
    /// Starts the desktop app at `path` with these arguments, as UXDuck's
    /// own child, after checking it again. A running one takes the
    /// arguments and comes forward.
    ///
    /// # Errors
    /// When it fails its checks or does not start.
    fn desktop_app_open(&mut self, path: &str, args: &[String]) -> Result<(), String> {
        let _ = (path, args);
        Err("This UXDuck has no desktop app.".into())
    }
    /// Starts downloading a catalogue build; `sim_catalogue` shows how it goes.
    ///
    /// # Errors
    /// When there is no such build, or it cannot start.
    fn sim_download(&mut self, id: &str) -> Result<(), String> {
        Err(format!("no catalogue build {id} here"))
    }
    /// Deletes an installed catalogue build.
    ///
    /// # Errors
    /// When it is not installed, or cannot be deleted.
    fn sim_delete(&mut self, id: &str) -> Result<(), String> {
        Err(format!("no catalogue build {id} installed"))
    }
}

/// What the host found on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageReport {
    pub data_dir: String,
    /// Bytes and files per category.
    pub categories: Vec<(uxduck_core::domain::settings::StorageCategory, u64, u32)>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub files: u32,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialPortInfo {
    pub device: String,
    pub usb: Option<uxduck_core::domain::link::UsbId>,
    pub description: Option<String>,
}

/// A host with nothing to report: every bind works, only loopback and the
/// unspecified address are local, no serial ports. For tests.
#[derive(Debug, Default)]
pub struct NoSystem {
    pub wake_lock: bool,
    /// What `controller_inputs` answers.
    pub controller_inputs: uxduck_contracts::dto::controller::ControllerInputsResult,
}

impl System for NoSystem {
    fn check_udp_bind(&self, _: &str, _: u16) -> Result<(), String> {
        Ok(())
    }
    fn is_local_address(&self, host: &str) -> bool {
        host.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
            || host == "localhost"
    }
    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        Vec::new()
    }
    fn wake_lock(&mut self, hold: bool) {
        self.wake_lock = hold;
    }
    fn controller_inputs(&self) -> uxduck_contracts::dto::controller::ControllerInputsResult {
        self.controller_inputs.clone()
    }
}

/// A plugin for the host to run (ADR-0006).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginLoad {
    pub id: String,
    /// The module's path, as the host resolves it; for a script, the
    /// script's path, which the host runs in its script host.
    pub module: String,
    pub script: bool,
    /// What it receives, at each level and cadence (X1).
    pub subscribe: uxduck_contracts::dto::plugin::Subscriptions,
    /// Its vendor's dialect file's path, registered while it runs (S60).
    pub dialect: Option<String>,
}

/// A MAVLink message by name for a vehicle, from a plugin (ADR-0006 X4):
/// the data plane encodes it and sends it as the ground station.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageOrder {
    pub vehicle_id: String,
    pub name: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Who asked, for the log when it cannot be sent.
    pub plugin_id: String,
}

/// Where plugins run: outside the engine, on threads of their own. The
/// host reports back through `Engine::plugin_described`,
/// `Engine::plugin_effects` and `Engine::plugin_failed`.
pub trait PluginPort: Send {
    /// Reads and checks a plugin directory's manifest.
    ///
    /// # Errors
    /// When there is no readable, valid `plugin.json`, or no module.
    fn inspect(&self, path: &str) -> Result<uxduck_contracts::dto::plugin::PluginManifest, String>;
    fn load(&mut self, plugin: PluginLoad);
    fn unload(&mut self, id: &str);
    fn command(&mut self, id: &str, call: uxduck_contracts::dto::plugin::CommandCall);
    /// Writes an agent's draft into the drafts folder (X12) and checks it
    /// as `inspect` would.
    ///
    /// # Errors
    /// When it cannot be written, or does not check.
    fn draft(
        &mut self,
        order: DraftOrder,
    ) -> Result<uxduck_contracts::dto::plugin::PluginDraft, String> {
        let _ = order;
        Err("plugins are not available here".into())
    }
    /// Every draft, newest first.
    fn drafts(&self) -> Vec<uxduck_contracts::dto::plugin::PluginDraft> {
        Vec::new()
    }
    /// Runs a draft in the harness, off the engine's thread; the host
    /// answers through `Engine::plugin_tested`. False when it cannot.
    fn test(&mut self, order: TestOrder) -> bool {
        let _ = order;
        false
    }
    /// The folders of the plugins shipped with UXDuck (S104), written out
    /// by the host from its own copy; none where plugins do not run.
    fn included(&self) -> Vec<String> {
        Vec::new()
    }
}

/// An agent's draft, and who wrote it where.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftOrder {
    pub params: uxduck_contracts::dto::plugin::PluginDraftParams,
    pub written_by: uxduck_contracts::dto::plugin::WrittenBy,
}

/// A draft's test run, and the request it answers.
#[derive(Clone, Debug, PartialEq)]
pub struct TestOrder {
    pub connection: crate::ConnectionId,
    pub request_id: serde_json::Value,
    /// The draft's folder.
    pub path: String,
    /// The vehicle whose last minutes it runs against.
    pub flight: Option<crate::feedback::FlightRef>,
    pub command: Option<String>,
    pub params: Option<serde_json::Value>,
}

/// No plugin host: nothing installs. For tests and the browser.
#[derive(Debug, Default)]
pub struct NoPlugins;

impl PluginPort for NoPlugins {
    fn inspect(&self, _: &str) -> Result<uxduck_contracts::dto::plugin::PluginManifest, String> {
        Err("plugins are not available here".into())
    }
    fn load(&mut self, _: PluginLoad) {}
    fn unload(&mut self, _: &str) {}
    fn command(&mut self, _: &str, _: uxduck_contracts::dto::plugin::CommandCall) {}
}

/// Where product analytics go: the daemon's outbox (ADR-0013, ADR-0016).
/// The engine checks the usage analytics switch before calling `track` or
/// `alias`; the outbox keeps the install id and sends in batches.
pub trait AnalyticsPort: Send {
    /// Queues one event under the install id. `surface` is where it came
    /// from (`browser`, `desktop` or `daemon`), unless the event says.
    fn track(&mut self, event: uxduck_contracts::dto::analytics::AnalyticsEvent, surface: &str);
    /// Joins a browser's earlier events to the install id.
    fn alias(&mut self, browser_id: &str);
    /// On: make sure there is an install id. Off: forget it and drop
    /// whatever was queued, so nothing identifies this install any more.
    fn set_enabled(&mut self, on: bool);
}

/// No analytics: everything is dropped. The default, for tests and the
/// browser worker, which hands its events to the page instead.
#[derive(Debug, Default)]
pub struct NoAnalytics;

impl AnalyticsPort for NoAnalytics {
    fn track(&mut self, _: uxduck_contracts::dto::analytics::AnalyticsEvent, _: &str) {}
    fn alias(&mut self, _: &str) {}
    fn set_enabled(&mut self, _: bool) {}
}

/// Everything the engine reaches outside itself.
pub struct Ports {
    pub persistence: Box<dyn Persistence>,
    pub agent: Box<dyn AgentPort>,
    pub credentials: Box<dyn Credentials>,
    pub links: Box<dyn LinkPort>,
    pub system: Box<dyn System>,
    pub plugins: Box<dyn PluginPort>,
}

impl std::fmt::Debug for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ports").finish_non_exhaustive()
    }
}

impl Ports {
    /// Persistence only; no agent and credentials in memory.
    #[must_use]
    pub fn with_persistence(persistence: Box<dyn Persistence>) -> Self {
        Self {
            persistence,
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(NoLinks),
            system: Box::new(NoSystem::default()),
            plugins: Box::new(NoPlugins),
        }
    }
}

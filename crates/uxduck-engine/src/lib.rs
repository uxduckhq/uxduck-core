//! The state engine: the single writer, journal and restore, subscriptions,
//! and the JSON-RPC router (ADR-0010, amended 2026-09-29).
//!
//! Never depends on Tokio, sockets, threads or the file system, so the native
//! daemon and the browser worker (S20) run the same engine. CI checks this with
//! `cargo check --target wasm32-unknown-unknown`.
//!
//! The engine is synchronous and owned by one task in each composition root,
//! which makes it the only writer. Frames come in through [`Engine::receive`];
//! replies and notifications leave through each connection's [`Outbox`].
//! Every change is one event: logged through [`Persistence`], applied to the
//! [`State`], then pushed to subscribers as `state/delta`, in that order.

#![forbid(unsafe_code)]

pub mod account;
pub mod agent;
pub mod analytics;
mod app;
mod app_update;
pub mod approvals;
pub mod commands;
pub mod controller_input;
pub mod controllers;
pub mod diagnostics;
mod display;
pub mod edge;
pub mod feedback;
pub mod finds;
mod flight_time;
mod flight_usage;
mod forwarding;
mod grounded;
mod history;
mod idle_simulations;
mod import;
pub mod interface;
mod klv;
mod launch;
mod learning;
pub mod location;
pub mod mavlink_inspect;
mod messages;
pub mod mission_history;
pub mod pairing;
pub mod param_definitions;
mod photos;
pub mod plan_checks;
pub mod plan_examples;
pub mod plan_files;
pub mod plan_patterns;
pub mod plane;
mod plans;
pub mod plugin_sync;
pub mod plugins;
pub mod practice;
mod recordings;
mod remote;
pub mod remote_view;
pub mod restart;
pub(crate) mod rpc;
pub mod runtime;
pub mod schema;
pub mod search;
pub mod sim;
mod snapshots;
pub mod state;
mod supervision;
pub mod sync;
mod telemetry;
mod telemetry_window;
pub mod terrain;
mod terrain_check;
mod terrain_keep;
pub mod trace;
pub mod transfers;
pub mod tray;
pub mod vehicle_files;
mod vehicles;
mod version;
pub mod video;
mod watch_link;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::link::Link as LinkDto;
use uxduck_contracts::dto::operator::Operator as OperatorDto;
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType, StateDelta};
use uxduck_contracts::dto::thread::{Folder as FolderDto, Item as ItemDto, Thread as ThreadDto};
use uxduck_contracts::dto::trace::{FrameKind, RequestId, TraceDirection, TraceFrame, Transport};
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::thread::{Item, ItemStatus};
use uxduck_core::events::{Actor, Change, Event};
use uxduck_core::policy::admission::Trust;

use crate::pairing::Sessions;
pub use crate::pairing::{PairingState, Session};
pub use crate::recordings::SpoolVideo;
use crate::runtime::{AgentPort, Clock, Credentials, LinkPort, Persistence, Ports, Random};
pub use crate::state::State;
use crate::trace::Parsed;

/// Where the engine writes a connection's outgoing frames, one JSON text each.
pub type Outbox = futures::channel::mpsc::UnboundedSender<String>;

/// The engine's handle on one connection; not the client id it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(u64);

/// No client: what UXDuck does by itself, such as at start. Clients are
/// numbered from 1.
pub(crate) const NO_CONNECTION: ConnectionId = ConnectionId(0);

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub daemon_version: String,
    pub capabilities: DaemonCapabilities,
    /// Where `uxduck open` sends the browser: the daemon's own origin, or the
    /// dev server's in `--dev`.
    pub ui_url: String,
    /// Write a checkpoint after this many events (ADR-0007).
    pub checkpoint_every: u64,
    /// Models offered for threads; the first is the default.
    pub models: Vec<String>,
    /// `openai`, or `fake` for development and tests.
    pub agent_provider: String,
    /// How the host was built and where it listens, for `daemon/about`
    /// (S87).
    pub about: HostAbout,
}

/// What the host knows of itself that the engine cannot (S87): its build,
/// from `build.rs` or the wasm build, and where it keeps data and listens.
/// Unknown fields stay empty, as in tests.
#[derive(Clone, Debug, Default)]
pub struct HostAbout {
    /// The commit built from, short.
    pub commit: Option<String>,
    /// When it was built, RFC 3339 in UTC.
    pub built_at: Option<String>,
    /// The target triple.
    pub target: String,
    pub data_dir: Option<String>,
    pub port: Option<u16>,
}

/// The credential the OpenAI provider needs.
pub const OPENAI_KEY: &str = "openai.key";
/// The credential the Anthropic provider needs in development builds.
pub const ANTHROPIC_KEY: &str = "anthropic.key";
/// Where the account's bearer token is kept (S27); a signed-in UXDuck has one.
pub const ACCOUNT_TOKEN: &str = "account.token";

/// What the composition root loaded from disk before starting the engine.
#[derive(Debug, Default)]
pub struct Loaded {
    pub state: State,
    /// Browser sessions by token hash.
    pub sessions: Vec<(String, Session)>,
}

/// What the daemon does when the last window closes (ADR-0001 shutdown
/// policy; keep-running by default since the 2026-09-27 follow-up).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnLastClientClosed {
    /// Quit after the grace period, unless something must keep running.
    Quit,
    #[default]
    KeepRunning,
}

/// Pages and windows (S107).
#[derive(Debug, Default)]
struct Opened {
    /// UXDuck runs on a desktop with its menu-bar helper: with the icon
    /// switched off it quits with its last window.
    desktop: bool,
    /// A window (a page or the desktop app) has been open since start.
    window_seen: bool,
    /// Counts pages admitted and saying whether they are seen.
    seen_order: u64,
}

#[derive(Debug, Default)]
struct Shutdown {
    policy: OnLastClientClosed,
    /// Running a headless job such as `uxduck forward`.
    headless: bool,
}

#[derive(Debug)]
struct Admitted {
    kind: ClientKind,
    /// Admitted as an operator: full control, never escalates.
    operator: bool,
    /// Its client record; `granted` follows the record's capabilities.
    record: String,
    granted: Vec<Capability>,
    /// Allowed "for this connection" by an approval.
    extra: Vec<Capability>,
    /// It declared that it can show approvals.
    approvals: bool,
}

impl Admitted {
    fn is_operator(&self) -> bool {
        self.operator
    }
}

#[derive(Debug)]
struct Connection {
    transport: Transport,
    trust: Trust,
    outbox: Outbox,
    client_id: String,
    admitted: Option<Admitted>,
    /// The operator this connection acts as, for journal attribution.
    operator_id: Option<String>,
    /// Arrival time of requests not answered yet, for the tap's latency.
    pending: HashMap<RequestId, u64>,
    /// The program on the other end, where the transport shows it.
    executable: Option<String>,
    /// The name it declared in `initialize`.
    name: String,
    /// Its last two minutes of frames, safe side only, for a feedback
    /// report's bundle (ADR-0016 O11).
    recent: std::collections::VecDeque<TraceFrame>,
    /// A page on screen, as it last said with `client/visibility` (S107).
    visible: bool,
    /// The order it was admitted in or last said whether it is seen:
    /// `app/focus` brings forward the page seen last.
    seen: u64,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "the wake lock and what clients asked of the daemon itself"
)]
pub struct Engine {
    config: EngineConfig,
    clock: Box<dyn Clock>,
    random: Box<dyn Random>,
    persistence: Box<dyn Persistence>,
    agent: Box<dyn AgentPort>,
    credentials: Box<dyn Credentials>,
    links: Box<dyn LinkPort>,
    system: Box<dyn runtime::System>,
    /// Whether the wake lock is held.
    awake: bool,
    /// The display is held on: a vehicle is armed.
    display_awake: bool,
    shutdown: Shutdown,
    /// Pages and windows, for opening UXDuck again and quitting (S107).
    opened: Opened,
    /// The request being handled, until a handler takes it to answer later.
    replying_to: Option<(ConnectionId, Value)>,
    /// Requests answered when their command ends, by receipt id.
    awaiting: BTreeMap<String, (ConnectionId, Value)>,
    /// Requests held for an approval, by approval id.
    held: BTreeMap<String, approvals::Held>,
    /// A capability allowed once, for the replay of one held request.
    one_shot: Option<Capability>,
    /// True while an operator's approval runs the request it allowed, which
    /// must not ask again (an install always asks otherwise).
    running_approved: bool,
    /// Parameter writes and mission uploads waiting for the vehicle.
    transfers: transfers::Pending,
    /// Simulated vehicles taking off to start in the air (S33).
    launches: launch::Launches,
    /// Parameter definition sets in memory, and the port that fetches them
    /// (S67).
    definitions: param_definitions::Definitions,
    /// Controllers' neutral state, holders and selections.
    live: controllers::Live,
    /// Camera streams' live counters and watches (S38).
    video: video::Video,
    /// Who is looking for vehicles and what was found (S44).
    finding: finds::Finding,
    /// The interface the daemon serves (S109).
    interface: interface::Interface,
    /// Where this computer is, while a person has it on (S137).
    location: location::Location,
    remote_viewing: remote_view::RemoteViewing,
    /// A client asked for a restart.
    restart: bool,
    app_update: app_update::Updates,
    /// A client asked the daemon to stop.
    stop: bool,
    /// A client asked a development build to panic (S32).
    panic: bool,
    plugin_host: Box<dyn runtime::PluginPort>,
    /// Product analytics; nothing until the composition root sets it.
    analytics: Box<dyn runtime::AnalyticsPort>,
    /// Bounded, transient browser task outcomes, drained once across windows.
    engagement: Vec<uxduck_contracts::dto::analytics::AppEvent>,
    /// Feedback reports; nothing until the composition root sets it.
    feedback: Box<dyn feedback::FeedbackPort>,
    /// Feedback requests answered when the root reports back, by report id.
    awaiting_feedback: BTreeMap<String, (ConnectionId, Value)>,
    /// Account calls waiting for the root (S27).
    awaiting_account: BTreeMap<String, (ConnectionId, Value)>,
    /// Account orders that fetch a shared plugin to install (S59).
    awaiting_shared_installs: std::collections::BTreeSet<String>,
    /// Account orders that list the account's computers (S78).
    awaiting_computers: std::collections::BTreeSet<String>,
    /// Account orders for watch links (S95), and what to do with each answer.
    awaiting_watch: BTreeMap<String, watch_link::Awaiting>,
    /// Guests watching through each watch link, from the relay (S95).
    watch_viewers: Vec<uxduck_contracts::dto::edge::WatchViewers>,
    /// This computer's connection to its account (S78), as the root's task reports it.
    edge_status: uxduck_contracts::dto::edge::EdgeStatus,
    /// Control from away (S80): leases and the commands' ids.
    remote: remote::Remote,
    /// Which vehicles are supervised, and whose heartbeat is withheld (S81).
    supervision: supervision::Supervision,
    /// Practice attempts being set up (S100).
    practice: practice::Practice,
    /// A vehicle's files: requests waiting for it, and the copies' port
    /// (S121).
    files: vehicle_files::Files,
    /// Terrain heights (S23): the cache and requests waiting for data.
    terrain: terrain::TerrainState,
    /// The modes each vehicle lists itself (`AVAILABLE_MODES`), by its id,
    /// for the command list; not kept across restarts, as the vehicle says
    /// again.
    available_modes: BTreeMap<String, Vec<uxduck_core::domain::modes::AvailableMode>>,
    /// Flights waiting to go to the account, as the root's uploader reports (S82).
    upload_status: Option<uxduck_contracts::dto::settings::UploadStatus>,
    account: Option<Box<dyn account::AccountPort>>,
    /// Feedback waiting for the root, by account order id: the answer rated.
    awaiting_feedback_items: BTreeMap<String, String>,
    /// Cloud sync's task (S16); none where sync is unavailable.
    sync: Option<Box<dyn sync::SyncPort>>,
    /// Why sync is not working right now.
    sync_problem: Option<String>,
    /// When the last sync round worked, whether or not it moved anything.
    sync_checked_us: Option<u64>,
    /// When this engine started, for the bundle's uptime.
    started_us: u64,
    /// Each running plugin's own connection.
    plugin_connections: BTreeMap<String, ConnectionId>,
    /// What each plugin draws on the map, by plugin and feature id (S55).
    /// Not journaled: a plugin draws again when it runs again.
    plugin_layers: BTreeMap<String, BTreeMap<String, uxduck_contracts::dto::plugin::MapFeature>>,
    /// Connections that asked for plugin features, which get their changes.
    feature_watchers: std::collections::BTreeSet<ConnectionId>,
    /// Each plugin's own items, those with no vehicle, by plugin and key
    /// (S101), as sent, with their rates (S104). Not journaled: a plugin
    /// produces them again when it runs.
    plugin_own_items:
        BTreeMap<String, BTreeMap<String, uxduck_contracts::dto::telemetry::TelemetryItem>>,
    /// Connections that asked for plugins' own items.
    item_watchers: std::collections::BTreeSet<ConnectionId>,
    /// What each running plugin used in the last minute, as the host last
    /// said (S104).
    plugin_usage: BTreeMap<String, uxduck_contracts::dto::plugin::PluginUsage>,
    /// The tray's connection, which gets the menu when it changes (S64).
    tray_watchers: std::collections::BTreeSet<ConnectionId>,
    last_tray_menu: Option<uxduck_contracts::dto::tray::TrayMenu>,
    /// Opening at login as the system last said (S108); none until asked.
    login_item: Option<uxduck_contracts::dto::settings::LoginItem>,
    /// Screenshots the tray took for "Send feedback…".
    tray_screenshots: tray::Screenshots,
    idle_simulations: idle_simulations::IdleSimulations,
    /// Vehicles' own messages (S26).
    messages: messages::Messages,
    /// Which simulated vehicles have flown, for the notice when one is
    /// back on the ground.
    flights: grounded::Flights,
    /// Each vehicle's time armed and in the air (S103).
    flight_times: flight_time::FlightTimes,
    flight_usage: flight_usage::FlightUsage,
    state: State,
    since_checkpoint: u64,
    /// Threads with a turn in progress, and that turn's id.
    turns: BTreeMap<String, u64>,
    /// When each turn began, by turn id, for its length in analytics.
    turn_started: BTreeMap<u64, u64>,
    /// What each turn used, reported by the host before it finishes.
    turn_stats: BTreeMap<u64, runtime::TurnStats>,
    next_turn: u64,
    /// Text streamed into agent messages still in progress. Not journaled;
    /// merged into snapshots so a late subscriber sees what has streamed.
    streaming: HashMap<String, String>,
    next_connection: u64,
    connections: BTreeMap<ConnectionId, Connection>,
    display: display::DisplayRelay,
    tracers: BTreeSet<ConnectionId>,
    /// Live telemetry by vehicle id; not journaled.
    telemetry: BTreeMap<String, uxduck_core::domain::telemetry::VehicleTelemetry>,
    telemetry_history: telemetry_window::Window,
    telemetry_subscribers: BTreeSet<ConnectionId>,
    subscribers: BTreeMap<ConnectionId, BTreeSet<EntityType>>,
    sessions: Sessions,
    /// Phones and tablets on this network (S97): the addresses and where
    /// UXDuck listens, as the host last said.
    network: uxduck_contracts::dto::network::NetworkStatus,
    /// Links removed in this run, so a late report from the data plane
    /// about one brings no vehicle back. Link ids are not reused.
    pub(crate) removed_links: BTreeSet<String>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("config", &self.config)
            .field("seq", &self.state.seq)
            .field("connections", &self.connections.len())
            .field("subscribers", &self.subscribers.len())
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Starts from what was loaded. `loaded.state.seq` is where the sequence
    /// continues.
    #[must_use]
    ///
    /// Items still in progress when the daemon stopped are marked
    /// `interrupted` first: no turn survives a restart, but the thread does.
    #[allow(clippy::too_many_lines, reason = "one line per field")]
    pub fn new(
        config: EngineConfig,
        clock: Box<dyn Clock>,
        random: Box<dyn Random>,
        ports: Ports,
        loaded: Loaded,
    ) -> Self {
        let mut engine = Self {
            config,
            clock,
            random,
            persistence: ports.persistence,
            agent: ports.agent,
            credentials: ports.credentials,
            links: ports.links,
            system: ports.system,
            awake: false,
            display_awake: false,
            shutdown: Shutdown::default(),
            opened: Opened::default(),
            replying_to: None,
            awaiting: BTreeMap::new(),
            held: BTreeMap::new(),
            one_shot: None,
            running_approved: false,
            transfers: transfers::Pending::default(),
            launches: launch::Launches::default(),
            definitions: param_definitions::Definitions::default(),
            video: video::Video::default(),
            finding: finds::Finding::default(),
            interface: interface::Interface::default(),
            location: location::Location::default(),
            remote_viewing: remote_view::RemoteViewing::default(),
            live: controllers::Live::default(),
            restart: false,
            app_update: app_update::Updates::default(),
            stop: false,
            panic: false,
            plugin_host: ports.plugins,
            analytics: Box::new(runtime::NoAnalytics),
            feedback: Box::new(feedback::NoFeedback),
            awaiting_feedback: BTreeMap::new(),
            awaiting_account: BTreeMap::new(),
            awaiting_shared_installs: std::collections::BTreeSet::new(),
            awaiting_computers: std::collections::BTreeSet::new(),
            awaiting_watch: BTreeMap::new(),
            watch_viewers: Vec::new(),
            edge_status: edge::signed_out(),
            remote: remote::Remote::default(),
            supervision: supervision::Supervision::default(),
            practice: practice::Practice::default(),
            files: vehicle_files::Files::default(),
            terrain: terrain::TerrainState::default(),
            available_modes: BTreeMap::new(),
            upload_status: None,
            account: None,
            awaiting_feedback_items: BTreeMap::new(),
            sync: None,
            sync_problem: None,
            sync_checked_us: None,
            started_us: 0,
            plugin_connections: BTreeMap::new(),
            plugin_layers: BTreeMap::new(),
            feature_watchers: std::collections::BTreeSet::new(),
            plugin_own_items: BTreeMap::new(),
            item_watchers: std::collections::BTreeSet::new(),
            plugin_usage: BTreeMap::new(),
            tray_watchers: std::collections::BTreeSet::new(),
            last_tray_menu: None,
            login_item: None,
            tray_screenshots: tray::Screenshots::default(),
            idle_simulations: idle_simulations::IdleSimulations::default(),
            messages: messages::Messages::default(),
            flights: grounded::Flights::default(),
            flight_times: flight_time::FlightTimes::default(),
            flight_usage: flight_usage::FlightUsage::default(),
            engagement: Vec::new(),
            state: loaded.state,
            since_checkpoint: 0,
            turns: BTreeMap::new(),
            turn_started: BTreeMap::new(),
            turn_stats: BTreeMap::new(),
            next_turn: 0,
            streaming: HashMap::new(),
            next_connection: 0,
            connections: BTreeMap::new(),
            display: display::DisplayRelay::default(),
            tracers: BTreeSet::new(),
            telemetry: BTreeMap::new(),
            telemetry_history: telemetry_window::Window::default(),
            telemetry_subscribers: BTreeSet::new(),
            subscribers: BTreeMap::new(),
            sessions: Sessions::restored(loaded.sessions),
            network: uxduck_contracts::dto::network::NetworkStatus::default(),
            removed_links: BTreeSet::new(),
        };
        engine.started_us = engine.clock.now_us();
        // A phone forgotten just before a crash may have left its session
        // behind (S97): without its record it is no longer paired.
        let clients = engine.state.clients.clone();
        engine.sessions.forget_orphaned_network_sessions(
            |client| clients.contains_key(client),
            engine.persistence.as_mut(),
        );
        let interrupted: Vec<Item> = engine
            .state
            .items
            .values()
            .filter(|item| item.status == ItemStatus::InProgress)
            .cloned()
            .collect();
        for mut item in interrupted {
            item.status = ItemStatus::Interrupted;
            item.version += 1;
            engine.commit_as(Actor::default(), Change::ItemUpserted { item });
        }
        engine.start_data_plane();
        engine.reset_controllers();
        engine.restore_after_restart();
        engine.retire_ground_testing_notices();
        engine.start_plugins();
        engine.expire_stale_approvals();
        engine
    }

    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Writes a checkpoint now, as a graceful shutdown does (ADR-0007).
    pub fn checkpoint(&mut self) {
        self.persistence.checkpoint(&self.state);
        self.since_checkpoint = 0;
    }

    /// Registers an accepted connection. Nothing is admitted until `initialize`.
    pub fn connect(&mut self, transport: Transport, trust: Trust, outbox: Outbox) -> ConnectionId {
        self.next_connection += 1;
        let id = ConnectionId(self.next_connection);
        let client_id = format!("c{}", &self.random.token()[..16]);
        self.connections.insert(
            id,
            Connection {
                transport,
                trust,
                outbox,
                client_id,
                admitted: None,
                operator_id: None,
                pending: HashMap::new(),
                recent: std::collections::VecDeque::new(),
                executable: None,
                name: String::new(),
                visible: false,
                seen: 0,
            },
        );
        id
    }

    /// Records the program on the other end of a local connection, before
    /// `initialize`: part of its client identity (ADR-0005).
    pub fn set_executable(&mut self, id: ConnectionId, executable: Option<String>) {
        if let Some(connection) = self.connections.get_mut(&id) {
            connection.executable = executable;
        }
    }

    /// Sets the shutdown policy, and whether a headless job is running.
    pub fn set_shutdown(&mut self, policy: OnLastClientClosed, headless: bool) {
        self.shutdown = Shutdown { policy, headless };
    }

    /// Says that UXDuck runs on a desktop with its menu-bar helper, which
    /// the composition root knows (S107).
    pub fn set_desktop(&mut self, desktop: bool) {
        self.opened.desktop = desktop;
    }

    /// Whether the daemon should quit now: the policy says quit, no window
    /// (an operator UI or the shell) is connected, and nothing must keep
    /// running: no headless job and no keep-alive link. The caller waits the
    /// grace period before asking, so a reload does not quit.
    ///
    /// On a desktop with the menu-bar icon switched off, and no close
    /// policy chosen, the last window closing quits too (ADR-0025 D1):
    /// otherwise an invisible UXDuck would hold its ports. Only once a
    /// window has been open, so starting at login keeps running.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        // A deliberate queued update is background work even with the menu
        // bar hidden. Explicit Quit still cancels it through daemon/stop.
        if self.app_update_pending() {
            return false;
        }
        let window_open = self.connections.values().any(|c| {
            c.admitted
                .as_ref()
                .is_some_and(|a| matches!(a.kind, ClientKind::OperatorUi | ClientKind::Shell))
        });
        let invisible = self.opened.desktop
            && self.opened.window_seen
            && !self.state.settings.show_in_menu_bar
            && self.state.settings.close_policy.is_none();
        (invisible || self.close_policy() == uxduck_core::domain::settings::ClosePolicy::Quit)
            && !self.shutdown.headless
            && !window_open
            && !self.state.links.values().any(|l| l.keep_alive)
    }

    pub fn disconnect(&mut self, id: ConnectionId) {
        self.disconnect_display(id);
        self.abandon_approvals(id);
        self.release_claims_of(id);
        self.release_leases_of(id);
        self.drop_video_watches(id);
        self.stop_finding_for(id);
        self.drop_remote_watch(id);
        self.feature_watchers.remove(&id);
        self.item_watchers.remove(&id);
        self.location_disconnected(id);
        self.tray_watchers.remove(&id);
        self.messages.forget(id);
        self.connections.remove(&id);
        self.tracers.remove(&id);
        self.telemetry_subscribers.remove(&id);
        self.subscribers.remove(&id);
        self.review_idle_simulations();
    }

    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// One frame from a connection: tap it, dispatch it, reply.
    pub fn receive(&mut self, id: ConnectionId, text: &str) {
        let now = self.clock.now_us();
        self.expire_display_pulls(now);
        let parsed = trace::parse(text);
        if parsed.kind == FrameKind::Request
            && let (Some(request_id), Some(connection)) =
                (parsed.id.clone(), self.connections.get_mut(&id))
        {
            connection.pending.insert(request_id, now);
        }
        self.tap(id, TraceDirection::In, &parsed, text.len(), None);
        if let Some(reply) = rpc::router::dispatch(self, id, text) {
            self.send(id, &reply);
        }
        self.review_idle_simulations();
    }

    /// Journals one change and pushes it to subscribers: the only way state
    /// changes. Returns the event.
    fn commit(&mut self, author: ConnectionId, change: Change) -> Event {
        let actor = self.actor(author);
        self.commit_as(actor, change)
    }

    fn actor(&self, connection: ConnectionId) -> Actor {
        self.connections
            .get(&connection)
            .map_or_else(Actor::default, |connection| Actor {
                client_id: connection.client_id.clone(),
                operator_id: connection.operator_id.clone(),
            })
    }

    fn commit_as(&mut self, actor: Actor, change: Change) -> Event {
        let event = Event {
            seq: self.state.seq + 1,
            ts_us: self.clock.now_us(),
            actor,
            change,
        };
        self.persistence.append(&event);
        // Built with `seq + 1`, so it always applies.
        let _ = self.state.apply(&event);
        self.since_checkpoint += 1;
        if self.since_checkpoint >= self.config.checkpoint_every {
            self.checkpoint();
        }
        self.broadcast(&event);
        self.wake_sync_for(&event);
        self.review_idle_simulations();
        self.push_tray();
        event
    }

    /// Sends one `state/delta` per event to every subscriber, keeping only
    /// the changes to types it subscribed to, so its `seq` stays contiguous.
    fn broadcast(&mut self, event: &Event) {
        let subscribers: Vec<(ConnectionId, BTreeSet<EntityType>)> = self
            .subscribers
            .iter()
            .map(|(id, types)| (*id, types.clone()))
            .collect();
        for (id, types) in subscribers {
            let changes = match &event.change {
                Change::SettingsChanged { settings } => vec![self.settings_change(settings)],
                change => entity_changes(change),
            }
            .into_iter()
            .filter(|change| types.contains(&change.entity))
            .collect();
            let delta = StateDelta {
                seq: event.seq,
                ts_us: event.ts_us,
                actor: (&event.actor).into(),
                changes,
            };
            let frame = rpc::router::notification(&ServerNotification::StateDelta(delta));
            self.send(id, &frame);
        }
    }

    /// Serialises and sends one frame to a connection, through the tap.
    fn send(&mut self, id: ConnectionId, frame: &Value) {
        let text = frame.to_string();
        let parsed = trace::classify(frame);
        let latency = match (&parsed.id, self.connections.get_mut(&id)) {
            (Some(request_id), Some(connection))
                if matches!(parsed.kind, FrameKind::Response | FrameKind::Error) =>
            {
                connection
                    .pending
                    .remove(request_id)
                    .map(|arrived| self.clock.now_us().saturating_sub(arrived))
            }
            _ => None,
        };
        self.tap(id, TraceDirection::Out, &parsed, text.len(), latency);
        if let Some(connection) = self.connections.get(&id) {
            // A closed outbox means the connection is going away; the listener
            // reports the disconnect.
            let _ = connection.outbox.unbounded_send(text);
        }
    }

    /// Mirrors a frame to every `trace/subscribe`r except the connection it
    /// belongs to. Trace frames themselves are not traced.
    fn tap(
        &mut self,
        id: ConnectionId,
        direction: TraceDirection,
        parsed: &Parsed,
        bytes: usize,
        latency_us: Option<u64>,
    ) {
        let now = self.clock.now_us();
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        let frame = TraceFrame {
            ts_us: now,
            direction,
            kind: parsed.kind,
            client_id: connection.client_id.clone(),
            transport: connection.transport,
            method: parsed.method.clone(),
            id: parsed.id.clone(),
            bytes: bytes as u64,
            latency_us,
            payload: parsed.payload.clone(),
        };
        if self.tracers.is_empty() {
            feedback::keep(&mut connection.recent, frame, now);
            return;
        }
        feedback::keep(&mut connection.recent, frame.clone(), now);
        let notification =
            rpc::router::notification(&ServerNotification::TraceFrame(frame)).to_string();
        for tracer in self.tracers.iter().filter(|tracer| **tracer != id) {
            if let Some(tracer) = self.connections.get(tracer) {
                let _ = tracer.outbox.unbounded_send(notification.clone());
            }
        }
    }

    // Browser sessions, for the loopback listener's HTTP routes.

    /// A one-time token for a page on `ui_url`, as `session/createBootstrap` makes.
    pub fn create_bootstrap(&mut self) -> String {
        let origin = self.config.ui_url.clone();
        self.sessions
            .create_bootstrap(&origin, self.clock.as_ref(), self.random.as_mut())
    }

    /// The UI's address with a fresh bootstrap token, as `uxduck open`
    /// opens it (S107: "Open in Chrome" from Safari).
    pub fn bootstrap_url(&mut self) -> String {
        let token = self.create_bootstrap();
        format!("{}?bootstrap={token}", self.config.ui_url)
    }

    /// Turns a bootstrap token into a session token, once.
    pub fn exchange_bootstrap(&mut self, token: &str) -> Option<String> {
        let first = !self.sessions.has_sessions();
        let session = self.sessions.exchange_bootstrap(
            token,
            self.clock.as_ref(),
            self.random.as_mut(),
            self.persistence.as_mut(),
        );
        if first && session.is_some() {
            self.track(uxduck_contracts::dto::analytics::DaemonEvent::DaemonPaired);
        }
        session
    }

    /// A browser asks to pair: it shows the code, and every operator's app
    /// gets the question with Allow and Deny, so nobody needs a terminal.
    pub fn request_pairing(
        &mut self,
        origin: &str,
        user_agent: Option<String>,
    ) -> (String, String) {
        self.ask_pairing(origin, user_agent, None)
            .unwrap_or_default()
    }

    /// A phone or tablet on the network asks to pair (S97), from `peer`.
    /// None while too many of those wait for an answer already.
    pub fn request_network_pairing(
        &mut self,
        origin: &str,
        user_agent: Option<String>,
        peer: String,
    ) -> Option<(String, String)> {
        self.ask_pairing(origin, user_agent, Some(peer))
    }

    fn ask_pairing(
        &mut self,
        origin: &str,
        user_agent: Option<String>,
        peer: Option<String>,
    ) -> Option<(String, String)> {
        let asked = self.sessions.request_pairing(
            origin,
            user_agent,
            peer,
            self.clock.as_ref(),
            self.random.as_mut(),
        );
        self.sync_pairing_questions();
        asked
    }

    /// Keeps the "asks to connect" notifications in step with the browsers
    /// waiting: one per browser and site, gone once it is answered, closed
    /// or expired, so reloads and test runs do not pile them up.
    pub(crate) fn sync_pairing_questions(&mut self) {
        let questions = self.sessions.questions(self.clock.as_ref());
        let asked: Vec<String> = questions
            .iter()
            .map(|question| pairing::condition(&question.code))
            .collect();
        let stale: Vec<String> = self
            .state
            .notifications
            .values()
            .filter_map(|n| n.condition.clone())
            .filter(|c| c.starts_with("pairing:") && !asked.contains(c))
            .collect();
        for condition in stale {
            self.resolve(&condition);
        }
        for question in questions {
            self.raise(pairing::question(&question));
        }
    }

    /// Drops pairing requests nobody is waiting on any more, with their questions.
    pub fn expire_pairings(&mut self) {
        self.sync_pairing_questions();
    }

    pub fn poll_pairing(&mut self, id: &str) -> PairingState {
        let first = !self.sessions.has_sessions();
        let state = self.sessions.poll_pairing(
            id,
            self.clock.as_ref(),
            self.random.as_mut(),
            self.persistence.as_mut(),
        );
        if first && matches!(state, PairingState::Approved { .. }) {
            self.track(uxduck_contracts::dto::analytics::DaemonEvent::DaemonPaired);
        }
        if let PairingState::Approved {
            session,
            network: Some(network),
        } = &state
        {
            self.network_client_paired(&pairing::network_client(session), network);
        }
        self.sync_pairing_questions();
        state
    }

    /// A phone or tablet paired on the network gets its own client record
    /// with what it was allowed (S97), listed and editable at the computer.
    fn network_client_paired(&mut self, client: &str, network: &pairing::NetworkPairing) {
        let record = uxduck_core::domain::client::ClientRecord {
            id: client.to_owned(),
            // Two phones of a kind told apart by where they paired from.
            name: network.peer.as_ref().map_or_else(
                || network.device.clone(),
                |peer| format!("{} ({peer})", network.device),
            ),
            kind: uxduck_core::domain::client::ClientKind::NetworkUi,
            executable: None,
            capabilities: network.access.capabilities(),
            created_at_us: self.clock.now_us(),
            version: 1,
        };
        self.commit_as(
            uxduck_core::events::Actor {
                client_id: "daemon".into(),
                operator_id: None,
            },
            uxduck_core::events::Change::ClientUpserted { client: record },
        );
    }

    #[must_use]
    pub fn session(&self, token: &str) -> Option<Session> {
        self.sessions.session(token).cloned()
    }

    /// The host's word on this computer's addresses and the network
    /// listener (S97), for `network/status`.
    pub fn set_network(&mut self, status: uxduck_contracts::dto::network::NetworkStatus) {
        self.network = status;
    }

    /// The host's word on the network, with the phones and tablets that
    /// are connected now.
    #[must_use]
    pub fn network(&self) -> uxduck_contracts::dto::network::NetworkStatus {
        let mut connected: Vec<String> = self
            .connections
            .values()
            .filter(|c| matches!(c.trust, Trust::NetworkBrowser { .. }))
            .filter_map(|c| c.admitted.as_ref().map(|a| a.record.clone()))
            .collect();
        connected.sort();
        connected.dedup();
        uxduck_contracts::dto::network::NetworkStatus {
            connected,
            ..self.network.clone()
        }
    }

    /// Unpairs a phone or tablet (S97): its record and session go, and an
    /// open page loses its connection and is asked to pair again.
    pub(crate) fn forget_network_client(&mut self, id: &str) -> bool {
        let known = self
            .state
            .clients
            .get(id)
            .is_some_and(|c| c.kind == uxduck_core::domain::client::ClientKind::NetworkUi);
        if !known {
            return false;
        }
        self.sessions
            .forget_network_client(id, self.persistence.as_mut());
        self.commit_as(
            uxduck_core::events::Actor {
                client_id: "daemon".into(),
                operator_id: None,
            },
            uxduck_core::events::Change::ClientRemoved { id: id.to_owned() },
        );
        let open: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| c.admitted.as_ref().is_some_and(|a| a.record == id))
            .map(|(connection, _)| *connection)
            .collect();
        // Dropping its outbox ends the socket (`serve_websocket`).
        for connection in open {
            self.disconnect(connection);
        }
        true
    }

    /// A phone or tablet on the network is admitted with its record (S97):
    /// what it was allowed when it paired, as changed since. None when it
    /// was forgotten: it must pair again.
    pub(crate) fn admit_network(&mut self, client: &str) -> Option<Vec<Capability>> {
        self.state
            .clients
            .get(client)
            .map(|record| record.capabilities.clone())
    }
}

/// A client record's delta: upserted, or forgotten (S97).
fn client_change(change: &Change) -> Option<EntityChange> {
    match change {
        Change::ClientUpserted { client } => Some(EntityChange {
            client: Some(uxduck_contracts::dto::client::ClientRecord::from(client)),
            ..upsert(EntityType::Client, &client.id)
        }),
        Change::ClientRemoved { id } => Some(EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Client, id)
        }),
        _ => None,
    }
}

/// An upsert of `entity` with every value field empty; the caller fills one.
pub(crate) fn upsert(entity: EntityType, id: &str) -> EntityChange {
    EntityChange {
        entity,
        id: id.to_owned(),
        op: ChangeOp::Upsert,
        operator: None,
        thread: None,
        item: None,
        folder: None,
        link: None,
        vehicle: None,
        receipt: None,
        notification: None,
        client: None,
        approval: None,
        parameter_set: None,
        parameter: None,
        mission: None,
        controller: None,
        input_mapping: None,
        settings: None,
        plugin: None,
        place: None,
        plan: None,
        snapshot: None,
        param_definitions: None,
        history_entry: None,
        video_source: None,
        video_recording: None,
        video_photo: None,
        practice_attempt: None,
        file_transfer: None,
    }
}

/// The wire form of one journal change.
#[allow(clippy::too_many_lines, reason = "one arm per kind of change")]
fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::OperatorUpserted { operator } => vec![EntityChange {
            operator: Some(OperatorDto::from(operator)),
            ..upsert(EntityType::Operator, &operator.id)
        }],
        Change::OperatorRemoved { id } => {
            vec![EntityChange {
                op: ChangeOp::Remove,
                ..upsert(EntityType::Operator, id)
            }]
        }
        Change::ThreadUpserted { thread } => vec![EntityChange {
            thread: Some(ThreadDto::from(thread)),
            ..upsert(EntityType::Thread, &thread.id)
        }],
        Change::ItemUpserted { item } => vec![EntityChange {
            item: Some(ItemDto::from(item)),
            ..upsert(EntityType::Item, &item.id)
        }],
        Change::FolderUpserted { folder } => vec![EntityChange {
            folder: Some(FolderDto::from(folder)),
            ..upsert(EntityType::Folder, &folder.id)
        }],
        Change::LinkUpserted { link } => vec![EntityChange {
            link: Some(LinkDto::from(link)),
            ..upsert(EntityType::Link, &link.id)
        }],
        Change::LinkRemoved { id } => {
            vec![EntityChange {
                op: ChangeOp::Remove,
                ..upsert(EntityType::Link, id)
            }]
        }
        Change::VehicleUpserted { vehicle } => vec![EntityChange {
            vehicle: Some(VehicleDto::from(vehicle)),
            ..upsert(EntityType::Vehicle, &vehicle.id)
        }],
        Change::VehicleRemoved { id } => param_definitions::removed_with_vehicle(id),
        Change::ReceiptUpserted { receipt } => vec![EntityChange {
            receipt: Some(uxduck_contracts::dto::command::Receipt::from(receipt)),
            ..upsert(EntityType::Receipt, &receipt.id)
        }],
        change @ (Change::ClientUpserted { .. } | Change::ClientRemoved { .. }) => {
            client_change(change).into_iter().collect()
        }
        Change::ApprovalUpserted { approval } => vec![approval_change(approval)],
        change @ (Change::ParameterSetUpserted { .. }
        | Change::ParametersReplaced { .. }
        | Change::ParameterChanged { .. }
        | Change::MissionUpserted { .. }) => transfers::entity_changes(change),
        change @ (Change::ControllerUpserted { .. }
        | Change::ControllerRemoved { .. }
        | Change::InputMappingUpserted { .. }) => controllers::entity_changes(change),
        change @ (Change::PluginUpserted { .. } | Change::PluginRemoved { .. }) => {
            plugins::entity_changes(change)
        }
        change @ (Change::PlaceUpserted { .. } | Change::PlaceRemoved { .. }) => {
            search::entity_changes(change)
        }
        change @ (Change::PlanSaved { .. } | Change::PlanRemoved { .. }) => {
            plans::entity_changes(change)
        }
        change @ (Change::SnapshotUpserted { .. } | Change::SnapshotRemoved { .. }) => {
            snapshots::entity_changes(change)
        }
        change @ (Change::FileTransferUpserted { .. } | Change::FileTransferRemoved { .. }) => {
            vehicle_files::entity_changes(change)
        }
        Change::PracticeAttemptUpserted { attempt } => vec![EntityChange {
            practice_attempt: Some(attempt.into()),
            ..upsert(EntityType::PracticeAttempt, &attempt.vehicle_id)
        }],
        change @ (Change::ParamDefinitionsChosen { .. }
        | Change::OwnSetUpserted { .. }
        | Change::OwnSetRemoved { .. }) => param_definitions::entity_changes(change),
        change @ (Change::HistoryUpserted { .. } | Change::HistoryCleared { .. }) => {
            history::entity_changes(change)
        }
        change @ (Change::VideoSourceUpserted { .. } | Change::VideoSourceRemoved { .. }) => {
            video::entity_changes(change)
        }
        change @ (Change::VideoRecordingUpserted { .. } | Change::VideoRecordingRemoved { .. }) => {
            recordings::entity_changes(change)
        }
        change @ (Change::VideoPhotoUpserted { .. } | Change::VideoPhotoRemoved { .. }) => {
            photos::entity_changes(change)
        }
        // Settings need the engine's default close policy, so `broadcast`
        // builds them; sync is read with `sync/get`, not pushed (S16).
        Change::SettingsChanged { .. }
        | Change::SyncChanged { .. }
        | Change::EdgeConnection { .. }
        | Change::WatchLinkCreated { .. }
        | Change::WatchLinkRevoked { .. }
        | Change::FlightStored { .. }
        | Change::VideoLost { .. }
        | Change::LibraryUpserted { .. }
        | Change::PluginCalled { .. }
        | Change::PluginPublished { .. }
        | Change::PluginDeclined { .. }
        // Read with `mission/history` (S134).
        | Change::MissionRecorded { .. } => Vec::new(),
        Change::NotificationUpserted { notification } => vec![notification_change(notification)],
    }
}

/// A pending approval, or its removal once decided.
fn approval_change(approval: &uxduck_core::domain::approval::Approval) -> EntityChange {
    if approval.state == uxduck_core::domain::approval::ApprovalState::Pending {
        EntityChange {
            approval: Some(uxduck_contracts::dto::client::Approval::from(approval)),
            ..upsert(EntityType::Approval, &approval.id)
        }
    } else {
        EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Approval, &approval.id)
        }
    }
}

/// A notification, or its removal once resolved.
fn notification_change(
    notification: &uxduck_core::domain::notification::Notification,
) -> EntityChange {
    if notification.state == uxduck_core::domain::notification::NotificationState::Resolved {
        EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Notification, &notification.id)
        }
    } else {
        EntityChange {
            notification: Some(uxduck_contracts::dto::notification::Notification::from(
                notification,
            )),
            ..upsert(EntityType::Notification, &notification.id)
        }
    }
}

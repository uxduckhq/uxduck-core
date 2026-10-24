//! The browser's composition root, without JavaScript (ADR-0011 M2): the
//! engine, the MAVLink session and what the engine's ports asked for. Each
//! call returns what the worker must do next, so the whole root runs and is
//! tested natively; `lib.rs` hands the outputs to JavaScript.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde::Serialize;
use uxduck_contracts::dto::analytics::{ControllerInputInterruption, ControllerInterruptionReason};
use uxduck_contracts::dto::controller::ControllerInputsResult;
use uxduck_contracts::dto::initialize::{DaemonCapabilities, RunsIn};
use uxduck_contracts::dto::inspect::{
    InspectDirection, MavlinkHistoryParams, MavlinkHistoryResult, MavlinkInspectResult,
};
use uxduck_contracts::dto::param_definitions::CatalogueSet;
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::link::{JET_FRAME, Link, LinkKind, LinkStatus, SimVehicle};
use uxduck_core::domain::param_definitions::DefinitionSet;
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::controller_input::{Controls, InputEvent, Report};
use uxduck_engine::mavlink_inspect::{self, Inspector};
use uxduck_engine::param_definitions::files;
use uxduck_engine::param_definitions::{DefinitionsArrived, DefinitionsOrder, DefinitionsPort};
use uxduck_engine::plane::{self, Routed};
use uxduck_engine::runtime::{
    AgentPort, Clock, CommandOrder, ControlRoute, Credentials, HistoryOf, LinkPort, NoPlugins,
    NoSystem, Persistence, Ports, Random, SerialPortInfo, System, TransferOrder, TurnRequest,
};
use uxduck_engine::state::{self, State};
use uxduck_engine::terrain::TerrainPort;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, HostAbout, Loaded};
use uxduck_mavlink::framing::Reassembler;
use uxduck_mavlink::session::{Config, Effect, Session};
use uxduck_terrain::manifest::DEFAULT_BASE_URL;
use uxduck_terrain::{Arrived, FetchError};

use crate::gamepads::{self, Pads};

/// The simulator is one peer on its link: its serial port.
const PEER: &str = "serial0";
/// Approvals expire on this beat, as in the daemon.
const EXPIRY_EVERY_US: u64 = 1_000_000;
/// Pilot input goes at most this often: 25 Hz, as in the daemon.
const PILOT_EVERY_US: u64 = 35_000;
/// Shown when no account is bound to the sending operator.
pub const NO_ACCOUNT: &str = "Sign in with this operator’s account to use AI. Request access from your account page if needed.";

/// Readiness is a hint only. The HTTP transport and API authorize each request.
struct AccountCredentials(Arc<AtomicBool>);
impl Credentials for AccountCredentials {
    fn set(&mut self, _: &str, _: &str) -> Result<(), String> {
        Err("Browser AI uses the signed-in account; provider keys are never stored here.".into())
    }
    fn has(&self, name: &str) -> bool {
        name == uxduck_engine::ACCOUNT_TOKEN && self.0.load(Ordering::SeqCst)
    }
}

/// What the worker must do after a call.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Hosted inference only; tools stay inside the capability-checked engine.
    Agent(crate::agent::Output),
    /// A JSON-RPC frame for one of the page's connections.
    Post { connection: u32, text: String },
    /// A line for the journal in the browser's storage.
    Journal(Write),
    /// Start or stop a simulated vehicle.
    Simulator(Order),
    /// Bytes for a simulator's serial port.
    LinkWrite { link_id: String, bytes: Vec<u8> },
    /// Fetch this URL and hand the bytes to [`Browser::fetched`] under
    /// `request`: the parameter definitions catalogue and its sets (S67),
    /// and terrain files (S23). A network failure's error starts with
    /// [`OFFLINE`].
    Fetch { request: u32, url: String },
    /// POST this path and JSON body through the worker's configured UXDuck
    /// place API, then return its bytes through `Browser::fetched`.
    PlaceLookup {
        request: u32,
        path: String,
        body: String,
    },
    /// Keep, read or remove terrain files in the Origin Private File
    /// System (S23).
    Store(Store),
}

/// What a failed fetch's error starts with when there was no connection,
/// as opposed to the server refusing.
pub const OFFLINE: &str = "offline: ";

/// A terrain file in the browser's storage, by its path in the bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Store {
    /// Read it and hand the bytes, or none, to [`Browser::stored`].
    Read {
        request: u32,
        key: String,
    },
    Write {
        key: String,
        bytes: Vec<u8>,
    },
    /// Remove every file under the prefix (a withdrawn release).
    Remove {
        prefix: String,
    },
}

/// An address the page saves these bytes from, as a download: a blob URL
/// in the browser.
pub type SaveUrl = fn(&[u8]) -> Option<String>;
/// Lets go of an address [`SaveUrl`] made.
pub type Revoke = fn(&str);

/// Copies of vehicles' files in the worker's memory (S121, ADR-0027 F5):
/// the browser's "Vehicle files", gone with the page. A copy is whole once
/// finished; the page saves it from a blob URL, one at a time per copy.
struct MemoryFiles {
    copies: HashMap<String, (Vec<u8>, bool)>,
    /// The address last given for each copy, let go of when replaced.
    granted: HashMap<String, String>,
    save: SaveUrl,
    revoke: Revoke,
}

impl std::fmt::Debug for MemoryFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryFiles")
            .field("copies", &self.copies.len())
            .finish_non_exhaustive()
    }
}

impl MemoryFiles {
    fn let_go(&mut self, local: &str) {
        if let Some(url) = self.granted.remove(local) {
            (self.revoke)(&url);
        }
    }
}

impl uxduck_engine::vehicle_files::FilesPort for MemoryFiles {
    fn copied(&self, local: &str) -> u64 {
        self.copies
            .get(local)
            .map_or(0, |(bytes, _)| bytes.len() as u64)
    }

    fn write(&mut self, local: &str, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let (copy, whole) = self.copies.entry(local.to_owned()).or_default();
        let at = usize::try_from(offset).map_err(|e| e.to_string())?;
        if copy.len() < at {
            return Err(format!(
                "the copy has {} bytes, not the {at} these follow",
                copy.len()
            ));
        }
        copy.truncate(at);
        copy.extend_from_slice(bytes);
        *whole = false;
        Ok(())
    }

    fn finish(&mut self, local: &str) -> Result<(), String> {
        self.copies.entry(local.to_owned()).or_default().1 = true;
        Ok(())
    }

    fn discard(&mut self, local: &str) {
        self.let_go(local);
        self.copies.remove(local);
    }

    fn grant(&mut self, local: &str, _name: &str) -> Option<String> {
        self.let_go(local);
        let (bytes, whole) = self.copies.get(local)?;
        if !whole {
            return None;
        }
        let url = (self.save)(bytes)?;
        self.granted.insert(local.to_owned(), url.clone());
        Some(url)
    }
}

/// What the journal keeps (ADR-0007): events in order, and now and then the
/// whole state so a visit need not replay everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Event(String),
    Checkpoint { seq: u64, state: String },
}

/// A simulated vehicle to start or stop, as JSON for the worker.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Order {
    Start {
        link_id: String,
        /// ArduPilot's program: `plane`, `copter`, `rover` or `sub`.
        program: &'static str,
        /// ArduPilot's `--model`: the frame.
        model: String,
        /// Times real time.
        speedup: u8,
        latitude_deg: f64,
        longitude_deg: f64,
        altitude_m: f64,
        heading_deg: f64,
        /// ArduPilot's `--sysid`: one past the link's instance, so two
        /// simulators in one browser are two vehicles.
        sysid: u8,
        /// ArduPilot's simulated ADS-B receiver, when asked for (ADR-0034).
        traffic: bool,
    },
    Stop {
        link_id: String,
    },
    /// Freeze a simulated vehicle where it is, or let it run again.
    Pause {
        link_id: String,
        paused: bool,
    },
}

/// What the engine's ports asked for, done after each engine call.
#[derive(Debug)]
enum Call {
    Open(Link),
    Close(String),
    Command(CommandOrder),
    Transfer(TransferOrder),
    /// The system id UXDuck sends as, from its settings.
    GcsSystemId(u8),
    /// A simulator frozen or let run again (`sim/pause`).
    Pause(String, bool),
    Journal(Write),
    Turn(TurnRequest),
    CancelTurn(String),
    Definitions(DefinitionsOrder),
    Terrain(uxduck_terrain::Order),
    Places(uxduck_engine::places::Order),
    /// What a controller drives now, as the engine decided.
    Route(String, Option<ControlRoute>),
    Input(InputEvent),
}

type Queue = Arc<Mutex<VecDeque<Call>>>;

fn push(queue: &Queue, call: Call) {
    queue
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(call);
}

/// The engine's journal: handed to the worker as lines.
#[derive(Debug)]
struct Journal(Queue);

impl Persistence for Journal {
    fn append(&mut self, event: &Event) {
        if let Ok(line) = serde_json::to_string(event) {
            push(&self.0, Call::Journal(Write::Event(line)));
        }
    }
    fn checkpoint(&mut self, state: &State) {
        if let Ok(json) = serde_json::to_string(state) {
            push(
                &self.0,
                Call::Journal(Write::Checkpoint {
                    seq: state.seq,
                    state: json,
                }),
            );
        }
    }
    /// Pages reach this daemon through their own worker, never by pairing.
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

/// The engine's links: only simulators exist here.
#[derive(Debug)]
struct Links(Queue);

impl LinkPort for Links {
    fn open(&mut self, link: &Link) {
        push(&self.0, Call::Open(link.clone()));
    }
    fn close(&mut self, link_id: &str) {
        push(&self.0, Call::Close(link_id.to_owned()));
    }
    fn command(&mut self, order: CommandOrder) {
        push(&self.0, Call::Command(order));
    }
    fn transfer(&mut self, order: TransferOrder) {
        push(&self.0, Call::Transfer(order));
    }
    fn gcs_system_id(&mut self, id: u8) {
        push(&self.0, Call::GcsSystemId(id));
    }
    fn pause_simulation(&mut self, link_id: &str, paused: bool) {
        push(&self.0, Call::Pause(link_id.to_owned(), paused));
    }
    fn controller_input(&mut self, event: InputEvent) {
        push(&self.0, Call::Input(event));
    }
    fn route_controller(&mut self, controller_id: &str, route: Option<ControlRoute>) {
        push(&self.0, Call::Route(controller_id.to_owned(), route));
    }
}

/// The agent: the hosted model after sign-in, which is not there yet.
#[derive(Debug)]
struct Agent(Queue);

impl AgentPort for Agent {
    fn start_turn(&mut self, turn: TurnRequest) {
        push(&self.0, Call::Turn(turn));
    }
    fn cancel_turn(&mut self, thread_id: &str) {
        push(&self.0, Call::CancelTurn(thread_id.to_owned()));
    }
}

/// Parameter definitions (S67): fetched by the worker, checked here.
#[derive(Debug)]
struct Definitions(Queue);

impl DefinitionsPort for Definitions {
    fn request(&mut self, order: DefinitionsOrder) {
        push(&self.0, Call::Definitions(order));
    }
}

/// Terrain (S23): fetched and kept by the worker, checked by the engine.
#[derive(Debug)]
struct TerrainFiles(Queue);

struct PlaceLookup(Queue);

impl uxduck_engine::places::PlacePort for PlaceLookup {
    fn request(&mut self, order: uxduck_engine::places::Order) {
        push(&self.0, Call::Places(order));
    }
}

impl TerrainPort for TerrainFiles {
    fn order(&mut self, order: uxduck_terrain::Order) {
        push(&self.0, Call::Terrain(order));
    }
}

/// What a fetch in flight is for.
#[derive(Debug)]
enum Fetching {
    Place(uxduck_engine::places::Order),
    /// A terrain file, by its key.
    Terrain(String),
    Manifest,
    Signature,
    Set(Box<CatalogueSet>),
    /// A person's set from its URL (S74).
    Own(String),
}

/// The browser's clock, shared by the root and its host.
#[derive(Clone)]
struct SharedClock(Arc<Mutex<Box<dyn Clock>>>);

impl Clock for SharedClock {
    fn now_us(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .now_us()
    }
}

/// The browser as a host: nothing to report, as [`NoSystem`], except the
/// MAVLink inspector's table, which the root records frames in, and the
/// gamepads the page reads.
struct Host {
    none: NoSystem,
    inspector: Arc<Mutex<Inspector>>,
    clock: SharedClock,
    inputs: Arc<Mutex<ControllerInputsResult>>,
}

impl System for Host {
    fn check_udp_bind(&self, host: &str, port: u16) -> Result<(), String> {
        self.none.check_udp_bind(host, port)
    }
    fn is_local_address(&self, host: &str) -> bool {
        self.none.is_local_address(host)
    }
    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        self.none.serial_ports()
    }
    fn wake_lock(&mut self, hold: bool) {
        self.none.wake_lock(hold);
    }
    fn controller_inputs(&self) -> ControllerInputsResult {
        self.inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    fn mavlink_inspect(&self, vehicle_ids: &[String]) -> MavlinkInspectResult {
        let now_us = self.clock.now_us();
        self.inspector
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .read(vehicle_ids, now_us)
            .decode(None)
    }
    fn vehicle_heartbeat_us(&self, vehicle_ids: &[String]) -> Option<u64> {
        self.inspector
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .heartbeat_us(vehicle_ids)
    }
    /// Earlier messages both ways from the table's own history: the
    /// browser writes no tlogs.
    fn mavlink_history(
        &self,
        vehicle: &HistoryOf<'_>,
        params: &MavlinkHistoryParams,
    ) -> Result<MavlinkHistoryResult, ApiError> {
        if params.detailed() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "Queries by link/time and exact frame bytes require installed UXDuck.",
            ));
        }
        let earlier = self
            .inspector
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .history(
                vehicle.vehicle_ids,
                params.direction,
                params.id,
                params.before.as_deref(),
                mavlink_inspect::page_size(params.limit),
            )
            .map_err(|_| {
                ApiError::new(ErrorKind::InvalidValue, "not a cursor from this history")
            })?;
        Ok(earlier.decode(None))
    }
}

/// The definitions catalogue as it arrives: its manifest and signature,
/// which come separately.
#[derive(Debug, Default)]
struct Arriving {
    manifest: Option<Result<Vec<u8>, String>>,
    signature: Option<Result<Vec<u8>, String>>,
}

/// Why the journal could not be read; the worker starts afresh and says so.
#[derive(Debug)]
pub struct Unreadable(pub String);

/// The browser's daemon.
pub struct Browser {
    engine: Engine,
    agents: crate::agent::Runtime,
    accounts: HashMap<ConnectionId, crate::agent::Account>,
    account_ready: Arc<AtomicBool>,
    session: Session,
    clock: Box<dyn Clock>,
    /// The latest message of each type to and from each vehicle, for
    /// `mavlink/inspect`.
    inspector: Arc<Mutex<Inspector>>,
    queue: Queue,
    connections: BTreeMap<u32, (ConnectionId, UnboundedReceiver<String>)>,
    next_connection: u32,
    /// Byte streams from each simulator, cut into frames.
    streams: HashMap<String, Reassembler>,
    last_expiry_us: u64,
    /// Where the definitions catalogue is, and the key it must be signed
    /// with; without both, there is none.
    catalogue: (Option<String>, Option<String>),
    fetches: BTreeMap<u32, Fetching>,
    next_fetch: u32,
    arriving: Arriving,
    /// Where terrain is served (S23).
    terrain_url: String,
    /// Terrain files being read from storage, by request.
    reading: BTreeMap<u32, String>,
    /// Sets people added, for this visit: the browser keeps them nowhere
    /// else yet.
    own_sets: HashMap<String, DefinitionSet>,
    /// Gamepads the pages read, and each one's input and route.
    pads: Pads,
    controls: Controls,
    /// What `controller/inputs` returns, shared with the host.
    inputs: Arc<Mutex<ControllerInputsResult>>,
    last_pilot_us: u64,
    last_tick_us: Option<u64>,
    last_tick_gap_ms: u32,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("connections", &self.connections.len())
            .finish_non_exhaustive()
    }
}

/// Writes a checkpoint after this many events.
const CHECKPOINT_EVERY: u64 = 500;

impl Browser {
    /// Restores from the journal (the last checkpoint, if any, then the
    /// events after it, one JSON per line) and starts the engine, which
    /// reopens any simulator that was running.
    ///
    /// # Errors
    /// When the checkpoint is not a state; unreadable event lines are
    /// skipped, and replay stops at the first gap, as in the daemon.
    pub fn new(
        version: &str,
        clock: Box<dyn Clock>,
        engine_clock: Box<dyn Clock>,
        random: Box<dyn Random>,
        checkpoint: Option<&str>,
        events: &str,
    ) -> Result<(Self, Vec<Output>), Unreadable> {
        let checkpoint = checkpoint
            .map(serde_json::from_str::<State>)
            .transpose()
            .map_err(|e| Unreadable(format!("the checkpoint: {e}")))?;
        let events = events
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<Event>(line).ok());
        let restored = state::restore(checkpoint, events);

        let queue: Queue = Arc::default();
        let clock = SharedClock(Arc::new(Mutex::new(clock)));
        let inspector: Arc<Mutex<Inspector>> = Arc::default();
        let inputs: Arc<Mutex<ControllerInputsResult>> = Arc::default();
        let account_ready = Arc::new(AtomicBool::new(false));
        let ports = Ports {
            persistence: Box::new(Journal(queue.clone())),
            agent: Box::new(Agent(queue.clone())),
            credentials: Box::new(AccountCredentials(account_ready.clone())),
            links: Box::new(Links(queue.clone())),
            system: Box::new(Host {
                none: NoSystem::default(),
                inspector: inspector.clone(),
                clock: clock.clone(),
                inputs: inputs.clone(),
            }),
            plugins: Box::new(NoPlugins),
        };
        let config = EngineConfig {
            daemon_version: version.to_owned(),
            capabilities: DaemonCapabilities {
                runs_in: RunsIn::Browser,
                simulator: true,
                ..DaemonCapabilities::default()
            },
            ui_url: String::new(),
            checkpoint_every: CHECKPOINT_EVERY,
            models: vec!["gpt-6-luna".into(), "gpt-6-sol".into()],
            // ADR-0011 M9: the agent is the hosted model, after sign-in.
            agent_provider: "hosted".into(),
            // The wasm build's own stamp (`build.rs`); it keeps its data in
            // the page's storage and listens on no port.
            about: HostAbout {
                commit: option_env!("UXDUCK_GIT_SHA").map(str::to_owned),
                built_at: option_env!("UXDUCK_BUILT_AT").map(str::to_owned),
                target: env!("UXDUCK_TARGET").to_owned(),
                data_dir: None,
                port: None,
            },
        };
        let engine = Engine::new(
            config,
            engine_clock,
            random,
            ports,
            Loaded {
                state: restored.state,
                sessions: vec![],
            },
        );
        let mut browser = Self {
            engine,
            agents: crate::agent::Runtime::default(),
            accounts: HashMap::new(),
            account_ready,
            session: Session::new(Config::default()),
            clock: Box::new(clock),
            inspector,
            queue,
            connections: BTreeMap::new(),
            next_connection: 0,
            streams: HashMap::new(),
            last_expiry_us: 0,
            catalogue: (
                Some(files::SAME_ORIGIN_URL.to_owned()),
                option_env!("UXDUCK_RELEASE_PUBLIC_KEY")
                    .filter(|key| !key.trim().is_empty())
                    .map(str::to_owned),
            ),
            fetches: BTreeMap::new(),
            next_fetch: 0,
            arriving: Arriving::default(),
            terrain_url: DEFAULT_BASE_URL.to_owned(),
            reading: BTreeMap::new(),
            own_sets: HashMap::new(),
            pads: Pads::default(),
            controls: Controls::browser(),
            inputs,
            last_pilot_us: 0,
            last_tick_us: None,
            last_tick_gap_ms: 0,
        };
        let port = Definitions(browser.queue.clone());
        browser.engine.set_definitions(Box::new(port));
        let terrain = TerrainFiles(browser.queue.clone());
        browser.engine.set_terrain(Box::new(terrain), release_key());
        browser
            .engine
            .set_places(Box::new(PlaceLookup(browser.queue.clone())));
        let outputs = browser.drain();
        Ok((browser, outputs))
    }

    /// Keeps copies of vehicles' files in the worker's memory (S121), and
    /// gives the page a `save` address for each whole one: a blob URL in
    /// the browser. Without it, downloads say they need UXDuck Desktop.
    pub fn keep_files(&mut self, save: SaveUrl, revoke: Revoke) -> Vec<Output> {
        self.engine.set_files(Box::new(MemoryFiles {
            copies: HashMap::new(),
            granted: HashMap::new(),
            save,
            revoke,
        }));
        self.drain()
    }

    /// Terrain from elsewhere, checked with another key: for tests and
    /// development builds. Starts afresh.
    pub fn terrain_from(&mut self, url: &str, key: Option<String>) -> Vec<Output> {
        url.trim_end_matches('/').clone_into(&mut self.terrain_url);
        let terrain = TerrainFiles(self.queue.clone());
        self.engine.set_terrain(Box::new(terrain), key);
        self.drain()
    }

    /// A terrain file [`Store::Read`] asked for, or `None` when there is
    /// none.
    pub fn stored(&mut self, request: u32, bytes: Option<Vec<u8>>) -> Vec<Output> {
        if let Some(key) = self.reading.remove(&request) {
            self.engine.terrain_arrived(Arrived::Read { key, bytes });
        }
        self.drain()
    }

    /// A terrain file finished writing, or could not be saved.
    pub fn terrain_written(&mut self, key: String, result: Result<(), String>) -> Vec<Output> {
        self.engine
            .terrain_arrived(Arrived::Written { key, result });
        self.drain()
    }

    /// Clears cached terrain after the worker has removed its files.
    pub fn clear_terrain(&mut self) -> Vec<Output> {
        self.reading.clear();
        self.fetches
            .retain(|_, value| !matches!(value, Fetching::Terrain(_)));
        self.engine.clear_terrain_storage();
        self.drain()
    }

    /// Another definitions catalogue and key, or none: for tests and
    /// development builds.
    pub fn definitions_from(&mut self, url: Option<String>, key: Option<String>) {
        self.catalogue = (url, key.filter(|key| !key.trim().is_empty()));
    }

    /// The worker fetched what [`Output::Fetch`] asked for, or could not.
    pub fn fetched(&mut self, request: u32, result: Result<Vec<u8>, String>) -> Vec<Output> {
        match self.fetches.remove(&request) {
            Some(Fetching::Place(order)) => {
                let result = result
                    .map_err(|error| {
                        uxduck_engine::places::failed(
                            error.strip_prefix("http:").and_then(|s| s.parse().ok()),
                        )
                    })
                    .and_then(|bytes| uxduck_engine::places::response(&order.action, &bytes));
                self.engine.place_lookup_done(&order.id, result);
            }
            Some(Fetching::Manifest) => {
                self.arriving.manifest = Some(result);
                self.catalogue_arrived();
            }
            Some(Fetching::Signature) => {
                self.arriving.signature = Some(result);
                self.catalogue_arrived();
            }
            Some(Fetching::Set(set)) => {
                let result = result.and_then(|bytes| files::read_set(&set, &bytes));
                self.engine.definitions_arrived(DefinitionsArrived::Set {
                    id: set.id.clone(),
                    result,
                });
            }
            Some(Fetching::Terrain(key)) => {
                let result = result.map_err(|error| FetchError {
                    offline: error.starts_with(OFFLINE),
                    message: error.trim_start_matches(OFFLINE).to_owned(),
                });
                self.engine
                    .terrain_arrived(Arrived::Fetched { key, result });
            }
            Some(Fetching::Own(id)) => {
                let result = result.and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| "the set is not text".to_owned())
                });
                self.engine
                    .definitions_arrived(DefinitionsArrived::Fetched { id, result });
            }
            None => {}
        }
        self.drain()
    }

    /// Both halves of the catalogue are here: verified, or why not.
    fn catalogue_arrived(&mut self) {
        let (Some(manifest), Some(signature)) = (&self.arriving.manifest, &self.arriving.signature)
        else {
            return;
        };
        let key = self.catalogue.1.clone().unwrap_or_default();
        let result = match (manifest, signature) {
            (Ok(manifest), Ok(signature)) => {
                files::verify(manifest, &String::from_utf8_lossy(signature), &key)
                    .map(files::usable)
            }
            (Err(why), _) | (_, Err(why)) => Err(why.clone()),
        };
        self.arriving = Arriving::default();
        self.engine
            .definitions_arrived(DefinitionsArrived::Catalogue(result));
    }

    fn fetch(&mut self, url: String, what: Fetching, outputs: &mut Vec<Output>) {
        self.next_fetch += 1;
        self.fetches.insert(self.next_fetch, what);
        outputs.push(Output::Fetch {
            request: self.next_fetch,
            url,
        });
    }

    fn definitions(&mut self, order: DefinitionsOrder, outputs: &mut Vec<Output>) {
        match order {
            DefinitionsOrder::Catalogue { .. } => match self.catalogue.clone() {
                (Some(url), Some(_)) => {
                    self.arriving = Arriving::default();
                    self.fetch(format!("{url}.minisig"), Fetching::Signature, outputs);
                    self.fetch(url, Fetching::Manifest, outputs);
                }
                (None, _) => self
                    .engine
                    .definitions_arrived(DefinitionsArrived::Catalogue(Err(
                        "the definitions catalogue is off here".into(),
                    ))),
                (_, None) => self
                    .engine
                    .definitions_arrived(DefinitionsArrived::Catalogue(Err(
                        files::NO_CATALOGUE_KEY.into(),
                    ))),
            },
            DefinitionsOrder::Set(set) => {
                // Beside the catalogue this page fetched, not the URL the
                // catalogue names: another origin's file is unreadable here.
                let Some(catalogue) = self.catalogue.0.clone() else {
                    self.engine.definitions_arrived(DefinitionsArrived::Set {
                        id: set.id.clone(),
                        result: Err("the definitions catalogue is off here".into()),
                    });
                    return;
                };
                let url = files::beside(&catalogue, &set.file.name);
                self.fetch(url, Fetching::Set(set), outputs);
            }
            DefinitionsOrder::Own { id } => {
                let result = self.own_sets.get(&id).cloned().ok_or_else(|| {
                    "Added during an earlier visit. This browser keeps parameter definitions only while it is open."
                        .to_owned()
                });
                self.engine
                    .definitions_arrived(DefinitionsArrived::Set { id, result });
            }
            DefinitionsOrder::StoreOwn { id, set } => {
                self.own_sets.insert(id, *set);
            }
            DefinitionsOrder::FetchOwn { id, url } => self.fetch(url, Fetching::Own(id), outputs),
            DefinitionsOrder::RemoveOwn { id } => {
                self.own_sets.remove(&id);
            }
        }
    }

    /// The state as it is now.
    #[must_use]
    pub fn state(&self) -> &State {
        self.engine.state()
    }

    /// This browser's work as the changes that create it, for an installed
    /// daemon's `journal/import` (ADR-0011): operators, folders, chats with
    /// their messages in order, saved places and simulation snapshots.
    /// Simulated vehicles and their links stay here.
    #[must_use]
    pub fn export(&self) -> Vec<serde_json::Value> {
        let state = self.engine.state();
        let mut changes: Vec<Change> = Vec::new();
        changes.extend(
            state
                .operators
                .values()
                .map(|operator| Change::OperatorUpserted {
                    operator: operator.clone(),
                }),
        );
        changes.extend(state.folders.values().map(|folder| Change::FolderUpserted {
            folder: folder.clone(),
        }));
        for thread in state.threads.values() {
            changes.push(Change::ThreadUpserted {
                thread: thread.clone(),
            });
            changes.extend(
                state
                    .items_of(&thread.id)
                    .into_iter()
                    .map(|item| Change::ItemUpserted { item: item.clone() }),
            );
        }
        changes.extend(state.places.values().map(|place| Change::PlaceUpserted {
            place: place.clone(),
        }));
        changes.extend(state.markers.values().map(|marker| Change::MarkerUpserted {
            marker: marker.clone(),
        }));
        changes.extend(state.plans.values().map(|plan| Change::PlanSaved {
            plan: plan.clone(),
            revision: uxduck_core::domain::plan::PlanRevision::None,
        }));
        changes.extend(
            state
                .snapshots
                .values()
                .map(|snapshot| Change::SnapshotUpserted {
                    snapshot: snapshot.clone(),
                }),
        );
        changes
            .iter()
            .filter_map(|change| serde_json::to_value(change).ok())
            .collect()
    }

    /// A page connects through its `MessagePort`.
    pub fn connect(&mut self) -> (u32, Vec<Output>) {
        let (outbox, inbox) = unbounded();
        let id = self
            .engine
            .connect(Transport::MessagePort, Trust::LocalUser, outbox);
        self.next_connection += 1;
        let connection = self.next_connection;
        self.connections.insert(connection, (id, inbox));
        (connection, self.drain())
    }

    /// Per-page account hint, never a credential. Changes cancel that page’s turns.
    pub fn account(&mut self, connection: u32, json: &str) -> Vec<Output> {
        let Some((id, _)) = self.connections.get(&connection) else {
            return Vec::new();
        };
        let id = *id;
        let account = serde_json::from_str::<crate::agent::Account>(json).unwrap_or_default();
        let next = account.valid().then_some(account);
        if self.accounts.get(&id) == next.as_ref() {
            return Vec::new();
        }
        let mut agent_outputs = Vec::new();
        self.agents
            .cancel_origin(&mut self.engine, id, &mut agent_outputs);
        if let Some(previous) = self.accounts.remove(&id) {
            self.agents.forget_account(&previous);
        }
        if let Some(next) = next {
            self.accounts.insert(id, next);
        }
        self.account_ready
            .store(!self.accounts.is_empty(), Ordering::SeqCst);
        let mut outputs: Vec<_> = agent_outputs.into_iter().map(Output::Agent).collect();
        outputs.extend(self.drain());
        outputs
    }

    /// One provider event, EOF, or a bounded transport failure.
    pub fn agent_event(
        &mut self,
        request: u64,
        json: Option<&str>,
        error: Option<&str>,
    ) -> Vec<Output> {
        self.agents.event(request, json, error);
        self.drain()
    }

    /// One JSON-RPC frame from a page.
    pub fn receive(&mut self, connection: u32, text: &str) -> Vec<Output> {
        if text.contains(gamepads::METHOD)
            && let Some(frame) = gamepad_frame(text)
        {
            return self.gamepads(connection, frame);
        }
        let mut outputs = Vec::new();
        if let Some((id, _)) = self.connections.get(&connection) {
            let id = *id;
            if serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| {
                matches!(
                    v["method"].as_str(),
                    Some("operator/switch" | "client/setOperator")
                )
            }) {
                let mut cancelled = Vec::new();
                self.agents
                    .cancel_origin(&mut self.engine, id, &mut cancelled);
                if let Some(account) = self.accounts.get(&id) {
                    self.agents.forget_account(account);
                }
                outputs.extend(cancelled.into_iter().map(Output::Agent));
            }
            self.engine.receive(id, text);
        }
        outputs.extend(self.drain());
        outputs
    }

    pub fn disconnect(&mut self, connection: u32) -> Vec<Output> {
        let gone = self.pads.forget(connection, self.clock.now_us());
        let mut outputs = Vec::new();
        let _ = self.report_gamepad_interruptions(&mut outputs);
        self.controller_input(gone, &mut outputs);
        if let Some((id, _)) = self.connections.remove(&connection) {
            let mut agent_outputs = Vec::new();
            self.agents
                .cancel_origin(&mut self.engine, id, &mut agent_outputs);
            outputs.extend(agent_outputs.into_iter().map(Output::Agent));
            if let Some(account) = self.accounts.remove(&id) {
                self.agents.forget_account(&account);
            }
            self.account_ready
                .store(!self.accounts.is_empty(), Ordering::SeqCst);
            self.engine.disconnect(id);
        }
        outputs.extend(self.drain());
        outputs
    }

    /// A page's gamepads, read by its Gamepad API (UXDuck Web only): the
    /// changes go to the controllers, then the routed ones' sticks to their
    /// vehicles, at most at 25 Hz.
    fn gamepads(&mut self, connection: u32, frame: gamepads::Frame) -> Vec<Output> {
        let now = self.clock.now_us();
        let mut outputs = Vec::new();
        if let Some(&(owner, _)) = self.connections.get(&connection) {
            let read_at_us = frame
                .read_at_ms
                .map_or(now, |at| at.saturating_mul(1000).min(now));
            let reported: Vec<_> = frame
                .pads
                .iter()
                .filter(|pad| {
                    self.engine
                        .state()
                        .controllers
                        .get(&pad.id)
                        .is_some_and(|c| !c.connected)
                })
                .map(|pad| pad.id.clone())
                .collect();
            let events = self.pads.frame(connection, now, frame);
            let recoverable = self.report_gamepad_interruptions(&mut outputs);
            self.controller_input_recoverable(events, &mut outputs, &recoverable);
            for id in reported {
                self.engine
                    .controller_input_recovered(&id, owner, read_at_us);
            }
        }
        self.send_pilot(&mut outputs);
        outputs.extend(self.drain());
        outputs
    }

    /// Observe actual input loss before output pauses. Page closure releases
    /// activation; other losses may recover while the claiming page stays alive.
    /// One diagnostic per reporting page, even when several devices stop together.
    fn report_gamepad_interruptions(&mut self, outputs: &mut Vec<Output>) -> BTreeSet<String> {
        let now = self.clock.now_us();
        let worker_tick_gap_ms = self.last_tick_gap_ms.max(
            self.last_tick_us
                .map_or(0, |at| gamepads::bounded_ms(now.saturating_sub(at))),
        );
        let mut reports = BTreeMap::<u32, ControllerInputInterruption>::new();
        let mut recoverable = BTreeSet::new();
        for lost in self.pads.take_interruptions() {
            if lost.reason != ControllerInterruptionReason::PageClosed {
                recoverable.insert(lost.id.clone());
            }
            let report = reports
                .entry(lost.page)
                .or_insert(ControllerInputInterruption {
                    engine_build: option_env!("UXDUCK_GIT_SHA").map(str::to_owned),
                    reason: lost.reason,
                    interrupted_at_ms: now / 1000,
                    report_gap_ms: 0,
                    worker_tick_gap_ms,
                    affected_controllers: 0,
                    claimed_controllers: 0,
                    driving_controllers: 0,
                });
            report.report_gap_ms = report.report_gap_ms.max(lost.gap_ms);
            report.affected_controllers = (report.affected_controllers + 1).min(64);
            if self
                .engine
                .state()
                .controllers
                .get(&lost.id)
                .is_some_and(|c| c.claim.is_some())
            {
                report.claimed_controllers = (report.claimed_controllers + 1).min(64);
                if self.controls.target(&lost.id).is_some() {
                    report.driving_controllers = (report.driving_controllers + 1).min(64);
                }
            }
        }
        for (connection, report) in reports {
            outputs.push(Output::Post {
                connection,
                text: serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": gamepads::INTERRUPTED,
                    "params": report,
                })
                .to_string(),
            });
        }
        recoverable
    }

    fn send_pilot(&mut self, outputs: &mut Vec<Output>) {
        let now = self.clock.now_us();
        let gone = self.controls.stale_keyboard_inputs(now);
        self.controller_input(gone, outputs);
        if self.controls.routed() && now.saturating_sub(self.last_pilot_us) >= PILOT_EVERY_US {
            self.last_pilot_us = now;
            let mut effects = Vec::new();
            for (vehicle_id, sticks, auxiliary, method) in self.controls.pilot_outputs() {
                effects.extend(self.session.pilot_control_with_method(
                    &vehicle_id,
                    sticks,
                    &auxiliary,
                    method,
                ));
            }
            self.apply(effects, outputs);
        }
    }

    /// Input events, as the daemon's data plane takes them: an unplugged
    /// controller's vehicle gets its inputs back first.
    fn controller_input(&mut self, events: Vec<InputEvent>, outputs: &mut Vec<Output>) {
        self.controller_input_recoverable(events, outputs, &BTreeSet::new());
    }

    fn controller_input_recoverable(
        &mut self,
        events: Vec<InputEvent>,
        outputs: &mut Vec<Output>,
        recoverable: &BTreeSet<String>,
    ) {
        if events.is_empty() {
            return;
        }
        for event in events {
            if let InputEvent::Disconnected { id } = &event
                && let Some(target) = self.controls.target(id)
            {
                let effects = self.session.release_control(&target);
                self.apply(effects, outputs);
            }
            let reports = self.controls.input(event);
            // A neutral report can now switch a following controller to
            // its first vehicle. Publish this event's raw readings before
            // the engine checks its mapping and airborne throttle gate.
            if !reports.is_empty() {
                *self.inputs.lock().unwrap_or_else(PoisonError::into_inner) =
                    self.controls.snapshot();
            }
            for report in reports {
                match report {
                    Report::Disconnected { id } if recoverable.contains(&id) => {
                        self.engine.controller_input_interrupted(&id);
                    }
                    report => self.report_control(report),
                }
            }
        }
        *self.inputs.lock().unwrap_or_else(PoisonError::into_inner) = self.controls.snapshot();
    }

    fn report_controls(&mut self, reports: Vec<Report>) {
        for report in reports {
            self.report_control(report);
        }
    }

    fn report_control(&mut self, report: Report) {
        match report {
            Report::Connected {
                id,
                name,
                layout_known,
            } => self.engine.controller_connected(&id, &name, layout_known),
            Report::Disconnected { id } => self.engine.controller_disconnected(&id),
            Report::Neutral { id, neutral } => self.engine.controller_neutral(&id, neutral),
            Report::Pressed { id, button, code } => {
                self.engine.controller_pressed(&id, button, code);
            }
            Report::Positioned { id, position } => {
                self.engine.controller_positioned(&id, &position);
            }
        }
    }

    /// The simulator started, or stopped with a reason.
    pub fn link_status(&mut self, link_id: &str, up: bool, detail: Option<String>) -> Vec<Output> {
        let status = if up { LinkStatus::Up } else { LinkStatus::Down };
        self.engine.link_status(link_id, status, detail);
        self.drain()
    }

    /// Bytes from a simulator's serial port.
    pub fn link_received(&mut self, link_id: &str, bytes: &[u8]) -> Vec<Output> {
        let Some(stream) = self.streams.get_mut(link_id) else {
            return Vec::new();
        };
        let frames = stream.push(bytes, uxduck_mavlink::decode::check);
        let now = self.clock.now_us();
        let mut effects = Vec::new();
        for frame in frames {
            effects.extend(self.session.ingest(link_id, PEER, &frame, now));
            if let Some(header) = uxduck_mavlink::framing::header(&frame)
                && let Some(vehicle_id) = self.session.vehicle_at(link_id, PEER, header.system_id)
                && let Some(payload) = uxduck_mavlink::framing::payload(&frame)
            {
                self.inspector
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record(vehicle_id, InspectDirection::Received, header, payload, now);
            }
        }
        let mut outputs = Vec::new();
        self.apply(effects, &mut outputs);
        outputs.extend(self.drain());
        outputs
    }

    /// The worker's beat, every 100 ms: timeouts, presence and telemetry,
    /// and approvals that ran out.
    pub fn tick(&mut self) -> Vec<Output> {
        let now = self.clock.now_us();
        self.last_tick_gap_ms = self
            .last_tick_us
            .map_or(0, |at| gamepads::bounded_ms(now.saturating_sub(at)));
        self.last_tick_us = Some(now);
        let mut outputs = Vec::new();
        if self.session.needs_tick() {
            let effects = self.session.tick(now);
            self.apply(effects, &mut outputs);
        }
        if now.saturating_sub(self.last_expiry_us) >= EXPIRY_EVERY_US {
            self.last_expiry_us = now;
            self.engine.expire_approvals(now);
            self.engine.expire_display_pulls(now);
            self.engine.expire_detached_claims(now);
            self.engine.expire_traffic(now);
        }
        let gone = self.controls.stale_keyboard_inputs(now);
        self.controller_input(gone, &mut outputs);
        // A page that stopped reporting (hidden, frozen) lets its pads go.
        if self.pads.any() {
            let gone = self.pads.expire(now);
            let recoverable = self.report_gamepad_interruptions(&mut outputs);
            self.controller_input_recoverable(gone, &mut outputs, &recoverable);
        }
        outputs.extend(self.drain());
        outputs
    }

    fn apply(&mut self, effects: Vec<Effect>, outputs: &mut Vec<Output>) {
        for effect in effects {
            match plane::route(effect) {
                Routed::Engine(report) => report(&mut self.engine),
                Routed::Send {
                    link_id,
                    peer,
                    bytes,
                } => {
                    // A paused simulator would read it all at once on resuming.
                    if self.streams.contains_key(&link_id) && !self.session.is_paused(&link_id) {
                        self.inspect_sent(&link_id, &peer, &bytes);
                        outputs.push(Output::LinkWrite { link_id, bytes });
                    }
                }
            }
        }
    }

    /// A frame for the simulator, for the inspector: recorded for each
    /// vehicle at its port, as the daemon does.
    fn inspect_sent(&self, link_id: &str, peer: &str, bytes: &[u8]) {
        let (Some(header), Some(payload)) = (
            uxduck_mavlink::framing::header(bytes),
            uxduck_mavlink::framing::payload(bytes),
        ) else {
            return;
        };
        let now = self.clock.now_us();
        let mut inspector = self
            .inspector
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (_, vehicle_id) in self.session.vehicles_at_peer(link_id, peer) {
            inspector.record(vehicle_id, InspectDirection::Sent, header, payload, now);
        }
    }

    /// Does what the ports asked for until nothing more is asked, then
    /// collects every connection's frames.
    fn drain(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        loop {
            let mut agent_outputs = Vec::new();
            let progressed =
                self.agents
                    .drive(&mut self.engine, self.clock.now_us(), &mut agent_outputs);
            outputs.extend(agent_outputs.into_iter().map(Output::Agent));
            let calls: Vec<Call> = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
                .collect();
            if calls.is_empty() && !progressed {
                break;
            }
            for call in calls {
                self.call(call, &mut outputs);
            }
        }
        for (connection, (_, inbox)) in &mut self.connections {
            while let Ok(text) = inbox.try_recv() {
                outputs.push(Output::Post {
                    connection: *connection,
                    text,
                });
            }
        }
        outputs
    }

    fn call(&mut self, call: Call, outputs: &mut Vec<Output>) {
        let now = self.clock.now_us();
        match call {
            Call::Open(link) => match start(&link) {
                Some(order) => {
                    self.session.simulated(&link.id);
                    self.streams.insert(link.id.clone(), Reassembler::default());
                    outputs.push(Output::Simulator(order));
                }
                None => self.engine.link_status(
                    &link.id,
                    LinkStatus::Down,
                    Some("a browser runs only simulated vehicles".into()),
                ),
            },
            Call::Close(link_id) => {
                self.session.remove_link(&link_id);
                if self.streams.remove(&link_id).is_some() {
                    outputs.push(Output::Simulator(Order::Stop { link_id }));
                }
            }
            Call::Command(order) => {
                let effects = plane::start_command(&mut self.session, order, now);
                self.apply(effects, outputs);
            }
            Call::Transfer(order) => {
                let effects = plane::start_transfer(&mut self.session, order, now);
                self.apply(effects, outputs);
            }
            Call::GcsSystemId(id) => self.session.set_gcs_system_id(id),
            Call::Pause(link_id, paused) => {
                if self.streams.contains_key(&link_id) {
                    self.session.pause(&link_id, paused, now);
                    outputs.push(Output::Simulator(Order::Pause { link_id, paused }));
                } else {
                    self.engine.simulation_pause_failed(
                        &link_id,
                        paused,
                        "the simulation isn't running".into(),
                    );
                }
            }
            Call::Journal(write) => outputs.push(Output::Journal(write)),
            Call::Turn(turn) => {
                let account = self
                    .accounts
                    .get(&turn.connection)
                    .cloned()
                    .unwrap_or_default();
                self.agents.start(&mut self.engine, turn, account, now);
            }
            Call::CancelTurn(thread) => {
                let mut agent_outputs = Vec::new();
                self.agents
                    .cancel(&mut self.engine, &thread, &mut agent_outputs);
                outputs.extend(agent_outputs.into_iter().map(Output::Agent));
            }
            Call::Definitions(order) => self.definitions(order, outputs),
            Call::Terrain(order) => self.terrain(order, outputs),
            Call::Places(order) => {
                self.next_fetch += 1;
                let path = order.action.path().to_owned();
                let body = order.action.body().to_string();
                self.fetches.insert(self.next_fetch, Fetching::Place(order));
                outputs.push(Output::PlaceLookup {
                    request: self.next_fetch,
                    path,
                    body,
                });
            }
            Call::Input(event) => {
                self.controller_input(vec![event], outputs);
                // Keyboard frames arrive without any gamepad notifications.
                self.send_pilot(outputs);
            }
            Call::Route(id, route) => {
                if let Some(target) = self.controls.target(&id) {
                    let effects = self.session.release_control(&target);
                    self.apply(effects, outputs);
                }
                let reports = self.controls.route(&id, route);
                self.report_controls(reports);
            }
        }
    }
}

impl Browser {
    fn terrain(&mut self, order: uxduck_terrain::Order, outputs: &mut Vec<Output>) {
        use uxduck_terrain::Order;
        match order {
            Order::Read { key } => {
                self.next_fetch += 1;
                self.reading.insert(self.next_fetch, key.clone());
                outputs.push(Output::Store(Store::Read {
                    request: self.next_fetch,
                    key,
                }));
            }
            Order::Fetch { key } => {
                let url = format!("{}/{key}", self.terrain_url);
                self.fetch(url, Fetching::Terrain(key), outputs);
            }
            Order::Write { key, bytes } => outputs.push(Output::Store(Store::Write { key, bytes })),
            Order::Remove { prefix } => outputs.push(Output::Store(Store::Remove { prefix })),
        }
    }
}

/// A page's gamepads, from its `browser/gamepads` notification.
fn gamepad_frame(text: &str) -> Option<gamepads::Frame> {
    #[derive(serde::Deserialize)]
    struct Notification {
        method: String,
        #[serde(default)]
        params: gamepads::Frame,
    }
    serde_json::from_str::<Notification>(text)
        .ok()
        .filter(|n| n.method == gamepads::METHOD)
        .map(|n| n.params)
}

/// The release key this build trusts, if it was built with one.
fn release_key() -> Option<String> {
    option_env!("UXDUCK_RELEASE_PUBLIC_KEY")
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
}

/// How to start a simulator link: ArduPilot's program and model.
fn start(link: &Link) -> Option<Order> {
    let LinkKind::Simulator {
        vehicle,
        latitude_deg,
        longitude_deg,
        altitude_m,
        heading_deg,
        ref frame,
        speedup,
        instance,
        traffic,
        ..
    } = link.kind
    else {
        return None;
    };
    let (program, default_model) = match vehicle {
        SimVehicle::Plane => ("plane", "plane"),
        SimVehicle::Multicopter
            if frame
                .as_deref()
                .is_some_and(|frame| frame.starts_with("heli")) =>
        {
            ("copter-heli", "heli")
        }
        SimVehicle::Multicopter => ("copter", "quad"),
        SimVehicle::Vtol => ("plane", "quadplane"),
        // ArduRover drives both, as the installed daemon's (S37).
        SimVehicle::Rover => ("rover", "rover"),
        SimVehicle::Boat => ("rover", "motorboat"),
        SimVehicle::Submarine => ("sub", "vectored"),
        SimVehicle::Airship => ("blimp", "blimp"),
        // ArduPlane's jet frame, with UXDuck's jet profile (S143).
        SimVehicle::Jet => ("plane", JET_FRAME),
    };
    Some(Order::Start {
        link_id: link.id.clone(),
        program,
        model: frame.clone().unwrap_or_else(|| default_model.to_owned()),
        speedup: speedup.unwrap_or(1),
        latitude_deg,
        longitude_deg,
        altitude_m,
        heading_deg,
        // A link journaled before instances in the browser is the first.
        sysid: instance.map_or(1, |i| i.saturating_add(1)),
        traffic,
    })
}

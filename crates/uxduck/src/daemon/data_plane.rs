//! The data plane: links, the MAVLink session, routing and tlog recording
//! (ADR-0004). It runs beside the engine and reports what it sees; the
//! engine keeps the entities. Frames are recorded byte for byte before they
//! are decoded, checked for loops before anything else sees them, and
//! forwarded without being changed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use uxduck_contracts::dto::inspect::InspectDirection;
use uxduck_core::domain::link::{Link, LinkCarries, LinkKind, LinkPolicy, LinkStatus};
use uxduck_engine::plane as glue;
use uxduck_engine::runtime::{Clock, ControlRoute, LinkPort, TransferOrder};
use uxduck_mavlink::framing::{Reassembler, header, split};
use uxduck_mavlink::links::{self, LinkEvent, LinkHandle, Outgoing};
use uxduck_mavlink::router::{LoopConfig, Role, Routed, Router};
use uxduck_mavlink::session::{Config, Effect, Session};

use super::controllers::{Controls, InputEvent, Report};
use super::engine_task::EngineHandle;
use crate::adapters::system::SystemClock;
use crate::ports::{Protocol, in_use_message};

enum Command {
    Open(Link),
    Close(String),
    /// A link changed in place (`link/update`): its role if its transport
    /// is the same, else reopened.
    Update(Link),
    Order(uxduck_engine::runtime::CommandOrder),
    Message(uxduck_engine::runtime::MessageOrder),
    Transfer(TransferOrder),
    Input(InputEvent),
    Route(String, Option<ControlRoute>),
    /// UXDuck's ground station heartbeat to a vehicle, on or withheld (S81).
    GroundHeartbeat(String, bool),
    /// The system id UXDuck sends as, from its settings.
    GcsSystemId(u8),
    /// A simulator frozen or let run again (`sim/pause`).
    Pause(String, bool),
}

#[derive(Debug)]
struct Incoming {
    link_id: String,
    generation: Arc<()>,
    event: LinkEvent,
    at_us: u64,
}

/// A second bounded queue shared by links; control orders have their own
/// inbox and cannot sit behind a flood of telemetry.
const INCOMING_CAPACITY: usize = 256;

/// The engine's end: never blocks.
#[derive(Debug)]
pub struct LinkChannel(mpsc::UnboundedSender<Command>);

impl LinkPort for LinkChannel {
    fn open(&mut self, link: &Link) {
        let _ = self.0.send(Command::Open(link.clone()));
    }
    fn close(&mut self, link_id: &str) {
        let _ = self.0.send(Command::Close(link_id.to_owned()));
    }
    fn update(&mut self, link: &Link) {
        let _ = self.0.send(Command::Update(link.clone()));
    }
    fn command(&mut self, order: uxduck_engine::runtime::CommandOrder) {
        let _ = self.0.send(Command::Order(order));
    }
    fn transfer(&mut self, order: TransferOrder) {
        let _ = self.0.send(Command::Transfer(order));
    }
    fn message(&mut self, order: uxduck_engine::runtime::MessageOrder) {
        let _ = self.0.send(Command::Message(order));
    }
    fn controller_input(&mut self, event: InputEvent) {
        let _ = self.0.send(Command::Input(event));
    }
    fn route_controller(&mut self, controller_id: &str, route: Option<ControlRoute>) {
        let _ = self.0.send(Command::Route(controller_id.to_owned(), route));
    }
    fn ground_heartbeat(&mut self, vehicle_id: &str, on: bool) {
        let _ = self
            .0
            .send(Command::GroundHeartbeat(vehicle_id.to_owned(), on));
    }
    fn gcs_system_id(&mut self, id: u8) {
        let _ = self.0.send(Command::GcsSystemId(id));
    }
    fn pause_simulation(&mut self, link_id: &str, paused: bool) {
        let _ = self.0.send(Command::Pause(link_id.to_owned(), paused));
    }
}

/// One frame a plugin wants, raw or to decode, as the vehicle sent it.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "plugins"), allow(dead_code))]
pub struct RawFrame {
    pub vehicle_id: String,
    pub message_id: u32,
    pub system_id: u8,
    pub component_id: u8,
    pub payload: Vec<u8>,
}

/// What plugins receive (ADR-0006 X1): frames by message id, whether they
/// want them raw or decoded, and telemetry items. Filtered here, natively,
/// before anything is copied for the plugin host.
pub struct PluginFeed {
    pub wants_frame: Box<dyn Fn(u32) -> bool + Send + Sync>,
    pub frame: Box<dyn Fn(RawFrame) + Send + Sync>,
    pub wants_items: Box<dyn Fn() -> bool + Send + Sync>,
    pub items: Box<dyn Fn(&uxduck_core::domain::telemetry::VehicleTelemetry) + Send + Sync>,
}

impl std::fmt::Debug for PluginFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginFeed")
    }
}

/// Where controller input goes: the data plane's own queue.
pub fn inputs(inbox: &LinkInbox) -> impl Fn(InputEvent) + Send + Clone + 'static {
    let sender = inbox.sender.clone();
    move |event| {
        let _ = sender.send(Command::Input(event));
    }
}

#[derive(Debug)]
pub struct LinkInbox {
    commands: mpsc::UnboundedReceiver<Command>,
    sender: mpsc::UnboundedSender<Command>,
    received: mpsc::Receiver<Incoming>,
    events: mpsc::Sender<Incoming>,
}

#[must_use]
pub fn channel() -> (LinkChannel, LinkInbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (events, received) = mpsc::channel(INCOMING_CAPACITY);
    (
        LinkChannel(tx.clone()),
        LinkInbox {
            commands: rx,
            sender: tx,
            received,
            events,
        },
    )
}

struct Open {
    handle: LinkHandle,
    generation: Arc<()>,
    /// Cancelling the relay also drops its receiver, releasing a serial
    /// thread that is waiting for incoming queue space when its link closes.
    relay: tokio::task::JoinHandle<()>,
    /// What the transport was opened for, so a change to anything else
    /// (`link/update`) keeps it.
    kind: LinkKind,
    /// A simulated vehicle's build, stopped when the link closes (S17),
    /// and watched until then for stopping on its own.
    simulator: Option<crate::sim::Running>,
    /// A USB receiver stick's decoder (S173), stopped when the link closes
    /// and watched until then for stopping on its own.
    decoder: Option<crate::receiver_stick::Decoder>,
    /// Whether the transport has connected (a stream) or heard anything,
    /// to tell a simulator that stopped from one that never started.
    started: bool,
    /// False for a read-only vehicle link: nothing is ever sent on it but
    /// a UDP client's hello, without which its server sends nothing.
    writable: bool,
    forward: bool,
    /// A stream's partial frames, by peer: a TCP server has a stream per
    /// connection.
    reassemblers: HashMap<String, Reassembler>,
    /// A receiver's traffic, by peer, when the link carries it (S172).
    traffic: Option<TrafficReaders>,
    tlog_path: PathBuf,
    tlog: Option<BufWriter<File>>,
    /// Where the tlog's next record goes, for its index; None once a write
    /// failed, after which its records are no longer indexed.
    tlog_end: Option<u64>,
}

/// Readers of a receiver's traffic, one per peer: each TCP connection or
/// UDP sender is a stream of its own.
enum TrafficReaders {
    Sbs(HashMap<String, uxduck_mavlink::ground_traffic::SbsReader>),
    Gdl90(HashMap<String, uxduck_mavlink::ground_traffic::Gdl90Reader>),
}

impl TrafficReaders {
    fn new(carries: LinkCarries) -> Option<Self> {
        match carries {
            LinkCarries::Mavlink => None,
            LinkCarries::Sbs => Some(Self::Sbs(HashMap::new())),
            LinkCarries::Gdl90 => Some(Self::Gdl90(HashMap::new())),
        }
    }

    fn read(
        &mut self,
        peer: &str,
        bytes: &[u8],
        now_us: u64,
    ) -> Vec<uxduck_core::domain::traffic::TrafficReport> {
        match self {
            Self::Sbs(readers) => readers
                .entry(peer.to_owned())
                .or_default()
                .push(bytes, now_us),
            Self::Gdl90(readers) => readers
                .entry(peer.to_owned())
                .or_default()
                .push(bytes, now_us),
        }
    }

    fn forget(&mut self, peer: &str) {
        match self {
            Self::Sbs(readers) => {
                readers.remove(peer);
            }
            Self::Gdl90(readers) => {
                readers.remove(peer);
            }
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.relay.abort();
    }
}

/// Starts the data plane. `tlog_dir` receives one tlog per link per run.
pub fn start(
    inbox: LinkInbox,
    engine: EngineHandle,
    config: Config,
    tlog_dir: PathBuf,
    plugin_feed: Option<PluginFeed>,
    simulators: std::sync::Arc<crate::sim::Simulators>,
) {
    let LinkInbox {
        mut commands,
        mut received,
        events,
        ..
    } = inbox;
    tokio::spawn(async move {
        let mut plane = Plane {
            gcs: Arc::new(AtomicU8::new(config.gcs_system_id)),
            session: Session::new(config),
            router: Router::new(LoopConfig::default()),
            open: HashMap::new(),
            engine,
            tlog_dir,
            controls: Controls::default(),
            plugin_feed,
            simulators,
            withheld: std::collections::HashSet::new(),
        };
        // MANUAL_CONTROL for routed controllers, 25 times a second.
        let mut manual = timer(Duration::from_millis(40));
        // The tick also flushes telemetry, so it sets the update rate: 10 Hz.
        let mut tick = timer(Duration::from_millis(100));
        // The ground station heartbeat, 1 Hz while there are vehicles (S81).
        let mut ground = timer(Duration::from_secs(1));
        // Whether a simulated vehicle's build stopped on its own, twice a
        // second while one runs.
        let mut watch = timer(Duration::from_millis(500));
        loop {
            // Each timer runs only while it has work, so an idle daemon
            // sleeps until a command or a frame arrives (S24).
            let steering = plane.controls.routed();
            let ticking = plane.session.needs_tick();
            let vehicles = !plane.session.routes().is_empty();
            // Simulators' builds and receiver sticks' decoders are watched.
            let simulating = plane
                .open
                .values()
                .any(|open| open.simulator.is_some() || open.decoder.is_some());
            tokio::select! {
                command = commands.recv() => match command {
                    None => break,
                    Some(Command::Open(link)) => plane.open(link, &events).await,
                    Some(Command::Close(link_id)) => plane.close(&link_id),
                    Some(Command::Update(link)) => plane.update(link, &events).await,
                    Some(Command::Order(order)) => {
                        let effects = glue::start_command(&mut plane.session, order, SystemClock.now_us());
                        plane.apply(effects).await;
                    }
                    Some(Command::Message(order)) => {
                        let effects = plane.message(&order);
                        plane.apply(effects).await;
                    }
                    Some(Command::Transfer(order)) => {
                        let effects = glue::start_transfer(&mut plane.session, order, SystemClock.now_us());
                        plane.apply(effects).await;
                    }
                    Some(Command::Input(event)) => {
                        if let super::controllers::InputEvent::Disconnected { id } = &event
                            && let Some(target) = plane.controls.target(id) {
                            let effects = plane.session.release_control(&target);
                            plane.apply(effects).await;
                        }
                        let reports = plane.controls.input(event);
                        super::controllers::publish(plane.controls.snapshot());
                        plane.report_controls(reports).await;
                    }
                    Some(Command::Route(id, route)) => {
                        if let Some(target) = plane.controls.target(&id) {
                            let effects = plane.session.release_control(&target);
                            plane.apply(effects).await;
                        }
                        let reports = plane.controls.route(&id, route);
                        plane.report_controls(reports).await;
                    }
                    Some(Command::GroundHeartbeat(vehicle_id, on)) => {
                        if on {
                            plane.withheld.remove(&vehicle_id);
                        } else {
                            tracing::warn!(%vehicle_id, "nobody supervises it: withholding the ground station heartbeat");
                            plane.withheld.insert(vehicle_id);
                        }
                    }
                    Some(Command::GcsSystemId(id)) => {
                        plane.gcs.store(id, Ordering::Relaxed);
                        plane.session.set_gcs_system_id(id);
                    }
                    Some(Command::Pause(link_id, paused)) => plane.pause(&link_id, paused).await,
                },
                Some(incoming) = received.recv() => plane.incoming(incoming).await,
                _ = manual.tick(), if steering => {
                    crate::wakes::count("data plane: manual control (25 Hz)");
                    plane.send_pilot().await;
                }
                _ = ground.tick(), if vehicles => {
                    crate::wakes::count("data plane: ground heartbeat (1 Hz)");
                    let effects = plane.ground_heartbeats();
                    plane.apply(effects).await;
                }
                _ = watch.tick(), if simulating => plane.simulators_stopped().await,
                _ = tick.tick(), if ticking => {
                    crate::wakes::count("data plane: tick (10 Hz)");
                    let effects = plane.session.tick(SystemClock.now_us());
                    plane.apply(effects).await;
                }
            }
        }
    });
}

/// A timer that waits out a missed tick rather than catching up.
fn timer(period: Duration) -> tokio::time::Interval {
    let mut timer = tokio::time::interval(period);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timer
}

struct Plane {
    /// The system id UXDuck sends as (a daemon setting), shared with the
    /// tasks that send on their own: UDP client hellos and PX4 greetings.
    gcs: Arc<AtomicU8>,
    session: Session,
    router: Router,
    open: HashMap<String, Open>,
    engine: EngineHandle,
    tlog_dir: PathBuf,
    controls: Controls,
    plugin_feed: Option<PluginFeed>,
    simulators: std::sync::Arc<crate::sim::Simulators>,
    /// Vehicles nobody supervises: no ground heartbeat to them (S81).
    withheld: std::collections::HashSet<String>,
}

impl Plane {
    async fn incoming(&mut self, incoming: Incoming) {
        // Close/reopen orders can overtake telemetry. Bytes from an earlier
        // transport must never enter its replacement.
        if self
            .open
            .get(&incoming.link_id)
            .is_some_and(|open| Arc::ptr_eq(&open.generation, &incoming.generation))
        {
            self.event(&incoming.link_id, incoming.event, incoming.at_us)
                .await;
        }
    }

    /// UXDuck's ground station heartbeat, once a second, to every vehicle's
    /// address on a writable link (S81, ADR-0003 C16), as Mission Planner
    /// sends one: what lets a vehicle's lost ground station failsafe act
    /// when it stops. An address is skipped only when every vehicle there is
    /// withheld, so a supervised vehicle never loses it.
    fn ground_heartbeats(&mut self) -> Vec<Effect> {
        let mut addresses: HashMap<(String, String), (String, bool)> = HashMap::new();
        for route in self.session.routes() {
            let withheld = self.withheld.contains(&route.vehicle_id);
            let entry = addresses
                .entry((route.link_id.clone(), route.peer.clone()))
                .or_insert((route.vehicle_id.clone(), true));
            if !withheld {
                *entry = (route.vehicle_id.clone(), false);
            }
        }
        let mut effects = Vec::new();
        let gcs = self.gcs.load(Ordering::Relaxed);
        for ((link_id, _), (vehicle_id, withheld)) in addresses {
            if withheld
                || self
                    .open
                    .get(&link_id)
                    .is_none_or(|o| !o.writable || o.forward)
            {
                continue;
            }
            effects.extend(self.session.send_to(&vehicle_id, |_, sequence| {
                uxduck_mavlink::sim::gcs_heartbeat_frame(
                    gcs,
                    uxduck_mavlink::GCS_COMPONENT_ID,
                    sequence,
                )
            }));
        }
        effects
    }

    async fn open(&mut self, link: Link, incoming: &mpsc::Sender<Incoming>) {
        // A simulator started again on its link (`sim/restart`) must be
        // gone before the next one takes its ports; waited for off this
        // task, for the few hundred milliseconds its guard takes.
        let previous = self
            .open
            .get_mut(&link.id)
            .and_then(|open| open.simulator.take());
        self.close(&link.id);
        if let Some(previous) = previous {
            let _ = tokio::task::spawn_blocking(move || previous.stop()).await;
        }
        if link.kind.is_simulator() {
            self.reaped(&link.id).await;
        }
        if link.kind.is_simulator() {
            self.session.simulated(&link.id);
        }
        let (events, mut received) = links::event_channel();
        let destination = incoming.clone();
        let link_id = link.id.clone();
        let generation = Arc::new(());
        let receiving_generation = generation.clone();
        let relay = tokio::spawn(async move {
            while let Some(links::TimedEvent { event, at_us }) = received.recv().await {
                let command = Incoming {
                    link_id: link_id.clone(),
                    generation: receiving_generation.clone(),
                    event,
                    at_us,
                };
                if destination.send(command).await.is_err() {
                    break;
                }
            }
        });
        // A simulated vehicle: its build starts first, then the link
        // connects to its SERIAL0, retrying while it starts (S17); PX4's
        // is UDP, and is greeted until it answers.
        let mut simulator = None;
        let mut decoder = None;
        let spawned = if matches!(link.kind, LinkKind::UsbReceiver {}) {
            let (spawned, started) = self.receiver_stick(&link, events);
            decoder = started;
            spawned
        } else if link.kind.is_simulator() {
            match self.simulators.launch(&link) {
                Ok((running, crate::sim::Endpoint::Tcp(port))) => {
                    simulator = Some(running);
                    Ok(links::tcp_client("127.0.0.1".into(), port, events))
                }
                Ok((running, crate::sim::Endpoint::Udp(port))) => {
                    simulator = Some(running);
                    let address = SocketAddr::from(([127, 0, 0, 1], port));
                    match links::udp_target(address, events).await {
                        Ok(handle) => {
                            greet(handle.sender(), self.gcs.clone());
                            Ok(handle)
                        }
                        Err(error) => Err((LinkStatus::Down, error.to_string())),
                    }
                }
                Err(detail) => Err((LinkStatus::Down, detail)),
            }
        } else {
            spawn(&link.kind, self.gcs.clone(), events).await
        };
        match spawned {
            Ok(handle) => {
                let tlog_path = self.tlog_dir.join(format!(
                    "{}-{}.tlog",
                    link.id,
                    SystemClock.now_us() / 1_000_000
                ));
                let forward = link.kind.is_forward();
                let writable = link.policy == LinkPolicy::ReadWrite;
                self.router.set_link(&link.id, role(&link));
                crate::diagnostics::register(&link);
                self.open.insert(
                    link.id.clone(),
                    Open {
                        handle,
                        generation,
                        relay,
                        kind: link.kind.clone(),
                        simulator,
                        decoder,
                        started: false,
                        writable,
                        forward,
                        reassemblers: HashMap::new(),
                        traffic: TrafficReaders::new(link.carries),
                        tlog_path: tlog_path.clone(),
                        tlog: None,
                        tlog_end: Some(0),
                    },
                );
                crate::tlog_index::open(&link.id, &tlog_path);
            }
            Err((status, detail)) => {
                relay.abort();
                self.report(&link.id, status, Some(detail)).await;
            }
        }
    }

    /// A build closed before its link opens again (`sim/restart` closes,
    /// then opens) is reaped off this task, and holds its link until it
    /// has exited; waited for here, up to 10 s.
    async fn reaped(&self, link_id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while self.simulators.is_active(link_id) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A link changed (`link/update`). With the transport it was opened
    /// for, only its role changes, so the vehicles on it are not dropped;
    /// opened for something else, or not open, it opens anew.
    async fn update(&mut self, link: Link, incoming: &mpsc::Sender<Incoming>) {
        match self.open.get_mut(&link.id) {
            Some(open) if open.kind == link.kind => {
                open.writable = link.policy == LinkPolicy::ReadWrite;
                open.forward = link.kind.is_forward();
                self.router.set_link(&link.id, role(&link));
                crate::diagnostics::register(&link);
            }
            _ => self.open(link, incoming).await,
        }
    }

    /// A USB receiver stick (S173): its decoder first, then its SBS-1 port,
    /// retried while the decoder starts.
    fn receiver_stick(
        &self,
        link: &Link,
        events: links::Events,
    ) -> (
        Result<LinkHandle, (LinkStatus, String)>,
        Option<crate::receiver_stick::Decoder>,
    ) {
        let log = self.tlog_dir.join(format!("{}-decoder.log", link.id));
        match crate::receiver_stick::launch(&log) {
            Ok(decoder) => {
                let handle = links::tcp_client("127.0.0.1".into(), decoder.port, events);
                (Ok(handle), Some(decoder))
            }
            Err(detail) => (Err((LinkStatus::Down, detail)), None),
        }
    }

    /// Simulated vehicles whose build stopped on its own (it crashed, or
    /// never started): the link closes, so it stops retrying a port nobody
    /// listens on, and says why it is down. The link and its vehicle are
    /// otherwise kept, for `sim/restart`. A build stopped on purpose is
    /// never seen here: closing, reopening or removing its link takes it
    /// out of [`Open`] first, on this task.
    async fn simulators_stopped(&mut self) {
        crate::wakes::count("data plane: simulator watch (2 Hz)");
        let mut stopped = Vec::new();
        for (link_id, open) in &mut self.open {
            if let Some(simulator) = open.simulator.as_mut()
                && let Some(status) = simulator.exited()
            {
                let detail = simulator.stopped_detail(open.started, status);
                stopped.push((link_id.clone(), detail));
            }
        }
        // A receiver stick's decoder that stopped: no stick, or it was
        // unplugged (S173).
        let mut decoders = Vec::new();
        for (link_id, open) in &mut self.open {
            if let Some(detail) = open
                .decoder
                .as_mut()
                .and_then(crate::receiver_stick::Decoder::exited)
            {
                decoders.push((link_id.clone(), detail));
            }
        }
        for (link_id, detail) in decoders {
            tracing::warn!(link = %link_id, %detail, "a receiver stick's decoder stopped");
            self.close(&link_id);
            self.report(&link_id, LinkStatus::Down, Some(detail)).await;
        }
        for (link_id, detail) in stopped {
            tracing::warn!(link = %link_id, %detail, "a simulated vehicle's build stopped by itself");
            self.close(&link_id);
            self.report(&link_id, LinkStatus::Down, Some(detail)).await;
        }
    }

    /// Freezes a simulator's build, or lets it run again; the session
    /// stops expecting to hear from it, and nothing is sent to it.
    async fn pause(&mut self, link_id: &str, paused: bool) {
        let Some(simulator) = self
            .open
            .get_mut(link_id)
            .and_then(|open| open.simulator.as_mut())
        else {
            tracing::warn!(link = %link_id, paused, "no running simulator to pause or resume");
            return;
        };
        if let Err(error) = simulator.pause(paused) {
            tracing::warn!(link = %link_id, paused, %error, "cannot pause or resume a simulator");
            let link_id = link_id.to_owned();
            let _ = self
                .engine
                .run(move |e| e.simulation_pause_failed(&link_id, paused, error))
                .await;
            return;
        }
        self.session.pause(link_id, paused, SystemClock.now_us());
    }

    fn close(&mut self, link_id: &str) {
        if self.open.remove(link_id).is_some() {
            crate::tlog_index::close(link_id);
        }
        self.router.remove_link(link_id);
        self.session.remove_link(link_id);
    }

    async fn report(&self, link_id: &str, status: LinkStatus, detail: Option<String>) {
        let link_id = link_id.to_owned();
        let _ = self
            .engine
            .run(move |e| e.link_status(&link_id, status, detail))
            .await;
    }

    async fn event(&mut self, link_id: &str, event: LinkEvent, at_us: u64) {
        if let Some(open) = self.open.get_mut(link_id) {
            match &event {
                LinkEvent::Up if open.handle.stream => open.started = true,
                LinkEvent::Received { .. } => open.started = true,
                _ => {}
            }
        }
        match event {
            LinkEvent::Up => self.report(link_id, LinkStatus::Up, None).await,
            LinkEvent::Down(reason) => self.report(link_id, LinkStatus::Down, Some(reason)).await,
            LinkEvent::Left { peer } => {
                if let Some(open) = self.open.get_mut(link_id) {
                    open.reassemblers.remove(&peer);
                    if let Some(traffic) = &mut open.traffic {
                        traffic.forget(&peer);
                    }
                }
            }
            LinkEvent::Received { peer, bytes } => {
                let Some(open) = self.open.get_mut(link_id) else {
                    return;
                };
                // A receiver's traffic, not MAVLink (S172): read, and told to
                // the engine as the link's own source.
                if let Some(traffic) = &mut open.traffic {
                    let reports = traffic.read(&peer, &bytes, SystemClock.now_us());
                    if !reports.is_empty() {
                        let link_id = link_id.to_owned();
                        let _ = self
                            .engine
                            .run(move |e| e.receiver_traffic(&link_id, reports))
                            .await;
                    }
                    return;
                }
                let frames: Vec<Vec<u8>> = if open.handle.stream {
                    open.reassemblers
                        .entry(peer.clone())
                        .or_default()
                        .push(&bytes, uxduck_mavlink::decode::check)
                } else {
                    split(&bytes)
                        .frames
                        .into_iter()
                        .map(<[u8]>::to_vec)
                        .collect()
                };
                record(link_id, open, &frames, at_us);
                let forward = open.forward;
                for frame in frames {
                    self.frame(link_id, &peer, &frame, forward, at_us).await;
                }
            }
        }
    }

    /// One whole frame: loop check, then the session (unless it came from a
    /// forward), then routing.
    async fn frame(&mut self, link_id: &str, peer: &str, frame: &[u8], forward: bool, at_us: u64) {
        let Some(header) = header(frame) else {
            return;
        };
        crate::diagnostics::record(
            link_id,
            Some(peer),
            InspectDirection::Received,
            frame,
            at_us,
        );
        let checked = self
            .router
            .check_loop((link_id, peer), frame, header, at_us);
        if checked.looped {
            crate::diagnostics::record_routing(link_id, peer, frame, header, &checked, at_us);
            self.send(checked).await;
            return;
        }
        let vehicle = if forward {
            self.session
                .forwarded(link_id, peer, frame, SystemClock.now_us());
            None
        } else {
            let effects =
                self.session
                    .ingest_received(link_id, peer, frame, at_us, SystemClock.now_us());
            self.apply(effects).await;
            self.session
                .vehicle_at(link_id, peer, header.system_id)
                .map(str::to_owned)
        };
        if let Some(vehicle_id) = &vehicle
            && let Some(payload) = uxduck_mavlink::framing::payload(frame)
        {
            crate::inspect::record(
                vehicle_id,
                InspectDirection::Received,
                header,
                payload,
                at_us,
            );
        }
        if let (Some(plugins), Some(vehicle_id)) = (&self.plugin_feed, &vehicle)
            && (plugins.wants_frame)(header.message_id)
            && let Some(payload) = uxduck_mavlink::framing::payload(frame)
        {
            (plugins.frame)(RawFrame {
                vehicle_id: vehicle_id.clone(),
                message_id: header.message_id,
                system_id: header.system_id,
                component_id: header.component_id,
                payload: payload.to_vec(),
            });
        }
        let reachable = self.session.routes();
        let routed = self.router.forward(
            (link_id, peer),
            frame,
            header,
            vehicle.as_deref(),
            &reachable,
            at_us,
        );
        crate::diagnostics::record_routing(link_id, peer, frame, header, &routed, at_us);
        self.send(routed).await;
    }

    async fn send(&mut self, routed: Routed) {
        for send in routed.sends {
            if let Some(open) = self.open.get(&send.link_id)
                && (open.forward || open.writable)
                && !self.session.is_paused(&send.link_id)
            {
                crate::diagnostics::record(
                    &send.link_id,
                    send.peer.as_deref(),
                    InspectDirection::Sent,
                    &send.bytes,
                    SystemClock.now_us(),
                );
                open.handle.send(Outgoing {
                    peer: send.peer,
                    bytes: send.bytes,
                });
            }
        }
        for detected in routed.disable {
            let link_id = detected.forward_link_id;
            tracing::warn!(link = %link_id, returned_on = %detected.received_link_id, peer = %detected.received_peer, "forward disabled by loop detection");
            self.close(&link_id);
            let _ = self
                .engine
                .run(move |e| {
                    e.link_disabled_by_loop(
                        &link_id,
                        &detected.received_link_id,
                        &detected.received_peer,
                    );
                })
                .await;
        }
    }

    async fn send_pilot(&mut self) {
        self.centre_stale_keyboards().await;
        let mut effects = Vec::new();
        for (vehicle_id, sticks, auxiliary, method) in self.controls.pilot_outputs() {
            effects.extend(self.session.pilot_control_with_method(
                &vehicle_id,
                sticks,
                &auxiliary,
                method,
            ));
        }
        self.apply(effects).await;
    }

    /// Keep keyboard control enabled while centring delayed steering input.
    async fn centre_stale_keyboards(&mut self) {
        for event in self.controls.stale_keyboard_inputs(SystemClock.now_us()) {
            let reports = self.controls.input(event);
            super::controllers::publish(self.controls.snapshot());
            self.report_controls(reports).await;
        }
    }

    /// Tells the engine about controller input it must decide on.
    async fn report_controls(&self, reports: Vec<Report>) {
        for report in reports {
            let _ = match report {
                Report::Connected {
                    id,
                    name,
                    layout_known,
                } => {
                    self.engine
                        .run(move |e| e.controller_connected(&id, &name, layout_known))
                        .await
                }
                Report::Disconnected { id } => {
                    self.engine
                        .run(move |e| e.controller_disconnected(&id))
                        .await
                }
                Report::Neutral { id, neutral } => {
                    self.engine
                        .run(move |e| e.controller_neutral(&id, neutral))
                        .await
                }
                Report::Pressed { id, button, code } => {
                    self.engine
                        .run(move |e| e.controller_pressed(&id, button, code))
                        .await
                }
                Report::Positioned { id, position } => {
                    self.engine
                        .run(move |e| e.controller_positioned(&id, &position))
                        .await
                }
            };
        }
    }

    /// A plugin's message by name, encoded and sent as the ground station
    /// (ADR-0006 X4). The engine checked the manifest allows it.
    fn message(&mut self, order: &uxduck_engine::runtime::MessageOrder) -> Vec<Effect> {
        let gcs = self.gcs.load(Ordering::Relaxed);
        match uxduck_mavlink::named::encode(&order.name, &order.fields) {
            Ok(encoded) => self.session.send_to(&order.vehicle_id, |_, sequence| {
                uxduck_mavlink::named::frame(
                    &encoded,
                    gcs,
                    uxduck_mavlink::GCS_COMPONENT_ID,
                    sequence,
                )
            }),
            Err(why) => {
                tracing::warn!(plugin = %order.plugin_id, %why, "a plugin's message was not sent");
                Vec::new()
            }
        }
    }

    async fn apply(&mut self, effects: Vec<Effect>) {
        if let Some(plugins) = &self.plugin_feed
            && (plugins.wants_items)()
        {
            for effect in &effects {
                if let Effect::Telemetry(update) = effect {
                    (plugins.items)(update);
                }
            }
        }
        for effect in effects {
            match glue::route(effect) {
                glue::Routed::Engine(report) => {
                    let _ = self.engine.run(report).await;
                }
                glue::Routed::Send {
                    link_id,
                    peer,
                    bytes,
                } => {
                    // A paused simulator would read it all at once on resuming.
                    if let Some(open) = self
                        .open
                        .get(&link_id)
                        .filter(|o| o.writable && !self.session.is_paused(&link_id))
                    {
                        inspect_sent(&self.session, &link_id, &peer, &bytes);
                        crate::diagnostics::record(
                            &link_id,
                            Some(&peer),
                            InspectDirection::Sent,
                            &bytes,
                            SystemClock.now_us(),
                        );
                        open.handle.send(Outgoing {
                            peer: Some(peer),
                            bytes,
                        });
                    }
                }
            }
        }
    }
}

/// A frame UXDuck sends, for the inspector: recorded for the vehicle at
/// its address, or for each of them when several share it, since which
/// one it is for would need the frame decoded.
fn inspect_sent(session: &Session, link_id: &str, peer: &str, bytes: &[u8]) {
    let (Some(header), Some(payload)) = (header(bytes), uxduck_mavlink::framing::payload(bytes))
    else {
        return;
    };
    let at_us = SystemClock.now_us();
    for (_, vehicle_id) in session.vehicles_at_peer(link_id, peer) {
        crate::inspect::record(vehicle_id, InspectDirection::Sent, header, payload, at_us);
    }
}

/// A ground station's heartbeat once a second on a simulated PX4's link,
/// as a ground station sends one: PX4 answers whoever spoke to it last
/// (until then it sends to port 14550), and without it reports the ground
/// station lost. It stops when the link closes. It comes from the system
/// id `gcs` holds when it is sent.
fn greet(sender: links::Sender, gcs: Arc<AtomicU8>) {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        let mut sequence: u8 = 0;
        loop {
            every.tick().await;
            crate::wakes::count("data plane: PX4 heartbeat (1 Hz)");
            let bytes = uxduck_mavlink::sim::gcs_heartbeat_frame(
                gcs.load(Ordering::Relaxed),
                uxduck_mavlink::GCS_COMPONENT_ID,
                sequence,
            );
            if !sender.send(Outgoing { peer: None, bytes }) {
                break;
            }
            sequence = sequence.wrapping_add(1);
        }
    });
}

/// Appends frames to the link's tlog, opening it on the first frame, and
/// indexes them for `mavlink/history` once flushed.
fn record(link_id: &str, open: &mut Open, frames: &[Vec<u8>], at_us: u64) {
    if frames.is_empty() {
        return;
    }
    if open.tlog.is_none() {
        open.tlog = std::fs::create_dir_all(open.tlog_path.parent().unwrap_or(Path::new(".")))
            .and_then(|()| File::create(&open.tlog_path))
            .map(BufWriter::new)
            .inspect_err(|error| tracing::warn!(%error, "cannot write the tlog"))
            .ok();
    }
    if let Some(log) = open.tlog.as_mut() {
        let mut written = Vec::with_capacity(frames.len());
        for frame in frames {
            let ok = uxduck_mavlink::tlog::record(log, at_us, frame).is_ok();
            if let Some(end) = open.tlog_end.as_mut() {
                if !ok {
                    open.tlog_end = None;
                    continue;
                }
                if let Some(header) = header(frame) {
                    written.push((header, *end));
                }
                *end += 8 + frame.len() as u64;
            }
        }
        // Indexed only once on disk, so a page never reads past it.
        if log.flush().is_ok() && !written.is_empty() {
            crate::tlog_index::indexed(link_id, &written);
        }
    }
}

/// A link's part in routing: it hears vehicles, or it carries their
/// traffic out (and, writable, back).
fn role(link: &Link) -> Role {
    if link.kind.is_forward() {
        Role::Forward {
            vehicles: link.vehicle_filter.clone(),
            from_link: link.from_link.clone(),
            writable: link.policy == LinkPolicy::ReadWrite,
        }
    } else {
        Role::Vehicles
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve {host}"))
}

/// Starts the transport for a link kind; a UDP client's hello comes from
/// the system id `gcs` holds.
async fn spawn(
    kind: &LinkKind,
    gcs: Arc<AtomicU8>,
    events: links::Events,
) -> Result<LinkHandle, (LinkStatus, String)> {
    match kind.clone() {
        LinkKind::UdpListen { host, port } => {
            let address = resolve(&host, port).map_err(|e| (LinkStatus::Down, e))?;
            links::udp_listen(address, events).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::AddrInUse {
                    (LinkStatus::PortInUse, in_use_message(Protocol::Udp, port))
                } else {
                    (LinkStatus::Down, error.to_string())
                }
            })
        }
        LinkKind::UdpForward { host, port } => {
            let address = resolve(&host, port).map_err(|e| (LinkStatus::Down, e))?;
            links::udp_target(address, events)
                .await
                .map_err(|e| (LinkStatus::Down, e.to_string()))
        }
        LinkKind::TcpClient { host, port } => Ok(links::tcp_client(host, port, events)),
        // Opened with its decoder, in `open`.
        LinkKind::UsbReceiver {} => Err((
            LinkStatus::Down,
            "a USB receiver stick opens with its decoder".into(),
        )),
        LinkKind::TcpServer { host, port } => {
            let address = resolve(&host, port).map_err(|e| (LinkStatus::Down, e))?;
            links::tcp_server(address, events).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::AddrInUse {
                    (LinkStatus::PortInUse, in_use_message(Protocol::Tcp, port))
                } else {
                    (LinkStatus::Down, error.to_string())
                }
            })
        }
        LinkKind::UdpClient { host, port } => {
            let address = resolve(&host, port).map_err(|e| (LinkStatus::Down, e))?;
            links::udp_client(address, gcs, events)
                .await
                .map_err(|e| (LinkStatus::Down, e.to_string()))
        }
        LinkKind::Serial { device, baud, usb } => Ok(links::serial(device, baud, usb, events)),
        // Started by the simulation manager in `Plane::open`.
        LinkKind::Simulator { .. } => Err((
            LinkStatus::Down,
            "a simulated vehicle starts through the simulation manager".to_owned(),
        )),
    }
}

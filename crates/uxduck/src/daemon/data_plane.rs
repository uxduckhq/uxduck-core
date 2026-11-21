//! The data plane: links, the MAVLink session, routing and tlog recording
//! (ADR-0004). It runs beside the engine and reports what it sees; the
//! engine keeps the entities. Frames are recorded byte for byte before they
//! are decoded, checked for loops before anything else sees them, and
//! forwarded without being changed.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use uxduck_contracts::dto::inspect::InspectDirection;
use uxduck_core::domain::link::{
    Link, LinkCarries, LinkKind, LinkPolicy, LinkStatus, SimFaultKind,
};
use uxduck_core::domain::link_failure::{LinkFailure, LinkOperation};
use uxduck_core::domain::transport::TransportError;
use uxduck_engine::plane as glue;
use uxduck_engine::runtime::{Clock, ControlRoute, LinkPort, TransferOrder};
use uxduck_mavlink::framing::{Reassembler, header, split};
use uxduck_mavlink::links::{self, LinkEvent, LinkHandle, Outgoing};
use uxduck_mavlink::payload::{Faults, OnLink};
use uxduck_mavlink::router::{LoopConfig, Role, Routed, Router};
use uxduck_mavlink::session::{Config, Effect, Session};

use super::controllers::{Controls, InputEvent, Report};
use super::engine_task::EngineHandle;
use crate::adapters::system::SystemClock;
use crate::ports::{Protocol, bind_refused};

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
    ConnectionOrder(Vec<String>),
    UseConnection(String, String),
    /// A simulator frozen or let run again (`sim/pause`).
    Pause(String, bool),
    /// The faults that are on in a simulation's camera and gimbal
    /// (`sim/fault`): all of them.
    Faults(String, Vec<uxduck_core::domain::link::SimFault>),
    /// Whether someone looks at where a vehicle's gimbals point (S406).
    WatchGimbals(String, bool),
    /// The gimbal a controller's inputs may turn, or none any more (S407).
    SteerGimbal(String, Option<uxduck_engine::runtime::GimbalRoute>),
    /// The daemon is stopping and reads input no more: what a controller
    /// was turning is told to stop first. Answers whether anything was.
    EndInput(tokio::sync::oneshot::Sender<bool>),
    /// A link's host looked up off this task; `lookup` says it is the
    /// link's latest open.
    Resolved {
        link: Link,
        address: Result<SocketAddr, LinkFailure>,
        lookup: Arc<()>,
    },
}

#[derive(Debug)]
struct Incoming {
    link_id: String,
    generation: Arc<()>,
    event: LinkEvent,
    at_us: u64,
    socket_generation: Option<u64>,
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
    fn connection_order(&mut self, order: &[String]) {
        let _ = self.0.send(Command::ConnectionOrder(order.to_vec()));
    }
    fn use_connection(&mut self, vehicle_id: &str, link_id: &str) {
        let _ = self.0.send(Command::UseConnection(
            vehicle_id.to_owned(),
            link_id.to_owned(),
        ));
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
    fn simulation_faults(&mut self, link_id: &str, faults: &[uxduck_core::domain::link::SimFault]) {
        let _ = self
            .0
            .send(Command::Faults(link_id.to_owned(), faults.to_vec()));
    }
    fn watch_gimbals(&mut self, vehicle_id: &str, watched: bool) {
        let _ = self
            .0
            .send(Command::WatchGimbals(vehicle_id.to_owned(), watched));
    }
    fn steer_gimbal(
        &mut self,
        controller_id: &str,
        gimbal: Option<uxduck_engine::runtime::GimbalRoute>,
    ) {
        let _ = self
            .0
            .send(Command::SteerGimbal(controller_id.to_owned(), gimbal));
    }
    fn rumble(&mut self, controller_id: &str, rumble: uxduck_core::domain::controller::Rumble) {
        super::controller_feedback::rumble(controller_id, rumble);
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

/// Tells the data plane that the daemon is stopping, so input is read no
/// more (S407).
#[derive(Clone, Debug)]
pub struct InputEnd(mpsc::UnboundedSender<Command>);

impl InputEnd {
    /// A gimbal a controller was turning is told to stop while its link
    /// still takes it: left alone it would keep the last rate it was sent.
    /// Waits only when there was one, for the stop to be said three times.
    pub async fn stop(&self) {
        let (told, answer) = tokio::sync::oneshot::channel();
        if self.0.send(Command::EndInput(told)).is_err() {
            return;
        }
        if matches!(
            tokio::time::timeout(Duration::from_millis(500), answer).await,
            Ok(Ok(true))
        ) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[must_use]
pub fn input_end(inbox: &LinkInbox) -> InputEnd {
    InputEnd(inbox.sender.clone())
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
    registration: links::RegistrationGate,
    socket_generation: u64,
    /// A stream's partial frames, by peer: a TCP server has a stream per
    /// connection.
    reassemblers: HashMap<String, Reassembler>,
    /// A receiver's traffic, by peer, when the link carries it (S172).
    traffic: Option<TrafficReaders>,
    /// The camera and gimbal a simulation carries (ADR-0063 Y2).
    carried: Option<Carried>,
    tlog_path: PathBuf,
    tlog: Option<BufWriter<File>>,
    /// Where the tlog's next record goes, for its index; None once a write
    /// failed, after which its records are no longer indexed.
    tlog_end: Option<u64>,
}

/// A test seam, never set in use: the mebibytes left on a simulated
/// camera's card, so a test or a demo sees a full card refuse a recording
/// without recording for hours (S405). With 0 the card is full.
pub const CAMERA_CARD_LEFT_ENV: &str = "UXDUCK_SIM_CAMERA_CARD_LEFT_MIB";

/// Test seams, never set in use: what a simulated camera says of its
/// definition file in place of its own (S411). The first is the address it
/// names, or `none` for a camera that names no file. The second is the
/// path of a file it serves over MAVLink FTP where its own would be.
pub const CAMERA_DEFINITION_ENV: &str = "UXDUCK_SIM_CAMERA_DEFINITION";
pub const CAMERA_DEFINITION_FILE_ENV: &str = "UXDUCK_SIM_CAMERA_DEFINITION_FILE";

/// What the seams say of a simulated camera's definition file. A file
/// that is named and cannot be read stops the daemon's link from carrying
/// the wrong camera: it is said, and the camera keeps its own.
fn camera_defined() -> Option<uxduck_mavlink::payload::Defined> {
    let uri = std::env::var(CAMERA_DEFINITION_ENV).ok();
    let path = std::env::var(CAMERA_DEFINITION_FILE_ENV).ok();
    if uri.is_none() && path.is_none() {
        return None;
    }
    let file = path.and_then(|path| match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            tracing::error!(%path, %error, "{CAMERA_DEFINITION_FILE_ENV} names a file that cannot be read");
            None
        }
    });
    Some(uxduck_mavlink::payload::Defined {
        uri: uri.map(|uri| Some(uri).filter(|uri| uri != "none")),
        file,
    })
}

/// A simulation's camera and gimbal (ADR-0063 Y2). They have no port of
/// their own: they speak on the simulation's link, beside its autopilot, and
/// every frame on it passes them, each way.
struct Carried {
    payload: OnLink,
    /// Where the autopilot is heard from, once it has spoken: what the
    /// payload says is taken as heard from there too.
    peer: Option<String>,
    /// The camera's picture (ADR-0063 Y7): the UDP port it is sent to, and
    /// the ticket it is kept under until the link closes.
    pictured: Option<(u16, u64)>,
    /// The faults the engine knows are on (`sim/fault`).
    faults: Faults,
}

impl Carried {
    /// What the camera sees now goes to whoever draws its picture.
    fn seen(&self) {
        if let Some((port, _)) = self.pictured {
            crate::sim::picture::pictures().sees(port, self.payload.view());
        }
    }

    /// The faults that ended by what happened on the wire since the engine
    /// last knew: a full card that was formatted, or a gimbal taken from
    /// the autopilot that held it.
    fn mended(&mut self) -> Vec<SimFaultKind> {
        let now = self.payload.faults();
        if now == self.faults {
            return Vec::new();
        }
        let before = std::mem::replace(&mut self.faults, now);
        SimFaultKind::ALL
            .into_iter()
            .filter(|kind| before.has(*kind) && !now.has(*kind))
            .collect()
    }
}

impl Drop for Carried {
    fn drop(&mut self) {
        if let Some((port, ticket)) = self.pictured {
            crate::sim::picture::pictures().leave(port, ticket);
        }
    }
}

impl Carried {
    /// What a link carries, with ArduPilot's simulator, whose defaults
    /// tell the autopilot of them (`MNT1_TYPE`, `CAM1_TYPE`), and with
    /// PX4's, whose parameters tell it of the gimbal (`MNT_MODE_IN`,
    /// `MNT_MODE_OUT`, S413). The engine refuses what neither would
    /// find. The vehicle's system id is the one its build is started
    /// with.
    fn of(kind: &LinkKind) -> Option<Self> {
        let LinkKind::Simulator {
            camera,
            gimbal,
            payload_profile,
            instance,
            ..
        } = *kind
        else {
            return None;
        };
        let instance = instance.unwrap_or(0);
        let mut payload = OnLink::new(crate::sim::system_id(instance), camera, gimbal)?
            .fitted(payload_profile.into());
        if let Some(mib) = std::env::var(CAMERA_CARD_LEFT_ENV)
            .ok()
            .and_then(|mib| mib.parse().ok())
        {
            payload.leave_room(mib);
        }
        if let Some(defined) = camera_defined() {
            payload.define(defined);
        }
        if !camera {
            return Some(Self {
                payload,
                peer: None,
                pictured: None,
                faults: Faults::default(),
            });
        }
        // The camera names where its picture is sent, a port of its own.
        let port = crate::sim::picture::port_for(instance);
        Some(Self {
            payload: payload.streaming(&crate::sim::picture::uri(port)),
            peer: None,
            pictured: Some((port, crate::sim::picture::pictures().carry(port))),
            faults: Faults::default(),
        })
    }
}

/// What simulated payloads said, each with its link, waiting to be taken as
/// frames their links received.
type Said = VecDeque<(String, Vec<u8>)>;

impl Open {
    /// What a simulated payload says goes down to the autopilot at once,
    /// whatever the link's policy, since it is the vehicle's own wiring and
    /// not UXDuck speaking, and waits in `said` for UXDuck to hear.
    fn payload_says(&self, link_id: &str, frames: Vec<Vec<u8>>, said: &mut Said) {
        for bytes in frames {
            self.handle.send(Outgoing {
                peer: None,
                bytes: bytes.clone(),
            });
            said.push_back((link_id.to_owned(), bytes));
        }
    }

    /// A frame on its way to the vehicle passes the payload it carries.
    /// Returns whether it was the payload's alone, and so is kept from the
    /// autopilot, which would answer it too (S400).
    fn payload_hears(&mut self, link_id: &str, frame: &[u8], said: &mut Said) -> bool {
        let Some(carried) = &mut self.carried else {
            return false;
        };
        let heard = carried.payload.from_ground(frame);
        carried.seen();
        self.payload_says(link_id, heard.said, said);
        heard.kept
    }
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
#[allow(
    clippy::too_many_lines,
    reason = "data plane event loop preserves ordered command handling"
)]
pub fn start(
    inbox: LinkInbox,
    engine: EngineHandle,
    config: Config,
    tlog_dir: PathBuf,
    plugin_feed: Option<PluginFeed>,
    simulators: std::sync::Arc<crate::sim::Simulators>,
    onboard: Option<super::onboard::Onboard>,
) {
    let LinkInbox {
        mut commands,
        sender,
        mut received,
        events,
    } = inbox;
    tokio::spawn(async move {
        let mut plane = Plane::new(engine, config, tlog_dir, plugin_feed, simulators, sender);
        plane.onboard = onboard.map(super::onboard::Computer::new);
        crate::network::changes::watch(plane.network.clone());
        // MANUAL_CONTROL for routed controllers, 25 times a second.
        let mut manual = timer(Duration::from_millis(40));
        // The tick also flushes telemetry, so it sets the update rate: 10 Hz.
        let mut tick = timer(Duration::from_millis(100));
        // The ground station heartbeat, 1 Hz while there are vehicles (S81),
        // and once more after the last is gone, for its link's last report
        // (S261).
        let mut ground = timer(Duration::from_secs(1));
        // Whether a simulated vehicle's build stopped on its own, twice a
        // second while one runs.
        let mut watch = timer(Duration::from_millis(500));
        loop {
            // Each timer runs only while it has work, so an idle daemon
            // sleeps until a command or a frame arrives (S24).
            let steering = plane.controls.routed();
            let ticking = plane.session.needs_tick();
            let vehicles = !plane.open.is_empty() || !plane.health.is_empty();
            // Simulators' builds and receiver sticks' decoders are watched.
            let simulating = plane
                .open
                .values()
                .any(|open| open.simulator.is_some() || open.decoder.is_some());
            tokio::select! {
                command = commands.recv() => match command {
                    None => break,
                    Some(Command::EndInput(told)) => {
                        let stops = plane.controls.gimbal_stops_all();
                        let any = !stops.is_empty();
                        plane.stop_gimbals(stops).await;
                        let _ = told.send(any);
                    }
                    Some(Command::Open(link)) => plane.open(link, &events).await,
                    Some(Command::Close(link_id)) => {
                        plane.close(&link_id).await;
                        plane.session.remove_link(&link_id);
                    }
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
                        let stops = plane.controls.gimbal_stops();
                        plane.stop_gimbals(stops).await;
                        super::controllers::publish(plane.controls.snapshot());
                        plane.report_controls(reports).await;
                    }
                    Some(Command::Route(id, route)) => {
                        if let Some(target) = plane.controls.target(&id) {
                            let effects = plane.session.release_control(&target);
                            plane.apply(effects).await;
                        }
                        let reports = plane.controls.route(&id, route);
                        let stops = plane.controls.gimbal_stops();
                        plane.stop_gimbals(stops).await;
                        plane.report_controls(reports).await;
                    }
                    Some(Command::SteerGimbal(id, gimbal)) => {
                        plane.controls.steer_gimbal(&id, gimbal);
                        let stops = plane.controls.gimbal_stops();
                        plane.stop_gimbals(stops).await;
                    }
                    Some(Command::GroundHeartbeat(vehicle_id, on)) => {
                        if on {
                            plane.withheld.remove(&vehicle_id);
                        } else {
                            tracing::warn!(%vehicle_id, "nobody supervises it: withholding the ground station heartbeat");
                            plane.withheld.insert(vehicle_id);
                        }
                        plane.refresh_registration();
                    }
                    Some(Command::ConnectionOrder(order)) => {
                        plane.session.set_connection_order(order);
                        let effects = plane.session.tick(SystemClock.now_us());
                        plane.apply(effects).await;
                    }
                    Some(Command::UseConnection(vehicle_id, link_id)) => {
                        let effects = plane.session.use_route(&vehicle_id, &link_id, SystemClock.now_us());
                        plane.apply(effects).await;
                    }
                    Some(Command::GcsSystemId(id)) => {
                        plane.gcs.store(id, Ordering::Relaxed);
                        plane.session.set_gcs_system_id(id);
                    }
                    Some(Command::Pause(link_id, paused)) => plane.pause(&link_id, paused).await,
                    Some(Command::Faults(link_id, faults)) => plane.faults(&link_id, &faults),
                    Some(Command::WatchGimbals(vehicle_id, watched)) => {
                        plane.session.watch_gimbals(&vehicle_id, watched);
                        // Asked for at once, not at the next tick's turn.
                        let effects = plane.session.tick(SystemClock.now_us());
                        plane.apply(effects).await;
                    }
                    Some(Command::Resolved { link, address, lookup }) => {
                        plane.resolved(link, address, &lookup, &events).await;
                    }
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
                    let effects = plane.time_syncs();
                    plane.apply(effects).await;
                    plane.report_link_health().await;
                    plane.report_clocks().await;
                    plane.refresh_registration();
                    plane.report_transports().await;
                    plane.send_onboard().await;
                    plane.announce_payloads();
                }
                _ = watch.tick(), if simulating => plane.simulators_stopped().await,
                _ = tick.tick(), if ticking => {
                    crate::wakes::count("data plane: tick (10 Hz)");
                    let effects = plane.session.tick(SystemClock.now_us());
                    plane.apply(effects).await;
                }
            }
            plane.payloads_said().await;
        }
    });
}

/// An onboard computer at an address of its own is asked for its clock
/// while it was heard this recently.
const COMPANION_HEARD_US: u64 = 10_000_000;

/// This computer's wall clock, in nanoseconds since 1970.
fn wall_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
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
    /// How each vehicle's link is doing (S261), on the clock of `started`.
    registration_vehicles: HashMap<String, std::collections::BTreeSet<String>>,
    health: uxduck_mavlink::link_health::Tracker,
    /// The vehicle at each address last reported, for the last report of
    /// an address that is gone.
    reported: std::collections::BTreeMap<(String, String, u8), String>,
    /// The monotonic clock's start, for round trips a wall clock change
    /// cannot bend.
    started: std::time::Instant,
    /// This computer's status, when it is on the vehicle (S295).
    onboard: Option<super::onboard::Computer>,
    /// Each onboard computer's clock against this computer's (S356).
    clocks: uxduck_mavlink::computer_clock::Clocks,
    /// Onboard computers that send from an address of their own (`uxduck
    /// companion` straight to this ground station), by address and
    /// component id, last heard when on the monotonic clock: each is asked
    /// for its clock there, since nothing else passes the request on.
    companion_peers: std::collections::BTreeMap<(String, String, u8, u8), u64>,
    /// This task's own queue, for host lookups that finish off it.
    commands: mpsc::UnboundedSender<Command>,
    /// Links waiting for their host to be looked up, by the latest lookup.
    resolving: HashMap<String, Arc<()>>,
    /// Says when this computer's network changes, so a UDP client waiting to
    /// retry does so at once.
    network: links::NetworkChanges,
    /// What simulated payloads said and UXDuck has not heard yet.
    said: Said,
}

/// Why a vehicle is not sent MAVLink's ping (S261): exactly where the
/// ground station heartbeat is not, so it never stands in for one. Not on
/// a read-only link or a forward (or a link not open), not to a vehicle
/// whose heartbeat is withheld (S81), and not to a paused simulator, which
/// would read it all at once on resuming. A replay's vehicle is never in
/// the session.
fn not_pinged(
    link: Option<(bool, bool)>,
    withheld: bool,
    paused: bool,
) -> Option<uxduck_core::domain::telemetry::RoundTripMissing> {
    use uxduck_core::domain::telemetry::RoundTripMissing;
    match link {
        None | Some((_, true)) => Some(RoundTripMissing::NotSent),
        Some((false, false)) => Some(RoundTripMissing::ReadOnly),
        Some((true, false)) if withheld => Some(RoundTripMissing::Unsupervised),
        Some((true, false)) if paused => Some(RoundTripMissing::Paused),
        Some((true, false)) => None,
    }
}

impl Plane {
    fn new(
        engine: EngineHandle,
        config: Config,
        tlog_dir: PathBuf,
        plugin_feed: Option<PluginFeed>,
        simulators: Arc<crate::sim::Simulators>,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Self {
        Self {
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
            health: uxduck_mavlink::link_health::Tracker::default(),
            registration_vehicles: HashMap::new(),
            reported: std::collections::BTreeMap::new(),
            started: std::time::Instant::now(),
            onboard: None,
            clocks: uxduck_mavlink::computer_clock::Clocks::default(),
            companion_peers: std::collections::BTreeMap::new(),
            commands,
            resolving: HashMap::new(),
            network: links::NetworkChanges::default(),
            said: Said::new(),
        }
    }

    async fn incoming(&mut self, incoming: Incoming) {
        if let Some(generation) = incoming.socket_generation
            && self
                .open
                .get(&incoming.link_id)
                .and_then(|o| o.handle.diagnostics.as_ref())
                .is_none_or(|d| {
                    let d = d.borrow();
                    d.generation != generation || !d.socket_open
                })
        {
            return;
        }

        if let Some(generation) = incoming.socket_generation {
            let changed = self.open.get_mut(&incoming.link_id).is_some_and(|open| {
                if open.socket_generation == generation {
                    return false;
                }
                open.socket_generation = generation;
                true
            });
            if changed {
                let effects = self
                    .session
                    .transport_lost(&incoming.link_id, SystemClock.now_us());
                self.apply(effects).await;
                self.session.transport_restored(&incoming.link_id);
            }
        }
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
    fn registration_withheld(&self, link_id: &str) -> bool {
        self.registration_vehicles
            .get(link_id)
            .is_some_and(|ids| !ids.is_empty() && ids.iter().all(|id| self.withheld.contains(id)))
    }

    fn refresh_registration(&mut self) {
        let mut known: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
        for (address, id) in self.session.addresses() {
            known
                .entry(address.0.clone())
                .or_default()
                .insert(id.to_owned());
        }
        self.registration_vehicles.extend(known);
        for (id, open) in &self.open {
            open.registration
                .renew(open.writable && !open.forward && !self.registration_withheld(id));
        }
    }

    async fn report_transports(&mut self) {
        let reports: Vec<_> = self
            .open
            .iter_mut()
            .filter_map(|(id, open)| {
                let receiver = open.handle.diagnostics.as_mut()?;
                if !receiver.has_changed().unwrap_or(false) {
                    return None;
                }
                Some((
                    id.clone(),
                    open.kind.clone(),
                    receiver.borrow_and_update().clone(),
                ))
            })
            .collect();
        for (id, kind, report) in reports {
            let (detail, error) = report
                .send_error
                .as_ref()
                .filter(|e| e.active)
                .or_else(|| report.receive_error.as_ref().filter(|e| e.active))
                .or_else(|| report.recovery_error.as_ref().filter(|e| e.active))
                .map(|e| transport_failure(e, host(&kind)))
                .unzip();
            let status = if report.socket_open {
                LinkStatus::Up
            } else {
                LinkStatus::Down
            };
            if report.configured_destination.is_some() {
                self.session.transport_send_health(
                    &id,
                    report.send_error.as_ref().is_some_and(|e| e.active),
                );
            } else {
                let mut latest = std::collections::BTreeMap::new();
                for send in report.sends.iter().filter(|s| s.outcome != "attempted") {
                    latest.insert(send.endpoint.clone(), send.outcome == "failed");
                }
                self.session.transport_peer_send_health(
                    &id,
                    latest
                        .into_iter()
                        .filter_map(|(peer, failed)| failed.then_some(peer))
                        .collect(),
                );
            }
            if report.socket_open {
                self.session.transport_restored(&id);
            } else {
                let effects = self.session.transport_lost(&id, SystemClock.now_us());
                self.apply(effects).await;
            }
            crate::diagnostics::transport(&id, report);
            self.report_failure(&id, status, detail, error).await;
        }
    }

    fn ground_heartbeats(&mut self) -> Vec<Effect> {
        let mut addresses: HashMap<(String, String), (String, bool)> = HashMap::new();
        for route in self.session.all_routes() {
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
        for ((link_id, peer), (_, withheld)) in addresses {
            if withheld
                || self
                    .open
                    .get(&link_id)
                    .is_none_or(|o| !o.writable || o.forward)
            {
                continue;
            }
            effects.extend(
                self.session
                    .send_address(&(link_id, peer, 0), |_, sequence| {
                        uxduck_mavlink::sim::gcs_heartbeat_frame(
                            gcs,
                            uxduck_mavlink::GCS_COMPONENT_ID,
                            sequence,
                        )
                    }),
            );
        }
        effects
    }

    fn now_mono_us(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// When a frame read now arrived, on this plane's monotonic clock, from
    /// its arrival `at_us` by the wall clock. A busy plane reads a frame
    /// tens of milliseconds after it arrived, and a round trip timed from
    /// the read would count that wait (S356, S261).
    fn arrived_mono_us(&self, at_us: u64) -> u64 {
        let waited_us = SystemClock.now_us().saturating_sub(at_us);
        self.now_mono_us().saturating_sub(waited_us)
    }

    /// MAVLink's ping, `TIMESYNC` (S261, ADR-0004 2026-11-01): a request
    /// from the ground station id to each vehicle the ground station
    /// heartbeat goes to, once a second with it, and only there
    /// ([`not_pinged`]). Its `ts1` is this plane's monotonic clock. The
    /// vehicle's onboard computers answer the same request with their clocks
    /// (S356), and one that sends from an address of its own is asked there.
    fn time_syncs(&mut self) -> Vec<Effect> {
        let gcs = self.gcs.load(Ordering::Relaxed);
        let now_us = self.now_mono_us();
        let sent = uxduck_mavlink::computer_clock::Moment {
            mono_us: now_us,
            wall_ns: wall_ns(),
        };
        let addresses: Vec<((String, String, u8), String)> = self
            .session
            .addresses()
            .map(|(address, id)| (address.clone(), id.to_owned()))
            .collect();
        let mut effects = Vec::new();
        for (address, vehicle_id) in addresses {
            let link = self.open.get(&address.0).map(|o| (o.writable, o.forward));
            let why = not_pinged(
                link,
                self.withheld.contains(&vehicle_id),
                self.session.is_paused(&address.0),
            );
            if let Some(why) = why {
                self.health.not_asked(&address, why);
                continue;
            }
            let ts1 = self.health.asked(&address, now_us);
            self.clocks.asked(ts1, sent);
            effects.extend(self.session.send_address(&address, |system_id, sequence| {
                uxduck_mavlink::sim::timesync_frame(
                    (gcs, uxduck_mavlink::GCS_COMPONENT_ID, sequence),
                    (0, ts1),
                    (system_id, 0),
                )
            }));
        }
        self.companion_peers
            .retain(|_, heard| now_us.saturating_sub(*heard) < COMPANION_HEARD_US);
        let peers: Vec<(String, String, u8, u8)> = self.companion_peers.keys().cloned().collect();
        for (link_id, peer, system_id, component_id) in peers {
            let Some(vehicle_id) = self
                .session
                .vehicle_of(&link_id, &peer, system_id, component_id)
                .map(str::to_owned)
            else {
                continue;
            };
            let link = self.open.get(&link_id).map(|o| (o.writable, o.forward));
            let why = not_pinged(
                link,
                self.withheld.contains(&vehicle_id),
                self.session.is_paused(&link_id),
            );
            if why.is_some() {
                continue;
            }
            let ts1 = self.clocks.ask(sent);
            let address = (link_id, peer, system_id);
            effects.extend(self.session.send_address(&address, |system_id, sequence| {
                uxduck_mavlink::sim::timesync_frame(
                    (gcs, uxduck_mavlink::GCS_COMPONENT_ID, sequence),
                    (0, ts1),
                    (system_id, component_id),
                )
            }));
        }
        effects
    }

    /// Tells the engine each onboard computer's clock (S356), once a
    /// second, after forgetting the computers of vehicles no longer known.
    async fn report_clocks(&mut self) {
        let known: std::collections::HashSet<String> = self
            .session
            .addresses()
            .map(|(_, id)| id.to_owned())
            .collect();
        self.clocks.retain(|id| known.contains(id));
        let reports = self.clocks.report(self.now_mono_us());
        if reports.is_empty() {
            return;
        }
        let _ = self
            .engine
            .run(move |e| e.computer_clocks_observed(reports))
            .await;
    }

    /// Tells the engine how each vehicle's link is doing (S261), once a
    /// second, and forgets the addresses of vehicles no longer known, with
    /// one last report saying nothing goes there now, so the agent and the
    /// windows do not keep its last round trip.
    async fn report_link_health(&mut self) {
        let now_us = self.now_mono_us();
        let addresses: std::collections::BTreeMap<(String, String, u8), String> = self
            .session
            .addresses()
            .map(|(address, id)| (address.clone(), id.to_owned()))
            .collect();
        let dropped = self
            .health
            .retain(|address| addresses.contains_key(address), now_us);
        let mut reports: Vec<_> = dropped
            .into_iter()
            .filter_map(|(address, last)| {
                let id = self.reported.get(&address)?;
                (!addresses.values().any(|known| known == id)).then(|| (id.clone(), last))
            })
            .collect();
        let mut readings = Vec::new();
        let wall_us = SystemClock.now_us();
        for (address, id) in &addresses {
            let health = self.health.report(address, now_us);
            self.session.route_health(address.clone(), health.clone());
            self.session
                .route_uplink(address.clone(), self.health.uplink(address, now_us));
            if self.session.telemetry_address(id) == Some(address) {
                reports.push((id.clone(), health.clone()));
            }
            readings.push(uxduck_engine::connection_history::ConnectionReading {
                vehicle_id: id.clone(),
                link_id: address.0.clone(),
                health,
                radio: self.session.radio_signal(&address.0, wall_us),
            });
        }
        self.reported = addresses;
        if !readings.is_empty() {
            let _ = self
                .engine
                .run(move |e| e.connections_sampled(readings))
                .await;
        }
        if reports.is_empty() {
            return;
        }
        let _ = self
            .engine
            .run(move |e| e.link_health_observed(reports))
            .await;
    }

    /// Opens a link. A host name is looked up on a task of its own, so a
    /// lookup that hangs (offline, or a `.local` name) never holds up the
    /// other links; the link opens when it is found.
    async fn open(&mut self, link: Link, incoming: &mpsc::Sender<Incoming>) {
        self.resolving.remove(&link.id);
        if matches!(link.kind, LinkKind::UdpClient { .. }) {
            return self.open_at(link, None, incoming).await;
        }
        let Some((host, port)) = host(&link.kind) else {
            return self.open_at(link, None, incoming).await;
        };
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self
                .open_at(link, Some(SocketAddr::new(ip, port)), incoming)
                .await;
        }
        self.close(&link.id).await;
        let lookup = Arc::new(());
        self.resolving.insert(link.id.clone(), lookup.clone());
        let host = host.to_owned();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let address = match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(mut found) => found.next().ok_or_else(|| {
                    LinkFailure::name_not_found(&std::io::Error::other(format!(
                        "no addresses found for {host}"
                    )))
                }),
                Err(error) => Err(LinkFailure::name_not_found(&error)),
            };
            let _ = commands.send(Command::Resolved {
                link,
                address,
                lookup,
            });
        });
    }

    /// A link's host was looked up: it opens there, unless it was closed
    /// or opened again meanwhile.
    async fn resolved(
        &mut self,
        link: Link,
        address: Result<SocketAddr, LinkFailure>,
        lookup: &Arc<()>,
        incoming: &mpsc::Sender<Incoming>,
    ) {
        if !self
            .resolving
            .get(&link.id)
            .is_some_and(|latest| Arc::ptr_eq(latest, lookup))
        {
            return;
        }
        self.resolving.remove(&link.id);
        match address {
            Ok(address) => self.open_at(link, Some(address), incoming).await,
            Err(failure) => {
                let (host, port) = host(&link.kind).unwrap_or_default();
                let detail = failure.words(host, port);
                self.report_failure(&link.id, LinkStatus::Down, Some(detail), Some(failure))
                    .await;
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "transport-specific setup and one complete open state"
    )]
    async fn open_at(
        &mut self,
        link: Link,
        address: Option<SocketAddr>,
        incoming: &mpsc::Sender<Incoming>,
    ) {
        // A simulator started again on its link (`sim/restart`) must be
        // gone before the next one takes its ports; waited for off this
        // task, for the few hundred milliseconds its guard takes.
        let previous = self
            .open
            .get_mut(&link.id)
            .and_then(|open| open.simulator.take());
        self.close(&link.id).await;
        if let Some(previous) = previous {
            let _ = tokio::task::spawn_blocking(move || previous.stop()).await;
        }
        if link.kind.is_simulator() {
            self.reaped(&link.id).await;
        }
        if link.kind.is_simulator() {
            self.session.simulated(&link.id);
        }
        self.session.configure_link(&link);
        let (events, mut received) = links::event_channel();
        let destination = incoming.clone();
        let link_id = link.id.clone();
        let generation = Arc::new(());
        let receiving_generation = generation.clone();
        let relay = tokio::spawn(async move {
            while let Some(links::TimedEvent {
                event,
                at_us,
                socket_generation,
            }) = received.recv().await
            {
                let command = Incoming {
                    link_id: link_id.clone(),
                    generation: receiving_generation.clone(),
                    event,
                    at_us,
                    socket_generation,
                };
                if destination.send(command).await.is_err() {
                    break;
                }
            }
        });
        // A simulated vehicle: its build starts first, then the link
        // connects to its SERIAL0, retrying while it starts (S17); PX4's
        // is UDP, and is greeted until it answers.
        let registration = links::RegistrationGate::default();
        registration
            .renew(link.policy == LinkPolicy::ReadWrite && !self.registration_withheld(&link.id));
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
                        Err(error) => Err(failed(LinkOperation::Socket, &error, "127.0.0.1", port)),
                    }
                }
                Err(detail) => Err((LinkStatus::Down, detail, None)),
            }
        } else if let LinkKind::UdpClient { host, port } = &link.kind {
            Ok(links::udp_client_host(
                host.clone(),
                *port,
                self.gcs.clone(),
                registration.clone(),
                &self.network,
                events,
            ))
        } else {
            spawn(&link.kind, address, self.gcs.clone(), events).await
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
                        registration,
                        socket_generation: 0,
                        reassemblers: HashMap::new(),
                        traffic: TrafficReaders::new(link.carries),
                        carried: Carried::of(&link.kind),
                        tlog_path: tlog_path.clone(),
                        tlog: None,
                        tlog_end: Some(0),
                    },
                );
                crate::tlog_index::open(&link.id, &tlog_path);
            }
            Err((status, detail, error)) => {
                relay.abort();
                self.report_failure(&link.id, status, Some(detail), error)
                    .await;
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
        self.session.configure_link(&link);
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
        Result<LinkHandle, Failed>,
        Option<crate::receiver_stick::Decoder>,
    ) {
        let log = self.tlog_dir.join(format!("{}-decoder.log", link.id));
        match crate::receiver_stick::launch(&log) {
            Ok(decoder) => {
                let handle = links::tcp_client("127.0.0.1".into(), decoder.port, events);
                (Ok(handle), Some(decoder))
            }
            Err(detail) => (Err((LinkStatus::Down, detail, None)), None),
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
            self.close(&link_id).await;
            self.report(&link_id, LinkStatus::Down, Some(detail)).await;
        }
        for (link_id, detail) in stopped {
            tracing::warn!(link = %link_id, %detail, "a simulated vehicle's build stopped by itself");
            self.close(&link_id).await;
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

    /// Switches faults on and off in a simulation's camera and gimbal: what
    /// is given is all that is on. A link that carries neither has none.
    fn faults(&mut self, link_id: &str, faults: &[uxduck_core::domain::link::SimFault]) {
        let Some(carried) = self
            .open
            .get_mut(link_id)
            .and_then(|open| open.carried.as_mut())
        else {
            tracing::warn!(link = %link_id, "no simulated camera or gimbal to switch faults in");
            return;
        };
        carried.payload.set_faults(Faults::of(faults));
        // One that does not fit what it carries is not on, and the engine
        // hears so with the ones that end by themselves.
        carried.faults = Faults::of(faults);
        carried.seen();
    }

    /// Tells the engine of faults that ended by themselves.
    async fn faults_mended(&mut self) {
        let mut mended = Vec::new();
        for (link_id, open) in &mut self.open {
            if let Some(carried) = &mut open.carried {
                mended.extend(
                    carried
                        .mended()
                        .into_iter()
                        .map(|kind| (link_id.clone(), kind)),
                );
            }
        }
        for (link_id, kind) in mended {
            let _ = self
                .engine
                .run(move |e| e.simulation_fault_ended(&link_id, kind))
                .await;
        }
    }

    /// A simulated payload's heartbeats on this computer's clock: as its
    /// link comes up, and once a second after, until the autopilot's clock
    /// paces them (`Payload::announce`). A paused simulator is sent nothing.
    fn announce_payload(&mut self, link_id: &str) {
        if self.session.is_paused(link_id) {
            return;
        }
        if let Some(open) = self.open.get_mut(link_id)
            && let Some(carried) = &mut open.carried
        {
            let heartbeats = carried.payload.announce();
            open.payload_says(link_id, heartbeats, &mut self.said);
        }
    }

    fn announce_payloads(&mut self) {
        let carrying: Vec<String> = self
            .open
            .iter()
            .filter(|(_, open)| open.carried.is_some())
            .map(|(link_id, _)| link_id.clone())
            .collect();
        for link_id in carrying {
            self.announce_payload(&link_id);
        }
    }

    /// UXDuck hears what simulated payloads said, as frames received on
    /// their links from where the autopilot is: into the tlog, the session,
    /// the inspector and the forwards, like any component's (ADR-0063 Y1).
    /// Before the autopilot has spoken there is nowhere they came from, and
    /// they are for the autopilot alone. Hearing one may send a frame the
    /// payload answers, so this runs until nothing more was said.
    async fn payloads_said(&mut self) {
        while let Some((link_id, frame)) = self.said.pop_front() {
            let Some(open) = self.open.get_mut(&link_id) else {
                continue;
            };
            let Some(peer) = open.carried.as_ref().and_then(|c| c.peer.clone()) else {
                continue;
            };
            let at_us = SystemClock.now_us();
            record(&link_id, open, std::slice::from_ref(&frame), at_us);
            let forward = open.forward;
            self.frame(&link_id, &peer, &frame, forward, at_us).await;
        }
        self.faults_mended().await;
    }

    async fn close(&mut self, link_id: &str) {
        self.resolving.remove(link_id);
        if self.open.remove(link_id).is_some() {
            crate::tlog_index::close(link_id);
        }
        self.router.remove_link(link_id);
        let effects = self.session.transport_lost(link_id, SystemClock.now_us());
        self.apply(effects).await;
    }

    async fn report(&self, link_id: &str, status: LinkStatus, detail: Option<String>) {
        self.report_failure(link_id, status, detail, None).await;
    }

    /// A link's status, with what the operating system said when it failed.
    async fn report_failure(
        &self,
        link_id: &str,
        status: LinkStatus,
        detail: Option<String>,
        error: Option<LinkFailure>,
    ) {
        let link_id = link_id.to_owned();
        let _ = self
            .engine
            .run(move |e| e.link_failed(&link_id, status, detail, error))
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
            LinkEvent::Up => {
                self.session.transport_restored(link_id);
                // ArduPilot looks for a camera and a gimbal in its first
                // minute, or until it is armed (S400): they speak first.
                self.announce_payload(link_id);
                self.report(link_id, LinkStatus::Up, None).await;
            }
            LinkEvent::Down { detail, error } => {
                let effects = self.session.transport_lost(link_id, SystemClock.now_us());
                self.apply(effects).await;
                self.report_failure(link_id, LinkStatus::Down, Some(detail), error)
                    .await;
            }
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
                // A simulated payload hears its autopilot too: its clock,
                // its questions and its gimbal's targets (ADR-0063 Y2).
                for frame in &frames {
                    let Some(carried) = &mut open.carried else {
                        break;
                    };
                    carried.peer.get_or_insert_with(|| peer.clone());
                    let answers = carried.payload.from_autopilot(frame);
                    open.payload_says(link_id, answers, &mut self.said);
                }
                if let Some(carried) = &open.carried {
                    carried.seen();
                }
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
    #[allow(
        clippy::too_many_lines,
        reason = "each frame's steps, in the order they run"
    )]
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
        let sources = if forward {
            Vec::new()
        } else {
            self.session
                .redundant_sources(link_id, peer, header.system_id)
        };
        let checked =
            self.router
                .check_loop_with_sources((link_id, peer), frame, header, at_us, &sources);
        if checked.looped {
            crate::diagnostics::record_routing(link_id, peer, frame, header, &checked, at_us);
            self.send(checked).await;
            return;
        }
        if forward && header.message_id == uxduck_mavlink::link_health::TIMESYNC {
            self.answer_clock(link_id, peer, frame).await;
        }
        if !forward && let Some(computer) = &mut self.onboard {
            let mut coin = [0_u8; 1];
            let _ = getrandom::fill(&mut coin);
            computer.heard(
                header.system_id,
                header.component_id,
                at_us,
                coin[0] & 1 == 1,
            );
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
            self.refresh_registration();
            let vehicle = self
                .session
                .vehicle_at(link_id, peer, header.system_id)
                .map(str::to_owned);
            if vehicle.is_some() {
                let address = (link_id.to_owned(), peer.to_owned(), header.system_id);
                let (gcs, arrived_us) = (
                    self.gcs.load(Ordering::Relaxed),
                    self.arrived_mono_us(at_us),
                );
                self.health.heard(&address, frame, gcs, arrived_us);
            }
            vehicle
        };
        if !forward && uxduck_mavlink::computer::COMPUTER_IDS.contains(&header.component_id) {
            self.computer_heard(link_id, peer, frame, header, vehicle.as_deref(), at_us)
                .await;
        }
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

    /// The ground station asking this onboard computer's clock through the
    /// forward it reached it by (S356): answered there at once. The request
    /// still goes on to the vehicle, whose autopilot answers it.
    async fn answer_clock(&mut self, link_id: &str, peer: &str, frame: &[u8]) {
        let Some(computer) = &mut self.onboard else {
            return;
        };
        let sends: Vec<uxduck_mavlink::router::Send> = computer
            .answer(frame)
            .into_iter()
            .map(|bytes| uxduck_mavlink::router::Send {
                link_id: link_id.to_owned(),
                peer: Some(peer.to_owned()),
                bytes,
            })
            .collect();
        if sends.is_empty() {
            return;
        }
        self.send(Routed {
            sends,
            ..Routed::default()
        })
        .await;
    }

    /// An onboard computer of a known vehicle asking this computer's clock
    /// (S373), to set its own when nothing else keeps it: answered with this
    /// computer's wall clock at the address it asked from, as UXDuck asks
    /// its clock there (S356).
    async fn answer_computer_clock(
        &mut self,
        link_id: &str,
        peer: &str,
        frame: &[u8],
        vehicle: Option<&str>,
    ) {
        let gcs = self.gcs.load(Ordering::Relaxed);
        let Some((ts1, asker)) = uxduck_mavlink::computer::asks_ground(frame, gcs) else {
            return;
        };
        let Some(vehicle_id) = vehicle
            .or_else(|| self.session.vehicle_of(link_id, peer, asker.0, asker.1))
            .map(str::to_owned)
        else {
            return;
        };
        let link = self.open.get(link_id).map(|o| (o.writable, o.forward));
        if not_pinged(
            link,
            self.withheld.contains(&vehicle_id),
            self.session.is_paused(link_id),
        )
        .is_some()
        {
            return;
        }
        let address = (link_id.to_owned(), peer.to_owned(), asker.0);
        let effects = self.session.send_address(&address, |_, sequence| {
            uxduck_mavlink::sim::timesync_frame(
                (gcs, uxduck_mavlink::GCS_COMPONENT_ID, sequence),
                (wall_ns(), ts1),
                asker,
            )
        });
        self.apply(effects).await;
    }

    /// A frame from an onboard computer of a vehicle (S356): its clock's
    /// `TIMESYNC` reply or status time stamp, as of when it arrived, and,
    /// when it sends from an address of its own, that address, to ask it
    /// there, timed from when it arrived. Its request for this computer's
    /// clock is answered first (S373).
    async fn computer_heard(
        &mut self,
        link_id: &str,
        peer: &str,
        frame: &[u8],
        header: uxduck_mavlink::framing::Header,
        vehicle: Option<&str>,
        at_us: u64,
    ) {
        self.answer_computer_clock(link_id, peer, frame, vehicle)
            .await;
        let now_us = self.now_mono_us();
        let owner = if let Some(vehicle_id) = vehicle {
            vehicle_id.to_owned()
        } else {
            let Some(owner) =
                self.session
                    .vehicle_of(link_id, peer, header.system_id, header.component_id)
            else {
                return;
            };
            self.companion_peers.insert(
                (
                    link_id.to_owned(),
                    peer.to_owned(),
                    header.system_id,
                    header.component_id,
                ),
                now_us,
            );
            owner.to_owned()
        };
        let heard = uxduck_mavlink::computer_clock::Moment {
            mono_us: self.arrived_mono_us(at_us),
            wall_ns: i64::try_from(at_us)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        };
        let gcs = self.gcs.load(Ordering::Relaxed);
        self.clocks.heard(&owner, frame, gcs, heard);
    }

    /// This computer's heartbeat and status (S295), out of every forward,
    /// from each vehicle's system id.
    async fn send_onboard(&mut self) {
        let Some(computer) = &mut self.onboard else {
            return;
        };
        let systems: Vec<u8> = self
            .session
            .addresses()
            .map(|(address, _)| address.2)
            .collect();
        let frames = computer.frames(systems, SystemClock.now_us());
        let sends = self
            .open
            .iter()
            .filter(|(_, open)| open.forward)
            .flat_map(|(link_id, _)| {
                frames.iter().map(|bytes| uxduck_mavlink::router::Send {
                    link_id: link_id.clone(),
                    peer: None,
                    bytes: bytes.clone(),
                })
            })
            .collect();
        self.send(Routed {
            sends,
            ..Routed::default()
        })
        .await;
    }

    async fn send(&mut self, routed: Routed) {
        let mut sent = Vec::new();
        for send in routed.sends {
            if let Some(open) = self.open.get_mut(&send.link_id)
                && (open.forward || open.writable)
                && !self.session.is_paused(&send.link_id)
                && open.handle.diagnostics.as_ref().is_none_or(|d| {
                    let d = d.borrow();
                    d.socket_open && d.generation == open.socket_generation
                })
            {
                let at_us = SystemClock.now_us();
                let remote = open
                    .handle
                    .diagnostics
                    .as_ref()
                    .and_then(|d| d.borrow().remote_endpoint.clone());
                crate::diagnostics::record(
                    &send.link_id,
                    send.peer.as_deref().or(remote.as_deref()),
                    InspectDirection::Sent,
                    &send.bytes,
                    at_us,
                );
                // A forward's tlog keeps what it hears; what it carries out
                // is already in the vehicle's link's.
                let keep = (!open.forward).then(|| send.bytes.clone());
                if !open.payload_hears(&send.link_id, &send.bytes, &mut self.said) {
                    open.handle.send(Outgoing {
                        peer: send.peer,
                        bytes: send.bytes,
                    });
                }
                if let Some(bytes) = keep {
                    sent.push((send.link_id, bytes, at_us));
                }
            }
        }
        self.record_sent(sent);
        for detected in routed.disable {
            let link_id = detected.forward_link_id;
            tracing::warn!(link = %link_id, returned_on = %detected.received_link_id, peer = %detected.received_peer, "forward disabled by loop detection");
            self.close(&link_id).await;
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
        if self.controls.expire_screen_holds(SystemClock.now_us()) {
            super::controllers::publish(self.controls.snapshot());
        }
        let held = self.controls.due_holds(SystemClock.now_us());
        self.report_controls(held).await;
        let mut effects = Vec::new();
        for (vehicle_id, sticks, auxiliary, method, buttons) in self.controls.pilot_outputs() {
            effects.extend(self.session.pilot_control_with_method(
                &vehicle_id,
                sticks,
                &auxiliary,
                method,
                buttons,
            ));
        }
        // What the gimbal inputs ask for (S407): the rates beside the
        // pilot input, and a stop where they ended.
        let gimbals = self.controls.gimbal_outputs();
        for (gimbal, rates) in &gimbals.rates {
            effects.extend(self.session.gimbal_rates(
                &gimbal.vehicle_id,
                gimbal.component_id,
                gimbal.device_id,
                *rates,
            ));
        }
        self.apply(effects).await;
        let stops = self.controls.gimbal_stops();
        self.stop_gimbals(stops).await;
        self.report_controls(gimbals.reports).await;
        // What the camera inputs ask for (S416), for the engine to send.
        let cameras = self.controls.camera_outputs(SystemClock.now_us());
        self.report_controls(cameras).await;
    }

    /// Tells gimbals whose rates ended to stop turning (S407).
    async fn stop_gimbals(&mut self, stops: Vec<uxduck_engine::runtime::GimbalRoute>) {
        let mut effects = Vec::new();
        for gimbal in stops {
            effects.extend(self.session.gimbal_stop(
                &gimbal.vehicle_id,
                gimbal.component_id,
                gimbal.device_id,
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
                Report::SecondUnit {
                    id,
                    name,
                    connected,
                } => {
                    self.engine
                        .run(move |e| e.controller_second_unit(&id, &name, connected))
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
                Report::Holding {
                    id,
                    command,
                    hold_ms,
                } => {
                    self.engine
                        .run(move |e| e.controller_holding(&id, command, hold_ms))
                        .await
                }
                Report::LetGo { id } => self.engine.run(move |e| e.controller_let_go(&id)).await,
                Report::GimbalAsked { id } => {
                    self.engine
                        .run(move |e| e.controller_gimbal_asked(&id))
                        .await
                }
                Report::GimbalLetGo { id } => {
                    self.engine
                        .run(move |e| e.controller_gimbal_let_go(&id))
                        .await
                }
                Report::CameraDrive { id, drive } => {
                    self.engine
                        .run(move |e| e.controller_camera_drive(&id, drive))
                        .await
                }
                Report::Gesture { id, binding } => {
                    self.engine
                        .run(move |e| e.controller_gesture(&id, &binding))
                        .await
                }
                Report::Power { id, power } => {
                    self.engine
                        .run(move |e| e.controller_power(&id, power))
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
        let mut sent = Vec::new();
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
                    if let Some(open) = self.open.get_mut(&link_id).filter(|o| {
                        o.writable
                            && !self.session.is_paused(&link_id)
                            && o.handle.diagnostics.as_ref().is_none_or(|d| {
                                let d = d.borrow();
                                d.socket_open && d.generation == o.socket_generation
                            })
                    }) {
                        let at_us = SystemClock.now_us();
                        inspect_sent(&self.session, &link_id, &peer, &bytes);
                        crate::diagnostics::record(
                            &link_id,
                            Some(&peer),
                            InspectDirection::Sent,
                            &bytes,
                            at_us,
                        );
                        if !open.payload_hears(&link_id, &bytes, &mut self.said) {
                            open.handle.send(Outgoing {
                                peer: Some(peer),
                                bytes: bytes.clone(),
                            });
                        }
                        if !open.forward {
                            sent.push((link_id, bytes, at_us));
                        }
                    }
                }
            }
        }
        self.record_sent(sent);
    }

    /// What went out on vehicles' links, into their tlogs (ADR-0050 L1),
    /// once every frame of the batch has been handed to its link, so
    /// writing never holds a send up.
    fn record_sent(&mut self, sent: Vec<(String, Vec<u8>, u64)>) {
        for (link_id, bytes, at_us) in sent {
            if let Some(open) = self.open.get_mut(&link_id) {
                record(&link_id, open, std::slice::from_ref(&bytes), at_us);
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

/// Appends frames received or sent to the link's tlog, opening it on the
/// first frame, and says where the latest begins once flushed.
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
        let mut latest = None;
        for frame in frames {
            let ok = uxduck_mavlink::tlog::record(log, at_us, frame).is_ok();
            if let Some(end) = open.tlog_end.as_mut() {
                if !ok {
                    open.tlog_end = None;
                    continue;
                }
                if header(frame).is_some() {
                    latest = Some(*end);
                }
                *end += 8 + frame.len() as u64;
            }
        }
        // Said only once on disk, so a flight never starts past it.
        if log.flush().is_ok()
            && let Some(latest) = latest
        {
            crate::tlog_index::written(link_id, latest);
        }
    }
}

/// A link's part in routing: it hears vehicles, from whoever sends to it or
/// through the one endpoint it dialed, or it carries their traffic out (and,
/// writable, back).
fn role(link: &Link) -> Role {
    if link.kind.is_forward() {
        Role::Forward {
            vehicles: link.vehicle_filter.clone(),
            from_link: link.from_link.clone(),
            writable: link.policy == LinkPolicy::ReadWrite,
        }
    } else if matches!(
        link.kind,
        LinkKind::UdpClient { .. } | LinkKind::TcpClient { .. } | LinkKind::Serial { .. }
    ) {
        Role::Dialed
    } else {
        Role::Vehicles
    }
}

/// A UDP socket's active error, in plain words for what it was doing, and
/// as the operating system said it (FB-267). A lookup or bind reads at the
/// link's configured host and port. A datagram reads at the peer it went
/// to when the socket names one.
fn transport_failure(
    error: &TransportError,
    configured: Option<(&str, u16)>,
) -> (String, LinkFailure) {
    let operation = match error.operation.as_str() {
        "resolve" => LinkOperation::Lookup,
        "bind" | "resolve-or-bind" => LinkOperation::Bind,
        "open" => LinkOperation::Socket,
        _ => LinkOperation::Datagram,
    };
    let failure = LinkFailure {
        kind: error.failure,
        operation,
        os_code: error.os_code,
        message: error.message.clone(),
    };
    let peer = error
        .endpoint
        .as_deref()
        .and_then(|at| at.parse::<SocketAddr>().ok())
        .filter(|_| operation == LinkOperation::Datagram);
    let detail = match (peer, configured) {
        (Some(peer), _) => failure.words(&peer.ip().to_string(), peer.port()),
        (None, Some((host, port))) => failure.words(host, port),
        (None, None) => failure.message.clone(),
    };
    (detail, failure)
}

/// The host a link kind opens at, for kinds that need its address before
/// they start; a TCP client looks its host up itself on every connect.
fn host(kind: &LinkKind) -> Option<(&str, u16)> {
    match kind {
        LinkKind::UdpListen { host, port }
        | LinkKind::UdpForward { host, port }
        | LinkKind::TcpServer { host, port }
        | LinkKind::UdpClient { host, port } => Some((host, *port)),
        _ => None,
    }
}

/// Why a listener could not start, as the check before a link is saved
/// says it (FB-267).
fn listen_error(protocol: Protocol, host: &str, port: u16, error: &std::io::Error) -> Failed {
    let refused = bind_refused(protocol, host, port, error);
    let status = if refused.in_use() {
        LinkStatus::PortInUse
    } else {
        LinkStatus::Down
    };
    (status, refused.words, refused.failure)
}

/// Why a link failed: its status, the plain words and what the operating
/// system said (FB-267).
type Failed = (LinkStatus, String, Option<LinkFailure>);

/// A link that failed opening at `host`:`port`.
fn failed(operation: LinkOperation, error: &std::io::Error, host: &str, port: u16) -> Failed {
    let failure = LinkFailure::from_io(operation, error);
    (LinkStatus::Down, failure.words(host, port), Some(failure))
}

/// Starts the transport for a link kind; a UDP client's hello comes from
/// the system id `gcs` holds.
async fn spawn(
    kind: &LinkKind,
    address: Option<SocketAddr>,
    gcs: Arc<AtomicU8>,
    events: links::Events,
) -> Result<LinkHandle, Failed> {
    let resolve = |host: &str| {
        address.ok_or_else(|| (LinkStatus::Down, format!("cannot resolve {host}"), None))
    };
    match kind.clone() {
        LinkKind::UdpListen { host, port } => {
            let address = resolve(&host)?;
            links::udp_listen(address, events)
                .await
                .map_err(|error| listen_error(Protocol::Udp, &host, port, &error))
        }
        LinkKind::UdpForward { host, port } => {
            let address = resolve(&host)?;
            links::udp_target(address, events)
                .await
                .map_err(|error| failed(LinkOperation::Socket, &error, &host, port))
        }
        LinkKind::TcpClient { host, port } => Ok(links::tcp_client(host, port, events)),
        // Opened with its decoder, in `open`.
        LinkKind::UsbReceiver {} => Err((
            LinkStatus::Down,
            "a USB receiver stick opens with its decoder".into(),
            None,
        )),
        LinkKind::TcpServer { host, port } => {
            let address = resolve(&host)?;
            links::tcp_server(address, events)
                .await
                .map_err(|error| listen_error(Protocol::Tcp, &host, port, &error))
        }
        LinkKind::UdpClient { host, port } => {
            let address = resolve(&host)?;
            links::udp_client(address, gcs, events)
                .await
                .map_err(|error| failed(LinkOperation::Socket, &error, &host, port))
        }
        LinkKind::Serial { device, baud, usb } => Ok(links::serial(device, baud, usb, events)),
        // Started by the simulation manager in `Plane::open`.
        LinkKind::Simulator { .. } => Err((
            LinkStatus::Down,
            "a simulated vehicle starts through the simulation manager".to_owned(),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Plane, not_pinged};
    use tokio::sync::mpsc;
    use uxduck_core::domain::link::Link;
    use uxduck_core::domain::telemetry::RoundTripMissing;
    use uxduck_mavlink::session::Config;

    /// FB-267: a UDP link whose host name can't be looked up reads at the
    /// host and port it was configured with, and keeps the system's words.
    #[test]
    fn a_udp_lookup_failure_names_the_configured_host() {
        use uxduck_core::domain::link_failure::{LinkFailureKind, LinkOperation};
        use uxduck_core::domain::transport::TransportError;
        let error = TransportError {
            at_us: 1,
            operation: "resolve".into(),
            kind: "Uncategorized".into(),
            os_code: None,
            endpoint: Some("review-test.invalid:14550".into()),
            message: "failed to lookup address information: Name or service not known".into(),
            active: true,
            failure: LinkFailureKind::NameNotFound,
        };
        let (detail, failure) =
            super::transport_failure(&error, Some(("review-test.invalid", 14550)));
        assert_eq!(
            detail,
            "UXDuck couldn't find review-test.invalid (port 14550). Check the name, or use its IP address."
        );
        assert_eq!(failure.operation, LinkOperation::Lookup);
        assert!(failure.message.contains("Name or service not known"));
    }

    /// FB-267: a UDP client whose own socket can't open reads plainly at
    /// its configured endpoint, not as the system's sentence.
    #[test]
    fn a_udp_socket_this_computer_refused_reads_plainly() {
        use uxduck_core::domain::link_failure::{LinkFailureKind, LinkOperation};
        use uxduck_core::domain::transport::TransportError;
        let error = TransportError {
            at_us: 1,
            operation: "open".into(),
            kind: "PermissionDenied".into(),
            os_code: Some(13),
            endpoint: Some("review-test.invalid:14550".into()),
            message: "Permission denied (os error 13)".into(),
            active: true,
            failure: LinkFailureKind::AccessDenied,
        };
        let (detail, failure) =
            super::transport_failure(&error, Some(("review-test.invalid", 14550)));
        assert_eq!(
            detail,
            "This computer didn't let UXDuck open a connection to review-test.invalid:14550. Check that a firewall or security software isn't blocking it."
        );
        assert_eq!(failure.operation, LinkOperation::Socket);
        assert_eq!(failure.message, "Permission denied (os error 13)");
    }

    fn plane(dir: &std::path::Path) -> (Plane, mpsc::UnboundedReceiver<Command>) {
        use crate::adapters::system::{OsRandom, SystemClock};
        use uxduck_engine::runtime::{Forget, Ports};
        use uxduck_engine::{Engine, EngineConfig, HostAbout, Loaded};
        let engine = Engine::new(
            EngineConfig {
                daemon_version: "0.1.0".into(),
                capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
                ui_url: "http://127.0.0.1:48091".into(),
                checkpoint_every: 1000,
                models: vec![],
                agent_provider: "fake".into(),
                about: HostAbout::default(),
            },
            Box::new(SystemClock),
            Box::new(OsRandom),
            Ports::with_persistence(Box::new(Forget)),
            Loaded::default(),
        );
        let (sender, commands) = mpsc::unbounded_channel();
        let plane = Plane::new(
            super::EngineHandle::spawn(engine),
            Config::default(),
            dir.to_owned(),
            None,
            crate::sim::Simulators::new(dir),
            sender,
        );
        (plane, commands)
    }

    fn listening_at(host: &str) -> Link {
        serde_json::from_value(serde_json::json!({
            "id": "li1",
            "name": "UDP",
            "kind": { "type": "udp-listen", "host": host, "port": 0 },
            "policy": "read-write",
            "topology": "auto",
            "status": "configured",
            "createdAtUs": 0,
            "version": 1,
        }))
        .unwrap()
    }

    /// FB: a host name whose lookup hangs (offline, or a `.local` name)
    /// held up every other link, since the data plane looked it up itself.
    #[tokio::test]
    async fn a_link_waiting_for_its_host_does_not_hold_up_the_data_plane() {
        let dir = tempfile::tempdir().unwrap();
        let (mut plane, mut commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(8);

        plane.open(listening_at("localhost"), &events).await;
        assert!(
            !plane.open.contains_key("li1"),
            "looked up off the data plane"
        );

        let Some(Command::Resolved {
            link,
            address,
            lookup,
        }) = commands.recv().await
        else {
            panic!("the lookup reports back");
        };
        assert!(address.is_ok(), "{address:?}");
        plane.resolved(link, address, &lookup, &events).await;
        assert!(
            plane.open.contains_key("li1"),
            "opens once its host is found"
        );
    }

    #[tokio::test]
    async fn a_link_closed_while_its_host_is_looked_up_stays_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut plane, mut commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(8);

        plane.open(listening_at("localhost"), &events).await;
        plane.close("li1").await;
        let Some(Command::Resolved {
            link,
            address,
            lookup,
        }) = commands.recv().await
        else {
            panic!("the lookup reports back");
        };
        plane.resolved(link, address, &lookup, &events).await;
        assert!(!plane.open.contains_key("li1"));
    }

    #[tokio::test]
    async fn an_address_opens_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut plane, _commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(8);

        plane.open(listening_at("127.0.0.1"), &events).await;
        assert!(plane.open.contains_key("li1"));
    }

    #[tokio::test]
    async fn registration_keeps_withholding_known_vehicles_across_silence_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (mut plane, _commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(8);
        plane
            .registration_vehicles
            .insert("li1".into(), ["buggy".into()].into());
        plane.withheld.insert("buggy".into());
        plane.refresh_registration();
        assert!(plane.registration_withheld("li1"));
        plane.open(listening_at("127.0.0.1"), &events).await;
        plane.close("li1").await;
        assert!(plane.registration_withheld("li1"));
        plane.open(listening_at("127.0.0.1"), &events).await;
        plane.refresh_registration();
        assert!(plane.registration_withheld("li1"));
        plane
            .registration_vehicles
            .get_mut("li1")
            .unwrap()
            .insert("scout".into());
        assert!(
            !plane.registration_withheld("li1"),
            "another supervised vehicle shares the peer"
        );
        plane.withheld.insert("scout".into());
        assert!(plane.registration_withheld("li1"));
    }

    /// S261's round trip is timed from when the reply arrived, not from
    /// when a busy plane got to it, as S356's computer clocks are: a
    /// debug daemon with a window open read replies 13 to 54 ms late, so a
    /// vehicle on this computer read 6 to 81 ms.
    #[tokio::test]
    async fn a_round_trip_is_timed_from_when_its_reply_arrived() {
        use crate::adapters::system::SystemClock;
        use std::sync::atomic::Ordering;
        use uxduck_engine::runtime::Clock;
        use uxduck_mavlink::sim::{heartbeat_frame, timesync_frame};

        let dir = tempfile::tempdir().unwrap();
        let (mut plane, _commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(8);
        plane.open(listening_at("127.0.0.1"), &events).await;
        let peer = "127.0.0.1:14550";
        plane
            .frame(
                "li1",
                peer,
                &heartbeat_frame(1, 1, 0),
                false,
                SystemClock.now_us(),
            )
            .await;
        let address = ("li1".to_owned(), peer.to_owned(), 1);
        let ts1 = plane.health.asked(&address, plane.now_mono_us());
        let gcs = plane.gcs.load(Ordering::Relaxed);
        let reply = timesync_frame((1, 1, 1), (1_000_000, ts1), (gcs, 0));
        let arrived = SystemClock.now_us();
        // The plane busy elsewhere while the reply waits.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        plane.frame("li1", peer, &reply, false, arrived).await;

        let health = plane.health.report(&address, plane.now_mono_us());
        let round_trip = health.round_trip_ms.expect("a round trip");
        assert!(
            round_trip < 20.0,
            "timed from its arrival, not 60 ms later: {round_trip} ms"
        );
    }

    fn connection(id: &str, kind: &str, port: u16) -> Link {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "kind": { "type": kind, "host": "127.0.0.1", "port": port },
            "policy": "read-write",
            "topology": "auto",
            "status": "configured",
            "createdAtUs": 0,
            "version": 1,
        }))
        .unwrap()
    }

    /// Buggy over its radio and over the internet, with QGC on a forward.
    /// Buggy's router sends every frame down both connections, so each
    /// frame arrives twice. The second copy is Buggy, not a forwarding
    /// loop: the forward stays on and the internet becomes Buggy's too.
    #[tokio::test]
    async fn a_second_connection_with_a_forward_on_is_not_a_loop() {
        use crate::adapters::system::SystemClock;
        use uxduck_engine::runtime::Clock;
        use uxduck_mavlink::sim::{autopilot_version_frame, heartbeat_frame};

        let dir = tempfile::tempdir().unwrap();
        let (mut plane, _commands) = plane(dir.path());
        let (events, _received) = mpsc::channel(64);
        let servers: Vec<std::net::UdpSocket> = (0..3)
            .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
            .collect();
        let port = |i: usize| servers[i].local_addr().unwrap().port();
        plane
            .open(connection("radio", "udp-client", port(0)), &events)
            .await;
        plane
            .open(connection("internet", "udp-client", port(1)), &events)
            .await;
        plane
            .open(connection("qgc", "udp-forward", port(2)), &events)
            .await;
        let radio = format!("127.0.0.1:{}", port(0));
        let internet = format!("127.0.0.1:{}", port(1));
        let mut frames = vec![
            heartbeat_frame(1, 1, 0),
            autopilot_version_frame(1, 1, 1, 0xB0661, 0x0406_0000),
        ];
        frames.extend((2..12).map(|sequence| heartbeat_frame(1, 1, sequence)));
        for frame in &frames {
            plane
                .frame("radio", &radio, frame, false, SystemClock.now_us())
                .await;
            plane
                .frame("internet", &internet, frame, false, SystemClock.now_us())
                .await;
        }

        assert!(plane.open.contains_key("qgc"), "the forward is still on");
        let buggy = plane
            .session
            .vehicle_at("radio", &radio, 1)
            .map(str::to_owned);
        assert!(buggy.is_some());
        assert_eq!(
            plane
                .session
                .vehicle_at("internet", &internet, 1)
                .map(str::to_owned),
            buggy,
            "the copies are Buggy's second connection"
        );
    }

    #[test]
    fn the_ping_goes_only_where_the_ground_heartbeat_does() {
        let writable = Some((true, false));
        assert_eq!(not_pinged(writable, false, false), None);
        assert_eq!(
            not_pinged(Some((false, false)), false, false),
            Some(RoundTripMissing::ReadOnly)
        );
        for forward in [Some((true, true)), Some((false, true))] {
            assert_eq!(
                not_pinged(forward, false, false),
                Some(RoundTripMissing::NotSent)
            );
        }
        assert_eq!(
            not_pinged(None, false, false),
            Some(RoundTripMissing::NotSent)
        );
        assert_eq!(
            not_pinged(writable, true, false),
            Some(RoundTripMissing::Unsupervised),
            "withheld (S81): no ping in its place"
        );
        assert_eq!(
            not_pinged(writable, true, true),
            Some(RoundTripMissing::Unsupervised)
        );
        assert_eq!(
            not_pinged(writable, false, true),
            Some(RoundTripMissing::Paused)
        );
        assert_eq!(
            not_pinged(Some((false, false)), true, true),
            Some(RoundTripMissing::ReadOnly)
        );
    }
}

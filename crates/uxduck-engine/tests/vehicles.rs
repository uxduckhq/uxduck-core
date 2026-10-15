//! Links and vehicles: add and remove links, merge observations, aliases, scope.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{DaemonCapabilities, RunsIn};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::UsbId;
use uxduck_core::domain::link::{Link, LinkPolicy, LinkStatus};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    AgentPort, Clock, Forget, LinkPort, MemoryCredentials, Ports, Random, SerialPortInfo, System,
    TurnRequest,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

thread_local! {
    /// The time on this test's thread: 1000 µs until a test moves it.
    static NOW: std::cell::Cell<u64> = const { std::cell::Cell::new(1_000) };
}

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        NOW.with(std::cell::Cell::get)
    }
}

/// Moves this test's clock on.
fn advance(us: u64) {
    NOW.with(|now| now.set(now.get() + us));
}

/// A 64-bit generator, so ids do not repeat within a test.
struct Counter(u64);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *byte = (self.0 >> 56) as u8;
        }
    }
}

#[derive(Clone, Default)]
struct Turns(Arc<Mutex<Vec<String>>>);

impl AgentPort for Turns {
    fn start_turn(&mut self, turn: TurnRequest) {
        self.0.lock().unwrap().push(format!(
            "start {} {} {}",
            turn.turn, turn.thread_id, turn.text
        ));
    }
    fn cancel_turn(&mut self, thread_id: &str) {
        self.0.lock().unwrap().push(format!("cancel {thread_id}"));
    }
}

/// The host as tests see it: busy ports, the wake lock, serial ports.
#[derive(Clone, Default)]
struct Host(Arc<Mutex<HostState>>);

#[derive(Default)]
struct HostState {
    heartbeats: std::collections::BTreeMap<String, u64>,
    busy: std::collections::BTreeMap<u16, String>,
    wake_lock: Vec<bool>,
    /// Start at login turned on and off (S64).
    logins: Vec<bool>,
    /// macOS waits for the person's OK before a login item runs (S108).
    login_needs_approval: bool,
    /// The desktop app's executable once downloaded, and its starts (S110).
    desktop_app: Option<String>,
    desktop_app_opens: Vec<Vec<String>>,
    desktop_app_broken: bool,
}

impl System for Host {
    fn vehicle_heartbeat_us(&self, vehicle_ids: &[String]) -> Option<u64> {
        let state = self.0.lock().unwrap();
        vehicle_ids
            .iter()
            .filter_map(|id| state.heartbeats.get(id).copied())
            .max()
    }
    fn check_udp_bind(&self, _: &str, port: u16) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .busy
            .get(&port)
            .cloned()
            .map_or(Ok(()), Err)
    }
    fn is_local_address(&self, host: &str) -> bool {
        matches!(host, "127.0.0.1" | "0.0.0.0" | "localhost" | "192.168.1.5")
    }
    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        vec![SerialPortInfo {
            device: "/dev/ttyACM0".into(),
            usb: Some(UsbId {
                vendor_id: 0x1209,
                product_id: 0x5741,
                serial_number: Some("ABC123".into()),
            }),
            description: Some("ArduPilot CubeOrange".into()),
        }]
    }
    fn wake_lock(&mut self, hold: bool) {
        self.0.lock().unwrap().wake_lock.push(hold);
    }
    fn desktop_app_path(&mut self) -> Result<String, String> {
        self.0
            .lock()
            .unwrap()
            .desktop_app
            .clone()
            .ok_or_else(|| "Download the desktop app first.".into())
    }
    fn desktop_app_open(&mut self, path: &str, args: &[String]) -> Result<(), String> {
        let mut host = self.0.lock().unwrap();
        if host.desktop_app_broken {
            return Err("It is not signed by UXDuck's developer.".into());
        }
        assert_eq!(host.desktop_app.as_deref(), Some(path));
        host.desktop_app_opens.push(args.to_vec());
        Ok(())
    }
    fn desktop_app_remove(&mut self) -> Result<(), String> {
        self.0.lock().unwrap().desktop_app = None;
        Ok(())
    }
    fn start_at_login(
        &mut self,
        on: bool,
    ) -> Result<uxduck_contracts::dto::settings::LoginItem, String> {
        use uxduck_contracts::dto::settings::{LoginItem, LoginItemState};
        let mut host = self.0.lock().unwrap();
        host.logins.push(on);
        let state = match (on, host.login_needs_approval) {
            (true, true) => LoginItemState::NeedsApproval,
            (true, false) => LoginItemState::On,
            (false, _) => LoginItemState::Off,
        };
        Ok(LoginItem {
            state,
            detail: None,
        })
    }
}

fn engine(turns: &Turns, provider: &str, state: State) -> Engine {
    engine_with(turns, provider, state, &Host::default())
}

fn engine_with(turns: &Turns, provider: &str, state: State, host: &Host) -> Engine {
    engine_running(turns, provider, state, host, DaemonCapabilities::default())
}

/// An engine whose asks of the data plane (`open`, `update`, `close`) are
/// recorded in the returned `Opened`.
fn engine_watching_links(turns: &Turns, state: State) -> (Engine, Opened) {
    let opened = Opened::default();
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(turns.clone()),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(opened.clone()),
        system: Box::new(Host::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let engine = Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    );
    (engine, opened)
}

/// An engine that runs simulated vehicles, its asks of the data plane
/// recorded as `engine_watching_links` does.
fn engine_simulating(turns: &Turns) -> (Engine, Opened) {
    let opened = Opened::default();
    (
        engine_simulating_with(turns, Box::new(opened.clone())),
        opened,
    )
}

/// An engine that runs simulated vehicles through the given data plane.
fn engine_simulating_with(turns: &Turns, links: Box<dyn LinkPort>) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities {
            simulator: true,
            ..DaemonCapabilities::default()
        },
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(turns.clone()),
        credentials: Box::new(MemoryCredentials::default()),
        links,
        system: Box::new(Host::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: State::default(),
            sessions: vec![],
        },
    )
}

fn engine_running(
    turns: &Turns,
    provider: &str,
    state: State,
    host: &Host,
    capabilities: DaemonCapabilities,
) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities,
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into(), "model-b".into()],
        agent_provider: provider.into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(turns.clone()),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(Opened::default()),
        system: Box::new(host.clone()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    )
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut client = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = call(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": kind, "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    loop {
        let frame: Value =
            serde_json::from_str(&client.inbox.try_recv().expect("a reply")).unwrap();
        if frame["id"] == id {
            return frame;
        }
        client.queued.push_back(frame);
    }
}

fn notifications(client: &mut Client, method: &str) -> Vec<Value> {
    let mut frames: Vec<Value> = client.queued.drain(..).collect();
    frames.extend(
        std::iter::from_fn(|| client.inbox.try_recv().ok())
            .map(|t| serde_json::from_str::<Value>(&t).unwrap()),
    );
    frames
        .into_iter()
        .filter(|f| f["method"] == method)
        .map(|f| f["params"].clone())
        .collect()
}

/// Records what the engine asked the data plane to open and close.
#[derive(Clone, Default)]
struct Opened(Arc<Mutex<Vec<String>>>);

impl LinkPort for Opened {
    fn open(&mut self, link: &Link) {
        self.0.lock().unwrap().push(format!("open {}", link.name));
    }
    fn close(&mut self, link_id: &str) {
        self.0.lock().unwrap().push(format!("close {link_id}"));
    }
    fn update(&mut self, link: &Link) {
        self.0.lock().unwrap().push(format!("update {}", link.name));
    }
    fn command(&mut self, order: uxduck_engine::runtime::CommandOrder) {
        self.0
            .lock()
            .unwrap()
            .push(format!("command {} {:?}", order.receipt_id, order.command));
    }
}

fn call_ok(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    let reply = call(engine, client, method, params);
    assert!(reply.get("result").is_some(), "{method}: {reply}");
    reply["result"].clone()
}

fn seen(id: &str, aliases: &[&str], link: &str, presence: Presence) -> Observation {
    Observation {
        id: id.into(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        link_id: link.into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    }
}

#[test]
fn vehicles_sharing_a_system_id_get_callsigns_and_a_reset_takes_a_free_one() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap();
    // Both say system id 1, from two addresses.
    engine.vehicle_observed(seen("one", &[], link_id, Presence::Online));
    let mut other = seen("two", &[], link_id, Presence::Online);
    other.peer = "127.0.0.1:5770".into();
    engine.vehicle_observed(other);
    assert_eq!(engine.state().vehicles["one"].name, "Alpha");
    assert_eq!(engine.state().vehicles["two"].name, "Bravo");

    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "one", "name": "Survey quad" }),
    );
    let reset = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "two", "name": "" }),
    );
    assert_eq!(
        reset["name"], "Alpha",
        "the first callsign nobody else goes by"
    );
}

#[test]
fn a_link_is_added_opened_and_reports_its_status() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    assert_eq!(
        (
            link["name"].as_str(),
            link["status"].as_str(),
            link["topology"].as_str()
        ),
        (Some("UDP 14550"), Some("configured"), Some("auto"))
    );
    let id = link["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "link/add",
            json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } })
        )["error"]["data"]["kind"],
        "invalid-value"
    );
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "link/add",
            json!({ "kind": { "type": "udp-listen", "host": "nope", "port": 1 } })
        )["error"]["data"]["kind"],
        "invalid-value"
    );

    engine.link_status(
        &id,
        LinkStatus::PortInUse,
        Some("Address already in use".into()),
    );
    assert_eq!(engine.state().links[&id].status, LinkStatus::PortInUse);
    // An agent adding a link waits for an operator (see the approval tests).
    let agent = connect(&mut engine, "agent");
    engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "id": 99, "method": "link/add",
                 "params": { "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 1 } } })
        .to_string(),
    );
    assert_eq!(engine.state().approvals.len(), 1);
}

#[test]
fn observations_become_vehicles_and_a_uid_replaces_the_provisional_id() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap();

    engine.vehicle_observed(seen("prov", &[], link_id, Presence::Online));
    let listed = call_ok(&mut engine, &mut ui, "vehicle/list", json!({}));
    assert_eq!(listed["vehicles"][0]["name"], "Alpha");
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "prov", "name": "Scout" }),
    );
    let seq = engine.state().seq;
    engine.vehicle_observed(seen("prov", &[], link_id, Presence::Online));
    assert_eq!(
        engine.state().seq,
        seq,
        "an unchanged observation writes nothing"
    );

    let mut identified = seen("vid-1", &["prov"], link_id, Presence::Online);
    identified.uid = Some("0000000000001234".into());
    engine.vehicle_observed(identified);
    assert!(!engine.state().vehicles.contains_key("prov"));
    let vehicle = &engine.state().vehicles["vid-1"];
    assert_eq!(
        (
            vehicle.name.as_str(),
            vehicle.saved,
            vehicle.aliases.as_slice()
        ),
        ("Scout", true, ["prov".to_owned()].as_slice())
    );
    assert_eq!(
        call_ok(&mut engine, &mut ui, "vehicle/get", json!({ "id": "prov" }))["id"],
        "vid-1",
        "the old id still finds it"
    );

    let thread = call_ok(
        &mut engine,
        &mut ui,
        "thread/create",
        json!({ "vehicleId": "prov" }),
    );
    assert_eq!(thread["vehicleId"], "vid-1");
    call_ok(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread["id"], "text": "describe my vehicle" }),
    );
    assert!(
        turns
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("describe my vehicle")
    );

    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/dismiss",
        json!({ "id": "vid-1" }),
    );
    assert_eq!(
        call_ok(&mut engine, &mut ui, "vehicle/list", json!({}))["vehicles"],
        json!([])
    );
}

#[test]
fn deleting_a_vehicle_forgets_it_until_it_is_heard_again_but_not_while_heard() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap();
    engine.vehicle_observed(seen("old", &[], link_id, Presence::Online));
    call_ok(&mut engine, &mut ui, "vehicle/save", json!({ "id": "old" }));
    let refused = call(
        &mut engine,
        &mut ui,
        "vehicle/delete",
        json!({ "id": "old" }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "busy", "{refused}");
    engine.vehicle_observed(seen("old", &[], link_id, Presence::Offline));
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/delete",
        json!({ "id": "old" }),
    );
    assert!(
        !engine.state().vehicles.contains_key("old"),
        "saved or not, it goes"
    );
    assert_eq!(
        call_ok(&mut engine, &mut ui, "vehicle/list", json!({}))["vehicles"],
        json!([])
    );
    // Heard again, it is a vehicle like any new one.
    engine.vehicle_observed(seen("old", &[], link_id, Presence::Online));
    assert!(!engine.state().vehicles["old"].saved);
}

#[test]
fn removing_a_link_remembers_its_vehicles_saved_or_not() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap();
    engine.vehicle_observed(seen("kept", &[], link_id, Presence::Online));
    let mut other = seen("gone", &[], link_id, Presence::Online);
    other.system_id = 2;
    engine.vehicle_observed(other);
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "kept" }),
    );
    call_ok(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({ "id": link_id }),
    );
    assert!(engine.state().links.is_empty());
    let mut ids: Vec<_> = engine.state().vehicles.keys().collect();
    ids.sort();
    assert_eq!(
        ids,
        ["gone", "kept"],
        "names and settings come back next time"
    );
    assert!(
        engine
            .state()
            .vehicles
            .values()
            .all(|v| v.presence == Presence::Offline)
    );
}

#[test]
fn a_restart_opens_every_link_and_starts_every_vehicle_offline() {
    let turns = Turns::default();
    let mut first = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut first, "operator-ui");
    let link = call_ok(
        &mut first,
        &mut ui,
        "link/add",
        json!({ "name": "SITL", "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": 14550 } }),
    );
    first.vehicle_observed(seen(
        "v",
        &[],
        link["id"].as_str().unwrap(),
        Presence::Online,
    ));
    call_ok(&mut first, &mut ui, "vehicle/dismiss", json!({ "id": "v" }));
    let state = first.state().clone();

    let opened = Opened::default();
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(turns.clone()),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(opened.clone()),
        system: Box::new(Host::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let second = Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(9)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    );
    assert_eq!(opened.0.lock().unwrap().as_slice(), ["open SITL"]);
    assert_eq!(second.state().vehicles["v"].presence, Presence::Offline);
    assert!(
        !second.state().vehicles["v"].dismissed,
        "a dismissal lasts one run"
    );
}

#[test]
fn presence_changes_reach_subscribers_as_deltas() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    call_ok(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["vehicle"] }),
    );
    engine.vehicle_observed(seen("v", &[], "lk", Presence::Online));
    engine.vehicle_observed(seen("v", &[], "lk", Presence::Stale));
    let presences: Vec<Value> = notifications(&mut ui, "state/delta")
        .iter()
        .flat_map(|delta| delta["changes"].as_array().cloned().unwrap_or_default())
        .map(|change| change["vehicle"]["presence"].clone())
        .collect();
    assert_eq!(presences, [json!("online"), json!("stale")]);
}

fn telemetry(id: &str, voltage: f64) -> uxduck_core::domain::telemetry::VehicleTelemetry {
    use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
    VehicleTelemetry {
        vehicle_id: id.into(),
        positions: vec![],
        items: vec![TelemetryItem {
            key: "battery.voltage_v".into(),
            label: "Battery".into(),
            unit: Unit::Volt,
            value: Some(voltage),
            text: None,
            source: "power".into(),
            component_id: 1,
            time_us: 1,
        }],
    }
}

#[test]
fn telemetry_is_live_not_journaled_and_follows_the_vehicle_id() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    engine.vehicle_observed(seen("prov", &[], "lk", Presence::Online));
    let seq = engine.state().seq;
    engine.telemetry_observed(telemetry("prov", 12.4));
    assert_eq!(engine.state().seq, seq, "telemetry is not journaled");

    let subscribed = call_ok(&mut engine, &mut ui, "telemetry/subscribe", json!({}));
    assert_eq!(
        subscribed["vehicles"][0]["items"][0]["value"], 12.4,
        "a subscriber starts from the latest"
    );
    engine.telemetry_observed(telemetry("prov", 12.3));
    let updates = notifications(&mut ui, "telemetry/update");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["items"][0]["value"], 12.3);

    // The UID arrives: the old id's telemetry goes, and the old id still
    // finds the new record's.
    engine.vehicle_observed(seen("vid", &["prov"], "lk", Presence::Online));
    engine.telemetry_observed(telemetry("vid", 12.2));
    let got = call_ok(
        &mut engine,
        &mut ui,
        "telemetry/get",
        json!({ "id": "prov" }),
    );
    assert_eq!(
        (got["vehicleId"].as_str(), got["items"][0]["value"].as_f64()),
        (Some("vid"), Some(12.2))
    );
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "telemetry/get",
            json!({ "id": "nope" })
        )["error"]["data"]["kind"],
        "not-found"
    );
}

#[test]
fn vehicle_reads_use_live_heartbeats_without_persisting_each_beat() {
    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut ui = connect(&mut engine, "operator-ui");
    engine.vehicle_observed(seen("v", &["provisional"], "lk", Presence::Online));
    let stored = engine.state().vehicles["v"].clone();
    let seq = engine.state().seq;
    NOW.with(|now| now.set(2_500_000));
    host.0
        .lock()
        .unwrap()
        .heartbeats
        .insert("provisional".into(), 2_000_000);

    let got = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/get",
        json!({ "id": "provisional" }),
    );
    assert_eq!(got["lastHeartbeatUs"], 2_000_000);
    assert_eq!(got["heartbeatAgeMs"], 500);
    let list = call_ok(&mut engine, &mut ui, "vehicle/list", json!({}));
    assert_eq!(list["vehicles"][0]["lastHeartbeatUs"], 2_000_000);
    assert_eq!(list["vehicles"][0]["heartbeatAgeMs"], 500);
    assert_eq!(engine.state().seq, seq);
    assert_eq!(engine.state().vehicles["v"], stored);

    advance(5_000_000);
    let got = call_ok(&mut engine, &mut ui, "vehicle/get", json!({ "id": "v" }));
    assert_eq!(got["heartbeatAgeMs"], 5_500, "silent links grow older");
    host.0.lock().unwrap().heartbeats.clear();
    let got = call_ok(&mut engine, &mut ui, "vehicle/get", json!({ "id": "v" }));
    assert!(
        got.get("heartbeatAgeMs").is_none(),
        "no live source means unknown age"
    );
    assert_eq!(got["lastHeartbeatUs"], stored.last_heartbeat_us);
}

#[test]
fn link_inventory_is_read_only_and_keeps_failures_and_disabled_links() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({
            "kind": { "type": "tcp-client", "host": "127.0.0.1", "port": 5760 }
        }),
    );
    let id = link["id"].as_str().unwrap();
    engine.link_status(id, LinkStatus::Down, Some("connection refused".into()));
    let mut agent = connect(&mut engine, "agent");
    let seq = engine.state().seq;
    let list = call_ok(&mut engine, &mut agent, "link/list", json!({}));
    assert_eq!(list["links"][0]["status"], "down");
    assert_eq!(list["links"][0]["statusDetail"], "connection refused");
    assert_eq!(engine.state().seq, seq);
    call_ok(&mut engine, &mut ui, "link/disable", json!({ "id": id }));
    let list = call_ok(&mut engine, &mut agent, "link/list", json!({}));
    assert_eq!(list["links"][0]["status"], "disabled");
    assert!(
        engine.state().approvals.is_empty(),
        "diagnostics need no approval"
    );
}

#[test]
fn recent_telemetry_rpc_preserves_history_through_aliases_and_requires_read() {
    NOW.with(|now| now.set(1_000_000));
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    engine.vehicle_observed(seen("prov", &[], "lk", Presence::Online));
    let mut sample = telemetry("prov", 12.0);
    sample.items[0].time_us = 1_000_000;
    let seq = engine.state().seq;
    engine.telemetry_observed(sample);
    assert_eq!(engine.state().seq, seq);
    engine.vehicle_observed(seen("uid", &["prov"], "lk", Presence::Online));
    advance(100_000);
    let mut sample = telemetry("uid", 14.0);
    sample.items[0].time_us = 1_100_000;
    engine.telemetry_observed(sample);
    let params = json!({ "vehicleId": "prov", "keys": ["battery.voltage_v"], "durationMs": 1000 });
    let result = call_ok(&mut engine, &mut ui, "telemetry/window", params.clone());
    assert_eq!(result["vehicleId"], "uid");
    assert_eq!(result["items"][0]["coverage"]["count"], 2);
    assert_eq!(result["items"][0]["numeric"]["max"], 14.0);
    assert_eq!(result["items"][0]["numeric"]["min"], 12.0);
    assert_eq!(
        result["historyTruncated"], true,
        "this window extends before daemon startup"
    );
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "telemetry/window",
            json!({ "vehicleId": "uid", "keys": [] })
        )["error"]["data"]["kind"],
        "invalid-value"
    );

    let mut agent = connect(&mut engine, "agent");
    let clients = call_ok(&mut engine, &mut ui, "client/list", json!({}));
    let row = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client"]["kind"] == "agent")
        .unwrap()
        .clone();
    call_ok(
        &mut engine,
        &mut ui,
        "client/setCapabilities",
        json!({ "id": row["client"]["id"], "expectedVersion": row["client"]["version"], "capabilities": [] }),
    );
    let denied = call(&mut engine, &mut agent, "telemetry/window", params.clone());
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    engine.vehicle_observed(seen("uid", &["prov"], "lk", Presence::Offline));
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/delete",
        json!({ "id": "uid" }),
    );
    let deleted = call(&mut engine, &mut ui, "telemetry/window", params);
    assert_eq!(deleted["error"]["data"]["kind"], "not-found");
}

fn add(engine: &mut Engine, ui: &mut Client, params: Value) -> Value {
    call(engine, ui, "link/add", params)
}

#[test]
fn each_kind_of_link_is_checked_before_it_is_created() {
    let turns = Turns::default();
    let host = Host::default();
    host.0.lock().unwrap().busy.insert(
        14550,
        "UDP port 14550 is in use by MissionPlanner (pid 812)".into(),
    );
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut ui = connect(&mut engine, "operator-ui");

    let busy = add(
        &mut engine,
        &mut ui,
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    assert_eq!(busy["error"]["data"]["kind"], "port-in-use");
    assert!(
        busy["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MissionPlanner (pid 812)")
    );
    assert!(engine.state().links.is_empty(), "nothing is created");

    let listener = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14551 } }),
    );
    // Forwarding into our own listener, on any of our addresses, is refused.
    for host in ["127.0.0.1", "192.168.1.5"] {
        let refused = add(
            &mut engine,
            &mut ui,
            json!({ "kind": { "type": "udp-forward", "host": host, "port": 14551 } }),
        );
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value", "{host}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("own listeners")
        );
    }
    // To another machine's port of the same number is fine.
    let forward = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({
            "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14551 },
            "policy": "read",
            "vehicleFilter": ["vid-a", " "],
            "keepAlive": true
        }),
    );
    assert_eq!(
        (
            forward["name"].as_str(),
            forward["vehicleFilter"].clone(),
            forward["keepAlive"].as_bool()
        ),
        (
            Some("Forward to 10.1.1.9:14551"),
            json!(["vid-a"]),
            Some(true)
        )
    );
    assert_eq!(listener["name"], "UDP 14551");

    let serial = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": {
            "type": "serial", "device": "/dev/ttyACM0", "baud": 115_200,
            "usb": { "vendorId": 4617, "productId": 22337, "serialNumber": "ABC123" }
        } }),
    );
    assert_eq!(serial["name"], "Serial ttyACM0");
    let same_usb = add(
        &mut engine,
        &mut ui,
        json!({ "kind": {
            "type": "serial", "device": "/dev/ttyACM1", "baud": 57600,
            "usb": { "vendorId": 4617, "productId": 22337, "serialNumber": "ABC123" }
        } }),
    );
    assert_eq!(
        same_usb["error"]["data"]["kind"], "invalid-value",
        "same adapter"
    );
    let tcp = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "tcp-client", "host": "sitl.local", "port": 5760 } }),
    );
    assert_eq!(tcp["name"], "TCP sitl.local:5760");

    // A TCP listener, once per port.
    let tcp_server = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "tcp-server", "host": "0.0.0.0", "port": 15010 } }),
    );
    assert_eq!(tcp_server["name"], "TCP 15010");
    let again = add(
        &mut engine,
        &mut ui,
        json!({ "kind": { "type": "tcp-server", "host": "127.0.0.1", "port": 15010 } }),
    );
    assert_eq!(again["error"]["data"]["kind"], "invalid-value");

    // A UDP client: by name, never into our own listener, once per server.
    let udp_client = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-client", "host": "192.168.4.1", "port": 14550 } }),
    );
    assert_eq!(udp_client["name"], "UDP 192.168.4.1:14550");
    for (host, port) in [("192.168.4.1", 14550), ("127.0.0.1", 14551)] {
        let refused = add(
            &mut engine,
            &mut ui,
            json!({ "kind": { "type": "udp-client", "host": host, "port": port } }),
        );
        assert_eq!(
            refused["error"]["data"]["kind"], "invalid-value",
            "{host}:{port}"
        );
    }

    let ports = call_ok(&mut engine, &mut ui, "serial/list", json!({}));
    assert_eq!(ports["ports"][0]["usb"]["serialNumber"], "ABC123");
}

#[test]
fn a_simulated_vehicle_starts_only_where_the_daemon_runs_them_and_one_at_a_time() {
    let turns = Turns::default();
    let place = json!({
        "type": "simulator",
        "vehicle": "multicopter",
        "latitudeDeg": -35.363_261,
        "longitudeDeg": 149.165_23,
        "altitudeM": 584.0,
        "headingDeg": 353.0
    });

    let mut installed = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut installed, "operator-ui");
    let refused = add(&mut installed, &mut ui, json!({ "kind": place }));
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
    assert!(installed.state().links.is_empty());

    let browser = DaemonCapabilities {
        runs_in: RunsIn::Browser,
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(&turns, "fake", State::default(), &Host::default(), browser);
    let mut ui = connect(&mut engine, "operator-ui");
    let mut nowhere = place.clone();
    nowhere["latitudeDeg"] = json!(123.0);
    let refused = add(&mut engine, &mut ui, json!({ "kind": nowhere }));
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");

    let link = call_ok(&mut engine, &mut ui, "link/add", json!({ "kind": place }));
    assert_eq!(link["name"], "Simulated multicopter");
    assert_eq!(link["kind"]["vehicle"], "multicopter");
    assert_eq!(link["kind"]["instance"], 0);
    // Two run in a browser, each its own instance (and system id); not three.
    let second = call_ok(&mut engine, &mut ui, "link/add", json!({ "kind": place }));
    assert_eq!(second["kind"]["instance"], 1);
    let third = add(&mut engine, &mut ui, json!({ "kind": place }));
    assert!(
        third["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already running")
    );
}

#[test]
fn loop_detection_disables_a_forward_until_it_is_enabled_again() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let forward = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14550 } }),
    );
    let id = forward["id"].as_str().unwrap();
    let radio = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({
            "name": "Field radio", "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": 15010 }
        }),
    );
    engine.link_disabled_by_loop(id, radio["id"].as_str().unwrap(), "10.1.1.9:14550");
    let link = &engine.state().links[id];
    let detail = link.status_detail.as_deref().unwrap();
    assert!(detail.contains("10.1.1.9:14550"));
    assert!(detail.contains("Field radio"));
    assert!(detail.contains("forwarding settings"));
    assert!(!detail.contains(radio["id"].as_str().unwrap()));
    assert_eq!(link.status, LinkStatus::DisabledByLoopDetection);
    engine.link_status(id, LinkStatus::Up, None);
    assert_eq!(
        engine.state().links[id].status,
        LinkStatus::DisabledByLoopDetection,
        "a late report does not turn it on"
    );
    let enabled = call_ok(&mut engine, &mut ui, "link/enable", json!({ "id": id }));
    assert_eq!(enabled["status"], "configured");
}

#[test]
fn a_link_is_changed_in_place_and_reopens_only_when_it_moves() {
    let turns = Turns::default();
    let (mut engine, opened) = engine_watching_links(&turns, State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15010 } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15012 } }),
    );
    engine.link_status(&id, LinkStatus::Up, None);
    engine.vehicle_observed(seen("v", &[], &id, Presence::Online));
    opened.0.lock().unwrap().clear();

    // A name and keep-alive are saved; nothing is asked of the data plane.
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "name": "  Field radio ", "keepAlive": true }),
    );
    assert_eq!(renamed["name"], "Field radio");
    assert_eq!(renamed["keepAlive"], true);
    assert_eq!(renamed["status"], "up");
    assert_eq!(renamed["version"], 3, "after add and up");
    assert!(opened.0.lock().unwrap().is_empty(), "{:?}", opened.0);

    // A policy changes the open link's role; its vehicle stays heard.
    let read_only = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "policy": "read" }),
    );
    assert_eq!(read_only["policy"], "read");
    assert_eq!(read_only["status"], "up");
    assert_eq!(opened.0.lock().unwrap().as_slice(), ["update Field radio"]);
    assert_eq!(engine.state().vehicles["v"].presence, Presence::Online);
    opened.0.lock().unwrap().clear();

    // Nothing changed: nothing happens.
    let same = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "policy": "read" }),
    );
    assert_eq!(same["version"], read_only["version"]);
    assert!(opened.0.lock().unwrap().is_empty());

    // Another link's port is refused, as it is for a new link.
    let clash = call(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15012 } }),
    );
    assert_eq!(clash["error"]["data"]["kind"], "invalid-value");
    assert!(
        clash["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already listens on port 15012")
    );
    // Its own port is not a clash with itself.
    let kept = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15010 } }),
    );
    assert_eq!(kept["version"], read_only["version"]);

    // Moving it reopens it, starting its status over; its vehicle is
    // offline until heard on the new port, and keeps its name and id.
    let moved = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15011 }, "name": "" }),
    );
    assert_eq!(moved["kind"]["port"], 15011);
    assert_eq!(moved["name"], "UDP 15011", "an empty name is the kind's");
    assert_eq!(moved["status"], "configured");
    assert_eq!(opened.0.lock().unwrap().as_slice(), ["update UDP 15011"]);
    assert_eq!(engine.state().vehicles["v"].presence, Presence::Offline);
    assert_eq!(engine.state().vehicles["v"].link_id, id);
    engine.link_status(&id, LinkStatus::Up, None);
    assert_eq!(engine.state().links[&id].status, LinkStatus::Up);

    // Becoming a forward takes a filter and drops the topology's meaning.
    let forward = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({
            "id": id,
            "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14550 },
            "vehicleFilter": ["v", ""],
        }),
    );
    assert_eq!(forward["name"], "Forward to 10.1.1.9:14550");
    assert_eq!(forward["vehicleFilter"], json!(["v"]));

    let gone = call(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": "nope", "name": "x" }),
    );
    assert_eq!(gone["error"]["data"]["kind"], "not-found");
}

#[test]
fn a_vehicle_heard_again_from_a_new_address_is_one_record() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15010 } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    call_ok(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["vehicle"] }),
    );
    // First contact: a provisional id, then its UID gives its lasting one.
    engine.vehicle_observed(seen("prov-1", &[], &id, Presence::Online));
    engine.vehicle_observed(seen("vid", &["prov-1"], &id, Presence::Online));
    assert_eq!(engine.state().vehicles.len(), 1);
    assert_eq!(engine.state().vehicles["vid"].aliases, ["prov-1"]);

    // The link moved (or the vehicle rebooted with a new port): it is heard
    // from a new address under a new provisional id, then identified.
    let mut again = seen("prov-2", &[], &id, Presence::Online);
    again.peer = "127.0.0.1:5761".into();
    engine.vehicle_observed(again.clone());
    assert_eq!(
        engine.state().vehicles.len(),
        2,
        "two until its UID arrives"
    );
    again.id = "vid".into();
    again.aliases = vec!["prov-2".into()];
    engine.vehicle_observed(again);
    let vehicles = engine.state().vehicles.clone();
    assert_eq!(vehicles.len(), 1, "{vehicles:?}");
    assert_eq!(vehicles["vid"].peer, "127.0.0.1:5761");
    assert_eq!(vehicles["vid"].aliases, ["prov-1", "prov-2"]);
    let removed: Vec<Value> = notifications(&mut ui, "state/delta")
        .iter()
        .flat_map(|delta| delta["changes"].as_array().cloned().unwrap_or_default())
        .filter(|change| change["op"] != "upsert")
        .map(|change| change["id"].clone())
        .collect();
    assert_eq!(
        removed,
        [json!("prov-1"), json!("prov-2")],
        "each provisional record is removed for subscribers"
    );
}

#[test]
fn a_simulated_vehicles_link_takes_only_a_new_name() {
    let turns = Turns::default();
    let browser = DaemonCapabilities {
        runs_in: RunsIn::Browser,
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(&turns, "fake", State::default(), &Host::default(), browser);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": {
            "type": "simulator", "vehicle": "multicopter",
            "latitudeDeg": 37.78, "longitudeDeg": -122.51, "altitudeM": 5.0, "headingDeg": 0.0
        } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "name": "Test quad" }),
    );
    assert_eq!(renamed["name"], "Test quad");
    let refused = call(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "policy": "read" }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
    let off = call(&mut engine, &mut ui, "link/disable", json!({ "id": id }));
    assert_eq!(off["error"]["data"]["kind"], "invalid-value");
    assert!(
        off["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Stop the simulation")
    );
}

#[test]
fn a_link_turned_off_stays_off_through_a_restart_until_turned_on() {
    let turns = Turns::default();
    let (mut engine, opened) = engine_watching_links(&turns, State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "name": "Radio", "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15010 } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    engine.link_status(&id, LinkStatus::Up, None);
    engine.vehicle_observed(seen("v", &[], &id, Presence::Online));
    opened.0.lock().unwrap().clear();

    let off = call_ok(&mut engine, &mut ui, "link/disable", json!({ "id": id }));
    assert_eq!(off["status"], "disabled");
    assert!(off.get("statusDetail").is_none());
    assert_eq!(opened.0.lock().unwrap().as_slice(), [format!("close {id}")]);
    assert_eq!(
        engine.state().vehicles["v"].presence,
        Presence::Offline,
        "nothing hears it while the link is off"
    );
    engine.link_status(&id, LinkStatus::Up, None);
    assert_eq!(
        engine.state().links[&id].status,
        LinkStatus::Disabled,
        "a late report does not turn it on"
    );
    // Off again is nothing.
    let version = off["version"].clone();
    let again = call_ok(&mut engine, &mut ui, "link/disable", json!({ "id": id }));
    assert_eq!(again["version"], version);

    // Its settings can still change, and it stays off.
    let moved = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({ "id": id, "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 15011 } }),
    );
    assert_eq!(moved["status"], "disabled");
    assert_eq!(
        opened.0.lock().unwrap().as_slice(),
        [format!("close {id}")],
        "nothing is opened while it is off"
    );

    // A restart leaves it off.
    let (restarted, reopened) = engine_watching_links(&turns, engine.state().clone());
    assert!(reopened.0.lock().unwrap().is_empty(), "{:?}", reopened.0);
    assert_eq!(restarted.state().links[&id].status, LinkStatus::Disabled);
    drop(restarted);

    let on = call_ok(&mut engine, &mut ui, "link/enable", json!({ "id": id }));
    assert_eq!(on["status"], "configured");
    assert_eq!(on["kind"]["port"], 15011);
    assert_eq!(opened.0.lock().unwrap().last().unwrap(), "open Radio");
}

#[test]
fn the_wake_lock_is_held_while_any_vehicle_is_online() {
    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    engine.vehicle_observed(seen("a", &[], "lk", Presence::Online));
    let mut b = seen("b", &[], "lk", Presence::Online);
    b.system_id = 2;
    engine.vehicle_observed(b.clone());
    engine.vehicle_observed(seen("a", &[], "lk", Presence::Offline));
    b.presence = Presence::Stale;
    engine.vehicle_observed(b);
    assert_eq!(host.0.lock().unwrap().wake_lock, [true, false]);
}

#[test]
fn quitting_waits_for_the_last_window_and_respects_keep_alive_and_headless() {
    use uxduck_engine::OnLastClientClosed;

    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    assert!(!engine.should_quit(), "keep-running is the default");
    engine.set_shutdown(OnLastClientClosed::Quit, false);
    let ui = connect(&mut engine, "operator-ui");
    assert!(!engine.should_quit(), "a window is open");
    let cli = connect(&mut engine, "operator-cli");
    engine.disconnect(ui.id);
    assert!(engine.should_quit(), "a CLI call is not a window");
    engine.disconnect(cli.id);

    let mut ui = connect(&mut engine, "operator-ui");
    call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14550 }, "keepAlive": true }),
    );
    engine.disconnect(ui.id);
    assert!(!engine.should_quit(), "a keep-alive link");

    let mut headless = engine_with(&turns, "fake", State::default(), &Host::default());
    headless.set_shutdown(OnLastClientClosed::Quit, true);
    assert!(!headless.should_quit(), "uxduck forward");
}

/// With the menu-bar icon switched off on a desktop, the last window
/// closing quits UXDuck (S107, ADR-0025 D1); never before a window opened,
/// and never when a close policy was chosen.
#[test]
fn with_the_menu_bar_icon_off_the_last_window_quits() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    engine.set_desktop(true);
    let mut cli = connect(&mut engine, "operator-cli");
    let settings = call_ok(
        &mut engine,
        &mut cli,
        "state/subscribe",
        json!({ "entities": ["settings"] }),
    );
    let version = settings["snapshot"]["settings"]["version"].clone();
    let updated = call_ok(
        &mut engine,
        &mut cli,
        "settings/update",
        json!({ "expectedVersion": version, "showInMenuBar": false }),
    );
    assert!(
        !engine.should_quit(),
        "no window has opened: started at login"
    );
    let ui = connect(&mut engine, "operator-ui");
    assert!(!engine.should_quit(), "a window is open");
    engine.disconnect(ui.id);
    assert!(engine.should_quit(), "the last window closed, no icon");

    call_ok(
        &mut engine,
        &mut cli,
        "settings/update",
        json!({ "expectedVersion": updated["version"], "closePolicy": "keep-running" }),
    );
    assert!(!engine.should_quit(), "a chosen policy wins");
}

/// Opening UXDuck again brings forward a page already open: one seen on
/// screen first, else the one seen last; with none, the tray opens one
/// (S107).
#[test]
fn opening_again_brings_the_open_page_forward() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut cli = connect(&mut engine, "operator-cli");
    let none = call_ok(&mut engine, &mut cli, "app/focus", json!({}));
    assert_eq!(none["focused"], false);

    let mut first = connect(&mut engine, "operator-ui");
    let mut second = connect(&mut engine, "operator-ui");
    call_ok(
        &mut engine,
        &mut first,
        "client/visibility",
        json!({ "visible": true }),
    );
    call_ok(
        &mut engine,
        &mut second,
        "client/visibility",
        json!({ "visible": false }),
    );
    let focused = call_ok(&mut engine, &mut cli, "app/focus", json!({}));
    assert_eq!(focused["focused"], true);
    assert_eq!(notifications(&mut first, "app/focus").len(), 1);
    assert!(notifications(&mut second, "app/focus").is_empty());

    // Both hidden: the one hidden last was seen last.
    call_ok(
        &mut engine,
        &mut first,
        "client/visibility",
        json!({ "visible": false }),
    );
    let _ = call_ok(&mut engine, &mut cli, "app/focus", json!({}));
    assert_eq!(notifications(&mut first, "app/focus").len(), 1);
    assert!(notifications(&mut second, "app/focus").is_empty());

    // The tray's "Open UXDuck" does the same, and opens nothing new.
    let mut tray = connect(&mut engine, "operator-cli");
    let open = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "open" }),
    );
    assert_eq!(open["type"], "done");
    assert_eq!(notifications(&mut first, "app/focus").len(), 1);

    // Pages close: the tray opens a new one.
    engine.disconnect(first.id);
    engine.disconnect(second.id);
    let open = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "open" }),
    );
    assert_eq!(open["type"], "open-url");
}

/// Quitting says what would stop, and asks nothing when nothing would
/// (S107, ADR-0025 D1).
#[test]
fn quitting_says_what_will_stop() {
    let (mut engine, _opened) = engine_simulating(&Turns::default());
    let mut tray = connect(&mut engine, "operator-cli");
    let preview = call_ok(&mut engine, &mut tray, "daemon/stopPreview", json!({}));
    assert_eq!(preview["stops"], json!([]));
    assert!(preview.get("summary").is_none());

    let start = json!({"vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16});
    call_ok(&mut engine, &mut tray, "sim/start", start);
    let preview = call_ok(&mut engine, &mut tray, "daemon/stopPreview", json!({}));
    assert_eq!(preview["summary"], "1 simulation will stop.");

    call_ok(
        &mut engine,
        &mut tray,
        "link/add",
        json!({ "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14550 } }),
    );
    let real = call_ok(
        &mut engine,
        &mut tray,
        "link/add",
        json!({"kind": {"type": "udp-listen", "host": "127.0.0.1", "port": 15010}}),
    );
    engine.vehicle_observed(seen(
        "v1",
        &[],
        real["id"].as_str().unwrap(),
        Presence::Online,
    ));
    let preview = call_ok(&mut engine, &mut tray, "daemon/stopPreview", json!({}));
    assert_eq!(
        preview["summary"],
        "1 simulation, a forward link and the connection to Alpha will stop."
    );

    let quit = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "quit" }),
    );
    assert_eq!(quit["type"], "confirm");
    assert_eq!(quit["title"], "Quit UXDuck?");
    assert_eq!(quit["confirm"], "Quit");
    assert!(!engine.stop_requested(), "asked, not quit");
    let quit = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": quit["item"] }),
    );
    assert_eq!(quit["type"], "done");
    assert!(engine.stop_requested());
}

#[test]
fn quitting_with_nothing_running_asks_nothing() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut tray = connect(&mut engine, "operator-cli");
    let quit = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "quit" }),
    );
    assert_eq!(quit["type"], "done");
    assert!(engine.stop_requested());
}

/// Sends a request and returns the reply, or `None` when the engine will
/// answer later.
fn call_maybe(
    engine: &mut Engine,
    client: &mut Client,
    method: &str,
    params: Value,
) -> Option<Value> {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            return Some(frame);
        }
        client.queued.push_back(frame);
    }
    None
}

fn with_vehicle(presence: Presence) -> (Engine, Client, String) {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v", &[], &link_id, Presence::Online));
    if presence != Presence::Online {
        engine.vehicle_observed(seen("v", &[], &link_id, presence));
    }
    (engine, ui, link_id)
}

#[test]
fn a_command_is_answered_when_the_vehicle_acknowledges_it() {
    use uxduck_engine::commands::Outcome;

    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    let pending = call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    );
    assert!(pending.is_none(), "answered once the vehicle has");
    let receipt = engine.state().receipts.values().next().unwrap().clone();
    assert_eq!(
        receipt.status,
        uxduck_core::domain::command::ReceiptStatus::Sent
    );
    assert!(!receipt.sent_without_recent_heartbeat);

    engine.command_result(&receipt.id, Outcome::Accepted, 1);
    let reply = notifications_or_reply(&mut ui);
    assert_eq!(reply["result"]["status"], "accepted");
    assert_eq!(reply["result"]["attempts"], 1);
    assert!(engine.state().notifications.is_empty(), "nothing to tell");

    // A refusal is answered, and a transient notification says so.
    call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "set-mode", "mode": "GUIDED" } }),
    );
    let second = engine
        .state()
        .receipts
        .values()
        .find(|r| r.status == uxduck_core::domain::command::ReceiptStatus::Sent)
        .unwrap()
        .id
        .clone();
    engine.command_result(&second, Outcome::Rejected("Denied".into()), 1);
    let note = engine
        .state()
        .notifications
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(note.title, "Alpha refused Set mode GUIDED");
    assert_eq!(note.report.as_deref(), Some("command-refused"));
    assert_eq!(
        note.kind,
        uxduck_core::domain::notification::NotificationKind::Transient
    );
    call_ok(
        &mut engine,
        &mut ui,
        "notification/dismiss",
        json!({ "id": note.id }),
    );
    assert!(engine.state().notifications.is_empty());
}

/// The next response frame from a client, skipping notifications.
fn notifications_or_reply(client: &mut Client) -> Value {
    std::iter::from_fn(|| client.inbox.try_recv().ok())
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .find(|f| f.get("id").is_some())
        .expect("a reply")
}

#[test]
fn the_offline_table_is_enforced() {
    let (mut engine, mut ui, _) = with_vehicle(Presence::Offline);
    let blocked = call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    )
    .expect("answered at once");
    assert_eq!(blocked["result"]["status"], "not-sent");
    assert!(
        blocked["result"]["detail"]
            .as_str()
            .unwrap()
            .contains("Can't send Arm")
    );

    let rtl = call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "rtl" } }),
    );
    assert!(rtl.is_none(), "RTL is sent");
    let sent = engine
        .state()
        .receipts
        .values()
        .find(|r| r.command == uxduck_core::domain::command::Command::Rtl)
        .unwrap();
    assert!(sent.sent_without_recent_heartbeat);

    let warn = call(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "set-mode", "mode": "GUIDED" } }),
    );
    assert_eq!(warn["error"]["data"]["kind"], "confirmation-required");
    assert!(
        call_maybe(
            &mut engine,
            &mut ui,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "set-mode", "mode": "GUIDED" }, "confirm": true }),
        )
        .is_none(),
        "sent once confirmed"
    );
    assert!(
        call_maybe(
            &mut engine,
            &mut ui,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" }, "force": true }),
        )
        .is_none(),
        "an operator may force it"
    );

    // The offline alarm is there, and goes when the vehicle is heard again.
    assert!(
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "Alpha is offline")
    );
    let link = engine.state().vehicles["v"].link_id.clone();
    engine.vehicle_observed(seen("v", &[], &link, Presence::Online));
    assert!(engine.state().notifications.is_empty());

    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let labels: Vec<&str> = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"Set mode LOITER") && labels.contains(&"Land"),
        "{labels:?}"
    );
}

#[test]
fn command_list_offers_guided_destinations_for_supported_firmware_families() {
    for (autopilot, vehicle_type, offered) in [
        ("Ardupilotmega", "Quadrotor", true),
        ("Ardupilotmega", "Helicopter", true),
        ("Ardupilotmega", "FixedWing", true),
        ("Ardupilotmega", "VtolQuadrotor", true),
        ("Ardupilotmega", "GroundRover", true),
        ("Ardupilotmega", "SurfaceBoat", true),
        ("Ardupilotmega", "Submarine", true),
        // Blimp's DO_REPOSITION handler acknowledges without setting a target.
        ("Ardupilotmega", "Airship", false),
        ("Ardupilotmega", "AntennaTracker", false),
        ("Ardupilotmega", "Generic", false),
        ("Px4", "Quadrotor", true),
        ("Px4", "FixedWing", true),
        ("Px4", "VtolQuadrotor", true),
        ("Px4", "GroundRover", true),
    ] {
        let (mut engine, mut ui, link) = with_vehicle(Presence::Online);
        let mut observation = seen("v", &[], &link, Presence::Online);
        observation.autopilot = autopilot.into();
        observation.vehicle_type = vehicle_type.into();
        engine.vehicle_observed(observation);
        let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
        let go_to = list["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["command"]["type"] == "go-to");
        assert_eq!(
            go_to.is_some(),
            offered,
            "{autopilot} {vehicle_type}: {list}"
        );
        if let Some(command) = go_to {
            assert_eq!(command["risk"]["level"], "confirm");
            assert_eq!(command["offlineBehaviour"], "warn");
            assert_eq!(command["class"], "mission");
            assert_eq!(command["maxAgeMs"], 2_000);
        }
        if vehicle_type == "Submarine" {
            assert!(list["commands"].as_array().unwrap().iter().any(|c| {
                c["command"]["type"] == "set-mode" && c["command"]["mode"] == "GUIDED"
            }));
        }
    }
}

#[test]
fn command_list_offers_the_mode_a_vehicle_is_in_when_it_is_not_a_usual_one() {
    let (mut engine, mut ui, link) = with_vehicle(Presence::Online);
    let mut observation = seen("v", &[], &link, Presence::Online);
    observation.mode = Some("FLOWHOLD".to_owned());
    engine.vehicle_observed(observation);
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let flowhold = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"]["mode"] == "FLOWHOLD")
        .unwrap_or_else(|| panic!("no FLOWHOLD in {list}"));
    assert_eq!(
        flowhold["description"],
        "Holds its position with an optical flow sensor; no GPS needed"
    );
}

#[test]
fn command_list_offers_every_mode_of_the_airframe_the_usual_ones_first() {
    let (mut engine, mut ui, link) = with_vehicle(Presence::Online);
    let modes = |list: &serde_json::Value| -> Vec<(String, bool)> {
        list["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["command"]["type"] == "set-mode")
            .map(|c| {
                (
                    c["command"]["mode"].as_str().unwrap().to_owned(),
                    c["advanced"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    };
    // A copter: the usual modes first, the rest marked for "More modes",
    // and nothing it only enters by itself.
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let copter = modes(&list);
    assert_eq!(copter[0], ("LOITER".to_owned(), false));
    assert!(copter.contains(&("FLIP".to_owned(), true)), "{copter:?}");
    assert!(copter.contains(&("SMART_RTL".to_owned(), false)));
    assert!(
        !copter
            .iter()
            .any(|(m, _)| m == "AVOID_ADSB" || m == "AUTOROTATE")
    );
    assert_eq!(
        copter.len(),
        23,
        "26 in the table, two hidden, AUTOROTATE for helicopters"
    );

    // A quadplane has its hover modes, a plain plane has none of them.
    let mut vtol = seen("v", &[], &link, Presence::Online);
    vtol.vehicle_type = "VtolQuadrotor".into();
    engine.vehicle_observed(vtol);
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let quadplane = modes(&list);
    assert!(
        quadplane.contains(&("QHOVER".to_owned(), false)),
        "{quadplane:?}"
    );
    assert!(quadplane.contains(&("QSTABILIZE".to_owned(), true)));
    assert!(quadplane.contains(&("FBWB".to_owned(), false)));
    let mut plane = seen("v", &[], &link, Presence::Online);
    plane.vehicle_type = "FixedWing".into();
    engine.vehicle_observed(plane);
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let plane = modes(&list);
    assert!(!plane.iter().any(|(m, _)| m.starts_with('Q')), "{plane:?}");
    assert!(plane.contains(&("FBWB".to_owned(), false)));
    assert!(plane.contains(&("THERMAL".to_owned(), true)));

    // A quadplane that reports itself as fixed wing, as the simulated one
    // does, is known by its Q_ENABLE: its hover modes and take-off come back.
    engine.parameter_reported(
        "v",
        uxduck_engine::transfers::Reported {
            name: "Q_ENABLE".into(),
            value: 1.0,
            kind: uxduck_core::domain::parameter::ParamKind::Int8,
        },
        false,
    );
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let quadplane = modes(&list);
    for mode in ["QLOITER", "QHOVER", "QRTL", "QLAND"] {
        assert!(
            quadplane.contains(&(mode.to_owned(), false)),
            "{mode} {quadplane:?}"
        );
    }
    assert!(
        list["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["command"]["type"] == "long" && c["command"]["command"] == 22),
        "take off: {list}"
    );
}

#[test]
fn command_list_prefers_the_modes_a_vehicle_lists_itself() {
    use uxduck_core::domain::modes::AvailableMode;
    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    // A copter whose firmware lists four modes, under its own names, one
    // of them scripted and one nobody chooses.
    engine.vehicle_modes_listed(
        "v",
        vec![
            AvailableMode::new(5, "LOITER", 0),
            AvailableMode::new(0, "STABILIZE", 0),
            AvailableMode::new(14, "FLIP", 1),
            AvailableMode::new(100, "Survey", 0),
            AvailableMode::new(27, "AUTO_RTL", 2),
        ],
    );
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let modes: Vec<(String, bool)> = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["command"]["type"] == "set-mode")
        .map(|c| {
            (
                c["command"]["mode"].as_str().unwrap().to_owned(),
                c["advanced"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        modes,
        vec![
            ("LOITER".to_owned(), false),
            ("STABILIZE".to_owned(), false),
            ("FLIP".to_owned(), true),
            ("Survey".to_owned(), true),
        ]
    );
    // The table's words for the modes it knows; none for a scripted one.
    let survey = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["command"]["mode"] == "Survey")
        .unwrap();
    assert!(survey.get("description").is_none(), "{survey}");
}

#[test]
fn command_list_gives_each_commands_risk_on_the_ground_in_flight_and_forced() {
    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    assert_eq!(list["flyingAboveHomeM"], 2.0);
    let info = |label: &str| {
        list["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["label"] == label)
            .unwrap_or_else(|| panic!("no {label} in {list}"))
            .clone()
    };
    // Safety commands are never in the way.
    for label in ["Land", "Return to launch", "Set mode LOITER"] {
        assert_eq!(info(label)["risk"]["level"], "none", "{label}");
        assert_eq!(info(label)["riskInFlight"]["level"], "none", "{label}");
    }
    // A mode says what it does, and so does every button on Control.
    assert_eq!(
        info("Set mode LOITER")["description"],
        "Holds its position and height by GPS; the sticks move it smoothly"
    );
    for label in ["Arm", "Disarm", "Take off", "Return to launch", "Land"] {
        assert!(info(label)["description"].is_string(), "{label}");
    }
    let take_off = info("Take off");
    assert_eq!(take_off["command"]["command"], 22);
    assert_eq!(take_off["risk"]["level"], "confirm");
    let arm = info("Arm");
    assert_eq!(arm["risk"]["level"], "confirm");
    assert_eq!(arm["forcedRisk"]["level"], "dangerous");
    assert!(arm["forcedRisk"]["reason"].is_string());
    let disarm = info("Disarm");
    assert_eq!(disarm["risk"]["level"], "none");
    assert_eq!(disarm["riskInFlight"]["level"], "dangerous");
    assert_eq!(info("Set mode GUIDED")["risk"]["level"], "confirm");
}

#[test]
fn skipped_confirmations_are_kept_per_operator_and_reset() {
    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    let ana = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    assert_eq!(ana["skippedConfirmations"], json!([]));
    let skipped = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": ana["version"],
                "skippedConfirmations": ["vehicle:take-off", "vehicle:take-off", "vehicle:arm"] }),
    );
    assert_eq!(
        skipped["skippedConfirmations"],
        json!(["vehicle:take-off", "vehicle:arm"]),
        "kept once each"
    );
    let reset = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": skipped["version"],
                "skippedConfirmations": [] }),
    );
    assert_eq!(reset["skippedConfirmations"], json!([]));
    let ben = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ben" }),
    );
    assert_eq!(ben["skippedConfirmations"], json!([]), "per operator");
}

#[test]
fn agents_are_held_for_approval_and_read_only_links_send_nothing() {
    let (mut engine, mut ui, link_id) = with_vehicle(Presence::Online);
    let mut agent = connect(&mut engine, "agent");
    let held = call_maybe(
        &mut engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "land" } }),
    );
    assert!(held.is_none(), "held for an operator, not refused");

    call_ok(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({ "id": link_id }),
    );
    let read_only = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14560 }, "policy": "read" }),
    );
    engine.vehicle_observed(seen(
        "w",
        &[],
        read_only["id"].as_str().unwrap(),
        Presence::Online,
    ));
    let refused = call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "w", "command": { "type": "rtl" } }),
    )
    .unwrap();
    assert_eq!(refused["result"]["status"], "not-sent");
    assert!(
        refused["result"]["detail"]
            .as_str()
            .unwrap()
            .contains("read-only")
    );
}

#[test]
fn a_loop_raises_an_alarm_with_a_button_and_only_recent_receipts_are_kept() {
    use uxduck_engine::commands::Outcome;

    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    let forward = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-forward", "host": "10.1.1.9", "port": 14550 } }),
    );
    let id = forward["id"].as_str().unwrap();
    engine.link_disabled_by_loop(id, "radio", "10.1.1.9:14550");
    // Forwarding also asks whether to keep it after a restart (S64).
    let alarm = engine
        .state()
        .notifications
        .values()
        .find(|n| n.title.contains("its messages were looping back"))
        .unwrap()
        .clone();
    assert_eq!(alarm.actions[0].label, "Turn on");
    call_ok(&mut engine, &mut ui, "link/enable", json!({ "id": id }));
    assert!(
        !engine
            .state()
            .notifications
            .values()
            .any(|n| n.id == alarm.id
                && n.state == uxduck_core::domain::notification::NotificationState::Active),
        "resolved by turning it on"
    );

    for _ in 0..25 {
        call_maybe(
            &mut engine,
            &mut ui,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "disarm" } }),
        );
        let open = engine
            .state()
            .receipts
            .values()
            .find(|r| r.status == uxduck_core::domain::command::ReceiptStatus::Sent)
            .unwrap()
            .id
            .clone();
        engine.command_result(&open, Outcome::Accepted, 1);
    }
    assert_eq!(
        engine.state().receipts.len(),
        uxduck_engine::state::RECEIPTS_PER_VEHICLE
    );
}

/// Takes the next frame of a method from a client's inbox.
fn next_of(client: &mut Client, method: &str) -> Option<Value> {
    notifications(client, method).into_iter().next()
}

#[test]
fn an_agent_escalates_and_the_operator_decides_how_long_it_may() {
    use uxduck_engine::commands::Outcome;

    let (mut engine, mut ui, _) = with_vehicle(Presence::Online);
    let mut agent = connect(&mut engine, "agent");
    // Reading needs nothing.
    assert!(
        call(&mut engine, &mut agent, "vehicle/list", json!({}))
            .get("result")
            .is_some()
    );

    // Arming is held and put to the operators.
    assert!(
        call_maybe(
            &mut engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none()
    );
    let approval = engine.state().approvals.values().next().unwrap().clone();
    assert_eq!(approval.summary, "arm Alpha");
    let note = engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some(&format!("approval:{}", approval.id)))
        .unwrap()
        .clone();
    let labels: Vec<&str> = note.actions.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Allow once",
            "Allow for this connection",
            "Always allow",
            "Deny"
        ]
    );
    assert_eq!(note.title, "agent wants to arm Alpha");
    // The agent cannot answer, having no `approve`.
    let own = call(
        &mut engine,
        &mut agent,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true }),
    );
    assert_eq!(own["error"]["data"]["kind"], "capability-denied");

    // Always allow: the arm runs now, as the agent, and answers its request.
    call_ok(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert!(engine.state().approvals.is_empty());
    assert!(
        engine.state().notifications.is_empty(),
        "the approval's notice resolves"
    );
    let sent = engine
        .state()
        .receipts
        .values()
        .find(|r| r.status == uxduck_core::domain::command::ReceiptStatus::Sent)
        .unwrap()
        .clone();
    assert!(sent.client_id != "daemon", "journaled as the agent");
    engine.command_result(&sent.id, Outcome::Accepted, 1);
    let reply = notifications_or_reply(&mut agent);
    assert_eq!(reply["result"]["status"], "accepted");

    // The next arm does not ask.
    assert!(
        call_maybe(
            &mut engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "disarm" } }),
        )
        .is_none()
    );
    assert!(engine.state().approvals.is_empty(), "no approval this time");
    let open = engine
        .state()
        .receipts
        .values()
        .find(|r| r.status == uxduck_core::domain::command::ReceiptStatus::Sent)
        .unwrap()
        .id
        .clone();
    engine.command_result(&open, Outcome::Accepted, 1);
    notifications_or_reply(&mut agent);

    // The operator takes control away again: live, and the next call asks.
    let clients = call_ok(&mut engine, &mut ui, "client/list", json!({}));
    let row = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client"]["kind"] == "agent")
        .unwrap()
        .clone();
    assert_eq!(row["connections"], 1);
    assert_eq!(row["client"]["capabilities"], json!(["read", "control"]));
    call_ok(
        &mut engine,
        &mut ui,
        "client/setCapabilities",
        json!({ "id": row["client"]["id"], "expectedVersion": row["client"]["version"], "capabilities": ["read"] }),
    );
    let changed = next_of(&mut agent, "client/capabilitiesChanged").expect("told at once");
    assert_eq!(changed["granted"], json!(["read"]));
    call_maybe(
        &mut engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    );
    let again = engine.state().approvals.values().next().unwrap().id.clone();
    call_ok(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": again, "allow": false }),
    );
    let denied = notifications_or_reply(&mut agent);
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    assert!(
        denied["error"]["message"]
            .as_str()
            .unwrap()
            .contains("denied")
    );
}

#[test]
fn an_unanswered_approval_is_denied_at_its_deadline() {
    let (mut engine, _ui, _) = with_vehicle(Presence::Online);
    let mut agent = connect(&mut engine, "agent");
    call_maybe(
        &mut engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    );
    let approval = engine.state().approvals.values().next().unwrap().clone();
    engine.expire_approvals(approval.deadline_us - 1);
    assert_eq!(engine.state().approvals.len(), 1, "not yet");
    engine.expire_approvals(approval.deadline_us);
    assert!(engine.state().approvals.is_empty());
    let expired = notifications_or_reply(&mut agent);
    assert!(
        expired["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nobody answered")
    );
    assert!(engine.state().receipts.is_empty(), "nothing was sent");
}

#[test]
fn a_vehicle_heard_again_under_its_provisional_id_keeps_its_lasting_record() {
    let (mut engine, _ui, link) = with_vehicle(Presence::Online);
    // "v" is lasting, with "p" as its provisional id from first contact.
    engine.vehicle_observed(seen("v", &["p"], &link, Presence::Online));
    // After a restart, the session knows it only as "p" until its UID.
    engine.vehicle_observed(seen("p", &[], &link, Presence::Online));
    assert_eq!(
        engine.state().vehicles.keys().collect::<Vec<_>>(),
        ["v"],
        "no second record"
    );
    engine.vehicle_observed(seen("v", &["p"], &link, Presence::Online));
    assert_eq!(engine.state().vehicles.len(), 1);
}

/// Simulations started together are named by their instance, whichever is
/// heard first: the second is Bravo even when it answers before the first.
#[test]
fn a_simulated_vehicle_takes_its_instances_callsign() {
    let turns = Turns::default();
    let installed = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        installed,
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let at = |vehicle: &str| json!({ "vehicle": vehicle, "latitudeDeg": -35.36, "longitudeDeg": 149.16 });
    let first = call_ok(&mut engine, &mut ui, "sim/start", at("plane"));
    let second = call_ok(&mut engine, &mut ui, "sim/start", at("plane"));
    engine.vehicle_observed(seen(
        "b",
        &[],
        second["id"].as_str().unwrap(),
        Presence::Online,
    ));
    engine.vehicle_observed(seen(
        "a",
        &[],
        first["id"].as_str().unwrap(),
        Presence::Online,
    ));
    assert_eq!(engine.state().vehicles["b"].name, "Bravo");
    assert_eq!(engine.state().vehicles["a"].name, "Alpha");
}

/// The last run's simulations are not remembered (maintainer, 2026-10-11):
/// they go with their vehicles, and nothing starts. "Recreate simulations
/// when UXDuck starts" starts each again afresh instead.
#[test]
fn simulations_from_the_last_run_are_forgotten_unless_recreated() {
    let turns = Turns::default();
    let installed = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        installed,
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let at = |vehicle: &str| json!({ "vehicle": vehicle, "latitudeDeg": -35.36, "longitudeDeg": 149.16 });
    let copter = call_ok(&mut engine, &mut ui, "sim/start", at("multicopter"));
    let plane = call_ok(&mut engine, &mut ui, "sim/start", at("plane"));
    let (copter, plane) = (
        copter["id"].as_str().unwrap().to_owned(),
        plane["id"].as_str().unwrap().to_owned(),
    );
    engine.vehicle_observed(seen("c", &[], &copter, Presence::Online));
    assert!(
        !engine.state().settings.recreate_simulations,
        "off until turned on"
    );

    let (forgotten, opened) = engine_watching_links(&turns, engine.state().clone());
    assert!(
        !opened
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|o| o.starts_with("open ")),
        "nothing started: {:?}",
        opened.0
    );
    assert!(!forgotten.state().links.contains_key(&copter));
    assert!(!forgotten.state().links.contains_key(&plane));
    assert!(!forgotten.state().vehicles.contains_key("c"));

    let version = engine.state().settings.version;
    call_ok(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": version, "recreateSimulations": true }),
    );
    let (recreated, opened) = engine_watching_links(&turns, engine.state().clone());
    for id in [&copter, &plane] {
        assert_eq!(recreated.state().links[id].status, LinkStatus::Configured);
    }
    assert!(
        recreated.state().vehicles.contains_key("c"),
        "the same vehicle, afresh"
    );
    let mut opened = opened.0.lock().unwrap().clone();
    opened.sort();
    let mut expected = vec![
        format!("close {copter}"),
        format!("close {plane}"),
        "open Simulated multicopter".to_owned(),
        "open Simulated plane".to_owned(),
    ];
    expected.sort();
    assert_eq!(opened, expected);
}

#[test]
fn a_simulation_started_with_a_forward_sends_only_its_own_traffic_and_takes_the_forward_with_it() {
    let turns = Turns::default();
    let installed = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        installed,
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let start = |forward: serde_json::Value| json!({ "vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16, "forward": forward });
    let sim = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        start(json!({ "host": "127.0.0.1", "port": 14550 })),
    );
    let sim_id = sim["id"].as_str().unwrap().to_owned();
    let forwards: Vec<_> = engine
        .state()
        .links
        .values()
        .filter(|l| l.kind.is_forward())
        .cloned()
        .collect();
    assert_eq!(forwards.len(), 1);
    let forward = &forwards[0];
    assert_eq!(forward.from_link.as_deref(), Some(sim_id.as_str()));
    assert_eq!(
        forward.policy,
        LinkPolicy::ReadWrite,
        "both ways by default"
    );
    assert_eq!(forward.name, "Simulated multicopter to 127.0.0.1:14550");

    // Another simulation can use the same destination with its own policy.
    let watched = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        start(json!({ "host": "127.0.0.1", "port": 14550, "policy": "read" })),
    );
    assert_ne!(watched["id"], sim["id"]);
    let watched_id = watched["id"].as_str().unwrap();
    assert!(
        engine.state().links.values().any(|l| {
            l.from_link.as_deref() == Some(watched_id) && l.policy == LinkPolicy::Read
        })
    );

    // Restarting keeps it; stopping removes it.
    call_ok(&mut engine, &mut ui, "sim/restart", json!({ "id": sim_id }));
    assert!(engine.state().links.contains_key(&forward.id));
    call_ok(&mut engine, &mut ui, "sim/stop", json!({ "id": sim_id }));
    assert!(!engine.state().links.contains_key(&forward.id));
    assert_eq!(
        engine.state().links.len(),
        2,
        "the other simulation and its forward stay"
    );
}

#[test]
fn simulated_vehicles_start_on_their_own_instances_and_stop_only_as_simulators() {
    let turns = Turns::default();
    let installed = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        installed,
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let at = |vehicle: &str| json!({ "vehicle": vehicle, "latitudeDeg": -35.36, "longitudeDeg": 149.16, "frame": "X" });
    let first = call_ok(&mut engine, &mut ui, "sim/start", at("multicopter"));
    let second = call_ok(&mut engine, &mut ui, "sim/start", at("plane"));
    assert_eq!(first["kind"]["instance"], 0);
    assert_eq!(second["kind"]["instance"], 1);
    assert_eq!(second["name"], "Simulated plane");

    let too_fast = call(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "vtol", "latitudeDeg": 0, "longitudeDeg": 0, "speedup": 50 }),
    );
    assert_eq!(too_fast["error"]["data"]["kind"], "invalid-value");
    let spaced = call(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "vtol", "latitudeDeg": 0, "longitudeDeg": 0, "frame": "quad plane" }),
    );
    assert_eq!(spaced["error"]["data"]["kind"], "invalid-value");

    // Stopping frees the instance for the next one.
    let first_id = first["id"].as_str().unwrap();
    // Its link is not removed on its own: stopping is the one way.
    let kept = call(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({ "id": first_id }),
    );
    assert_eq!(kept["error"]["data"]["kind"], "invalid-value");
    call_ok(&mut engine, &mut ui, "sim/stop", json!({ "id": first_id }));
    // Its first heartbeat, already on its way as it stopped, brings no
    // vehicle back.
    engine.vehicle_observed(seen("late", &[], first_id, Presence::Online));
    assert!(!engine.state().vehicles.contains_key("late"));
    let third = call_ok(&mut engine, &mut ui, "sim/start", at("multicopter"));
    assert_eq!(third["kind"]["instance"], 0);

    // sim/stop leaves real links alone.
    let udp = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14570 } }),
    );
    let refused = call(&mut engine, &mut ui, "sim/stop", json!({ "id": udp["id"] }));
    assert_eq!(refused["error"]["data"]["kind"], "not-found");
}

/// Renaming (maintainer, 2026-10-07): a name goes with the vehicle's id,
/// which comes from its autopilot's UID, so it stays when the system id is
/// reused. A real vehicle renamed is kept; a simulated one is only renamed
/// and still goes with its simulation.
#[test]
fn a_vehicle_is_renamed_and_a_simulated_one_still_goes_with_its_simulation() {
    let turns = Turns::default();
    let installed = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        installed,
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let real = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14571 } }),
    );
    let real_link = real["id"].as_str().unwrap();
    engine.vehicle_observed(seen("real", &[], real_link, Presence::Online));
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "real", "name": "Survey quad" }),
    );
    assert_eq!(renamed["name"], "Survey quad");
    assert_eq!(renamed["saved"], true);
    // An empty name puts back a callsign.
    let reset = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "real", "name": "  " }),
    );
    assert_eq!(reset["name"], "Alpha");
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "real", "name": "Survey quad" }),
    );

    let sim = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    let sim_link = sim["id"].as_str().unwrap();
    engine.vehicle_observed(seen("simulated", &[], sim_link, Presence::Online));
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "simulated", "name": "Practice" }),
    );
    assert_eq!(renamed["name"], "Practice");
    assert_eq!(
        renamed["saved"], false,
        "a simulation is not kept by its name"
    );
    call_ok(&mut engine, &mut ui, "sim/stop", json!({ "id": sim_link }));
    assert!(!engine.state().vehicles.contains_key("simulated"));
    assert_eq!(engine.state().vehicles["real"].name, "Survey quad");
}

/// A simulation started again (`sim/restart`, maintainer 2026-10-07): the
/// same link, closed and opened again, so the simulator starts afresh at
/// its place while the vehicle keeps its link, instance and identity. Its
/// vehicles go offline until the new simulator is heard, and the take-off
/// begins again. Real links are left alone.
#[test]
fn a_simulation_started_again_keeps_its_link_and_starts_afresh() {
    let turns = Turns::default();
    let (mut engine, opened) = engine_simulating(&turns);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16, "inAirM": 50 }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    assert_eq!(link["launch"]["stage"], "climbing");
    engine.vehicle_observed(seen("v", &[], &link_id, Presence::Online));
    engine.link_status(
        &link_id,
        LinkStatus::Down,
        Some("the simulated vehicle stopped".into()),
    );
    opened.0.lock().unwrap().clear();

    let again = call_ok(
        &mut engine,
        &mut ui,
        "sim/restart",
        json!({ "id": link_id }),
    );
    assert_eq!(again["id"], link_id);
    assert_eq!(again["kind"]["instance"], link["kind"]["instance"]);
    assert_eq!(again["status"], "configured");
    assert!(again.get("statusDetail").is_none(), "{again}");
    assert_eq!(
        again["launch"]["stage"], "climbing",
        "the take-off begins again"
    );
    assert_eq!(
        opened.0.lock().unwrap().as_slice(),
        [
            format!("close {link_id}"),
            "open Simulated multicopter".to_owned()
        ]
    );
    assert_eq!(
        engine.state().vehicles["v"].presence,
        Presence::Offline,
        "offline until the new simulator is heard"
    );
    // Heard again: the same vehicle, not a second record.
    engine.vehicle_observed(seen("v", &[], &link_id, Presence::Online));
    assert_eq!(engine.state().vehicles.len(), 1);
    assert_eq!(engine.state().vehicles["v"].presence, Presence::Online);

    // Only simulated vehicles start again.
    let udp = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14570 } }),
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "sim/restart",
        json!({ "id": udp["id"] }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "not-found");
}

/// Records the commands and parameter writes a take-off sends, in order.
#[derive(Clone, Default)]
struct Flown(Arc<Mutex<Vec<(String, String)>>>);

impl LinkPort for Flown {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: uxduck_engine::runtime::CommandOrder) {
        let sent = format!("{:?}", order.command);
        self.0.lock().unwrap().push((order.receipt_id, sent));
    }
    fn transfer(&mut self, order: uxduck_engine::runtime::TransferOrder) {
        if let uxduck_engine::runtime::TransferOrder::SetParameter {
            order_id,
            name,
            value,
            ..
        } = order
        {
            self.0
                .lock()
                .unwrap()
                .push((order_id, format!("{name} = {value}")));
        }
    }
}

impl Flown {
    fn last(&self) -> (String, String) {
        self.0.lock().unwrap().last().cloned().unwrap()
    }
    fn sent(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.clone())
            .collect()
    }
}

/// The autopilot's height above home, heard now.
fn height(id: &str, meters: f64) -> uxduck_core::domain::telemetry::VehicleTelemetry {
    use uxduck_core::domain::telemetry::{
        Altitude, AltitudeFrame, PositionEstimate, VehicleTelemetry,
    };
    VehicleTelemetry {
        vehicle_id: id.into(),
        positions: vec![PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: 37.79,
            longitude_deg: -122.48,
            altitudes: vec![Altitude {
                meters,
                frame: AltitudeFrame::AboveHome,
            }],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: NOW.with(std::cell::Cell::get),
        }],
        items: vec![],
    }
}

/// Someone who sets a mode while a simulated VTOL is still climbing keeps
/// it: the take-off no longer switches it to QLOITER at the top of the
/// climb, and the simulator goes back to its speed at once (maintainer
/// 2026-10-08, GUIDED turning into QLOITER in the browser).
#[test]
fn a_mode_set_during_a_simulated_take_off_is_kept() {
    use uxduck_engine::commands::Outcome;
    use uxduck_engine::transfers::Reported;

    let flown = Flown::default();
    let mut engine = engine_simulating_with(&Turns::default(), Box::new(flown.clone()));
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "vtol", "latitudeDeg": 37.79, "longitudeDeg": -122.48, "inAirM": 100 }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v", &[], &link_id, Presence::Online));

    // Sped up, ready, GUIDED, armed, taking off.
    let (write, what) = flown.last();
    assert_eq!(what, "SIM_SPEEDUP = 9");
    let reported = |value| Reported {
        name: "SIM_SPEEDUP".into(),
        value,
        kind: uxduck_core::domain::parameter::ParamKind::Real32,
    };
    engine.parameter_written(&write, Ok(reported(9.0)), 1);
    advance(1_000);
    engine.telemetry_observed(height("v", 0.0));
    for _ in ["GUIDED", "arm", "take off"] {
        engine.command_result(&flown.last().0, Outcome::Accepted, 1);
    }
    assert!(flown.last().1.contains("command: 22"), "{:?}", flown.sent());
    advance(1_000);
    engine.telemetry_observed(height("v", 30.0));

    // Climbing: someone sets GUIDED.
    call_maybe(
        &mut engine,
        &mut ui,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "set-mode", "mode": "GUIDED" } }),
    );
    let guided = flown.0.lock().unwrap()[4].0.clone();
    engine.command_result(&guided, Outcome::Accepted, 1);
    let (write, what) = flown.last();
    assert_eq!(what, "SIM_SPEEDUP = 1", "back to its speed at once");
    engine.parameter_written(&write, Ok(reported(1.0)), 1);

    advance(1_000);
    engine.telemetry_observed(height("v", 100.0));
    assert!(
        !flown.sent().iter().any(|s| s.contains("QLOITER")),
        "{:?}",
        flown.sent()
    );
    assert_eq!(
        engine.state().links[&link_id]
            .launch
            .as_ref()
            .unwrap()
            .stage,
        uxduck_core::domain::link::LaunchStage::Flying
    );
}

/// A simulated VTOL is handed over in GUIDED, which holds its height by
/// itself. QLOITER climbs or sinks with the throttle stick, which rests at
/// the bottom in a simulator: handed over there, it sank from its take-off
/// height to the ground (maintainer 2026-10-11).
#[test]
fn a_simulated_vtol_is_handed_over_in_guided() {
    use uxduck_engine::commands::Outcome;
    use uxduck_engine::transfers::Reported;

    let flown = Flown::default();
    let mut engine = engine_simulating_with(&Turns::default(), Box::new(flown.clone()));
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "vtol", "latitudeDeg": 37.79, "longitudeDeg": -122.48, "inAirM": 10 }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v", &[], &link_id, Presence::Online));

    let reported = |value| Reported {
        name: "SIM_SPEEDUP".into(),
        value,
        kind: uxduck_core::domain::parameter::ParamKind::Real32,
    };
    let (write, _) = flown.last();
    engine.parameter_written(&write, Ok(reported(9.0)), 1);
    advance(1_000);
    engine.telemetry_observed(height("v", 0.0));
    for _ in ["GUIDED", "arm", "take off"] {
        engine.command_result(&flown.last().0, Outcome::Accepted, 1);
    }

    // At the top of the climb it holds in GUIDED, then slows back down.
    advance(1_000);
    engine.telemetry_observed(height("v", 10.0));
    let (hold, what) = flown.last();
    assert_eq!(what, r#"SetMode { mode: "GUIDED" }"#, "{:?}", flown.sent());
    engine.command_result(&hold, Outcome::Accepted, 1);
    let (write, what) = flown.last();
    assert_eq!(what, "SIM_SPEEDUP = 1");
    engine.parameter_written(&write, Ok(reported(1.0)), 1);

    assert!(
        !flown.sent().iter().any(|s| s.contains("QLOITER")),
        "{:?}",
        flown.sent()
    );
    assert_eq!(
        engine.state().links[&link_id]
            .launch
            .as_ref()
            .unwrap()
            .stage,
        uxduck_core::domain::link::LaunchStage::Flying
    );
}

/// Telemetry that puts a vehicle this high above home.
fn at_height(id: &str, height_m: f64) -> uxduck_core::domain::telemetry::VehicleTelemetry {
    use uxduck_core::domain::telemetry::{
        Altitude, AltitudeFrame, PositionEstimate, VehicleTelemetry,
    };
    VehicleTelemetry {
        vehicle_id: id.into(),
        positions: vec![PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: -35.36,
            longitude_deg: 149.16,
            altitudes: vec![Altitude {
                meters: height_m,
                frame: AltitudeFrame::AboveHome,
            }],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: 1,
        }],
        items: vec![],
    }
}

/// A simulated vehicle back on the ground after flying (maintainer,
/// 2026-10-07): one notice with "Start again", named a crash when the
/// autopilot said so; cleared when it arms again or its simulation starts
/// again. A real vehicle gets no such notice.
#[test]
fn a_simulated_vehicle_on_the_ground_after_flying_is_offered_a_start_again() {
    let turns = Turns::default();
    let (mut engine, _opened) = engine_simulating(&turns);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({ "vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    let observed = |armed: bool| {
        let mut observation = seen("v1", &[], &link_id, Presence::Online);
        observation.armed = armed;
        observation
    };
    engine.vehicle_observed(observed(false));
    engine.vehicle_booted("v1", 1_000, false);
    // Disarmed without ever flying: nothing to say.
    engine.vehicle_observed(observed(true));
    engine.telemetry_observed(at_height("v1", 0.5));
    engine.vehicle_observed(observed(false));
    assert!(active_notifications(&mut engine, &mut ui).is_empty());

    // Flown, then disarmed: on the ground.
    engine.vehicle_observed(observed(true));
    engine.telemetry_observed(at_height("v1", 30.0));
    engine.vehicle_observed(observed(false));
    let notices = active_notifications(&mut engine, &mut ui);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["title"], "Alpha is on the ground");
    assert_eq!(notices[0]["kind"], "persistent");
    assert_eq!(notices[0]["actions"][0]["label"], "Start again");
    assert_eq!(
        notices[0]["actions"][0]["action"],
        json!({ "type": "restart-simulation", "linkId": link_id })
    );
    // Armed again: over.
    engine.vehicle_observed(observed(true));
    assert!(active_notifications(&mut engine, &mut ui).is_empty());

    // A crash is said at once, by name, and the disarm that follows adds
    // nothing; a repeated crash message is one notice.
    engine.telemetry_observed(at_height("v1", 30.0));
    engine.vehicle_message("v1", 1, 0, "Crash: Disarming");
    engine.vehicle_message("v1", 1, 0, "Crash: Disarming");
    engine.vehicle_observed(observed(false));
    let notices = active_notifications(&mut engine, &mut ui);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["title"], "Alpha crashed");
    assert_eq!(
        notices[0]["actions"][0]["action"]["type"],
        "restart-simulation"
    );

    // Started again: over, and the next flight starts afresh.
    call_ok(
        &mut engine,
        &mut ui,
        "sim/restart",
        json!({ "id": link_id }),
    );
    assert!(active_notifications(&mut engine, &mut ui).is_empty());
    engine.vehicle_observed(observed(false));
    assert!(active_notifications(&mut engine, &mut ui).is_empty());

    // A real vehicle landing is its own business.
    let udp = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14570 } }),
    );
    let udp_id = udp["id"].as_str().unwrap().to_owned();
    let mut real = seen("v2", &[], &udp_id, Presence::Online);
    real.armed = true;
    engine.vehicle_observed(real.clone());
    engine.telemetry_observed(at_height("v2", 30.0));
    engine.vehicle_message("v2", 1, 0, "Crash: Disarming");
    real.armed = false;
    engine.vehicle_observed(real);
    let notices = active_notifications(&mut engine, &mut ui);
    assert!(
        notices
            .iter()
            .all(|n| n["actions"][0]["action"]["type"] != "restart-simulation"),
        "{notices:?}"
    );
}

/// Take off is offered to a quadplane too (maintainer, 2026-10-07):
/// `ArduPlane` lifts it straight up in GUIDED once armed, and the button
/// says so. A plain plane still has none.
#[test]
fn command_list_offers_take_off_to_a_quadplane_with_its_conditions() {
    let (mut engine, mut ui, link) = with_vehicle(Presence::Online);
    let mut observation = seen("v", &[], &link, Presence::Online);
    observation.vehicle_type = "VtolQuadrotor".into();
    engine.vehicle_observed(observation.clone());
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    let take_off = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["label"] == "Take off")
        .unwrap_or_else(|| panic!("no take-off in {list}"));
    assert_eq!(take_off["command"]["command"], 22);
    assert_eq!(
        take_off["description"],
        "Lifts off straight up and hovers. Choose GUIDED and arm it first."
    );

    observation.vehicle_type = "FixedWing".into();
    engine.vehicle_observed(observation);
    let list = call_ok(&mut engine, &mut ui, "command/list", json!({ "id": "v" }));
    assert!(
        list["commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["label"] != "Take off"),
        "{list}"
    );
}

/// The tray's menu (S64): plain while all is well, the vehicles online, and
/// what is wrong when something is broken or waiting.
#[test]
fn the_tray_menu_follows_the_state_and_says_what_is_wrong() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut tray = connect(&mut engine, "operator-cli");
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(menu["status"], "UXDuck is running");
    assert_eq!(menu["tone"], "plain");
    let ids: Vec<&str> = menu["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "open",
            "simulations",
            "stop-simulations",
            "show-files",
            "login-item",
            "docs",
            "feedback",
            "quit"
        ]
    );
    assert_eq!(menu["items"][7]["label"], "Quit UXDuck");
    assert_eq!(menu["items"][2]["enabled"], false);

    // A vehicle online is in the status line, and the tray hears of it.
    let link = call_ok(
        &mut engine,
        &mut tray,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v1", &[], &link_id, Presence::Online));
    let changed = notifications(&mut tray, "tray/menuChanged");
    assert_eq!(
        changed.last().unwrap()["status"],
        "UXDuck is running · Alpha online"
    );

    // A link that went down is broken.
    engine.link_status(&link_id, LinkStatus::Down, Some("no route".into()));
    let changed = notifications(&mut tray, "tray/menuChanged");
    assert_eq!(
        changed.last().unwrap()["status"],
        "UDP 14550: not connected"
    );
    assert_eq!(changed.last().unwrap()["tone"], "broken");
    engine.link_status(&link_id, LinkStatus::Up, None);
    assert_eq!(
        notifications(&mut tray, "tray/menuChanged").last().unwrap()["tone"],
        "plain"
    );

    // With the desktop app registered, "Open in a browser" is offered too.
    let settings = call_ok(
        &mut engine,
        &mut tray,
        "state/subscribe",
        json!({ "entities": ["settings"] }),
    );
    let version = settings["snapshot"]["settings"]["version"].clone();
    call_ok(
        &mut engine,
        &mut tray,
        "settings/update",
        json!({ "expectedVersion": version, "shellPath": "/Applications/UXDuck.app/Contents/MacOS/UXDuck" }),
    );
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(menu["items"][1]["id"], "open-browser");
}

/// What each item does (S64).
#[test]
fn each_tray_item_says_what_the_tray_does_next() {
    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut tray = connect(&mut engine, "operator-cli");
    let open = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "open" }),
    );
    assert_eq!(open["type"], "open-url");
    assert!(
        open["url"].as_str().unwrap().contains("?bootstrap="),
        "{open}"
    );
    let docs = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "docs" }),
    );
    // The local website's in a development build, as this test is.
    assert_eq!(docs["url"], uxduck_engine::tray::docs_url());
    if std::env::var_os("UXDUCK_DOCS_URL").is_none() {
        assert_eq!(docs["url"], "http://localhost:8792/docs?from=tray");
    }

    // Feedback: the screen is kept for the app's dialog, once.
    let feedback = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "feedback", "screenshot": "/9j/AAAA" }),
    );
    let url = feedback["url"].as_str().unwrap();
    let id = url.split("&feedback=").nth(1).unwrap().to_owned();
    let shot = call_ok(
        &mut engine,
        &mut tray,
        "feedback/screenshot",
        json!({ "id": id }),
    );
    assert_eq!(shot["screenshot"], "data:image/jpeg;base64,/9j/AAAA");
    let again = call_ok(
        &mut engine,
        &mut tray,
        "feedback/screenshot",
        json!({ "id": id }),
    );
    assert!(again.get("screenshot").is_none(), "taken once");

    // With the desktop app, UXDuck starts it for open, a page and feedback (S110).
    host.0.lock().unwrap().desktop_app = Some("/opt/UXDuck/uxduck-desktop".into());
    call_ok(
        &mut engine,
        &mut tray,
        "desktopApp/choose",
        json!({ "on": true }),
    );
    for item in ["open", "simulations", "feedback"] {
        let action = call_ok(
            &mut engine,
            &mut tray,
            "tray/activate",
            json!({ "item": item }),
        );
        assert_eq!(action["type"], "done", "{item}");
    }
    {
        let opens = &host.0.lock().unwrap().desktop_app_opens;
        assert!(opens[0].is_empty());
        assert_eq!(opens[1], ["--page=/vehicles#simulated"]);
        assert!(opens[2][0].starts_with("--feedback=ss"));
    }
    let browser = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "open-browser" }),
    );
    assert_eq!(browser["type"], "open-url");

    // Quit stops UXDuck.
    let quit = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "quit" }),
    );
    assert_eq!(quit["type"], "done");
    assert!(engine.stop_requested());

    // Only an operator may use the tray.
    let mut agent = connect(&mut engine, "agent");
    let refused = call(&mut engine, &mut agent, "tray/menu", json!({}));
    assert!(refused.get("error").is_some(), "{refused}");
}

/// Start at login (S64): off until turned on, registered through the host,
/// and suggested once when a forward is added.
#[test]
fn start_at_login_is_registered_by_the_host_and_suggested_once_for_a_forward() {
    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut ui = connect(&mut engine, "operator-ui");
    let settings = call_ok(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["settings", "notification"] }),
    );
    let settings = &settings["snapshot"]["settings"];
    assert_eq!(settings["startAtLogin"], false);
    assert_eq!(settings["showInMenuBar"], true);
    let version = settings["version"].as_u64().unwrap();

    for port in [14_560, 14_561] {
        call_ok(
            &mut engine,
            &mut ui,
            "link/add",
            json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": port }, "keepAlive": true }),
        );
    }
    let suggested: Vec<Value> = notifications(&mut ui, "state/delta")
        .into_iter()
        .filter(|d| {
            d.to_string()
                .contains("Keep UXDuck running after a restart?")
        })
        .collect();
    assert_eq!(suggested.len(), 1, "suggested once");
    assert!(
        host.0.lock().unwrap().logins.is_empty(),
        "never turned on by itself"
    );

    let updated = call_ok(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": version + 1, "startAtLogin": true }),
    );
    assert_eq!(updated["startAtLogin"], true);
    assert_eq!(host.0.lock().unwrap().logins, [true]);
}

/// Open at login and Show files in the menu bar (S108): the switch shows
/// what the system has, follows it when it is switched off there, and
/// leads to System Settings when macOS waits for the person's OK.
#[test]
fn open_at_login_in_the_menu_shows_what_the_system_has() {
    use uxduck_contracts::dto::settings::{LoginItem, LoginItemState};

    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut tray = connect(&mut engine, "operator-cli");
    let login = |menu: &Value| {
        menu["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"].as_str().unwrap().starts_with("login-item"))
            .cloned()
    };
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(login(&menu).unwrap()["label"], "Open at login");
    assert_eq!(login(&menu).unwrap()["checked"], false);
    assert!(
        menu["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "show-files")
    );

    let done = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "login-item" }),
    );
    assert_eq!(done["type"], "done");
    assert_eq!(host.0.lock().unwrap().logins, [true]);
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(login(&menu).unwrap()["checked"], true);
    assert!(engine.state().settings.start_at_login);

    // Switched off in System Settings: the host says so, and the setting follows.
    engine.login_item_changed(LoginItem {
        state: LoginItemState::Off,
        detail: None,
    });
    assert!(!engine.state().settings.start_at_login);
    let changed = notifications(&mut tray, "tray/menuChanged");
    assert_eq!(login(changed.last().unwrap()).unwrap()["checked"], false);

    // macOS waits for an OK: System Settings opens, and the menu says so.
    host.0.lock().unwrap().login_needs_approval = true;
    let allow = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({ "item": "login-item" }),
    );
    assert_eq!(
        allow["url"],
        "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
    );
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(
        login(&menu).unwrap()["label"],
        "Open at login: allow in System Settings…"
    );
    assert!(login(&menu).unwrap().get("checked").is_none());

    // Not possible here: no item at all.
    engine.login_item_changed(LoginItem {
        state: LoginItemState::Unavailable,
        detail: Some("Open at login needs UXDuck installed in Applications.".into()),
    });
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert!(login(&menu).is_none());
    let settings = call_ok(
        &mut engine,
        &mut tray,
        "state/subscribe",
        json!({ "entities": ["settings"] }),
    );
    assert_eq!(
        settings["snapshot"]["settings"]["loginItem"]["state"],
        "unavailable"
    );
}

/// The desktop app (S110): chosen only once downloaded; then opening
/// Opening UXDuck again while it runs tells the tray, which says so under
/// its icon; an ordinary open does not.
#[test]
fn opening_uxduck_again_tells_the_tray() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut tray = connect(&mut engine, "operator-cli");
    call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    let mut cli = connect(&mut engine, "operator-cli");

    call_ok(&mut engine, &mut cli, "app/open", json!({}));
    assert!(notifications(&mut tray, "tray/reopened").is_empty());

    call_ok(
        &mut engine,
        &mut cli,
        "app/open",
        json!({ "reopened": true }),
    );
    assert_eq!(notifications(&mut tray, "tray/reopened"), vec![json!({})]);
    assert!(notifications(&mut cli, "tray/reopened").is_empty());
}

/// UXDuck, from the menu bar or `app/open`, starts it rather than a page,
/// and one that fails its checks falls back to the browser, said so.
#[test]
fn opening_uxduck_starts_the_desktop_app_once_chosen() {
    let turns = Turns::default();
    let host = Host::default();
    let mut engine = engine_with(&turns, "fake", State::default(), &host);
    let mut cli = connect(&mut engine, "operator-cli");
    let refused = call(
        &mut engine,
        &mut cli,
        "desktopApp/choose",
        json!({ "on": true }),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Download")
    );

    let exe = "/data/desktop-app/0.2.0/UXDuck.app/Contents/MacOS/UXDuck";
    host.0.lock().unwrap().desktop_app = Some(exe.into());
    let chosen = call_ok(
        &mut engine,
        &mut cli,
        "desktopApp/choose",
        json!({ "on": true }),
    );
    assert_eq!(chosen["chosen"], true);
    assert_eq!(engine.state().settings.shell_path.as_deref(), Some(exe));

    let opened = call_ok(&mut engine, &mut cli, "app/open", json!({}));
    assert_eq!(opened, json!({ "opened": true }));
    let feedback = call_ok(
        &mut engine,
        &mut cli,
        "tray/activate",
        json!({ "item": "feedback", "screenshot": "/9j/AAAA" }),
    );
    assert_eq!(feedback["type"], "done");
    let page = call_ok(
        &mut engine,
        &mut cli,
        "app/open",
        json!({ "page": "/vehicles" }),
    );
    assert_eq!(page["opened"], true);
    {
        let opens = &host.0.lock().unwrap().desktop_app_opens;
        assert_eq!(opens.len(), 3);
        assert!(opens[0].is_empty());
        assert!(opens[1][0].starts_with("--feedback=ss"));
        assert_eq!(opens[2], ["--page=/vehicles"]);
    }

    // It fails its checks: the browser instead, and a notice says why.
    host.0.lock().unwrap().desktop_app_broken = true;
    let fallback = call_ok(&mut engine, &mut cli, "app/open", json!({}));
    assert_eq!(fallback["opened"], false);
    assert!(fallback["url"].as_str().unwrap().contains("?bootstrap="));
    assert!(
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "The desktop app couldn't open")
    );

    // Removed: UXDuck opens in a browser again.
    host.0.lock().unwrap().desktop_app_broken = false;
    let removed = call_ok(&mut engine, &mut cli, "desktopApp/remove", json!({}));
    assert_eq!(removed["chosen"], false);
    assert!(engine.state().settings.shell_path.is_none());
    let browser = call_ok(&mut engine, &mut cli, "app/open", json!({}));
    assert_eq!(browser["opened"], false);
}

/// The initial Simple layout is present before a vehicle connects, and a
/// vehicle connecting changes no one's dock.
#[test]
fn the_initial_layout_is_there_before_the_first_vehicle_and_stays_after() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    let ben = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ben" }),
    );
    let ana_layout = &ana["layout"];
    assert_eq!(ana_layout["flightOpened"], true);
    assert!(
        !ana_layout["dock"].to_string().contains("\"chat\""),
        "the chat opens on request"
    );
    // Simple starts with a 2D map and Control above Telemetry on the right.
    assert_eq!(ana_layout["current"], "preset-simple");
    assert_eq!(
        ana_layout["dock"]["children"][0]["node"]["tabs"][0]["kind"],
        "map"
    );
    let tabs: Vec<&str> = ana_layout["dock"]["children"][1]["node"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["node"]["tabs"][0]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(tabs, ["control", "telemetry"]);
    // Ben opens a tab: his dock is his to arrange.
    let mut layout = ben["layout"].clone();
    layout["dock"] = json!({ "type": "group", "tabs": [{ "id": "map", "kind": "map" }, { "id": "telemetry", "kind": "telemetry" }], "active": 0 });
    let ben = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ben["id"], "expectedVersion": ben["version"], "layout": layout }),
    );
    assert_eq!(ben["layout"]["arranged"], true);

    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    engine.vehicle_observed(seen(
        "v1",
        &[],
        link["id"].as_str().unwrap(),
        Presence::Online,
    ));
    let operators = call_ok(&mut engine, &mut ui, "operator/list", json!({}));
    let layout_of = |name: &str| {
        operators["operators"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == name)
            .unwrap()["layout"]
            .clone()
    };
    assert_eq!(layout_of("Ana"), ana["layout"], "unchanged by the vehicle");
    assert_eq!(layout_of("Ben"), ben["layout"], "Ben arranged his");
}

/// Telemetry layouts (S65) live on the operator, cleaned, with a choice per
/// vehicle only of a layout that exists.
#[test]
fn telemetry_layouts_are_kept_on_the_operator() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    let updated = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({
            "id": ana["id"],
            "expectedVersion": ana["version"],
            "telemetryLayouts": [
                { "id": "mine", "name": "My planes", "class": "plane", "tiles": [
                    { "item": "speed.air_mps", "decimals": 1 },
                    { "item": "battery.remaining_pct", "rules": [
                        { "test": "below", "value": 25, "colour": "red" }
                    ] }
                ] },
                { "id": "survey", "name": "Survey", "tiles": [{ "item": "camera.photos" }] }
            ],
            "vehicleTelemetry": [
                { "vehicleId": "v1", "layoutId": "survey" },
                { "vehicleId": "v2", "layoutId": "gone" }
            ]
        }),
    );
    assert_eq!(updated["telemetryLayouts"].as_array().unwrap().len(), 2);
    assert_eq!(
        updated["telemetryLayouts"][0]["tiles"][1]["rules"][0]["value"],
        25.0
    );
    assert_eq!(
        updated["vehicleTelemetry"],
        json!([{ "vehicleId": "v1", "layoutId": "survey" }]),
        "a choice of a layout that is gone is dropped"
    );
}

/// Parameter groups (S74) and pinned inspector messages live on the
/// operator, so they follow them to other computers.
#[test]
fn parameter_groups_and_inspector_pins_are_kept_on_the_operator() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    assert_eq!(ana["parameterGroups"], json!([]));
    assert_eq!(ana["inspectorPinned"], json!([]));
    let updated = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({
            "id": ana["id"],
            "expectedVersion": ana["version"],
            "parameterGroups": [
                { "name": " Hover tuning ", "parameters": ["ATC_RAT_RLL_P", "ATC_RAT_PIT_P", "ATC_RAT_RLL_P"] },
                { "name": "Battery", "parameters": ["BATT_CAPACITY"] }
            ],
            "inspectorPinned": ["received:HEARTBEAT", "sent:COMMAND_LONG", "received:HEARTBEAT"]
        }),
    );
    assert_eq!(
        updated["parameterGroups"],
        json!([
            { "name": "Battery", "parameters": ["BATT_CAPACITY"] },
            { "name": "Hover tuning", "parameters": ["ATC_RAT_RLL_P", "ATC_RAT_PIT_P"] }
        ]),
        "by name, trimmed, each parameter once in the order added"
    );
    assert_eq!(
        updated["inspectorPinned"],
        json!(["received:HEARTBEAT", "sent:COMMAND_LONG"])
    );
    let listed = call_ok(&mut engine, &mut ui, "operator/list", json!({}));
    assert_eq!(
        listed["operators"][0]["parameterGroups"],
        updated["parameterGroups"]
    );

    // Another field changed leaves them as they are.
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": updated["version"], "name": "Ana K" }),
    );
    assert_eq!(renamed["parameterGroups"], updated["parameterGroups"]);
    assert_eq!(renamed["inspectorPinned"], updated["inspectorPinned"]);

    for bad in [
        json!({ "parameterGroups": [{ "name": " ", "parameters": [] }] }),
        json!({ "parameterGroups": [{ "name": "x".repeat(41), "parameters": [] }] }),
        json!({ "parameterGroups": [
            { "name": "Hover", "parameters": [] },
            { "name": "Hover ", "parameters": [] }
        ] }),
        json!({ "parameterGroups": [{ "name": "Hover", "parameters": ["atc_rat_rll_p"] }] }),
        json!({ "parameterGroups": [{ "name": "Hover", "parameters": ["A_NAME_TOO_LONG_17"] }] }),
        json!({ "parameterGroups": [{ "name": "Hover", "parameters": vec!["RTL_ALT"; 201] }] }),
        json!({ "parameterGroups": (0..101)
            .map(|i| json!({ "name": format!("Group {i}"), "parameters": [] }))
            .collect::<Vec<_>>() }),
        json!({ "inspectorPinned": ["HEARTBEAT"] }),
        json!({ "inspectorPinned": ["heard:HEARTBEAT"] }),
        json!({ "inspectorPinned": ["sent:"] }),
        json!({ "inspectorPinned": [format!("sent:{}", "A".repeat(65))] }),
        json!({ "inspectorPinned": vec!["sent:PING"; 201] }),
    ] {
        let mut params = bad.clone();
        params["id"] = ana["id"].clone();
        params["expectedVersion"] = renamed["version"].clone();
        let refused = call(&mut engine, &mut ui, "operator/update", params);
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value", "{bad}");
    }

    let cleared = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": renamed["version"],
                "parameterGroups": [], "inspectorPinned": [] }),
    );
    assert_eq!(cleared["parameterGroups"], json!([]));
    assert_eq!(cleared["inspectorPinned"], json!([]));
}

/// The Control tab's buttons live on the operator, per vehicle type, in
/// their order.
#[test]
fn control_actions_are_kept_on_the_operator_per_type() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call_ok(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    assert_eq!(ana["controlActions"], json!([]));
    let updated = call_ok(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({
            "id": ana["id"],
            "expectedVersion": ana["version"],
            "controlActions": [
                { "class": "copter", "actions": ["vehicle:mode:guided", "vehicle:arm", "vehicle:mode:guided"] },
                { "class": "plane", "actions": ["vehicle:mode:precision land"] }
            ]
        }),
    );
    assert_eq!(
        updated["controlActions"],
        json!([
            { "class": "plane", "actions": ["vehicle:mode:precision land"] },
            { "class": "copter", "actions": ["vehicle:mode:guided", "vehicle:arm"] }
        ]),
        "each type once, each action once in the operator's order"
    );
    for bad in [
        json!({ "controlActions": [{ "class": "copter", "actions": ["land"] }] }),
        json!({ "controlActions": [{ "class": "copter", "actions": ["vehicle:mode:GUIDED"] }] }),
        json!({ "controlActions": [
            { "class": "copter", "actions": [] },
            { "class": "copter", "actions": [] }
        ] }),
        json!({ "controlActions": [{ "class": "copter", "actions": (0..41)
            .map(|i| format!("vehicle:mode:m{i}"))
            .collect::<Vec<_>>() }] }),
    ] {
        let mut params = bad.clone();
        params["id"] = ana["id"].clone();
        params["expectedVersion"] = updated["version"].clone();
        let refused = call(&mut engine, &mut ui, "operator/update", params);
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value", "{bad}");
    }
}

/// An operator stored before parameter groups and pins has none of either.
#[test]
fn an_operator_stored_before_groups_and_pins_has_none() {
    let stored: uxduck_core::domain::operator::Operator = serde_json::from_value(json!({
        "id": "op1",
        "name": "Ana",
        "units": "metric",
        "coordinateFormat": "decimal",
        "version": 3
    }))
    .unwrap();
    let mut state = State::default();
    state.operators.insert(stored.id.clone(), stored);
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", state);
    let mut ui = connect(&mut engine, "operator-ui");
    let listed = call_ok(&mut engine, &mut ui, "operator/list", json!({}));
    let ana = &listed["operators"][0];
    assert_eq!(ana["name"], "Ana");
    assert_eq!(ana["parameterGroups"], json!([]));
    assert_eq!(ana["inspectorPinned"], json!([]));
    assert_eq!(ana["controlActions"], json!([]));
}

const SECOND: u64 = 1_000_000;

fn active_notifications(engine: &mut Engine, ui: &mut Client) -> Vec<Value> {
    let state = call_ok(
        engine,
        ui,
        "state/subscribe",
        json!({ "entities": ["notification"] }),
    );
    state["snapshot"]["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["state"] == "active")
        .cloned()
        .collect()
}

/// A simulated vehicle's messages go to its log only (maintainer,
/// 2026-10-03), but for its pre-arm reasons when no simulation start is
/// under way.
#[test]
fn a_simulated_vehicles_messages_stay_in_its_log() {
    let turns = Turns::default();
    let browser = DaemonCapabilities {
        runs_in: RunsIn::Browser,
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(&turns, "fake", State::default(), &Host::default(), browser);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": {
            "type": "simulator",
            "vehicle": "multicopter",
            "latitudeDeg": -35.363_261,
            "longitudeDeg": 149.165_23,
            "altitudeM": 584.0,
            "headingDeg": 353.0
        } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v1", &[], &link_id, Presence::Online));
    engine.vehicle_booted("v1", 1_000, false);
    engine.vehicle_message("v1", 1, 4, "Flight mode change failed");
    advance(60 * SECOND);
    engine.vehicle_message("v1", 1, 4, "Throttle failsafe off");
    assert!(active_notifications(&mut engine, &mut ui).is_empty());
    let log = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/messages",
        json!({ "vehicleId": "v1" }),
    );
    assert_eq!(log["messages"].as_array().unwrap().len(), 2);

    engine.vehicle_message("v1", 1, 3, "PreArm: Gyros not calibrated");
    assert_eq!(active_notifications(&mut engine, &mut ui).len(), 1);

    // Offline, as when the computer sleeps: no alarm.
    engine.vehicle_observed(seen("v1", &[], &link_id, Presence::Offline));
    assert_eq!(active_notifications(&mut engine, &mut ui).len(), 1);
}

/// The MAVLink inspector's calls: a known vehicle, needs `read`, and a host
/// that keeps no table answers nothing, undecoded.
#[test]
fn mavlink_inspect_needs_read_and_a_known_vehicle() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v1", &["provisional"], &link_id, Presence::Online));

    let result = call_ok(
        &mut engine,
        &mut ui,
        "mavlink/inspect",
        json!({ "vehicleId": "v1" }),
    );
    assert_eq!(result, json!({ "decoded": false, "messages": [] }));
    // By an earlier id too, as other vehicle methods.
    call_ok(
        &mut engine,
        &mut ui,
        "mavlink/inspect",
        json!({ "vehicleId": "provisional" }),
    );
    let unknown = call(
        &mut engine,
        &mut ui,
        "mavlink/inspect",
        json!({ "vehicleId": "nope" }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found", "{unknown}");
    // Its history likewise: an empty page from a host that keeps none.
    let history = call_ok(
        &mut engine,
        &mut ui,
        "mavlink/history",
        json!({ "vehicleId": "provisional", "direction": "sent", "id": 0 }),
    );
    assert_eq!(history, json!({ "messages": [], "fromLog": false }));
    let unknown = call(
        &mut engine,
        &mut ui,
        "mavlink/history",
        json!({ "vehicleId": "nope", "direction": "received", "id": 0 }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found", "{unknown}");

    // An agent the operator left without `read` is refused, not held.
    let mut agent = connect(&mut engine, "agent");
    let clients = call_ok(&mut engine, &mut ui, "client/list", json!({}));
    let row = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client"]["kind"] == "agent")
        .unwrap()
        .clone();
    call_ok(
        &mut engine,
        &mut ui,
        "client/setCapabilities",
        json!({ "id": row["client"]["id"], "expectedVersion": row["client"]["version"], "capabilities": [] }),
    );
    let refused = call(
        &mut engine,
        &mut agent,
        "mavlink/inspect",
        json!({ "vehicleId": "v1" }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "capability-denied",
        "{refused}"
    );
    let refused = call(
        &mut engine,
        &mut agent,
        "mavlink/history",
        json!({ "vehicleId": "v1", "direction": "received", "id": 0 }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "capability-denied",
        "{refused}"
    );
}

/// A vehicle's messages (S26): every one in its log, a run of repeats
/// folded with a count, warnings and worse as notifications, start-up warnings grouped,
/// and pre-arm failures one notice that updates and clears.
#[test]
fn vehicle_messages_are_logged_folded_and_notified_by_the_rules() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["id"].as_str().unwrap().to_owned();
    engine.vehicle_observed(seen("v1", &[], &link_id, Presence::Online));
    engine.vehicle_booted("v1", 1_000, false);

    // The start-up burst: info stays in the log, warnings make one notice.
    for (severity, text) in [
        (6, "ArduCopter V4.5.1"),
        (4, "GPS 1: not healthy"),
        (6, "Frame: QUAD"),
        (4, "Compass not calibrated"),
    ] {
        engine.vehicle_message("v1", 1, severity, text);
    }
    let notices = active_notifications(&mut engine, &mut ui);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["title"], "Alpha started with 2 warnings");
    assert_eq!(
        notices[0]["body"],
        "GPS 1: not healthy\nCompass not calibrated"
    );
    assert_eq!(notices[0]["actions"][0]["action"]["type"], "open-messages");

    // Later, a warning repeated every second: one entry, counted, one notice.
    advance(60 * SECOND);
    for _ in 0..5 {
        engine.vehicle_message("v1", 1, 4, "Vibration high");
        advance(SECOND);
    }
    let log = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/messages",
        json!({ "vehicleId": "v1", "query": "vibration" }),
    );
    assert_eq!(log["messages"].as_array().unwrap().len(), 1);
    assert_eq!(log["messages"][0]["count"], 5);
    let raw = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/messages",
        json!({ "vehicleId": "v1", "raw": true }),
    );
    assert_eq!(
        raw["messages"].as_array().unwrap().len(),
        9,
        "every message as it came"
    );
    let vibration = |engine: &mut Engine, ui: &mut Client| {
        active_notifications(engine, ui)
            .iter()
            .filter(|n| n["title"] == "Alpha: Vibration high")
            .count()
    };
    assert_eq!(vibration(&mut engine, &mut ui), 1);
    // Something else between: a new entry, so the log stays in order, but
    // not a new notice.
    engine.vehicle_message("v1", 1, 6, "Mode changed");
    engine.vehicle_message("v1", 1, 4, "Vibration high");
    let log = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/messages",
        json!({ "vehicleId": "v1" }),
    );
    let texts: Vec<_> = log["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .take(3)
        .map(|m| (m["text"].as_str().unwrap(), m["count"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        texts,
        [
            ("Vibration high", 1),
            ("Mode changed", 1),
            ("Vibration high", 5)
        ]
    );
    assert_eq!(vibration(&mut engine, &mut ui), 1);
    // After the quiet window it alerts again.
    advance(61 * SECOND);
    engine.vehicle_message("v1", 1, 4, "Vibration high");
    assert_eq!(vibration(&mut engine, &mut ui), 2);
    // Below the threshold, the log only.
    let before = active_notifications(&mut engine, &mut ui).len();
    engine.vehicle_message("v1", 1, 5, "Throttle failsafe cleared");
    assert_eq!(active_notifications(&mut engine, &mut ui).len(), before);
    let warnings = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/messages",
        json!({ "vehicleId": "v1", "maxSeverity": 4 }),
    );
    assert!(
        warnings["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["severity"].as_u64().unwrap() <= 4)
    );

    // Pre-arm failures: one notice listing them, updated, cleared when armed.
    engine.vehicle_message("v1", 1, 3, "PreArm: Throttle below failsafe");
    engine.vehicle_message("v1", 1, 3, "PreArm: Hardware safety switch");
    let prearm = |engine: &mut Engine, ui: &mut Client| {
        active_notifications(engine, ui)
            .into_iter()
            .filter(|n| n["title"] == "Alpha cannot arm")
            .collect::<Vec<_>>()
    };
    let notice = prearm(&mut engine, &mut ui);
    assert_eq!(notice.len(), 1);
    assert_eq!(
        notice[0]["body"],
        "Hardware safety switch\nThrottle below failsafe"
    );
    advance(30 * SECOND);
    engine.vehicle_message("v1", 1, 3, "PreArm: Throttle below failsafe");
    assert_eq!(
        prearm(&mut engine, &mut ui).len(),
        1,
        "updated, not repeated"
    );
    let mut armed = seen("v1", &[], &link_id, Presence::Online);
    armed.armed = true;
    engine.vehicle_observed(armed);
    assert!(prearm(&mut engine, &mut ui).is_empty(), "cleared on arming");
}

/// Flight time (S103): time armed for every vehicle, time in the air for
/// one that flies, both held after the disarm, worked out by UXDuck.
#[test]
fn time_armed_and_in_the_air_are_worked_out_and_held_after_landing() {
    for vehicle_type in ["Quadrotor", "Airship"] {
        check_flight_times(vehicle_type);
    }
}

fn check_flight_times(vehicle_type: &str) {
    let (mut engine, _ui, link) = with_vehicle(Presence::Online);
    let observed = |armed: bool, vehicle_type: &str| {
        let mut observation = seen("v", &[], &link, Presence::Online);
        observation.armed = armed;
        observation.vehicle_type = vehicle_type.into();
        observation
    };
    let times = |engine: &Engine| {
        let telemetry = engine.telemetry("v").cloned().unwrap_or_default();
        let value = |key: &str| {
            telemetry
                .items
                .iter()
                .find(|i| i.key == key)
                .and_then(|i| i.value)
        };
        (value("flight.armed_s"), value("flight.air_s"))
    };
    // Nothing before the first arm.
    engine.telemetry_observed(at_height("v", 0.0));
    assert_eq!(times(&engine), (None, None));

    engine.vehicle_observed(observed(true, vehicle_type));
    advance(3_000_000);
    engine.telemetry_observed(at_height("v", 0.2));
    assert_eq!(
        times(&engine),
        (Some(3.0), Some(0.0)),
        "armed, still on the ground"
    );
    // Up past 2 m, then 10 s in the air, through the band between the lines.
    engine.telemetry_observed(at_height("v", 5.0));
    advance(10_000_000);
    engine.telemetry_observed(at_height("v", 1.5));
    assert_eq!(times(&engine), (Some(13.0), Some(10.0)));
    // Down below 1 m: the air time stops; the armed time goes on.
    engine.telemetry_observed(at_height("v", 0.5));
    advance(4_000_000);
    engine.telemetry_observed(at_height("v", 0.5));
    assert_eq!(times(&engine), (Some(17.0), Some(10.0)));
    // The autopilot's word wins over the height.
    engine.telemetry_observed(uxduck_core::domain::telemetry::VehicleTelemetry {
        vehicle_id: "v".into(),
        positions: Vec::new(),
        items: vec![uxduck_core::domain::telemetry::TelemetryItem {
            key: "flight.state".into(),
            label: "Flight state".into(),
            unit: uxduck_core::domain::telemetry::Unit::None,
            value: None,
            text: Some("Taking off".into()),
            source: "health".into(),
            component_id: 1,
            time_us: 2,
        }],
    });
    advance(2_000_000);
    engine.telemetry_observed(at_height("v", 0.5));
    assert_eq!(times(&engine), (Some(19.0), Some(12.0)));
    // Disarmed: both hold, and a second later still say the flight's.
    engine.vehicle_observed(observed(false, vehicle_type));
    advance(5_000_000);
    engine.telemetry_observed(at_height("v", 0.5));
    assert_eq!(times(&engine), (Some(19.0), Some(12.0)));
    // Armed again: from zero.
    engine.vehicle_observed(observed(true, vehicle_type));
    engine.telemetry_observed(at_height("v", 0.5));
    assert_eq!(times(&engine), (Some(0.0), Some(0.0)));

    // A rover has a time armed and no time in the air.
    let (mut rover, _, link) = with_vehicle(Presence::Online);
    let mut observation = seen("v", &[], &link, Presence::Online);
    observation.vehicle_type = "GroundRover".into();
    observation.armed = true;
    rover.vehicle_observed(observation);
    advance(1_000_000);
    rover.telemetry_observed(at_height("v", 30.0));
    assert_eq!(times(&rover), (Some(1.0), None));
}

#[test]
fn tray_simulations_are_managed_before_discovery_and_real_links_are_left_running() {
    let (mut engine, opened) = engine_simulating(&Turns::default());
    let mut tray = connect(&mut engine, "operator-cli");
    let real = call_ok(
        &mut engine,
        &mut tray,
        "link/add",
        json!({"kind": {"type": "udp-listen", "host": "127.0.0.1", "port": 15010}}),
    );
    let start =
        |vehicle| json!({"vehicle": vehicle, "latitudeDeg": -35.36, "longitudeDeg": 149.16});
    let first = call_ok(&mut engine, &mut tray, "sim/start", start("multicopter"));
    let second = call_ok(&mut engine, &mut tray, "sim/start", start("plane"));
    let id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    let row = menu["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == format!("simulation:{id}"))
        .unwrap();
    assert!(row["label"].as_str().unwrap().contains("Starting"));
    assert_eq!(row["children"][0]["enabled"], false);
    assert_eq!(row["children"][1]["id"], format!("stop-simulation:{id}"));
    // A failed simulator with no vehicle can still be stopped.
    engine.link_status(
        second_id,
        LinkStatus::Down,
        Some("SITL could not start".into()),
    );
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert!(menu["items"].as_array().unwrap().iter().any(|item| {
        item["label"]
            .as_str()
            .is_some_and(|label| label.contains("Failed"))
    }));
    engine.link_status(id, LinkStatus::Up, None);
    engine.vehicle_observed(seen("sim-v", &[], id, Presence::Online));
    let changes = notifications(&mut tray, "tray/menuChanged");
    assert!(
        changes.last().unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["label"]
                .as_str()
                .is_some_and(|label| label.contains("Running")))
    );
    let action = call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({"item": format!("open-simulation:{id}")}),
    );
    assert!(
        action["url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:47800/vehicles/sim-v?bootstrap=")
    );
    call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({"item": format!("stop-simulation:{second_id}")}),
    );
    assert!(engine.state().links.contains_key(id));
    assert!(!engine.state().links.contains_key(second_id));
    // An arbitrary real link cannot be stopped via a simulation menu id.
    let refused = call(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({"item": format!("stop-simulation:{}", real["id"].as_str().unwrap())}),
    );
    assert!(refused.get("error").is_some());
    opened.0.lock().unwrap().clear();
    call_ok(
        &mut engine,
        &mut tray,
        "tray/activate",
        json!({"item": "stop-simulations"}),
    );
    assert_eq!(engine.state().links.len(), 1);
    assert!(
        engine
            .state()
            .links
            .contains_key(real["id"].as_str().unwrap())
    );
    assert_eq!(*opened.0.lock().unwrap(), vec![format!("close {id}")]);
    let menu = call_ok(&mut engine, &mut tray, "tray/menu", json!({}));
    assert_eq!(menu["items"][2]["enabled"], false);
}

/// Labels (S106): on and off several vehicles at once, matched in any
/// case, a new one taking the spelling in use; renamed by taking one off
/// and putting another on; checked before anything changes; searched.
#[test]
fn labels_go_on_and_off_several_vehicles() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14572 } }),
    );
    let link = link["id"].as_str().unwrap();
    for id in ["a", "b", "c"] {
        engine.vehicle_observed(seen(id, &[], link, Presence::Online));
    }
    let labels = |engine: &Engine, id: &str| engine.state().vehicles[id].labels.clone();

    let changed = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/label",
        json!({ "ids": ["a", "b"], "add": [" Survey ", "Site B"] }),
    );
    assert_eq!(changed["vehicles"].as_array().unwrap().len(), 2);
    assert_eq!(
        changed["vehicles"][0]["labels"],
        json!(["Site B", "Survey"])
    );
    assert_eq!(labels(&engine, "a"), ["Site B", "Survey"]);
    assert!(labels(&engine, "c").is_empty());
    assert!(
        engine.state().vehicles["c"].labels.is_empty() && !engine.state().vehicles["a"].saved,
        "labelling keeps a vehicle as it was otherwise"
    );

    // The spelling in use wins; one already on is not doubled or changed.
    let changed = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/label",
        json!({ "ids": ["a", "c"], "add": ["survey"] }),
    );
    assert_eq!(
        changed["vehicles"].as_array().unwrap().len(),
        1,
        "only c changed"
    );
    assert_eq!(labels(&engine, "c"), ["Survey"]);

    // Renamed where it is: off, and on with the new spelling.
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/label",
        json!({ "ids": ["a", "b", "c"], "remove": ["SURVEY"], "add": ["survey"] }),
    );
    assert_eq!(labels(&engine, "a"), ["Site B", "survey"]);
    assert_eq!(labels(&engine, "c"), ["survey"]);

    // Removed everywhere it is.
    call_ok(
        &mut engine,
        &mut ui,
        "vehicle/label",
        json!({ "ids": ["a", "b"], "remove": ["site b"] }),
    );
    assert_eq!(labels(&engine, "b"), ["survey"]);

    // Searched by label, below a name.
    let found = call_ok(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "surv", "kinds": ["vehicle"] }),
    );
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "{found}");
    assert_eq!(hits[0]["subtitle"], "Vehicles · survey");

    // Nothing changes when any vehicle or label is wrong.
    let before = engine.state().vehicles["a"].version;
    for params in [
        json!({ "ids": ["a", "nope"], "add": ["Night"] }),
        json!({ "ids": ["a"], "add": ["  "] }),
        json!({ "ids": ["a"], "add": ["x".repeat(41)] }),
    ] {
        let reply = call(&mut engine, &mut ui, "vehicle/label", params);
        assert!(reply.get("error").is_some(), "{reply}");
    }
    let many: Vec<String> = (0..20).map(|n| format!("L{n}")).collect();
    let reply = call(
        &mut engine,
        &mut ui,
        "vehicle/label",
        json!({ "ids": ["a"], "add": many }),
    );
    assert!(reply.get("error").is_some(), "21 labels: {reply}");
    assert_eq!(engine.state().vehicles["a"].version, before);
    assert_eq!(labels(&engine, "a"), ["survey"]);
}

#[test]
fn destinations_can_be_added_and_edited_while_a_simulation_runs() {
    let turns = Turns::default();
    let mut engine = engine_running(
        &turns,
        "fake",
        State::default(),
        &Host::default(),
        DaemonCapabilities {
            simulator: true,
            ..DaemonCapabilities::default()
        },
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let sim = call_ok(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({
            "vehicle": "multicopter", "latitudeDeg": -35.36, "longitudeDeg": 149.16,
            "forward": { "host": "127.0.0.1", "port": 14550 }
        }),
    );
    let sim_id = sim["id"].as_str().unwrap();
    let add = |port| {
        json!({
            "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": port },
            "fromLink": sim_id, "policy": "read"
        })
    };
    let second = call_ok(&mut engine, &mut ui, "link/add", add(14551));
    let third = call_ok(&mut engine, &mut ui, "link/add", add(14552));
    assert_eq!(second["fromLink"], sim_id);
    assert_eq!(second["policy"], "read");
    assert_eq!(engine.state().links.len(), 4);

    // Validation failures leave every existing destination intact.
    for params in [
        add(14550),
        json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": 14553 }, "fromLink": "gone" }),
        json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": 14553 }, "fromLink": sim_id }),
        json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": 14553 }, "fromLink": sim_id, "vehicleFilter": ["v"] }),
        json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": 14553 }, "fromLink": second["id"] }),
    ] {
        let refused = call(&mut engine, &mut ui, "link/add", params);
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
        assert_eq!(engine.state().links.len(), 4);
    }
    for params in [
        json!({ "id": second["id"], "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": 14553 } }),
        json!({ "id": second["id"], "vehicleFilter": ["v"] }),
    ] {
        let refused = call(&mut engine, &mut ui, "link/update", params);
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
        assert_eq!(
            engine.state().links[second["id"].as_str().unwrap()].version,
            1
        );
    }

    let edited = call_ok(
        &mut engine,
        &mut ui,
        "link/update",
        json!({
            "id": second["id"], "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": 14554 }, "policy": "read-write"
        }),
    );
    assert_eq!(edited["fromLink"], sim_id);
    assert_eq!(edited["kind"]["port"], 14554);
    assert_eq!(edited["policy"], "read-write");
    call_ok(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({ "id": third["id"] }),
    );
    call_ok(&mut engine, &mut ui, "sim/restart", json!({ "id": sim_id }));
    assert_eq!(
        engine.state().links.len(),
        3,
        "restart keeps both remaining destinations"
    );
    // A daemon reload, recreating simulations, restores the same source
    // binding and edited settings.
    let version = engine.state().settings.version;
    call_ok(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": version, "recreateSimulations": true }),
    );
    let mut restored = engine_running(
        &turns,
        "fake",
        engine.state().clone(),
        &Host::default(),
        DaemonCapabilities {
            simulator: true,
            ..DaemonCapabilities::default()
        },
    );
    let mut restored_ui = connect(&mut restored, "operator-ui");
    let listed = call_ok(&mut restored, &mut restored_ui, "link/list", json!({}));
    let saved = listed["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == second["id"])
        .unwrap();
    assert_eq!(saved["fromLink"], sim_id);
    assert_eq!(saved["kind"]["port"], 14554);
    call_ok(
        &mut restored,
        &mut restored_ui,
        "sim/stop",
        json!({ "id": sim_id }),
    );
    assert!(
        restored.state().links.is_empty(),
        "stop removes every destination"
    );
}

#[test]
fn diagnostics_resolve_connections_and_reject_ambiguous_names_and_bad_bounds() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let source = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({"name":"Telemetry wire","kind":{"type":"udp-listen","host":"127.0.0.1","port":15010}}),
    );
    let forward = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({"name":"Other program","kind":{"type":"udp-forward","host":"10.1.1.9","port":14550}}),
    );
    let seq = engine.state().seq;
    let report = call_ok(
        &mut engine,
        &mut ui,
        "link/diagnose",
        json!({"query":"telemetry WIRE"}),
    );
    assert_eq!(report["link"]["id"], source["id"]);
    assert_eq!(report["forwards"][0]["id"], forward["id"]);
    let destination = call_ok(
        &mut engine,
        &mut ui,
        "link/diagnose",
        json!({"query":forward["id"]}),
    );
    assert!(
        destination["forwards"].as_array().unwrap().is_empty(),
        "the router never forwards from one forward to another"
    );
    assert_eq!(
        engine.state().seq,
        seq,
        "diagnostics do not modify configuration"
    );
    let unsupported = call_ok(
        &mut engine,
        &mut ui,
        "socket/inspect",
        json!({"port":14550}),
    );
    assert_eq!(unsupported["available"], false);
    assert!(!unsupported["notes"].as_array().unwrap().is_empty());
    for (method, params) in [
        ("router/events", json!({"limit":0})),
        ("router/events", json!({"fromUs":2000,"toUs":1000})),
        ("router/events", json!({"fromUs":0,"toUs":600_000_001})),
        ("socket/inspect", json!({"port":0})),
        ("socket/inspect", json!({"port":14550,"host":"not an IP"})),
        ("daemon/logs", json!({"level":"pretend"})),
    ] {
        let refused = call(&mut engine, &mut ui, method, params);
        assert_eq!(
            refused["error"]["data"]["kind"], "invalid-value",
            "{refused}"
        );
    }
    call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({"name":"Telemetry wire","kind":{"type":"udp-listen","host":"127.0.0.1","port":15011}}),
    );
    let ambiguous = call(
        &mut engine,
        &mut ui,
        "link/diagnose",
        json!({"query":"Telemetry wire"}),
    );
    assert_eq!(ambiguous["error"]["data"]["kind"], "invalid-value");
    let exact = call_ok(
        &mut engine,
        &mut ui,
        "link/diagnose",
        json!({"query":source["id"]}),
    );
    assert_eq!(exact["link"]["id"], source["id"]);
}

#[test]
fn distinct_vehicles_on_one_connection_can_forward_to_the_same_destination() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let source = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({"kind":{"type":"udp-listen","host":"127.0.0.1","port":15010}}),
    );
    let source = source["id"].as_str().unwrap();
    engine.vehicle_observed(seen("a", &["earlier-a"], source, Presence::Online));
    let mut b = seen("b", &[], source, Presence::Online);
    b.system_id = 2;
    engine.vehicle_observed(b.clone());
    let make = |ids: serde_json::Value, host: &str| json!({"kind":{"type":"udp-forward","host":host,"port":14550},"vehicleFilter":ids});
    let a = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        make(json!(["a"]), "127.0.0.1"),
    );
    let forward_b = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        make(json!(["b"]), "localhost"),
    );
    assert_ne!(a["id"], forward_b["id"]);
    assert_eq!(forward_b["vehicleFilter"], json!(["b"]));
    let seq = engine.state().seq;
    for ids in [
        json!(["a"]),
        json!(["earlier-a"]),
        json!([]),
        json!(["a", "b"]),
    ] {
        let duplicate = call(&mut engine, &mut ui, "link/add", make(ids, "localhost"));
        assert_eq!(
            duplicate["error"]["data"]["kind"], "invalid-value",
            "{duplicate}"
        );
        assert!(
            duplicate["error"]["message"]
                .as_str()
                .unwrap()
                .contains("overlap")
        );
    }
    let widened = call(
        &mut engine,
        &mut ui,
        "link/update",
        json!({"id":forward_b["id"],"vehicleFilter":["a","b"]}),
    );
    assert_eq!(widened["error"]["data"]["kind"], "invalid-value");
    assert_eq!(
        engine.state().seq,
        seq,
        "rejected additions and updates change nothing"
    );
    call_ok(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({"id":forward_b["id"]}),
    );
    b.system_id = 1;
    engine.vehicle_observed(b);
    let collision = call(
        &mut engine,
        &mut ui,
        "link/add",
        make(json!(["b"]), "localhost"),
    );
    assert!(
        collision["error"]["message"]
            .as_str()
            .unwrap()
            .contains("both use MAVLink system ID 1"),
        "{collision}"
    );
    assert_eq!(
        engine
            .state()
            .links
            .values()
            .filter(|l| l.kind.is_forward())
            .count(),
        1
    );
}

#[test]
fn an_overlapping_forward_is_rejected_before_a_simulation_starts() {
    let turns = Turns::default();
    let (mut engine, _) = engine_simulating(&turns);
    let mut ui = connect(&mut engine, "operator-ui");
    call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({"kind":{"type":"udp-forward","host":"127.0.0.1","port":14550}}),
    );
    let seq = engine.state().seq;
    let result = call(
        &mut engine,
        &mut ui,
        "sim/start",
        json!({"vehicle":"multicopter","latitudeDeg":0,"longitudeDeg":0,"forward":{"host":"localhost","port":14550}}),
    );
    assert_eq!(result["error"]["data"]["kind"], "invalid-value");
    assert_eq!(
        engine.state().seq,
        seq,
        "no simulation started or rollback journaled"
    );
}

/// A vehicle's size for the 3D map (maintainer, 2026-10-13): set and
/// cleared with `vehicle/size`, kept over a restart, checked, and never
/// saving the vehicle as `vehicle/save` would.
#[test]
fn a_vehicle_is_given_a_size_and_keeps_it() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call_ok(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14573 } }),
    );
    let link = link["id"].as_str().unwrap();
    engine.vehicle_observed(seen("a", &[], link, Presence::Online));

    let sized = call_ok(
        &mut engine,
        &mut ui,
        "vehicle/size",
        json!({ "id": "a", "sizeCm": 320 }),
    );
    assert_eq!(sized["sizeCm"], 320);
    assert_eq!(engine.state().vehicles["a"].size_cm, Some(320));
    assert!(
        !engine.state().vehicles["a"].saved,
        "a size doesn't save it"
    );

    for refused in [json!(0), json!(100_001)] {
        let reply = call(
            &mut engine,
            &mut ui,
            "vehicle/size",
            json!({ "id": "a", "sizeCm": refused }),
        );
        assert_eq!(reply["error"]["data"]["kind"], "invalid-value", "{reply}");
    }
    assert_eq!(engine.state().vehicles["a"].size_cm, Some(320));

    let restarted = self::engine(&turns, "fake", engine.state().clone());
    assert_eq!(restarted.state().vehicles["a"].size_cm, Some(320));

    let cleared = call_ok(&mut engine, &mut ui, "vehicle/size", json!({ "id": "a" }));
    assert!(cleared.get("sizeCm").is_none(), "{cleared}");
    assert_eq!(engine.state().vehicles["a"].size_cm, None);
}

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem,
    Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, State};

/// A clock the test moves: microseconds since the Unix epoch.
#[derive(Clone)]
struct Tick(Arc<std::sync::atomic::AtomicU64>);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 2026-10-03, in microseconds.
const NOW_US: u64 = 1_791_000_000_000_000;

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

/// Commands and routes the engine gave the data plane.
#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<String>>>);

impl Orders {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl LinkPort for Orders {
    fn open(&mut self, link: &Link) {
        self.0.lock().unwrap().push(format!("open {}", link.id));
    }
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.0
            .lock()
            .unwrap()
            .push(format!("command {} {:?}", order.vehicle_id, order.command));
    }
    fn ground_heartbeat(&mut self, vehicle_id: &str, on: bool) {
        self.0.lock().unwrap().push(format!(
            "heartbeat {vehicle_id} {}",
            if on { "on" } else { "off" }
        ));
    }
    fn route_controller(&mut self, id: &str, route: Option<ControlRoute>) {
        self.0.lock().unwrap().push(match route {
            Some(route) => format!("route {id} {}", route.vehicle_id),
            None => format!("route {id} none"),
        });
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
    connect_as(engine, kind, Trust::LocalUser)
}

/// Someone away, as the daemon's edge task connects them (S80).
#[allow(dead_code, reason = "shared harness")]
fn remote(engine: &mut Engine, user_id: &str, name: &str) -> Client {
    connect_as(
        engine,
        "remote-operator",
        Trust::Relay {
            user_id: user_id.into(),
            name: name.into(),
        },
    )
}

fn connect_as(engine: &mut Engine, kind: &str, trust: Trust) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Internal, trust, outbox);
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
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

/// Sends a request; its reply if it came at once. A command that was sent
/// is answered only when the vehicle acknowledges it.
fn send(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Option<Value> {
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

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    send(engine, client, method, params).expect("a reply")
}

fn link(id: &str, kind: Value) -> Link {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "kind": kind,
        "policy": "read-write",
        "topology": "auto",
        "status": "up",
        "createdAtUs": 1,
        "version": 1,
    }))
    .unwrap()
}

/// A real vehicle `real` on a UDP link and a simulated one `sim`.
fn setup(clock: Tick) -> (Engine, Client, Orders) {
    let mut world = State::default();
    // The simulation from the last run starts again with UXDuck.
    world.settings.recreate_simulations = true;
    world.links.insert(
        "udp".into(),
        link(
            "udp",
            json!({ "type": "udp-listen", "host": "0.0.0.0", "port": 14550 }),
        ),
    );
    world.links.insert(
        "sitl".into(),
        link(
            "sitl",
            json!({
                "type": "simulator",
                "vehicle": "multicopter",
                "latitudeDeg": -35.363_261,
                "longitudeDeg": 149.165_23,
                "altitudeM": 584.0,
                "headingDeg": 353.0
            }),
        ),
    );
    let orders = Orders::default();
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
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(orders.clone()),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(clock),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: world,
            sessions: vec![],
        },
    );
    let ui = connect(&mut engine, "operator-ui");
    observe(&mut engine, "real", "udp", 1, &[]);
    observe(&mut engine, "sim", "sitl", 2, &[]);
    orders.take();
    (engine, ui, orders)
}

fn engine() -> (Engine, Client, Orders) {
    let clock = Tick(Arc::new(std::sync::atomic::AtomicU64::new(NOW_US)));
    setup(clock)
}

fn attempt(engine: &Engine) -> Value {
    serde_json::to_value(engine.state().practice.get("sim").cloned()).unwrap()
}

#[test]
fn practice_is_listed_with_what_each_vehicle_can_run() {
    let (mut engine, mut ui, _) = engine();
    let listed = call(
        &mut engine,
        &mut ui,
        "practice/list",
        json!({ "vehicleId": "sim" }),
    );
    let exercises = listed["result"]["exercises"].as_array().unwrap();
    assert_eq!(exercises.len(), 22);
    assert_eq!(exercises[0]["id"], "read-the-panel");
    assert_eq!(exercises[10]["audience"], "basics");
    assert_eq!(exercises[0]["audience"], "first-flight");
    assert_eq!(exercises[0]["questions"].as_array().unwrap().len(), 3);
    // Those flown by hand wait for a controller claimed for the vehicle;
    // those for other vehicles (S117) say which they fit.
    let waiting: Vec<_> = exercises
        .iter()
        .filter(|e| {
            e["vehicles"]
                .as_array()
                .unwrap()
                .contains(&json!("multicopter"))
        })
        .filter(|e| e.get("unavailable").is_some())
        .map(|e| {
            (
                e["id"].as_str().unwrap(),
                e["unavailable"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        waiting,
        [
            ("hold-the-box", "Claim a controller to try this."),
            ("through-the-gates", "Claim a controller to try this.")
        ]
    );
    // A real vehicle never gets one (X1).
    let real = call(
        &mut engine,
        &mut ui,
        "practice/list",
        json!({ "vehicleId": "real" }),
    );
    assert_eq!(
        real["result"]["exercises"][0]["unavailable"],
        "Practice needs a simulated vehicle started in UXDuck."
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "mode-tour", "vehicleId": "real" }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
    assert!(orders_empty_of_commands(&engine));
}

fn orders_empty_of_commands(engine: &Engine) -> bool {
    engine
        .state()
        .receipts
        .values()
        .all(|r| r.vehicle_id != "real")
}

#[test]
fn an_attempt_starts_the_simulation_again_at_its_height_and_is_given_up_or_stopped() {
    let (mut engine, mut ui, orders) = engine();
    let started = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "mode-tour", "vehicleId": "sim" }),
    );
    assert_eq!(started["result"]["state"], "starting", "{started}");
    assert_eq!(started["result"]["exerciseId"], "mode-tour");
    // Started again in the air at the exercise's 50 m, from nothing saved.
    let link = &engine.state().links["sitl"];
    let kind = serde_json::to_value(&link.kind).unwrap();
    assert_eq!(kind["inAirM"], 50.0);
    assert!(kind.get("snapshotId").is_none());
    assert!(
        orders.take().iter().any(|o| o == "open sitl"),
        "opened again"
    );
    // Its vehicle's id keys the pushed attempt.
    assert_eq!(attempt(&engine)["state"], "starting");

    let again = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "fly-there", "vehicleId": "sim" }),
    );
    assert_eq!(
        again["error"]["data"]["kind"], "invalid-value",
        "one at a time"
    );

    let stopped = call(
        &mut engine,
        &mut ui,
        "practice/stop",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(stopped["result"]["state"], "given-up");
    let status = call(
        &mut engine,
        &mut ui,
        "practice/status",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(status["result"]["attempt"]["state"], "given-up");
    // Given up counts for nothing.
    assert!(engine.state().practice_best.is_empty());

    // "Start again" gives up a running attempt.
    call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "fly-there", "vehicleId": "sim" }),
    );
    call(&mut engine, &mut ui, "sim/restart", json!({ "id": "sitl" }));
    assert_eq!(attempt(&engine)["state"], "given-up");
    assert_eq!(
        attempt(&engine)["line"],
        "Given up: the simulation was started again or stopped."
    );

    // An exercise that starts on the ground starts it again there.
    let on_ground = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "timed-delivery", "vehicleId": "sim" }),
    );
    assert_eq!(on_ground["result"]["state"], "starting", "{on_ground}");
    let kind = serde_json::to_value(&engine.state().links["sitl"].kind).unwrap();
    assert!(kind.get("inAirM").is_none(), "{kind}");
}

fn observe(engine: &mut Engine, id: &str, link_id: &str, system_id: u8, aliases: &[&str]) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        link_id: link_id.into(),
        peer: format!("127.0.0.1:576{system_id}"),
        system_id,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
}

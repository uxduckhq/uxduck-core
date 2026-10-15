//! Real vehicles fly as simulated ones do: UXDuck holds nothing back for a
//! vehicle on a link that is not a simulator's (SITL started outside UXDuck
//! counts as real). An older UXDuck held them on the ground; what it left
//! behind is tidied away.

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
use uxduck_core::domain::notification::Notification;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem,
    Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, State};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        1_000
    }
}

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
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.0
            .lock()
            .unwrap()
            .push(format!("command {} {:?}", order.vehicle_id, order.command));
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

/// A real vehicle `real` on a UDP link and a simulated one `sim`, from
/// `world`.
fn setup_with(mut world: State) -> (Engine, Client, Orders) {
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
        Box::new(Tick),
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

fn setup() -> (Engine, Client, Orders) {
    setup_with(State::default())
}

/// What became of a command: `sent`, or `not-sent` with its reason.
fn outcome(engine: &mut Engine, ui: &mut Client, vehicle: &str, params: Value) -> String {
    let mut params = params;
    params["vehicleId"] = json!(vehicle);
    match send(engine, ui, "command/send", params) {
        None => "sent".into(),
        Some(reply) => {
            let receipt = &reply["result"];
            assert_eq!(receipt["status"], "not-sent", "{reply}");
            format!("not-sent {}", receipt["reason"].as_str().unwrap_or("none"))
        }
    }
}

/// Each command an older UXDuck held back for a real vehicle, and ones it
/// always sent.
fn commands() -> Vec<Value> {
    let command = |command: Value| json!({ "command": command });
    let mode = |mode: &str| command(json!({ "type": "set-mode", "mode": mode }));
    let long = |number: u16, params: Value| {
        command(json!({ "type": "long", "command": number, "params": params }))
    };
    vec![
        command(json!({ "type": "arm" })),
        json!({ "command": { "type": "arm" }, "force": true }),
        command(json!({ "type": "disarm" })),
        command(json!({ "type": "land" })),
        command(json!({ "type": "rtl" })),
        mode("AUTO"),
        mode("GUIDED"),
        mode("TAKEOFF"),
        mode("LOITER"),
        // MAV_CMD_NAV_TAKEOFF and MAV_CMD_MISSION_START.
        long(22, json!([0, 0, 0, 0, 0, 0, 10])),
        long(300, json!([0, 0])),
        // DO_SET_MODE to GUIDED (4), and an arm by COMPONENT_ARM_DISARM.
        long(176, json!([1, 4])),
        long(400, json!([1])),
    ]
}

#[test]
fn every_command_goes_to_a_real_vehicle_as_to_a_simulated_one() {
    let (mut engine, mut ui, orders) = setup();
    for params in commands() {
        for vehicle in ["real", "sim"] {
            let sent = outcome(&mut engine, &mut ui, vehicle, params.clone());
            assert_eq!(sent, "sent", "{vehicle} {params}");
        }
    }
    let sent = orders.take();
    for flying in ["\"AUTO\"", "\"GUIDED\"", "command: 22,", "command: 300,"] {
        assert!(
            sent.iter()
                .any(|order| order.starts_with("command real") && order.contains(flying)),
            "{flying} reached the real vehicle: {sent:?}"
        );
    }
}

#[test]
fn a_controller_drives_a_real_vehicle() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("g1", "Xbox", true);
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "real" }),
    );
    assert_eq!(claimed["result"]["target"], "real", "{claimed}");
    assert!(
        orders.take().iter().any(|o| o == "route g1 real"),
        "routed to the real vehicle"
    );
}

#[test]
fn initialize_says_flight_for_an_older_interface() {
    let (mut engine, _, _) = setup();
    let (outbox, mut inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    engine.receive(
        id,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "clientInfo": { "name": "t", "version": "0.1.0", "kind": "operator-cli" },
            "capabilities": { "protocolVersion": 1 }
        } })
        .to_string(),
    );
    let reply: Value = serde_json::from_str(&inbox.try_recv().unwrap()).unwrap();
    assert_eq!(reply["result"]["releaseStage"], "flight", "{reply}");
}

#[test]
fn a_real_vehicle_gets_no_notice_and_an_older_ones_is_resolved() {
    let mut world = State::default();
    let older: Notification = serde_json::from_value(json!({
        "id": "n-old",
        "severity": "info",
        "kind": "persistent",
        "state": "active",
        "subject": { "type": "vehicle", "id": "real" },
        "title": "Alpha is a real vehicle",
        "condition": "ground-testing:real",
        "createdAtUs": 1,
        "version": 1
    }))
    .unwrap();
    world.notifications.insert(older.id.clone(), older);
    let (engine, _, _) = setup_with(world);
    // Resolved, it leaves the state; and no new one is raised.
    let notices: Vec<&str> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.title.ends_with("is a real vehicle"))
        .map(|n| n.id.as_str())
        .collect();
    assert!(notices.is_empty(), "{notices:?}");
}

#[test]
fn an_agents_arm_asks_the_operator_in_plain_words() {
    let (mut engine, mut ui, _) = setup();
    let mut agent = connect(&mut engine, "agent");
    let held = send(
        &mut engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "real", "command": { "type": "arm" } }),
    );
    assert!(held.is_none(), "held for the operator: {held:?}");
    let approvals = call(&mut engine, &mut ui, "approval/list", json!({}));
    assert_eq!(approvals["result"]["approvals"][0]["summary"], "arm Alpha");
}

//! Finding vehicles (S44, ADR-0018): what probes look at follows who is
//! looking, finds reach the lookers and expire, `find/vehicles` answers after
//! a round, the network needs an approval, and a flight controller plugged
//! in is announced once unless a link owns it.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::find::{Find, FindPlace, FindReason, FindScope, FoundVehicle};
use uxduck_core::domain::link::UsbId;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::finds::{DiscoveryPort, Look};
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const NOW: u64 = 1_790_000_000_000_000;

#[derive(Clone, Default)]
struct Manual(Arc<AtomicU64>);

impl Clock for Manual {
    fn now_us(&self) -> u64 {
        NOW + self.0.load(Ordering::SeqCst)
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

/// The looks the engine asked for.
#[derive(Clone, Debug, Default)]
struct Probes(Arc<Mutex<Vec<Option<Look>>>>);

impl DiscoveryPort for Probes {
    fn look(&mut self, look: Option<Look>) {
        self.0.lock().unwrap().push(look);
    }
    fn subnets(&self) -> Vec<String> {
        vec!["192.168.1.0/24".into()]
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str, trust: Trust) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, trust, outbox);
    let mut client = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = send(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": kind, "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    )
    .expect("a reply");
    assert!(init.get("result").is_some(), "{init}");
    client
}

fn send(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Option<Value> {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    let mut reply = None;
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            reply = Some(frame);
        } else {
            client.queued.push_back(frame);
        }
    }
    reply
}

fn drain(client: &mut Client) -> Vec<Value> {
    while let Ok(text) = client.inbox.try_recv() {
        client
            .queued
            .push_back(serde_json::from_str(&text).unwrap());
    }
    client.queued.drain(..).collect()
}

fn engine() -> (Engine, Manual, Probes) {
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
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let clock = Manual::default();
    let mut engine = Engine::new(
        config,
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    );
    let probes = Probes::default();
    engine.set_discovery(Box::new(probes.clone()));
    (engine, clock, probes)
}

fn copter_on_tcp(seen_us: u64) -> Find {
    Find {
        id: "tcp:127.0.0.1:5760".into(),
        place: FindPlace::ThisComputer,
        link: Some(uxduck_core::domain::link::LinkKind::TcpClient {
            host: "127.0.0.1".into(),
            port: 5760,
        }),
        evidence: "a heartbeat on TCP 5760".into(),
        usb: None,
        device_name: None,
        vehicle: Some(FoundVehicle {
            system_id: 1,
            vehicle_type: "Quadrotor".into(),
            autopilot: "Ardupilotmega".into(),
            armed: false,
            vehicle_id: None,
            name: None,
        }),
        reason: None,
        detail: None,
        seen_us,
    }
}

#[test]
fn the_probes_look_while_someone_does_and_finds_reach_them_until_they_expire() {
    let (mut engine, clock, probes) = engine();
    let mut ui = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let started = send(
        &mut engine,
        &mut ui,
        "find/start",
        json!({ "scope": "nearby" }),
    )
    .unwrap();
    assert_eq!(started["result"]["finds"], json!([]));
    let look = probes
        .0
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .expect("a look");
    assert_eq!(look.scope, FindScope::Nearby);

    engine.finds_observed(vec![copter_on_tcp(NOW)]);
    let pushed = drain(&mut ui);
    let changed = pushed
        .iter()
        .find(|f| f["method"] == "find/changed")
        .expect("pushed");
    assert_eq!(
        changed["params"]["finds"][0]["vehicle"]["vehicleType"],
        "Quadrotor"
    );
    // Seen again, unchanged: nothing more to say.
    engine.finds_observed(vec![copter_on_tcp(NOW + 1_000_000)]);
    assert!(drain(&mut ui).iter().all(|f| f["method"] != "find/changed"));
    // Gone for 10 s: it expires.
    clock.0.store(12_000_000, Ordering::SeqCst);
    engine.finds_tick();
    let expired = drain(&mut ui);
    assert_eq!(expired[0]["params"]["finds"], json!([]));

    send(&mut engine, &mut ui, "find/stop", json!({}));
    assert_eq!(
        probes.0.lock().unwrap().last().cloned().flatten(),
        None,
        "nobody looks: nothing held"
    );
}

#[test]
fn find_vehicles_answers_after_a_round_and_the_network_asks_the_operator() {
    let (mut engine, _, probes) = engine();
    let mut ui = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let asked = send(
        &mut engine,
        &mut ui,
        "find/vehicles",
        json!({ "scope": "nearby" }),
    );
    assert!(asked.is_none(), "answered after the probes' round");
    engine.finds_observed(vec![Find {
        id: "serial:/dev/ttyUSB0".into(),
        place: FindPlace::Usb,
        link: None,
        evidence: "USB id 0403:6015, nothing at 57600, 115200, 921600 or 460800 baud".into(),
        usb: Some(UsbId {
            vendor_id: 0x0403,
            product_id: 0x6015,
            serial_number: None,
        }),
        device_name: Some("FTDI serial adapter".into()),
        vehicle: None,
        reason: Some(FindReason::NoMavlink),
        detail: None,
        seen_us: NOW,
    }]);
    engine.find_round_done(FindScope::Nearby);
    let answer = drain(&mut ui)
        .into_iter()
        .find(|f| f["id"] == 2)
        .expect("the answer");
    assert_eq!(answer["result"]["finds"][0]["reason"], "no-mavlink");
    assert_eq!(
        probes.0.lock().unwrap().last().cloned().flatten(),
        None,
        "done: nothing held"
    );

    // An agent may look nearby, but searching the network asks first.
    let mut agent = connect(&mut engine, "agent", Trust::LocalUser);
    let nearby = send(
        &mut engine,
        &mut agent,
        "find/start",
        json!({ "scope": "nearby" }),
    )
    .unwrap();
    assert!(nearby.get("result").is_some(), "{nearby}");
    let network = send(
        &mut engine,
        &mut agent,
        "find/vehicles",
        json!({ "scope": "network" }),
    );
    assert!(network.is_none(), "held for an approval");
    let approval = engine
        .state()
        .approvals
        .values()
        .next()
        .expect("an approval");
    assert!(
        approval.summary.contains("192.168.1.0/24"),
        "{}",
        approval.summary
    );
}

#[test]
fn a_flight_controller_plugged_in_is_announced_unless_a_link_owns_it() {
    let (mut engine, _, _) = engine();
    let mut ui = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let cube = UsbId {
        vendor_id: 0x2dae,
        product_id: 0x1016,
        serial_number: Some("ABC".into()),
    };
    engine.usb_plugged("/dev/cu.usbmodem1101", &cube, "Cube Orange");
    let plugged: Vec<_> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.title == "Flight controller plugged in")
        .cloned()
        .collect();
    assert_eq!(plugged.len(), 1);
    assert_eq!(plugged[0].body.as_deref(), Some("Cube Orange"));

    let added = send(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "serial", "device": "/dev/cu.usbmodem1101", "baud": 115_200,
            "usb": { "vendorId": 0x2dae, "productId": 0x1016, "serialNumber": "ABC" } } }),
    )
    .unwrap();
    assert!(added.get("result").is_some(), "{added}");
    // Unplugged and plugged back: the link has it, so no notice.
    engine.usb_plugged("/dev/cu.usbmodem2201", &cube, "Cube Orange");
    assert_eq!(
        engine
            .state()
            .notifications
            .values()
            .filter(|n| n.title == "Flight controller plugged in")
            .count(),
        1
    );
}

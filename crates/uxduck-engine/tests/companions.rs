//! Companion computers in the vehicle profile (S296, ADR-0049 O7, O8):
//! added by host name with the next free onboard id, a private key kept in
//! the credential store and never sent, checked through a fake port in
//! place of the daemon's SSH, a refused host key keeping the one pinned,
//! and removing one forgetting its key. The profile survives a restart.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::companions::{CheckAnswer, CompanionOrder, CompanionPort, Found};
use uxduck_engine::runtime::{
    Clock, CommandOrder, Credentials, Forget, LinkPort, NoAgent, NoSystem, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        5_000_000
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

struct NoLinks;

impl LinkPort for NoLinks {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
}

#[derive(Clone, Default)]
struct Keys(Arc<Mutex<BTreeMap<String, String>>>);

impl Credentials for Keys {
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let mut all = self.0.lock().unwrap();
        if value.is_empty() {
            all.remove(name);
        } else {
            all.insert(name.to_owned(), value.to_owned());
        }
        Ok(())
    }
    fn has(&self, name: &str) -> bool {
        self.0.lock().unwrap().contains_key(name)
    }
}

#[derive(Clone, Default)]
struct Ssh(Arc<Mutex<Vec<CompanionOrder>>>);

impl CompanionPort for Ssh {
    fn request(&mut self, order: CompanionOrder) {
        self.0.lock().unwrap().push(order);
    }
}

fn engine(state: State, keys: &Keys) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:45000/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(NoAgent),
        credentials: Box::new(keys.clone()),
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(7)),
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

fn connect(engine: &mut Engine) -> Client {
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
            "clientInfo": { "name": "ui", "version": "0.1.0", "kind": "operator-ui" },
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

fn ok(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    let reply = call(engine, client, method, params);
    assert!(reply.get("result").is_some(), "{method}: {reply}");
    reply["result"].clone()
}

fn refused(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> String {
    let reply = call(engine, client, method, params);
    reply["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("{method} was accepted: {reply}"))
        .to_owned()
}

fn heard(engine: &mut Engine) {
    engine.vehicle_observed(Observation {
        connections: None,
        id: "v".into(),
        aliases: vec![],
        link_id: "l".into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
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

const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";

#[test]
fn computers_are_added_with_free_ids_and_their_keys_are_kept_apart() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-vision.local", "user": "pi" }),
    );
    let first = &vehicle["companions"][0];
    assert_eq!(first["name"], "pi-vision.local");
    assert_eq!(first["componentId"], 191);
    assert_eq!(first["role"], "report");
    assert!(first.get("key").is_none());

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-fc.local", "port": 2222, "role": "forward", "name": "Flight Pi", "privateKey": KEY }),
    );
    let second = vehicle["companions"][1].clone();
    assert_eq!(second["componentId"], 192);
    assert_eq!(second["key"], true);
    assert!(
        !vehicle.to_string().contains("OPENSSH PRIVATE KEY"),
        "the key is never sent"
    );
    let key_name = format!("companion-key-{}", second["id"].as_str().unwrap());
    assert!(keys.has(&key_name));

    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "PI-VISION.local" })
        )
        .contains("already")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "-oProxyCommand=sh" })
        )
        .contains("not a host name")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi-2.local", "role": "forward" })
        )
        .contains("already has a computer forwarding")
    );
    assert!(
        refused(
            &mut engine,
            &mut ui,
            "computer/add",
            json!({ "vehicleId": "v", "host": "pi-3.local", "privateKey": "hunter2" })
        )
        .contains("not a private key")
    );

    // The profile survives a restart.
    let restarted = engine.state().clone();
    let again = self::engine(
        serde_json::from_value(serde_json::to_value(&restarted).unwrap()).unwrap(),
        &keys,
    );
    assert_eq!(
        again.state().vehicles["v"].companions,
        restarted.vehicles["v"].companions
    );

    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/remove",
        json!({ "vehicleId": "v", "computerId": second["id"] }),
    );
    assert_eq!(vehicle["companions"].as_array().unwrap().len(), 1);
    assert!(!keys.has(&key_name), "its key is forgotten with it");
}

#[test]
fn a_check_reports_what_the_computer_runs_and_keeps_its_pinned_key() {
    let keys = Keys::default();
    let mut engine = engine(State::default(), &keys);
    let mut ui = connect(&mut engine);
    heard(&mut engine);
    let vehicle = ok(
        &mut engine,
        &mut ui,
        "computer/add",
        json!({ "vehicleId": "v", "host": "pi-vision.local", "user": "pi" }),
    );
    let id = vehicle["companions"][0]["id"].as_str().unwrap().to_owned();
    let params = json!({ "vehicleId": "v", "computerId": id });

    // Without SSH (a browser), a check is refused with where to do it.
    assert!(
        refused(&mut engine, &mut ui, "computer/check", params.clone()).contains("UXDuck Desktop")
    );

    let ssh = Ssh::default();
    engine.set_companions(Box::new(ssh.clone()));
    let vehicle = ok(&mut engine, &mut ui, "computer/check", params.clone());
    assert_eq!(vehicle["companions"][0]["check"]["state"], "checking");
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { vehicle_id, computer, password: None }] if vehicle_id == "v" && computer.host == "pi-vision.local")
    );

    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Ok(Found {
            os: Some("Debian GNU/Linux 12 (bookworm)".into()),
            arch: Some("aarch64".into()),
            uxduck: None,
            host_key: Some("SHA256:first".into()),
        }),
    });
    let check = engine.state().vehicles["v"].companions[0]
        .check
        .clone()
        .unwrap();
    assert_eq!(check.os.as_deref(), Some("Debian GNU/Linux 12 (bookworm)"));
    assert_eq!(check.uxduck, None);
    assert_eq!(check.host_key.as_deref(), Some("SHA256:first"));

    // A password goes to the host for this check alone, never onto the vehicle.
    let with_password = json!({ "vehicleId": "v", "computerId": id, "password": "raspberry" });
    let vehicle = ok(&mut engine, &mut ui, "computer/check", with_password);
    assert!(!vehicle.to_string().contains("raspberry"));
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { password: Some(p), .. }] if p.0 == "raspberry" && !format!("{p:?}").contains("raspberry"))
    );
    assert!(!format!("{:?}", engine.state()).contains("raspberry"));

    // A refused change keeps the key it pinned, so the next check refuses too.
    ok(&mut engine, &mut ui, "computer/check", params.clone());
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert!(
        matches!(&orders[..], [CompanionOrder::Check { computer, .. }] if computer.check.as_ref().and_then(|c| c.host_key.as_deref()) == Some("SHA256:first"))
    );
    engine.companion_checked(CheckAnswer {
        vehicle_id: "v".into(),
        computer_id: id.clone(),
        result: Err("pi-vision.local's host key changed".into()),
    });
    let check = engine.state().vehicles["v"].companions[0]
        .check
        .clone()
        .unwrap();
    assert!(check.problem.unwrap().contains("changed"));
    assert_eq!(check.host_key.as_deref(), Some("SHA256:first"));

    ok(&mut engine, &mut ui, "computer/remove", params);
    let orders = std::mem::take(&mut *ssh.0.lock().unwrap());
    assert_eq!(orders, [CompanionOrder::Forget { computer_id: id }]);
}

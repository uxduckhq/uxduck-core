//! Search from the input (S22): ranking, kinds, saved places per operator.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
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
use uxduck_core::domain::parameter::{ParamKind, Parameter};
use uxduck_core::domain::settings::StorageCategory;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Deleted, Forget, LinkPort, MemoryCredentials, NoAgent,
    Ports, Random, SerialPortInfo, StorageReport, System,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

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

#[derive(Clone, Default)]
struct Orders;

impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn route_controller(&mut self, _: &str, _: Option<ControlRoute>) {}
}

/// A disk with a given amount free, recording what it was asked to delete.
#[derive(Clone, Default)]
struct Disk(Arc<Mutex<DiskState>>);

#[derive(Default)]
struct DiskState {
    free: u64,
    deletions: Vec<(StorageCategory, u64)>,
}

impl System for Disk {
    fn check_udp_bind(&self, _: &str, _: u16) -> Result<(), String> {
        Ok(())
    }
    fn is_local_address(&self, _: &str) -> bool {
        true
    }
    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        Vec::new()
    }
    fn wake_lock(&mut self, _: bool) {}
    fn storage(&self) -> StorageReport {
        StorageReport {
            data_dir: "/data".into(),
            categories: vec![(StorageCategory::Tlogs, 5_000_000, 3)],
            free_bytes: Some(self.0.lock().unwrap().free),
            total_bytes: Some(500_000_000_000),
        }
    }
    fn delete_older_than(&mut self, category: StorageCategory, cutoff_us: u64) -> Deleted {
        self.0.lock().unwrap().deletions.push((category, cutoff_us));
        Deleted {
            files: 2,
            bytes: 3_000_000,
        }
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
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

/// Sends a request; its reply if it came at once.
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

fn engine(state: State) -> Engine {
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
        links: Box::new(Orders),
        system: Box::new(Disk::default()),
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

fn titles(reply: &Value) -> Vec<(String, String)> {
    reply["result"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["kind"].as_str().unwrap().to_owned(),
                h["title"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn search_ranks_by_how_well_the_title_matches_across_kinds() {
    let mut state = State::default();
    for (name, value) in [
        ("RTL_ALT", 1500.0),
        ("WPNAV_SPEED", 1000.0),
        ("ARSPD_RATIO", 2.0),
    ] {
        let parameter = Parameter {
            vehicle_id: "v".into(),
            name: name.into(),
            value,
            kind: ParamKind::Real32,
            default: None,
        };
        state.parameters.insert(parameter.id(), parameter);
    }
    let mut engine = engine(state);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    engine.vehicle_observed(Observation {
        id: "v".into(),
        aliases: Vec::new(),
        link_id: link["result"]["id"].as_str().unwrap().into(),
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
    call(
        &mut engine,
        &mut ui,
        "thread/create",
        json!({ "title": "Speed check before RTL" }),
    );

    let speed = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "speed" }),
    );
    assert_eq!(
        titles(&speed),
        [
            ("chat".into(), "Speed check before RTL".into()),
            ("parameter".into(), "WPNAV_SPEED".into()),
        ],
        "a title starting with it first"
    );
    let copter = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "alp" }),
    );
    assert_eq!(titles(&copter)[0], ("vehicle".into(), "Alpha".into()));
    let only = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "rtl", "kinds": ["parameter"] }),
    );
    assert_eq!(titles(&only), [("parameter".into(), "RTL_ALT".into())]);
    let settings = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "storage" }),
    );
    assert_eq!(titles(&settings), [("setting".into(), "Storage".into())]);
    let nothing = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "  " }),
    );
    assert!(titles(&nothing).is_empty());
}

#[test]
fn saved_places_belong_to_their_operator_and_are_checked() {
    let mut engine = engine(State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    let ben = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ben" }),
    );
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ana["result"]["id"] }),
    );
    let off_globe = call(
        &mut engine,
        &mut ui,
        "place/save",
        json!({ "name": "Nowhere", "latitudeDeg": 91, "longitudeDeg": 0 }),
    );
    assert_eq!(off_globe["error"]["data"]["kind"], "invalid-value");
    let saved = call(
        &mut engine,
        &mut ui,
        "place/save",
        json!({ "name": "Home field", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    assert_eq!(saved["result"]["operatorId"], ana["result"]["id"]);
    let found = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "home" }),
    );
    assert_eq!(found["result"]["hits"][0]["place"]["latitudeDeg"], -35.36);
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ben["result"]["id"] }),
    );
    let theirs = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "home" }),
    );
    assert!(titles(&theirs).is_empty(), "Ana's places are Ana's");
    let removed = call(
        &mut engine,
        &mut ui,
        "place/remove",
        json!({ "id": saved["result"]["id"] }),
    );
    assert_eq!(removed["error"]["data"]["kind"], "not-found");
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ana["result"]["id"] }),
    );
    let removed = call(
        &mut engine,
        &mut ui,
        "place/remove",
        json!({ "id": saved["result"]["id"] }),
    );
    assert!(removed.get("result").is_some());
    let _ = StorageCategory::Tlogs;
}

#[test]
fn renaming_a_place_keeps_its_identity_and_rejects_stale_or_foreign_edits() {
    let mut engine = engine(State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    );
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ana["result"]["id"] }),
    );
    let saved = call(
        &mut engine,
        &mut ui,
        "place/save",
        json!({ "name": "Field", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    let params = json!({ "id": saved["result"]["id"], "expectedVersion": 1, "name": "Home field" });
    let renamed = call(&mut engine, &mut ui, "place/update", params.clone());
    assert_eq!(renamed["result"]["id"], saved["result"]["id"]);
    assert_eq!(
        renamed["result"]["createdAtUs"],
        saved["result"]["createdAtUs"]
    );
    assert_eq!(
        renamed["result"]["operatorId"],
        saved["result"]["operatorId"]
    );
    assert_eq!(renamed["result"]["version"], 2);
    assert_eq!(engine.state().places.len(), 1);
    let stale = call(&mut engine, &mut ui, "place/update", params.clone());
    assert_eq!(stale["error"]["data"]["kind"], "version-conflict");
    assert_eq!(stale["error"]["data"]["currentVersion"], 2);
    let mut missing_version = params.clone();
    missing_version
        .as_object_mut()
        .unwrap()
        .remove("expectedVersion");
    let refused = call(&mut engine, &mut ui, "place/update", missing_version);
    assert_eq!(refused["error"]["data"]["kind"], "invalid-params");
    let found = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "home field" }),
    );
    assert_eq!(found["result"]["hits"][0]["title"], "Home field");
    let ben = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ben" }),
    );
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ben["result"]["id"] }),
    );
    let mut foreign = params;
    foreign["expectedVersion"] = json!(2);
    let refused = call(&mut engine, &mut ui, "place/update", foreign);
    assert_eq!(refused["error"]["data"]["kind"], "not-found");
    assert_eq!(engine.state().places.len(), 1);
}

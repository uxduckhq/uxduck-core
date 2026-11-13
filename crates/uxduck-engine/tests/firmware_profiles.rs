//! A fork's firmware profiles (S254, ADR-0044 F12): one URL naming the
//! fork's parameter descriptions and dialects, read through the host, bound
//! to the profile's commit, fetched again on a refresh, and kept when a
//! refresh fails.

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
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::inspect::InspectDirection;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::mavlink_inspect::Inspector;
use uxduck_engine::param_definitions::{DefinitionsArrived, DefinitionsOrder, DefinitionsPort};
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};
use uxduck_mavlink::framing::Header;

const JSON: &str = include_str!("../../uxduck-contracts/fixtures/pdef/copter.pdef.json");
const ACME: &str = include_str!("../../uxduck-mavlink/fixtures/acme_fork.xml");
const URL: &str = "https://ci.acme.example/builds/r12/profile.json";
const COMMIT: &str = "2a3dc4b7bf9e";

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

/// What the engine asked the host for.
#[derive(Clone, Default)]
struct Asked(Arc<Mutex<Vec<DefinitionsOrder>>>);

impl Asked {
    fn take(&self) -> Vec<DefinitionsOrder> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl DefinitionsPort for Asked {
    fn request(&mut self, order: DefinitionsOrder) {
        self.0.lock().unwrap().push(order);
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine) -> Client {
    connect_as(engine, "operator-ui")
}

fn connect_as(engine: &mut Engine, kind: &str) -> Client {
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
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            return frame;
        }
        client.queued.push_back(frame);
    }
    panic!("no reply to {method}");
}

fn engine(port: Option<&Asked>) -> (Engine, Client) {
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
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    );
    if let Some(port) = port {
        engine.set_definitions(Box::new(port.clone()));
    }
    let ui = connect(&mut engine);
    (engine, ui)
}

/// A vehicle built from `commit`.
fn heard(engine: &mut Engine, id: &str, commit: &str) {
    engine.vehicle_observed(Observation {
        connections: None,
        id: id.into(),
        aliases: Vec::new(),
        link_id: "lk".into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: Some("4.5.7".into()),
        firmware_commit: Some(commit.into()),
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
}

fn profile_text() -> String {
    json!({
        "uxduck": "firmware/1",
        "label": "Acme r12",
        "firmware": "Copter",
        "version": "4.5.7-acme12",
        "commit": COMMIT,
        "parameters": "copter.pdef.json",
        "dialects": ["../mavlink/acme.xml"]
    })
    .to_string()
}

/// `ACME_STATUS`: time, 12.5 V, surveying, armed and hot, "probe", 21.5 °C.
fn acme_status() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&7_u64.to_le_bytes());
    payload.extend_from_slice(&12.5_f32.to_le_bytes());
    payload.push(3);
    payload.push(5);
    payload.extend_from_slice(b"probe\0\0\0");
    payload.extend_from_slice(&2150_i16.to_le_bytes());
    payload
}

fn inspected_name(id: &str) -> Value {
    let mut table = Inspector::default();
    table.record(
        id,
        InspectDirection::Received,
        Header {
            sequence: 0,
            system_id: 1,
            component_id: 1,
            message_id: 43001,
        },
        &acme_status(),
        10,
    );
    let seen = table.read(&[id.to_owned()], 20).decode(None);
    serde_json::to_value(&seen.messages[0]).unwrap()["name"].clone()
}

fn fetched(engine: &mut Engine, id: &str, result: Result<&str, &str>) {
    engine.definitions_arrived(DefinitionsArrived::Fetched {
        id: id.to_owned(),
        result: result.map(str::to_owned).map_err(str::to_owned),
    });
}

#[test]
fn a_profile_brings_its_parameters_and_dialect_for_its_commit() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "fp-acme", "2a3dc4b7");
    heard(&mut engine, "fp-other", "0123abcd");
    asked.take();

    let plain = call(
        &mut engine,
        &mut ui,
        "firmware/add",
        json!({ "url": "http://ci.acme.example/profile.json" }),
    );
    assert_eq!(plain["error"]["data"]["kind"], "invalid-value", "{plain}");

    let added = call(&mut engine, &mut ui, "firmware/add", json!({ "url": URL }));
    let profile = &added["result"];
    assert_eq!(profile["label"], "ci.acme.example", "{added}");
    let id = profile["id"].as_str().unwrap().to_owned();
    assert!(matches!(&asked.take()[..],
        [DefinitionsOrder::FetchProfile { id: asked_for, url }] if *asked_for == id && url == URL));
    let again = call(&mut engine, &mut ui, "firmware/add", json!({ "url": URL }));
    assert_eq!(again["error"]["data"]["kind"], "invalid-value", "{again}");

    // The profile arrives: its set is made and fetched, its dialect asked for.
    fetched(&mut engine, &id, Ok(&profile_text()));
    let own_id = format!("{id}-parameters");
    let dialect_file = format!("{id}#1#0");
    let orders = asked.take();
    assert!(
        orders
            .iter()
            .any(|o| matches!(o, DefinitionsOrder::FetchOwn { id: own, url }
        if *own == own_id && url == "https://ci.acme.example/builds/r12/copter.pdef.json")),
        "{orders:?}"
    );
    assert!(
        orders.iter().any(
            |o| matches!(o, DefinitionsOrder::FetchProfile { id: file, url }
        if *file == dialect_file && url == "https://ci.acme.example/builds/mavlink/acme.xml")
        ),
        "{orders:?}"
    );
    let own = &engine.state().own_sets[&own_id];
    assert_eq!(own.commit.as_deref(), Some(COMMIT));
    assert_eq!(own.profile_id.as_deref(), Some(id.as_str()));
    assert_eq!(own.label, "Acme r12");

    fetched(&mut engine, &own_id, Ok(JSON));
    assert_eq!(
        engine.state().param_definitions["fp-acme"]
            .own_set_id
            .as_deref(),
        Some(own_id.as_str()),
        "the vehicle built from the commit is described by it"
    );

    assert_eq!(inspected_name("fp-acme"), "#43001");
    fetched(&mut engine, &dialect_file, Ok(ACME));
    assert_eq!(inspected_name("fp-acme"), "ACME_STATUS");
    assert_eq!(inspected_name("fp-other"), "#43001", "another commit");

    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    let shown = &listed["result"]["profiles"][0];
    assert_eq!(shown["label"], "Acme r12", "{listed}");
    assert_eq!(shown["commit"], COMMIT);
    assert_eq!(shown["ownSetId"], own_id.as_str());
    let dialect_id = shown["dialectId"].as_str().unwrap().to_owned();
    assert!(shown.get("problem").is_none());
    let dialects = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/list",
        json!({ "vehicleId": "fp-acme" }),
    );
    assert_eq!(
        dialects["result"]["dialects"][0]["profileId"],
        id.as_str(),
        "{dialects}"
    );

    // Another vehicle may choose its dialect.
    let chosen = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "fp-other", "choice": { "type": "chosen", "ids": [dialect_id] } }),
    );
    assert!(chosen.get("result").is_some(), "{chosen}");
    assert_eq!(inspected_name("fp-other"), "ACME_STATUS");
    call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "fp-other", "choice": { "type": "automatic" } }),
    );
    assert_eq!(inspected_name("fp-other"), "#43001");

    // Its parts go with it, not alone.
    for (method, part) in [
        ("mavlink/dialects/remove", &dialect_id),
        ("param/definitions/remove", &own_id),
    ] {
        let refused = call(&mut engine, &mut ui, method, json!({ "id": part }));
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("Acme r12"), "{method}: {refused}");
    }

    // A refresh asks again; a failure keeps both and says why, once.
    call(
        &mut engine,
        &mut ui,
        "firmware/list",
        json!({ "refresh": true }),
    );
    assert!(
        asked
            .take()
            .iter()
            .any(|o| matches!(o, DefinitionsOrder::FetchProfile { id: again, .. } if *again == id))
    );
    let dialect_before = engine.state().dialects[&dialect_id].clone();
    for _ in 0..2 {
        fetched(
            &mut engine,
            &id,
            Err("https://ci.acme.example answered 503"),
        );
    }
    let condition = format!("firmware-profile:{id}");
    let notices: Vec<_> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.condition.as_deref() == Some(condition.as_str()))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].title,
        "Couldn't refresh the firmware profile Acme r12"
    );
    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    assert_eq!(
        listed["result"]["profiles"][0]["problem"],
        "https://ci.acme.example answered 503"
    );
    assert_eq!(
        inspected_name("fp-acme"),
        "ACME_STATUS",
        "the last copy stays"
    );
    assert!(engine.state().own_sets.contains_key(&own_id));

    // Read again, the same: nothing new journaled for the dialect.
    fetched(&mut engine, &id, Ok(&profile_text()));
    fetched(&mut engine, &format!("{id}#2#0"), Ok(ACME));
    assert_eq!(engine.state().dialects[&dialect_id], dialect_before);
    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    assert!(
        listed["result"]["profiles"][0].get("problem").is_none(),
        "{listed}"
    );

    let removed = call(&mut engine, &mut ui, "firmware/remove", json!({ "id": id }));
    assert!(removed.get("result").is_some(), "{removed}");
    assert!(engine.state().firmware_profiles.is_empty());
    assert!(!engine.state().own_sets.contains_key(&own_id));
    assert!(!engine.state().dialects.contains_key(&dialect_id));
    assert_eq!(inspected_name("fp-acme"), "#43001");
}

#[test]
fn a_profile_without_a_commit_may_only_add_messages() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    let added = call(&mut engine, &mut ui, "firmware/add", json!({ "url": URL }));
    let id = added["result"]["id"].as_str().unwrap().to_owned();
    let text = json!({
        "uxduck": "firmware/1", "label": "Acme any", "firmware": "ArduCopter",
        "dialects": ["acme.xml"]
    })
    .to_string();
    fetched(&mut engine, &id, Ok(&text));
    fetched(&mut engine, &format!("{id}#1#0"), Ok(ACME));
    assert!(engine.state().dialects.is_empty());
    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    let problem = listed["result"]["profiles"][0]["problem"]
        .as_str()
        .unwrap_or_default();
    assert!(problem.contains("can't be for every vehicle"), "{listed}");
    assert!(
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "Couldn't refresh the firmware profile Acme any")
    );
}

#[test]
fn a_named_profile_keeps_its_name_over_its_own_label_and_can_be_renamed() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    let long = call(
        &mut engine,
        &mut ui,
        "firmware/add",
        json!({ "url": URL, "name": "x".repeat(65) }),
    );
    assert_eq!(long["error"]["data"]["kind"], "invalid-value", "{long}");

    let added = call(
        &mut engine,
        &mut ui,
        "firmware/add",
        json!({ "url": URL, "name": "  Our fork  " }),
    );
    assert_eq!(added["result"]["label"], "Our fork", "{added}");
    let id = added["result"]["id"].as_str().unwrap().to_owned();
    fetched(&mut engine, &id, Ok(&profile_text()));
    fetched(&mut engine, &format!("{id}#1#0"), Ok(ACME));

    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    assert_eq!(
        listed["result"]["profiles"][0]["label"], "Our fork",
        "{listed}"
    );
    assert_eq!(
        engine.state().own_sets[&format!("{id}-parameters")].label,
        "Our fork"
    );
    assert_eq!(
        engine.state().dialects[&format!("{id}-dialect")].label,
        "Our fork"
    );

    let empty = call(
        &mut engine,
        &mut ui,
        "firmware/rename",
        json!({ "id": id, "name": "  " }),
    );
    assert_eq!(empty["error"]["data"]["kind"], "invalid-value", "{empty}");
    let missing = call(
        &mut engine,
        &mut ui,
        "firmware/rename",
        json!({ "id": "firmware-nope", "name": "X" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found", "{missing}");

    let renamed = call(
        &mut engine,
        &mut ui,
        "firmware/rename",
        json!({ "id": id, "name": "Acme field build" }),
    );
    assert_eq!(renamed["result"]["label"], "Acme field build", "{renamed}");
    assert_eq!(
        engine.state().own_sets[&format!("{id}-parameters")].label,
        "Acme field build"
    );
    assert_eq!(
        engine.state().dialects[&format!("{id}-dialect")].label,
        "Acme field build"
    );

    // Read again, it keeps the new name.
    fetched(&mut engine, &id, Ok(&profile_text()));
    fetched(&mut engine, &format!("{id}#2#0"), Ok(ACME));
    let listed = call(&mut engine, &mut ui, "firmware/list", json!({}));
    assert_eq!(
        listed["result"]["profiles"][0]["label"], "Acme field build",
        "{listed}"
    );
    assert_eq!(
        engine.state().dialects[&format!("{id}-dialect")].label,
        "Acme field build"
    );
}

#[test]
fn without_a_way_to_fetch_a_profile_is_refused() {
    let (mut engine, mut ui) = engine(None);
    let refused = call(&mut engine, &mut ui, "firmware/add", json!({ "url": URL }));
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
    assert!(engine.state().firmware_profiles.is_empty());
}

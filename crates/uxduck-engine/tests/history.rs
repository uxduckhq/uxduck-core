//! A vehicle's history (S69, ADR-0022): notes with their earlier versions
//! and deletions kept, events UXDuck records by itself, and the agent's
//! note waiting for the operator.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::history::EntryKind;
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::transfers::Reported;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const DAY: u64 = 86_400_000_000;
const NOW: u64 = 1_790_000_000_000_000;

/// A microsecond later at every look, as a real clock moves.
#[derive(Default)]
struct Ticking(std::sync::atomic::AtomicU64);

impl Clock for Ticking {
    fn now_us(&self) -> u64 {
        NOW + self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

/// A ticking clock the test also moves forward.
#[derive(Clone, Default)]
struct Movable {
    moved_us: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ticks: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Movable {
    fn forward(&self, us: u64) {
        self.moved_us
            .fetch_add(us, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for Movable {
    fn now_us(&self) -> u64 {
        NOW + self.moved_us.load(std::sync::atomic::Ordering::SeqCst)
            + self.ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
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

fn engine() -> Engine {
    engine_with(Box::new(Ticking::default()))
}

fn engine_with(clock: Box<dyn Clock>) -> Engine {
    engine_running(clock, DaemonCapabilities::default())
}

fn engine_running(clock: Box<dyn Clock>, capabilities: DaemonCapabilities) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities,
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
    Engine::new(
        config,
        clock,
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    )
}

fn heard(engine: &mut Engine, firmware: &str, vehicle_type: &str) {
    heard_on(engine, "v", "lk", firmware, vehicle_type);
}

fn heard_on(engine: &mut Engine, id: &str, link_id: &str, firmware: &str, vehicle_type: &str) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: Vec::new(),
        link_id: link_id.into(),
        peer: "p".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: vehicle_type.into(),
        uid: Some("uid-1".into()),
        firmware: Some(firmware.into()),
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
}

fn events(engine: &Engine) -> Vec<String> {
    let mut all: Vec<_> = engine.state().history.values().collect();
    all.sort_by_key(|e| e.written_at_us);
    all.iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Event { event } => Some(event.describe()),
            EntryKind::Note { .. } => None,
        })
        .collect()
}

#[test]
fn a_note_is_backdated_edited_and_deleted_and_keeps_every_version() {
    let mut engine = engine();
    heard(&mut engine, "4.5.7", "Quadrotor");
    let mut ui = connect(&mut engine, "operator-ui");
    let added = call(
        &mut engine,
        &mut ui,
        "vehicle/addNote",
        json!({ "vehicleId": "v", "text": "Replaced motor 3", "category": "repair",
                "happenedAtUs": NOW - DAY }),
    );
    let note = &added["result"];
    assert_eq!(note["happenedAtUs"], NOW - DAY);
    assert!(
        note["writtenAtUs"].as_u64().unwrap() >= NOW,
        "written now, whatever it is dated"
    );
    assert_eq!(note["author"]["clientName"], "operator-ui");
    let id = note["id"].as_str().unwrap().to_owned();

    let later = call(
        &mut engine,
        &mut ui,
        "vehicle/addNote",
        json!({ "vehicleId": "v", "text": "Later", "happenedAtUs": NOW + 3_600_000_000 }),
    );
    assert_eq!(
        later["error"]["data"]["kind"], "invalid-value",
        "never later than now"
    );

    let edited = call(
        &mut engine,
        &mut ui,
        "vehicle/editNote",
        json!({ "id": id, "expectedVersion": 1, "text": "Replaced motors 3 and 4" }),
    );
    assert_eq!(edited["result"]["kind"]["text"], "Replaced motors 3 and 4");
    assert_eq!(
        edited["result"]["kind"]["earlier"][0]["text"],
        "Replaced motor 3"
    );
    assert_eq!(edited["result"]["kind"]["category"], "repair", "unchanged");
    let stale = call(
        &mut engine,
        &mut ui,
        "vehicle/editNote",
        json!({ "id": id, "expectedVersion": 1, "text": "x" }),
    );
    assert_eq!(stale["error"]["data"]["kind"], "version-conflict");

    let deleted = call(
        &mut engine,
        &mut ui,
        "vehicle/deleteNote",
        json!({ "id": id }),
    );
    assert_eq!(
        deleted["result"]["deleted"]["by"]["clientName"],
        "operator-ui"
    );
    let history = call(
        &mut engine,
        &mut ui,
        "vehicle/history",
        json!({ "kind": "note" }),
    );
    assert_eq!(history["result"]["total"], 0, "hidden unless asked");
    let all = call(
        &mut engine,
        &mut ui,
        "vehicle/history",
        json!({ "kind": "note", "includeDeleted": true }),
    );
    assert_eq!(
        all["result"]["entries"][0]["id"],
        id.as_str(),
        "kept, marked"
    );
    let again = call(
        &mut engine,
        &mut ui,
        "vehicle/editNote",
        json!({ "id": id, "expectedVersion": 3, "text": "back" }),
    );
    assert_eq!(again["error"]["data"]["kind"], "invalid-value");
}

#[test]
fn changes_of_one_parameter_in_a_sitting_fold_into_one_entry() {
    const MINUTE: u64 = 60_000_000;
    let clock = Movable::default();
    let mut engine = engine_with(Box::new(clock.clone()));
    heard(&mut engine, "4.5.7", "Plane");
    let flip = |engine: &mut Engine, value: f64| {
        engine.parameter_reported(
            "v",
            Reported {
                name: "ARSPD_USE".into(),
                value,
                kind: ParamKind::Int8,
            },
            false,
        );
    };
    flip(&mut engine, 1.0);
    // Ten minutes without a change end a sitting: the next one is its own entry.
    clock.forward(11 * MINUTE);
    flip(&mut engine, 0.0);
    clock.forward(5_000_000);
    flip(&mut engine, 1.0);
    clock.forward(5_000_000);
    flip(&mut engine, 0.0);
    clock.forward(3 * MINUTE);
    flip(&mut engine, 1.0);
    assert_eq!(
        events(&engine),
        [
            "First seen, as a Plane",
            "ARSPD_USE set to 1",
            "ARSPD_USE changed 4 times over 3 minutes and is back at 1",
        ],
        "the first value before, the latest after, how many and over how long"
    );
    let folded = engine
        .state()
        .history
        .values()
        .find(|e| e.version == 4)
        .expect("folded three times");
    assert!(
        (NOW + 11 * MINUTE..NOW + 11 * MINUTE + 1_000).contains(&folded.happened_at_us),
        "dated by its first change, not its last: {}",
        folded.happened_at_us
    );

    let mut ui = connect(&mut engine, "operator-ui");
    let history = call(
        &mut engine,
        &mut ui,
        "param/history",
        json!({ "vehicleId": "v" }),
    );
    let changes = history["result"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["changes"], 4);
    assert!(changes[0]["spanUs"].as_u64().unwrap() >= 3 * MINUTE + 10_000_000);
    assert_eq!(
        changes[1]["changes"],
        Value::Null,
        "one change says nothing of folding"
    );
}

#[test]
fn uxduck_records_first_seen_firmware_type_and_name_changes() {
    let mut engine = engine();
    heard(&mut engine, "4.5.7", "Quadrotor");
    heard(&mut engine, "4.5.7", "Quadrotor");
    heard(&mut engine, "4.6.0", "Quadrotor");
    heard(&mut engine, "4.6.0", "Hexarotor");
    let mut ui = connect(&mut engine, "operator-ui");
    call(
        &mut engine,
        &mut ui,
        "vehicle/save",
        json!({ "id": "v", "name": "Survey" }),
    );
    assert_eq!(
        events(&engine),
        [
            "First seen, as a Quadrotor",
            "Firmware 4.5.7 → 4.6.0",
            "Type changed from Quadrotor to Hexarotor",
            "Renamed from “Alpha” to “Survey”",
        ]
    );
    let history = call(
        &mut engine,
        &mut ui,
        "vehicle/history",
        json!({ "text": "firmware" }),
    );
    assert_eq!(history["result"]["total"], 1);
    assert_eq!(history["result"]["vehicleName"], "Survey");
    let event = &history["result"]["entries"][0]["kind"]["event"];
    assert_eq!(event["type"], "firmware-changed");
    assert_eq!(event["to"], "4.6.0");
    let entry = engine.state().history.values().next().unwrap();
    assert!(entry.local, "an unsaved vehicle's history stays here");
}

#[test]
fn an_agents_note_waits_for_the_operator_and_says_who_it_was_for() {
    let mut engine = engine();
    heard(&mut engine, "4.5.7", "Quadrotor");
    let mut agent = connect(&mut engine, "agent");
    let read = call(&mut engine, &mut agent, "vehicle/history", json!({}));
    assert_eq!(read["result"]["total"], 1, "reading needs no one: {read}");
    assert!(
        send(
            &mut engine,
            &mut agent,
            "vehicle/addNote",
            json!({ "vehicleId": "v", "text": "Props swapped to 10 inch" })
        )
        .is_none(),
        "held for an operator"
    );
    let summaries: Vec<&str> = engine
        .state()
        .approvals
        .values()
        .map(|a| a.summary.as_str())
        .collect();
    assert_eq!(
        summaries,
        ["note “Props swapped to 10 inch” in Alpha's history"]
    );
}

#[test]
fn search_finds_a_note_by_its_words_and_not_once_deleted() {
    let mut engine = engine();
    heard(&mut engine, "4.5.7", "Quadrotor");
    let mut ui = connect(&mut engine, "operator-ui");
    let note = call(
        &mut engine,
        &mut ui,
        "vehicle/addNote",
        json!({ "vehicleId": "v", "text": "Replaced motor 3" }),
    );
    let found = |engine: &mut Engine, ui: &mut Client| {
        call(
            engine,
            ui,
            "search/query",
            json!({ "query": "motor", "kinds": ["note"] }),
        )["result"]["hits"]
            .clone()
    };
    let hits = found(&mut engine, &mut ui);
    assert_eq!(hits[0]["title"], "Replaced motor 3");
    assert_eq!(hits[0]["subtitle"], "History · Alpha");
    assert_eq!(hits[0]["vehicleId"], "v");
    call(
        &mut engine,
        &mut ui,
        "vehicle/deleteNote",
        json!({ "id": note["result"]["id"] }),
    );
    assert_eq!(found(&mut engine, &mut ui), json!([]));
}

#[test]
fn a_known_vehicle_heard_again_after_a_restart_is_not_seen_first_again() {
    let mut engine = engine();
    heard(&mut engine, "4.5.7", "Quadrotor");
    // After a restart: a provisional id until the UID comes, then the
    // lasting one with the provisional as an alias.
    let observe = |engine: &mut Engine, id: &str, aliases: Vec<String>, uid: Option<&str>| {
        engine.vehicle_observed(Observation {
            id: id.into(),
            aliases,
            link_id: "lk".into(),
            peer: "p".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: uid.map(Into::into),
            firmware: Some("4.5.7".into()),
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 9,
            armed: false,
            mode: None,
        });
    };
    observe(&mut engine, "provisional", Vec::new(), None);
    observe(&mut engine, "v", vec!["provisional".into()], Some("uid-1"));
    assert_eq!(events(&engine), ["First seen, as a Quadrotor"], "once");

    // A new vehicle: provisional first, then identified.
    observe(&mut engine, "p2", Vec::new(), None);
    observe(&mut engine, "w", vec!["p2".into()], Some("uid-2"));
    assert_eq!(
        events(&engine),
        ["First seen, as a Quadrotor", "First seen, as a Quadrotor"]
    );
}

#[test]
fn a_simulated_vehicles_history_goes_with_its_simulation() {
    let capabilities = DaemonCapabilities {
        simulator: true,
        ..DaemonCapabilities::default()
    };
    let mut engine = engine_running(Box::new(Ticking::default()), capabilities);
    let mut ui = connect(&mut engine, "operator-ui");
    let sim = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": {
            "type": "simulator", "vehicle": "multicopter",
            "latitudeDeg": -35.363_261, "longitudeDeg": 149.165_23,
            "altitudeM": 584.0, "headingDeg": 353.0
        } }),
    );
    let sim = sim["result"]["id"]
        .as_str()
        .expect("a simulation")
        .to_owned();
    heard_on(&mut engine, "s", &sim, "4.6.0", "Quadrotor");
    heard_on(&mut engine, "v", "lk", "4.6.0", "Quadrotor");
    let rtl = Reported {
        name: "RTL_ALT".into(),
        value: 3000.0,
        kind: ParamKind::Int32,
    };
    engine.parameter_reported("s", rtl.clone(), false);
    engine.parameter_reported("v", rtl, false);
    let of = |engine: &Engine, id: &str| {
        engine
            .state()
            .history
            .values()
            .filter(|e| e.vehicle_id == id)
            .count()
    };
    assert_eq!((of(&engine, "s"), of(&engine, "v")), (2, 2));

    let stopped = call(&mut engine, &mut ui, "sim/stop", json!({ "id": sim }));
    assert!(stopped.get("error").is_none(), "{stopped}");
    assert_eq!(
        (of(&engine, "s"), of(&engine, "v")),
        (0, 2),
        "a new simulation is a new vehicle; a real one keeps its history"
    );
}

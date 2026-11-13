//! A fork's own MAVLink dialects (S252 and S255, ADR-0044): used by the
//! vehicles built from their commits or chosen for a vehicle, refused when
//! they would change a standard message for every vehicle, and used by the
//! inspector for the vehicles that use them.

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
use uxduck_core::events::Event;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::mavlink_inspect::Inspector;
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Persistence, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};
use uxduck_mavlink::framing::Header;

const ACME: &str = include_str!("../../uxduck-mavlink/fixtures/acme_fork.xml");
const SURVEY: &str = include_str!("../../uxduck-mavlink/fixtures/acme_survey.xml");

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

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
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

/// What the engine wrote, kept to start it again from.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<Event>>>);

impl Persistence for Journal {
    fn append(&mut self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

fn engine() -> (Engine, Client) {
    engine_from(Box::new(Forget), State::default())
}

fn engine_from(persistence: Box<dyn Persistence>, state: State) -> (Engine, Client) {
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
        persistence,
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
        Loaded {
            state,
            sessions: vec![],
        },
    );
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut ui = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = call(
        &mut engine,
        &mut ui,
        "initialize",
        json!({
            "clientInfo": { "name": "operator-ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    (engine, ui)
}

/// A vehicle heard on no particular link, built from a commit.
fn heard(engine: &mut Engine, id: &str, commit: Option<&str>) {
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
        firmware_commit: commit.map(Into::into),
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
}

fn add(engine: &mut Engine, ui: &mut Client, binding: Value) -> Value {
    let mut params =
        json!({ "files": [{ "name": "acme.xml", "text": ACME }], "label": "Acme r12" });
    params
        .as_object_mut()
        .unwrap()
        .extend(binding.as_object().unwrap().clone());
    call(engine, ui, "mavlink/dialects/add", params)
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

fn inspected(id: &str, message_id: u32, payload: &[u8]) -> Value {
    let mut table = Inspector::default();
    table.record(
        id,
        InspectDirection::Received,
        Header {
            sequence: 0,
            system_id: 1,
            component_id: 1,
            message_id,
        },
        payload,
        10,
    );
    let seen = table.read(&[id.to_owned()], 20).decode(None);
    serde_json::to_value(&seen.messages[0]).unwrap()
}

#[test]
fn a_dialect_chosen_for_one_vehicle_decodes_its_messages_there_only() {
    let (mut engine, mut ui) = engine();
    heard(&mut engine, "dialect-v1", None);
    heard(&mut engine, "dialect-v2", None);

    let before = inspected("dialect-v1", 43001, &acme_status());
    assert_eq!(before["name"], "#43001", "{before}");

    let added = add(&mut engine, &mut ui, json!({ "vehicleId": "dialect-v1" }));
    let dialect = &added["result"];
    assert_eq!(dialect["usedBy"], json!(["dialect-v1"]), "{added}");
    assert!(dialect.get("commits").is_none(), "{added}");
    let changes: Vec<(&str, &str)> = dialect["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["change"].as_str().unwrap()))
        .collect();
    assert_eq!(
        changes,
        [
            ("SYS_STATUS", "extensions"),
            ("GLOBAL_POSITION_INT", "layout"),
            ("ACME_STATUS", "new")
        ]
    );
    assert_eq!(dialect["unchanged"], 1, "HEARTBEAT is ArduPilot's");
    assert_eq!(
        dialect["unread"],
        json!(["GLOBAL_POSITION_INT"]),
        "UXDuck's own readers lose the changed layout"
    );

    let shown = inspected("dialect-v1", 43001, &acme_status());
    assert_eq!(shown["name"], "ACME_STATUS", "{shown}");
    assert_eq!(shown["ownDialect"], "Acme r12");
    assert_eq!(shown["change"], "new");
    assert_eq!(shown["fields"]["mode"], 3);
    assert_eq!(shown["fields"]["name"], "probe");
    assert_eq!(shown["fields"]["temperature"], 2150);
    assert_eq!(shown["labels"]["mode"], "ACME_MODE_SURVEY");

    let other = inspected("dialect-v2", 43001, &acme_status());
    assert_eq!(other["name"], "#43001", "another vehicle: {other}");

    let listed = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/list",
        json!({ "vehicleId": "dialect-v2" }),
    );
    assert_eq!(listed["result"]["dialects"], json!([]), "{listed}");
    let listed = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/list",
        json!({ "vehicleId": "dialect-v1" }),
    );
    assert_eq!(listed["result"]["dialects"][0]["label"], "Acme r12");
    assert_eq!(listed["result"]["choice"]["type"], "chosen", "{listed}");

    let id = dialect["id"].as_str().unwrap().to_owned();
    let removed = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/remove",
        json!({ "id": id }),
    );
    assert!(removed.get("result").is_some(), "{removed}");
    let after = inspected("dialect-v1", 43001, &acme_status());
    assert_eq!(after["name"], "#43001", "removed: {after}");
}

/// S338: the inspector, and the agent through `mavlink/inspect`, read a
/// dialect's message with its fields' units and enum names; an id no
/// dialect has stays raw, by its number.
#[test]
fn a_dialects_message_shows_its_units_and_an_unknown_one_stays_raw() {
    let (mut engine, mut ui) = engine();
    heard(&mut engine, "spray-v1", None);
    let added = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/add",
        json!({ "files": [{ "name": "acme_survey.xml", "text": SURVEY }], "vehicleId": "spray-v1" }),
    );
    assert_eq!(
        added["result"]["commands"][0]["noLocation"], true,
        "{added}"
    );
    let mut status = 1_234_u32.to_le_bytes().to_vec();
    status.extend_from_slice(&12.5_f32.to_le_bytes());
    status.extend_from_slice(&900_u16.to_le_bytes());
    status.push(1);
    let shown = inspected("spray-v1", 43_010, &status);
    assert_eq!(shown["name"], "ACME_SPRAY_STATUS", "{shown}");
    assert_eq!(shown["fields"]["flow"], 12.5);
    assert_eq!(
        shown["units"],
        json!({ "time_boot_ms": "ms", "flow": "l/min", "tank": "ml" })
    );
    assert_eq!(shown["labels"]["state"], "ACME_SPRAY_ON");

    let unknown = inspected("spray-v1", 43_011, &status);
    assert_eq!(unknown["name"], "#43011", "{unknown}");
    assert!(unknown.get("fields").is_none() && unknown.get("units").is_none());
}

#[test]
fn a_dialect_of_commands_alone_lists_them_for_plans() {
    let (mut engine, mut ui) = engine();
    let commands = r#"<mavlink><enums><enum name="MAV_CMD">
        <entry value="31000" name="MAV_CMD_ACME_DROP"><description>Drop the payload.</description>
          <param index="1" label="Bay">Which bay opens.</param><param index="3" reserved="true"/></entry>
        </enum></enums></mavlink>"#;
    let added = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/add",
        json!({ "files": [{ "name": "acme_commands.xml", "text": commands }], "everyVehicle": true }),
    );
    assert_eq!(added["result"]["messages"], json!([]), "{added}");
    assert_eq!(
        added["result"]["commands"],
        json!([{
            "value": 31000,
            "name": "MAV_CMD_ACME_DROP",
            "description": "Drop the payload.",
            "params": [{ "index": 1, "label": "Bay", "description": "Which bay opens." }]
        }]),
        "{added}"
    );
    let listed = call(&mut engine, &mut ui, "mavlink/dialects/list", json!({}));
    assert_eq!(
        listed["result"]["dialects"][0]["commands"][0]["name"], "MAV_CMD_ACME_DROP",
        "{listed}"
    );
}

#[test]
fn changing_a_standard_message_for_every_vehicle_is_refused() {
    let (mut engine, mut ui) = engine();
    let refused = add(&mut engine, &mut ui, json!({ "everyVehicle": true }));
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Acme r12 changes SYS_STATUS, GLOBAL_POSITION_INT"),
        "{refused}"
    );
    assert!(message.contains("can't be for every vehicle"), "{message}");

    let only_new = String::from(
        r#"<mavlink><enums><enum name="ACME_MODE"><entry value="3" name="ACME_MODE_SURVEY"/></enum></enums><messages><message id="43002" name="ACME_PING"><field type="uint8_t" name="mode" enum="ACME_MODE">Mode.</field></message></messages></mavlink>"#,
    );
    let added = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/add",
        json!({ "files": [{ "name": "ping.xml", "text": only_new }], "everyVehicle": true }),
    );
    assert_eq!(added["result"]["label"], "ping", "{added}");
    assert_eq!(added["result"]["everyVehicle"], true, "{added}");

    let nothing = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/add",
        json!({ "files": [{ "name": "same.xml", "text": "<mavlink><messages/></mavlink>" }] }),
    );
    assert!(nothing.get("error").is_some(), "{nothing}");
}

#[test]
fn a_dialect_follows_the_builds_of_its_commits_unless_a_vehicle_chose_otherwise() {
    let (mut engine, mut ui) = engine();
    heard(&mut engine, "dialect-c1", Some("2a3dc4b7"));
    heard(&mut engine, "dialect-c2", Some("cccc0000"));
    heard(&mut engine, "dialect-c3", Some("9f00aa12"));
    let name = |id: &str| inspected(id, 43001, &acme_status())["name"].clone();

    let bad = add(&mut engine, &mut ui, json!({ "commits": ["xyz"] }));
    assert!(bad.get("error").is_some(), "{bad}");

    // A fleet on two builds of the fork.
    let added = add(
        &mut engine,
        &mut ui,
        json!({ "commits": ["2A3DC4B7BF", "9f00aa12", "2a3dc4b7bf"] }),
    );
    assert_eq!(
        added["result"]["commits"],
        json!(["2a3dc4b7bf", "9f00aa12"]),
        "{added}"
    );
    assert_eq!(
        added["result"]["usedBy"],
        json!(["dialect-c1", "dialect-c3"])
    );
    assert_eq!(name("dialect-c1"), "ACME_STATUS");
    assert_eq!(name("dialect-c3"), "ACME_STATUS");
    assert_eq!(name("dialect-c2"), "#43001");
    let id = added["result"]["id"].as_str().unwrap().to_owned();

    // Chosen for a vehicle of another build.
    let chosen = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "dialect-c2", "choice": { "type": "chosen", "ids": [id] } }),
    );
    assert_eq!(
        chosen["result"]["dialects"][0]["id"],
        id.as_str(),
        "{chosen}"
    );
    assert_eq!(name("dialect-c2"), "ACME_STATUS");
    assert_eq!(
        engine.state().vehicles["dialect-c2"].dialects,
        uxduck_core::domain::dialect::DialectChoice::Chosen {
            ids: vec![id.clone()]
        }
    );

    // None for one of its own builds, then back to Automatic.
    let none = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "dialect-c1", "choice": { "type": "chosen", "ids": [] } }),
    );
    assert_eq!(none["result"]["dialects"], json!([]), "{none}");
    assert_eq!(name("dialect-c1"), "#43001");
    call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "dialect-c1", "choice": { "type": "automatic" } }),
    );
    assert_eq!(name("dialect-c1"), "ACME_STATUS");

    let unknown = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/use",
        json!({ "vehicleId": "dialect-c1", "choice": { "type": "chosen", "ids": ["nope"] } }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found", "{unknown}");

    // Adding another from a vehicle keeps what it used.
    let ping = String::from(
        r#"<mavlink><messages><message id="43003" name="ACME_PONG"><field type="uint8_t" name="n">N.</field></message></messages></mavlink>"#,
    );
    let second = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/add",
        json!({ "files": [{ "name": "pong.xml", "text": ping }], "vehicleId": "dialect-c1" }),
    );
    let second_id = second["result"]["id"].as_str().unwrap().to_owned();
    let listed = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/list",
        json!({ "vehicleId": "dialect-c1" }),
    );
    assert_eq!(
        listed["result"]["choice"],
        json!({ "type": "chosen", "ids": [id, second_id] }),
        "{listed}"
    );
    assert_eq!(name("dialect-c1"), "ACME_STATUS");

    // Which builds use it by themselves changes; every vehicle is refused.
    let moved = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/bind",
        json!({ "id": id, "commits": ["9f00aa12"] }),
    );
    assert_eq!(moved["result"]["commits"], json!(["9f00aa12"]), "{moved}");
    let every = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/bind",
        json!({ "id": id, "everyVehicle": true }),
    );
    assert!(
        every["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("can't be for every vehicle"),
        "{every}"
    );
}

#[test]
fn a_dialect_is_bound_again_when_the_journal_is_replayed() {
    let journal = Journal::default();
    let (mut engine, mut ui) = engine_from(Box::new(journal.clone()), State::default());
    heard(&mut engine, "dialect-replayed", None);
    let added = add(
        &mut engine,
        &mut ui,
        json!({ "vehicleId": "dialect-replayed" }),
    );
    assert!(added.get("result").is_some(), "{added}");
    drop(engine);

    uxduck_mavlink::custom::publish(
        "dialect-replayed",
        uxduck_mavlink::custom::Messages::default(),
    );
    let forgotten = inspected("dialect-replayed", 43001, &acme_status());
    assert_eq!(
        forgotten["name"], "#43001",
        "a new process knows nothing yet"
    );

    let mut state = State::default();
    for event in journal.0.lock().unwrap().iter() {
        state.apply(event).unwrap();
    }
    let (mut engine, mut ui) = engine_from(Box::new(Forget), state);
    let shown = inspected("dialect-replayed", 43001, &acme_status());
    assert_eq!(shown["name"], "ACME_STATUS", "{shown}");
    assert_eq!(shown["ownDialect"], "Acme r12");
    let listed = call(
        &mut engine,
        &mut ui,
        "mavlink/dialects/list",
        json!({ "vehicleId": "dialect-replayed" }),
    );
    assert_eq!(
        listed["result"]["dialects"][0]["label"], "Acme r12",
        "{listed}"
    );
}

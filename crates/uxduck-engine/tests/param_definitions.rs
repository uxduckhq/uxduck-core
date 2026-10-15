//! Parameter definitions (S67, ADR-0021): which set describes a vehicle,
//! chosen in P3's order, fetched once through the host, and a person's own
//! sets for a fork.

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
use uxduck_contracts::dto::param_definitions::{CatalogueSet, Firmware as WireFirmware};
use uxduck_contracts::dto::sim::CatalogueFile;
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::pdef;
use uxduck_core::domain::param_definitions::{
    DefinitionSet, DefinitionSource, DefinitionsStatus, Firmware, Match,
};
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::param_definitions::{DefinitionsArrived, DefinitionsOrder, DefinitionsPort};
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::transfers::Reported;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const JSON: &str = include_str!("../../uxduck-contracts/fixtures/pdef/copter.pdef.json");
const XML: &str = include_str!("../../uxduck-contracts/fixtures/pdef/copter.pdef.xml");

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

/// A vehicle heard on no particular link.
fn heard(engine: &mut Engine, id: &str, vehicle_type: &str, firmware: Option<&str>) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: Vec::new(),
        link_id: "lk".into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: vehicle_type.into(),
        uid: None,
        firmware: firmware.map(Into::into),
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
}

fn entry(firmware: WireFirmware, version: &str, commit: &str) -> CatalogueSet {
    let id = format!("{firmware:?}-{version}").to_ascii_lowercase();
    CatalogueSet {
        id: id.clone(),
        firmware,
        version: version.into(),
        commit: commit.into(),
        file: CatalogueFile {
            name: format!("{id}.json.gz"),
            url: format!("https://definitions.example/sets/{id}.json.gz"),
            size: 1,
            sha256: "0".repeat(64),
            encoding: None,
        },
        source_url: format!("https://github.com/ArduPilot/ardupilot/tree/{commit}"),
    }
}

fn catalogue() -> Vec<CatalogueSet> {
    vec![
        entry(WireFirmware::ArduCopter, "4.5.6", "aaaa0000"),
        entry(WireFirmware::ArduCopter, "4.5.7", "2a3dc4b7bf"),
        entry(WireFirmware::ArduCopter, "latest", "cccc0000"),
    ]
}

/// The fixture's Copter set, both variants kept, as the catalogue serves it.
fn copter_set(version: &str) -> DefinitionSet {
    let (_, xml) = pdef::from_xml(XML).unwrap();
    let (_, json) = pdef::from_json(JSON).unwrap();
    DefinitionSet {
        source: DefinitionSource::Ardupilot,
        firmware: Firmware::ArduCopter,
        version: Some(version.into()),
        commit: None,
        parameters: pdef::merge(xml, &json),
    }
}

fn status(engine: &Engine, id: &str) -> (DefinitionsStatus, Option<Match>, Option<String>) {
    let chosen = &engine.state().param_definitions[id];
    (chosen.status, chosen.matched, chosen.detail.clone())
}

/// Reports these parameters as the vehicle's.
fn reports(engine: &mut Engine, id: &str, names: &[&str]) {
    engine.vehicle_booted(id, 100, false);
    engine.parameters_downloaded(
        id,
        Ok(names
            .iter()
            .map(|name| Reported {
                name: (*name).into(),
                value: 1.0,
                kind: ParamKind::Int32,
            })
            .collect()),
        &std::collections::BTreeMap::new(),
    );
}

#[test]
fn a_copter_gets_its_exact_release_fetched_once_and_described_by_name() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    assert_eq!(
        asked.take(),
        [DefinitionsOrder::Catalogue { refresh: false }]
    );
    assert_eq!(status(&engine, "v").0, DefinitionsStatus::Loading);

    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    let orders = asked.take();
    assert!(
        matches!(&orders[..], [DefinitionsOrder::Set(set)] if set.id == "arducopter-4.5.7"),
        "{orders:?}"
    );
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    assert!(asked.take().is_empty(), "asked once");

    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    assert_eq!(
        status(&engine, "v"),
        (DefinitionsStatus::Ready, Some(Match::Exact), None)
    );
    assert_eq!(
        engine.state().param_definitions["v"].title.as_deref(),
        Some("Copter 4.5.7")
    );

    reports(
        &mut engine,
        "v",
        &["RTL_ALT", "FS_THR_ENABLE", "MYFORK_GAIN"],
    );
    let answer = call(
        &mut engine,
        &mut ui,
        "param/definitions",
        json!({ "vehicleId": "v" }),
    );
    let result = &answer["result"];
    assert_eq!(result["status"]["status"], "ready", "{answer}");
    let names: Vec<&str> = result["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["FS_THR_ENABLE", "RTL_ALT"],
        "only what it reports, and not what the set does not describe"
    );
    let rtl = &result["definitions"][1];
    assert_eq!(rtl["displayName"], "RTL Altitude");
    assert_eq!(rtl["units"], "cm");
}

#[test]
fn the_search_finds_a_parameter_by_what_it_means() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    reports(&mut engine, "v", &["BATT_FS_LOW_ACT", "RTL_ALT"]);
    let found = call(
        &mut engine,
        &mut ui,
        "search/query",
        json!({ "query": "battery failsafe" }),
    );
    let hits = found["result"]["hits"].as_array().expect("hits");
    let parameters: Vec<&Value> = hits.iter().filter(|h| h["kind"] == "parameter").collect();
    assert_eq!(parameters.len(), 1, "{found}");
    assert_eq!(parameters[0]["title"], "BATT_FS_LOW_ACT");
    assert!(
        parameters[0]["subtitle"]
            .as_str()
            .unwrap()
            .ends_with("· Low battery failsafe action")
    );
}

#[test]
fn a_development_build_is_described_by_the_nearest_release_and_says_so() {
    let asked = Asked::default();
    let (mut engine, _ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.8"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    assert_eq!(
        status(&engine, "v"),
        (
            DefinitionsStatus::Ready,
            Some(Match::Approximate),
            Some("Descriptions are from Copter 4.5.7; this vehicle runs 4.5.8.".into())
        )
    );
}

#[test]
fn a_helicopter_reads_the_helicopter_variant() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "heli", "Helicopter", Some("4.5.7"));
    heard(&mut engine, "quad", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    let high = |engine: &mut Engine, ui: &mut Client, id: &str| {
        reports(engine, id, &["ATC_RAT_PIT_D"]);
        let answer = call(engine, ui, "param/definitions", json!({ "vehicleId": id }));
        answer["result"]["definitions"][0]["range"]["high"]
            .as_f64()
            .unwrap()
    };
    assert_eq!(high(&mut engine, &mut ui, "quad"), 0.05);
    assert_eq!(high(&mut engine, &mut ui, "heli"), 0.02);
}

#[test]
fn a_plane_never_gets_a_copters_set() {
    let asked = Asked::default();
    let (mut engine, _ui) = engine(Some(&asked));
    heard(&mut engine, "p", "FixedWing", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    assert_eq!(
        status(&engine, "p"),
        (
            DefinitionsStatus::Missing,
            None,
            Some("No descriptions for Plane 4.5.7 yet. Names and values only.".into())
        )
    );
}

#[test]
fn without_a_catalogue_or_a_set_it_says_why() {
    let (mut engine, _ui) = engine(None);
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    let (state, _, detail) = status(&engine, "v");
    assert_eq!(state, DefinitionsStatus::Missing);
    assert!(
        detail
            .unwrap()
            .contains("this UXDuck cannot download descriptions")
    );

    let asked = Asked::default();
    let (mut engine, _ui) = self::engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Err("offline".into())));
    assert!(
        status(&engine, "v")
            .2
            .unwrap()
            .starts_with("No descriptions: offline.")
    );

    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Err("its checksum did not match".into()),
    });
    assert_eq!(
        status(&engine, "v"),
        (
            DefinitionsStatus::Failed,
            Some(Match::Exact),
            Some(
                "Descriptions could not be loaded: its checksum did not match. Names and values only."
                    .into()
            )
        )
    );
}

#[test]
fn a_forks_own_set_wins_for_its_vehicle_until_it_is_removed() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    heard(&mut engine, "other", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    asked.take();

    // A fork's file: ArduPilot's format with a parameter of its own.
    let fork = JSON.replacen(
        "\"Copter\": {",
        "\"Copter\": {\n    \"MYFORK_GAIN\": { \"DisplayName\": \"My fork's gain\", \"Description\": \"Ours.\" },",
        1,
    );
    let added = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "text": fork, "label": "My fork", "vehicleId": "v" }),
    );
    let set = &added["result"];
    assert_eq!(set["label"], "My fork", "{added}");
    assert_eq!(set["firmware"], "ArduCopter", "read from the file");
    let id = set["id"].as_str().unwrap().to_owned();
    let orders = asked.take();
    assert!(
        matches!(&orders[..], [DefinitionsOrder::StoreOwn { id: stored, .. }] if *stored == id),
        "{orders:?}"
    );
    assert_eq!(
        status(&engine, "v"),
        (
            DefinitionsStatus::Ready,
            Some(Match::Own),
            Some("Descriptions from your set “My fork”.".into())
        )
    );
    assert_eq!(
        status(&engine, "other").1,
        Some(Match::Exact),
        "only its vehicle"
    );
    reports(&mut engine, "v", &["MYFORK_GAIN"]);
    let described = call(
        &mut engine,
        &mut ui,
        "param/definitions",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(
        described["result"]["definitions"][0]["displayName"],
        "My fork's gain"
    );
    let listed = call(&mut engine, &mut ui, "param/definitions/list", json!({}));
    assert_eq!(listed["result"]["own"][0]["id"], id.as_str());
    assert_eq!(listed["result"]["catalogue"], "ready");
    assert_eq!(listed["result"]["catalogueSets"], 3);

    let removed = call(
        &mut engine,
        &mut ui,
        "param/definitions/remove",
        json!({ "id": id }),
    );
    assert!(removed.get("result").is_some(), "{removed}");
    assert_eq!(asked.take(), [DefinitionsOrder::RemoveOwn { id }]);
    assert_eq!(
        status(&engine, "v").1,
        Some(Match::Exact),
        "back to ArduPilot's"
    );
    let described = call(
        &mut engine,
        &mut ui,
        "param/definitions",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(
        described["result"]["definitions"],
        json!([]),
        "MYFORK_GAIN is undescribed again"
    );
}

#[test]
fn a_set_must_say_its_firmware_and_a_commit_must_be_one() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    let unnamed = JSON.replace("\"Copter\"", "\"RTL_\"");
    let refused = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "text": unnamed }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
    let given = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "text": unnamed, "firmware": "ArduCopter", "commit": "2A3DC4B7" }),
    );
    assert_eq!(given["result"]["label"], "Copter 2a3dc4b7", "{given}");
    assert_eq!(given["result"]["commit"], "2a3dc4b7");
    let bad = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "text": JSON, "commit": "nope" }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value");
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "param/definitions/add",
            json!({ "text": "hello" })
        )["error"]["data"]["kind"],
        "invalid-value"
    );
}

#[test]
fn a_set_bound_to_a_commit_describes_vehicles_built_from_it() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    engine.vehicle_observed(Observation {
        id: "v".into(),
        aliases: Vec::new(),
        link_id: "lk".into(),
        peer: "p".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: Some("4.5.7".into()),
        firmware_commit: Some("deadbeef".into()),
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    let added = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "text": JSON, "commit": "deadbeef12" }),
    );
    assert!(added.get("result").is_some(), "{added}");
    assert_eq!(status(&engine, "v").1, Some(Match::Own));
}

/// A copter on 4.5.7 with ArduPilot's set ready and these parameters.
fn described_copter(names: &[&str]) -> (Engine, Client) {
    let asked = Asked::default();
    let (mut engine, ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    engine.definitions_arrived(DefinitionsArrived::Set {
        id: "arducopter-4.5.7".into(),
        result: Ok(copter_set("4.5.7")),
    });
    reports(&mut engine, "v", names);
    (engine, ui)
}

#[test]
fn the_agent_searches_by_meaning_and_learns_which_set_answers() {
    let (mut engine, _ui) = described_copter(&["BATT_FS_LOW_ACT", "RTL_ALT", "MYFORK_GAIN"]);
    let mut agent = connect_as(&mut engine, "agent");
    let found = call(
        &mut engine,
        &mut agent,
        "param/search",
        json!({ "query": "battery failsafe" }),
    );
    let result = &found["result"];
    assert_eq!(
        result["describedBy"]["vehicleName"], "Alpha",
        "the only vehicle"
    );
    assert_eq!(
        result["describedBy"]["definitions"]["title"],
        "Copter 4.5.7"
    );
    assert_eq!(result["describedBy"]["definitions"]["matched"], "exact");
    assert_eq!(result["total"], 1);
    let hit = &result["hits"][0];
    assert_eq!(hit["name"], "BATT_FS_LOW_ACT");
    assert_eq!(hit["displayName"], "Low battery failsafe action");
    assert_eq!(hit["value"], 1.0);
    assert_eq!(hit["valueName"], "Land");

    // The whole set: parameters the vehicle does not report, without values.
    let whole = call(
        &mut engine,
        &mut agent,
        "param/search",
        json!({ "query": "frame", "wholeSet": true }),
    );
    assert_eq!(whole["result"]["hits"][0]["name"], "FRAME_CLASS");
    assert!(whole["result"]["hits"][0].get("value").is_none());

    // Paged.
    let paged = call(
        &mut engine,
        &mut agent,
        "param/search",
        json!({ "query": "a", "wholeSet": true, "limit": 2, "offset": 1 }),
    );
    assert_eq!(paged["result"]["hits"].as_array().unwrap().len(), 2);
    assert!(paged["result"]["total"].as_u64().unwrap() > 3);

    let got = call(
        &mut engine,
        &mut agent,
        "param/get",
        json!({ "names": ["rtl_alt", "MYFORK_GAIN"] }),
    );
    let parameters = &got["result"]["parameters"];
    assert_eq!(parameters[0]["definition"]["units"], "cm");
    assert_eq!(parameters[0]["value"], 1.0);
    assert_eq!(parameters[1]["name"], "MYFORK_GAIN");
    assert!(
        parameters[1].get("definition").is_none(),
        "undescribed: the agent must say so"
    );
}

#[test]
fn a_long_list_answers_with_groups_and_a_group_with_its_parameters() {
    let names: Vec<String> = (0..120).map(|n| format!("G{}_P{n}", n % 3)).collect();
    let mut all: Vec<&str> = names.iter().map(String::as_str).collect();
    all.extend(["BATT_FS_LOW_ACT", "BATT_CAPACITY"]);
    let (mut engine, mut ui) = described_copter(&all);
    let listed = call(&mut engine, &mut ui, "param/list", json!({}));
    let groups = &listed["result"]["groups"];
    assert_eq!(listed["result"]["total"], 122);
    assert!(listed["result"].get("parameters").is_none());
    assert!(
        groups
            .as_array()
            .unwrap()
            .contains(&json!({ "group": "BATT_", "count": 2 }))
    );
    let batt = call(
        &mut engine,
        &mut ui,
        "param/list",
        json!({ "group": "batt_" }),
    );
    let names: Vec<&str> = batt["result"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["BATT_CAPACITY", "BATT_FS_LOW_ACT"]);
}

#[test]
fn a_param_file_is_compared_with_units_and_nothing_written() {
    let (mut engine, mut ui) = described_copter(&["RTL_ALT", "BATT_CAPACITY"]);
    let diff = call(
        &mut engine,
        &mut ui,
        "param/diff",
        json!({ "text": "# ours\nRTL_ALT,3000\nBATT_CAPACITY,1\nMYFORK_GAIN 2\noops\n" }),
    );
    let result = &diff["result"];
    assert_eq!(
        result["differences"],
        json!([
            { "name": "MYFORK_GAIN", "theirs": 2.0 },
            { "name": "RTL_ALT", "value": 1.0, "theirs": 3000.0, "units": "cm" }
        ]),
        "BATT_CAPACITY is the same"
    );
    assert_eq!(result["unreadLines"], json!([5]));
    assert_eq!(engine.state().parameters["v/RTL_ALT"].value, 1.0);
}

#[test]
fn an_agents_write_asks_with_the_value_it_replaces_and_its_units() {
    let (mut engine, _ui) = described_copter(&["RTL_ALT"]);
    let mut agent = connect_as(&mut engine, "agent");
    agent.next_id += 1;
    engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "id": agent.next_id, "method": "param/set",
                 "params": { "vehicleId": "v", "name": "RTL_ALT", "value": 3000 } })
        .to_string(),
    );
    let summaries: Vec<&str> = engine
        .state()
        .approvals
        .values()
        .map(|a| a.summary.as_str())
        .collect();
    assert_eq!(summaries, ["set RTL_ALT from 1 to 3000 cm on Alpha"]);
}

#[test]
fn a_writes_answer_says_the_units_and_whether_it_needs_a_reboot() {
    let (mut engine, mut ui) = described_copter(&["RTL_ALT", "FRAME_CLASS"]);
    // Its link is not one this test opened, so nothing is sent: the answer
    // comes at once.
    let rtl = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 3000 }),
    );
    assert_eq!(rtl["result"]["status"], "not-sent", "{rtl}");
    assert_eq!(rtl["result"]["units"], "cm");
    assert!(rtl["result"].get("rebootRequired").is_none());
    let frame = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "FRAME_CLASS", "value": 2 }),
    );
    assert_eq!(frame["result"]["rebootRequired"], true, "{frame}");
}

/// A set from where a person's CI publishes it (S74): fetched, kept and
/// chosen like a file; fetched again on a refresh; a notice when it fails.
#[test]
fn a_set_from_a_url_is_fetched_kept_and_fetched_again() {
    let asked = Asked::default();
    let (mut engine, mut ui) = engine(Some(&asked));
    heard(&mut engine, "v", "Quadrotor", Some("4.5.7"));
    engine.definitions_arrived(DefinitionsArrived::Catalogue(Ok(catalogue())));
    asked.take();

    let plain = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "url": "http://example.com/set.json", "firmware": "ArduCopter" }),
    );
    assert_eq!(
        plain["error"]["data"]["kind"], "invalid-value",
        "https only"
    );
    let unsaid = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "url": "https://ci.example.com/copter.pdef.json" }),
    );
    assert_eq!(
        unsaid["error"]["data"]["kind"], "invalid-value",
        "the firmware is needed"
    );

    let added = call(
        &mut engine,
        &mut ui,
        "param/definitions/add",
        json!({ "url": "https://ci.example.com/copter.pdef.json", "firmware": "ArduCopter", "vehicleId": "v" }),
    );
    let set = &added["result"];
    assert_eq!(set["label"], "Copter from ci.example.com", "{added}");
    assert_eq!(set["parameterCount"], 0, "not fetched yet");
    let id = set["id"].as_str().unwrap().to_owned();
    assert!(
        matches!(&asked.take()[..], [DefinitionsOrder::FetchOwn { id: asked_for, url }]
            if *asked_for == id && url == "https://ci.example.com/copter.pdef.json")
    );

    // Fetched: kept, counted and chosen for its vehicle.
    engine.definitions_arrived(DefinitionsArrived::Fetched {
        id: id.clone(),
        result: Ok(JSON.to_owned()),
    });
    let own = &engine.state().own_sets[&id];
    assert!(own.parameter_count > 0);
    assert!(own.fetched_at_us.is_some());
    assert!(matches!(
        &asked.take()[..],
        [DefinitionsOrder::StoreOwn { .. }]
    ));
    assert_eq!(status(&engine, "v").1, Some(Match::Own));

    // A refresh fetches it again; a failure says why, once.
    call(
        &mut engine,
        &mut ui,
        "param/definitions/list",
        json!({ "refresh": true }),
    );
    assert!(
        asked
            .take()
            .iter()
            .any(|o| matches!(o, DefinitionsOrder::FetchOwn { id: again, .. } if *again == id))
    );
    for _ in 0..2 {
        engine.definitions_arrived(DefinitionsArrived::Fetched {
            id: id.clone(),
            result: Err("cannot fetch it: 404".into()),
        });
    }
    let notices: Vec<_> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.condition.as_deref() == Some(&format!("own-set:{id}")))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body.as_deref(), Some("cannot fetch it: 404"));
    assert_eq!(
        status(&engine, "v").1,
        Some(Match::Own),
        "the kept copy stays"
    );
}

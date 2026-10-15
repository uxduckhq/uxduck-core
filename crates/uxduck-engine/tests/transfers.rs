//! Parameters and missions: downloads per boot, compare-and-swap writes,
//! versioned uploads and the restart notice (S11).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::mission::MissionItem;
use uxduck_core::domain::parameter::{ParamKind, TransferStatus};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem, Ports, Random,
    TransferOrder,
};
use uxduck_engine::transfers::{Failure, Reported};
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

/// The transfers the engine asked the data plane for.
#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<TransferOrder>>>);

impl Orders {
    fn take(&self) -> Vec<TransferOrder> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn one(&self) -> TransferOrder {
        let mut all = self.take();
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }
}

impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn transfer(&mut self, order: TransferOrder) {
        self.0.lock().unwrap().push(order);
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

/// The reply to the last request, once it comes.
fn answer(client: &mut Client) -> Value {
    let id = client.next_id;
    std::iter::from_fn(|| client.inbox.try_recv().ok())
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .find(|f| f["id"] == id)
        .expect("an answer")
}

const S: u64 = 1_000_000;

/// An engine with one online copter, `v`, on a writable UDP link.
fn setup() -> (Engine, Client, Orders) {
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
        Loaded::default(),
    );
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
    (engine, ui, orders)
}

fn reported(name: &str, value: f64, kind: ParamKind) -> Reported {
    Reported {
        name: name.into(),
        value,
        kind,
    }
}

fn waypoint(altitude_m: f64) -> MissionItem {
    MissionItem {
        command: 16,
        frame: 3,
        param1: 0.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        latitude_deg: -35.36,
        longitude_deg: 149.16,
        altitude_m,
        autocontinue: true,
    }
}

/// Boots `v` and completes both downloads.
fn synced(engine: &mut Engine, orders: &Orders, boot: u64) {
    engine.vehicle_booted("v", boot, false);
    assert_eq!(
        orders.take(),
        [
            TransferOrder::DownloadParameters {
                vehicle_id: "v".into()
            },
            TransferOrder::DownloadMission {
                vehicle_id: "v".into()
            }
        ]
    );
    engine.parameters_downloaded(
        "v",
        Ok(vec![
            reported("RTL_ALT", 1500.0, ParamKind::Int32),
            reported("WPNAV_SPEED", 1000.0, ParamKind::Real32),
        ]),
        &BTreeMap::new(),
    );
    engine.mission_downloaded("v", Ok(vec![waypoint(0.0), waypoint(30.0)]));
}

#[test]
fn a_boot_downloads_once_and_a_restart_downloads_again_with_a_notice() {
    let (mut engine, _ui, orders) = setup();
    synced(&mut engine, &orders, 100 * S);
    let set = &engine.state().parameter_sets["v"];
    assert_eq!(
        (set.status, set.count, set.boot_instant_us),
        (TransferStatus::Synced, 2, Some(100 * S))
    );
    assert_eq!(engine.state().parameters["v/RTL_ALT"].value, 1500.0);
    assert_eq!(engine.state().missions["v"].version, 1);

    // The same boot, seen again (a daemon restart, or delay): nothing.
    engine.vehicle_booted("v", 101 * S, false);
    assert!(orders.take().is_empty());

    // A restart: both again, with a notice until both are back.
    engine.vehicle_booted("v", 400 * S, false);
    assert_eq!(orders.take().len(), 2);
    let notice = |engine: &Engine| {
        engine
            .state()
            .notifications
            .values()
            .find(|n| n.condition.as_deref() == Some("refreshing:v"))
            .map(|n| n.title.clone())
    };
    assert_eq!(notice(&engine).as_deref(), Some("Alpha restarted"));
    engine.parameters_downloaded(
        "v",
        Ok(vec![reported("RTL_ALT", 1500.0, ParamKind::Int32)]),
        &BTreeMap::new(),
    );
    assert!(notice(&engine).is_some(), "the mission is still coming");
    assert!(
        !engine.state().parameters.contains_key("v/WPNAV_SPEED"),
        "gone from the new list"
    );
    engine.mission_downloaded("v", Ok(vec![waypoint(0.0), waypoint(30.0)]));
    assert!(notice(&engine).is_none());
    assert_eq!(
        engine.state().missions["v"].version,
        1,
        "the same items keep their version"
    );

    // A restart the data plane saw is one, however soon after the last.
    engine.vehicle_booted("v", 402 * S, true);
    assert_eq!(orders.take().len(), 2);
}

#[test]
fn a_parameter_write_is_a_compare_and_swap_confirmed_by_the_vehicle() {
    let (mut engine, mut ui, orders) = setup();
    synced(&mut engine, &orders, 100 * S);
    let conflict = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 3000, "expectedValue": 1200 }),
    );
    assert_eq!(conflict["error"]["data"]["kind"], "version-conflict");
    let fraction = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 30.5 }),
    );
    assert_eq!(fraction["error"]["data"]["kind"], "invalid-value");

    assert!(
        send(
            &mut engine,
            &mut ui,
            "param/set",
            json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 3000, "expectedValue": 1500 }),
        )
        .is_none(),
        "answered when the vehicle reports back"
    );
    let TransferOrder::SetParameter {
        order_id,
        name,
        value,
        kind,
        ..
    } = orders.one()
    else {
        panic!()
    };
    assert_eq!(
        (name.as_str(), value, kind),
        ("RTL_ALT", 3000.0, ParamKind::Int32)
    );
    engine.parameter_written(
        &order_id,
        Ok(reported("RTL_ALT", 3000.0, ParamKind::Int32)),
        1,
    );
    let reply = answer(&mut ui);
    assert_eq!(reply["result"]["status"], "accepted", "{reply}");
    assert_eq!(reply["result"]["parameter"]["value"], 3000.0);
    assert_eq!(engine.state().parameters["v/RTL_ALT"].value, 3000.0);

    // The vehicle keeps its old value: rejected, with a notice.
    send(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "WPNAV_SPEED", "value": 99999 }),
    );
    let TransferOrder::SetParameter { order_id, .. } = orders.one() else {
        panic!()
    };
    engine.parameter_written(
        &order_id,
        Ok(reported("WPNAV_SPEED", 1000.0, ParamKind::Real32)),
        1,
    );
    let reply = answer(&mut ui);
    assert_eq!(
        (
            reply["result"]["status"].as_str(),
            reply["result"]["detail"].as_str()
        ),
        (Some("rejected"), Some("the vehicle kept 1000"))
    );
    // Silence.
    send(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "WPNAV_SPEED", "value": 500 }),
    );
    let TransferOrder::SetParameter { order_id, .. } = orders.one() else {
        panic!()
    };
    engine.parameter_written(&order_id, Err(Failure::Timeout), 3);
    let reply = answer(&mut ui);
    assert_eq!(reply["result"]["detail"], "no answer after 3 tries");

    // Another ground station changed one: kept, as the daemon saw it.
    engine.parameter_reported("v", reported("WPNAV_SPEED", 750.0, ParamKind::Real32), true);
    assert_eq!(engine.state().parameters["v/WPNAV_SPEED"].value, 750.0);
}

#[test]
fn a_mission_upload_names_its_version_and_the_second_writer_loses() {
    let (mut engine, mut ui, orders) = setup();
    synced(&mut engine, &orders, 100 * S);
    let mut other = connect(&mut engine, "operator-cli");
    let items = |altitude: f64| {
        json!([
            { "command": 16, "frame": 3, "param1": 0, "param2": 0, "param3": 0, "param4": 0,
              "latitudeDeg": -35.36, "longitudeDeg": 149.16, "altitudeM": 0, "autocontinue": true },
            { "command": 22, "frame": 3, "param1": 0, "param2": 0, "param3": 0, "param4": 0,
              "latitudeDeg": 0, "longitudeDeg": 0, "altitudeM": altitude, "autocontinue": true }
        ])
    };
    assert!(
        send(
            &mut engine,
            &mut ui,
            "mission/upload",
            json!({ "vehicleId": "v", "expectedVersion": 1, "items": items(20.0) }),
        )
        .is_none()
    );
    assert_eq!(
        engine.state().missions["v"].status,
        TransferStatus::Uploading
    );
    let busy = call(
        &mut engine,
        &mut other,
        "mission/upload",
        json!({ "vehicleId": "v", "expectedVersion": 1, "items": items(40.0) }),
    );
    assert_eq!(busy["error"]["data"]["kind"], "busy");
    let TransferOrder::UploadMission {
        order_id,
        items: sent,
        ..
    } = orders.one()
    else {
        panic!()
    };
    assert_eq!(sent[1].altitude_m, 20.0);
    engine.mission_uploaded(&order_id, Ok(()));
    let reply = answer(&mut ui);
    assert_eq!(reply["result"]["status"], "accepted");
    assert_eq!(reply["result"]["mission"]["version"], 2);

    // The other editor started from version 1: it loses, told the version.
    let late = call(
        &mut engine,
        &mut other,
        "mission/upload",
        json!({ "vehicleId": "v", "expectedVersion": 1, "items": items(40.0) }),
    );
    assert_eq!(late["error"]["data"]["kind"], "version-conflict");
    assert_eq!(late["error"]["data"]["currentVersion"], 2);

    // A refusal leaves the mission unknown, and says why.
    send(
        &mut engine,
        &mut other,
        "mission/upload",
        json!({ "vehicleId": "v", "expectedVersion": 2, "items": items(40.0) }),
    );
    let TransferOrder::UploadMission { order_id, .. } = orders.one() else {
        panic!()
    };
    engine.mission_uploaded(
        &order_id,
        Err(Failure::Rejected("No space for that many items".into())),
    );
    let reply = answer(&mut other);
    assert_eq!(reply["result"]["status"], "rejected");
    let mission = &engine.state().missions["v"];
    assert_eq!(
        (mission.status, mission.version),
        (TransferStatus::Failed, 2)
    );
}

#[test]
fn offline_vehicles_need_confirming_or_are_not_sent_to() {
    let (mut engine, mut ui, orders) = setup();
    synced(&mut engine, &orders, 100 * S);
    let observation = engine.vehicle("v").map(|v| v.link_id.clone()).unwrap();
    engine.vehicle_observed(Observation {
        id: "v".into(),
        aliases: Vec::new(),
        link_id: observation,
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Offline,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    let warn = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 2000 }),
    );
    assert_eq!(warn["error"]["data"]["kind"], "confirmation-required");
    let upload = call(
        &mut engine,
        &mut ui,
        "mission/upload",
        json!({ "vehicleId": "v", "expectedVersion": 1, "items": [] }),
    );
    assert_eq!(upload["result"]["status"], "not-sent");
    assert!(orders.take().is_empty());
}

/// Defaults from the vehicle (S74): kept per boot, through changes of
/// value, and shown as "changed" in `param/get` and `param/list`.
#[test]
fn defaults_say_what_changed_and_last_the_boot() {
    let (mut engine, mut ui, orders) = setup();
    engine.vehicle_booted("v", 100 * S, false);
    orders.take();
    let defaults = BTreeMap::from([
        ("RTL_ALT".to_owned(), 1500.0),
        ("WPNAV_SPEED".to_owned(), 500.0),
    ]);
    let both = || {
        vec![
            reported("RTL_ALT", 1500.0, ParamKind::Int32),
            reported("WPNAV_SPEED", 1000.0, ParamKind::Real32),
        ]
    };
    engine.parameters_downloaded("v", Ok(both()), &defaults);
    let changed = |engine: &mut Engine, ui: &mut Client| {
        let listed = call(
            engine,
            ui,
            "param/list",
            json!({ "vehicleId": "v", "changedOnly": true }),
        );
        assert!(listed.get("result").is_some(), "{listed}");
        listed["result"]["parameters"]
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|p| p["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(changed(&mut engine, &mut ui), ["WPNAV_SPEED"]);
    let got = call(
        &mut engine,
        &mut ui,
        "param/get",
        json!({ "vehicleId": "v", "names": ["RTL_ALT", "WPNAV_SPEED"] }),
    );
    assert_eq!(got["result"]["parameters"][0]["default"], 1500.0);
    assert!(got["result"]["parameters"][0].get("changed").is_none());
    assert_eq!(got["result"]["parameters"][1]["changed"], true);

    // Another ground station changes it: the default stays.
    engine.parameter_reported("v", reported("RTL_ALT", 3000.0, ParamKind::Int32), true);
    assert_eq!(engine.state().parameters["v/RTL_ALT"].default, Some(1500.0));
    assert_eq!(changed(&mut engine, &mut ui), ["RTL_ALT", "WPNAV_SPEED"]);

    // By the list in the same boot, the defaults are kept; after a restart, not.
    engine.parameters_downloaded("v", Ok(both()), &BTreeMap::new());
    assert_eq!(
        engine.state().parameters["v/WPNAV_SPEED"].default,
        Some(500.0)
    );
    engine.vehicle_booted("v", 400 * S, false);
    engine.parameters_downloaded("v", Ok(both()), &BTreeMap::new());
    assert_eq!(engine.state().parameters["v/WPNAV_SPEED"].default, None);
    assert!(
        changed(&mut engine, &mut ui).is_empty(),
        "unknown is not changed"
    );
}

/// Each change of value is kept in the vehicle's history with who or what
/// made it and why (S74): a person with a reason, the agent, another
/// ground station, a download after a restart; never `STAT_`.
#[test]
#[allow(clippy::too_many_lines, reason = "one story, start to end")]
fn parameter_changes_are_kept_with_their_source_and_reason() {
    let (mut engine, mut ui, orders) = setup();
    synced(&mut engine, &orders, 100 * S);
    let write = |engine: &mut Engine, client: &mut Client, params: Value, to: f64| {
        send(engine, client, "param/set", params);
        let TransferOrder::SetParameter {
            order_id,
            name,
            kind,
            ..
        } = orders.one()
        else {
            panic!()
        };
        engine.parameter_written(&order_id, Ok(reported(&name, to, kind)), 1);
        answer(client)
    };
    let too_long = call(
        &mut engine,
        &mut ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 3000, "reason": "x".repeat(201) }),
    );
    assert_eq!(too_long["error"]["data"]["kind"], "invalid-value");
    write(
        &mut engine,
        &mut ui,
        json!({ "vehicleId": "v", "name": "RTL_ALT", "value": 3000, "reason": " higher hills " }),
        3000.0,
    );
    // The agent's write waits for an operator's yes, then goes as the agent's.
    let mut agent = connect(&mut engine, "agent");
    send(
        &mut engine,
        &mut agent,
        "param/set",
        json!({ "vehicleId": "v", "name": "WPNAV_SPEED", "value": 750 }),
    );
    let pending = call(&mut engine, &mut ui, "approval/list", json!({}));
    let approval = pending["result"]["approvals"][0]["id"].clone();
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval, "allow": true }),
    );
    let TransferOrder::SetParameter { order_id, .. } = orders.one() else {
        panic!()
    };
    engine.parameter_written(
        &order_id,
        Ok(reported("WPNAV_SPEED", 750.0, ParamKind::Real32)),
        1,
    );
    engine.parameter_reported("v", reported("RTL_ALT", 2500.0, ParamKind::Int32), true);
    engine.parameter_reported("v", reported("STAT_RUNTIME", 12.0, ParamKind::Int32), false);
    // With no ground station heard, the vehicle changed it; what it sets
    // itself at each start is left out, descriptions or not.
    engine.parameter_reported("v", reported("ARSPD_USE", 0.0, ParamKind::Int8), false);
    engine.parameter_reported(
        "v",
        reported("BARO1_GND_PRESS", 101_325.0, ParamKind::Real32),
        false,
    );
    engine.vehicle_booted("v", 400 * S, false);
    orders.take();
    engine.parameters_downloaded(
        "v",
        Ok(vec![
            reported("RTL_ALT", 2000.0, ParamKind::Int32),
            reported("WPNAV_SPEED", 750.0, ParamKind::Real32),
        ]),
        &BTreeMap::new(),
    );

    let history = call(
        &mut engine,
        &mut ui,
        "param/history",
        json!({ "vehicleId": "v" }),
    );
    let changes = history["result"]["changes"].as_array().unwrap();
    // This test's clock stands still, so the order among them is not tested.
    let mut seen: Vec<(String, String, Option<&str>)> = changes
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["source"].as_str().unwrap().to_owned(),
                c["reason"].as_str(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("ARSPD_USE".into(), "vehicle".into(), None),
            ("RTL_ALT".into(), "download".into(), None),
            ("RTL_ALT".into(), "other-station".into(), None),
            ("RTL_ALT".into(), "person".into(), Some("higher hills")),
            ("WPNAV_SPEED".into(), "agent".into(), None),
        ],
        "no STAT_ or ground pressure"
    );
    let person = changes.iter().find(|c| c["source"] == "person").unwrap();
    assert_eq!(person["from"], 1500.0);
    assert_eq!(person["to"], 3000.0);
    assert_eq!(person["by"]["clientKind"], "operator-ui");
    let one = call(
        &mut engine,
        &mut ui,
        "param/history",
        json!({ "vehicleId": "v", "name": "wpnav_speed" }),
    );
    assert_eq!(one["result"]["changes"].as_array().unwrap().len(), 1);
}

//! Controllers: claims, the neutral-and-confirm switching rule, one
//! controller per vehicle, release on disconnect (S12).

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
use uxduck_core::domain::command::Command;
use uxduck_core::domain::controller::GamepadButton;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem,
    Ports, Random,
};
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

/// Routes and commands the engine gave the data plane, as text.
#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<ControlRoute>>>);

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
            Some(route) => {
                self.1.lock().unwrap().push(route.clone());
                format!("route {id} {} {}", route.vehicle_id, route.mapping.version)
            }
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

fn setup() -> (Engine, Client, Orders) {
    setup_with(NoSystem::default())
}

fn setup_with(system: NoSystem) -> (Engine, Client, Orders) {
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
        system: Box::new(system),
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
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    for (id, system_id) in [("v1", 1), ("v2", 2)] {
        engine.vehicle_observed(Observation {
            id: id.into(),
            aliases: Vec::new(),
            link_id: link_id.clone(),
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
    engine.controller_connected("g1", "Xbox", true);
    orders.take();
    (engine, ui, orders)
}

fn error_kind(reply: &Value) -> &str {
    reply["error"]["data"]["kind"].as_str().unwrap_or("none")
}

fn titles(engine: &Engine) -> Vec<String> {
    engine
        .state()
        .notifications
        .values()
        .map(|n| n.title.clone())
        .collect()
}

#[test]
fn a_controller_is_claimed_at_neutral_and_switches_only_when_confirmed_at_neutral() {
    let (mut engine, mut ui, orders) = setup();
    let follow = json!({ "id": "g1", "mode": "follows-selection" });
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    let off_centre = call(&mut engine, &mut ui, "controller/claim", follow.clone());
    assert_eq!(error_kind(&off_centre), "invalid-value");
    assert!(orders.take().is_empty(), "nothing drives");

    engine.controller_neutral("g1", true);
    let claimed = call(&mut engine, &mut ui, "controller/claim", follow);
    assert_eq!(claimed["result"]["target"], "v1", "{claimed}");
    assert_eq!(orders.take(), ["route g1 v1 1"]);
    assert!(titles(&engine).contains(&"Xbox now drives Alpha".to_owned()));

    // Others may not take it: an agent never, another operator not while held.
    let mut agent = connect(&mut engine, "agent");
    let refused = call(
        &mut engine,
        &mut agent,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(error_kind(&refused), "capability-denied");
    let mut cli = connect(&mut engine, "operator-cli");
    let busy = call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(error_kind(&busy), "busy");

    // A new selection does not move it: the switch waits.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v1")
    );
    assert_eq!(
        engine.state().controllers["g1"].pending_target.as_deref(),
        Some("v2")
    );
    assert!(orders.take().is_empty(), "still driving Alpha");
    engine.controller_neutral("g1", false);
    let confirm = json!({ "id": "g1", "mode": "follows-selection" });
    let early = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        confirm.clone(),
    );
    assert_eq!(error_kind(&early), "invalid-value");
    assert!(
        early["error"]["message"]
            .as_str()
            .unwrap()
            .contains("it stays on Alpha")
    );
    engine.controller_neutral("g1", true);
    let switched = call(&mut engine, &mut ui, "controller/setTarget", confirm);
    assert_eq!(switched["result"]["target"], "v2");
    assert!(switched["result"].get("pendingTarget").is_none());
    assert_eq!(orders.take(), ["route g1 v2 1"]);
}

#[test]
fn one_controller_per_vehicle_buttons_send_commands_and_a_lost_client_releases() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("g2", "Stick", true);
    engine.controller_neutral("g1", true);
    engine.controller_neutral("g2", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    let mut cli = connect(&mut engine, "operator-cli");
    let taken = call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g2", "mode": "pinned", "vehicleId": "v1" }),
    );
    assert_eq!(error_kind(&taken), "busy");
    assert!(
        taken["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Xbox already drives Alpha")
    );
    orders.take();

    // Pinned: the selection is ignored. A mapped button sends its command.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    assert!(engine.state().controllers["g1"].pending_target.is_none());
    engine.controller_pressed("g1", Some(GamepadButton::East), None);
    engine.controller_pressed("g1", Some(GamepadButton::West), None);
    assert_eq!(orders.take(), [format!("command v1 {:?}", Command::Rtl)]);

    // The claiming client goes: released at once, and people are told.
    engine.disconnect(ui.id);
    assert_eq!(orders.take(), ["route g1 none"]);
    assert!(engine.state().controllers["g1"].claim.is_none());
    assert!(titles(&engine).iter().any(
        |t| t.starts_with("Xbox stopped driving ") && t.ends_with(": its client disconnected")
    ));

    // Unplugged while held: the same.
    call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g2", "mode": "pinned", "vehicleId": "v1" }),
    );
    orders.take();
    engine.controller_disconnected("g2");
    assert_eq!(orders.take(), ["route g2 none"]);
    let g2 = &engine.state().controllers["g2"];
    assert!(!g2.connected && g2.claim.is_none());
}

#[test]
fn a_mapping_edit_is_versioned_and_reaches_the_controllers_using_it() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    orders.take();
    let edit = |version: u64| {
        json!({
            "id": "mode-2", "expectedVersion": version, "name": "Mode 2, B lands",
            "roll": { "axis": "right-stick-x", "inverted": false },
            "pitch": { "axis": "right-stick-y", "inverted": false },
            "yaw": { "axis": "left-stick-x", "inverted": false },
            "throttle": { "axis": "left-stick-y", "inverted": false },
            "deadzone": 0.05,
            "buttons": [{ "button": "south", "command": { "type": "land" } }]
        })
    };
    let stale = call(&mut engine, &mut ui, "mapping/update", edit(7));
    assert_eq!(error_kind(&stale), "version-conflict");
    let saved = call(&mut engine, &mut ui, "mapping/update", edit(1));
    assert_eq!(saved["result"]["version"], 2, "{saved}");
    assert_eq!(orders.take(), ["route g1 v1 2"]);
    let listed = call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["input-mapping", "controller"] }),
    );
    let names: Vec<&str> = listed["result"]["snapshot"]["inputMappings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "ExpressLRS Bluetooth",
            "Mode 1",
            "Mode 2, B lands",
            "Radio (AETR)",
            "Radio (TAER)"
        ]
    );
}

#[test]
fn raw_inputs_come_from_the_host_and_change_nothing() {
    let (mut engine, mut ui, orders) = setup();
    let version = engine.state().controllers["g1"].version;
    let inputs = call(&mut engine, &mut ui, "controller/inputs", json!({}));
    assert_eq!(inputs["result"], json!({ "controllers": [] }), "{inputs}");
    assert_eq!(engine.state().controllers["g1"].version, version);
    assert!(orders.take().is_empty());
}

#[test]
fn a_throttle_range_change_needs_every_controller_released_and_copies_with_the_mapping() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    orders.take();
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    edit["throttleRange"] = json!("centre-to-top");
    let mut other_ui = connect(&mut engine, "operator-ui");
    let refused = call(&mut engine, &mut other_ui, "mapping/update", edit.clone());
    assert_eq!(error_kind(&refused), "invalid-value");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Release every controller")
    );
    assert!(orders.take().is_empty());
    assert!(!engine.state().input_mappings.contains_key("mode-2"));
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "g1" }),
    );
    let saved = call(&mut engine, &mut other_ui, "mapping/update", edit);
    assert_eq!(saved["result"]["throttleRange"], "centre-to-top", "{saved}");
    assert_eq!(saved["result"]["version"], 2);
    let copied = call(
        &mut engine,
        &mut other_ui,
        "mapping/create",
        json!({ "name": "Xbox idle", "from": "mode-2" }),
    );
    assert_eq!(copied["result"]["throttleRange"], "centre-to-top");
    // An old client omitting the range must not reset it while a controller is held.
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    let mut legacy_edit = saved["result"].clone();
    legacy_edit.as_object_mut().unwrap().remove("throttleRange");
    legacy_edit["expectedVersion"] = json!(2);
    let refused = call(&mut engine, &mut ui, "mapping/update", legacy_edit);
    assert_eq!(error_kind(&refused), "invalid-value");
}

#[test]
fn a_claim_judges_the_sticks_by_the_controllers_own_mapping() {
    // Mode 1's pitch is the left stick's Y, Mode 2's throttle: held forward,
    // the data plane (judging by Mode 2) calls it neutral.
    let pad = |left_y: f64| NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": true, "buttons": [],
            "axes": [{ "name": "Left Y", "axis": "left-stick-y", "value": left_y }],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    };
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-1" });
    let (mut engine, mut ui, orders) = setup_with(pad(1.0));
    engine.controller_neutral("g1", true);
    let refused = call(&mut engine, &mut ui, "controller/claim", claim.clone());
    assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
    assert!(orders.take().is_empty(), "nothing drives");
    // Mode 2's throttle at idle is the operator's.
    let (mut engine, mut ui, _) = setup_with(pad(-1.0));
    engine.controller_neutral("g1", true);
    let mode_2 = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-2" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", mode_2);
    assert_eq!(claimed["result"]["target"], "v1", "{claimed}");
}

#[test]
fn a_mapping_of_its_own_is_a_copy_named_by_its_maker() {
    let (mut engine, mut ui, _) = setup();
    let created = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "TBS Tango 2", "from": "mode-1" }),
    );
    let mapping = &created["result"];
    assert_eq!(mapping["id"], "tbs-tango-2", "{created}");
    assert_eq!(mapping["name"], "TBS Tango 2");
    assert_eq!(mapping["version"], 1);
    assert_eq!(mapping["throttle"]["axis"], "right-stick-y", "Mode 1's");
    let again = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "TBS Tango 2" }),
    );
    assert_eq!(
        again["result"]["id"], "tbs-tango-2-2",
        "numbered when taken"
    );
    assert_eq!(
        again["result"]["throttle"]["axis"], "left-stick-y",
        "Mode 2 by default"
    );
    assert!(engine.state().input_mappings.contains_key("tbs-tango-2"));
    let unnamed = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": " " }),
    );
    assert_eq!(error_kind(&unnamed), "invalid-value");
}

#[test]
fn a_radio_starts_on_the_radio_mapping() {
    let (mut engine, _ui, _) = setup();
    assert_eq!(
        engine.state().controllers["g1"].mapping_id,
        "mode-2",
        "a gamepad"
    );

    engine.controller_connected("r1", "TBS Joystick", false);
    assert_eq!(engine.state().controllers["r1"].mapping_id, "radio");
    engine.controller_connected("e1", "ExpressLRS Joystick", false);
    assert_eq!(
        engine.state().controllers["e1"].mapping_id,
        "expresslrs-bluetooth"
    );

    // Known from before the radio mapping, on Mode 2: moved to it.
    engine.controller_disconnected("g1");
    engine.controller_connected("g1", "Xbox", false);
    assert_eq!(engine.state().controllers["g1"].mapping_id, "radio");
}

#[test]
fn a_radio_keeps_a_mapping_that_reads_it_by_code() {
    let (mut engine, mut ui, _) = setup();
    engine.controller_connected("r1", "Tango", false);
    let created = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "Tango", "from": "radio" }),
    );
    assert_eq!(created["result"]["roll"]["code"], 65584, "{created}");
    let claim = json!({ "id": "r1", "mode": "pinned", "vehicleId": "v1", "mappingId": "tango" });
    engine.controller_neutral("r1", true);
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim)
            .get("result")
            .is_some()
    );
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "r1" }),
    );
    engine.controller_disconnected("r1");
    engine.controller_connected("r1", "Tango", false);
    assert_eq!(engine.state().controllers["r1"].mapping_id, "tango");
}

#[test]
fn a_calibration_must_have_its_ends_either_side_of_its_centre() {
    let (mut engine, mut ui, _) = setup();
    let listed = call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["input-mapping"] }),
    );
    let mut radio = listed["result"]["snapshot"]["inputMappings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "radio")
        .unwrap()
        .clone();
    radio["expectedVersion"] = radio["version"].clone();
    radio["throttle"]["calibration"] = json!({ "min": -0.98, "centre": 0.0, "max": 0.0 });
    let refused = call(&mut engine, &mut ui, "mapping/update", radio.clone());
    assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
    radio["throttle"]["calibration"] = json!({ "min": -0.98, "centre": -0.01, "max": 0.97 });
    let saved = call(&mut engine, &mut ui, "mapping/update", radio);
    assert_eq!(
        saved["result"]["throttle"]["calibration"]["min"], -0.98,
        "{saved}"
    );
}

#[test]
fn the_sticks_of_a_mapping_in_use_cannot_change() {
    let (mut engine, mut ui, _) = setup();
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" });
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim)
            .get("result")
            .is_some()
    );
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    for (field, value) in [
        (
            "throttle",
            json!({ "axis": "left-stick-y", "inverted": false,
            "calibration": { "min": -0.98, "centre": 0.0, "max": 0.97 } }),
        ),
        (
            "pitch",
            json!({ "axis": "right-stick-y", "inverted": true }),
        ),
        ("deadzone", json!(0.2)),
    ] {
        let mut changed = edit.clone();
        changed[field] = value;
        let refused = call(&mut engine, &mut ui, "mapping/update", changed);
        assert!(
            refused["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("before changing its sticks")),
            "{field}: {refused}"
        );
    }
    // Its buttons can.
    edit["buttons"] = json!([]);
    let saved = call(&mut engine, &mut ui, "mapping/update", edit);
    assert_eq!(saved["result"]["version"], 2, "{saved}");
}

#[test]
fn a_refused_claim_says_which_channels_and_why() {
    // A radio: no known layout, so Mode 2's gamepad sticks read nothing of it.
    let radio = |x: f64| NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": false, "buttons": [],
            "axes": [
                { "code": 65584, "name": "X", "number": 1, "value": x },
                { "code": 65585, "name": "Y", "number": 2, "value": 0.0 },
                { "code": 65586, "name": "Z", "number": 3, "value": -1.0 },
                { "code": 65587, "name": "Rx", "number": 4, "value": 0.0 },
            ],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    };
    let claim = |mapping: &str| json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": mapping });
    let (mut engine, mut ui, _) = setup_with(radio(0.4));
    engine.controller_neutral("g1", true);
    let refused = call(&mut engine, &mut ui, "controller/claim", claim("mode-2"));
    assert_eq!(
        refused["error"]["message"],
        "Mode 2 doesn't read Xbox's roll, pitch, throttle and yaw. Make a mapping for Xbox \
         on the Mapping page, then move each stick to set it.",
        "{refused}"
    );
    // The built-in radio mapping reads it by its codes.
    let off = call(&mut engine, &mut ui, "controller/claim", claim("radio"));
    assert_eq!(
        off["error"]["message"],
        "Centre roll (it reads 0.37) on Xbox before using it. Throttle can stay where it is.",
        "{off}"
    );
}

#[test]
fn a_flying_copter_is_not_taken_over_with_the_throttle_at_idle() {
    // A radio with roll, pitch and yaw centred and its throttle (Z) down.
    let (mut engine, mut ui, _) = setup_with(NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": false, "buttons": [],
            "axes": [
                { "code": 65584, "name": "X", "number": 1, "value": 0.0 },
                { "code": 65585, "name": "Y", "number": 2, "value": 0.0 },
                { "code": 65586, "name": "Z", "number": 3, "value": -1.0 },
                { "code": 65587, "name": "Rx", "number": 4, "value": 0.0 },
            ],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    });
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "radio" });

    // On the ground, the throttle may be anywhere.
    let on_ground = call(&mut engine, &mut ui, "controller/claim", claim.clone());
    assert!(on_ground.get("result").is_some(), "{on_ground}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "g1" }),
    );

    // Armed at 30 m: refused, saying why.
    let link_id = engine.state().vehicles["v1"].link_id.clone();
    engine.vehicle_observed(Observation {
        id: "v1".into(),
        aliases: Vec::new(),
        link_id,
        peer: "127.0.0.1:5761".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 6,
        armed: true,
        mode: None,
    });
    engine.telemetry_observed(uxduck_core::domain::telemetry::VehicleTelemetry {
        vehicle_id: "v1".into(),
        positions: vec![uxduck_core::domain::telemetry::PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: 37.79,
            longitude_deg: -122.48,
            altitudes: vec![uxduck_core::domain::telemetry::Altitude {
                meters: 30.0,
                frame: uxduck_core::domain::telemetry::AltitudeFrame::AboveHome,
            }],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: 7,
        }],
        items: vec![],
    });
    let refused = call(&mut engine, &mut ui, "controller/claim", claim);
    assert_eq!(
        refused["error"]["message"],
        "Alpha is in the air. Raise the throttle on Xbox to about the middle before using \
         it, or it would descend at once.",
        "{refused}"
    );
}

#[test]
fn a_position_of_an_axis_sends_its_command_to_the_target() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    let mut mode_2 = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    let position = json!({ "axis": "left-stick-x", "code": 65589, "at": 0.2,
                           "command": { "type": "set-mode", "mode": "LOITER" } });
    mode_2["positions"] = json!([position]);
    mode_2["expectedVersion"] = json!(1);
    let saved = call(&mut engine, &mut ui, "mapping/update", mode_2.clone());
    assert_eq!(saved["result"]["positions"][0]["at"], 0.2, "{saved}");
    mode_2["positions"][0]["at"] = json!(1.5);
    mode_2["expectedVersion"] = json!(2);
    let refused = call(&mut engine, &mut ui, "mapping/update", mode_2);
    assert_eq!(error_kind(&refused), "invalid-value");

    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-2" });
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim)
            .get("result")
            .is_some()
    );
    orders.take();
    let held = engine.state().input_mappings["mode-2"].positions[0].clone();
    engine.controller_positioned("g1", &held);
    let sent = orders.take();
    assert!(
        sent.iter()
            .any(|o| o.starts_with("command v1") && o.contains("LOITER")),
        "{sent:?}"
    );
    let mut stale = held;
    stale.at = 0.6;
    engine.controller_positioned("g1", &stale);
    assert!(
        orders.take().is_empty(),
        "not in the mapping now: nothing sent"
    );
}

#[test]
fn extra_rc_channels_validate_and_cannot_change_while_claimed() {
    let (mut engine, mut ui, _) = setup();
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    let binding =
        |channel| json!({ "channel": channel, "input": { "axis": "left-z", "inverted": false } });
    for channels in [
        json!([binding(4)]),
        json!([binding(17)]),
        json!([binding(9), binding(9)]),
    ] {
        edit["rcChannels"] = channels;
        let refused = call(&mut engine, &mut ui, "mapping/update", edit.clone());
        assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
    }
    edit["rcChannels"] = json!([binding(9)]);
    let saved = call(&mut engine, &mut ui, "mapping/update", edit.clone());
    assert_eq!(saved["result"]["rcChannels"], json!([binding(9)]));
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    edit["expectedVersion"] = json!(2);
    edit["rcChannels"] = json!([]);
    let refused = call(&mut engine, &mut ui, "mapping/update", edit.clone());
    assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    let saved = call(&mut engine, &mut ui, "mapping/update", edit);
    assert!(saved.get("result").is_some(), "{saved}");
}

#[test]
fn an_unplugged_controller_can_be_forgotten_and_comes_back_on_its_fitting_mapping() {
    let (mut engine, mut ui, _) = setup();
    engine.controller_connected("r1", "Tango", false);
    let plugged = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({ "id": "r1" }),
    );
    assert_eq!(error_kind(&plugged), "busy", "{plugged}");
    assert_eq!(
        plugged["error"]["message"], "Unplug Tango before forgetting it.",
        "{plugged}"
    );
    assert!(engine.state().controllers.contains_key("r1"));

    // Remembered on a mapping of its own, then unplugged.
    call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "Tango", "from": "radio" }),
    );
    let claim = json!({ "id": "r1", "mode": "pinned", "vehicleId": "v1", "mappingId": "tango" });
    engine.controller_neutral("r1", true);
    let claimed = call(&mut engine, &mut ui, "controller/claim", claim);
    assert!(claimed.get("result").is_some(), "{claimed}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "r1" }),
    );
    engine.controller_disconnected("r1");
    assert_eq!(engine.state().controllers["r1"].mapping_id, "tango");

    let forgotten = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({ "id": "r1" }),
    );
    assert_eq!(forgotten["result"], json!({}), "{forgotten}");
    assert!(!engine.state().controllers.contains_key("r1"));

    let unknown = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({ "id": "r1" }),
    );
    assert_eq!(error_kind(&unknown), "not-found", "{unknown}");

    // Plugged in again, it is new: on the radio mapping, as any radio is.
    engine.controller_connected("r1", "Tango", false);
    let back = &engine.state().controllers["r1"];
    assert!(back.connected);
    assert!(back.claim.is_none());
    assert_eq!(back.mapping_id, "radio");
}

#[test]
fn pilot_input_preferences_persist_and_cannot_change_a_claimed_or_pending_target() {
    use uxduck_core::domain::settings::PilotInputMethod;
    let (mut engine, mut ui, orders) = setup();
    let update = |version, vehicle, method| json!({"expectedVersion":version,"pilotInput":{"vehicleId":vehicle,"method":method}});
    let saved = call(
        &mut engine,
        &mut ui,
        "settings/update",
        update(0, "v1", "manual-control"),
    );
    assert_eq!(
        saved["result"]["pilotInputMethods"],
        json!([{"vehicleId":"v1","method":"manual-control"}])
    );
    let encoded = serde_json::to_string(engine.state()).unwrap();
    let restored: uxduck_engine::State = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        restored.settings.pilot_input_methods["v1"],
        PilotInputMethod::ManualControl
    );
    assert!(!restored.settings.pilot_input_methods.contains_key("v2"));
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({"vehicleId":"v1"}),
    );
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"follows-selection","vehicleId":"v1"}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    let refused = call(
        &mut engine,
        &mut ui,
        "settings/update",
        update(1, "v1", "rc-channels-override"),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    assert_eq!(engine.state().settings.version, 1);
    assert_eq!(
        orders.1.lock().unwrap().last().unwrap().pilot_input_method,
        PilotInputMethod::ManualControl
    );
    engine.controller_neutral("g1", false);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({"vehicleId":"v2"}),
    );
    assert_eq!(
        engine.state().controllers["g1"].pending_target.as_deref(),
        Some("v2")
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "settings/update",
        update(1, "v2", "manual-control"),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    let saved = call(
        &mut engine,
        &mut ui,
        "settings/update",
        update(1, "v1", "automatic"),
    );
    assert!(saved.get("result").is_some(), "{saved}");
    assert!(engine.state().settings.pilot_input_methods.is_empty());
}

#[test]
fn manual_control_rejects_extra_rc_bindings_instead_of_silently_ignoring_them() {
    let (mut engine, mut ui, _) = setup();
    let result = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({"expectedVersion":0,"pilotInput":{"vehicleId":"v1","method":"manual-control"}}),
    );
    assert!(result.get("result").is_some(), "{result}");
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    edit["rcChannels"] = json!([{"channel":9,"input":{"axis":"left-z","inverted":false}}]);
    let result = call(&mut engine, &mut ui, "mapping/update", edit);
    assert!(result.get("result").is_some(), "{result}");
    engine.controller_neutral("g1", true);
    let result = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    assert_eq!(error_kind(&result), "invalid-value", "{result}");
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("RC_CHANNELS_OVERRIDE")
    );
}

#[test]
fn pilot_input_selection_refuses_unhandled_firmware_and_preserves_other_settings() {
    let (mut engine, mut ui, _) = setup();
    for (autopilot, kind) in [("Ardupilotmega", "Airship"), ("Generic", "Quadrotor")] {
        let v = engine.state().vehicles["v1"].clone();
        engine.vehicle_observed(Observation {
            id: v.id,
            aliases: v.aliases,
            link_id: v.link_id,
            peer: v.peer,
            system_id: v.system_id,
            component_ids: v.component_ids,
            autopilot: autopilot.into(),
            vehicle_type: kind.into(),
            uid: v.uid,
            firmware: v.firmware,
            firmware_commit: v.firmware_commit,
            presence: v.presence,
            last_heartbeat_us: v.last_heartbeat_us + 1,
            armed: v.armed,
            mode: v.mode,
        });
        let refused = call(
            &mut engine,
            &mut ui,
            "settings/update",
            json!({"expectedVersion":0,"pilotInput":{"vehicleId":"v1","method":"manual-control"}, "usageAnalytics":false}),
        );
        assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
        assert_eq!(engine.state().settings.version, 0);
        assert!(engine.state().settings.usage_analytics);
        assert!(engine.state().settings.pilot_input_methods.is_empty());
    }
}

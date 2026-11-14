//! Controllers: claims, automatic switching at neutral, one
//! controller per vehicle, release on disconnect after a window's grace (S12).

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
use uxduck_core::domain::notification::Action;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::controller_input::{Controls, InputEvent};
use uxduck_engine::controllers::CLAIM_GRACE_US;
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

/// Routes and commands the engine gave the data plane, as text, and the
/// keyboard frames it passed on.
#[derive(Clone, Default)]
struct Orders(
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<ControlRoute>>>,
    Arc<Mutex<Vec<[f64; 4]>>>,
);

impl Orders {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
    fn keyboard_frames(&self) -> Vec<[f64; 4]> {
        std::mem::take(&mut *self.2.lock().unwrap())
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
    fn controller_input(&mut self, event: InputEvent) {
        if let InputEvent::Keyboard { axes, .. } = event {
            self.2.lock().unwrap().push(axes);
        }
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
    connect_as(engine, kind, Trust::LocalUser)
}

fn connect_as(engine: &mut Engine, kind: &str, trust: Trust) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, trust, outbox);
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
    setup_with_state(system, uxduck_engine::state::State::default())
}

fn load_state(system: NoSystem, state: uxduck_engine::state::State) -> (Engine, Client, Orders) {
    let orders = Orders::default();
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities {
            simulator: true,
            ..DaemonCapabilities::default()
        },
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
        links: Box::new(orders.clone()),
        system: Box::new(system),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: agreed(state),
            sessions: vec![],
        },
    );
    let ui = connect(&mut engine, "operator-ui");
    (engine, ui, orders)
}

fn setup_with_state(
    system: NoSystem,
    state: uxduck_engine::state::State,
) -> (Engine, Client, Orders) {
    let (mut engine, mut ui, orders) = load_state(system, state);
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    for (id, system_id) in [("v1", 1), ("v2", 2)] {
        engine.vehicle_observed(Observation {
            connections: None,
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

#[test]
fn disconnected_local_sessions_retire_records_and_presence_without_resetting_mappings() {
    let mut state = uxduck_engine::State::default();
    let mut mapping = uxduck_core::domain::controller::InputMapping::mode_2();
    mapping.id = "kept-calibration".into();
    mapping.roll.calibration = Some(uxduck_core::domain::controller::AxisCalibration {
        min: -0.9,
        centre: 0.1,
        max: 0.8,
    });
    state
        .input_mappings
        .insert(mapping.id.clone(), mapping.clone());
    let (mut engine, mut ui, orders) = setup_with_state(NoSystem::default(), state);
    let ana = as_ana(&mut engine, &mut ui);
    engine.controller_connected("unit-other", "Other pad", true);
    let other = engine.state().controllers["unit-other"].clone();
    let mappings = engine.state().input_mappings.clone();
    let notices = engine.state().notifications.clone();
    for i in 0..64 {
        let id = format!("gp-session-local-{i}");
        engine.controller_connected(&id, "Radio", true);
        let chosen = call(
            &mut engine,
            &mut ui,
            "controller/chooseMapping",
            json!({
                "id": id, "vehicleId": "v2", "mappingId": mapping.id,
            }),
        );
        assert!(chosen.get("error").is_none(), "{chosen}");
        engine.controller_disconnected(&id);
        assert!(!engine.state().controllers.contains_key(&id));
        assert_eq!(engine.state().controllers["unit-other"], other);
        assert_eq!(engine.state().input_mappings, mappings);
        assert_eq!(engine.state().notifications, notices);
        assert!(orders.take().is_empty());
    }
    let operator = &engine.state().operators[&ana];
    assert_eq!(operator.controller_mappings.len(), 64);
    assert_eq!(
        operator.mapping_for("gp-session-next", "Radio", &["v2"]),
        Some(mapping.id.as_str())
    );
    engine.controller_connected("gp-session-next", "Radio", true);
    let fresh = &engine.state().controllers["gp-session-next"];
    assert!(fresh.claim.is_none() && fresh.target.is_none() && fresh.last_target.is_none());
    assert!(orders.take().is_empty());
}

#[test]
fn a_session_keeps_its_detached_claim_until_disconnect_releases_before_pruning() {
    let (mut engine, mut ui, orders) = setup();
    let id = "gp-session-held";
    engine.controller_connected(id, "Session pad", true);
    engine.controller_neutral(id, true);
    let claim = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({
            "id": id, "mode": "pinned", "vehicleId": "v1",
        }),
    );
    assert!(claim.get("error").is_none(), "{claim}");
    let mappings = engine.state().input_mappings.clone();
    orders.take();
    engine.disconnect(ui.id);
    engine.expire_detached_claims(1_000 + CLAIM_GRACE_US - 1);
    assert!(engine.state().controllers[id].claim.is_some());
    assert!(orders.take().is_empty());
    let mut other = connect(&mut engine, "operator-ui");
    let refused = call(
        &mut engine,
        &mut other,
        "controller/claim",
        json!({
            "id": "g1", "mode": "pinned", "vehicleId": "v1",
        }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    engine.controller_disconnected(id);
    assert_eq!(orders.take(), [format!("route {id} none")]);
    assert!(!engine.state().controllers.contains_key(id));
    assert_eq!(engine.state().input_mappings, mappings);
    assert!(
        titles(&engine)
            .iter()
            .any(|title| title.starts_with("Session pad stopped driving ")
                && title.ends_with(": it was unplugged"))
    );
    assert!(
        !engine.state().notifications.values().any(
            |notice| notice.condition.as_deref() == Some("controller-presence:gp-session-held")
        )
    );
    let accepted = call(
        &mut engine,
        &mut other,
        "controller/claim",
        json!({
            "id": "g1", "mode": "pinned", "vehicleId": "v1",
        }),
    );
    assert!(accepted.get("error").is_none(), "{accepted}");
}

#[test]
fn restarts_retire_old_session_records_but_preserve_synced_mapping_refs_and_calibration() {
    let (mut engine, mut ui, _) = setup();
    let ana = as_ana(&mut engine, &mut ui);
    let mut state = engine.state().clone();
    let operator = state.operators.get_mut(&ana).unwrap();
    operator.choose_mapping("gp-session-foreign", "Radio", Some("v1"), "mode-1");
    operator.choose_mapping("gp-session-another-computer", "Radio", Some("v1"), "mode-2");
    let kept_refs = operator.controller_mappings.clone();
    let mut mapping = uxduck_core::domain::controller::InputMapping::mode_2();
    mapping.id = "saved-radio".into();
    mapping.pitch.calibration = Some(uxduck_core::domain::controller::AxisCalibration {
        min: -0.8,
        centre: 0.1,
        max: 0.9,
    });
    state.input_mappings.insert(mapping.id.clone(), mapping);
    let mut legacy = state.controllers["g1"].clone();
    legacy.id = "gp-030000005e0400008e02000000007801".into();
    state.controllers.insert(legacy.id.clone(), legacy);
    for generation in 0..4 {
        let (mut engine, _, orders) = load_state(NoSystem::default(), state);
        assert!(
            engine
                .state()
                .controllers
                .keys()
                .all(|id| !id.starts_with("gp-session-"))
        );
        assert!(
            engine
                .state()
                .controllers
                .contains_key("gp-030000005e0400008e02000000007801")
        );
        assert_eq!(
            engine.state().operators[&ana].controller_mappings,
            kept_refs
        );
        let mappings = engine.state().input_mappings.clone();
        for index in 0..16 {
            engine.controller_connected(&format!("gp-session-{generation}-{index}"), "Radio", true);
        }
        state = serde_json::from_str(&serde_json::to_string(engine.state()).unwrap()).unwrap();
        for (index, controller) in state
            .controllers
            .values_mut()
            .filter(|c| c.id.starts_with("gp-session-"))
            .enumerate()
        {
            controller.connected = index % 2 == 0;
            if index % 3 == 0 {
                controller.claim = Some(uxduck_core::domain::controller::Claim {
                    client_id: "closed-window".into(),
                    client_name: "Old window".into(),
                    operator_id: Some(ana.clone()),
                });
                controller.target = Some("v1".into());
                controller.pending_target = Some("v2".into());
            }
        }
        let (restarted, _, _) = load_state(NoSystem::default(), state);
        assert!(
            restarted
                .state()
                .controllers
                .keys()
                .all(|id| !id.starts_with("gp-session-"))
        );
        assert!(!restarted.state().notifications.values().any(|notice| {
            notice
                .condition
                .as_deref()
                .is_some_and(|condition| condition.starts_with("controller-presence:gp-session-"))
        }));
        assert_eq!(restarted.state().input_mappings, mappings);
        assert_eq!(
            restarted.state().operators[&ana].controller_mappings,
            kept_refs
        );
        assert!(orders.take().is_empty());
        state = restarted.state().clone();
    }
}

#[test]
fn a_promoted_alias_cannot_admit_a_second_controller_for_the_same_aircraft() {
    let (mut engine, mut ui, orders) = setup();
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    let vehicle = engine.state().vehicles["v2"].clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "lasting-vid".into(),
        aliases: vec!["v2".into()],
        link_id: vehicle.link_id,
        peer: vehicle.peer,
        system_id: vehicle.system_id,
        component_ids: vehicle.component_ids,
        autopilot: vehicle.autopilot,
        vehicle_type: vehicle.vehicle_type,
        uid: Some("unit-uid".into()),
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 7,
        armed: false,
        mode: None,
    });
    engine.controller_connected("g2", "Second controller", true);
    orders.take();
    let refused = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g2", "mode": "pinned", "vehicleId": "lasting-vid" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    assert!(engine.state().controllers["g2"].claim.is_none());
    assert!(orders.take().is_empty());
}

#[test]
fn native_and_browser_model_ids_never_persist_a_physical_controllers_pin() {
    let (mut engine, mut ui, orders) = setup();
    for id in [
        "g1",
        "keyboard:page",
        "unknown-id",
        "gp-030000005e0400008e02000000007801",
        "gp-0",
        "web-045e-028e",
    ] {
        engine.controller_connected(id, "Controller model", true);
        orders.take();
        let answer = call(
            &mut engine,
            &mut ui,
            "controller/setTarget",
            json!({ "id": id, "mode": "pinned", "vehicleId": "v2" }),
        );
        if id.starts_with("keyboard:") {
            assert_eq!(error_kind(&answer), "capability-denied", "{answer}");
        } else {
            assert!(answer.get("error").is_none(), "{answer}");
            assert!(answer["result"]["lastTarget"].is_null(), "{id}: {answer}");
        }
        assert!(engine.state().controllers[id].last_target.is_none());
        assert!(orders.take().is_empty());
    }
}

fn offline_vehicle(engine: &mut Engine, id: &str) {
    let vehicle = engine.state().vehicles[id].clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: vehicle.id,
        aliases: vehicle.aliases,
        link_id: vehicle.link_id,
        peer: vehicle.peer,
        system_id: vehicle.system_id,
        component_ids: vehicle.component_ids,
        autopilot: vehicle.autopilot,
        vehicle_type: vehicle.vehicle_type,
        uid: vehicle.uid,
        firmware: vehicle.firmware,
        firmware_commit: vehicle.firmware_commit,
        presence: Presence::Offline,
        last_heartbeat_us: vehicle.last_heartbeat_us,
        armed: vehicle.armed,
        mode: vehicle.mode,
    });
    assert_eq!(engine.state().vehicles[id].presence, Presence::Offline);
}

#[test]
fn a_saved_target_is_a_binding_not_a_claim_or_a_control_route() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("unit-g1", "Xbox", true);
    offline_vehicle(&mut engine, "v2");
    orders.take();
    let pinned = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({
            "id": "unit-g1", "mode": "pinned", "vehicleId": "v2"
        }),
    );
    assert_eq!(
        pinned["result"]["lastTarget"],
        json!({ "mode": "pinned", "vehicleId": "v2" }),
        "{pinned}"
    );
    assert!(pinned["result"]["claim"].is_null());
    assert!(pinned["result"]["target"].is_null());
    assert!(orders.take().is_empty());
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    engine.controller_disconnected("unit-g1");
    engine.controller_connected("unit-g1", "Xbox", true);
    assert!(
        orders
            .take()
            .iter()
            .all(|order| order == "route unit-g1 none")
    );
    let stored = serde_json::to_value(engine.state()).unwrap();
    assert_eq!(
        stored["controllers"]["unit-g1"]["lastTarget"]["vehicleId"],
        "v2"
    );
    let mut agent = connect(&mut engine, "agent");
    let denied = call(
        &mut engine,
        &mut agent,
        "controller/setTarget",
        json!({ "id": "unit-g1", "mode": "follows-selection" }),
    );
    assert_eq!(error_kind(&denied), "capability-denied");
    let selected = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({ "id": "unit-g1", "mode": "follows-selection" }),
    );
    assert_eq!(
        selected["result"]["lastTarget"],
        json!({ "mode": "follows-selection" })
    );
    assert!(orders.take().is_empty());
}

#[test]
fn release_keeps_the_last_pin_and_a_missing_vehicle_does_not_redirect_it() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("unit-g1", "Xbox", true);
    let pinned = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "unit-g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(
        pinned["result"]["lastTarget"]["vehicleId"], "v2",
        "{pinned}"
    );
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "unit-g1" }),
    );
    let kept = serde_json::to_value(&engine.state().controllers["unit-g1"]).unwrap();
    assert_eq!(kept["lastTarget"]["vehicleId"], "v2");
    assert!(kept["target"].is_null());
    offline_vehicle(&mut engine, "v2");
    let hidden = call(
        &mut engine,
        &mut ui,
        "vehicle/hide",
        json!({ "id": "v2", "hidden": true }),
    );
    assert!(hidden.get("error").is_none(), "{hidden}");
    let removed = call(
        &mut engine,
        &mut ui,
        "vehicle/delete",
        json!({ "id": "v2" }),
    );
    assert!(removed.get("error").is_none(), "{removed}");
    assert!(engine.vehicle("v2").is_none());
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    orders.take();
    let refused = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "unit-g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(error_kind(&refused), "not-found", "{refused}");
    assert!(orders.take().is_empty());
    assert!(engine.state().controllers["unit-g1"].claim.is_none());
    assert_eq!(
        serde_json::to_value(&engine.state().controllers["unit-g1"]).unwrap()["lastTarget"]["vehicleId"],
        "v2"
    );
}

#[test]
fn transient_native_indices_never_remember_a_pin_for_the_next_device() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("gp-0", "Unknown device", true);
    orders.take();
    let answer = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({ "id": "gp-0", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert!(answer.get("error").is_none(), "{answer}");
    assert!(answer["result"]["lastTarget"].is_null());
    assert!(answer["result"]["claim"].is_null());
    assert!(orders.take().is_empty());
}

#[test]
fn a_remembered_pin_promotes_to_the_vehicles_lasting_identity_without_routing() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("unit-g1", "Xbox", true);
    call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({ "id": "unit-g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    let vehicle = engine.state().vehicles["v2"].clone();
    orders.take();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "lasting-uid-vid".into(),
        aliases: vec!["v2".into()],
        link_id: vehicle.link_id,
        peer: vehicle.peer,
        system_id: 1,
        component_ids: vehicle.component_ids,
        autopilot: vehicle.autopilot,
        vehicle_type: vehicle.vehicle_type,
        uid: Some("board-uid".into()),
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 7,
        armed: false,
        mode: None,
    });
    let controller = serde_json::to_value(&engine.state().controllers["unit-g1"]).unwrap();
    assert_eq!(controller["lastTarget"]["vehicleId"], "lasting-uid-vid");
    assert!(controller["claim"].is_null() && controller["target"].is_null());
    assert!(orders.take().is_empty());
    let claim = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "unit-g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(claim["result"]["target"], "lasting-uid-vid", "{claim}");
    assert_eq!(orders.take(), ["route unit-g1 lasting-uid-vid 1"]);
}

/// Vehicle v3 on v1's link, of this autopilot and type.
fn observe_v3(engine: &mut Engine, autopilot: &str, vehicle_type: &str, armed: bool) {
    let link_id = engine.state().vehicles["v1"].link_id.clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "v3".into(),
        aliases: Vec::new(),
        link_id,
        peer: "127.0.0.1:5763".into(),
        system_id: 3,
        component_ids: vec![1],
        autopilot: autopilot.into(),
        vehicle_type: vehicle_type.into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed,
        mode: None,
    });
}

/// Vehicles whose throttle centres, with what it is called there: an
/// autopilot neither `ArduPilot` nor PX4 is driven as `ArduPilot` is.
const CENTRED: [(&str, &str, &str); 7] = [
    ("Ardupilotmega", "GroundRover", "forward / reverse"),
    ("Ardupilotmega", "SurfaceBoat", "forward / reverse"),
    ("Generic", "GroundRover", "forward / reverse"),
    ("Px4", "GroundRover", "forward / reverse"),
    ("Px4", "SurfaceBoat", "forward / reverse"),
    ("Ardupilotmega", "Submarine", "ascend / descend"),
    ("Ardupilotmega", "Airship", "ascend / descend"),
];

/// The preflight's controller check on v3, of this autopilot and type,
/// with Xbox claimed for it on `mode-2` and its left stick's up and down
/// (throttle) at `throttle`, the rest centred; `prepare` runs first.
fn controller_check(
    autopilot: &str,
    vehicle_type: &str,
    armed: bool,
    throttle: f64,
    prepare: impl FnOnce(&mut Engine, &mut Client),
) -> String {
    claimed_check(
        "uxduck:controller",
        autopilot,
        vehicle_type,
        armed,
        throttle,
        prepare,
    )
}

/// One of the preflight's checks on v3, with Xbox claimed for it as for
/// `controller_check`.
fn claimed_check(
    id: &str,
    autopilot: &str,
    vehicle_type: &str,
    armed: bool,
    throttle: f64,
    prepare: impl FnOnce(&mut Engine, &mut Client),
) -> String {
    let (mut engine, mut ui, _) = setup_with(NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": true, "buttons": [],
            "axes": [
                { "name": "Right X", "axis": "right-stick-x", "value": 0.0 },
                { "name": "Right Y", "axis": "right-stick-y", "value": 0.0 },
                { "name": "Left X", "axis": "left-stick-x", "value": 0.0 },
                { "name": "Left Y", "axis": "left-stick-y", "value": throttle },
            ],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    });
    observe_v3(&mut engine, autopilot, vehicle_type, armed);
    prepare(&mut engine, &mut ui);
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v3", "mappingId": "mode-2" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", claim);
    assert_eq!(claimed["result"]["target"], "v3", "{claimed}");
    engine.review_preflights();
    let state = call(
        &mut engine,
        &mut ui,
        "preflight/state",
        json!({ "vehicleId": "v3" }),
    );
    let check = state["result"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("{state}"))
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("{state}"))
        .clone();
    format!(
        "{}: {}",
        check["status"].as_str().unwrap(),
        check["detail"].as_str().unwrap()
    )
}

/// A vehicle counts UXDuck's stick input as RC, and its failsafe does too,
/// so a controller driving it from UXDuck passes "Controller input"
/// (maintainer, 2026-11-13). Asked for its own receiver alone, its word
/// that it is receiving does not say that receiver works. Asked for the
/// link alone, the controller is what passes it.
#[test]
fn controller_input_passes_with_a_controller_from_uxduck_unless_rc_alone_is_asked_for() {
    let receiving = |input: Option<&'static str>| {
        claimed_check(
            "uxduck:rc-receiver",
            "Ardupilotmega",
            "GroundRover",
            false,
            0.0,
            move |engine, ui| {
                if let Some(input) = input {
                    let set = call(
                        engine,
                        ui,
                        "preflight/configure",
                        json!({ "vehicleId": "v3", "check": "rc-receiver", "input": input }),
                    );
                    assert!(set.get("error").is_none(), "{set}");
                }
                engine.telemetry_observed(uxduck_core::domain::telemetry::VehicleTelemetry {
                    vehicle_id: "v3".into(),
                    positions: Vec::new(),
                    items: vec![uxduck_core::domain::telemetry::TelemetryItem {
                        key: "rc.status".into(),
                        label: "RC receiver".into(),
                        unit: uxduck_core::domain::telemetry::Unit::None,
                        value: None,
                        text: Some("Receiving".into()),
                        source: "rc".into(),
                        component_id: 1,
                        time_us: 1_000,
                    }],
                });
            },
        )
    };
    let name = "Charlie";
    assert_eq!(
        receiving(None),
        format!("pass: {name} takes Xbox's input from UXDuck as RC.")
    );
    assert_eq!(
        receiving(Some("rc")),
        format!(
            "unknown: {name} takes Xbox's input from UXDuck as RC, so it can't say whether its own receiver works."
        )
    );
    assert_eq!(
        receiving(Some("link")),
        format!("pass: {name} takes Xbox's input from UXDuck.")
    );
}

/// Forward / reverse and ascend / descend rest mid-stick, so the
/// preflight's controller check fails when a controller holds them either
/// way, full reverse included, as it passed before; an aircraft's throttle
/// still passes low and fails raised.
#[test]
fn the_controller_check_wants_a_centred_throttle_in_the_middle_and_an_aircraft_s_low() {
    let check = |autopilot: &str, vehicle_type: &str, throttle: f64| {
        controller_check(autopilot, vehicle_type, false, throttle, |_, _| {})
    };
    let neutral = "pass: Xbox at neutral.";
    for (autopilot, vehicle_type, control) in CENTRED {
        let off = format!("fail: Centre {control} on Xbox.");
        for (throttle, expected) in [(-1.0, off.as_str()), (1.0, off.as_str()), (0.0, neutral)] {
            assert_eq!(
                check(autopilot, vehicle_type, throttle),
                expected,
                "{autopilot} {vehicle_type} at {throttle}"
            );
        }
    }
    for (autopilot, vehicle_type) in [
        ("Ardupilotmega", "Quadrotor"),
        ("Ardupilotmega", "FixedWing"),
        ("Px4", "Quadrotor"),
    ] {
        for (throttle, expected) in [
            (-1.0, neutral),
            (0.0, neutral),
            (1.0, "fail: Lower the throttle on Xbox."),
        ] {
            assert_eq!(
                check(autopilot, vehicle_type, throttle),
                expected,
                "{autopilot} {vehicle_type} at {throttle}"
            );
        }
    }
}

/// Where the throttle centres, keyboard control starts at neutral,
/// armed or not, and a start away from it reaches neither a route nor the
/// data plane; an aircraft's keyboard still starts where the page says.
#[test]
fn keyboard_control_starts_a_centred_throttle_at_neutral() {
    for (autopilot, vehicle_type, control) in CENTRED {
        for armed in [true, false] {
            let (mut engine, mut ui, orders) = setup();
            observe_v3(&mut engine, autopilot, vehicle_type, armed);
            for throttle in [-1.0, 1.0, -0.1, 0.1] {
                let refused = call(
                    &mut engine,
                    &mut ui,
                    "controller/keyboardStart",
                    json!({ "vehicleId": "v3", "throttle": throttle }),
                );
                assert_eq!(
                    refused["error"]["message"],
                    format!("Start keyboard control with {control} at neutral."),
                    "{autopilot} {vehicle_type} at {throttle}: {refused}"
                );
                assert_eq!(error_kind(&refused), "invalid-value");
            }
            assert!(
                orders.take().is_empty(),
                "{autopilot} {vehicle_type}: routed"
            );
            assert!(
                orders.keyboard_frames().is_empty(),
                "{autopilot} {vehicle_type}: sent"
            );
            let started = call(
                &mut engine,
                &mut ui,
                "controller/keyboardStart",
                json!({ "vehicleId": "v3", "throttle": 0.0 }),
            );
            assert_eq!(started["result"]["target"], "v3", "{started}");
            assert_eq!(orders.keyboard_frames(), [[0.0; 4]]);
            let id = started["result"]["id"].as_str().unwrap();
            assert!(
                orders
                    .take()
                    .iter()
                    .any(|order| order.starts_with(&format!("route {id} v3 ")))
            );
        }
    }
    for (autopilot, vehicle_type) in [("Ardupilotmega", "Quadrotor"), ("Px4", "FixedWing")] {
        for (armed, throttle) in [(false, -1.0), (true, 0.0)] {
            let (mut engine, mut ui, orders) = setup();
            observe_v3(&mut engine, autopilot, vehicle_type, armed);
            let started = call(
                &mut engine,
                &mut ui,
                "controller/keyboardStart",
                json!({ "vehicleId": "v3", "throttle": throttle }),
            );
            assert_eq!(started["result"]["target"], "v3", "{started}");
            assert_eq!(orders.keyboard_frames(), [[0.0, 0.0, 0.0, throttle]]);
        }
    }
}

/// The keyboard's start frame, through the route the daemon gives the data
/// plane, is the throttle stick `MANUAL_CONTROL` and RC override are built
/// from. An armed rover's or boat's start is the centre, which `ArduRover`
/// reads as stop; the 0% the page sent them before, the bottom and so full
/// reverse, is refused. An aircraft's 0% is the bottom, its idle.
#[test]
fn a_keyboard_start_sends_a_centred_throttle_at_its_centre() {
    for (vehicle_type, start, stick) in [
        ("GroundRover", 0.0, 500),
        ("SurfaceBoat", 0.0, 500),
        ("Quadrotor", -1.0, 0),
    ] {
        let (mut engine, mut ui, orders) = setup();
        observe_v3(&mut engine, "Ardupilotmega", vehicle_type, true);
        if start == 0.0 {
            let refused = call(
                &mut engine,
                &mut ui,
                "controller/keyboardStart",
                json!({ "vehicleId": "v3", "throttle": -1.0 }),
            );
            assert_eq!(error_kind(&refused), "invalid-value", "{vehicle_type}");
        }
        let started = call(
            &mut engine,
            &mut ui,
            "controller/keyboardStart",
            json!({ "vehicleId": "v3", "throttle": start }),
        );
        let id = started["result"]["id"].as_str().unwrap();
        let frames = orders.keyboard_frames();
        let [axes] = frames.as_slice() else {
            panic!("{vehicle_type}: one start frame, got {frames:?}");
        };
        let route = orders
            .1
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|route| route.vehicle_id == "v3")
            .cloned()
            .unwrap();
        let mut controls = Controls::default();
        controls.input(InputEvent::Keyboard {
            id: id.into(),
            axes: *axes,
            at_us: 1_000,
        });
        controls.route(id, Some(route));
        let outputs = controls.pilot_outputs();
        let [(vehicle, sticks, ..)] = outputs.as_slice() else {
            panic!("{vehicle_type}: one output, got {outputs:?}");
        };
        assert_eq!(vehicle, "v3");
        assert_eq!(sticks[2], stick, "{vehicle_type}");
    }
}

/// The mapping turns the stick into what is judged and sent: a deadzone
/// first, then the band either side of the middle (10% unless the
/// vehicle's preflight sets another), an inverted throttle turned over, and
/// Centre to top putting a resting stick at the bottom: full reverse where
/// the throttle centres, idle on an aircraft. Disarmed, as preflight checks
/// are judged; an armed vehicle keeps the list it armed with.
#[test]
fn the_controller_check_judges_the_mapped_throttle_against_its_band() {
    let mapping = |range: &'static str, inverted: bool| {
        move |engine: &mut Engine, ui: &mut Client| {
            let updated = call(
                engine,
                ui,
                "mapping/update",
                json!({
                    "id": "mode-2", "expectedVersion": 1, "name": "Mode 2",
                    "roll": { "axis": "right-stick-x", "inverted": false },
                    "pitch": { "axis": "right-stick-y", "inverted": false },
                    "yaw": { "axis": "left-stick-x", "inverted": false },
                    "throttle": { "axis": "left-stick-y", "inverted": inverted },
                    "throttleRange": range, "deadzone": 0.05, "buttons": []
                }),
            );
            assert!(updated.get("result").is_some(), "{updated}");
        }
    };
    let neutral = "pass: Xbox at neutral.";
    let off = "fail: Centre forward / reverse on Xbox.";
    for (autopilot, vehicle_type) in [("Ardupilotmega", "GroundRover"), ("Px4", "SurfaceBoat")] {
        for (range, inverted, throttle, expected) in [
            ("full-travel", false, 0.04, neutral),
            ("full-travel", false, 0.14, neutral),
            ("full-travel", false, -0.14, neutral),
            ("full-travel", false, 0.15, off),
            ("full-travel", false, -0.15, off),
            ("full-travel", true, -1.0, off),
            ("full-travel", true, 0.0, neutral),
            ("centre-to-top", false, 0.0, off),
            ("centre-to-top", false, 0.525, neutral),
            ("centre-to-top", false, 1.0, off),
        ] {
            assert_eq!(
                controller_check(
                    autopilot,
                    vehicle_type,
                    false,
                    throttle,
                    mapping(range, inverted)
                ),
                expected,
                "{autopilot} {vehicle_type}, {range}, inverted {inverted}, at {throttle}"
            );
        }
    }
    let raised = "fail: Lower the throttle on Xbox.";
    for (range, inverted, throttle, expected) in [
        ("full-travel", false, -1.0, neutral),
        ("full-travel", false, 0.14, neutral),
        ("full-travel", false, 0.15, raised),
        ("full-travel", true, 1.0, neutral),
        ("full-travel", true, -1.0, raised),
        ("centre-to-top", false, 0.0, neutral),
        ("centre-to-top", false, 0.55, neutral),
        ("centre-to-top", false, 0.6, raised),
    ] {
        assert_eq!(
            controller_check(
                "Ardupilotmega",
                "Quadrotor",
                false,
                throttle,
                mapping(range, inverted)
            ),
            expected,
            "copter, {range}, inverted {inverted}, at {throttle}"
        );
    }
}

/// A parameter download of these values for a vehicle, with what the data
/// plane's session then says of its RC calibration.
fn parameters(engine: &mut Engine, vehicle_id: &str, values: &[(&str, f64)]) {
    use std::collections::BTreeMap;
    use uxduck_core::domain::parameter::ParamKind;
    use uxduck_engine::transfers::Reported;
    let reported = values
        .iter()
        .map(|(name, value)| Reported {
            name: (*name).into(),
            value: *value,
            kind: ParamKind::Real32,
        })
        .collect();
    engine.parameters_downloaded(vehicle_id, Ok(reported), &BTreeMap::new());
    let problem = uxduck_core::domain::controller::RoverThrottle::from_params(|name| {
        values.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    })
    .err();
    engine.rc_calibration(vehicle_id, true, problem);
}

/// Sets a vehicle's pilot input method.
fn pilot_input(engine: &mut Engine, ui: &mut Client, vehicle_id: &str, method: &str) {
    let saved = call(
        engine,
        ui,
        "settings/update",
        json!({ "expectedVersion": 0, "pilotInput": { "vehicleId": vehicle_id, "method": method } }),
    );
    assert!(saved.get("result").is_some(), "{saved}");
}

fn rover_calibration(map: f64, [min, trim, max]: [f64; 3]) -> Vec<(&'static str, f64)> {
    vec![
        ("RCMAP_THROTTLE", map),
        ("RC3_MIN", min),
        ("RC3_TRIM", trim),
        ("RC3_MAX", max),
    ]
}

/// `ArduRover` on `MANUAL_CONTROL` stops at its throttle channel's trim,
/// so without a usable calibration the data plane sends it nothing and the
/// controller check says why instead of passing; with one, mid-range,
/// off-centre or forward-only, and after it changes, the middle is stop and
/// the check passes. RC overrides and PX4 are judged as before.
#[test]
fn a_rover_on_manual_control_needs_its_throttle_calibration_to_pass() {
    let check =
        |autopilot: &str, method: &'static str, downloads: Vec<Vec<(&'static str, f64)>>| {
            controller_check(autopilot, "GroundRover", false, 0.0, move |engine, ui| {
                pilot_input(engine, ui, "v3", method);
                for values in &downloads {
                    parameters(engine, "v3", values);
                }
            })
        };
    let neutral = "pass: Xbox at neutral.";
    for trims in [
        [1100.0, 1500.0, 1900.0],
        [1100.0, 1460.0, 1900.0],
        [1100.0, 1100.0, 1900.0],
    ] {
        assert_eq!(
            check(
                "Ardupilotmega",
                "manual-control",
                vec![rover_calibration(3.0, trims)]
            ),
            neutral,
            "{trims:?}"
        );
    }
    for (downloads, problem) in [
        (vec![vec![]], "RCMAP_THROTTLE is missing"),
        (
            vec![rover_calibration(3.0, [1100.0, 1950.0, 1900.0])],
            "RC3_TRIM 1950 is outside RC3_MIN to RC3_MAX",
        ),
        (
            vec![rover_calibration(2.0, [1100.0, 1500.0, 1900.0])],
            "RC2_MIN is missing",
        ),
        (
            vec![
                rover_calibration(3.0, [1100.0, 1500.0, 1900.0]),
                rover_calibration(3.0, [1900.0, 1500.0, 1100.0]),
            ],
            "RC3_MIN 1900 to RC3_MAX 1100 is not a PWM range",
        ),
    ] {
        let detail = check("Ardupilotmega", "manual-control", downloads);
        assert!(
            detail.starts_with(&format!("fail: {problem}, so UXDuck sends "))
                && detail
                    .ends_with(" no MANUAL_CONTROL. Read its parameters, or calibrate its radio."),
            "{detail}"
        );
    }
    assert_eq!(
        check(
            "Ardupilotmega",
            "manual-control",
            vec![
                rover_calibration(3.0, [1100.0, 1950.0, 1900.0]),
                rover_calibration(3.0, [1100.0, 1500.0, 1900.0]),
            ]
        ),
        neutral,
        "calibrated again"
    );
    assert_eq!(check("Ardupilotmega", "automatic", vec![]), neutral);
    assert_eq!(check("Px4", "manual-control", vec![]), neutral);
}

/// A rover's kept parameters outlive a daemon restart and a new
/// connection, but the data plane's new session reads its own before it
/// sends `MANUAL_CONTROL`, and says whether it has. So the check fails,
/// with a usable calibration kept, until that session says it has read it:
/// before any boot is reported, after one is, and after a failed read. A
/// read that lands before the boot is reported counts, and what the session
/// then says of its calibration, such as a refresh breaking it part way, is
/// what is judged.
#[test]
fn a_rover_s_kept_calibration_passes_only_once_its_session_has_read_it() {
    use std::collections::BTreeMap;
    use uxduck_engine::transfers::Failure;
    type Then = fn(&mut Engine, &[(&'static str, f64)]);
    const BOOT: u64 = 90_000_000;
    let check = |then: Then| {
        controller_check(
            "Ardupilotmega",
            "GroundRover",
            false,
            0.0,
            move |engine, ui| {
                let calibration = rover_calibration(3.0, [1100.0, 1460.0, 1900.0]);
                pilot_input(engine, ui, "v3", "manual-control");
                engine.vehicle_booted("v3", BOOT, false);
                parameters(engine, "v3", &calibration);
                engine.rc_calibration("v3", false, None);
                assert_eq!(engine.state().parameters["v3/RC3_TRIM"].value, 1460.0);
                then(engine, &calibration);
            },
        )
    };
    let unread = |detail: &str| {
        detail.starts_with("fail: ")
            && detail
                .contains("'s parameters haven't been read on this connection, so UXDuck sends ")
            && detail.ends_with(" no MANUAL_CONTROL. Read its parameters, or calibrate its radio.")
    };
    let detail = check(|_, _| {});
    assert!(unread(&detail), "a new session, no boot reported: {detail}");
    let detail = check(|engine, _| engine.vehicle_booted("v3", BOOT, false));
    assert!(unread(&detail), "the same boot reported: {detail}");
    let detail = check(|engine, _| {
        engine.parameters_downloaded("v3", Err(Failure::Timeout), &BTreeMap::new());
    });
    assert!(unread(&detail), "read failed: {detail}");
    let detail = check(|engine, calibration| {
        engine.parameters_downloaded("v3", Err(Failure::Timeout), &BTreeMap::new());
        parameters(engine, "v3", calibration);
    });
    assert_eq!(detail, "pass: Xbox at neutral.", "read again");
    let detail = check(|engine, calibration| {
        parameters(engine, "v3", calibration);
        engine.vehicle_booted("v3", BOOT, false);
    });
    assert_eq!(detail, "pass: Xbox at neutral.", "read before the boot");
    let detail = check(|engine, calibration| {
        parameters(engine, "v3", calibration);
        engine.rc_calibration(
            "v3",
            true,
            Some("RC3_TRIM 2100 is outside RC3_MIN to RC3_MAX".into()),
        );
    });
    assert!(
        detail.starts_with("fail: RC3_TRIM 2100 is outside RC3_MIN to RC3_MAX, so UXDuck sends "),
        "what the session holds, not what was kept: {detail}"
    );
}

/// An armed rover's keyboard starts at neutral, and over `MANUAL_CONTROL`
/// that goes out at its throttle channel's trim, stop, whether the trim is
/// off-centre or at the bottom on a forward-only setup.
#[test]
#[allow(clippy::cast_possible_truncation, reason = "as the C++ truncates")]
fn a_rover_s_keyboard_start_goes_out_at_its_throttle_trim() {
    use uxduck_core::domain::controller::RoverThrottle;
    use uxduck_core::domain::settings::PilotInputMethod;
    for [min, trim, max] in [[1100, 1460, 1900], [1100, 1100, 1900]] {
        let (mut engine, mut ui, orders) = setup();
        observe_v3(&mut engine, "Ardupilotmega", "GroundRover", true);
        pilot_input(&mut engine, &mut ui, "v3", "manual-control");
        let values = rover_calibration(3.0, [min, trim, max].map(f64::from));
        parameters(&mut engine, "v3", &values);
        let started = call(
            &mut engine,
            &mut ui,
            "controller/keyboardStart",
            json!({ "vehicleId": "v3", "throttle": 0.0 }),
        );
        let id = started["result"]["id"].as_str().unwrap();
        let frames = orders.keyboard_frames();
        let [axes] = frames.as_slice() else {
            panic!("one start frame, got {frames:?}");
        };
        let route = orders
            .1
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|route| route.vehicle_id == "v3")
            .cloned()
            .unwrap();
        let mut controls = Controls::default();
        controls.input(InputEvent::Keyboard {
            id: id.into(),
            axes: *axes,
            at_us: 1_000,
        });
        controls.route(id, Some(route));
        let outputs = controls.pilot_outputs();
        let [(_, sticks, _, method, _)] = outputs.as_slice() else {
            panic!("one output, got {outputs:?}");
        };
        assert_eq!(*method, PilotInputMethod::ManualControl);
        let z = RoverThrottle::from_params(|name| {
            values.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
        })
        .unwrap()
        .manual_z(sticks[2]);
        // ArduRover's PWM for z (GCS_MAVLINK::manual_override, Rover 4.5 to 4.7).
        let pwm = (f64::from(min) + f64::from((max - min) * (i32::from(z) + 1000)) / 2000.0) as i32;
        assert_eq!(pwm, trim, "{min} {trim} {max}");
    }
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
fn plugging_in_a_controller_notifies_with_the_way_to_its_tab() {
    let (mut engine, _ui, _orders) = setup();
    let connected = |engine: &Engine| {
        engine
            .state()
            .notifications
            .values()
            .filter(|n| n.title == "Xbox connected")
            .cloned()
            .collect::<Vec<_>>()
    };
    let notices = connected(&engine);
    assert_eq!(notices.len(), 1, "{:?}", titles(&engine));
    assert_eq!(notices[0].actions[0].label, "Open");
    assert_eq!(
        notices[0].actions[0].action,
        Action::OpenControllers {
            controller_id: "g1".into()
        }
    );

    // Heard again while still plugged in: not news.
    engine.controller_connected("g1", "Xbox", true);
    assert_eq!(connected(&engine).len(), 1);
    // Unplugging warns, and reconnecting updates the same device notice.
    engine.controller_disconnected("g1");
    assert!(titles(&engine).contains(&"Xbox disconnected".to_owned()));
    engine.controller_connected("g1", "Xbox", true);
    assert!(titles(&engine).contains(&"Xbox reconnected".to_owned()));
    assert_eq!(engine.state().notifications.values().filter(|n| n.actions.iter().any(|a| matches!(&a.action, Action::OpenControllers { controller_id } if controller_id == "g1"))).count(), 1);
}

#[test]
fn an_unclaimed_browser_controller_warns_on_input_loss_and_reuses_one_action_on_reconnect() {
    let (mut engine, _ui, _orders) = setup();
    engine.controller_connected("web-pad", "Browser pad", true);
    for _ in 0..3 {
        engine.controller_disconnected("web-pad");
        let notices: Vec<_> = engine
            .state()
            .notifications
            .values()
            .filter(|n| n.condition.as_deref() == Some("controller-presence:web-pad"))
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Browser pad input interrupted");
        assert_eq!(
            notices[0].severity,
            uxduck_core::domain::notification::Severity::Warning
        );
        engine.controller_connected("web-pad", "Browser pad", true);
    }
    let notices: Vec<_> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.condition.as_deref() == Some("controller-presence:web-pad"))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].title, "Browser pad reconnected");
    assert_eq!(notices[0].actions.len(), 1);
}

#[test]
fn a_controller_is_claimed_and_follows_selection_wherever_its_sticks_are() {
    let (mut engine, mut ui, orders) = setup();
    let follow = json!({ "id": "g1", "mode": "follows-selection" });
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    // Sticks off centre: used all the same.
    engine.controller_neutral("g1", false);
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

    // A new selection with a displaced stick switches at once.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v2")
    );
    assert!(engine.state().controllers["g1"].pending_target.is_none());
    assert_eq!(orders.take(), ["route g1 v2 1"]);

    // Selecting switches immediately, without setTarget.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v1")
    );
    assert_eq!(orders.take(), ["route g1 v1 1"]);
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    assert!(orders.take().is_empty(), "unchanged input never reroutes");
}

#[test]
fn a_controller_follows_a_selection_made_after_it_is_claimed() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    // Pinned needs a vehicle; following holds the controller and drives nothing.
    let pinned = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned" }),
    );
    assert_eq!(error_kind(&pinned), "invalid-value");
    let follow = json!({ "id": "g1", "mode": "follows-selection" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", follow.clone());
    assert!(claimed["result"].get("target").is_none(), "{claimed}");
    assert!(engine.state().controllers["g1"].claim.is_some());
    assert_eq!(orders.take(), ["route g1 none"]);

    // The first selection switches it at once, sticks as they are.
    engine.controller_neutral("g1", false);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v1")
    );
    assert_eq!(orders.take(), ["route g1 v1 1"]);
}

#[test]
fn following_never_takes_another_controllers_vehicle_and_ignores_other_clients() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("g2", "Stick", true);
    engine.controller_neutral("g1", true);
    engine.controller_neutral("g2", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({
            "id": "g1", "mode": "follows-selection", "vehicleId": "v1"
        }),
    );
    let mut other = connect(&mut engine, "operator-cli");
    call(
        &mut engine,
        &mut other,
        "controller/claim",
        json!({
            "id": "g2", "mode": "pinned", "vehicleId": "v2"
        }),
    );
    orders.take();
    call(
        &mut engine,
        &mut other,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    assert!(engine.state().controllers["g1"].pending_target.is_none());
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    engine.controller_neutral("g1", true);
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v1")
    );
    assert_eq!(
        engine.state().controllers["g2"].target.as_deref(),
        Some("v2")
    );
    assert!(orders.take().is_empty(), "no conflicting route was sent");

    // Going back to the current target cancels a pending switch.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    call(
        &mut engine,
        &mut other,
        "controller/release",
        json!({ "id": "g2" }),
    );
    orders.take();
    engine.controller_neutral("g1", true);
    assert!(engine.state().controllers["g1"].pending_target.is_none());
    assert!(orders.take().is_empty());
}

#[test]
fn a_removed_target_gives_way_to_the_next_selection_at_once() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({
            "id": "g1", "mode": "follows-selection", "vehicleId": "v1"
        }),
    );
    let link_id = engine.state().vehicles["v1"].link_id.clone();
    call(
        &mut engine,
        &mut ui,
        "link/remove",
        json!({ "id": link_id }),
    );
    let deleted = call(
        &mut engine,
        &mut ui,
        "vehicle/delete",
        json!({ "id": "v1" }),
    );
    assert!(deleted.get("result").is_some(), "{deleted}");
    orders.take();
    engine.controller_neutral("g1", false);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v2")
    );
    assert_eq!(orders.take(), ["route g1 v2 1"]);
}

#[test]
fn selecting_the_same_vehicle_under_its_lasting_id_is_not_a_switch() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({
            "id": "g1", "mode": "follows-selection", "vehicleId": "v1"
        }),
    );
    let vehicle = engine.state().vehicles["v1"].clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "lasting-id".into(),
        aliases: vec!["v1".into()],
        link_id: vehicle.link_id,
        peer: vehicle.peer,
        system_id: vehicle.system_id,
        component_ids: vehicle.component_ids,
        autopilot: vehicle.autopilot,
        vehicle_type: vehicle.vehicle_type,
        uid: Some("uid".into()),
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 6,
        armed: false,
        mode: None,
    });
    orders.take();
    engine.controller_neutral("g1", false);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "lasting-id" }),
    );
    assert!(engine.state().controllers["g1"].pending_target.is_none());
    assert!(orders.take().is_empty(), "an alias is the same aircraft");
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

    // The claiming window goes: it keeps driving for the grace, in case
    // the window is reloading, then is released and people are told.
    engine.disconnect(ui.id);
    assert!(orders.take().is_empty(), "still driving Alpha");
    assert!(engine.state().controllers["g1"].claim.is_some());
    let busy = call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(
        error_kind(&busy),
        "busy",
        "held for the window to come back"
    );
    engine.expire_detached_claims(1_000 + CLAIM_GRACE_US - 1);
    assert!(orders.take().is_empty());
    engine.expire_detached_claims(1_000 + CLAIM_GRACE_US);
    assert_eq!(orders.take(), ["route g1 none"]);
    assert!(engine.state().controllers["g1"].claim.is_none());
    assert!(
        titles(&engine)
            .iter()
            .any(|t| t.starts_with("Xbox stopped driving ") && t.ends_with(": its window closed"))
    );

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
#[allow(
    clippy::too_many_lines,
    reason = "one handover across open, closed, refused and released windows"
)]
fn the_same_operator_takes_a_controller_over_from_another_window_still_flying() {
    let (mut engine, mut ui, orders) = setup();
    let ana = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({ "name": "Ana" }),
    )["result"]["id"]
        .clone();
    let as_ana = |engine: &mut Engine, client: &mut Client| {
        call(
            engine,
            client,
            "client/setOperator",
            json!({ "operatorId": ana }),
        );
    };
    as_ana(&mut engine, &mut ui);
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    orders.take();

    // A window left open somewhere: another of Ana's, in a browser, takes it
    // with its sticks pushed, and it keeps flying Alpha on its route.
    let browser = Trust::PairedBrowser {
        origin: "http://127.0.0.1:45000".into(),
    };
    let mut other = connect_as(&mut engine, "operator-ui", browser.clone());
    as_ana(&mut engine, &mut other);
    engine.controller_neutral("g1", false);
    let taken = call(
        &mut engine,
        &mut other,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(taken["result"]["target"], "v1", "{taken}");
    assert_eq!(taken["result"]["mode"], "pinned");
    assert!(orders.take().is_empty(), "the route is unchanged");
    // The first window no longer answers for it; the second does.
    let moved = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(error_kind(&moved), "capability-denied", "{moved}");
    engine.controller_pressed("g1", Some(GamepadButton::East), None);
    engine.controller_pressed("g1", Some(GamepadButton::West), None);
    assert_eq!(orders.take(), [format!("command v1 {:?}", Command::Rtl)]);

    // Another operator is refused, and so is a window with none.
    let mut ben = connect(&mut engine, "operator-ui");
    let ben_id = call(
        &mut engine,
        &mut ben,
        "operator/create",
        json!({ "name": "Ben" }),
    )["result"]["id"]
        .clone();
    call(
        &mut engine,
        &mut ben,
        "client/setOperator",
        json!({ "operatorId": ben_id }),
    );
    let refused = call(
        &mut engine,
        &mut ben,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    let mut nobody = connect(&mut engine, "operator-ui");
    let refused = call(
        &mut engine,
        &mut nobody,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    // The CLI is not a window: it neither takes one over nor gives its own up.
    let mut cli = connect(&mut engine, "operator-cli");
    as_ana(&mut engine, &mut cli);
    let refused = call(
        &mut engine,
        &mut cli,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    engine.controller_connected("g2", "Stick", true);
    engine.controller_neutral("g2", true);
    call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g2", "mode": "pinned", "vehicleId": "v2" }),
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "controller/takeOver",
        json!({ "id": "g2" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    orders.take();

    // Closed moments ago: the open window takes it at once, in the grace,
    // and the grace no longer releases it.
    engine.disconnect(other.id);
    let taken = call(
        &mut engine,
        &mut ui,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(taken["result"]["target"], "v1", "{taken}");
    engine.expire_detached_claims(1_000 + CLAIM_GRACE_US);
    assert!(orders.take().is_empty(), "still driving Alpha");
    assert!(engine.state().controllers["g1"].claim.is_some());

    // Nothing to take over once released; keyboards stay with their page.
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "g1" }),
    );
    let free = call(
        &mut engine,
        &mut ui,
        "controller/takeOver",
        json!({ "id": "g1" }),
    );
    assert_eq!(error_kind(&free), "invalid-value", "{free}");
    let keys = call(
        &mut engine,
        &mut ui,
        "controller/takeOver",
        json!({ "id": "keyboard:1" }),
    );
    assert_eq!(error_kind(&keys), "capability-denied", "{keys}");
}

#[test]
fn a_mapping_keeps_its_stick_layout_and_may_change_it_while_flying() {
    let (mut engine, mut ui, _orders) = setup();
    let edit = |version: u64, stick_mode: Option<&str>| {
        let mut params = json!({
            "id": "mode-2", "expectedVersion": version, "name": "Mode 2",
            "roll": { "axis": "right-stick-x", "inverted": false },
            "pitch": { "axis": "right-stick-y", "inverted": false },
            "yaw": { "axis": "left-stick-x", "inverted": false },
            "throttle": { "axis": "left-stick-y", "inverted": false },
            "deadzone": 0.05,
            "buttons": []
        });
        if let Some(mode) = stick_mode {
            params["stickMode"] = json!(mode);
        }
        params
    };
    // An older client leaves the layout out: the built-in's stays.
    let kept = call(&mut engine, &mut ui, "mapping/update", edit(1, None));
    assert_eq!(kept["result"]["stickMode"], "mode-2", "{kept}");
    // It only says how the sticks are drawn, so it may change while held.
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    let changed = call(
        &mut engine,
        &mut ui,
        "mapping/update",
        edit(2, Some("mode-4")),
    );
    assert_eq!(changed["result"]["stickMode"], "mode-4", "{changed}");
    let copied = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "Pad", "from": "mode-2" }),
    );
    assert_eq!(copied["result"]["stickMode"], "mode-4");
    let radio = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({ "name": "Tango", "from": "radio" }),
    );
    assert!(radio["result"].get("stickMode").is_none(), "{radio}");
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
            "ExpressLRS 4.x Bluetooth",
            "ExpressLRS 3.x Bluetooth",
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
fn a_claim_takes_the_sticks_wherever_they_are() {
    // Mode 1's pitch is the left stick's Y: held full forward.
    let (mut engine, mut ui, orders) = setup_with(NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": true, "buttons": [],
            "axes": [{ "name": "Left Y", "axis": "left-stick-y", "value": 1.0 }],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    });
    engine.controller_neutral("g1", false);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-1" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", claim);
    assert_eq!(claimed["result"]["target"], "v1", "{claimed}");
    assert_eq!(orders.take(), ["route g1 v1 1"]);
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

/// `ExpressLRS` 4 calls itself "ELRS Joystick" and takes channels 3 and 4
/// on Ry and Rz (FB-88). A radio set up by hand keeps its mapping.
#[test]
fn an_elrs_4_joystick_starts_on_its_mapping_unless_set_up_by_hand() {
    let (mut engine, mut ui, _) = setup();
    engine.controller_connected("e4", "ELRS Joystick", false);
    assert_eq!(
        engine.state().controllers["e4"].mapping_id,
        "expresslrs-4-bluetooth"
    );

    // Known from before, on the untouched radio mapping: moved to it.
    engine.controller_connected("e5", "Radio", false);
    engine.controller_disconnected("e5");
    engine.controller_connected("e5", "ELRS Joystick", false);
    assert_eq!(
        engine.state().controllers["e5"].mapping_id,
        "expresslrs-4-bluetooth"
    );

    // Set up by hand on the radio mapping itself: kept.
    engine.controller_connected("e6", "Radio", false);
    engine.controller_disconnected("e6");
    let edited = call(
        &mut engine,
        &mut ui,
        "mapping/update",
        json!({
            "id": "radio", "expectedVersion": 1, "name": "Radio (AETR)",
            "roll": { "axis": "left-stick-x", "code": 196_608, "inverted": false },
            "pitch": { "axis": "left-stick-y", "code": 196_609, "inverted": false },
            "throttle": { "axis": "right-stick-x", "code": 196_612, "inverted": false },
            "yaw": { "axis": "right-stick-y", "code": 196_613, "inverted": false },
            "throttleRange": "full-travel", "deadzone": 0.05, "buttons": []
        }),
    );
    assert!(edited.get("result").is_some(), "{edited}");
    engine.controller_connected("e6", "ELRS Joystick", false);
    assert_eq!(engine.state().controllers["e6"].mapping_id, "radio");
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

/// FB-60: a mapping may leave roll, pitch or yaw unused, never throttle,
/// and says so at `initialize`. A radio that sends only steering and
/// throttle can then be claimed; what is unused cannot change while it is.
#[test]
fn controls_not_used_need_no_input_to_claim_and_stay_put_while_claimed() {
    let rover_radio = NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": false, "buttons": [],
            "axes": [
                { "code": 65584, "name": "X", "number": 1, "value": 0.0 },
                { "code": 65586, "name": "Z", "number": 3, "value": -1.0 },
            ],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    };
    let (mut engine, mut ui, _) = setup_with(rover_radio);
    let (outbox, inbox) = unbounded();
    let mut other = Client {
        id: engine.connect(Transport::Socket, Trust::LocalUser, outbox),
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let hello = call(
        &mut engine,
        &mut other,
        "initialize",
        json!({
            "clientInfo": { "name": "ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert_eq!(
        hello["result"]["capabilities"]["unusedControls"], true,
        "{hello}"
    );
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "radio" });
    let refused = call(&mut engine, &mut ui, "controller/claim", claim.clone());
    assert_eq!(
        refused["error"]["message"],
        "Radio (AETR) doesn't read Xbox's pitch and yaw. Make a mapping for Xbox on the \
         Mapping page, then move each stick to set it.",
        "{refused}"
    );
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
    let mut throttle = radio.clone();
    throttle["unused"] = json!(["throttle"]);
    let refused = call(&mut engine, &mut ui, "mapping/update", throttle);
    assert!(
        refused["error"].is_object(),
        "throttle is always used: {refused}"
    );
    radio["unused"] = json!(["yaw", "pitch", "yaw"]);
    let saved = call(&mut engine, &mut ui, "mapping/update", radio.clone());
    assert_eq!(
        saved["result"]["unused"],
        json!(["pitch", "yaw"]),
        "{saved}"
    );
    assert_eq!(
        saved["result"]["yaw"]["code"], 65587,
        "bindings kept: {saved}"
    );
    let used = call(&mut engine, &mut ui, "controller/claim", claim);
    assert_eq!(used["result"]["target"], "v1", "{used}");
    radio["expectedVersion"] = saved["result"]["version"].clone();
    radio["unused"] = json!(["pitch"]);
    let refused = call(&mut engine, &mut ui, "mapping/update", radio);
    assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
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
fn a_refused_claim_says_which_channels_the_mapping_cannot_read() {
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
    // The built-in radio mapping reads it by its codes: used with roll off
    // centre.
    let used = call(&mut engine, &mut ui, "controller/claim", claim("radio"));
    assert_eq!(used["result"]["target"], "v1", "{used}");
}

#[test]
fn a_flying_vehicle_is_taken_over_with_the_throttle_where_it_is() {
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

    let link_id = engine.state().vehicles["v1"].link_id.clone();
    engine.vehicle_observed(Observation {
        connections: None,
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
    // Armed at 30 m with the throttle at idle: taken over all the same.
    let in_air = call(&mut engine, &mut ui, "controller/claim", claim);
    assert!(in_air.get("result").is_some(), "{in_air}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "g1" }),
    );

    // Following the selection, it takes its first vehicle at once too.
    let follow = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({
            "id": "g1", "mode": "follows-selection", "mappingId": "radio"
        }),
    );
    assert!(follow.get("result").is_some(), "{follow}");
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    engine.controller_neutral("g1", true);
    assert_eq!(
        engine.state().controllers["g1"].target.as_deref(),
        Some("v1")
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
fn restarting_with_a_reused_radio_mapping_refuses_activation_until_repaired() {
    use uxduck_core::domain::controller::{GamepadAxis, InputMapping, PositionBinding};
    let mut mapping = InputMapping::mode_1();
    mapping.positions = vec![PositionBinding {
        axis: GamepadAxis::LeftStickX,
        code: Some(65589),
        at: -1.0,
        command: Command::Disarm,
        hold_ms: None,
    }];
    let mut state = uxduck_engine::state::State::default();
    state.input_mappings.insert(mapping.id.clone(), mapping);
    let system = NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": true, "buttons": [],
            "axes": [{ "name": "Right Y", "axis": "right-stick-y", "code": 65589, "value": 0.0 }]
        }] }))
        .unwrap(),
        ..NoSystem::default()
    };
    let (mut engine, mut ui, orders) = setup_with_state(system, state);
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-1" });
    let refused = call(&mut engine, &mut ui, "controller/claim", claim.clone());
    assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicting")
    );
    assert!(engine.state().controllers["g1"].claim.is_none());
    assert!(orders.take().is_empty());
    let mut repair = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &engine.state().input_mappings["mode-1"],
    ))
    .unwrap();
    repair["expectedVersion"] = json!(1);
    repair["positions"] = json!([]);
    assert!(
        call(&mut engine, &mut ui, "mapping/update", repair)
            .get("result")
            .is_some()
    );
    let used = call(&mut engine, &mut ui, "controller/claim", claim);
    assert!(used.get("result").is_some(), "{used}");
}

#[test]
fn mapping_updates_reject_driving_positions_and_overlaps_without_changing_saved_state() {
    let (mut engine, mut ui, _) = setup();
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    for positions in [
        json!([{ "axis": "left-stick-y", "at": -1, "command": { "type": "disarm" } }]),
        json!([
            { "axis": "left-stick-x", "code": 999, "at": 0, "command": { "type": "arm" } },
            { "axis": "right-stick-y", "code": 999, "at": 0.1, "command": { "type": "disarm" } }
        ]),
    ] {
        edit["positions"] = positions;
        let refused = call(&mut engine, &mut ui, "mapping/update", edit.clone());
        assert_eq!(error_kind(&refused), "invalid-value", "{refused}");
        assert!(!engine.state().input_mappings.contains_key("mode-2"));
    }
}

#[test]
fn a_settled_or_queued_position_is_rechecked_against_current_input_before_sending() {
    let (mut engine, mut ui, orders) = setup_with(NoSystem {
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": true, "buttons": [],
            "axes": [{ "name": "Switch", "code": 999, "value": 0.0 }]
        }] }))
        .unwrap(),
        ..NoSystem::default()
    });
    let mut edit = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    edit["expectedVersion"] = json!(1);
    edit["positions"] =
        json!([{ "axis": "left-stick-x", "code": 999, "at": -1, "command": { "type": "disarm" } }]);
    assert!(
        call(&mut engine, &mut ui, "mapping/update", edit)
            .get("result")
            .is_some()
    );
    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-2" });
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim)
            .get("result")
            .is_some()
    );
    orders.take();
    let old = engine.state().input_mappings["mode-2"].positions[0].clone();
    engine.controller_positioned("g1", &old);
    assert!(
        orders.take().is_empty(),
        "the switch has left this position"
    );
}

/// FB-67: a position settled while the controller's last position command
/// is unanswered goes once that is answered, never alongside it, and only
/// the last of several; a new claim drops it.
#[test]
fn a_position_settled_while_the_last_is_unanswered_goes_after_it_and_only_the_last() {
    use uxduck_core::domain::command::ReceiptStatus;
    use uxduck_engine::commands::Outcome;
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    let mut mode_2 = serde_json::to_value(uxduck_contracts::dto::controller::InputMapping::from(
        &uxduck_core::domain::controller::InputMapping::mode_2(),
    ))
    .unwrap();
    let at = |at: f64, mode: &str| {
        json!({ "axis": "left-stick-x", "code": 65589, "at": at,
                "command": { "type": "set-mode", "mode": mode } })
    };
    mode_2["positions"] = json!([
        at(-1.0, "STABILIZE"),
        at(0.0, "ALT_HOLD"),
        at(1.0, "LOITER")
    ]);
    mode_2["expectedVersion"] = json!(1);
    let saved = call(&mut engine, &mut ui, "mapping/update", mode_2);
    assert!(saved.get("result").is_some(), "{saved}");
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-2" });
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim.clone())
            .get("result")
            .is_some()
    );
    orders.take();
    let positions = engine.state().input_mappings["mode-2"].positions.clone();
    let unanswered = |engine: &Engine| -> String {
        let waiting: Vec<_> = engine
            .state()
            .receipts
            .values()
            .filter(|r| r.status == ReceiptStatus::Sent)
            .collect();
        assert_eq!(waiting.len(), 1, "{waiting:?}");
        waiting[0].id.clone()
    };

    engine.controller_positioned("g1", &positions[0]);
    assert_eq!(
        orders.take(),
        ["command v1 SetMode { mode: \"STABILIZE\" }"]
    );
    let stabilize = unanswered(&engine);
    engine.controller_positioned("g1", &positions[1]);
    engine.controller_positioned("g1", &positions[2]);
    assert!(
        orders.take().is_empty(),
        "nothing goes while STABILIZE is unanswered"
    );
    engine.command_result(&stabilize, Outcome::Accepted, 1);
    assert_eq!(
        orders.take(),
        ["command v1 SetMode { mode: \"LOITER\" }"],
        "then only the last"
    );

    // A refusal answers as well; the next goes after it.
    let loiter = unanswered(&engine);
    engine.controller_positioned("g1", &positions[1]);
    assert!(orders.take().is_empty());
    engine.command_result(&loiter, Outcome::Rejected("denied".into()), 1);
    assert_eq!(orders.take(), ["command v1 SetMode { mode: \"ALT_HOLD\" }"]);

    // Waiting through a release and a new claim, it is dropped.
    let alt_hold = unanswered(&engine);
    engine.controller_positioned("g1", &positions[0]);
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({ "id": "g1" }),
    );
    assert!(
        call(&mut engine, &mut ui, "controller/claim", claim)
            .get("result")
            .is_some()
    );
    orders.take();
    engine.command_result(&alt_hold, Outcome::Accepted, 1);
    assert!(orders.take().is_empty(), "a claim never sends a command");
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
    assert_eq!(plugged["result"], json!({}), "{plugged}");
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
fn reset_connected_controller_clears_choices_without_editing_shared_mappings() {
    let (mut engine, mut ui, orders) = setup();
    let ana = as_ana(&mut engine, &mut ui);
    engine.controller_connected("g2", "Xbox", true);
    let made = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({"name":"Shared", "from":"mode-2"}),
    );
    let id = made["result"]["id"].as_str().unwrap();
    for device in ["g1", "g2"] {
        for vehicle in ["v1", "v2"] {
            let chosen = call(
                &mut engine,
                &mut ui,
                "controller/chooseMapping",
                json!({"id":device,"mappingId":id,"vehicleId":vehicle}),
            );
            assert!(chosen.get("result").is_some(), "{chosen}");
        }
    }
    let before = engine.state().input_mappings[id].clone();
    orders.take();
    let result = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({"id":"g1"}),
    );
    assert_eq!(result["result"], json!({}), "{result}");
    let controller = &engine.state().controllers["g1"];
    assert!(controller.connected);
    assert!(controller.claim.is_none());
    let fresh = &engine.state().input_mappings[&controller.mapping_id];
    assert!(
        fresh.buttons.is_empty()
            && fresh.actions.is_empty()
            && fresh.positions.is_empty()
            && fresh.rc_channels.is_empty()
    );
    assert!(
        [fresh.roll, fresh.pitch, fresh.yaw, fresh.throttle]
            .iter()
            .all(|b| b.calibration.is_none())
    );
    assert_eq!(engine.state().input_mappings[id], before);
    let operator = &engine.state().operators[&ana];
    assert_eq!(
        operator.mapping_for("g1", "Xbox", &["v1"]),
        Some(fresh.id.as_str())
    );
    assert_eq!(operator.mapping_for("g2", "Xbox", &["v2"]), Some(id));
    assert!(orders.take().iter().all(|order| order == "route g1 none"));
    // Kept across a restart, the reset still wins over another Xbox's per-vehicle picks.
    let restored: uxduck_engine::State =
        serde_json::from_str(&serde_json::to_string(engine.state()).unwrap()).unwrap();
    assert_eq!(
        restored.operators[&ana].mapping_for("g1", "Xbox", &["v2"]),
        Some(fresh.id.as_str())
    );
}

#[test]
fn resetting_a_connected_controller_forgets_only_its_target_without_restoring_control() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("unit-g1", "Xbox", true);
    engine.controller_connected("unit-g2", "Other controller", true);
    for id in ["unit-g1", "unit-g2"] {
        let saved = call(
            &mut engine,
            &mut ui,
            "controller/setTarget",
            json!({ "id": id, "mode": "pinned", "vehicleId": "v2" }),
        );
        assert_eq!(saved["result"]["lastTarget"]["vehicleId"], "v2", "{saved}");
    }
    let other = engine.state().controllers["unit-g2"].clone();
    let mappings = engine.state().input_mappings.clone();
    orders.take();
    let reset = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({ "id": "unit-g1" }),
    );
    assert_eq!(reset["result"], json!({}), "{reset}");
    let controller = &engine.state().controllers["unit-g1"];
    assert!(controller.connected);
    assert!(controller.last_target.is_none());
    assert!(controller.claim.is_none());
    assert!(controller.target.is_none());
    assert!(controller.pending_target.is_none());
    assert_eq!(engine.state().controllers["unit-g2"], other);
    for (id, mapping) in mappings {
        assert_eq!(engine.state().input_mappings[&id], mapping);
    }
    assert_eq!(orders.take(), vec!["route unit-g1 none"]);
    let restored: uxduck_engine::State =
        serde_json::from_str(&serde_json::to_string(engine.state()).unwrap()).unwrap();
    assert!(restored.controllers["unit-g1"].last_target.is_none());
    assert!(restored.controllers["unit-g1"].claim.is_none());
    assert_eq!(
        restored.controllers["unit-g2"].last_target,
        other.last_target
    );
}

#[test]
fn a_refused_reset_keeps_the_remembered_target_and_current_control_unchanged() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_connected("unit-g1", "Xbox", true);
    engine.controller_neutral("unit-g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "unit-g1", "mode": "pinned", "vehicleId": "v2", "mappingId": "mode-2" }),
    );
    assert_eq!(
        claimed["result"]["lastTarget"]["vehicleId"], "v2",
        "{claimed}"
    );
    let before = engine.state().controllers["unit-g1"].clone();
    orders.take();
    let refused = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({ "id": "unit-g1" }),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    assert_eq!(engine.state().controllers["unit-g1"], before);
    assert!(orders.take().is_empty());
}

#[test]
fn deleting_a_mapping_resets_its_devices_and_removes_all_saved_selections() {
    let (mut engine, mut ui, orders) = setup();
    let ana = as_ana(&mut engine, &mut ui);
    let made = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({"name":"Old", "from":"mode-2"}),
    );
    let id = made["result"]["id"].as_str().unwrap();
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1","mappingId":id}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    call(
        &mut engine,
        &mut ui,
        "controller/chooseMapping",
        json!({"id":"g1", "mappingId":id,"vehicleId":"v1"}),
    );
    orders.take();
    let result = call(
        &mut engine,
        &mut ui,
        "mapping/remove",
        json!({"id":id,"expectedVersion":made["result"]["version"]}),
    );
    assert_eq!(result["result"], json!({}), "{result}");
    assert!(!engine.state().input_mappings.contains_key(id));
    let controller = &engine.state().controllers["g1"];
    assert_ne!(controller.mapping_id, id);
    assert!(
        engine.state().input_mappings[&controller.mapping_id]
            .roll
            .calibration
            .is_none()
    );
    assert!(
        engine.state().operators[&ana]
            .controller_mappings
            .iter()
            .all(|m| m.mapping_id != id)
    );
    assert!(orders.take().iter().all(|order| order == "route g1 none"));
}

#[test]
fn deleting_a_mapping_checks_version_builtins_and_every_claim_before_changing_state() {
    let (mut engine, mut ui, orders) = setup();
    let made = call(
        &mut engine,
        &mut ui,
        "mapping/create",
        json!({"name":"Held", "from":"mode-2"}),
    );
    let id = made["result"]["id"].as_str().unwrap();
    engine.controller_connected("g0", "Second Xbox", true);
    engine.controller_neutral("g0", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g0","mode":"pinned","vehicleId":"v2","mappingId":id}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g0"}),
    );
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1","mappingId":id}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    orders.take();
    let before = serde_json::to_value(engine.state()).unwrap();
    for (mapping, version, kind) in [
        ("mode-2", 1, "invalid-value"),
        (id, 999, "version-conflict"),
        (id, 1, "busy"),
    ] {
        let result = call(
            &mut engine,
            &mut ui,
            "mapping/remove",
            json!({"id":mapping,"expectedVersion":version}),
        );
        assert_eq!(error_kind(&result), kind, "{result}");
        assert_eq!(serde_json::to_value(engine.state()).unwrap(), before);
    }
    let reset = call(
        &mut engine,
        &mut ui,
        "controller/forget",
        json!({"id":"g1"}),
    );
    assert_eq!(error_kind(&reset), "busy");
    assert_eq!(serde_json::to_value(engine.state()).unwrap(), before);
    assert!(orders.take().is_empty());
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    let deleted = call(
        &mut engine,
        &mut ui,
        "mapping/remove",
        json!({"id":id,"expectedVersion":1}),
    );
    assert!(deleted.get("result").is_some(), "{deleted}");
}

#[test]
fn pilot_input_preferences_persist_and_cannot_change_a_claimed_target() {
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
        engine.state().controllers["g1"].target.as_deref(),
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
            connections: None,
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

#[test]
fn a_reloaded_window_takes_back_its_controller_still_driving() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    orders.take();
    let before = engine.state().controllers["g1"].claim.clone().unwrap();

    // A reload: the window goes and the page connects again, sticks off
    // centre and all. Nothing stops driving, and the new page holds it.
    engine.controller_neutral("g1", false);
    engine.disconnect(ui.id);
    let mut cli = connect(&mut engine, "operator-cli");
    assert!(engine.state().controllers["g1"].claim.is_some());
    let mut again = connect(&mut engine, "operator-ui");
    assert!(orders.take().is_empty(), "never stopped driving Alpha");
    let controller = &engine.state().controllers["g1"];
    assert_eq!(controller.target.as_deref(), Some("v1"));
    let claim = controller.claim.clone().unwrap();
    assert_ne!(claim.client_id, before.client_id, "held by the new page");
    assert!(
        !titles(&engine)
            .iter()
            .any(|t| t.contains("stopped driving"))
    );

    // It is the new page's to change, and the grace no longer applies.
    engine.expire_detached_claims(u64::MAX);
    assert!(engine.state().controllers["g1"].claim.is_some());
    let pinned = call(
        &mut engine,
        &mut again,
        "controller/setTarget",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    assert!(pinned.get("result").is_some(), "{pinned}");
    let busy = call(
        &mut engine,
        &mut cli,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" }),
    );
    assert_eq!(error_kind(&busy), "busy");

    // Unplugged during a grace: released at once.
    engine.disconnect(again.id);
    engine.controller_disconnected("g1");
    assert_eq!(orders.take(), ["route g1 none"]);
    assert!(engine.state().controllers["g1"].claim.is_none());
}

fn keyboard_sim(engine: &mut Engine, ui: &mut Client) {
    let link = call(
        engine,
        ui,
        "link/add",
        json!({ "kind": {
            "type": "simulator", "vehicle": "multicopter",
            "latitudeDeg": -35.36, "longitudeDeg": 149.16,
            "altitudeM": 584.0, "headingDeg": 0.0
        }}),
    );
    let link_id = link["result"]["id"]
        .as_str()
        .expect("simulation link")
        .to_owned();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "sim".into(),
        aliases: Vec::new(),
        link_id,
        peer: "simulator".into(),
        system_id: 3,
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

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario across real and simulated vehicles"
)]
fn keyboard_drives_real_vehicles_and_simulations_with_exclusive_pinned_targets() {
    let (mut engine, mut ui, orders) = setup();
    let real = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"v1", "throttle":-1.0}),
    );
    assert_eq!(real["result"]["target"], "v1", "{real}");
    let id = real["result"]["id"].as_str().unwrap();
    assert!(
        orders
            .take()
            .iter()
            .any(|order| order.starts_with(&format!("route {id} v1 ")))
    );
    engine.controller_neutral("g1", true);
    let busy = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1", "mode":"pinned", "vehicleId":"v1"}),
    );
    assert_eq!(error_kind(&busy), "busy", "{busy}");
    let input = call(
        &mut engine,
        &mut ui,
        "controller/keyboardInput",
        json!({"axes":[0.5,0.0,0.0,-1.0]}),
    );
    assert!(input.get("result").is_some(), "{input}");
    engine.disconnect(ui.id);
    assert!(engine.state().controllers[id].claim.is_none());
    assert!(!engine.state().controllers[id].connected);
    assert!(
        orders
            .take()
            .iter()
            .any(|order| order == &format!("route {id} none"))
    );
    let mut ui = connect(&mut engine, "operator-ui");
    keyboard_sim(&mut engine, &mut ui);
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1", "mode":"pinned", "vehicleId":"sim"}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    let refused = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":-1.0}),
    );
    assert_eq!(error_kind(&refused), "busy", "{refused}");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    let keyboard = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":-1.0}),
    );
    let id = keyboard["result"]["id"].as_str().unwrap();
    assert_eq!(keyboard["result"]["mode"], "pinned");
    assert_eq!(keyboard["result"]["target"], "sim");
    let target = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({"id":id, "mode":"pinned", "vehicleId":"v1"}),
    );
    assert_eq!(error_kind(&target), "capability-denied");
    let busy = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1", "mode":"pinned", "vehicleId":"sim"}),
    );
    assert_eq!(error_kind(&busy), "busy");
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({"vehicleId":"v2"}),
    );
    assert_eq!(
        engine.state().controllers[id].target.as_deref(),
        Some("sim")
    );
    assert!(
        orders
            .take()
            .iter()
            .any(|order| order.starts_with("route keyboard:"))
    );
}

#[test]
fn keyboard_stays_claimed_through_idle_until_explicit_release_or_disconnect() {
    let (mut engine, mut ui, _) = setup();
    keyboard_sim(&mut engine, &mut ui);
    let mut other = connect(&mut engine, "operator-ui");
    let keyboard = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":0.0}),
    );
    let id = keyboard["result"]["id"].as_str().unwrap().to_owned();
    let frame = json!({"axes":[0.5, 0.0, 0.0, 0.0]});
    let denied = call(
        &mut engine,
        &mut other,
        "controller/keyboardInput",
        frame.clone(),
    );
    assert_eq!(error_kind(&denied), "capability-denied");
    let invalid = call(
        &mut engine,
        &mut ui,
        "controller/keyboardInput",
        json!({"axes":[2.0,0.0,0.0,0.0]}),
    );
    assert_eq!(error_kind(&invalid), "invalid-value");
    assert!(
        call(
            &mut engine,
            &mut ui,
            "controller/keyboardInput",
            frame.clone()
        )
        .get("result")
        .is_some()
    );
    engine.expire_detached_claims(60_000_000);
    assert!(engine.state().controllers[&id].claim.is_some());
    assert!(engine.state().controllers[&id].connected);
    let resumed = call(
        &mut engine,
        &mut ui,
        "controller/keyboardInput",
        frame.clone(),
    );
    assert!(resumed.get("result").is_some(), "{resumed}");
    let generic = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":id,"mode":"pinned","vehicleId":"sim"}),
    );
    assert_eq!(error_kind(&generic), "capability-denied");
    let stopped = call(&mut engine, &mut ui, "controller/keyboardStop", json!({}));
    assert!(stopped.get("result").is_some());
    let late = call(
        &mut engine,
        &mut ui,
        "controller/keyboardInput",
        frame.clone(),
    );
    assert_eq!(error_kind(&late), "capability-denied");
    let again = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":0.0}),
    );
    assert!(again.get("result").is_some(), "{again}");
    engine.disconnect(ui.id);
    assert!(
        engine.state().controllers[&id].claim.is_none(),
        "no 15-second physical-controller grace"
    );
    assert!(!engine.state().controllers[&id].connected);
}

#[test]
fn manually_starting_and_releasing_keyboard_control_is_quiet() {
    let (mut engine, mut ui, _) = setup();
    keyboard_sim(&mut engine, &mut ui);
    let before = engine.state().notifications.len();
    let started = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":0.0}),
    );
    assert!(started.get("result").is_some(), "{started}");
    let stopped = call(&mut engine, &mut ui, "controller/keyboardStop", json!({}));
    assert!(stopped.get("result").is_some(), "{stopped}");
    assert_eq!(engine.state().notifications.len(), before);
}

/// S291: a page that lets its keyboard go because UXDuck left the front says
/// so; quietly while disarmed, as a warning (with its tone) while armed.
#[test]
fn a_keyboard_released_for_focus_says_so_louder_when_armed() {
    let (mut engine, mut ui, _) = setup();
    keyboard_sim(&mut engine, &mut ui);
    let unfocused = |engine: &mut Engine, ui: &mut Client| {
        let started = call(
            engine,
            ui,
            "controller/keyboardStart",
            json!({"vehicleId":"sim", "throttle":0.0}),
        );
        assert!(started.get("result").is_some(), "{started}");
        let before: Vec<String> = engine.state().notifications.keys().cloned().collect();
        let stopped = call(
            engine,
            ui,
            "controller/keyboardStop",
            json!({"reason":"appUnfocused"}),
        );
        assert!(stopped.get("result").is_some(), "{stopped}");
        let id = started["result"]["id"].as_str().unwrap().to_owned();
        assert!(engine.state().controllers[&id].claim.is_none());
        let raised: Vec<_> = engine
            .state()
            .notifications
            .values()
            .filter(|n| !before.contains(&n.id))
            .cloned()
            .collect();
        assert_eq!(raised.len(), 1, "{raised:?}");
        raised[0].clone()
    };
    let quiet = unfocused(&mut engine, &mut ui);
    assert_eq!(
        quiet.severity,
        uxduck_core::domain::notification::Severity::Info
    );
    assert!(
        quiet.title.contains("UXDuck isn't the app in front"),
        "{}",
        quiet.title
    );
    let link_id = engine.state().vehicles["sim"].link_id.clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "sim".into(),
        aliases: Vec::new(),
        link_id,
        peer: "simulator".into(),
        system_id: 3,
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
    assert!(engine.state().vehicles["sim"].armed);
    let loud = unfocused(&mut engine, &mut ui);
    assert_eq!(
        loud.severity,
        uxduck_core::domain::notification::Severity::Warning
    );
    assert!(loud.body.unwrap().contains("failsafe takes over"));
}

#[test]
fn an_actual_keyboard_disconnect_still_releases_its_claim() {
    let (mut engine, mut ui, _) = setup();
    keyboard_sim(&mut engine, &mut ui);
    let started = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sim", "throttle":0.0}),
    );
    let id = started["result"]["id"].as_str().unwrap().to_owned();
    engine.controller_disconnected(&id);
    assert!(engine.state().controllers[&id].claim.is_none());
    let late = call(
        &mut engine,
        &mut ui,
        "controller/keyboardInput",
        json!({"axes":[0.0,0.0,0.0,0.0]}),
    );
    assert_eq!(error_kind(&late), "capability-denied");
}

#[test]
fn switching_operators_requires_releasing_control_without_changing_the_route() {
    let (mut engine, mut ui, orders) = setup();
    let ana = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name":"Ana"}),
    )["result"]
        .clone();
    let ben = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name":"Ben"}),
    )["result"]
        .clone();
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({"operatorId":ana["id"]}),
    );
    engine.controller_neutral("g1", true);
    let claim = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    assert!(claim.get("result").is_some(), "{claim}");
    orders.take();
    let denied = call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    assert_eq!(error_kind(&denied), "confirmation-required", "{denied}");
    assert!(
        orders.take().is_empty(),
        "refusing a switch must not interrupt live control"
    );
    assert_eq!(
        engine.state().controllers["g1"]
            .claim
            .as_ref()
            .unwrap()
            .operator_id
            .as_deref(),
        ana["id"].as_str()
    );
    let removed = call(
        &mut engine,
        &mut ui,
        "operator/remove",
        json!({"id":ana["id"],"expectedVersion":ana["version"]}),
    );
    assert_eq!(error_kind(&removed), "confirmation-required");
    call(
        &mut engine,
        &mut ui,
        "controller/release",
        json!({"id":"g1"}),
    );
    orders.take();
    let switched = call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    assert!(switched.get("result").is_some(), "{switched}");
    assert!(engine.state().controllers["g1"].claim.is_none());
    assert!(
        orders.take().is_empty(),
        "the new person must deliberately claim control"
    );
}

#[test]
fn a_reloaded_window_restores_the_operator_whose_controller_it_took_back() {
    let (mut engine, mut ui, orders) = setup();
    let ben = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name":"Ben"}),
    )["result"]
        .clone();
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    orders.take();

    // The page reconnects and takes Ben's controller back before it says
    // it is Ben again. Becoming Ben is not a switch away from Ben.
    engine.disconnect(ui.id);
    let mut again = connect(&mut engine, "operator-ui");
    let restored = call(
        &mut engine,
        &mut again,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    assert!(restored.get("result").is_some(), "{restored}");
    assert!(orders.take().is_empty(), "never stopped driving");
    assert!(engine.state().controllers["g1"].claim.is_some());

    // Becoming anyone else still asks for the controller first.
    let guest = call(
        &mut engine,
        &mut again,
        "client/setOperator",
        json!({"operatorId":null}),
    );
    assert_eq!(error_kind(&guest), "confirmation-required", "{guest}");
}

#[test]
fn a_pending_account_change_cannot_race_with_claiming_a_controller() {
    #[derive(Clone, Default)]
    struct AccountOrders(Arc<Mutex<Vec<uxduck_engine::account::AccountOrder>>>);
    impl uxduck_engine::account::AccountPort for AccountOrders {
        fn request(&mut self, order: uxduck_engine::account::AccountOrder) {
            self.0.lock().unwrap().push(order);
        }
    }
    let (mut engine, mut ui, _) = setup();
    let operator = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name":"Ana"}),
    )["result"]
        .clone();
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({"operatorId":operator["id"]}),
    );
    let orders = AccountOrders::default();
    engine.set_account(Box::new(orders.clone()));
    assert!(
        send(
            &mut engine,
            &mut ui,
            "account/verify",
            json!({"email":"ana@example.com","code":"123456","productUpdates":false})
        )
        .is_none()
    );
    engine.controller_neutral("g1", true);
    let denied = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    assert_eq!(error_kind(&denied), "busy", "{denied}");
    let order = orders.0.lock().unwrap().pop().unwrap();
    engine.account_done(
        &order.id,
        Err(uxduck_contracts::errors::ApiError::new(
            uxduck_contracts::errors::ErrorKind::InvalidValue,
            "The code expired.",
        )),
    );
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
}

/// One account per computer (ADR-0033, 2026-10-31): a switch hands over
/// control, never the account, so only a window of the operator holding a
/// controller is refused; another window may choose another operator.
#[test]
fn a_held_controller_refuses_its_operators_switch_but_not_another_windows() {
    let (mut engine, mut ana, _) = setup();
    let profile = call(
        &mut engine,
        &mut ana,
        "operator/create",
        json!({"name":"Ana"}),
    )["result"]
        .clone();
    call(
        &mut engine,
        &mut ana,
        "client/setOperator",
        json!({"operatorId":profile["id"]}),
    );
    engine.set_account_identity(uxduck_contracts::dto::account::AccountState {
        signed_in: true,
        account_id: Some("ana-account".into()),
        ..Default::default()
    });
    engine.controller_neutral("g1", true);
    call(
        &mut engine,
        &mut ana,
        "controller/claim",
        json!({"id":"g1","mode":"pinned","vehicleId":"v1"}),
    );
    let mut other = connect(&mut engine, "operator-ui");
    let ben = call(
        &mut engine,
        &mut other,
        "operator/create",
        json!({"name":"Ben"}),
    )["result"]
        .clone();
    let refused = call(
        &mut engine,
        &mut ana,
        "operator/switch",
        json!({"operatorId":ben["id"]}),
    );
    assert_eq!(error_kind(&refused), "confirmation-required", "{refused}");
    let restored = call(
        &mut engine,
        &mut other,
        "client/setOperator",
        json!({"operatorId":profile["id"]}),
    );
    assert!(
        restored.get("result").is_some(),
        "restoring another window for Ana preserves control: {restored}"
    );
    let refused = call(
        &mut engine,
        &mut other,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    assert_eq!(
        error_kind(&refused),
        "confirmation-required",
        "Ana's other window holds her controller too: {refused}"
    );
    let mut third = connect(&mut engine, "operator-ui");
    let chosen = call(
        &mut engine,
        &mut third,
        "client/setOperator",
        json!({"operatorId":ben["id"]}),
    );
    assert!(
        chosen.get("result").is_some(),
        "Ben's own window takes no control from Ana: {chosen}"
    );
    assert!(engine.state().controllers["g1"].claim.is_some());
    let signed_out = call(&mut engine, &mut third, "account/signOut", json!({}));
    assert_eq!(
        error_kind(&signed_out),
        "confirmation-required",
        "signing the computer out asks for every controller: {signed_out}"
    );
}

/// A mapping is the person's (maintainer, 2026-10-23): each operator's
/// claim remembers theirs, and a later claim without one uses it, not the
/// mapping someone else last used on this controller.
#[test]
fn each_operator_keeps_their_own_mapping_for_a_controller() {
    let (mut engine, mut ui, _orders) = setup();
    let ana = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name": "Ana"}),
    )["result"]["id"]
        .clone();
    let ben = call(
        &mut engine,
        &mut ui,
        "operator/create",
        json!({"name": "Ben"}),
    )["result"]["id"]
        .clone();
    engine.controller_neutral("g1", true);
    let claim_as =
        |engine: &mut Engine, ui: &mut Client, operator: &Value, mapping: Option<&str>| {
            call(
                engine,
                ui,
                "client/setOperator",
                json!({"operatorId": operator}),
            );
            let mut params = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" });
            if let Some(mapping) = mapping {
                params["mappingId"] = json!(mapping);
            }
            let claimed = call(engine, ui, "controller/claim", params);
            assert!(claimed.get("result").is_some(), "{claimed}");
            let used = engine.state().controllers["g1"].mapping_id.clone();
            call(engine, ui, "controller/release", json!({ "id": "g1" }));
            used
        };
    assert_eq!(
        claim_as(&mut engine, &mut ui, &ana, Some("mode-1")),
        "mode-1"
    );
    assert_eq!(
        claim_as(&mut engine, &mut ui, &ben, Some("mode-2")),
        "mode-2"
    );
    // Ana again, naming none: hers, not the one Ben left on the controller.
    assert_eq!(claim_as(&mut engine, &mut ui, &ana, None), "mode-1");
    let kept = &engine.state().operators[ana.as_str().unwrap()].controller_mappings;
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (
            kept[0].controller_name.as_str(),
            kept[0].mapping_id.as_str()
        ),
        ("Xbox", "mode-1")
    );
    let listed = call(&mut engine, &mut ui, "operator/list", json!({}));
    let ben_listed = listed["result"]["operators"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == ben)
        .unwrap()
        .clone();
    assert_eq!(ben_listed["controllerMappings"][0]["mappingId"], "mode-2");
}

/// Ana as this window's operator.
fn as_ana(engine: &mut Engine, ui: &mut Client) -> String {
    let ana = call(engine, ui, "operator/create", json!({"name": "Ana"}))["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    call(engine, ui, "client/setOperator", json!({"operatorId": ana}));
    ana
}

/// Each vehicle keeps the mapping it was flown with (maintainer,
/// 2026-11-12), and a held controller following the selection switches to
/// it. A vehicle without one keeps the mapping in use.
#[test]
fn a_held_controller_switches_to_each_vehicle_s_own_mapping() {
    let (mut engine, mut ui, _orders) = setup();
    let ana = as_ana(&mut engine, &mut ui);
    engine.controller_neutral("g1", true);
    for (vehicle, mapping) in [("v1", "mode-1"), ("v2", "mode-2")] {
        let params =
            json!({ "id": "g1", "mode": "pinned", "vehicleId": vehicle, "mappingId": mapping });
        let claimed = call(&mut engine, &mut ui, "controller/claim", params);
        assert!(claimed.get("result").is_some(), "{claimed}");
        call(
            &mut engine,
            &mut ui,
            "controller/release",
            json!({ "id": "g1" }),
        );
    }
    let kept: Vec<_> = engine.state().operators[&ana]
        .controller_mappings
        .iter()
        .map(|m| (m.vehicle_id.as_deref(), m.mapping_id.as_str()))
        .collect();
    assert_eq!(kept, [(Some("v1"), "mode-1"), (Some("v2"), "mode-2")]);

    // Named none, a claim takes the vehicle's own.
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    let follow = json!({ "id": "g1", "mode": "follows-selection" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", follow);
    assert_eq!(claimed["result"]["mappingId"], "mode-1", "{claimed}");

    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    let controller = &engine.state().controllers["g1"];
    assert_eq!(controller.target.as_deref(), Some("v2"));
    assert_eq!(controller.mapping_id, "mode-2");
    let bravo = engine.state().vehicles["v2"].name.clone();
    assert!(
        titles(&engine).contains(&format!("Xbox now drives {bravo} with Mode 2")),
        "{:?}",
        titles(&engine)
    );

    // A vehicle with none of its own keeps the one in use.
    observe_v3(&mut engine, "Ardupilotmega", "Quadrotor", false);
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v3" }),
    );
    let controller = &engine.state().controllers["g1"];
    assert_eq!(controller.target.as_deref(), Some("v3"));
    assert_eq!(controller.mapping_id, "mode-2");

    // Pinned to another vehicle from the dropdown: its own again.
    let pinned = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" });
    let moved = call(&mut engine, &mut ui, "controller/setTarget", pinned);
    assert_eq!(moved["result"]["mappingId"], "mode-1", "{moved}");
}

/// A mapping chosen for a vehicle is kept on the operator before any claim,
/// never for the vehicle the controller flies now, and never without an
/// operator.
#[test]
fn a_mapping_is_chosen_for_a_vehicle_but_not_for_the_one_it_flies() {
    let (mut engine, mut ui, _orders) = setup();
    let choose = |engine: &mut Engine, ui: &mut Client, vehicle: &str, mapping: &str| {
        call(
            engine,
            ui,
            "controller/chooseMapping",
            json!({ "id": "g1", "vehicleId": vehicle, "mappingId": mapping }),
        )
    };
    let nobody = choose(&mut engine, &mut ui, "v2", "mode-1");
    assert_eq!(error_kind(&nobody), "invalid-value", "{nobody}");
    let ana = as_ana(&mut engine, &mut ui);
    let unknown = choose(&mut engine, &mut ui, "v2", "nope");
    assert_eq!(error_kind(&unknown), "not-found", "{unknown}");
    let chosen = choose(&mut engine, &mut ui, "v2", "mode-1");
    assert!(chosen.get("result").is_some(), "{chosen}");
    let kept = &engine.state().operators[&ana].controller_mappings;
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].vehicle_id.as_deref(), Some("v2"));
    assert_eq!(kept[0].mapping_id, "mode-1");

    engine.controller_neutral("g1", true);
    let claim = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1", "mappingId": "mode-2" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", claim);
    assert!(claimed.get("result").is_some(), "{claimed}");
    let flying = choose(&mut engine, &mut ui, "v1", "mode-1");
    assert_eq!(error_kind(&flying), "busy", "{flying}");
    assert!(
        flying["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Release it to change the mapping it uses there."),
        "{flying}"
    );
    assert_eq!(engine.state().controllers["g1"].mapping_id, "mode-2");
    // Choosing again the one it flies with changes nothing.
    let same = choose(&mut engine, &mut ui, "v1", "mode-2");
    assert!(same.get("result").is_some(), "{same}");

    let pinned = json!({ "id": "g1", "mode": "pinned", "vehicleId": "v2" });
    let moved = call(&mut engine, &mut ui, "controller/setTarget", pinned);
    assert_eq!(moved["result"]["mappingId"], "mode-1", "{moved}");
}

/// A vehicle's own mapping that reads nothing of the controller is refused
/// as a claim with it would be: the switch waits, on the old vehicle with
/// the old mapping.
#[test]
fn a_switch_to_a_mapping_that_reads_nothing_waits() {
    let (mut engine, mut ui, orders) = setup_with(NoSystem {
        // A radio in joystick mode: its inputs by code, no gamepad names.
        controller_inputs: serde_json::from_value(json!({ "controllers": [{
            "id": "g1", "layoutKnown": false, "buttons": [],
            "axes": [
                { "name": "X", "code": 0x1_0030, "number": 1, "value": 0.0 },
                { "name": "Y", "code": 0x1_0031, "number": 2, "value": 0.0 },
                { "name": "Z", "code": 0x1_0032, "number": 3, "value": 0.0 },
                { "name": "Rx", "code": 0x1_0033, "number": 4, "value": 0.0 },
            ],
        }] }))
        .unwrap(),
        ..NoSystem::default()
    });
    as_ana(&mut engine, &mut ui);
    let chosen = call(
        &mut engine,
        &mut ui,
        "controller/chooseMapping",
        json!({ "id": "g1", "vehicleId": "v2", "mappingId": "mode-2" }),
    );
    assert!(chosen.get("result").is_some(), "{chosen}");
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v1" }),
    );
    let follow = json!({ "id": "g1", "mode": "follows-selection", "mappingId": "radio" });
    let claimed = call(&mut engine, &mut ui, "controller/claim", follow);
    assert!(claimed.get("result").is_some(), "{claimed}");
    orders.take();
    call(
        &mut engine,
        &mut ui,
        "client/select",
        json!({ "vehicleId": "v2" }),
    );
    let controller = &engine.state().controllers["g1"];
    assert_eq!(controller.target.as_deref(), Some("v1"));
    assert_eq!(controller.pending_target.as_deref(), Some("v2"));
    assert_eq!(controller.mapping_id, "radio");
    assert!(!orders.take().iter().any(|o| o.starts_with("route")));
    let retried = call(
        &mut engine,
        &mut ui,
        "controller/setTarget",
        json!({ "id": "g1", "mode": "follows-selection" }),
    );
    assert!(
        retried["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Mode 2 doesn't read Xbox's"),
        "{retried}"
    );
}

/// Alpha heard again, armed or not, as `presence`.
fn observe_v1(engine: &mut Engine, armed: bool, presence: Presence) {
    let link_id = engine.state().vehicles["v1"].link_id.clone();
    engine.vehicle_observed(Observation {
        connections: None,
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
        presence,
        last_heartbeat_us: 7,
        armed,
        mode: None,
    });
}

#[test]
fn a_controller_arms_disarms_and_toggles_with_holds_shown_and_refusals_said() {
    let (mut engine, mut ui, orders) = setup();
    engine.controller_neutral("g1", true);
    let edit = |version: u64, hold_ms: u32| {
        json!({
            "id": "mode-2", "expectedVersion": version, "name": "Mode 2",
            "roll": { "axis": "right-stick-x", "inverted": false },
            "pitch": { "axis": "right-stick-y", "inverted": false },
            "yaw": { "axis": "left-stick-x", "inverted": false },
            "throttle": { "axis": "left-stick-y", "inverted": false },
            "deadzone": 0.05,
            "buttons": [
                { "button": "south", "command": { "type": "arm" }, "holdMs": hold_ms },
                { "button": "north", "command": { "type": "disarm" } },
                { "button": "east", "command": { "type": "arm-toggle" } },
                { "button": "west", "command": { "type": "set-mode", "mode": "GUIDED" } }
            ]
        })
    };
    let refused = call(&mut engine, &mut ui, "mapping/update", edit(1, 20_000));
    assert_eq!(error_kind(&refused), "invalid-value", "a 20 s hold");
    let saved = call(&mut engine, &mut ui, "mapping/update", edit(1, 1_000));
    assert_eq!(saved["result"]["buttons"][0]["holdMs"], 1_000, "{saved}");

    // Not claimed: a hold shows nothing, as its command would not go.
    engine.controller_holding("g1", Command::Arm, 1_000);
    assert!(engine.state().controllers["g1"].holding.is_none());
    call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v1" }),
    );
    orders.take();

    engine.controller_holding("g1", Command::Arm, 1_000);
    let shown = call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["controller"] }),
    );
    assert_eq!(
        shown["result"]["snapshot"]["controllers"][0]["holding"],
        json!({ "command": { "type": "arm" }, "holdMs": 1_000 }),
        "{shown}"
    );
    engine.controller_let_go("g1");
    assert!(engine.state().controllers["g1"].holding.is_none());

    // The data plane times the hold; the engine sends what it is told.
    engine.controller_pressed("g1", Some(GamepadButton::South), None);
    engine.controller_pressed("g1", Some(GamepadButton::North), None);
    assert_eq!(
        orders.take(),
        [
            format!("command v1 {:?}", Command::Arm),
            format!("command v1 {:?}", Command::Disarm)
        ]
    );

    // A toggle goes as what the vehicle needs, and is receipted so.
    engine.controller_pressed("g1", Some(GamepadButton::East), None);
    observe_v1(&mut engine, true, Presence::Online);
    engine.controller_pressed("g1", Some(GamepadButton::East), None);
    assert_eq!(
        orders.take(),
        [
            format!("command v1 {:?}", Command::Arm),
            format!("command v1 {:?}", Command::Disarm)
        ]
    );
    assert!(
        engine
            .state()
            .receipts
            .values()
            .all(|r| r.command != Command::ArmToggle),
        "never receipted as a toggle"
    );

    // A refusal that is no receipt is said, not dropped.
    observe_v1(&mut engine, true, Presence::Stale);
    engine.controller_pressed("g1", Some(GamepadButton::West), None);
    assert!(orders.take().is_empty());
    assert!(
        titles(&engine)
            .iter()
            .any(|t| t == "UXDuck held back Set mode GUIDED from Xbox to Alpha"),
        "{:?}",
        titles(&engine)
    );

    // Unplugged mid-hold: it ends with the controller.
    engine.controller_holding("g1", Command::Disarm, 2_000);
    engine.controller_disconnected("g1");
    assert!(engine.state().controllers["g1"].holding.is_none());
}

#[test]
#[allow(clippy::too_many_lines, reason = "one scenario from setup to hold")]
fn a_sub_gripper_is_set_up_from_its_parameters_and_joins_the_pilot_route() {
    use std::collections::BTreeMap;
    use uxduck_core::domain::parameter::ParamKind;
    use uxduck_engine::transfers::Reported;

    let (mut engine, mut ui, orders) = setup();
    let quad = call(
        &mut engine,
        &mut ui,
        "vehicle/gripper",
        json!({"id":"v1", "gripper":{"actuator":1, "opensAt":"max"}}),
    );
    assert!(quad.get("error").is_some(), "a quad has no gripper: {quad}");
    let link_id = engine.state().vehicles["v1"].link_id.clone();
    engine.vehicle_observed(Observation {
        connections: None,
        id: "sub".into(),
        aliases: Vec::new(),
        link_id,
        peer: "127.0.0.1:5763".into(),
        system_id: 3,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Submarine".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    let bad = call(
        &mut engine,
        &mut ui,
        "vehicle/gripper",
        json!({"id":"sub", "gripper":{"actuator":7, "opensAt":"max"}}),
    );
    assert!(bad.get("error").is_some(), "{bad}");
    let set = call(
        &mut engine,
        &mut ui,
        "vehicle/gripper",
        json!({"id":"sub", "gripper":{"actuator":1, "opensAt":"max"}}),
    );
    assert_eq!(set["result"]["gripper"]["actuator"], 1, "{set}");
    let status = call(&mut engine, &mut ui, "gripper/status", json!({"id":"sub"}));
    assert_eq!(
        status["result"]["problems"],
        json!(["Read the vehicle's parameters first"]),
        "{status}"
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "gripper/hold",
        json!({"vehicleId":"sub", "action":"open"}),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Claim a controller"),
        "no pilot route, no hold: {refused}"
    );
    let released = call(
        &mut engine,
        &mut ui,
        "gripper/hold",
        json!({"vehicleId":"sub"}),
    );
    assert!(
        released.get("result").is_some(),
        "a release is always taken: {released}"
    );

    let reported = [
        ("ACTUATOR1_INC", 0.01),
        ("SERVO9_FUNCTION", 181.0),
        ("SERVO10_FUNCTION", 7.0),
        ("SERVO11_FUNCTION", 184.0),
        ("SERVO11_MIN", 1100.0),
        ("SERVO11_MAX", 1900.0),
        ("SERVO12_FUNCTION", 0.0),
        ("BTN0_FUNCTION", 76.0),
        ("BTN1_FUNCTION", 6.0),
        ("BTN15_FUNCTION", 77.0),
        ("BTN16_FUNCTION", 0.0),
    ]
    .into_iter()
    .map(|(name, value)| Reported {
        name: name.into(),
        value,
        kind: ParamKind::Real32,
    })
    .collect();
    engine.parameters_downloaded("sub", Ok(reported), &BTreeMap::new());
    let status = call(&mut engine, &mut ui, "gripper/status", json!({"id":"sub"}));
    assert_eq!(
        status["result"]["wiring"],
        json!({"output":11, "openButton":15, "closeButton":0, "minUs":1100, "maxUs":1900, "neutralUs":1500}),
        "{status}"
    );
    assert_eq!(status["result"]["problems"], json!([]));
    assert_eq!(status["result"]["freeOutputs"], json!([12]));
    assert_eq!(status["result"]["freeButtons"], json!([16]));

    let keyboard = call(
        &mut engine,
        &mut ui,
        "controller/keyboardStart",
        json!({"vehicleId":"sub", "throttle":0.0}),
    );
    assert!(keyboard.get("result").is_some(), "{keyboard}");
    let route = orders.1.lock().unwrap().last().cloned().unwrap();
    assert_eq!(route.vehicle_id, "sub");
    assert_eq!(route.gripper.as_ref().map(|w| w.output), Some(11));
    let held = call(
        &mut engine,
        &mut ui,
        "gripper/hold",
        json!({"vehicleId":"sub", "action":"open"}),
    );
    assert!(held.get("result").is_some(), "{held}");

    let mut other = connect(&mut engine, "operator-ui");
    let elsewhere = call(
        &mut engine,
        &mut other,
        "gripper/hold",
        json!({"vehicleId":"sub", "action":"close"}),
    );
    assert!(
        elsewhere.get("error").is_some(),
        "another window cannot join this one's route: {elsewhere}"
    );

    orders.1.lock().unwrap().clear();
    let unset = call(&mut engine, &mut ui, "vehicle/gripper", json!({"id":"sub"}));
    assert!(unset["result"].get("gripper").is_none(), "{unset}");
    let rerouted = orders.1.lock().unwrap().last().cloned().unwrap();
    assert!(
        rerouted.gripper.is_none(),
        "removing the gripper reroutes without it"
    );
}

/// Someone at this computer agreed to the Terms of Service (ADR-0047), so a
/// real vehicle arms for a window that acts as no one in particular.
fn agreed(mut state: uxduck_engine::state::State) -> uxduck_engine::state::State {
    let operator = serde_json::from_value(json!({
        "id": "op-agreed",
        "name": "Agreed",
        "units": "metric",
        "coordinateFormat": "decimal",
        "terms": { "version": uxduck_contracts::version::TERMS_VERSION, "acceptedAtMs": 1 },
        "version": 1
    }))
    .unwrap();
    state.operators.insert("op-agreed".into(), operator);
    state
}

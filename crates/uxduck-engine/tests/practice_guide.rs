//! The "Get started" guide (S117): an exercise judged on a simulated
//! vehicle as it is, one per kind of vehicle, and exercises that name the
//! vehicles they fit.

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
use uxduck_core::domain::link::Link;
use uxduck_core::domain::telemetry::{Altitude, AltitudeFrame, PositionEstimate, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem,
    Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, State};

/// 2026-10-03, in microseconds.
const NOW_US: u64 = 1_791_000_000_000_000;
const HOME: (f64, f64) = (-35.363_261, 149.165_23);

#[derive(Clone)]
struct Tick(Arc<AtomicU64>);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Tick {
    fn advance_s(&self, s: u64) {
        self.0.fetch_add(s * 1_000_000, Ordering::SeqCst);
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

/// What the engine gave the data plane.
#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<String>>>);

impl Orders {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl LinkPort for Orders {
    fn open(&mut self, link: &Link) {
        self.0.lock().unwrap().push(format!("open {}", link.id));
    }
    fn close(&mut self, id: &str) {
        self.0.lock().unwrap().push(format!("close {id}"));
    }
    fn command(&mut self, order: CommandOrder) {
        self.0
            .lock()
            .unwrap()
            .push(format!("command {} {:?}", order.vehicle_id, order.command));
    }
    fn ground_heartbeat(&mut self, _: &str, _: bool) {}
    fn route_controller(&mut self, _: &str, _: Option<ControlRoute>) {}
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Internal, Trust::LocalUser, outbox);
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
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    client
}

/// Sends a request; its reply if it came at once (a command is answered
/// when the vehicle acknowledges it).
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

fn link(id: &str, kind: Value) -> Link {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "kind": kind,
        "policy": "read-write",
        "topology": "auto",
        "status": "up",
        "createdAtUs": 1,
        "version": 1,
    }))
    .unwrap()
}

fn simulator(vehicle: &str) -> Value {
    json!({
        "type": "simulator",
        "vehicle": vehicle,
        "latitudeDeg": HOME.0,
        "longitudeDeg": HOME.1,
        "altitudeM": 584.0,
        "headingDeg": 353.0
    })
}

/// The vehicles: `real` on a UDP link, and a simulated `copter`, `vtol`,
/// `rover` and `blimp` (and a `px4` copter, simulated but not ArduPilot).
fn setup() -> (Engine, Client, Orders, Tick) {
    let clock = Tick(Arc::new(AtomicU64::new(NOW_US)));
    let mut world = State::default();
    world.settings.recreate_simulations = true;
    world.links.insert(
        "udp".into(),
        link(
            "udp",
            json!({ "type": "udp-listen", "host": "0.0.0.0", "port": 14550 }),
        ),
    );
    for (id, vehicle) in [
        ("copter-sitl", "multicopter"),
        ("vtol-sitl", "vtol"),
        ("rover-sitl", "rover"),
        ("blimp-sitl", "airship"),
    ] {
        world.links.insert(id.into(), link(id, simulator(vehicle)));
    }
    let mut px4 = simulator("multicopter");
    px4["autopilot"] = "px4".into();
    world.links.insert("px4-sitl".into(), link("px4-sitl", px4));
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
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: world,
            sessions: vec![],
        },
    );
    let ui = connect(&mut engine, "operator-ui");
    for (n, (id, link_id, kind)) in [
        ("real", "udp", "Quadrotor"),
        ("copter", "copter-sitl", "Quadrotor"),
        ("vtol", "vtol-sitl", "VtolQuadrotor"),
        ("rover", "rover-sitl", "GroundRover"),
        ("blimp", "blimp-sitl", "Airship"),
        ("px4", "px4-sitl", "Quadrotor"),
    ]
    .into_iter()
    .enumerate()
    {
        observe(
            &mut engine,
            id,
            link_id,
            u8::try_from(n + 1).unwrap(),
            kind,
            None,
            false,
        );
    }
    orders.take();
    (engine, ui, orders, clock)
}

fn observe(
    engine: &mut Engine,
    id: &str,
    link_id: &str,
    system_id: u8,
    vehicle_type: &str,
    mode: Option<&str>,
    armed: bool,
) {
    let link = engine.state().links.get(link_id).cloned();
    let autopilot = match link.map(|l| serde_json::to_value(&l.kind).unwrap()) {
        Some(kind) if kind["autopilot"] == "px4" => "Px4",
        _ => "Ardupilotmega",
    };
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: vec![],
        link_id: link_id.into(),
        peer: format!("127.0.0.1:576{system_id}"),
        system_id,
        component_ids: vec![1],
        autopilot: autopilot.into(),
        vehicle_type: vehicle_type.into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: engine.state().vehicles.get(id).map_or(5, |_| NOW_US),
        armed,
        mode: mode.map(str::to_owned),
    });
}

/// The vehicle `id` reports a mode and whether it is armed.
fn reports(engine: &mut Engine, id: &str, mode: &str, armed: bool) {
    let vehicle = engine.state().vehicles[id].clone();
    observe(
        engine,
        id,
        &vehicle.link_id,
        vehicle.system_id,
        &vehicle.vehicle_type,
        Some(mode),
        armed,
    );
}

/// The vehicle `id` is this high above home, at a place.
fn at(engine: &mut Engine, id: &str, place: (f64, f64), height_m: f64) {
    engine.telemetry_observed(VehicleTelemetry {
        vehicle_id: id.into(),
        positions: vec![PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: place.0,
            longitude_deg: place.1,
            altitudes: vec![Altitude {
                meters: height_m,
                frame: AltitudeFrame::AboveHome,
            }],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: 1,
        }],
        items: vec![],
    });
}

fn command(engine: &mut Engine, ui: &mut Client, vehicle: &str, command: Value) {
    let answer = send(
        engine,
        ui,
        "command/send",
        json!({ "vehicleId": vehicle, "command": command, "confirm": true }),
    );
    // Sent: answered only once the vehicle acknowledges it.
    assert!(answer.is_none(), "{answer:?}");
}

fn attempt(engine: &Engine, vehicle: &str) -> Value {
    serde_json::to_value(engine.state().practice.get(vehicle).cloned()).unwrap()
}

fn guide(engine: &mut Engine, ui: &mut Client, vehicle: &str) -> Value {
    call(
        engine,
        ui,
        "practice/guide",
        json!({ "vehicleId": vehicle }),
    )
}

#[test]
fn the_guide_for_the_vehicles_kind_starts_on_it_as_it_is_and_is_asked_for_again() {
    let (mut engine, mut ui, orders, _) = setup();
    let link_before = engine.state().links["vtol-sitl"].clone();
    let started = guide(&mut engine, &mut ui, "vtol");
    let result = &started["result"];
    assert_eq!(result["exerciseId"], "guide-vtol", "{started}");
    assert_eq!(result["state"], "running", "no set-up to wait for");
    assert_eq!(result["guide"], true);
    assert_eq!(result["step"], 0);
    // As it is: not started again, nothing sent, the link as it was.
    assert_eq!(engine.state().links["vtol-sitl"], link_before);
    assert!(orders.take().is_empty());

    let again = guide(&mut engine, &mut ui, "vtol");
    assert_eq!(again["result"]["id"], result["id"], "the running guide");

    for (vehicle, guide_id) in [("copter", "guide-multicopter"), ("rover", "guide-rover")] {
        let started = guide(&mut engine, &mut ui, vehicle);
        assert_eq!(started["result"]["exerciseId"], guide_id, "{started}");
    }
    // Never on a real vehicle, nor a simulator UXDuck cannot judge.
    for vehicle in ["real", "px4"] {
        let refused = guide(&mut engine, &mut ui, vehicle);
        assert_eq!(
            refused["error"]["data"]["kind"], "invalid-value",
            "{refused}"
        );
        assert_eq!(
            refused["error"]["message"],
            "Practice needs a simulated vehicle started in UXDuck."
        );
    }
    assert!(!engine.state().practice.contains_key("real"));

    // Given up as an exercise is; asked for again, a new one starts.
    let stopped = call(
        &mut engine,
        &mut ui,
        "practice/stop",
        json!({ "vehicleId": "vtol" }),
    );
    assert_eq!(stopped["result"]["state"], "given-up");
    let next = guide(&mut engine, &mut ui, "vtol");
    assert_eq!(next["result"]["state"], "running");
    assert_ne!(next["result"]["id"], result["id"]);
}

#[test]
fn the_vtol_guide_ticks_each_step_from_what_was_sent_and_done_and_never_fails() {
    let (mut engine, mut ui, _, clock) = setup();
    reports(&mut engine, "vtol", "GUIDED", true);
    at(&mut engine, "vtol", HOME, 100.0);
    // Sent before the guide began: does not count.
    let there = json!({
        "type": "go-to", "latitudeDeg": -35.36, "longitudeDeg": 149.17, "altitudeM": 100.0
    });
    command(&mut engine, &mut ui, "vtol", there.clone());
    clock.advance_s(1);
    guide(&mut engine, &mut ui, "vtol");
    at(&mut engine, "vtol", HOME, 100.0);
    assert_eq!(attempt(&engine, "vtol")["step"], 0);

    clock.advance_s(1);
    command(&mut engine, &mut ui, "vtol", there);
    at(&mut engine, "vtol", HOME, 100.0);
    assert_eq!(attempt(&engine, "vtol")["step"], 1, "Fly here");

    // Lost for a while: a guide waits rather than fails.
    clock.advance_s(1);
    let mut vehicle = engine.state().vehicles["vtol"].clone();
    vehicle.presence = Presence::Offline;
    engine.vehicle_observed(Observation {
        id: vehicle.id.clone(),
        aliases: vec![],
        link_id: vehicle.link_id.clone(),
        peer: "127.0.0.1:5763".into(),
        system_id: vehicle.system_id,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: vehicle.vehicle_type.clone(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Offline,
        last_heartbeat_us: NOW_US,
        armed: true,
        mode: Some("GUIDED".into()),
    });
    assert_eq!(attempt(&engine, "vtol")["state"], "running");

    clock.advance_s(1);
    command(
        &mut engine,
        &mut ui,
        "vtol",
        json!({ "type": "set-mode", "mode": "LOITER" }),
    );
    reports(&mut engine, "vtol", "LOITER", true);
    assert_eq!(attempt(&engine, "vtol")["step"], 2, "Loiter");

    clock.advance_s(1);
    command(&mut engine, &mut ui, "vtol", json!({ "type": "rtl" }));
    reports(&mut engine, "vtol", "RTL", true);
    assert_eq!(attempt(&engine, "vtol")["step"], 3, "Return to launch");

    // Coming down is not on the ground yet.
    clock.advance_s(1);
    command(&mut engine, &mut ui, "vtol", json!({ "type": "land" }));
    reports(&mut engine, "vtol", "QLAND", true);
    at(&mut engine, "vtol", HOME, 20.0);
    assert_eq!(attempt(&engine, "vtol")["state"], "running");
    at(&mut engine, "vtol", HOME, 0.1);
    reports(&mut engine, "vtol", "QLAND", false);
    let done = attempt(&engine, "vtol");
    assert_eq!(done["state"], "passed", "{done}");
    assert_eq!(done["line"], "Done. It's back on the ground.");
    assert_eq!(done["step"], 4);
}

#[test]
fn a_rover_guide_arms_drives_holds_and_comes_home() {
    let (mut engine, mut ui, _, clock) = setup();
    reports(&mut engine, "rover", "MANUAL", false);
    at(&mut engine, "rover", HOME, 0.0);
    guide(&mut engine, &mut ui, "rover");
    let away = (-35.363_261, 149.166_33);
    let there = json!({
        "type": "go-to", "latitudeDeg": away.0, "longitudeDeg": away.1, "altitudeM": 0.0
    });
    let steps = [
        (json!({ "type": "arm" }), "MANUAL", HOME),
        (there, "GUIDED", away),
        (json!({ "type": "set-mode", "mode": "HOLD" }), "HOLD", away),
        (json!({ "type": "rtl" }), "RTL", away),
    ];
    for (n, (sent, mode, place)) in steps.into_iter().enumerate() {
        clock.advance_s(1);
        command(&mut engine, &mut ui, "rover", sent);
        reports(&mut engine, "rover", mode, true);
        at(&mut engine, "rover", place, 0.0);
        assert_eq!(attempt(&engine, "rover")["step"], n + 1, "{mode}");
    }
    clock.advance_s(30);
    at(&mut engine, "rover", HOME, 0.0);
    assert_eq!(attempt(&engine, "rover")["state"], "passed");
}

#[test]
fn another_exercise_ends_a_running_guide_and_the_guide_waits_for_an_exercise() {
    let (mut engine, mut ui, _, _) = setup();
    let subscribed = call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["practice-attempt"] }),
    );
    assert!(subscribed.get("result").is_some(), "{subscribed}");
    let started = guide(&mut engine, &mut ui, "copter");
    assert_eq!(started["result"]["state"], "running");
    let exercise = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "mode-tour", "vehicleId": "copter" }),
    );
    assert_eq!(exercise["result"]["state"], "starting", "{exercise}");
    assert_eq!(exercise["result"]["guide"], Value::Null);
    // One attempt per vehicle: the guide was given up first.
    assert_eq!(attempt(&engine, "copter")["exerciseId"], "mode-tour");
    let pushed: String = ui.queued.iter().map(Value::to_string).collect();
    assert!(
        pushed.contains("Given up: an exercise started."),
        "the guide's end was pushed"
    );
    // While the exercise runs, the guide is refused, plainly.
    let refused = guide(&mut engine, &mut ui, "copter");
    assert_eq!(
        refused["error"]["message"], "This vehicle is practising. Give that up first.",
        "{refused}"
    );
}

#[test]
fn an_exercise_says_which_vehicles_it_fits_and_a_guide_says_it_is_one() {
    let (mut engine, mut ui, _, _) = setup();
    let listed = call(
        &mut engine,
        &mut ui,
        "practice/list",
        json!({ "vehicleId": "vtol" }),
    );
    let exercises = listed["result"]["exercises"].as_array().unwrap();
    let find = |id: &str| exercises.iter().find(|e| e["id"] == id).unwrap();
    assert_eq!(find("mode-tour")["vehicles"], json!(["multicopter"]));
    assert_eq!(
        find("mode-tour")["unavailable"],
        "This exercise is for a simulated multicopter."
    );
    assert_eq!(find("mode-tour-vtol")["vehicles"], json!(["vtol"]));
    assert!(find("mode-tour-vtol").get("unavailable").is_none());
    assert_eq!(find("mode-tour-vtol")["title"], find("mode-tour")["title"]);
    assert_eq!(find("guide-vtol")["guide"], true);
    assert!(find("guide-vtol").get("unavailable").is_none());
    assert_eq!(
        find("guide-multicopter")["unavailable"],
        "This exercise is for a simulated multicopter."
    );
    assert!(find("mode-tour").get("guide").is_none());
    // Every kind of simulated vehicle has its guide.
    for kind in [
        "multicopter",
        "vtol",
        "plane",
        "rover",
        "boat",
        "submarine",
        "airship",
    ] {
        let guides: Vec<_> = exercises
            .iter()
            .filter(|e| {
                e["guide"] == true && e["vehicles"].as_array().unwrap().contains(&json!(kind))
            })
            .collect();
        assert_eq!(guides.len(), 1, "{kind}");
    }

    let refused = call(
        &mut engine,
        &mut ui,
        "practice/start",
        json!({ "exerciseId": "mode-tour", "vehicleId": "vtol" }),
    );
    assert_eq!(
        refused["error"]["message"],
        "This exercise is for a simulated multicopter."
    );
    let blimp = call(
        &mut engine,
        &mut ui,
        "practice/list",
        json!({ "vehicleId": "blimp" }),
    );
    let fits: Vec<&str> = blimp["result"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e.get("unavailable").is_none())
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(fits, ["guide-airship"]);
}

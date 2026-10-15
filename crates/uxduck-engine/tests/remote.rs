//! Control from away (S80, ADR-0003 C9, C10): commands from someone signed
//! in to the same account, through the relay, take the computer's own
//! command path with its rules for remote control.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
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
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Forget, LinkPort, MemoryCredentials, NoAgent, NoSystem,
    Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, State};

/// A clock the test moves: microseconds since the Unix epoch.
#[derive(Clone)]
struct Tick(Arc<std::sync::atomic::AtomicU64>);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 2026-10-03, in microseconds.
const NOW_US: u64 = 1_791_000_000_000_000;

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

/// Commands and routes the engine gave the data plane.
#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<String>>>);

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
    fn ground_heartbeat(&mut self, vehicle_id: &str, on: bool) {
        self.0.lock().unwrap().push(format!(
            "heartbeat {vehicle_id} {}",
            if on { "on" } else { "off" }
        ));
    }
    fn route_controller(&mut self, id: &str, route: Option<ControlRoute>) {
        self.0.lock().unwrap().push(match route {
            Some(route) => format!("route {id} {}", route.vehicle_id),
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

/// Someone away, as the daemon's edge task connects them (S80).
fn remote(engine: &mut Engine, user_id: &str, name: &str) -> Client {
    connect_as(
        engine,
        "remote-operator",
        Trust::Relay {
            user_id: user_id.into(),
            name: name.into(),
        },
    )
}

fn connect_as(engine: &mut Engine, kind: &str, trust: Trust) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Internal, trust, outbox);
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

/// Sends a request; its reply if it came at once. A command that was sent
/// is answered only when the vehicle acknowledges it.
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

/// A real vehicle `real` on a UDP link and a simulated one `sim`.
fn setup(clock: Tick) -> (Engine, Client, Orders) {
    let mut world = State::default();
    // The simulation from the last run starts again with UXDuck.
    world.settings.recreate_simulations = true;
    world.links.insert(
        "udp".into(),
        link(
            "udp",
            json!({ "type": "udp-listen", "host": "0.0.0.0", "port": 14550 }),
        ),
    );
    world.links.insert(
        "sitl".into(),
        link(
            "sitl",
            json!({
                "type": "simulator",
                "vehicle": "multicopter",
                "latitudeDeg": -35.363_261,
                "longitudeDeg": 149.165_23,
                "altitudeM": 584.0,
                "headingDeg": 353.0
            }),
        ),
    );
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
        Box::new(clock),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: world,
            sessions: vec![],
        },
    );
    let ui = connect(&mut engine, "operator-ui");
    observe(&mut engine, "real", "udp", 1, &[]);
    observe(&mut engine, "sim", "sitl", 2, &[]);
    orders.take();
    (engine, ui, orders)
}

/// Heard again, armed.
fn observe_armed(engine: &mut Engine, id: &str) {
    let vehicle = engine.state().vehicles.get(id).cloned().unwrap();
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: vec![],
        link_id: vehicle.link_id.clone(),
        peer: vehicle.peer.clone(),
        system_id: vehicle.system_id,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 6,
        armed: true,
        mode: Some("LOITER".into()),
    });
}

fn observe(engine: &mut Engine, id: &str, link_id: &str, system_id: u8, aliases: &[&str]) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        link_id: link_id.into(),
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

fn engine() -> (Engine, Client, Orders, Tick) {
    let clock = Tick(Arc::new(std::sync::atomic::AtomicU64::new(NOW_US)));
    let (engine, ui, orders) = setup(clock.clone());
    (engine, ui, orders, clock)
}

fn allow(engine: &mut Engine, ui: &mut Client, classes: Value) {
    let state = call(
        engine,
        ui,
        "state/subscribe",
        json!({ "entities": ["settings"] }),
    );
    let version = state["result"]["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    let reply = call(
        engine,
        ui,
        "settings/update",
        json!({ "expectedVersion": version, "remoteControl": classes }),
    );
    assert!(reply.get("result").is_some(), "{reply}");
}

/// A command from away, as the relay passes it on: its id and the relay's time.
fn command(vehicle: &str, command: Value, id: &str, sent_at_ms: u64) -> Value {
    json!({
        "vehicleId": vehicle,
        "command": command,
        "commandId": id,
        "sentAtMs": sent_at_ms,
    })
}

fn kind(reply: &Value) -> &str {
    reply["error"]["data"]["kind"].as_str().unwrap_or_default()
}

const NOW_MS: u64 = NOW_US / 1000;

#[test]
fn nothing_from_away_until_the_computer_allows_it() {
    let (mut engine, mut ui, orders, _) = engine();
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    let rtl = command("sim", json!({ "type": "rtl" }), "c1", NOW_MS);
    let off = send(&mut engine, &mut ana, "command/send", rtl.clone()).unwrap();
    assert_eq!(kind(&off), "capability-denied", "{off}");
    assert!(
        off["error"]["message"]
            .as_str()
            .unwrap()
            .contains("remote control is off")
    );

    // Someone away cannot let themselves in, or do anything but commands.
    let sneaky = send(
        &mut engine,
        &mut ana,
        "settings/update",
        json!({ "expectedVersion": 0, "remoteControl": ["critical"] }),
    )
    .unwrap();
    assert_eq!(kind(&sneaky), "capability-denied", "{sneaky}");
    assert!(
        engine.state().approvals.is_empty(),
        "never an approval for more"
    );
    assert!(engine.state().settings.remote_control.is_empty());

    allow(&mut engine, &mut ui, json!(["safety"]));
    assert_eq!(
        send(&mut engine, &mut ana, "command/send", rtl),
        None,
        "sent"
    );
    assert_eq!(orders.take().len(), 1);
    let receipt = engine.state().receipts.values().last().unwrap().clone();
    let by = receipt.remote.expect("named");
    assert_eq!(
        (by.name.as_str(), by.command_id.as_str()),
        ("ana@example.com", "c1")
    );

    // Mission is not allowed: refused, not asked.
    let loiter = command(
        "sim",
        json!({ "type": "set-mode", "mode": "GUIDED" }),
        "c2",
        NOW_MS,
    );
    let refused = send(&mut engine, &mut ana, "command/send", loiter).unwrap();
    assert_eq!(kind(&refused), "capability-denied", "{refused}");
    assert!(engine.state().approvals.is_empty());
}

#[test]
fn a_command_sent_twice_is_carried_out_once() {
    let (mut engine, mut ui, orders, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    let rtl = command("sim", json!({ "type": "rtl" }), "c1", NOW_MS);
    assert_eq!(
        send(&mut engine, &mut ana, "command/send", rtl.clone()),
        None
    );
    // The network dropped before the answer; the app sends it again.
    let again = send(&mut engine, &mut ana, "command/send", rtl.clone()).unwrap();
    assert_eq!(
        again["result"]["status"], "sent",
        "the first one's receipt, as it is"
    );
    assert_eq!(again["result"]["remote"]["commandId"], "c1");
    assert_eq!(orders.take().len(), 1, "sent to the vehicle once");
    assert_eq!(
        engine.remote_receipt("u-ana", "c1").map(|r| r.id),
        again["result"]["id"].as_str().map(str::to_owned)
    );
    // Someone else's c1 is their own.
    let mut ben = remote(&mut engine, "u-ben", "ben@example.com");
    assert_eq!(send(&mut engine, &mut ben, "command/send", rtl), None);
    assert_eq!(orders.take().len(), 1);
}

#[test]
fn a_command_too_old_when_it_arrives_is_refused() {
    let (mut engine, mut ui, orders, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety", "mission"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    // Return to launch may be 5 s old; this one is 6.
    let late = command("sim", json!({ "type": "rtl" }), "c1", NOW_MS - 6_000);
    let reply = send(&mut engine, &mut ana, "command/send", late).unwrap();
    assert_eq!(reply["result"]["status"], "not-sent", "{reply}");
    assert_eq!(reply["result"]["reason"], "expired");
    assert!(orders.take().is_empty());
    // 4 s is fine.
    let fresh = command("sim", json!({ "type": "rtl" }), "c2", NOW_MS - 4_000);
    assert_eq!(send(&mut engine, &mut ana, "command/send", fresh), None);
}

#[test]
fn mission_commands_need_the_lease_which_the_computer_can_take_back() {
    let (mut engine, mut ui, orders, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety", "mission"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    let mut ben = remote(&mut engine, "u-ben", "ben@example.com");
    let loiter = |id: &str| {
        command(
            "sim",
            json!({ "type": "set-mode", "mode": "LOITER" }),
            id,
            NOW_MS,
        )
    };
    let auto = |id: &str| {
        command(
            "sim",
            json!({ "type": "set-mode", "mode": "AUTO" }),
            id,
            NOW_MS,
        )
    };
    let busy = send(&mut engine, &mut ana, "command/send", auto("c1")).unwrap();
    assert_eq!(kind(&busy), "busy", "{busy}");
    // A safety command never needs it.
    assert_eq!(
        send(&mut engine, &mut ana, "command/send", loiter("c2")),
        None
    );
    orders.take();

    let lease = call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(lease["result"]["name"], "ana@example.com", "{lease}");
    let asked = engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some("lease:sim"))
        .cloned()
        .expect("the computer is told");
    assert_eq!(
        asked.title,
        "ana@example.com has control of Bravo from away"
    );
    assert_eq!(asked.actions[0].label, "Take back");
    let other = call(
        &mut engine,
        &mut ben,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(kind(&other), "busy");
    assert_eq!(
        send(&mut engine, &mut ana, "command/send", auto("c3")),
        None
    );
    assert_eq!(orders.take().len(), 1);
    let theirs = send(&mut engine, &mut ben, "command/send", auto("c4")).unwrap();
    assert_eq!(kind(&theirs), "busy");

    // At the computer, take it back: no asking.
    let back = call(
        &mut engine,
        &mut ui,
        "lease/release",
        json!({ "vehicleId": "sim" }),
    );
    assert!(back.get("result").is_some(), "{back}");
    assert!(engine.lease_of("sim").is_none());
    assert!(
        engine
            .state()
            .notifications
            .values()
            .all(|n| n.condition.as_deref() != Some("lease:sim"))
    );
    let after = send(&mut engine, &mut ana, "command/send", auto("c5")).unwrap();
    assert_eq!(kind(&after), "busy");
}

/// A vehicle's lost ground station failsafe, as if just downloaded.
fn failsafe(engine: &mut Engine, vehicle: &str, fs_gcs_enable: f64) {
    use uxduck_core::domain::parameter::ParamKind;
    use uxduck_engine::transfers::Reported;
    engine.parameters_downloaded(
        vehicle,
        Ok(vec![
            Reported {
                name: "FS_GCS_ENABLE".into(),
                value: fs_gcs_enable,
                kind: ParamKind::Int8,
            },
            Reported {
                name: "FS_GCS_TIMEOUT".into(),
                value: 5.0,
                kind: ParamKind::Real32,
            },
            Reported {
                name: "SYSID_MYGCS".into(),
                value: 255.0,
                kind: ParamKind::Int16,
            },
        ]),
        &std::collections::BTreeMap::new(),
    );
}

#[test]
fn a_vehicle_is_left_with_nobody_at_the_computer_only_with_its_failsafe_on() {
    let (mut engine, mut ui, _, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety", "mission"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    // Someone at the computer: no question.
    let taken = call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "real" }),
    );
    assert!(taken.get("result").is_some(), "{taken}");
    call(
        &mut engine,
        &mut ana,
        "lease/release",
        json!({ "vehicleId": "real" }),
    );
    engine.disconnect(ui.id);
    // Nobody: its failsafe is off, as it ships.
    let alone = call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(kind(&alone), "capability-denied", "{alone}");
    assert!(
        alone["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Set FS_GCS_ENABLE")
    );
    // Set to return to launch: allowed, and the person away is told what happens.
    failsafe(&mut engine, "sim", 1.0);
    let sim = call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    assert_eq!(
        sim["result"]["failsafe"],
        "If you lose connection and nobody is at the ground station, Bravo will return to launch about 15 s later.",
        "{sim}"
    );
    // Gone from away: the lease ends with them.
    engine.disconnect(ana.id);
    assert!(engine.lease_of("sim").is_none());
}

#[test]
fn nobody_supervising_stops_the_heartbeat_and_someone_back_resumes_it() {
    let (mut engine, ui, orders, clock) = engine();
    let at = |s: u64| NOW_US + s * 1_000_000;
    // Armed and flying: the computer will say so.
    observe_armed(&mut engine, "sim");
    failsafe(&mut engine, "sim", 1.0);
    engine.supervise(at(0));
    assert!(
        orders.take().iter().all(|o| !o.starts_with("heartbeat")),
        "supervised"
    );

    // The last window closes.
    engine.disconnect(ui.id);
    engine.supervise(at(1));
    engine.supervise(at(10));
    assert!(!engine.heartbeat_withheld("sim"), "within the grace period");
    engine.supervise(at(11));
    assert!(engine.heartbeat_withheld("sim"));
    let stopped = orders.take();
    assert!(
        stopped.contains(&"heartbeat sim off".to_owned()),
        "{stopped:?}"
    );
    let told = engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some("supervision:sim"))
        .cloned()
        .expect("said at the computer");
    assert!(
        told.title
            .contains("stopped its ground station heartbeat to Bravo")
    );
    assert!(
        told.body
            .unwrap()
            .contains("will return to launch about 15 s later")
    );
    assert!(
        engine
            .state()
            .history
            .values()
            .any(|e| format!("{:?}", e.kind).contains("SupervisionLost"))
    );
    // Disarmed and on the ground: withheld quietly.
    assert!(stopped.contains(&"heartbeat real off".to_owned()));
    assert!(
        engine
            .state()
            .notifications
            .values()
            .all(|n| n.condition.as_deref() != Some("supervision:real"))
    );

    // Someone opens a window: the heartbeat resumes.
    let mut back = connect(&mut engine, "operator-ui");
    let _ = &mut back;
    engine.supervise(at(12));
    let resumed = orders.take();
    assert!(
        resumed.contains(&"heartbeat sim on".to_owned()),
        "{resumed:?}"
    );
    assert!(!engine.heartbeat_withheld("sim"));
    assert!(
        engine
            .state()
            .notifications
            .values()
            .all(|n| n.condition.as_deref() != Some("supervision:sim"))
    );
    let _ = clock;
}

#[test]
fn a_lease_holder_away_supervises_the_vehicle() {
    let (mut engine, mut ui, orders, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety", "mission"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    engine.disconnect(ui.id);
    for s in 0..30 {
        engine.supervise(NOW_US + s * 1_000_000);
    }
    assert!(!engine.heartbeat_withheld("sim"), "held from away");
    assert!(engine.heartbeat_withheld("real"), "nobody holds this one");
    // Their network drops: the lease ends, and the grace period starts.
    engine.disconnect(ana.id);
    engine.supervise(NOW_US + 31 * 1_000_000);
    engine.supervise(NOW_US + 42 * 1_000_000);
    assert!(engine.heartbeat_withheld("sim"));
    assert!(orders.take().contains(&"heartbeat sim off".to_owned()));
}

#[test]
fn critical_commands_ask_the_computer_unless_allowed() {
    let (mut engine, mut ui, orders, _) = engine();
    allow(&mut engine, &mut ui, json!(["safety", "mission"]));
    let mut ana = remote(&mut engine, "u-ana", "ana@example.com");
    call(
        &mut engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "sim" }),
    );
    let arm = command("sim", json!({ "type": "arm" }), "c1", NOW_MS);
    assert_eq!(
        send(&mut engine, &mut ana, "command/send", arm),
        None,
        "held"
    );
    assert!(orders.take().is_empty());
    let approval = engine
        .state()
        .approvals
        .values()
        .next()
        .cloned()
        .expect("asked");
    assert!(
        approval.summary.contains("arm Bravo"),
        "{}",
        approval.summary
    );
    let reply = call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(orders.take().len(), 1, "sent once allowed");
    // "Always" does not stop the next one asking: only the setting does.
    let again = command("sim", json!({ "type": "arm" }), "c2", NOW_MS);
    assert_eq!(send(&mut engine, &mut ana, "command/send", again), None);
    assert!(orders.take().is_empty());
    assert_eq!(
        engine
            .state()
            .approvals
            .values()
            .filter(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
            .count(),
        1
    );

    allow(
        &mut engine,
        &mut ui,
        json!(["safety", "mission", "critical"]),
    );
    let allowed = command("sim", json!({ "type": "arm" }), "c3", NOW_MS);
    assert_eq!(send(&mut engine, &mut ana, "command/send", allowed), None);
    assert_eq!(orders.take().len(), 1, "allowed: sent without asking");
}

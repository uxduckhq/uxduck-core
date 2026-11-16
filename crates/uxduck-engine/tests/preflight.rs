//! Preflight checks and the gate on the launch (S192, S352, ADR-0038 K1 to
//! K6 and K22 to K25): one list per vehicle, the vehicle's own reasons
//! first; UXDuck's own checks, each on or off with its severity; arming
//! never held by checks; the gate in the daemon on a take-off and a
//! mission's start, for every client (window, CLI, agent and MCP,
//! plugins), until the vehicle has launched, warnings never stopping it
//! and a check set to hold that is unknown or waiting counting as
//! failing; the operator's override naming the checks, refused for anyone
//! else; an agent's or a plugin's launch put to an operator as an
//! approval that lists them.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{Capability, DaemonCapabilities};
use uxduck_contracts::dto::plugin::{
    CommandCall, Description, Effects, PluginManifest, SendCommand, SendMessage, Subscriptions,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::command::{Command, ReceiptStatus, RefusalReason};
use uxduck_core::domain::link::Link;
use uxduck_core::domain::mission::MissionItem;
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::plan::PlanUpload;
use uxduck_core::domain::preflight::{CheckStatus, PreflightOverride};
use uxduck_core::domain::telemetry::{PositionEstimate, TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::commands::Outcome;
use uxduck_engine::runtime::{
    Clock, CommandOrder, LinkPort, MemoryCredentials, MessageOrder, NoAgent, NoSystem, Persistence,
    PluginLoad, PluginPort, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::transfers::Reported;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};
use uxduck_mavlink::session::MagCal;

const S: u64 = 1_000_000;

#[derive(Clone, Default)]
struct Time(Arc<AtomicU64>);

impl Time {
    fn advance(&self, us: u64) {
        self.0.fetch_add(us, Ordering::SeqCst);
    }
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Clock for Time {
    fn now_us(&self) -> u64 {
        self.now()
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

/// The commands and raw messages the engine sent, and the plugins it ran.
#[derive(Clone, Default)]
struct Wire {
    commands: Arc<Mutex<Vec<CommandOrder>>>,
    messages: Arc<Mutex<Vec<MessageOrder>>>,
}

impl Wire {
    fn take(&self) -> Vec<CommandOrder> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }
    fn messages(&self) -> Vec<MessageOrder> {
        std::mem::take(&mut *self.messages.lock().unwrap())
    }
}

impl LinkPort for Wire {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.commands.lock().unwrap().push(order);
    }
    fn message(&mut self, order: MessageOrder) {
        self.messages.lock().unwrap().push(order);
    }
}

/// A plugin that may control, and send `COMMAND_LONG` itself.
impl PluginPort for Wire {
    fn inspect(&self, _: &str) -> Result<PluginManifest, String> {
        Ok(PluginManifest {
            abi: 2,
            id: "launcher".into(),
            name: "Launcher".into(),
            version: "0.1.0".into(),
            description: None,
            wasm: "launcher.wasm".into(),
            script: None,
            capabilities: vec![Capability::Read, Capability::Control],
            subscribe: Subscriptions::default(),
            sends: vec!["COMMAND_LONG".into()],
            commands: Vec::new(),
            written_by: None,
            dialect: None,
            hosts: Vec::new(),
            traffic_source: false,
            starts_off: false,
            checks: Vec::new(),
        })
    }
    fn load(&mut self, _: PluginLoad) {}
    fn unload(&mut self, _: &str) {}
    fn command(&mut self, _: &str, _: CommandCall) {}
}

/// The journal, as plain events.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<Event>>>);

impl Persistence for Journal {
    fn append(&mut self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

impl Journal {
    fn preflight_updates(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e.change, Change::PreflightUpdated { .. }))
            .count()
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
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
    send(engine, client, method, params).unwrap_or_else(|| panic!("no reply to {method}"))
}

/// A reply that came later, once the vehicle answered.
fn later(client: &mut Client) -> Option<Value> {
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        client.queued.push_back(frame);
    }
    let at = client.queued.iter().position(|f| f.get("id").is_some())?;
    client.queued.remove(at)
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
    client.queued.clear();
    client
}

struct Bench {
    engine: Engine,
    ui: Client,
    wire: Wire,
    journal: Journal,
    time: Time,
    link_id: String,
}

fn engine_with(state: State, wire: &Wire, journal: &Journal, time: &Time) -> Engine {
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
        persistence: Box::new(journal.clone()),
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(wire.clone()),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(wire.clone()),
    };
    Engine::new(
        config,
        Box::new(time.clone()),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state: agreed(state),
            sessions: vec![],
        },
    )
}

fn bench() -> Bench {
    let wire = Wire::default();
    let journal = Journal::default();
    let time = Time::default();
    time.advance(1_000);
    let mut engine = engine_with(State::default(), &wire, &journal, &time);
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let mut bench = Bench {
        engine,
        ui,
        wire,
        journal,
        time,
        link_id,
    };
    bench.seen(false);
    bench.wire.take();
    bench
}

impl Bench {
    fn seen(&mut self, armed: bool) {
        self.seen_as("Quadrotor", "GUIDED", armed, "Ardupilotmega");
    }

    fn seen_as(&mut self, vehicle_type: &str, mode: &str, armed: bool, autopilot: &str) {
        self.engine.vehicle_observed(Observation {
            connections: None,
            id: "v".into(),
            aliases: Vec::new(),
            link_id: self.link_id.clone(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: autopilot.into(),
            vehicle_type: vehicle_type.into(),
            uid: None,
            firmware: Some("4.5.1".into()),
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: self.time.now(),
            armed,
            mode: Some(mode.into()),
        });
    }

    /// A quadplane, as `ArduPlane` reports one: fixed wing, with `Q_ENABLE`.
    fn quadplane_in(&mut self, mode: &str) {
        self.seen_as("FixedWing", mode, false, "Ardupilotmega");
        self.engine.parameter_reported(
            "v",
            Reported {
                name: "Q_ENABLE".into(),
                value: 1.0,
                kind: ParamKind::Int8,
            },
            false,
        );
    }

    /// Sends Take off as `client`, the vehicle accepting each step: the
    /// commands that reached the wire and the request's answer.
    fn take_off(&mut self, client: Option<&mut Client>, params: Value) -> (Vec<Command>, Value) {
        let mut params = params;
        params["vehicleId"] = json!("v");
        params["command"] =
            json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] });
        let client = client.unwrap_or(&mut self.ui);
        if let Some(reply) = send(&mut self.engine, client, "command/send", params) {
            return (Vec::new(), reply);
        }
        let mut steps = Vec::new();
        loop {
            let sent = self.wire.take();
            assert_eq!(sent.len(), 1, "one command at a time: {sent:?}");
            steps.push(sent[0].command.clone());
            self.engine
                .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
            if let Some(reply) = later(client) {
                return (steps, reply);
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        call(&mut self.engine, &mut self.ui, method, params)
    }

    fn checks(&mut self) -> Value {
        let state = self.call("preflight/state", json!({ "vehicleId": "v" }));
        assert!(state.get("error").is_none(), "{state}");
        state["result"].clone()
    }

    fn check(&mut self, id: &str) -> Value {
        let checks = self.checks();
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} in {checks}"))
    }

    fn configure(&mut self, params: Value) -> Value {
        let mut params = params;
        params["vehicleId"] = json!("v");
        let reply = self.call("preflight/configure", params);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    /// A plan of the vehicle's type, by id.
    fn plan(&mut self, name: &str) -> String {
        let plan = self.call("plan/create", json!({ "vehicleType": "multicopter" }));
        let id = plan["result"]["id"].as_str().unwrap().to_owned();
        let mut body = plan["result"].clone();
        body["name"] = json!(name);
        let updated = self.call(
            "plan/update",
            json!({ "id": id, "expectedVersion": plan["result"]["version"], "body": body }),
        );
        assert!(updated.get("error").is_none(), "{updated}");
        id
    }

    /// The plan check made blocking, with this plan chosen for the flight
    /// though it was never uploaded: one blocking check fails.
    fn wrong_plan(&mut self) {
        let plan = self.plan("Ridge survey");
        self.configure(json!({ "check": "plan", "severity": "block", "planId": plan }));
    }

    /// Sends a command as `client`: its receipt once the vehicle answers
    /// with `outcome` if it was sent, or at once if it was not.
    fn command(&mut self, client: Option<&mut Client>, params: Value) -> Value {
        let mut params = params;
        params["vehicleId"] = json!("v");
        let client = client.unwrap_or(&mut self.ui);
        if let Some(reply) = send(&mut self.engine, client, "command/send", params) {
            return reply;
        }
        let sent = self.wire.take();
        assert_eq!(sent.len(), 1, "one command went: {sent:?}");
        self.engine
            .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
        later(client).expect("its receipt")
    }

    fn arm(&mut self, client: Option<&mut Client>) -> Value {
        self.command(client, json!({ "command": { "type": "arm" } }))
    }

    /// Take off as `client`: refused at once, or the take-off's receipt
    /// once the vehicle has accepted each step.
    fn launch(&mut self, client: Option<&mut Client>) -> Value {
        self.take_off(client, json!({})).1
    }

    /// What the autopilot says of being on the ground or in the air.
    fn flight_state(&mut self, state: &str) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items: vec![TelemetryItem {
                key: "flight.state".into(),
                label: "Flight".into(),
                unit: Unit::Count,
                value: None,
                text: Some(state.into()),
                source: "EXTENDED_SYS_STATE".into(),
                component_id: 1,
                time_us: self.time.now(),
            }],
        });
    }

    /// The engine's state rebuilt from what it holds, with a change: as a
    /// restart of the daemon would find it.
    fn restart_with(&mut self, change: impl FnOnce(&mut State)) {
        let mut state = self.engine.state().clone();
        change(&mut state);
        self.engine = engine_with(state, &self.wire, &self.journal, &self.time);
        self.ui = connect(&mut self.engine, "operator-ui");
        self.seen(false);
        self.wire.take();
    }

    fn telemetry(&mut self, key: &str, value: f64) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items: vec![TelemetryItem {
                key: key.into(),
                label: key.into(),
                unit: Unit::Count,
                value: Some(value),
                text: None,
                source: "TEST".into(),
                component_id: 1,
                time_us: self.time.now(),
            }],
        });
    }
}

impl Bench {
    /// Where the autopilot says the vehicle is.
    fn at(&mut self, latitude_deg: f64, longitude_deg: f64) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: vec![PositionEstimate {
                source: "fused".into(),
                component_id: 1,
                latitude_deg,
                longitude_deg,
                altitudes: Vec::new(),
                heading_deg: None,
                course_deg: None,
                ground_speed_mps: None,
                climb_mps: None,
                horizontal_accuracy_m: None,
                vertical_accuracy_m: None,
                time_us: self.time.now(),
            }],
            items: Vec::new(),
        });
    }

    fn home(&mut self, latitude_deg: f64, longitude_deg: f64) {
        self.telemetry("home.latitude_deg", latitude_deg);
        self.telemetry("home.longitude_deg", longitude_deg);
    }

    /// What the vehicle says of its RC input, as `rc.status`.
    fn rc(&mut self, text: &str) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items: vec![TelemetryItem {
                key: "rc.status".into(),
                label: "RC receiver".into(),
                unit: Unit::None,
                value: None,
                text: Some(text.into()),
                source: "rc".into(),
                component_id: 1,
                time_us: self.time.now(),
            }],
        });
    }

    fn ekf(&mut self, text: &str) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items: vec![TelemetryItem {
                key: "ekf.status".into(),
                label: "EKF".into(),
                unit: Unit::Count,
                value: None,
                text: Some(text.into()),
                source: "EKF_STATUS_REPORT".into(),
                component_id: 1,
                time_us: self.time.now(),
            }],
        });
    }
}

#[test]
fn a_simulations_take_off_waits_until_it_knows_where_it_is() {
    let mut b = bench();
    b.telemetry("arming.ready", 1.0);
    b.ekf("No position");
    // A vehicle of the person's own decides for itself: taking off in a
    // stabilised mode without a position is theirs to choose.
    assert!(gating(&b.checks()).is_empty());

    // A simulation says it is ready some 30 s before it knows where it is,
    // when GUIDED, AUTO and Take off are still refused: its launch waits.
    b.restart_with(|state| state.vehicles.get_mut("v").unwrap().simulated = Some(true));
    b.telemetry("arming.ready", 1.0);
    b.ekf("No position");
    let check = b.check("uxduck:simulation-position");
    assert_eq!(check["status"], "waiting");
    assert_eq!(check["severity"], "block");
    assert_eq!(
        check["detail"],
        "Alpha is still starting: it can take off once it knows where it is, in a few seconds."
    );
    assert_eq!(gating(&b.checks()), ["uxduck:simulation-position"]);
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");
    // Arming is never held (K22).
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");

    b.ekf("Good");
    assert_eq!(b.check("uxduck:simulation-position")["status"], "pass");
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
}

#[test]
fn a_simulation_that_has_not_said_where_it_is_waits_to_take_off() {
    let mut b = bench();
    b.restart_with(|state| state.vehicles.get_mut("v").unwrap().simulated = Some(true));
    b.telemetry("arming.ready", 1.0);
    // No EKF status yet: not knowing is no pass.
    let check = b.check("uxduck:simulation-position");
    assert_eq!(check["status"], "waiting");
    assert_eq!(check["severity"], "block");
    assert_eq!(
        check["detail"],
        "Alpha hasn't said yet whether it knows where it is."
    );
    assert_eq!(gating(&b.checks()), ["uxduck:simulation-position"]);
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["status"], "not-sent", "{receipt}");
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");
}

#[test]
fn a_simulations_ekf_status_counts_for_5_s() {
    let mut b = bench();
    b.restart_with(|state| state.vehicles.get_mut("v").unwrap().simulated = Some(true));
    b.telemetry("arming.ready", 1.0);
    b.ekf("Good");
    // Its EKF says nothing more, while all else is heard.
    b.time.advance(6 * S);
    b.seen(false);
    b.telemetry("arming.ready", 1.0);
    let check = b.check("uxduck:simulation-position");
    assert_eq!(check["status"], "waiting");
    assert_eq!(
        check["detail"],
        "Alpha hasn't said for over 5 s whether it knows where it is."
    );
    assert_eq!(gating(&b.checks()), ["uxduck:simulation-position"]);
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["status"], "not-sent", "{receipt}");
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");

    // Good again, and 5 s later still so.
    b.ekf("Good");
    b.time.advance(5 * S);
    b.seen(false);
    b.telemetry("arming.ready", 1.0);
    let check = b.check("uxduck:simulation-position");
    assert_eq!(check["status"], "pass");
    assert_eq!(check["detail"], "Alpha knows where it is.");
    assert!(gating(&b.checks()).is_empty());
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
}

#[test]
fn a_simulated_px4_vehicle_arms_without_an_ekf_status() {
    let mut b = bench();
    b.restart_with(|state| state.vehicles.get_mut("v").unwrap().simulated = Some(true));
    // PX4 sends no EKF_STATUS_REPORT: there is never one to wait for.
    b.seen_as("Quadrotor", "GUIDED", false, "Px4");
    b.telemetry("arming.ready", 1.0);
    let checks = b.checks();
    assert!(
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] != "uxduck:simulation-position"),
        "{checks}"
    );
    assert!(gating(&checks).is_empty(), "{checks}");
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
}

#[test]
fn px4s_report_says_what_holds_the_mode_it_is_in() {
    use uxduck_mavlink::px4_events::{ArmingChecks, Check};
    let mut b = bench();
    b.seen_as("Quadrotor", "Takeoff", false, "Px4");
    let sticks = Check {
        text: "No manual control input".into(),
        description: "Connect and enable stick input or use autonomous mode.".into(),
        severity: 4,
        modes: u32::MAX,
        holds: false,
    };
    let position = Check {
        text: "Navigation error: No valid position estimate".into(),
        description: String::new(),
        severity: 3,
        modes: 8_257_660,
        holds: true,
    };
    b.engine.arming_checks(
        "v",
        ArmingChecks {
            mode: Some("Takeoff".into()),
            can_arm: Some(false),
            checks: vec![position, sticks.clone()],
        },
    );
    let held = b.check("vehicle:Navigation error: No valid position estimate");
    assert_eq!(held["status"], "fail", "{held}");
    assert_eq!(held["severity"], "block", "{held}");
    assert_eq!(
        held["detail"], "Alpha says it can't arm in Takeoff until this clears.",
        "{held}"
    );
    let warned = b.check("vehicle:No manual control input");
    assert_eq!(warned["severity"], "warn", "{warned}");
    assert_eq!(
        warned["detail"],
        "Alpha warns about this. Connect and enable stick input or use autonomous mode.",
        "{warned}"
    );
    assert!(
        b.checks()["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] != "vehicle:ready"),
        "what holds it says why"
    );
    assert!(
        gating(&b.checks()).is_empty(),
        "the vehicle refuses by itself (K5)"
    );

    // Held by what it only warns about: it says it can't, and why it may be.
    b.engine.arming_checks(
        "v",
        ArmingChecks {
            mode: Some("Position".into()),
            can_arm: Some(false),
            checks: vec![sticks.clone()],
        },
    );
    let ready = b.check("vehicle:ready");
    assert_eq!(ready["status"], "fail", "{ready}");
    assert_eq!(
        ready["detail"], "It says it can't arm in Position yet.",
        "{ready}"
    );
    assert_eq!(
        b.check("vehicle:No manual control input")["severity"],
        "warn"
    );

    b.engine.arming_checks(
        "v",
        ArmingChecks {
            mode: Some("Hold".into()),
            can_arm: Some(true),
            checks: vec![sticks],
        },
    );
    let ready = b.check("vehicle:ready");
    assert_eq!(ready["status"], "pass", "{ready}");
    assert_eq!(ready["detail"], "It says it can arm in Hold.", "{ready}");
    let warned = b.check("vehicle:No manual control input");
    assert_eq!(warned["severity"], "warn", "{warned}");
}

fn gating(checks: &Value) -> Vec<String> {
    checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["gates"] == true)
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_fresh_vehicle_lists_its_own_checks_first_and_every_uxduck_check_on() {
    let mut b = bench();
    let checks = b.checks();
    let ids: Vec<&str> = checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "vehicle:ready",
            "uxduck:setup",
            "uxduck:restart",
            "uxduck:link",
            "uxduck:rc-receiver",
            "uxduck:controller",
            "uxduck:plan",
            "uxduck:terrain",
            "uxduck:recording",
            "uxduck:home"
        ]
    );
    // The vehicle has not said whether it is ready: its row says so, and
    // never holds anything in UXDuck (K5).
    assert_eq!(checks["checks"][0]["source"], "vehicle");
    assert_eq!(checks["checks"][0]["status"], "unknown");
    assert_eq!(checks["checks"][0]["gates"], false);
    // Defaults (K25): every one only warns until a person sets it to hold
    // the launch. Everything passes now.
    let severities: Vec<(&str, &str)> = checks["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["check"].as_str().unwrap(),
                s["severity"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        severities,
        [
            ("setup", "warn"),
            ("restart", "warn"),
            ("link", "warn"),
            ("rc-receiver", "warn"),
            ("controller", "warn"),
            ("plan", "warn"),
            ("terrain", "warn"),
            ("recording", "warn"),
            ("home", "warn"),
            ("preflight-finished", "warn"),
            ("battery-warning", "warn")
        ]
    );
    // "Preflight finished" is the one a person turns on (K29): until then it
    // has a setting and no row.
    assert_eq!(checks["settings"][9]["on"], false);
    // Everything passes now, but the RC receiver and Home: the vehicle has
    // not said whether it has RC input, nor where it and its home are.
    for check in &checks["checks"].as_array().unwrap()[1..] {
        let expected = if check["id"] == "uxduck:rc-receiver" || check["id"] == "uxduck:home" {
            "unknown"
        } else {
            "pass"
        };
        assert_eq!(check["status"], expected, "{check}");
    }
    assert_eq!(checks["evaluating"], true);
    assert!(gating(&checks).is_empty());
    assert_eq!(checks["settings"][2]["threshold"]["value"], 30.0);
    // Ready, and it says so: arming goes out.
    b.telemetry("arming.ready", 1.0);
    assert_eq!(b.check("vehicle:ready")["status"], "pass");
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert!(receipt["result"].get("preflight").is_none());
}

#[test]
fn home_is_where_the_vehicle_is_counts_at_the_launch_and_offers_to_set_it() {
    let mut b = bench();
    // No home yet: not known, at the launch only, and a warning (K25, K27).
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "unknown", "{check}");
    assert_eq!(check["when"], "launch");
    assert_eq!(check["severity"], "warn");
    assert_eq!(check["group"], "Home");
    assert_eq!(
        check["detail"],
        "Alpha has no home yet. It sets one once it knows where it is."
    );
    assert!(check.get("fix").is_none(), "{check}");
    let setting = &b.checks()["settings"][8];
    assert_eq!(setting["when"], "launch", "{setting}");
    assert_eq!(setting["threshold"]["value"], 50.0);
    assert_eq!(setting["threshold"]["unit"], "m");
    // A home and no position: still not known.
    b.home(-35.0, 149.0);
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "unknown", "{check}");
    assert_eq!(check["detail"], "Alpha hasn't said where it is.");
    // About 9 m away: where it is.
    b.at(-35.0, 149.0001);
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "pass", "{check}");
    assert_eq!(check["detail"], "Home is within 50 m of Alpha.");
    // Carried about 110 m while disarmed: most autopilots move home when
    // they arm, so it is no warning yet, and says why (maintainer,
    // 2026-11-13).
    b.at(-35.001, 149.0);
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "pass", "{check}");
    assert_eq!(
        check["detail"],
        "Home is more than 50 m from Alpha. Most autopilots move it here when they arm, so UXDuck reads it again once Alpha is armed."
    );
    assert!(check.get("fix").is_none(), "{check}");
    // Set to hold, a far home holds neither arming nor a launch from
    // disarmed.
    b.configure(json!({ "check": "home", "severity": "block" }));
    assert_eq!(b.check("uxduck:home")["gates"], false);
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    let disarm = b.command(None, json!({ "command": { "type": "disarm" } }));
    assert_eq!(disarm["result"]["status"], "accepted", "{disarm}");
    let receipt = b.launch(None);
    assert!(receipt["result"].get("preflight").is_none(), "{receipt}");
    // Armed on the ground and home has not moved: it stays there, a
    // return would go back to it, and that holds the launch.
    b.seen(true);
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "fail", "{check}");
    assert_eq!(
        check["detail"],
        "Home is more than 50 m from Alpha. A return goes to home."
    );
    assert_eq!(check["fix"]["type"], "home");
    assert_eq!(check["gates"], true);
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    assert_eq!(receipt["result"]["preflight"][0]["id"], "uxduck:home");
    // Only warning, it holds nothing.
    b.configure(json!({ "check": "home", "severity": "warn" }));
    assert_eq!(b.check("uxduck:home")["gates"], false);
    // The distance is the person's to set.
    b.configure(json!({ "check": "home", "threshold": 200.0 }));
    let check = b.check("uxduck:home");
    assert_eq!(check["status"], "pass", "{check}");
    assert_eq!(check["detail"], "Home is within 200 m of Alpha.");
    // Home moved to the vehicle, as Set home here does.
    b.configure(json!({ "check": "home", "threshold": null }));
    assert_eq!(b.check("uxduck:home")["status"], "fail");
    b.home(-35.001, 149.0);
    assert_eq!(b.check("uxduck:home")["status"], "pass");
}

#[test]
fn warnings_never_stop_arming_or_a_launch() {
    let mut b = bench();
    let plan = b.plan("Ridge survey");
    b.configure(json!({ "planId": plan }));
    let check = b.check("uxduck:plan");
    assert_eq!(check["status"], "fail");
    assert_eq!(check["severity"], "warn");
    assert_eq!(check["gates"], false);
    assert_eq!(
        check["detail"],
        "Ridge survey is chosen, but it isn't on Alpha yet."
    );
    assert_eq!(check["fix"], json!({ "type": "plan", "planId": plan }));
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    let receipt = b.launch(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert!(receipt["result"].get("preflight").is_none(), "{receipt}");
}

#[test]
fn a_check_set_to_hold_refuses_a_launch_from_the_window_and_the_cli_and_never_arming() {
    let mut b = bench();
    b.wrong_plan();
    let checks = b.checks();
    assert_eq!(gating(&checks), ["uxduck:plan"]);

    // Arming is never held by checks (K22), however it is asked: from the
    // window, from the CLI, by number, forced past the vehicle's own
    // checks, or by a mode with the armed flag.
    let receipt = b.arm(None);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert!(receipt["result"].get("preflight").is_none(), "{receipt}");
    let mut cli = connect(&mut b.engine, "operator-cli");
    let receipt = b.arm(Some(&mut cli));
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    for command in [
        json!({ "type": "long", "command": 400, "params": [1, 0] }),
        json!({ "type": "long", "command": 176, "params": [217, 4] }),
    ] {
        let receipt = b.command(None, json!({ "command": command }));
        assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    }
    let forced = b.command(None, json!({ "command": { "type": "arm" }, "force": true }));
    assert_eq!(forced["result"]["status"], "accepted", "{forced}");

    // A launch is held (K23): a take-off.
    let receipt = b.launch(None);
    let result = &receipt["result"];
    assert_eq!(result["status"], "not-sent", "{receipt}");
    assert_eq!(result["reason"], "preflight");
    assert_eq!(result["preflight"][0]["id"], "uxduck:plan");
    assert_eq!(
        result["detail"],
        "Take off wasn't sent to Alpha: 1 check that holds the launch doesn't pass: Plan on \
         the vehicle (Ridge survey is chosen, but it isn't on Alpha yet). Fix it, or hold Take \
         off anyway to launch past it."
    );
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");
    // The CLI, an operator too, the same.
    let receipt = b.launch(Some(&mut cli));
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    // A mission's start, from a step too, whose first command only makes
    // the step current. Forcing passes the vehicle's checks, not UXDuck's.
    for command in [
        json!({ "type": "long", "command": 300, "params": [0, 0] }),
        json!({ "type": "long", "command": 300, "params": [3, 0] }),
    ] {
        let receipt = b.command(None, json!({ "command": command }));
        assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    }
    let forced = b.command(None, json!({ "command": json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }), "force": true }));
    assert_eq!(forced["result"]["reason"], "preflight", "{forced}");
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");
    // The rest goes: a mode, disarming.
    let mode = b.command(
        None,
        json!({ "command": { "type": "set-mode", "mode": "LOITER" } }),
    );
    assert_eq!(mode["result"]["status"], "accepted", "{mode}");
    let disarm = b.command(None, json!({ "command": { "type": "disarm" } }));
    assert_eq!(disarm["result"]["status"], "accepted", "{disarm}");

    // Armed and still on the ground, a take-off is a launch all the same.
    b.seen(true);
    b.flight_state("On the ground");
    assert_eq!(b.checks()["evaluating"], true, "read live on the ground");
    let take_off = b.command(None, json!({ "command": json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }) }));
    assert_eq!(take_off["result"]["reason"], "preflight", "{take_off}");
    // In the air nothing is held: a take-off, a mission's start.
    b.flight_state("In the air");
    assert_eq!(b.checks()["evaluating"], false);
    for command in [
        json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }),
        json!({ "type": "long", "command": 300, "params": [0, 0] }),
    ] {
        let receipt = b.command(None, json!({ "command": command }));
        assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
        assert!(receipt["result"].get("preflight").is_none(), "{receipt}");
    }
}

#[test]
fn an_operator_takes_off_anyway_naming_every_failing_check_and_the_journal_keeps_it() {
    let mut b = bench();
    b.wrong_plan();
    // Naming another check, or none, is no override.
    let (steps, wrong) = b.take_off(None, json!({ "overridePreflight": ["uxduck:terrain"] }));
    assert_eq!(wrong["result"]["reason"], "preflight", "{wrong}");
    assert!(steps.is_empty() && b.wire.take().is_empty());
    // Held before its first step, which is arming here: receipted as the
    // take-off asked for, the command a person then holds to send anyway.
    assert_eq!(wrong["result"]["command"]["command"], 22, "{wrong}");
    // Arming needs no override, and records none though one is named.
    let armed = b.command(
        None,
        json!({ "command": { "type": "arm" }, "overridePreflight": ["uxduck:plan"] }),
    );
    assert_eq!(armed["result"]["status"], "accepted", "{armed}");
    assert!(
        armed["result"].get("preflightOverride").is_none(),
        "{armed}"
    );

    let (steps, receipt) = b.take_off(None, json!({ "overridePreflight": ["uxduck:plan"] }));
    assert_eq!(steps, [Command::Arm, Command::take_off(10.0)]);
    let result = &receipt["result"];
    assert_eq!(result["status"], "accepted", "{receipt}");
    assert_eq!(result["preflightOverride"], json!({ "by": "operator" }));
    assert_eq!(result["preflight"][0]["id"], "uxduck:plan");
    assert_eq!(result["preflight"][0]["status"], "fail");
    // The journal has each step's receipt with the override and the checks.
    let journaled: Vec<_> = b
        .journal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.change {
            Change::ReceiptUpserted { receipt } => Some(receipt.clone()),
            _ => None,
        })
        .filter(|r| r.preflight_override.is_some())
        .collect();
    assert!(journaled.iter().any(|r| r.command == Command::Arm));
    assert!(journaled.iter().any(|r| r.command.is_take_off()));
    for receipt in &journaled {
        assert_eq!(
            receipt.preflight_override,
            Some(PreflightOverride::Operator)
        );
        assert_eq!(receipt.preflight[0].id, "uxduck:plan");
    }
}

#[test]
fn an_agents_launch_waits_for_an_operator_who_sees_the_checks() {
    let mut b = bench();
    b.wrong_plan();
    // MCP clients are agents (`uxduck mcp`): they read the same list.
    let mut agent = connect(&mut b.engine, "agent");
    let read = call(
        &mut b.engine,
        &mut agent,
        "preflight/state",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(gating(&read["result"]), ["uxduck:plan"], "{read}");
    // It cannot change them.
    let change = call(
        &mut b.engine,
        &mut agent,
        "preflight/configure",
        json!({ "vehicleId": "v", "check": "plan", "on": false }),
    );
    assert_eq!(
        change["error"]["data"]["kind"], "capability-denied",
        "{change}"
    );
    let take_off = || json!({ "vehicleId": "v", "command": json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }) });

    // Its arming asks for `control` as ever, and lists no check (K22).
    let held = send(
        &mut b.engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    );
    assert!(held.is_none(), "held: {held:?}");
    let approval = b.engine.state().approvals.values().next().cloned().unwrap();
    assert!(approval.preflight.is_empty(), "{approval:?}");
    assert_eq!(approval.summary, "arm Alpha");
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": false }),
    );
    assert!(later(&mut agent).is_some());

    // Its launch is held, asking an operator, listing the check.
    let held = send(&mut b.engine, &mut agent, "command/send", take_off());
    assert!(held.is_none(), "held: {held:?}");
    assert!(b.wire.take().is_empty());
    let pending = |b: &Bench| {
        b.engine
            .state()
            .approvals
            .values()
            .find(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
            .cloned()
            .unwrap()
    };
    let approval = pending(&b);
    assert_eq!(approval.preflight.len(), 1);
    assert_eq!(approval.preflight[0].id, "uxduck:plan");
    assert_eq!(
        approval.summary,
        "take off Alpha past 1 failing preflight check: Plan on the vehicle"
    );
    let notice = b
        .engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some(&format!("approval:{}", approval.id)))
        .cloned()
        .unwrap();
    let body = notice.body.as_deref().unwrap();
    assert!(
        body.starts_with("Plan on the vehicle: Ridge survey is chosen"),
        "{notice:?}"
    );
    assert!(body.contains("Allowing it launches past them."), "{body}");
    assert_eq!(notice.actions.len(), 2, "allow once or deny: {notice:?}");
    // Denied: nothing goes.
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": false }),
    );
    let denied = later(&mut agent).unwrap();
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    assert!(b.wire.take().is_empty());

    // Asked again and allowed: it launches past them, as the operator's.
    assert!(send(&mut b.engine, &mut agent, "command/send", take_off()).is_none());
    let approval = pending(&b);
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    let sent = b.wire.take();
    assert_eq!(sent.len(), 1, "sent once allowed");
    assert_eq!(sent[0].command, Command::Arm, "the take-off's first step");
    let receipt = b.engine.state().receipts[&sent[0].receipt_id].clone();
    assert_eq!(
        receipt.preflight_override,
        Some(PreflightOverride::Approval {
            approval_id: approval.id.clone()
        })
    );
    // "Always" remembers `control`, never the override: the next launch
    // asks again, and the next arming goes straight out.
    assert!(
        send(&mut b.engine, &mut agent, "command/send", take_off()).is_none(),
        "asks again"
    );
    assert!(b.wire.take().is_empty());
    let again = pending(&b);
    b.call(
        "approval/resolve",
        json!({ "id": again.id, "allow": false }),
    );
    assert!(later(&mut agent).is_some());
    assert!(
        send(
            &mut b.engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none()
    );
    let sent = b.wire.take();
    assert_eq!(sent.len(), 1, "arming needs no approval now: {sent:?}");
    assert_eq!(sent[0].command, Command::Arm);

    // It cannot override on its own, even allowed.
    let mut own = connect(&mut b.engine, "agent");
    let mut params = take_off();
    params["overridePreflight"] = json!(["uxduck:plan"]);
    let tried = send(&mut b.engine, &mut own, "command/send", params);
    let tried = tried.or_else(|| {
        let approval = b
            .engine
            .state()
            .approvals
            .values()
            .filter(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
            .max_by_key(|a| a.created_at_us)
            .cloned()
            .unwrap();
        b.call(
            "approval/resolve",
            json!({ "id": approval.id, "allow": true }),
        );
        later(&mut own)
    });
    let tried = tried.unwrap();
    assert_eq!(
        tried["error"]["data"]["kind"], "capability-denied",
        "{tried}"
    );
}

#[test]
fn with_nothing_failing_an_agent_with_control_arms_as_before() {
    let mut b = bench();
    let mut agent = connect(&mut b.engine, "agent");
    assert!(
        send(
            &mut b.engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none()
    );
    let approval = b.engine.state().approvals.values().next().cloned().unwrap();
    assert!(approval.preflight.is_empty());
    assert_eq!(approval.summary, "arm Alpha");
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert_eq!(b.wire.take().len(), 1);
    // Allowed always: the next goes straight out.
    assert!(
        send(
            &mut b.engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none()
    );
    let sent = b.wire.take();
    assert_eq!(sent.len(), 1, "no approval: {sent:?}");
    assert!(
        b.engine.state().receipts[&sent[0].receipt_id]
            .preflight_override
            .is_none()
    );
}

#[test]
fn a_plugins_launch_asks_an_operator_and_its_raw_launch_is_not_sent() {
    let mut b = bench();
    let installed = b.call("plugin/install", json!({ "path": "/plugins/launcher" }));
    assert!(installed.get("error").is_none(), "{installed}");
    b.engine
        .plugin_described("launcher", Description { panels: Vec::new() });
    let take_off = || SendCommand {
        vehicle_id: "v".into(),
        command: serde_json::from_value(
            json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }),
        )
        .unwrap(),
    };
    let raw = |command: u16| SendMessage {
        vehicle_id: "v".into(),
        name: "COMMAND_LONG".into(),
        fields: serde_json::from_value(json!({
            "target_system": 1, "target_component": 1, "command": command,
            "confirmation": 0, "param1": 1.0, "param2": 0.0, "param3": 0.0,
            "param4": 0.0, "param5": 0.0, "param6": 0.0, "param7": 10.0
        }))
        .unwrap(),
    };
    // Nothing failing: its launch goes, under its control capability.
    b.engine.plugin_effects(
        "launcher",
        Effects {
            commands: vec![take_off()],
            messages: vec![raw(22)],
            ..Effects::default()
        },
    );
    assert_eq!(b.wire.take().len(), 1);
    assert_eq!(b.wire.messages().len(), 1);

    b.wrong_plan();
    b.engine.plugin_effects(
        "launcher",
        Effects {
            commands: vec![take_off()],
            messages: vec![raw(22), raw(300)],
            ..Effects::default()
        },
    );
    assert!(b.wire.take().is_empty(), "held for an operator");
    assert!(
        b.wire.messages().is_empty(),
        "its own take-off and mission start are not sent"
    );
    let approval = b
        .engine
        .state()
        .approvals
        .values()
        .find(|a| a.client_name == "Launcher")
        .cloned()
        .unwrap();
    assert_eq!(approval.preflight[0].id, "uxduck:plan");
    // Its arming is never held by checks (K22), raw or not.
    b.engine.plugin_effects(
        "launcher",
        Effects {
            commands: vec![SendCommand {
                vehicle_id: "v".into(),
                command: uxduck_contracts::dto::command::Command::Arm,
            }],
            messages: vec![raw(400)],
            ..Effects::default()
        },
    );
    assert_eq!(b.wire.take().len(), 1, "its arming went");
    assert_eq!(b.wire.messages().len(), 1, "its raw arming went");
}

#[test]
fn a_check_set_to_hold_that_is_unknown_or_waiting_holds_a_launch_too() {
    let mut b = bench();
    // A mission above terrain, and the vehicle has not said whether its
    // terrain is loaded.
    let item = |frame: u8| MissionItem {
        command: 16,
        frame,
        param1: 0.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        latitude_deg: -35.36,
        longitude_deg: 149.16,
        altitude_m: 40.0,
        autocontinue: true,
    };
    b.engine
        .mission_downloaded("v", Ok(vec![item(0), item(10)]));
    b.configure(json!({ "check": "terrain", "severity": "block" }));
    let terrain = b.check("uxduck:terrain");
    assert_eq!(terrain["status"], "unknown", "{terrain}");
    assert_eq!(terrain["gates"], true);
    assert_eq!(b.launch(None)["result"]["reason"], "preflight");
    // Loading.
    b.telemetry("terrain.pending", 12.0);
    b.telemetry("terrain.loaded", 40.0);
    let terrain = b.check("uxduck:terrain");
    assert_eq!(terrain["status"], "waiting");
    assert_eq!(terrain["detail"], "Alpha is loading terrain.");
    assert_eq!(b.launch(None)["result"]["reason"], "preflight");
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
    // Loaded: it takes off.
    b.telemetry("terrain.pending", 0.0);
    assert_eq!(b.check("uxduck:terrain")["status"], "pass");
    assert_eq!(b.launch(None)["result"]["status"], "accepted");
}

#[test]
fn a_running_procedure_and_a_restart_due_warn_and_hold_the_launch_once_set_to() {
    let mut b = bench();
    // The vehicle's compasses, so the compass calibration can start.
    let mut reported = Vec::new();
    for (name, value, kind) in [
        ("COMPASS_DEV_ID", 97_539.0, ParamKind::Int32),
        ("COMPASS_USE", 1.0, ParamKind::Int8),
        ("COMPASS_EXTERNAL", 1.0, ParamKind::Int8),
        ("COMPASS_OFS_X", 0.0, ParamKind::Real32),
        ("COMPASS_OFS_Y", 0.0, ParamKind::Real32),
        ("COMPASS_OFS_Z", 0.0, ParamKind::Real32),
    ] {
        reported.push(Reported {
            name: name.into(),
            value,
            kind,
        });
    }
    b.engine
        .parameters_downloaded("v", Ok(reported), &BTreeMap::new());
    let started = b.call(
        "setup/start",
        json!({ "vehicleId": "v", "kind": "compass" }),
    );
    assert!(started.get("error").is_none(), "{started}");
    let order = b.wire.take().remove(0);
    b.engine
        .command_result(&order.receipt_id, Outcome::Accepted, 1);
    let setup = b.check("uxduck:setup");
    assert_eq!(setup["status"], "fail");
    assert_eq!(setup["detail"], "The compass calibration is running.");
    assert_eq!(setup["fix"], json!({ "type": "setup", "row": "compass" }));
    // It only warns (K25): nothing is held, a launch either.
    assert_eq!(setup["severity"], "warn");
    assert_eq!(setup["gates"], false);
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
    assert_eq!(b.launch(None)["result"]["status"], "accepted");
    // Set to hold, it holds the launch and never arming.
    b.configure(json!({ "check": "setup", "severity": "block" }));
    let refused = b.launch(None);
    assert_eq!(refused["result"]["preflight"][0]["id"], "uxduck:setup");
    assert_eq!(b.arm(None)["result"]["status"], "accepted");

    // Done, and waiting for a restart.
    b.time.advance(S);
    b.engine.mag_cal(
        "v",
        MagCal::Report {
            compass_id: 0,
            cal_mask: 1,
            cal_status: 4,
            autosaved: true,
            fitness: 3.5,
            offsets: [42.5, -17.25, 63.0],
        },
    );
    assert_eq!(b.check("uxduck:setup")["status"], "pass");
    let restart = b.check("uxduck:restart");
    assert_eq!(restart["status"], "fail", "{restart}");
    assert_eq!(
        restart["detail"],
        "The compass calibration takes effect once Alpha restarts."
    );
    assert_eq!(restart["gates"], false, "it only warns until set to hold");
    b.configure(json!({ "check": "restart", "severity": "block" }));
    assert_eq!(
        b.launch(None)["result"]["preflight"][0]["id"],
        "uxduck:restart"
    );
    // A person may make it a warning again, or turn it off.
    b.configure(json!({ "check": "restart", "severity": "warn" }));
    assert_eq!(b.check("uxduck:restart")["gates"], false);
    b.configure(json!({ "check": "restart", "on": false }));
    let checks = b.checks();
    assert!(
        !checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "uxduck:restart"),
        "off checks are not listed"
    );
    assert_eq!(checks["settings"][1]["on"], false);
    b.configure(json!({ "check": "restart", "on": true, "severity": "block" }));
    // Restarted: nothing waits.
    b.engine.vehicle_booted("v", 99 * S, true);
    assert_eq!(b.check("uxduck:restart")["status"], "pass");
    assert_eq!(b.launch(None)["result"]["status"], "accepted");
}

#[test]
fn the_link_the_controller_and_the_cameras() {
    let mut b = bench();
    let link = b.check("uxduck:link");
    assert_eq!(link["status"], "pass");
    assert_eq!(link["detail"], "Heard just now.");
    // A weak radio, below the threshold.
    b.telemetry("link.rssi_pct", 22.0);
    let link = b.check("uxduck:link");
    assert_eq!(link["status"], "fail");
    assert_eq!(link["detail"], "The radio signal is below 30 %.");
    // Its words carry no live number: the list is journaled only when the
    // signal crosses the threshold, not as it wavers.
    let updates = b.journal.preflight_updates();
    b.telemetry("link.rssi_pct", 25.0);
    b.engine.review_preflights();
    assert_eq!(b.journal.preflight_updates(), updates);
    b.configure(json!({ "check": "link", "threshold": 20 }));
    assert_eq!(b.check("uxduck:link")["status"], "pass");
    let bad = b.call(
        "preflight/configure",
        json!({ "vehicleId": "v", "check": "link", "threshold": 140 }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value", "{bad}");
    let none = b.call(
        "preflight/configure",
        json!({ "vehicleId": "v", "check": "plan", "threshold": 3 }),
    );
    assert_eq!(none["error"]["data"]["kind"], "invalid-value", "{none}");
    let reset = b.configure(json!({ "check": "link", "threshold": null }));
    assert_eq!(reset["settings"][2]["threshold"]["value"], 30.0);
    // Not heard.
    b.engine.vehicle_observed(Observation {
        connections: None,
        id: "v".into(),
        aliases: Vec::new(),
        link_id: b.link_id.clone(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: Some("4.5.1".into()),
        firmware_commit: None,
        presence: Presence::Stale,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    assert_eq!(
        b.check("uxduck:link")["detail"],
        "UXDuck can't hear Alpha right now."
    );
    // No controller drives it; no camera records with it.
    assert_eq!(
        b.check("uxduck:controller")["detail"],
        "No controller drives Alpha."
    );
    assert_eq!(
        b.check("uxduck:recording")["detail"],
        "No camera records with Alpha."
    );
}

#[test]
fn the_rc_receiver_check_is_what_the_vehicle_says_of_its_rc_input() {
    let mut b = bench();
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["title"], "Controller input");
    assert_eq!(rc["status"], "unknown");
    assert_eq!(rc["severity"], "warn");
    assert_eq!(rc["gates"], false);
    assert_eq!(rc["detail"], "Alpha hasn't said whether it has RC input.");
    b.rc("Not seen");
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["status"], "fail");
    assert_eq!(
        rc["detail"],
        "Alpha has had no RC input since it started. Check its transmitter is on and bound."
    );
    assert_eq!(rc["fix"], json!({ "type": "setup", "row": "radio" }));
    assert_eq!(rc["gates"], false);
    b.rc("Receiving");
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["status"], "pass");
    assert_eq!(rc["detail"], "Alpha is receiving RC input.");
    b.rc("Signal lost");
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["status"], "fail");
    assert_eq!(
        rc["detail"],
        "Alpha has lost its RC signal. Check its transmitter is on and in range."
    );
    assert_eq!(rc["fix"], json!({ "type": "setup", "row": "radio" }));
    // A person may have it hold the launch. Arm is never held.
    b.configure(json!({ "check": "rc-receiver", "severity": "block" }));
    assert_eq!(b.check("uxduck:rc-receiver")["gates"], true);
    assert_eq!(
        b.launch(None)["result"]["preflight"][0]["id"],
        "uxduck:rc-receiver"
    );
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
    b.configure(json!({ "check": "rc-receiver", "severity": "warn" }));
    // Its word counts for 10 s.
    b.rc("Receiving");
    b.time.advance(11 * S);
    assert_eq!(b.check("uxduck:rc-receiver")["status"], "unknown");
    // Turned off, as for a vehicle with no receiver, it is not listed.
    b.configure(json!({ "check": "rc-receiver", "on": false }));
    assert!(
        !b.checks()["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "uxduck:rc-receiver")
    );
}

/// A vehicle's failsafe takes a controller over RC or over the link alike,
/// so either passes until a person asks for one of the two (maintainer,
/// 2026-11-13).
#[test]
fn controller_input_takes_rc_the_link_or_either_as_a_person_sets_it() {
    let mut b = bench();
    let input = |b: &mut Bench| {
        b.checks()["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["check"] == "rc-receiver")
            .unwrap()["input"]
            .clone()
    };
    assert_eq!(input(&mut b), "either");
    assert!(
        b.checks()["settings"][2].get("input").is_none(),
        "only the check that asks has one"
    );
    b.rc("Receiving");
    assert_eq!(b.check("uxduck:rc-receiver")["status"], "pass");
    // The link alone: no controller drives it from UXDuck, so it fails and
    // leads to the controllers, whatever its receiver says.
    b.configure(json!({ "check": "rc-receiver", "input": "link" }));
    assert_eq!(input(&mut b), "link");
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["status"], "fail");
    assert_eq!(
        rc["detail"],
        "No controller drives Alpha from UXDuck. Choose one in Controls."
    );
    assert_eq!(rc["fix"], json!({ "type": "controllers" }));
    // Its own receiver alone: what the vehicle says.
    b.configure(json!({ "check": "rc-receiver", "input": "rc" }));
    let rc = b.check("uxduck:rc-receiver");
    assert_eq!(rc["status"], "pass");
    assert_eq!(rc["detail"], "Alpha is receiving RC input.");
    // Either again is the default, and nothing is kept for it.
    b.configure(json!({ "check": "rc-receiver", "input": "either" }));
    assert_eq!(input(&mut b), "either");
    // A check that takes no input refuses one.
    let refused = b.call(
        "preflight/configure",
        json!({ "vehicleId": "v", "check": "link", "input": "rc" }),
    );
    assert_eq!(
        refused["error"]["message"], "“Link” has no input to choose",
        "{refused}"
    );
}

#[test]
fn the_plan_on_the_vehicle_is_the_one_chosen_verified_and_unchanged() {
    let mut b = bench();
    let ridge = b.plan("Ridge survey");
    let field = b.plan("Field loop");
    assert_eq!(
        b.check("uxduck:plan")["detail"],
        "No plan chosen for Alpha."
    );
    // Ridge survey was uploaded to Alpha and read back the same.
    let at = b.time.now() + S;
    b.time.advance(2 * S);
    let (ridge_id, field_id) = (ridge.clone(), field.clone());
    b.restart_with(|state| {
        state
            .plans
            .get_mut(&ridge_id)
            .unwrap()
            .uploads
            .push(PlanUpload {
                vehicle_id: "v".into(),
                plan_version: 2,
                at_us: at,
                verified: true,
                mission_version: 0,
            });
        let _ = &field_id;
    });
    let check = b.check("uxduck:plan");
    assert_eq!(check["status"], "pass", "{check}");
    assert_eq!(check["detail"], "Ridge survey, verified on Alpha.");
    assert_eq!(b.checks()["planId"], json!(ridge));

    // The demo: another plan chosen, made blocking.
    b.configure(json!({ "check": "plan", "severity": "block", "planId": field }));
    let check = b.check("uxduck:plan");
    assert_eq!(check["status"], "fail");
    assert_eq!(
        check["detail"],
        "Field loop is chosen, but Alpha has Ridge survey."
    );
    assert_eq!(b.launch(None)["result"]["reason"], "preflight");
    // Flown by hand: no plan.
    b.configure(json!({ "planId": null }));
    assert_eq!(b.check("uxduck:plan")["status"], "pass");
    // Ridge survey again, then its mission changed on the vehicle.
    b.configure(json!({ "planId": ridge }));
    assert_eq!(b.check("uxduck:plan")["status"], "pass");
    b.engine.mission_downloaded(
        "v",
        Ok(vec![MissionItem {
            command: 16,
            frame: 3,
            param1: 0.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            latitude_deg: -35.0,
            longitude_deg: 149.0,
            altitude_m: 30.0,
            autocontinue: true,
        }]),
    );
    assert_eq!(
        b.check("uxduck:plan")["detail"],
        "Alpha's mission changed after Ridge survey was uploaded."
    );
    let missing = b.call(
        "preflight/configure",
        json!({ "vehicleId": "v", "planId": "pl-none" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");
}

#[test]
fn the_list_is_journaled_when_it_changes_read_live_on_the_ground_and_kept_in_the_air() {
    let mut b = bench();
    b.rc("Receiving");
    b.checks();
    let before = b.journal.preflight_updates();
    assert!(before >= 1);
    // The same list again changes nothing.
    b.engine.review_preflights();
    b.time.advance(S);
    b.engine.review_preflights();
    assert_eq!(b.journal.preflight_updates(), before);
    // A change is journaled once.
    b.wrong_plan();
    let after = b.journal.preflight_updates();
    assert!(after > before);
    b.engine.review_preflights();
    assert_eq!(b.journal.preflight_updates(), after);
    // Armed on the ground: still read live, for the launch (K26).
    b.arm(None);
    b.seen(true);
    b.flight_state("On the ground");
    let armed = b.checks();
    assert_eq!(armed["evaluating"], true);
    assert_eq!(gating(&armed), ["uxduck:plan"]);
    b.configure(json!({ "planId": null }));
    assert!(
        gating(&b.checks()).is_empty(),
        "read live while on the ground"
    );
    b.wrong_plan();
    // In the air: the list stays as it launched, marked so, while the
    // checks would change.
    b.flight_state("In the air");
    let flying = b.checks();
    assert_eq!(flying["evaluating"], false);
    assert_eq!(gating(&flying), ["uxduck:plan"]);
    b.configure(json!({ "planId": null }));
    assert_eq!(gating(&b.checks()), ["uxduck:plan"], "kept in the air");
    // Landed and disarmed: evaluated again.
    b.flight_state("On the ground");
    b.seen(false);
    let disarmed = b.checks();
    assert_eq!(disarmed["evaluating"], true);
    assert!(gating(&disarmed).is_empty());
    // Pushed as the `preflight` entity.
    let entity = b.engine.state().preflights["v"].clone();
    assert!(entity.evaluating);
    assert!(entity.checks.iter().all(|c| c.status == CheckStatus::Pass
        || c.id.starts_with("vehicle:")
        || c.id == "uxduck:home"));
}

/// "On the ground" for a moment in flight, as a land detector that
/// flickers says, is still in the air: the list stays as it launched and a
/// mission's start is not held. Down for good, it is read live again.
#[test]
fn a_moment_on_the_ground_in_flight_is_still_launched() {
    let mut b = bench();
    b.wrong_plan();
    b.arm(None);
    b.seen(true);
    b.flight_state("On the ground");
    assert_eq!(b.checks()["evaluating"], true);
    b.flight_state("In the air");
    assert_eq!(b.checks()["evaluating"], false);
    let launched_with = b.journal.preflight_updates();
    let start = json!({ "command": { "type": "long", "command": 300, "params": [0, 0] } });
    for _ in 0..3 {
        b.time.advance(S);
        b.flight_state("On the ground");
        assert_eq!(b.checks()["evaluating"], false, "down for a moment");
        let receipt = b.command(None, start.clone());
        assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
        b.time.advance(S);
        b.flight_state("In the air");
    }
    assert_eq!(
        b.journal.preflight_updates(),
        launched_with,
        "the list it launched with stays"
    );
    b.flight_state("On the ground");
    b.time.advance(3 * S);
    b.flight_state("On the ground");
    assert_eq!(b.checks()["evaluating"], true, "landed");
    let receipt = b.command(None, start);
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
}

#[test]
fn a_refusal_is_a_receipt_with_its_reason_for_every_client() {
    let mut b = bench();
    b.wrong_plan();
    let receipt = b
        .engine
        .state()
        .receipts
        .values()
        .find(|r| r.reason == Some(RefusalReason::Preflight));
    assert!(receipt.is_none());
    b.launch(None);
    let receipt = b
        .engine
        .state()
        .receipts
        .values()
        .find(|r| r.reason == Some(RefusalReason::Preflight))
        .cloned()
        .unwrap();
    assert_eq!(receipt.status, ReceiptStatus::NotSent);
    assert_eq!(receipt.preflight[0].title, "Plan on the vehicle");
}

#[test]
fn a_vehicle_that_does_not_fly_is_held_until_it_is_armed() {
    // A rover has no leaving the ground to tell a launch by: its mission's
    // start is held while it is disarmed, and never once it is armed.
    let mut b = bench();
    b.seen_as("GroundRover", "MANUAL", false, "Ardupilotmega");
    b.wrong_plan();
    b.wire.take();
    let start = json!({ "type": "long", "command": 300, "params": [0, 0] });
    assert_eq!(b.checks()["evaluating"], true);
    let receipt = b.command(None, json!({ "command": start }));
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");
    // Arming it is never held.
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
    b.seen_as("GroundRover", "MANUAL", true, "Ardupilotmega");
    assert_eq!(b.checks()["evaluating"], false, "armed, it is under way");
    let receipt = b.command(None, json!({ "command": start }));
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert!(receipt["result"].get("preflight").is_none(), "{receipt}");
}

/// The take-off's steps from a mode other than GUIDED (`take_off_steps`).
fn from_another_mode() -> Vec<Command> {
    vec![
        Command::SetMode {
            mode: "GUIDED".into(),
        },
        Command::Arm,
        Command::take_off(10.0),
    ]
}

/// A disarmed copter or quadplane outside GUIDED is switched to GUIDED,
/// then armed, then taken off: the launch is held at once, before its
/// first step, so nothing reaches the real vehicle (review of #736, H1).
fn held_before_its_first_step(airframe: &str, heard: impl FnOnce(&mut Bench)) {
    let mut b = bench();
    heard(&mut b);
    b.wrong_plan();
    b.wire.take();
    let (steps, receipt) = b.take_off(None, json!({}));
    let result = &receipt["result"];
    assert_eq!(result["status"], "not-sent", "{airframe}: {receipt}");
    assert_eq!(result["reason"], "preflight", "{airframe}: {receipt}");
    assert_eq!(result["preflight"][0]["id"], "uxduck:plan", "{airframe}");
    assert!(
        result["detail"].as_str().unwrap().starts_with(
            "Take off wasn't sent to Alpha: 1 check that holds the launch doesn't pass: Plan on the vehicle"
        ),
        "{airframe}: {receipt}"
    );
    assert!(steps.is_empty(), "{airframe}: {steps:?}");
    assert!(
        b.wire.take().is_empty(),
        "{airframe}: nothing reached the vehicle"
    );
}

#[test]
fn a_take_off_from_loiter_waits_for_blocking_checks() {
    held_before_its_first_step("copter in LOITER", |b| {
        b.seen_as("Quadrotor", "LOITER", false, "Ardupilotmega");
    });
}

#[test]
fn a_take_off_from_stabilize_waits_for_blocking_checks() {
    held_before_its_first_step("copter in STABILIZE", |b| {
        b.seen_as("Quadrotor", "STABILIZE", false, "Ardupilotmega");
    });
}

#[test]
fn a_quadplanes_take_off_from_another_mode_waits_for_blocking_checks() {
    held_before_its_first_step("quadplane in QLOITER", |b| b.quadplane_in("QLOITER"));
}

#[test]
fn a_take_off_with_its_checks_passing_or_named_goes_as_before() {
    let mut b = bench();
    b.wrong_plan();
    // Arming alone is never held (K22).
    let arm = b.arm(None);
    assert_eq!(arm["result"]["status"], "accepted", "{arm}");
    // From GUIDED the take-off is held, and nothing goes.
    let (steps, guided) = b.take_off(None, json!({}));
    assert_eq!(guided["result"]["reason"], "preflight", "{guided}");
    assert!(steps.is_empty() && b.wire.take().is_empty());

    // An operator naming every failing check takes off past them, from
    // another mode, each step journaled with the override.
    b.seen_as("Quadrotor", "LOITER", false, "Ardupilotmega");
    let (steps, receipt) = b.take_off(None, json!({ "overridePreflight": ["uxduck:plan"] }));
    assert_eq!(steps, from_another_mode());
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert_eq!(receipt["result"]["command"]["command"], 22);
    assert_eq!(
        receipt["result"]["preflightOverride"],
        json!({ "by": "operator" })
    );
    assert_eq!(receipt["result"]["preflight"][0]["id"], "uxduck:plan");
    // Naming another check is no override.
    b.seen_as("Quadrotor", "LOITER", false, "Ardupilotmega");
    let (steps, wrong) = b.take_off(None, json!({ "overridePreflight": ["uxduck:terrain"] }));
    assert_eq!(wrong["result"]["reason"], "preflight", "{wrong}");
    assert!(steps.is_empty() && b.wire.take().is_empty());

    // Checks passing: arming goes; from GUIDED the take-off arms and
    // climbs, and from another mode it switches to GUIDED first.
    b.configure(json!({ "check": "plan", "severity": "warn" }));
    b.seen(false);
    let arm = b.arm(None);
    assert_eq!(arm["result"]["status"], "accepted", "{arm}");
    b.seen(false);
    let (steps, receipt) = b.take_off(None, json!({}));
    assert_eq!(steps, [Command::Arm, Command::take_off(10.0)]);
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
    assert!(
        receipt["result"].get("preflightOverride").is_none(),
        "{receipt}"
    );
    for mode in ["LOITER", "STABILIZE"] {
        b.seen_as("Quadrotor", mode, false, "Ardupilotmega");
        let (steps, receipt) = b.take_off(None, json!({}));
        assert_eq!(steps, from_another_mode(), "{mode}");
        assert_eq!(receipt["result"]["status"], "accepted", "{mode}: {receipt}");
    }
    let mut q = bench();
    q.quadplane_in("QLOITER");
    q.wire.take();
    let (steps, receipt) = q.take_off(None, json!({}));
    assert_eq!(steps, from_another_mode(), "quadplane");
    assert_eq!(receipt["result"]["status"], "accepted", "{receipt}");
}

#[test]
fn an_agents_take_off_from_another_mode_goes_past_failing_checks_only_once_approved() {
    let mut b = bench();
    b.seen_as("Quadrotor", "LOITER", false, "Ardupilotmega");
    b.wrong_plan();
    b.wire.take();
    let mut agent = connect(&mut b.engine, "agent");
    let take_off = json!({
        "vehicleId": "v",
        "command": { "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }
    });
    assert!(
        send(&mut b.engine, &mut agent, "command/send", take_off).is_none(),
        "held"
    );
    assert!(b.wire.take().is_empty());
    let approval = b.engine.state().approvals.values().next().cloned().unwrap();
    assert_eq!(approval.preflight[0].id, "uxduck:plan");
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": true }),
    );
    let mut steps = Vec::new();
    let reply = loop {
        let sent = b.wire.take();
        assert_eq!(sent.len(), 1, "one command at a time: {sent:?}");
        let receipt = b.engine.state().receipts[&sent[0].receipt_id].clone();
        assert_eq!(
            receipt.preflight_override,
            Some(PreflightOverride::Approval {
                approval_id: approval.id.clone()
            }),
            "{receipt:?}"
        );
        steps.push(sent[0].command.clone());
        b.engine
            .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
        if let Some(reply) = later(&mut agent) {
            break reply;
        }
    };
    assert_eq!(steps, from_another_mode());
    assert_eq!(reply["result"]["status"], "accepted", "{reply}");
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

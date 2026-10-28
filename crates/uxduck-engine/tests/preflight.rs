//! Preflight checks and the arming gate (S192, ADR-0038 K1 to K6): one list
//! per vehicle, the vehicle's own reasons first; UXDuck's own checks, each
//! on or off with its severity; the gate in the daemon for every client
//! (window, CLI, agent and MCP, plugins), warnings never stopping it and a
//! blocking check that is unknown or waiting counting as failing; the
//! operator's override naming the checks, refused for anyone else; an
//! agent's or a plugin's arming put to an operator as an approval that
//! lists them.

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
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
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
            state,
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
        self.engine.vehicle_observed(Observation {
            id: "v".into(),
            aliases: Vec::new(),
            link_id: self.link_id.clone(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: None,
            firmware: Some("4.5.1".into()),
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: self.time.now(),
            armed,
            mode: Some("GUIDED".into()),
        });
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
            "uxduck:controller",
            "uxduck:plan",
            "uxduck:terrain",
            "uxduck:recording"
        ]
    );
    // The vehicle has not said whether it is ready: its row says so, and
    // never holds arming in UXDuck (K5).
    assert_eq!(checks["checks"][0]["source"], "vehicle");
    assert_eq!(checks["checks"][0]["status"], "unknown");
    assert_eq!(checks["checks"][0]["gates"], false);
    // Defaults (ADR-0038's open question, as assumed): a running procedure
    // and a restart due block, the rest warn; everything passes now.
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
            ("setup", "block"),
            ("restart", "block"),
            ("link", "warn"),
            ("controller", "warn"),
            ("plan", "warn"),
            ("terrain", "warn"),
            ("recording", "warn")
        ]
    );
    for check in &checks["checks"].as_array().unwrap()[1..] {
        assert_eq!(check["status"], "pass", "{check}");
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
fn warnings_never_stop_arming() {
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
}

#[test]
fn a_blocking_check_refuses_arming_from_the_window_and_the_cli_and_what_arms_on_the_ground() {
    let mut b = bench();
    b.wrong_plan();
    let checks = b.checks();
    assert_eq!(gating(&checks), ["uxduck:plan"]);

    let receipt = b.arm(None);
    let result = &receipt["result"];
    assert_eq!(result["status"], "not-sent", "{receipt}");
    assert_eq!(result["reason"], "preflight");
    assert_eq!(result["preflight"][0]["id"], "uxduck:plan");
    assert_eq!(
        result["detail"],
        "Arm wasn't sent to Alpha: 1 preflight check doesn't pass: Plan on the vehicle \
         (Ridge survey is chosen, but it isn't on Alpha yet). Fix them, or hold Arm anyway to \
         arm past them."
    );
    assert!(b.wire.take().is_empty(), "nothing reached the vehicle");

    // The CLI, an operator too, the same.
    let mut cli = connect(&mut b.engine, "operator-cli");
    let receipt = b.arm(Some(&mut cli));
    assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");

    // Whatever arms on the ground: a take-off, a mission's start, arming
    // by number, forced past the vehicle's own checks, or by a mode.
    for command in [
        json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }),
        json!({ "type": "long", "command": 300, "params": [0, 0] }),
        json!({ "type": "long", "command": 400, "params": [1, 0] }),
        json!({ "type": "long", "command": 176, "params": [217, 4] }),
    ] {
        let receipt = b.command(None, json!({ "command": command }));
        assert_eq!(receipt["result"]["reason"], "preflight", "{receipt}");
    }
    let forced = b.command(None, json!({ "command": { "type": "arm" }, "force": true }));
    assert_eq!(
        forced["result"]["reason"], "preflight",
        "forcing passes the vehicle's checks, not UXDuck's: {forced}"
    );
    // The rest goes: a mode, disarming.
    let mode = b.command(
        None,
        json!({ "command": { "type": "set-mode", "mode": "LOITER" } }),
    );
    assert_eq!(mode["result"]["status"], "accepted", "{mode}");
    let disarm = b.command(None, json!({ "command": { "type": "disarm" } }));
    assert_eq!(disarm["result"]["status"], "accepted", "{disarm}");
    // Armed already (by the radio), a take-off is no arming: it goes.
    b.seen(true);
    let take_off = b.command(
        None,
        json!({ "command": { "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] } }),
    );
    assert_eq!(take_off["result"]["status"], "accepted", "{take_off}");
}

#[test]
fn an_operator_arms_anyway_naming_every_failing_check_and_the_journal_keeps_it() {
    let mut b = bench();
    b.wrong_plan();
    // Naming another check, or none, is no override.
    let wrong = b.command(
        None,
        json!({ "command": { "type": "arm" }, "overridePreflight": ["uxduck:terrain"] }),
    );
    assert_eq!(wrong["result"]["reason"], "preflight", "{wrong}");

    let receipt = b.command(
        None,
        json!({ "command": { "type": "arm" }, "overridePreflight": ["uxduck:plan"] }),
    );
    let result = &receipt["result"];
    assert_eq!(result["status"], "accepted", "{receipt}");
    assert_eq!(result["preflightOverride"], json!({ "by": "operator" }));
    assert_eq!(result["preflight"][0]["id"], "uxduck:plan");
    assert_eq!(result["preflight"][0]["status"], "fail");
    // The journal has the receipt with the override and the checks.
    let journaled = b
        .journal
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.change {
            Change::ReceiptUpserted { receipt } => Some(receipt.clone()),
            _ => None,
        })
        .find(|r| r.preflight_override.is_some())
        .expect("journaled");
    assert_eq!(
        journaled.preflight_override,
        Some(PreflightOverride::Operator)
    );
    assert_eq!(journaled.preflight[0].id, "uxduck:plan");
}

#[test]
fn never_past_the_checks_from_away() {
    let mut b = bench();
    b.wrong_plan();
    // Remote control allows critical commands; someone away holds the lease.
    let state = b.call("state/subscribe", json!({ "entities": ["settings"] }));
    let version = state["result"]["snapshot"]["settings"]["version"].clone();
    let allowed = b.call(
        "settings/update",
        json!({ "expectedVersion": version, "remoteControl": ["critical"] }),
    );
    assert!(allowed.get("result").is_some(), "{allowed}");
    let (outbox, inbox) = unbounded();
    let id = b.engine.connect(
        Transport::Internal,
        Trust::Relay {
            user_id: "u-ana".into(),
            name: "ana@example.com".into(),
        },
        outbox,
    );
    let mut ana = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = call(
        &mut b.engine,
        &mut ana,
        "initialize",
        json!({
            "clientInfo": { "name": "remote-operator", "version": "0.1.0", "kind": "remote-operator" },
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    let lease = call(
        &mut b.engine,
        &mut ana,
        "lease/take",
        json!({ "vehicleId": "v" }),
    );
    assert!(lease.get("result").is_some(), "{lease}");
    let now_ms = b.time.now() / 1000;
    let overriding = call(
        &mut b.engine,
        &mut ana,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" }, "commandId": "c1",
                "sentAtMs": now_ms, "overridePreflight": ["uxduck:plan"] }),
    );
    assert_eq!(
        overriding["error"]["data"]["kind"], "capability-denied",
        "{overriding}"
    );
    let refused = call(
        &mut b.engine,
        &mut ana,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" }, "commandId": "c2",
                "sentAtMs": now_ms }),
    );
    assert_eq!(refused["result"]["reason"], "preflight", "{refused}");
    assert!(
        !refused["result"]["detail"]
            .as_str()
            .unwrap()
            .contains("Arm anyway"),
        "no override offered away: {refused}"
    );
    assert!(b.wire.take().is_empty());
}

#[test]
fn an_agents_arming_waits_for_an_operator_who_sees_the_checks() {
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

    // Its arming is held, asking an operator, listing the check.
    let held = send(
        &mut b.engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" } }),
    );
    assert!(held.is_none(), "held: {held:?}");
    assert!(b.wire.take().is_empty());
    let approval = b.engine.state().approvals.values().next().cloned().unwrap();
    assert_eq!(approval.preflight.len(), 1);
    assert_eq!(approval.preflight[0].id, "uxduck:plan");
    assert_eq!(
        approval.summary,
        "arm Alpha past 1 failing preflight check: Plan on the vehicle"
    );
    let notice = b
        .engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some(&format!("approval:{}", approval.id)))
        .cloned()
        .unwrap();
    assert!(
        notice
            .body
            .as_deref()
            .unwrap()
            .starts_with("Plan on the vehicle: Ridge survey is chosen"),
        "{notice:?}"
    );
    assert_eq!(notice.actions.len(), 2, "allow once or deny: {notice:?}");
    // Denied: nothing goes.
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": false }),
    );
    let denied = later(&mut agent).unwrap();
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    assert!(b.wire.take().is_empty());

    // Asked again and allowed: it arms past them, as the operator's.
    assert!(
        send(
            &mut b.engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none()
    );
    let approval = b
        .engine
        .state()
        .approvals
        .values()
        .find(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
        .cloned()
        .unwrap();
    b.call(
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    let sent = b.wire.take();
    assert_eq!(sent.len(), 1, "sent once allowed");
    assert_eq!(sent[0].command, Command::Arm);
    let receipt = b.engine.state().receipts[&sent[0].receipt_id].clone();
    assert_eq!(
        receipt.preflight_override,
        Some(PreflightOverride::Approval {
            approval_id: approval.id.clone()
        })
    );
    // "Always" remembers `control`, never the override: the next arming
    // asks again.
    assert!(
        send(
            &mut b.engine,
            &mut agent,
            "command/send",
            json!({ "vehicleId": "v", "command": { "type": "arm" } }),
        )
        .is_none(),
        "asks again"
    );
    assert!(b.wire.take().is_empty());
    let again = b
        .engine
        .state()
        .approvals
        .values()
        .find(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
        .cloned()
        .unwrap();
    b.call(
        "approval/resolve",
        json!({ "id": again.id, "allow": false }),
    );
    assert!(later(&mut agent).is_some());

    // It cannot override on its own, even allowed.
    let mut own = connect(&mut b.engine, "agent");
    let tried = send(
        &mut b.engine,
        &mut own,
        "command/send",
        json!({ "vehicleId": "v", "command": { "type": "arm" }, "overridePreflight": ["uxduck:plan"] }),
    );
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
    assert!(b.wire.take().is_empty());
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
fn a_plugins_arming_asks_an_operator_and_its_raw_arming_is_not_sent() {
    let mut b = bench();
    let installed = b.call("plugin/install", json!({ "path": "/plugins/launcher" }));
    assert!(installed.get("error").is_none(), "{installed}");
    b.engine
        .plugin_described("launcher", Description { panels: Vec::new() });
    let arm = || SendCommand {
        vehicle_id: "v".into(),
        command: uxduck_contracts::dto::command::Command::Arm,
    };
    let raw = || SendMessage {
        vehicle_id: "v".into(),
        name: "COMMAND_LONG".into(),
        fields: serde_json::from_value(json!({
            "target_system": 1, "target_component": 1, "command": 400,
            "confirmation": 0, "param1": 1.0, "param2": 0.0, "param3": 0.0,
            "param4": 0.0, "param5": 0.0, "param6": 0.0, "param7": 0.0
        }))
        .unwrap(),
    };
    // Nothing failing: its arming goes, under its control capability.
    b.engine.plugin_effects(
        "launcher",
        Effects {
            commands: vec![arm()],
            messages: vec![raw()],
            ..Effects::default()
        },
    );
    assert_eq!(b.wire.take().len(), 1);
    assert_eq!(b.wire.messages().len(), 1);

    b.wrong_plan();
    b.engine.plugin_effects(
        "launcher",
        Effects {
            commands: vec![arm()],
            messages: vec![raw()],
            ..Effects::default()
        },
    );
    assert!(b.wire.take().is_empty(), "held for an operator");
    assert!(
        b.wire.messages().is_empty(),
        "its own COMMAND_LONG is not sent"
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
}

#[test]
fn a_blocking_check_that_is_unknown_or_waiting_holds_arming_too() {
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
    assert_eq!(b.arm(None)["result"]["reason"], "preflight");
    // Loading.
    b.telemetry("terrain.pending", 12.0);
    b.telemetry("terrain.loaded", 40.0);
    let terrain = b.check("uxduck:terrain");
    assert_eq!(terrain["status"], "waiting");
    assert_eq!(terrain["detail"], "Alpha is loading terrain.");
    assert_eq!(b.arm(None)["result"]["reason"], "preflight");
    // Loaded: it arms.
    b.telemetry("terrain.pending", 0.0);
    assert_eq!(b.check("uxduck:terrain")["status"], "pass");
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
}

#[test]
fn a_running_procedure_and_a_restart_due_block_by_default() {
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
    let refused = b.arm(None);
    assert_eq!(refused["result"]["preflight"][0]["id"], "uxduck:setup");

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
    assert_eq!(
        b.arm(None)["result"]["preflight"][0]["id"],
        "uxduck:restart"
    );
    // A person may make it a warning, or turn it off.
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
    assert_eq!(b.arm(None)["result"]["status"], "accepted");
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
    assert_eq!(b.arm(None)["result"]["reason"], "preflight");
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
fn the_list_is_journaled_when_it_changes_and_kept_while_armed() {
    let mut b = bench();
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
    // Armed (with the operator's override): the list stays as it armed,
    // marked so, while the checks would change.
    b.command(
        None,
        json!({ "command": { "type": "arm" }, "overridePreflight": ["uxduck:plan"] }),
    );
    b.seen(true);
    let armed = b.checks();
    assert_eq!(armed["evaluating"], false);
    assert_eq!(gating(&armed), ["uxduck:plan"]);
    b.configure(json!({ "planId": null }));
    assert_eq!(gating(&b.checks()), ["uxduck:plan"], "kept while armed");
    // Disarmed: evaluated again.
    b.seen(false);
    let disarmed = b.checks();
    assert_eq!(disarmed["evaluating"], true);
    assert!(gating(&disarmed).is_empty());
    // Pushed as the `preflight` entity.
    let entity = b.engine.state().preflights["v"].clone();
    assert!(entity.evaluating);
    assert!(
        entity
            .checks
            .iter()
            .all(|c| c.status == CheckStatus::Pass || c.id.starts_with("vehicle:"))
    );
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
    b.arm(None);
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

//! Plugins' preflight checks (S194, ADR-0038 K12, K13): declared in the
//! manifest, reported per vehicle with `Effects.checks`, listed after the
//! vehicle's checklist under the plugin's name; a report gone stale or a
//! plugin that is off, stopped or starting leaves the check unknown or
//! waiting, which holds arming when it blocks; a `block` check stops arming
//! only once a person allowed the plugin's checks to, and warns until then;
//! the answer is a person's, journaled, kept across reinstalls and
//! restarts; a plugin reports only the checks it declares and cannot tick a
//! checklist's step.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{Capability, DaemonCapabilities};
use uxduck_contracts::dto::plugin::{
    CommandCall, DeclaredCheck, Description, Effects, PluginManifest, ReportedCheck,
    ReportedStatus, SendCommand, Subscriptions,
};
use uxduck_contracts::dto::preflight::CheckSeverity;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::commands::Outcome;
use uxduck_engine::runtime::{
    Clock, CommandOrder, LinkPort, MemoryCredentials, NoAgent, NoSystem, Persistence, PluginLoad,
    PluginPort, Ports, Random,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const S: u64 = 1_000_000;
const WIND: &str = "plugin:wind-check:wind";

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

/// The commands the engine sent, and the plugins it ran.
#[derive(Clone, Default)]
struct Wire {
    commands: Arc<Mutex<Vec<CommandOrder>>>,
    loaded: Arc<Mutex<Vec<String>>>,
}

impl Wire {
    fn take(&self) -> Vec<CommandOrder> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }
}

impl LinkPort for Wire {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.commands.lock().unwrap().push(order);
    }
}

fn wind_check(stale_ms: u32) -> DeclaredCheck {
    DeclaredCheck {
        id: "wind".into(),
        title: "Wind under 8 m/s".into(),
        severity: CheckSeverity::Block,
        stale_ms,
    }
}

fn manifest(id: &str, name: &str, checks: Vec<DeclaredCheck>) -> PluginManifest {
    PluginManifest {
        abi: 2,
        id: id.into(),
        name: name.into(),
        version: "0.1.0".into(),
        description: None,
        wasm: String::new(),
        script: Some("index.ts".into()),
        capabilities: vec![Capability::Read, Capability::Control],
        subscribe: Subscriptions::default(),
        sends: Vec::new(),
        commands: Vec::new(),
        written_by: None,
        dialect: None,
        hosts: Vec::new(),
        traffic_source: false,
        starts_off: false,
        checks,
    }
}

/// The wind plugin, a radio check that only warns, and plugins with
/// mistakes in their checks.
impl PluginPort for Wire {
    fn inspect(&self, path: &str) -> Result<PluginManifest, String> {
        Ok(match path {
            "/plugins/wind" => manifest("wind-check", "Wind check", vec![wind_check(600_000)]),
            "/plugins/radio" => manifest(
                "radio-id",
                "Remote ID",
                vec![DeclaredCheck {
                    id: "broadcasting".into(),
                    title: "Remote ID broadcasting".into(),
                    severity: CheckSeverity::Warn,
                    stale_ms: 10_000,
                }],
            ),
            "/plugins/twice" => {
                manifest("twice", "Twice", vec![wind_check(1000), wind_check(1000)])
            }
            "/plugins/too-stale" => manifest("too-stale", "Too stale", vec![wind_check(500)]),
            "/plugins/untitled" => manifest(
                "untitled",
                "Untitled",
                vec![DeclaredCheck {
                    title: " ".into(),
                    ..wind_check(1000)
                }],
            ),
            path => return Err(format!("{path}: no readable plugin.json")),
        })
    }
    fn load(&mut self, plugin: PluginLoad) {
        self.loaded.lock().unwrap().push(plugin.id);
    }
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

    /// Whether the plugin's latest record, and who wrote it.
    fn last_plugin(&self, id: &str) -> Option<(Option<bool>, String)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|e| match &e.change {
                Change::PluginUpserted { plugin } if plugin.id == id => {
                    Some((plugin.checks_block, e.actor.client_id.clone()))
                }
                _ => None,
            })
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
    me: String,
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
    send(engine, client, method, params).unwrap_or_else(|| panic!("no reply to {method}"))
}

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
        me: String::new(),
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
    init["result"]["clientId"]
        .as_str()
        .unwrap_or_default()
        .clone_into(&mut client.me);
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
    bench.seen("v", 1);
    bench.wire.take();
    bench
}

impl Bench {
    fn seen(&mut self, id: &str, system_id: u8) {
        self.engine.vehicle_observed(Observation {
            id: id.into(),
            aliases: Vec::new(),
            link_id: self.link_id.clone(),
            peer: format!("127.0.0.1:{}", 5760 + u16::from(system_id)),
            system_id,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: None,
            firmware: Some("4.5.1".into()),
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: self.time.now(),
            armed: false,
            mode: Some("GUIDED".into()),
        });
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        call(&mut self.engine, &mut self.ui, method, params)
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn checks_of(&mut self, vehicle: &str) -> Value {
        self.ok("preflight/state", json!({ "vehicleId": vehicle }))
    }

    fn check_of(&mut self, vehicle: &str, id: &str) -> Value {
        let checks = self.checks_of(vehicle);
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} in {checks}"))
    }

    fn check(&mut self, id: &str) -> Value {
        self.check_of("v", id)
    }

    fn ids(&mut self) -> Vec<String> {
        self.checks_of("v")["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Installs a plugin and says it described itself: running.
    fn install(&mut self, path: &str, checks_block: Option<bool>) -> Value {
        let mut params = json!({ "path": path });
        if let Some(block) = checks_block {
            params["checksBlock"] = json!(block);
        }
        let installed = self.ok("plugin/install", params);
        let id = installed["id"].as_str().unwrap().to_owned();
        self.engine
            .plugin_described(&id, Description { panels: Vec::new() });
        installed
    }

    fn report(&mut self, plugin: &str, checks: Vec<ReportedCheck>) {
        self.engine.plugin_effects(
            plugin,
            Effects {
                checks,
                ..Effects::default()
            },
        );
    }

    fn wind(&mut self, status: ReportedStatus, detail: &str) {
        self.report(
            "wind-check",
            vec![ReportedCheck {
                vehicle_id: "v".into(),
                id: "wind".into(),
                status,
                detail: Some(detail.into()),
            }],
        );
    }

    fn arm(&mut self) -> Value {
        let params = json!({ "vehicleId": "v", "command": { "type": "arm" } });
        if let Some(reply) = send(&mut self.engine, &mut self.ui, "command/send", params) {
            return reply;
        }
        let sent = self.wire.take();
        assert_eq!(sent.len(), 1, "one command went: {sent:?}");
        self.engine
            .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
        later(&mut self.ui).expect("its receipt")
    }

    /// Arms, and disarms again if it went.
    fn try_arm(&mut self) -> Value {
        let receipt = self.arm();
        if receipt["result"]["status"] == "accepted" {
            self.seen("v", 1);
        }
        receipt["result"].clone()
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
fn a_plugins_check_follows_the_checklist_and_only_warns_until_a_person_allows_it_to_block() {
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "manual", "title": "Propellers tight" }] }),
    );
    let step = b.checks_of("v")["checklist"]["steps"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tick = b.ok(
        "checklist/tick",
        json!({ "vehicleId": "v", "stepId": step, "ticked": true }),
    );
    assert!(gating(&tick).is_empty(), "{tick}");

    // Installed without an answer: listed after the checklist, under the
    // plugin's name, warning only.
    let installed = b.ok("plugin/install", json!({ "path": "/plugins/wind" }));
    assert!(installed.get("checksBlock").is_none(), "{installed}");
    assert_eq!(
        installed["checks"],
        json!([{ "id": "wind", "title": "Wind under 8 m/s", "severity": "block", "staleMs": 600_000 }])
    );
    let ids = b.ids();
    assert_eq!(
        &ids[ids.len() - 2..],
        [format!("checklist:{step}"), WIND.to_owned()]
    );
    let starting = b.check(WIND);
    assert_eq!(starting["status"], "waiting");
    assert_eq!(starting["detail"], "The Wind check plugin is starting.");
    assert_eq!(starting["severity"], "warn");
    assert_eq!(
        starting["plugin"],
        json!({ "id": "wind-check", "name": "Wind check", "heldBack": true })
    );
    assert_eq!(starting["source"], "plugin");
    b.engine
        .plugin_described("wind-check", Description { panels: Vec::new() });
    let waiting = b.check(WIND);
    assert_eq!(waiting["status"], "waiting");
    assert_eq!(
        waiting["detail"],
        "The Wind check plugin hasn't reported on Alpha yet."
    );
    assert_eq!(
        waiting["fix"],
        json!({ "type": "plugin", "pluginId": "wind-check" })
    );

    // Over 8 m/s: it fails, but only warns, so arming goes.
    b.wind(ReportedStatus::Fail, "Over 8 m/s.");
    let failing = b.check(WIND);
    assert_eq!(failing["status"], "fail");
    assert_eq!(failing["detail"], "Over 8 m/s.");
    assert_eq!(failing["gates"], false);
    assert!(
        failing.get("fix").is_none(),
        "the plugin said what it found"
    );
    assert_eq!(b.try_arm()["status"], "accepted");

    // A person allows its checks to stop arming: now it holds arming.
    let allowed = b.ok(
        "plugin/setChecksBlock",
        json!({ "id": "wind-check", "block": true }),
    );
    assert_eq!(allowed["checksBlock"], true);
    let me = b.ui.me.clone();
    assert_eq!(
        b.journal.last_plugin("wind-check"),
        Some((Some(true), me)),
        "journaled under the person"
    );
    let blocking = b.check(WIND);
    assert_eq!(blocking["severity"], "block");
    assert_eq!(blocking["gates"], true);
    assert_eq!(blocking["plugin"]["heldBack"], json!(null));
    let refused = b.try_arm();
    assert_eq!(refused["status"], "not-sent", "{refused}");
    assert_eq!(refused["reason"], "preflight");
    assert_eq!(refused["preflight"][0]["id"], WIND);
    assert_eq!(
        refused["detail"],
        "Arm wasn't sent to Alpha: 1 preflight check doesn't pass: Wind under 8 m/s (Wind check) \
         (Over 8 m/s). Fix them, or hold Arm anyway to arm past them."
    );

    // The wind drops: it passes and arming goes.
    b.wind(ReportedStatus::Pass, "Under 8 m/s.");
    assert_eq!(b.check(WIND)["status"], "pass");
    assert_eq!(b.try_arm()["status"], "accepted");

    // Changed back on the plugin's page: it warns again.
    b.wind(ReportedStatus::Fail, "Over 8 m/s.");
    b.ok(
        "plugin/setChecksBlock",
        json!({ "id": "wind-check", "block": false }),
    );
    let warns = b.check(WIND);
    assert_eq!(
        (warns["severity"].as_str(), warns["gates"].as_bool()),
        (Some("warn"), Some(false))
    );
    assert_eq!(warns["plugin"]["heldBack"], true);
    assert_eq!(b.try_arm()["status"], "accepted");
}

#[test]
fn a_stale_report_and_a_plugin_that_is_off_or_stopped_leave_the_check_unknown_and_blocking() {
    let mut b = bench();
    let installed = b.install("/plugins/wind", Some(true));
    assert_eq!(installed["checksBlock"], true, "allowed at install");
    b.wind(ReportedStatus::Pass, "Under 8 m/s.");
    assert!(gating(&b.checks_of("v")).is_empty());

    // No report for longer than its staleMs (10 min): unknown, and it
    // holds arming. The 1 Hz beat notices with nothing else happening.
    b.time.advance(600 * S + 1);
    b.engine.review_preflights();
    let stale = b.check(WIND);
    assert_eq!(stale["status"], "unknown");
    assert_eq!(
        stale["detail"],
        "The Wind check plugin hasn't reported for 10 min."
    );
    assert_eq!(stale["gates"], true);
    assert_eq!(b.try_arm()["reason"], "preflight");
    b.wind(ReportedStatus::Pass, "Under 8 m/s.");
    assert_eq!(b.check(WIND)["status"], "pass");

    // Turned off: unknown, saying it isn't running, and still blocking.
    b.ok(
        "plugin/setEnabled",
        json!({ "id": "wind-check", "enabled": false }),
    );
    let off = b.check(WIND);
    assert_eq!(off["status"], "unknown");
    assert_eq!(
        off["detail"],
        "The Wind check plugin isn't running: it's turned off."
    );
    assert_eq!(off["gates"], true);
    assert_eq!(
        off["fix"],
        json!({ "type": "plugin", "pluginId": "wind-check" })
    );
    let refused = b.try_arm();
    assert_eq!(refused["reason"], "preflight", "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("Wind under 8 m/s (Wind check) (The Wind check plugin isn't running"),
        "{refused}"
    );
    // A report from before it stopped no longer counts once it is back.
    b.ok(
        "plugin/setEnabled",
        json!({ "id": "wind-check", "enabled": true }),
    );
    assert_eq!(b.check(WIND)["status"], "waiting");
    b.engine
        .plugin_described("wind-check", Description { panels: Vec::new() });
    assert_eq!(
        b.check(WIND)["detail"],
        "The Wind check plugin hasn't reported on Alpha yet."
    );
    assert_eq!(b.try_arm()["reason"], "preflight");
    b.wind(ReportedStatus::Pass, "Under 8 m/s.");

    // It stops by itself (a trap, too slow): unknown again.
    b.engine.plugin_failed("wind-check", "It ran out of time.");
    let stopped = b.check(WIND);
    assert_eq!(stopped["status"], "unknown");
    assert_eq!(
        stopped["detail"],
        "The Wind check plugin isn't running: it stopped."
    );
    assert_eq!(stopped["gates"], true);

    // Removed: its checks go with it.
    b.ok("plugin/remove", json!({ "id": "wind-check" }));
    assert!(!b.ids().iter().any(|id| id.starts_with("plugin:")));
    assert!(gating(&b.checks_of("v")).is_empty());
}

#[test]
fn a_plugin_reports_only_its_own_declared_checks_for_known_vehicles() {
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "manual", "title": "Propellers tight" }] }),
    );
    let step = b.checks_of("v")["checklist"]["steps"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    b.install("/plugins/wind", Some(true));
    b.install("/plugins/radio", None);
    let before = b.ids();
    // Checks it does not declare, another plugin's, a checklist step's and
    // UXDuck's own: all ignored. A vehicle UXDuck does not know: ignored.
    let reported = |vehicle: &str, id: &str| ReportedCheck {
        vehicle_id: vehicle.into(),
        id: id.into(),
        status: ReportedStatus::Pass,
        detail: None,
    };
    b.report(
        "wind-check",
        vec![
            reported("v", "rain"),
            reported("v", "broadcasting"),
            reported("v", &step),
            reported("v", &format!("checklist:{step}")),
            reported("v", "uxduck:setup"),
            reported("nobody", "wind"),
        ],
    );
    assert_eq!(b.ids(), before, "no check was added");
    let manual = b.check(&format!("checklist:{step}"));
    assert_eq!(manual["status"], "to-do", "a plugin cannot tick a step");
    assert_eq!(b.check(WIND)["status"], "waiting");
    let radio = b.check("plugin:radio-id:broadcasting");
    assert_eq!(radio["status"], "waiting", "another plugin's is untouched");
    assert_eq!(radio["severity"], "warn");
    assert_eq!(
        radio["plugin"]["heldBack"],
        json!(null),
        "it never asked to block"
    );

    // Two vehicles: each has its own report, by its own plugin's order.
    b.seen("w", 2);
    b.report(
        "wind-check",
        vec![ReportedCheck {
            vehicle_id: "w".into(),
            id: "wind".into(),
            status: ReportedStatus::Fail,
            detail: Some("Over 8 m/s.\nGusting.".into()),
        }],
    );
    let other = b.check_of("w", WIND);
    assert_eq!(other["status"], "fail");
    assert_eq!(other["detail"], "Over 8 m/s.", "one line");
    assert_eq!(b.check(WIND)["status"], "waiting");
    let ids = b.ids();
    let at = |id: &str| ids.iter().position(|i| i == id).unwrap();
    assert!(
        at("plugin:radio-id:broadcasting") < at(WIND),
        "by plugin name: Remote ID, then Wind check"
    );

    // Mistakes in a manifest's checks are refused at install.
    for (path, words) in [
        ("/plugins/twice", "declared twice"),
        ("/plugins/too-stale", "staleMs"),
        ("/plugins/untitled", "needs a title"),
    ] {
        let refused = b.call("plugin/install", json!({ "path": path }));
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains(words),
            "{path}: {refused}"
        );
    }
}

#[test]
fn only_a_person_answers_and_an_agents_arming_lists_the_plugins_check() {
    let mut b = bench();
    b.install("/plugins/wind", None);
    let mut agent = connect(&mut b.engine, "agent");
    let asked = call(
        &mut b.engine,
        &mut agent,
        "plugin/setChecksBlock",
        json!({ "id": "wind-check", "block": true }),
    );
    assert_eq!(
        asked["error"]["data"]["kind"], "capability-denied",
        "{asked}"
    );
    assert_eq!(b.check(WIND)["severity"], "warn");
    // A plugin with no check that asks to block has nothing to answer.
    b.install("/plugins/radio", None);
    let nothing = b.call(
        "plugin/setChecksBlock",
        json!({ "id": "radio-id", "block": true }),
    );
    assert_eq!(
        nothing["error"]["data"]["kind"], "invalid-request",
        "{nothing}"
    );
    let missing = b.call(
        "plugin/setChecksBlock",
        json!({ "id": "nothing", "block": true }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");

    // Allowed, and failing: an agent's arming is put to an operator with
    // the plugin's check listed (K4).
    b.ok(
        "plugin/setChecksBlock",
        json!({ "id": "wind-check", "block": true }),
    );
    b.wind(ReportedStatus::Fail, "Over 8 m/s.");
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
    assert_eq!(approval.preflight[0].id, WIND);
    assert_eq!(
        approval.summary,
        "arm Alpha past 1 failing preflight check: Wind under 8 m/s (Wind check)"
    );

    // The plugin's own arming is held the same way.
    b.engine.plugin_effects(
        "wind-check",
        Effects {
            commands: vec![SendCommand {
                vehicle_id: "v".into(),
                command: uxduck_contracts::dto::command::Command::Arm,
            }],
            ..Effects::default()
        },
    );
    assert!(b.wire.take().is_empty(), "held for an operator");
}

#[test]
fn the_answer_is_kept_across_reinstalls_and_restarts_and_reports_are_journaled_only_when_they_change()
 {
    let mut b = bench();
    b.install("/plugins/wind", Some(true));
    b.wind(ReportedStatus::Pass, "Under 8 m/s.");
    let journaled = b.journal.preflight_updates();
    for _ in 0..5 {
        b.time.advance(S);
        b.wind(ReportedStatus::Pass, "Under 8 m/s.");
    }
    assert_eq!(
        b.journal.preflight_updates(),
        journaled,
        "the same report again changes nothing"
    );
    b.wind(ReportedStatus::Fail, "Over 8 m/s.");
    assert_eq!(b.journal.preflight_updates(), journaled + 1);

    // Installed again from the folder (a new version): the answer stays.
    let again = b.install("/plugins/wind", None);
    assert_eq!(again["checksBlock"], true);
    assert_eq!(b.check(WIND)["severity"], "block");
    assert_eq!(
        b.check(WIND)["status"],
        "waiting",
        "reports went with the old one"
    );

    // A restart of the daemon: the answer is kept with the plugin; reports
    // are not, so it waits for the plugin again.
    let state = b.engine.state().clone();
    b.engine = engine_with(state, &b.wire, &b.journal, &b.time);
    b.ui = connect(&mut b.engine, "operator-ui");
    b.seen("v", 1);
    b.engine
        .plugin_described("wind-check", Description { panels: Vec::new() });
    let after = b.check(WIND);
    assert_eq!(after["severity"], "block");
    assert_eq!(after["status"], "waiting");
    assert!(
        b.wire
            .loaded
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == "wind-check"),
        "it started again"
    );
}

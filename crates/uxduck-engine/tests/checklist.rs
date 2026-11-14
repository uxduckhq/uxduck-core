//! A vehicle's checklist (S193 and S353, ADR-0038 K7 to K11, K14, K16,
//! K17): steps under their groups beside UXDuck's own checks, and a rule a
//! person ticks too, only once it passes; manual steps and
//! rules join the vehicle's preflight checks and hold the launch while
//! one set to hold does not pass; ticks come only from operators, say who and
//! when, and are never cleared by themselves (not by arming, landing, a
//! battery change, a restart of the vehicle or of the daemon); rules held
//! for seconds; the battery per cell; copies from another vehicle or a
//! file that stay independent; a pasted Markdown list; the agent writing
//! steps after asking; the record on arming in history and the flight;
//! the warning when it leaves the ground, flown from its radio, with a
//! check set to hold failing;
//! and the offer to set `BATT_ARM_VOLT`.

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
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::approval::ApprovalState;
use uxduck_core::domain::checklist::StepKind;
use uxduck_core::domain::history::{EntryKind, HistoryEvent};
use uxduck_core::domain::link::Link;
use uxduck_core::domain::notification::{NotificationState, Severity};
use uxduck_core::domain::parameter::ParamKind;
use uxduck_core::domain::preflight::{CheckStatus, PreflightOverride};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::commands::Outcome;
use uxduck_engine::runtime::{
    Clock, CommandOrder, LinkPort, MemoryCredentials, NoAgent, NoPlugins, NoSystem, Persistence,
    Ports, Random, TransferOrder,
};
use uxduck_engine::state::State;
use uxduck_engine::transfers::Reported;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

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

/// The commands and transfers the engine sent.
#[derive(Clone, Default)]
struct Wire {
    commands: Arc<Mutex<Vec<CommandOrder>>>,
    transfers: Arc<Mutex<Vec<TransferOrder>>>,
}

impl Wire {
    fn take(&self) -> Vec<CommandOrder> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }
    fn transfers(&self) -> Vec<TransferOrder> {
        std::mem::take(&mut *self.transfers.lock().unwrap())
    }
}

impl LinkPort for Wire {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.commands.lock().unwrap().push(order);
    }
    fn transfer(&mut self, order: TransferOrder) {
        self.transfers.lock().unwrap().push(order);
    }
}

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
    fn count(&self, what: fn(&Change) -> bool) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| what(&e.change))
            .count()
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
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
        plugins: Box::new(NoPlugins),
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

const PROPS: &str = "Propellers tight";
const BATTERY: &str = "Battery at least 22.2 V";

impl Bench {
    fn observe(&mut self, id: &str, system_id: u8, armed: bool) {
        self.engine.vehicle_observed(Observation {
            connections: None,
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
            armed,
            mode: Some("GUIDED".into()),
        });
    }

    fn seen(&mut self, armed: bool) {
        self.observe("v", 1, armed);
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        call(&mut self.engine, &mut self.ui, method, params)
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn checks(&mut self) -> Value {
        self.ok("preflight/state", json!({ "vehicleId": "v" }))
    }

    /// The check of the step with this title.
    fn step_check(&mut self, title: &str) -> Value {
        let checks = self.checks();
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["title"] == title && c["source"] == "checklist")
            .cloned()
            .unwrap_or_else(|| panic!("no step {title} in {checks}"))
    }

    fn step(&mut self, vehicle: &str, title: &str) -> Value {
        let checks = self.ok("preflight/state", json!({ "vehicleId": vehicle }));
        checks["checklist"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["title"] == title)
            .cloned()
            .unwrap_or_else(|| panic!("no step {title} in {checks}"))
    }

    fn step_id(&mut self, title: &str) -> String {
        self.step("v", title)["id"].as_str().unwrap().to_owned()
    }

    /// "Propellers tight" and "Battery at least 22.2 V, held 3 s", both
    /// set to hold the launch.
    fn demo_steps(&mut self) -> Value {
        self.ok(
            "checklist/update",
            json!({
                "vehicleId": "v",
                "add": [
                    { "kind": "manual", "title": PROPS, "severity": "block" },
                    {
                        "kind": "rule", "title": BATTERY, "item": "battery.voltage_v",
                        "op": ">=", "value": 22.2, "holdS": 3, "severity": "block"
                    }
                ]
            }),
        )
    }

    fn tick(&mut self, title: &str, ticked: bool) -> Value {
        let id = self.step_id(title);
        self.ok(
            "checklist/tick",
            json!({ "vehicleId": "v", "stepId": id, "ticked": ticked }),
        )
    }

    fn command(&mut self, params: Value) -> Value {
        let mut params = params;
        params["vehicleId"] = json!("v");
        if let Some(reply) = send(&mut self.engine, &mut self.ui, "command/send", params) {
            return reply;
        }
        let sent = self.wire.take();
        assert_eq!(sent.len(), 1, "one command went: {sent:?}");
        self.engine
            .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
        later(&mut self.ui).expect("its receipt")
    }

    fn arm(&mut self) -> Value {
        self.command(json!({ "command": { "type": "arm" } }))
    }

    /// Take off, the vehicle accepting each step: refused at once, or the
    /// take-off's receipt.
    fn launch(&mut self, params: Value) -> Value {
        let mut params = params;
        params["vehicleId"] = json!("v");
        params["command"] =
            json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] });
        if let Some(reply) = send(&mut self.engine, &mut self.ui, "command/send", params) {
            return reply;
        }
        loop {
            let sent = self.wire.take();
            assert_eq!(sent.len(), 1, "one command at a time: {sent:?}");
            self.engine
                .command_result(&sent[0].receipt_id, Outcome::Accepted, 1);
            if let Some(reply) = later(&mut self.ui) {
                return reply;
            }
        }
    }

    /// What the autopilot says of being on the ground or in the air.
    fn flight_state(&mut self, state: &str) {
        let item = TelemetryItem {
            value: None,
            text: Some(state.into()),
            ..self.item("flight.state", Unit::Count, 0.0)
        };
        self.telemetry(vec![item]);
    }

    fn item(&self, key: &str, unit: Unit, value: f64) -> TelemetryItem {
        TelemetryItem {
            key: key.into(),
            label: match key {
                "battery.voltage_v" => "Battery".into(),
                _ => key.into(),
            },
            unit,
            value: Some(value),
            text: None,
            source: "power".into(),
            component_id: 1,
            time_us: self.time.now(),
        }
    }

    fn telemetry(&mut self, items: Vec<TelemetryItem>) {
        self.engine.telemetry_observed(VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: Vec::new(),
            items,
        });
    }

    fn volts(&mut self, value: f64) {
        let item = self.item("battery.voltage_v", Unit::Volt, value);
        self.telemetry(vec![item]);
    }

    fn params(&mut self, params: &[(&str, f64)]) {
        let reported = params
            .iter()
            .map(|(name, value)| Reported {
                name: (*name).into(),
                value: *value,
                kind: ParamKind::Real32,
            })
            .collect();
        self.engine
            .parameters_downloaded("v", Ok(reported), &BTreeMap::new());
    }

    fn restart_daemon(&mut self) {
        let state = self.engine.state().clone();
        self.engine = engine_with(state, &self.wire, &self.journal, &self.time);
        self.ui = connect(&mut self.engine, "operator-ui");
        self.seen(false);
        self.wire.take();
    }

    fn history(&self) -> Vec<HistoryEvent> {
        let mut entries: Vec<_> = self.engine.state().history.values().cloned().collect();
        entries.sort_by_key(|e| e.written_at_us);
        entries
            .into_iter()
            .filter_map(|e| match e.kind {
                EntryKind::Event { event } => Some(event),
                EntryKind::Note { .. } => None,
            })
            .collect()
    }

    fn armed_records(&self) -> Vec<HistoryEvent> {
        self.history()
            .into_iter()
            .filter(|e| matches!(e, HistoryEvent::Armed { .. }))
            .collect()
    }
}

fn gating(checks: &Value) -> Vec<String> {
    checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["gates"] == true)
        .map(|c| c["title"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_demo_take_off_waits_for_both_steps_and_arm_never_does() {
    let mut b = bench();
    let after = b.demo_steps();
    // Its steps follow UXDuck's checks, in their order.
    let titles: Vec<&str> = after["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["source"] == "checklist")
        .map(|c| c["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, [PROPS, BATTERY]);
    let rule = &after["checklist"]["steps"][1];
    assert_eq!(rule["unit"], "V", "from the item's key");
    assert_eq!(rule["holdS"], 3.0);
    assert_eq!(rule["severity"], "block");
    let props = b.step_check(PROPS);
    assert_eq!(props["status"], "to-do");
    assert_eq!(props["detail"], "For a person to tick.");
    assert_eq!(b.step_check(BATTERY)["status"], "unknown");
    assert_eq!(gating(&b.checks()), [PROPS, BATTERY]);

    // Arm goes (K22). Take off waits for both (K23).
    assert_eq!(b.arm()["result"]["status"], "accepted");
    let refused = b.launch(json!({}));
    assert_eq!(refused["result"]["status"], "not-sent", "{refused}");
    assert_eq!(refused["result"]["reason"], "preflight");
    let named: Vec<&str> = refused["result"]["preflight"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["title"].as_str().unwrap())
        .collect();
    assert_eq!(named, [PROPS, BATTERY]);

    // The battery: low fails, enough waits its 3 s, then passes.
    b.volts(21.9);
    let low = b.step_check(BATTERY);
    assert_eq!(low["status"], "fail");
    assert_eq!(low["detail"], "Battery is below 22.2 V.");
    b.volts(22.6);
    let waiting = b.step_check(BATTERY);
    assert_eq!(waiting["status"], "waiting");
    assert_eq!(
        waiting["detail"],
        "Battery is at least 22.2 V. It passes once that holds 3 s."
    );
    b.time.advance(2 * S);
    b.volts(22.5);
    assert_eq!(b.step_check(BATTERY)["status"], "waiting");
    b.time.advance(S + 1);
    b.volts(22.4);
    let held = b.step_check(BATTERY);
    assert_eq!(held["status"], "pass", "{held}");
    assert_eq!(held["detail"], "Battery is at least 22.2 V, held 3 s.");
    assert_eq!(gating(&b.checks()), [PROPS]);

    // Ticked: who and when, and Take off goes.
    let ticked_at = b.time.now();
    let after = b.tick(PROPS, true);
    let step = &after["checklist"]["steps"][0];
    assert_eq!(step["tick"]["atUs"], ticked_at);
    assert_eq!(step["tick"]["by"]["clientName"], "operator-ui");
    assert_eq!(b.step_check(PROPS)["status"], "pass");
    assert!(gating(&b.checks()).is_empty());
    assert_eq!(b.launch(json!({}))["result"]["status"], "accepted");
}

#[test]
fn a_held_rule_starts_again_when_its_value_drops_and_a_warning_never_stops_arming() {
    let mut b = bench();
    b.demo_steps();
    b.tick(PROPS, true);
    b.volts(22.6);
    b.time.advance(2 * S);
    b.volts(21.0);
    assert_eq!(b.step_check(BATTERY)["status"], "fail");
    b.volts(22.6);
    b.time.advance(2 * S);
    b.volts(22.6);
    assert_eq!(
        b.step_check(BATTERY)["status"],
        "waiting",
        "the 3 s start again from the drop"
    );
    // Stale telemetry is not read.
    b.time.advance(11 * S);
    b.engine.review_preflights();
    assert_eq!(b.step_check(BATTERY)["status"], "unknown");

    // Made a warning, it never holds the launch.
    let id = b.step_id(BATTERY);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "severity": "warn" }] }),
    );
    let check = b.step_check(BATTERY);
    assert_eq!(check["gates"], false, "{check}");
    assert_eq!(b.arm()["result"]["status"], "accepted");
}

#[test]
fn rules_over_parameters_and_their_words() {
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [{ "kind": "rule", "title": "Arming checks on", "param": "arming_check", "op": "==", "value": 1 }]
        }),
    );
    let check = b.step_check("Arming checks on");
    assert_eq!(check["status"], "unknown");
    assert_eq!(
        check["detail"],
        "UXDuck hasn't read ARMING_CHECK from Alpha."
    );
    b.params(&[("ARMING_CHECK", 0.0)]);
    let check = b.step_check("Arming checks on");
    assert_eq!(check["status"], "fail");
    assert_eq!(check["detail"], "ARMING_CHECK is not 1.");
    b.params(&[("ARMING_CHECK", 1.0)]);
    assert_eq!(b.step_check("Arming checks on")["status"], "pass");

    // A rule without its operator or value, or reading both, is refused in
    // words.
    for (step, words) in [
        (
            json!({ "kind": "rule", "title": "X", "item": "battery.voltage_v", "value": 1 }),
            "needs an operator",
        ),
        (
            json!({ "kind": "rule", "title": "X", "item": "battery.voltage_v", "op": ">=" }),
            "needs a value",
        ),
        (
            json!({ "kind": "rule", "title": "X", "item": "battery.voltage_v", "param": "X", "op": ">=", "value": 1 }),
            "not both",
        ),
        (
            json!({ "kind": "rule", "title": "X", "item": "Battery", "op": ">=", "value": 1 }),
            "not a telemetry item",
        ),
        (json!({ "kind": "manual", "title": "  " }), "needs a title"),
    ] {
        let refused = b.call(
            "checklist/update",
            json!({ "vehicleId": "v", "add": [step] }),
        );
        let message = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(words), "{words}: {refused}");
    }
    assert_eq!(
        b.checks()["checklist"]["steps"].as_array().unwrap().len(),
        1,
        "nothing refused was kept"
    );
}

#[test]
fn one_rule_per_cell_reads_the_same_on_4s_and_6s() {
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [{ "kind": "rule", "title": "Cells charged", "item": "battery.cell_v", "op": ">=", "value": 3.8 }]
        }),
    );
    // Neither cells nor a cell count: unknown, saying what is missing.
    b.volts(16.4);
    let check = b.step_check("Cells charged");
    assert_eq!(check["status"], "unknown");
    assert!(
        check["detail"].as_str().unwrap().contains("OSD_CELL_COUNT"),
        "{check}"
    );
    // A 4S pack at 16.4 V, its count set on the vehicle: 4.1 V a cell.
    b.params(&[("OSD_CELL_COUNT", 4.0)]);
    b.volts(16.4);
    let cell = b
        .engine
        .telemetry("v")
        .unwrap()
        .items
        .iter()
        .find(|i| i.key == "battery.cell_v")
        .cloned()
        .unwrap();
    assert!((cell.value.unwrap() - 4.1).abs() < 1e-9, "{cell:?}");
    assert_eq!(cell.label, "Battery per cell");
    assert_eq!(b.step_check("Cells charged")["status"], "pass");
    // A 6S at 21.6 V is 3.6 V a cell: the same rule fails.
    b.params(&[("OSD_CELL_COUNT", 6.0)]);
    b.volts(21.6);
    assert_eq!(b.step_check("Cells charged")["status"], "fail");
    // Its own cells, when it reports them: the lowest counts.
    let cells = vec![
        b.item("battery.cell_1_v", Unit::Volt, 4.15),
        b.item("battery.cell_2_v", Unit::Volt, 3.7),
        b.item("battery.cell_3_v", Unit::Volt, 4.1),
    ];
    b.telemetry(cells);
    let check = b.step_check("Cells charged");
    assert_eq!(check["status"], "fail", "the lowest cell is 3.7 V");
    assert_eq!(check["detail"], "Battery per cell is below 3.8 V.");
}

#[test]
fn the_cells_average_sits_beside_the_lowest() {
    let mut b = bench();
    let item = |b: &Bench, key: &str| {
        b.engine
            .telemetry("v")
            .unwrap()
            .items
            .iter()
            .find(|i| i.key == key)
            .cloned()
    };
    // Neither cells nor a cell count: no average.
    b.volts(14.8);
    assert!(item(&b, "battery.cell_avg_v").is_none());
    // A 4S pack at 14.8 V, its count set on the vehicle: 3.7 V a cell,
    // the lowest and the average alike.
    b.params(&[("OSD_CELL_COUNT", 4.0)]);
    b.volts(14.8);
    let average = item(&b, "battery.cell_avg_v").unwrap();
    assert!((average.value.unwrap() - 3.7).abs() < 1e-9, "{average:?}");
    assert_eq!(average.label, "Battery cell average");
    assert_eq!(average.unit, Unit::Volt);
    let lowest = item(&b, "battery.cell_v").unwrap();
    assert!((lowest.value.unwrap() - 3.7).abs() < 1e-9, "{lowest:?}");
    // Its own cells: the average of all of them, the lowest apart.
    let cells = vec![
        b.item("battery.cell_1_v", Unit::Volt, 4.0),
        b.item("battery.cell_2_v", Unit::Volt, 3.5),
        b.item("battery.cell_3_v", Unit::Volt, 3.9),
    ];
    b.telemetry(cells);
    let average = item(&b, "battery.cell_avg_v").unwrap();
    assert!((average.value.unwrap() - 3.8).abs() < 1e-9, "{average:?}");
    let lowest = item(&b, "battery.cell_v").unwrap();
    assert!((lowest.value.unwrap() - 3.5).abs() < 1e-9, "{lowest:?}");
}

#[test]
fn ticks_come_only_from_people_and_never_clear_by_themselves() {
    let mut b = bench();
    b.demo_steps();
    let id = b.step_id(PROPS);
    // An agent cannot tick or clear, with or without `config`.
    let mut agent = connect(&mut b.engine, "agent");
    for (method, params) in [
        (
            "checklist/tick",
            json!({ "vehicleId": "v", "stepId": id, "ticked": true }),
        ),
        ("checklist/reset", json!({ "vehicleId": "v" })),
    ] {
        let refused = call(&mut b.engine, &mut agent, method, params);
        assert_eq!(
            refused["error"]["data"]["kind"], "capability-denied",
            "{refused}"
        );
    }
    assert!(
        b.engine.state().approvals.is_empty(),
        "ticking is never asked for"
    );
    // A rule is not ticked.
    let rule = b.step_id(BATTERY);
    let refused = b.call(
        "checklist/tick",
        json!({ "vehicleId": "v", "stepId": rule, "ticked": true }),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is a rule"),
        "{refused}"
    );

    b.tick(PROPS, true);
    let tick = b.step("v", PROPS)["tick"].clone();
    assert_eq!(b.step("v", PROPS)["flownSince"], Value::Null);
    // Armed, flown, landed: the tick stays, with a flight since.
    b.volts(22.6);
    b.time.advance(4 * S);
    b.volts(22.6);
    assert_eq!(b.arm()["result"]["status"], "accepted");
    b.seen(true);
    b.time.advance(60 * S);
    b.seen(false);
    let step = b.step("v", PROPS);
    assert_eq!(step["tick"], tick, "kept through the flight");
    assert_eq!(step["flownSince"], true);
    assert_eq!(b.step_check(PROPS)["status"], "pass");
    // A battery change (the voltage drops and comes back) and a restart
    // of the vehicle.
    b.volts(0.0);
    b.volts(25.0);
    b.engine.vehicle_booted("v", 99 * S, true);
    b.seen(false);
    assert_eq!(b.step("v", PROPS)["tick"], tick);
    // A restart of the daemon.
    b.restart_daemon();
    let step = b.step("v", PROPS);
    assert_eq!(step["tick"], tick, "kept with the vehicle");
    assert_eq!(step["flownSince"], true);

    // A person clears it, step by step or all at once.
    b.tick(PROPS, false);
    assert_eq!(b.step_check(PROPS)["status"], "to-do");
    b.tick(PROPS, true);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "manual", "title": "Payload latched" }] }),
    );
    b.tick("Payload latched", true);
    let reset = b.ok("checklist/reset", json!({ "vehicleId": "v" }));
    assert!(
        reset["checklist"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["tick"].is_null()),
        "{reset}"
    );
    // New words need a new tick; a new severity keeps it.
    b.tick(PROPS, true);
    let id = b.step_id(PROPS);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "severity": "warn" }] }),
    );
    assert!(b.step("v", PROPS)["tick"].is_object());
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "title": "Propellers tight and undamaged" }] }),
    );
    assert!(b.step("v", "Propellers tight and undamaged")["tick"].is_null());
    // Each change is journaled once.
    assert!(
        b.journal
            .count(|c| matches!(c, Change::ChecklistUpdated { .. }))
            >= 8
    );
}

#[test]
fn a_steady_or_wavering_battery_journals_nothing_new() {
    let mut b = bench();
    b.demo_steps();
    b.volts(22.6);
    b.time.advance(4 * S);
    b.volts(22.61);
    assert_eq!(b.step_check(BATTERY)["status"], "pass");
    let before = b
        .journal
        .count(|c| matches!(c, Change::PreflightUpdated { .. }));
    for i in 0..20 {
        b.time.advance(S);
        b.volts(22.6 + f64::from(i) * 0.01);
        b.engine.review_preflights();
    }
    assert_eq!(
        b.journal
            .count(|c| matches!(c, Change::PreflightUpdated { .. })),
        before,
        "no live value in the words"
    );
}

#[test]
fn a_copy_from_another_vehicle_is_its_own_from_then_on() {
    let mut b = bench();
    b.demo_steps();
    b.tick(PROPS, true);
    b.observe("w", 2, false);
    let bravo = b.engine.vehicle("w").unwrap().name.clone();
    let copied = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "w", "fromVehicleId": "v" }),
    );
    assert_eq!(copied["checklist"]["copiedFrom"], "Alpha's checklist");
    let steps = copied["checklist"]["steps"].as_array().unwrap().clone();
    assert_eq!(steps.len(), 2);
    assert!(steps[0]["tick"].is_null(), "ticks are never copied");
    assert_eq!(steps[1]["holdS"], 3.0);
    assert_ne!(steps[0]["id"], b.step_id(PROPS), "its own ids");

    // Bravo's changes leave Alpha's as it is, and the other way round.
    let bravo_battery = steps[1]["id"].clone();
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "w", "change": [{ "id": bravo_battery, "value": 16.0, "title": "Battery at least 16 V" }],
                "add": [{ "kind": "manual", "title": "Airspeed sensor cover off" }] }),
    );
    let alpha = b.checks();
    assert_eq!(alpha["checklist"]["steps"][1]["value"], 22.2);
    assert_eq!(alpha["checklist"]["steps"].as_array().unwrap().len(), 2);
    assert!(alpha["checklist"]["steps"][0]["tick"].is_object());
    let props = b.step_id(PROPS);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "remove": [props] }),
    );
    let w = b.ok("preflight/state", json!({ "vehicleId": "w" }));
    assert_eq!(w["checklist"]["steps"].as_array().unwrap().len(), 3);
    assert_eq!(w["checklist"]["steps"][0]["title"], PROPS);
    // Replacing, and copying from itself.
    let refused = b.call(
        "checklist/copy",
        json!({ "vehicleId": "w", "fromVehicleId": "w" }),
    );
    assert!(refused.get("error").is_some());
    let replaced = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "w", "fromVehicleId": "v", "replace": true }),
    );
    assert_eq!(
        replaced["checklist"]["steps"].as_array().unwrap().len(),
        1,
        "{bravo}: replaced by Alpha's one step"
    );
}

#[test]
fn a_checklist_file_round_trips_and_a_pasted_list_becomes_manual_steps() {
    let mut b = bench();
    b.demo_steps();
    b.tick(PROPS, true);
    let file = b.ok("checklist/export", json!({ "vehicleId": "v" }));
    assert_eq!(file["format"], "uxduck.checklist/1");
    assert_eq!(file["name"], "Alpha's checklist");
    assert_eq!(
        file["steps"][1],
        json!({ "kind": "rule", "title": BATTERY, "severity": "block", "item": "battery.voltage_v",
                "op": ">=", "value": 22.2, "unit": "V", "holdS": 3.0 })
    );
    assert!(!file.to_string().contains("tick"), "{file}");

    b.observe("w", 2, false);
    let imported = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "w", "file": file, "sourceName": "alpha.checklist.json" }),
    );
    assert_eq!(imported["checklist"]["copiedFrom"], "alpha.checklist.json");
    let again = b.ok("checklist/export", json!({ "vehicleId": "w" }));
    assert_eq!(again["steps"], file["steps"], "round trip");

    // A newer file is refused by name; another file is not a checklist.
    let newer = b.call(
        "checklist/copy",
        json!({ "vehicleId": "w", "file": { "format": "uxduck.checklist/2", "steps": [] } }),
    );
    assert!(
        newer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("newer UXDuck"),
        "{newer}"
    );
    let other = b.call(
        "checklist/copy",
        json!({ "vehicleId": "w", "file": { "steps": [] } }),
    );
    assert!(other.get("error").is_some());

    // A Notion or Markdown list: one manual step per line.
    let pasted = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "w", "replace": true,
                "markdown": "## At the field\n- [ ] Payload latched\n- [x] Area clear\n\nBattery charged" }),
    );
    let titles: Vec<&str> = pasted["checklist"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Payload latched", "Area clear", "Battery charged"]);
    assert!(
        pasted["checklist"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["kind"] == "manual" && s["tick"].is_null() && s["severity"] == "warn")
    );
    assert!(
        pasted["checklist"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["group"] == "At the field"),
        "the heading is their group"
    );
    assert_eq!(pasted["checklist"]["copiedFrom"], "a pasted list");
    // Two sources at once are refused.
    let two = b.call(
        "checklist/copy",
        json!({ "vehicleId": "w", "fromVehicleId": "v", "markdown": "- a" }),
    );
    assert!(two.get("error").is_some());
}

#[test]
fn the_agent_writes_steps_after_asking_and_never_ticks() {
    let mut b = bench();
    let mut agent = connect(&mut b.engine, "agent");
    // Reading and exporting are free.
    let read = call(
        &mut b.engine,
        &mut agent,
        "preflight/state",
        json!({ "vehicleId": "v" }),
    );
    assert!(read.get("result").is_some(), "{read}");
    // Writing a step asks the operator first.
    let held = send(
        &mut b.engine,
        &mut agent,
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "manual", "title": "Payload latched", "severity": "block" }] }),
    );
    assert!(held.is_none(), "held: {held:?}");
    let approval = b
        .engine
        .state()
        .approvals
        .values()
        .find(|a| a.state == ApprovalState::Pending)
        .cloned()
        .unwrap();
    assert_eq!(approval.method, "checklist/update");
    assert!(
        b.checks()["checklist"]["steps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    b.ok(
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    let done = later(&mut agent).unwrap();
    assert!(done.get("result").is_some(), "{done}");
    assert_eq!(b.step("v", "Payload latched")["kind"], "manual");
    // Holding `config` now, it adds a rule without asking.
    let rule = call(
        &mut b.engine,
        &mut agent,
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "rule", "title": BATTERY, "item": "battery.voltage_v", "op": ">=", "value": 22.2, "severity": "block" }] }),
    );
    assert!(rule.get("result").is_some(), "{rule}");
    // A pasted list too; it still cannot tick.
    let pasted = call(
        &mut b.engine,
        &mut agent,
        "checklist/copy",
        json!({ "vehicleId": "v", "markdown": "- [ ] Propellers tight" }),
    );
    assert!(pasted.get("result").is_some(), "{pasted}");
    let id = b.step_id(PROPS);
    let tick = call(
        &mut b.engine,
        &mut agent,
        "checklist/tick",
        json!({ "vehicleId": "v", "stepId": id, "ticked": true }),
    );
    assert_eq!(tick["error"]["data"]["kind"], "capability-denied");
    let exported = call(
        &mut b.engine,
        &mut agent,
        "checklist/export",
        json!({ "vehicleId": "v" }),
    );
    assert_eq!(
        exported["result"]["steps"].as_array().unwrap().len(),
        3,
        "{exported}"
    );
    // Its launch past the steps is the operator's to allow, as in S192.
    let arming = send(
        &mut b.engine,
        &mut agent,
        "command/send",
        json!({ "vehicleId": "v", "command": json!({ "type": "long", "command": 22, "params": [0, 0, 0, 0, 0, 0, 10] }) }),
    );
    assert!(arming.is_none());
    let asked = b
        .engine
        .state()
        .approvals
        .values()
        .find(|a| a.state == ApprovalState::Pending)
        .cloned()
        .unwrap();
    let listed: Vec<&str> = asked.preflight.iter().map(|c| c.title.as_str()).collect();
    // The two set to hold. The pasted step only warns (K25).
    assert_eq!(listed, ["Payload latched", BATTERY]);
}

#[test]
fn arming_records_the_checklist_in_history_and_the_flight() {
    let mut b = bench();
    b.demo_steps();
    b.tick(PROPS, true);
    let ticked_at = b.time.now();
    b.volts(22.6);
    b.time.advance(4 * S);
    b.volts(22.7);
    assert!(b.armed_records().is_empty());
    assert_eq!(b.arm()["result"]["status"], "accepted");
    b.seen(true);
    let records = b.armed_records();
    assert_eq!(records.len(), 1, "{records:?}");
    let HistoryEvent::Armed {
        flight_id,
        text,
        record,
    } = &records[0]
    else {
        unreachable!()
    };
    assert_eq!(text, "Armed with its checklist: 2 of 2 steps passed");
    assert_eq!(record.steps[0].title, PROPS);
    assert_eq!(record.steps[0].kind, StepKind::Manual);
    assert_eq!(record.steps[0].status, CheckStatus::Pass);
    let tick = record.steps[0].tick.as_ref().unwrap();
    assert_eq!(tick.at_us, ticked_at);
    assert_eq!(tick.by.client_name, "operator-ui");
    assert_eq!(record.steps[1].value, Some(22.7), "the value at arming");
    assert!(record.overridden.is_empty() && record.failing.is_empty());
    let flight = &b.engine.state().flights[flight_id.as_ref().unwrap()];
    assert_eq!(flight.checklist.as_ref(), Some(record));
    // Nothing failing, no warning.
    assert!(
        !b.engine
            .state()
            .notifications
            .values()
            .any(|n| n.condition.as_deref() == Some("preflight-armed:v"))
    );
    b.seen(false);

    // Launched past a step by the operator: the override is recorded.
    b.time.advance(S);
    b.ok("checklist/reset", json!({ "vehicleId": "v" }));
    let check_id = format!("checklist:{}", b.step_id(PROPS));
    let launched = b.launch(json!({ "overridePreflight": [check_id] }));
    assert_eq!(launched["result"]["status"], "accepted", "{launched}");
    b.seen(true);
    // In the air by UXDuck's own launch: nothing more to warn about.
    b.flight_state("In the air");
    assert!(
        !b.engine
            .state()
            .notifications
            .values()
            .any(|n| n.condition.as_deref() == Some("preflight-armed:v"))
    );
    let records = b.armed_records();
    let HistoryEvent::Armed { text, record, .. } = records.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(record.overridden[0].title, PROPS);
    assert_eq!(record.override_by, Some(PreflightOverride::Operator));
    assert_eq!(record.steps[0].status, CheckStatus::ToDo);
    assert_eq!(
        text,
        "Armed with its checklist: 1 of 2 steps passed. Launched past Propellers tight by the operator"
    );
}

#[test]
fn leaving_the_ground_by_hand_while_a_step_holds_the_launch_warns() {
    let mut b = bench();
    b.demo_steps();
    b.params(&[("BATT_ARM_VOLT", 0.0)]);
    b.volts(21.0);
    b.engine.review_preflights();
    let warning = |b: &Bench| {
        b.engine
            .state()
            .notifications
            .values()
            .find(|n| n.condition.as_deref() == Some("preflight-armed:v"))
            .cloned()
    };
    // Nobody sent it: the vehicle reports itself armed. Arming is never
    // held and never warned about (K22): a bench test arms too.
    b.seen(true);
    b.flight_state("On the ground");
    assert!(warning(&b).is_none(), "armed on the ground is no launch");
    let records = b.armed_records();
    let HistoryEvent::Armed { record, text, .. } = records.last().unwrap() else {
        unreachable!()
    };
    assert!(record.failing.is_empty() && record.overridden.is_empty());
    assert_eq!(text, "Armed with its checklist: 0 of 2 steps passed");

    // Flown by hand: it leaves the ground while both steps hold (K23).
    b.flight_state("In the air");
    let warning = warning(&b).expect("a warning");
    assert_eq!(warning.severity, Severity::Warning);
    assert_eq!(
        warning.title,
        "Alpha took off while 2 checks hold the launch"
    );
    assert_eq!(
        warning.body.as_deref(),
        Some(
            "UXDuck didn't launch it (it may have been flown by hand): Propellers tight, Battery at least 22.2 V. Set BATT_ARM_VOLT so Alpha holds arming for it itself."
        )
    );
    // Its flight keeps what it left the ground past.
    let flight = b
        .engine
        .state()
        .flights
        .values()
        .max_by_key(|f| f.version)
        .cloned()
        .unwrap();
    let record = flight.checklist.expect("the flight's record");
    assert_eq!(record.failing.len(), 2);
    assert_eq!(record.steps.len(), 2, "the steps as it armed stay");
    assert!(
        record
            .describe()
            .contains("Left the ground without UXDuck launching it"),
        "{}",
        record.describe()
    );
    // Disarmed, the warning goes.
    b.flight_state("On the ground");
    b.seen(false);
    assert!(
        b.engine
            .state()
            .notifications
            .get(&warning.id)
            .is_none_or(|n| n.state == NotificationState::Resolved)
    );
}

#[test]
fn the_vehicle_can_hold_the_battery_rule_itself() {
    let mut b = bench();
    b.demo_steps();
    assert!(
        b.checks()["checklist"]["offers"].is_null(),
        "no offer without the parameter"
    );
    b.params(&[("BATT_ARM_VOLT", 0.0), ("ARMING_CHECK", 1.0)]);
    let offer = b.checks()["checklist"]["offers"][0].clone();
    assert_eq!(offer["name"], "BATT_ARM_VOLT");
    assert_eq!(offer["value"], 22.2);
    assert_eq!(offer["current"], 0.0);
    assert_eq!(offer["held"], false);
    assert_eq!(offer["stepId"], b.step_id(BATTERY));
    // Set through the normal parameter path, with a reason in history.
    let set = send(
        &mut b.engine,
        &mut b.ui,
        "param/set",
        json!({ "vehicleId": "v", "name": "BATT_ARM_VOLT", "value": 22.2,
                "reason": "Checklist: Battery at least 22.2 V" }),
    );
    assert!(set.is_none(), "answered once written: {set:?}");
    let order = b
        .wire
        .transfers()
        .into_iter()
        .find_map(|o| match o {
            TransferOrder::SetParameter {
                order_id, value, ..
            } => Some((order_id, value)),
            _ => None,
        })
        .unwrap();
    assert!((order.1 - 22.2).abs() < 1e-6);
    b.engine.parameter_written(
        &order.0,
        Ok(Reported {
            name: "BATT_ARM_VOLT".into(),
            value: order.1,
            kind: ParamKind::Real32,
        }),
        1,
    );
    let offer = b.checks()["checklist"]["offers"][0].clone();
    assert_eq!(offer["held"], true, "{offer}");
    assert!(b.history().iter().any(|e| matches!(e,
        HistoryEvent::ParameterChanged { name, reason, .. }
            if name == "BATT_ARM_VOLT" && reason.as_deref() == Some("Checklist: Battery at least 22.2 V"))));
    // With ArduPilot's arming checks off, it would not hold.
    b.params(&[("BATT_ARM_VOLT", 22.2), ("ARMING_CHECK", 0.0)]);
    let offer = b.checks()["checklist"]["offers"][0].clone();
    assert_eq!(offer["held"], false);
    assert_eq!(
        offer["note"],
        "ARMING_CHECK leaves the battery out, so Alpha doesn't use BATT_ARM_VOLT."
    );
    // From ArduPilot 4.7, the checks it skips.
    b.params(&[("BATT_ARM_VOLT", 22.2), ("ARMING_SKIPCHK", 0.0)]);
    assert_eq!(b.checks()["checklist"]["offers"][0]["held"], true);
    b.params(&[("BATT_ARM_VOLT", 22.2), ("ARMING_SKIPCHK", 256.0)]);
    let offer = b.checks()["checklist"]["offers"][0].clone();
    assert_eq!(offer["held"], false);
    assert_eq!(
        offer["note"],
        "ARMING_SKIPCHK leaves the battery out, so Alpha doesn't use BATT_ARM_VOLT."
    );
}

#[test]
fn reordering_and_removing_steps() {
    let mut b = bench();
    b.demo_steps();
    let (props, battery) = (b.step_id(PROPS), b.step_id(BATTERY));
    let moved = b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "order": [battery, props] }),
    );
    assert_eq!(moved["checklist"]["steps"][0]["title"], BATTERY);
    let missing = b.call(
        "checklist/update",
        json!({ "vehicleId": "v", "remove": ["nope"] }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");
    let removed = b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "remove": [battery] }),
    );
    assert_eq!(removed["checklist"]["steps"].as_array().unwrap().len(), 1);
    assert_eq!(gating(&removed), [PROPS]);
    // Turned into a rule, a manual step loses its tick.
    b.tick(PROPS, true);
    let id = b.step_id(PROPS);
    let rule = b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "kind": "rule", "item": "battery.voltage_v", "op": ">=", "value": 20 }] }),
    );
    let step = &rule["checklist"]["steps"][0];
    assert_eq!(step["kind"], "rule");
    assert!(step["tick"].is_null());
    assert_eq!(step["unit"], "V");
}

#[test]
fn a_step_counts_in_the_preflight_or_at_the_launch() {
    const AREA: &str = "Area clear of people";
    let mut b = bench();
    let after = b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [
                { "kind": "manual", "title": PROPS },
                { "kind": "manual", "title": AREA, "when": "launch", "severity": "block" }
            ]
        }),
    );
    // A step as written counts in the preflight and only warns (K25, K26).
    let steps = &after["checklist"]["steps"];
    assert!(
        steps[0]["when"].is_null(),
        "the default is left out: {steps}"
    );
    assert_eq!(steps[0]["severity"], "warn");
    assert_eq!(steps[1]["when"], "launch");
    assert_eq!(b.step_check(PROPS)["when"], "preflight");
    assert_eq!(b.step_check(AREA)["when"], "launch");
    // The vehicle's reasons and UXDuck's own checks are read live and
    // count in both: they name neither. Home is about the place of the
    // launch, and counts there only (K27).
    let checks = b.checks();
    assert!(
        checks["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["source"] != "checklist" && c["id"] != "uxduck:home")
            .all(|c| c["when"].is_null()),
        "{checks}"
    );
    // Only the step set to hold holds the launch, and never arming.
    assert_eq!(gating(&checks), [AREA]);
    assert_eq!(b.arm()["result"]["status"], "accepted");
    assert_eq!(b.launch(json!({}))["result"]["reason"], "preflight");

    // Its tick stays until a person clears it (K9), a flight or not, and
    // through a move to the preflight and back.
    b.tick(AREA, true);
    assert_eq!(b.launch(json!({}))["result"]["status"], "accepted");
    b.seen(true);
    b.flight_state("In the air");
    b.flight_state("On the ground");
    b.seen(false);
    assert_eq!(
        b.step_check(AREA)["status"],
        "pass",
        "still ticked after a flight"
    );
    let id = b.step_id(AREA);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "when": "preflight" }] }),
    );
    let moved = b.step("v", AREA);
    assert!(
        moved["when"].is_null() && !moved["tick"].is_null(),
        "{moved}"
    );
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "when": "launch" }] }),
    );
    // A file and a copy carry where each step counts.
    let file = b.ok("checklist/export", json!({ "vehicleId": "v" }));
    assert_eq!(
        file["steps"][1],
        json!({ "kind": "manual", "title": AREA, "severity": "block", "when": "launch" })
    );
    // The whole list is reset by a person, both kinds of step at once.
    b.tick(PROPS, true);
    let reset = b.ok("checklist/reset", json!({ "vehicleId": "v" }));
    assert!(
        reset["checklist"]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["tick"].is_null()),
        "{reset}"
    );
}

/// Someone at this computer agreed to the Terms of Service (ADR-0047), so a
/// real vehicle arms for a window that acts as no one in particular.
#[test]
fn a_rule_a_person_ticks_too_passes_once_both_have() {
    const CHARGED: &str = "Battery charged and strapped";
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [{
                "kind": "rule", "title": CHARGED, "group": "Battery", "confirm": true,
                "item": "battery.voltage_v", "op": ">=", "value": 22.2, "severity": "block"
            }]
        }),
    );
    let step = b.step("v", CHARGED);
    assert_eq!(step["confirm"], true);
    assert_eq!(step["group"], "Battery");
    let id = b.step_id(CHARGED);

    // Until its rule passes nobody can tick it, and it says so.
    let unread = b.step_check(CHARGED);
    assert_eq!(unread["status"], "unknown");
    assert!(
        unread["detail"]
            .as_str()
            .unwrap()
            .ends_with("A person ticks it once that passes."),
        "{unread}"
    );
    b.volts(21.9);
    let low = b.step_check(CHARGED);
    assert_eq!(low["status"], "fail");
    assert_eq!(
        low["detail"],
        "Battery is below 22.2 V. A person ticks it once that passes."
    );
    let refused = b.call(
        "checklist/tick",
        json!({ "vehicleId": "v", "stepId": id, "ticked": true }),
    );
    assert!(
        refused["error"]["message"].as_str().unwrap().contains(
            "“Battery charged and strapped” can be ticked once its rule passes. Battery is below 22.2 V."
        ),
        "{refused}"
    );
    assert!(b.step("v", CHARGED)["tick"].is_null());

    // The rule passes: it waits for a person, and still holds the launch.
    b.volts(22.6);
    let waiting = b.step_check(CHARGED);
    assert_eq!(waiting["status"], "to-do");
    assert_eq!(
        waiting["detail"],
        "Battery is at least 22.2 V. For a person to tick."
    );
    assert_eq!(gating(&b.checks()), [CHARGED]);
    assert_eq!(b.launch(json!({}))["result"]["status"], "not-sent");
    b.tick(CHARGED, true);
    let passed = b.step_check(CHARGED);
    assert_eq!(passed["status"], "pass");
    assert_eq!(passed["detail"], "Battery is at least 22.2 V.");
    assert!(gating(&b.checks()).is_empty());

    // The rule failing again holds the launch. The tick stays for when it
    // passes again: only a person clears it (K9).
    b.volts(21.0);
    let dropped = b.step_check(CHARGED);
    assert_eq!(dropped["status"], "fail");
    assert_eq!(dropped["detail"], "Battery is below 22.2 V.");
    assert_eq!(gating(&b.checks()), [CHARGED]);
    assert!(!b.step("v", CHARGED)["tick"].is_null());
    b.volts(22.6);
    assert_eq!(b.step_check(CHARGED)["status"], "pass");

    // A new group or severity leaves the tick. Another value needs a new one.
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "group": "Power", "severity": "warn" }] }),
    );
    let moved = b.step("v", CHARGED);
    assert_eq!(moved["group"], "Power");
    assert!(!moved["tick"].is_null(), "{moved}");
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "value": 22.4, "severity": "block" }] }),
    );
    assert!(
        b.step("v", CHARGED)["tick"].is_null(),
        "the tick was for the old rule"
    );
    assert_eq!(b.step_check(CHARGED)["status"], "to-do");

    // Clearing a tick needs no passing rule.
    b.tick(CHARGED, true);
    b.volts(20.0);
    b.tick(CHARGED, false);
    assert!(b.step("v", CHARGED)["tick"].is_null());

    // Without `confirm` it is a plain rule again, in no group.
    b.volts(22.6);
    b.tick(CHARGED, true);
    b.ok(
        "checklist/update",
        json!({ "vehicleId": "v", "change": [{ "id": id, "confirm": false, "group": null }] }),
    );
    let plain = b.step("v", CHARGED);
    assert!(
        plain["tick"].is_null() && plain["confirm"].is_null() && plain["group"].is_null(),
        "{plain}"
    );
    let check = b.step_check(CHARGED);
    assert_eq!(check["status"], "pass");
    assert_eq!(check["detail"], "Battery is at least 22.4 V.");
}

#[test]
fn the_record_on_arming_keeps_a_ticked_rule_and_its_group() {
    let mut b = bench();
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [{
                "kind": "rule", "title": BATTERY, "group": "Battery", "confirm": true,
                "item": "battery.voltage_v", "op": ">=", "value": 22.2
            }]
        }),
    );
    b.volts(22.6);
    b.tick(BATTERY, true);
    assert_eq!(b.arm()["result"]["status"], "accepted");
    b.seen(true);
    let records = b.armed_records();
    let HistoryEvent::Armed { record, .. } = &records[0] else {
        unreachable!()
    };
    let step = &record.steps[0];
    assert_eq!(step.group.as_deref(), Some("Battery"));
    assert!(step.confirm);
    assert_eq!(step.status, CheckStatus::Pass);
    assert_eq!(step.value, Some(22.6));
    assert_eq!(step.tick.as_ref().unwrap().by.client_name, "operator-ui");
}

#[test]
fn checks_sit_under_their_concepts() {
    let mut b = bench();
    let checks = b.checks();
    let list = checks["checks"].as_array().unwrap();
    // UXDuck's own checks name their group. The vehicle's reasons have none.
    for (id, group) in [
        ("uxduck:setup", "Setup"),
        ("uxduck:restart", "Setup"),
        ("uxduck:link", "Link"),
        ("uxduck:rc-receiver", "Controller"),
        ("uxduck:controller", "Controller"),
        ("uxduck:plan", "Plan"),
        ("uxduck:terrain", "Plan"),
        ("uxduck:recording", "Cameras"),
    ] {
        let check = list
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("no {id} in {checks}"));
        assert_eq!(check["group"], group, "{id}");
    }
    assert!(
        list.iter()
            .filter(|c| c["source"] == "vehicle")
            .all(|c| c["group"].is_null())
    );

    // A step joins a group by its name, tidied. One without stays loose.
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "v",
            "add": [
                { "kind": "manual", "title": "Sticks move the right surfaces", "group": "  Controller " },
                { "kind": "manual", "title": PROPS }
            ]
        }),
    );
    assert_eq!(
        b.step_check("Sticks move the right surfaces")["group"],
        "Controller"
    );
    assert!(b.step_check(PROPS)["group"].is_null());
    let long = b.call(
        "checklist/update",
        json!({ "vehicleId": "v", "add": [{ "kind": "manual", "title": "X", "group": "g".repeat(61) }] }),
    );
    assert!(
        long["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at most 60 characters"),
        "{long}"
    );

    // A pasted list's headings are its groups. A file and a copy keep
    // groups, and a rule's need for a tick.
    b.observe("w", 2, false);
    let pasted = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "w", "markdown": "# Battery\n- [ ] Strapped\n## Airframe\n- Propellers tight" }),
    );
    let groups: Vec<(&str, &str)> = pasted["checklist"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["title"].as_str().unwrap(), s["group"].as_str().unwrap()))
        .collect();
    assert_eq!(
        groups,
        [("Strapped", "Battery"), ("Propellers tight", "Airframe")]
    );
    b.ok(
        "checklist/update",
        json!({
            "vehicleId": "w",
            "add": [{
                "kind": "rule", "title": "Charged", "group": "Battery", "confirm": true,
                "item": "battery.voltage_v", "op": ">=", "value": 22.2
            }]
        }),
    );
    let file = b.ok("checklist/export", json!({ "vehicleId": "w" }));
    assert_eq!(file["format"], "uxduck.checklist/1");
    assert_eq!(
        file["steps"][0],
        json!({ "kind": "manual", "title": "Strapped", "group": "Battery", "severity": "warn" })
    );
    assert_eq!(file["steps"][2]["confirm"], true);
    let copied = b.ok(
        "checklist/copy",
        json!({ "vehicleId": "v", "replace": true, "fromVehicleId": "w" }),
    );
    let step = &copied["checklist"]["steps"][2];
    assert_eq!(
        (step["group"].as_str(), step["confirm"].as_bool()),
        (Some("Battery"), Some(true))
    );
    assert!(step["tick"].is_null());
}

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

//! The browser's daemon driven as its worker drives it, with the fake
//! vehicle in place of ArduPilot's WebAssembly build (S20).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value,
    reason = "test scenarios"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use uxduck_engine::runtime::{Clock, Random};
use uxduck_engine::{Engine, EngineConfig};
use uxduck_mavlink::sim::FakeVehicle;
use uxduck_web::root::{Browser, NO_ACCOUNT, Order, Output, Write};

#[derive(Clone, Default)]
struct TestClock(Arc<AtomicU64>);

impl Clock for TestClock {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn advance_ms(&self, ms: u64) {
        self.0.fetch_add(ms * 1000, Ordering::SeqCst);
    }
}

struct Counter(u8);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_add(1);
            *byte = self.0;
        }
    }
}

/// A page's view: its connection and what the worker did.
struct Page {
    connection: u32,
    next_id: i64,
    posts: Vec<Value>,
    journal: Vec<Write>,
    simulator: Vec<Order>,
    written: Vec<Vec<u8>>,
    /// Fetches the worker was asked for (S67).
    fetches: Vec<(u32, String)>,
}

impl Page {
    fn take(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            match output {
                Output::Post { connection, text } if connection == self.connection => {
                    self.posts.push(serde_json::from_str(&text).unwrap());
                }
                // Terrain (S23) has its own test.
                Output::Post { .. } | Output::Store(_) => {}
                Output::Journal(write) => self.journal.push(write),
                Output::Simulator(order) => self.simulator.push(order),
                Output::LinkWrite { bytes, .. } => self.written.push(bytes),
                Output::Fetch { request, url } => self.fetches.push((request, url)),
            }
        }
    }

    fn call(&mut self, browser: &mut Browser, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let text = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let outputs = browser.receive(self.connection, &text.to_string());
        self.take(outputs);
        self.posts
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no answer to {method}: {:?}", self.posts.last()))
    }
}

fn start(clock: &TestClock, checkpoint: Option<&str>, events: &str) -> (Browser, Page) {
    let (mut browser, outputs) = Browser::new(
        "0.1.0",
        Box::new(clock.clone()),
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        checkpoint,
        events,
    )
    .unwrap();
    let (connection, more) = browser.connect();
    let mut page = Page {
        connection,
        next_id: 0,
        posts: vec![],
        journal: vec![],
        simulator: vec![],
        written: vec![],
        fetches: vec![],
    };
    page.take(outputs);
    page.take(more);
    let init = page.call(
        &mut browser,
        "initialize",
        json!({
            "clientInfo": { "name": "web", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["runsIn"], "browser",
        "{init}"
    );
    assert_eq!(init["result"]["capabilities"]["simulator"], true);
    (browser, page)
}

const PLACE: &str = r#"{
    "type": "simulator",
    "vehicle": "multicopter",
    "latitudeDeg": -35.363261,
    "longitudeDeg": 149.16523,
    "altitudeM": 584.0,
    "headingDeg": 353.0
}"#;

/// Bytes between the fake vehicle and the browser's daemon until both
/// have nothing more to say.
fn converse(browser: &mut Browser, page: &mut Page, vehicle: &mut FakeVehicle, link_id: &str) {
    for _ in 0..10 {
        let written: Vec<Vec<u8>> = std::mem::take(&mut page.written);
        if written.is_empty() {
            break;
        }
        for bytes in written {
            for reply in vehicle.answer(&bytes) {
                let outputs = browser.link_received(link_id, &reply);
                page.take(outputs);
            }
        }
    }
}

#[test]
fn a_simulated_vehicle_starts_appears_and_obeys_like_a_real_one() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");

    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        page.simulator,
        vec![Order::Start {
            link_id: link_id.clone(),
            program: "copter",
            model: "quad".into(),
            speedup: 1,
            latitude_deg: -35.363_261,
            longitude_deg: 149.165_23,
            altitude_m: 584.0,
            heading_deg: 353.0,
            sysid: 1,
        }]
    );
    let order = serde_json::to_value(&page.simulator[0]).unwrap();
    assert_eq!(order["op"], "start");
    assert_eq!(order["linkId"], link_id.as_str());

    // A real link is refused: a browser has no sockets or serial ports.
    let refused = page.call(
        &mut browser,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only simulated vehicles")
    );

    let outputs = browser.link_status(&link_id, true, None);
    page.take(outputs);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    let outputs = browser.link_received(&link_id, &vehicle.heartbeat());
    page.take(outputs);
    converse(&mut browser, &mut page, &mut vehicle, &link_id);
    let vehicles: Vec<_> = browser.state().vehicles.values().collect();
    assert_eq!(vehicles.len(), 1);
    assert_eq!(vehicles[0].link_id, link_id);

    // Arm from the page: the command goes out on the simulator's serial
    // port, and the answer waits for its acknowledgement.
    let vehicle_id = vehicles[0].id.clone();
    page.next_id += 1;
    let id = page.next_id;
    let request = json!({
        "jsonrpc": "2.0", "id": id, "method": "command/send",
        "params": { "vehicleId": vehicle_id, "command": { "type": "arm" } }
    });
    let outputs = browser.receive(page.connection, &request.to_string());
    page.take(outputs);
    assert!(
        !page.written.is_empty(),
        "the command goes to the simulator"
    );
    converse(&mut browser, &mut page, &mut vehicle, &link_id);
    assert!(vehicle.armed);
    let answer = page.posts.iter().find(|p| p["id"] == id).unwrap();
    assert_eq!(answer["result"]["status"], "accepted", "{answer}");

    // Starting it again stops the simulator and starts one on the same link.
    page.simulator.clear();
    let again = page.call(&mut browser, "sim/restart", json!({ "id": link_id }));
    assert_eq!(again["result"]["id"], link_id, "{again}");
    assert!(
        matches!(
            &page.simulator[..],
            [Order::Stop { link_id: stopped }, Order::Start { link_id: started, .. }]
                if *stopped == link_id && *started == link_id
        ),
        "{:?}",
        page.simulator
    );

    // Stopping the simulation stops the simulator.
    page.simulator.clear();
    page.call(&mut browser, "sim/stop", json!({ "id": link_id }));
    assert_eq!(page.simulator, vec![Order::Stop { link_id }]);
}

#[test]
fn the_journal_brings_the_work_back_but_not_the_simulations_unless_asked() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    page.call(&mut browser, "operator/create", json!({ "name": "Ana" }));
    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let journal = |page: &Page| -> String {
        page.journal
            .iter()
            .filter_map(|w| match w {
                Write::Event(event) => Some(format!("{event}\n")),
                Write::Checkpoint { .. } => None,
            })
            .collect()
    };
    let lines = journal(&page);
    assert!(!lines.is_empty(), "every change is journaled");

    // The next visit: a torn last line is skipped, the rest restored. The
    // simulation is not remembered (maintainer, 2026-10-11).
    let (again, page_again) = start(&clock, None, &format!("{lines}{{\"seq\":"));
    assert_eq!(again.state().operators.len(), 1);
    assert!(!again.state().links.contains_key(&link_id));
    assert!(
        page_again.simulator.is_empty(),
        "{:?}",
        page_again.simulator
    );

    // With "Recreate simulations when UXDuck starts", it starts again.
    let version = browser.state().settings.version;
    page.call(
        &mut browser,
        "settings/update",
        json!({ "expectedVersion": version, "recreateSimulations": true }),
    );
    let (again, page_again) = start(&clock, None, &journal(&page));
    assert!(again.state().links.contains_key(&link_id));
    assert!(
        matches!(&page_again.simulator[..], [.., Order::Start { link_id: id, .. }] if *id == link_id),
        "{:?}",
        page_again.simulator
    );
}

#[test]
fn a_message_to_the_agent_is_kept_and_answered_with_why_it_cannot_reply_yet() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let config = page.call(&mut browser, "agent/config", json!({}));
    assert_eq!(config["result"]["provider"], "hosted");
    assert_eq!(config["result"]["ready"], false);

    let thread = page.call(&mut browser, "thread/create", json!({}));
    let thread_id = thread["result"]["id"].as_str().unwrap().to_owned();
    page.call(
        &mut browser,
        "thread/send",
        json!({ "threadId": thread_id, "text": "what can this vehicle do?" }),
    );
    let items: Vec<_> = browser
        .state()
        .items
        .values()
        .filter(|i| i.thread_id == thread_id)
        .collect();
    assert!(items.iter().any(|i| i.text == "what can this vehicle do?"));
    assert!(items.iter().any(|i| i.text == NO_ACCOUNT));
    // The turn is over, so another message goes in straight away.
    let again = page.call(
        &mut browser,
        "thread/send",
        json!({ "threadId": thread_id, "text": "and now?" }),
    );
    assert!(again.get("result").is_some(), "{again}");
}

/// An installed daemon's engine, recording what it journals.
#[derive(Clone, Default)]
struct Recorded(Arc<std::sync::Mutex<Vec<uxduck_core::events::Event>>>);

impl uxduck_engine::runtime::Persistence for Recorded {
    fn append(&mut self, event: &uxduck_core::events::Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &uxduck_engine::state::State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

#[test]
fn the_work_moves_to_an_installed_daemon_once_and_the_simulated_vehicle_stays() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    page.call(&mut browser, "operator/create", json!({ "name": "Ana" }));
    let thread = page.call(&mut browser, "thread/create", json!({}));
    let thread_id = thread["result"]["id"].as_str().unwrap().to_owned();
    page.call(
        &mut browser,
        "thread/send",
        json!({ "threadId": thread_id, "text": "how high can it go?" }),
    );
    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    let outputs = browser.link_received(&link_id, &vehicle.heartbeat());
    page.take(outputs);
    assert_eq!(browser.state().vehicles.len(), 1);
    let changes = browser.export();

    let journal = Recorded::default();
    let mut installed = Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: String::new(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(clock.clone()),
        Box::new(Counter(100)),
        uxduck_engine::runtime::Ports::with_persistence(Box::new(journal.clone())),
        uxduck_engine::Loaded::default(),
    );
    let (outbox, mut inbox) = futures::channel::mpsc::unbounded();
    let ui = installed.connect(
        uxduck_contracts::dto::trace::Transport::Socket,
        uxduck_core::policy::admission::Trust::LocalUser,
        outbox,
    );
    let mut ask = |engine: &mut Engine, id: u32, method: &str, params: Value| -> Value {
        engine.receive(
            ui,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|text| serde_json::from_str::<Value>(&text).unwrap())
            .find(|frame| frame["id"] == id)
            .unwrap()
    };
    ask(
        &mut installed,
        1,
        "initialize",
        json!({
            "clientInfo": { "name": "uxduck-web", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    let params = json!({ "source": "browser 1f3a", "changes": changes });
    let first = ask(&mut installed, 2, "journal/import", params.clone());
    assert_eq!(first["result"]["imported"], 4, "{first}"); // operator, chat, message, and the agent's answer
    assert_eq!(installed.state().operators.len(), 1);
    assert!(installed.state().threads.contains_key(&thread_id));
    assert!(
        installed.state().vehicles.is_empty(),
        "the simulated vehicle stays"
    );
    assert!(installed.state().links.is_empty());
    assert!(
        journal
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|e| e.actor.client_id == "import:browser 1f3a")
    );

    let again = ask(&mut installed, 3, "journal/import", params);
    assert_eq!(again["result"]["imported"], 0, "{again}");
}

/// An installed daemon's engine with one operator UI connected, and a way
/// to ask it.
fn installed(
    clock: &TestClock,
) -> (
    Engine,
    impl FnMut(&mut Engine, u32, &str, Value) -> Value + use<>,
) {
    let mut engine = Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: String::new(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(clock.clone()),
        Box::new(Counter(100)),
        uxduck_engine::runtime::Ports::with_persistence(Box::new(Recorded::default())),
        uxduck_engine::Loaded::default(),
    );
    let (outbox, mut inbox) = futures::channel::mpsc::unbounded();
    let ui = engine.connect(
        uxduck_contracts::dto::trace::Transport::Socket,
        uxduck_core::policy::admission::Trust::LocalUser,
        outbox,
    );
    let mut ask = move |engine: &mut Engine, id: u32, method: &str, params: Value| -> Value {
        engine.receive(
            ui,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        );
        std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|text| serde_json::from_str::<Value>(&text).unwrap())
            .find(|frame| frame["id"] == id)
            .unwrap()
    };
    ask(
        &mut engine,
        1,
        "initialize",
        json!({
            "clientInfo": { "name": "uxduck-web", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    (engine, ask)
}

/// The page makes an operator named "Operator" for a browser with none,
/// and the browser forgets its work once it has moved, so each visit
/// without an installed UXDuck made another: five "Operator"s beside the
/// maintainer's own by 2026-10-05. The stand-in stays behind when UXDuck
/// already has an operator; the chats still move.
#[test]
fn the_browsers_stand_in_operator_stays_behind_when_uxduck_has_one() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    page.call(
        &mut browser,
        "operator/create",
        json!({ "name": "Operator" }),
    );
    let thread = page.call(&mut browser, "thread/create", json!({}));
    let thread_id = thread["result"]["id"].as_str().unwrap().to_owned();
    let changes = browser.export();

    let (mut engine, mut ask) = installed(&clock);
    ask(&mut engine, 2, "operator/create", json!({ "name": "Ben" }));
    let params = json!({ "source": "browser 1f3a", "changes": changes });
    let result = ask(&mut engine, 3, "journal/import", params);
    assert_eq!(result["result"]["skipped"], 1, "{result}");
    assert!(engine.state().threads.contains_key(&thread_id));
    let names: Vec<&str> = engine
        .state()
        .operators
        .values()
        .map(|o| o.name.as_str())
        .collect();
    assert_eq!(names, ["Ben"]);

    // With no operator here, the stand-in moves in, and moves in again as
    // itself: the same one, not another.
    let (mut engine, mut ask) = installed(&clock);
    let params = json!({ "source": "browser 1f3a", "changes": browser.export() });
    let first = ask(&mut engine, 2, "journal/import", params.clone());
    assert_eq!(first["result"]["skipped"], 0, "{first}");
    assert_eq!(engine.state().operators.len(), 1);
    let id = browser.state().operators.keys().next().unwrap().clone();
    page.call(
        &mut browser,
        "operator/update",
        json!({ "id": id, "expectedVersion": 1, "units": "imperial" }),
    );
    let newer = json!({ "source": "browser 1f3a", "changes": browser.export() });
    let again = ask(&mut engine, 3, "journal/import", newer);
    assert_eq!(again["result"]["imported"], 1, "{again}");
    assert_eq!(engine.state().operators.len(), 1);
}

/// Runs the fake vehicle for `seconds`, five times a second: a heartbeat
/// each second, its telemetry, the session's tick, and the conversation.
fn fly(
    browser: &mut Browser,
    page: &mut Page,
    vehicle: &mut FakeVehicle,
    link_id: &str,
    clock: &TestClock,
    t_s: &mut f64,
    seconds: u32,
) {
    for i in 0..seconds * 5 {
        clock.advance_ms(200);
        *t_s += 0.2;
        if i % 5 == 0 {
            let outputs = browser.link_received(link_id, &vehicle.heartbeat());
            page.take(outputs);
        }
        for frame in vehicle.telemetry(*t_s) {
            let outputs = browser.link_received(link_id, &frame);
            page.take(outputs);
        }
        let outputs = browser.tick();
        page.take(outputs);
        converse(browser, page, vehicle, link_id);
    }
}

fn in_the_air(browser: &mut Browser, page: &mut Page, height: f64) -> String {
    let mut place: Value = serde_json::from_str(PLACE).unwrap();
    place["inAirM"] = json!(height);
    let link = page.call(browser, "link/add", json!({ "kind": place }));
    assert_eq!(link["result"]["launch"]["stage"], "climbing", "{link}");
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    let outputs = browser.link_status(&link_id, true, None);
    page.take(outputs);
    link_id
}

fn launch_of(browser: &Browser, link_id: &str) -> uxduck_core::domain::link::Launch {
    browser.state().links[link_id].launch.clone().unwrap()
}

#[test]
fn a_multicopter_started_in_the_air_is_flying_and_holding_without_anyone_taking_off() {
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let link_id = in_the_air(&mut browser, &mut page, 40.0);

    // SITL starts on the ground and is ready to arm a few seconds in.
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(4.0);
    let mut t_s = 0.0;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        3,
    );
    assert!(
        !vehicle.armed,
        "nothing arms before the pre-arm checks pass"
    );
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Climbing);

    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        20,
    );
    assert!(vehicle.armed);
    assert!(
        (vehicle.height_m - 40.0).abs() < 0.01,
        "{}",
        vehicle.height_m
    );
    assert_eq!(vehicle.mode, 4, "handed over in GUIDED");
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
    let speedup = vehicle
        .parameters
        .iter()
        .find(|(name, _, _)| name == "SIM_SPEEDUP")
        .map(|(_, value, _)| *value);
    assert_eq!(speedup, Some(1.0), "normal speed at hand-over");

    // Every command was the simulation start's: GUIDED, arm, take-off,
    // GUIDED to hold. Nobody armed or took off.
    let mut receipts: Vec<_> = browser.state().receipts.values().collect();
    // Sent in one instant against the fake vehicle, so ties go in the order
    // they were journaled.
    let journaled = |id: &str| {
        page.journal
            .iter()
            .position(|w| matches!(w, Write::Event(event) if event.contains(id)))
            .unwrap_or(usize::MAX)
    };
    receipts.sort_by_key(|r| (r.created_at_us, journaled(&r.id)));
    let labels: Vec<String> = receipts.iter().map(|r| r.command.label()).collect();
    assert_eq!(
        labels,
        ["Set mode GUIDED", "Arm", "Take off", "Set mode GUIDED"]
    );
    assert!(receipts.iter().all(|r| r.client_id == "simulation-start"));
    assert!(browser.state().notifications.is_empty());
}

#[test]
fn a_restarted_simulation_waits_for_new_prearm_checks_and_height() {
    use uxduck_core::domain::command::Command;
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let link_id = in_the_air(&mut browser, &mut page, 40.0);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    let mut t_s = 0.0;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        20,
    );
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
    let identity = browser.state().vehicles.values().next().unwrap().id.clone();
    let arms_before = browser
        .state()
        .receipts
        .values()
        .filter(|r| r.command == Command::Arm)
        .count();

    clock.advance_ms(200);
    page.call(&mut browser, "sim/restart", json!({ "id": link_id }));
    let mut restarted = FakeVehicle::new(1, 0x1234_abcd);
    restarted.on_ground = true;
    restarted.prearm_from_s = Some(12.0);
    // Heartbeats and parameter acknowledgements arrive before SYS_STATUS
    // and position. Neither the old ready flag nor old altitude may count.
    for _ in 0..8 {
        clock.advance_ms(1_000);
        page.take(browser.link_received(&link_id, &restarted.heartbeat()));
        page.take(browser.tick());
        converse(&mut browser, &mut page, &mut restarted, &link_id);
    }
    assert_eq!(
        browser
            .state()
            .receipts
            .values()
            .filter(|r| r.command == Command::Arm)
            .count(),
        arms_before,
        "old pre-arm readiness must not spend the restarted vehicle's arming retries"
    );
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Climbing);

    // Its new pre-arm report arrives before its new position. It may arm
    // and take off, but the previous flight's altitude cannot finish launch.
    clock.advance_ms(200);
    for frame in restarted.telemetry(12.0) {
        if uxduck_mavlink::decode::decode(&frame).is_some_and(|decoded| {
            matches!(
                decoded.message,
                uxduck_mavlink::decode::Message::SysStatus { .. }
            )
        }) {
            page.take(browser.link_received(&link_id, &frame));
        }
    }
    page.take(browser.tick());
    converse(&mut browser, &mut page, &mut restarted, &link_id);
    assert!(restarted.armed);
    assert_eq!(
        launch_of(&browser, &link_id).stage,
        LaunchStage::Climbing,
        "the last flight's altitude does not complete the new take-off"
    );

    t_s = 12.0;
    fly(
        &mut browser,
        &mut page,
        &mut restarted,
        &link_id,
        &clock,
        &mut t_s,
        20,
    );
    assert!(restarted.armed, "{:?}", launch_of(&browser, &link_id));
    assert_eq!(restarted.mode, 4, "holding in GUIDED");
    assert!((restarted.height_m - 40.0).abs() < 0.01);
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
    assert_eq!(browser.state().vehicles.len(), 1);
    assert!(browser.state().vehicles.contains_key(&identity));
}

/// A vehicle that disarmed itself before its take-off (a copter does, left
/// armed on the ground) is set to GUIDED and armed again, not sent the
/// take-off it can only refuse (maintainer, 2026-10-03: "sometimes it
/// doesn't take off").
#[test]
fn a_refused_take_off_starts_again_from_the_mode_and_flies() {
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let link_id = in_the_air(&mut browser, &mut page, 40.0);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    vehicle.disarms_before_take_off = true;
    let mut t_s = 0.0;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        30,
    );
    assert!(vehicle.armed);
    assert!(
        (vehicle.height_m - 40.0).abs() < 0.01,
        "{}",
        vehicle.height_m
    );
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
    assert!(browser.state().notifications.is_empty());
}

#[test]
fn an_optimistic_prearm_report_does_not_exhaust_retries_before_home_is_ready() {
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let link_id = in_the_air(&mut browser, &mut page, 40.0);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    // A healthy SYS_STATUS is not a promise that a GUIDED arm succeeds:
    // ArduPilot can still report "AHRS: waiting for home" when it is tried.
    vehicle.refuses_arm = true;
    let mut t_s = 0.0;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        12,
    );
    assert!(!vehicle.armed);
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Climbing);
    vehicle.refuses_arm = false;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        20,
    );
    assert!(vehicle.armed);
    assert_eq!(vehicle.mode, 4, "holding in GUIDED");
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
}

#[test]
fn a_take_off_that_cannot_arm_leaves_the_vehicle_on_the_ground_with_the_reason() {
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let link_id = in_the_air(&mut browser, &mut page, 40.0);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    vehicle.refuses_arm = true;
    let mut t_s = 0.0;
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        150,
    );

    assert!(!vehicle.armed);
    assert!(vehicle.height_m.abs() < f64::EPSILON);
    let launch = launch_of(&browser, &link_id);
    assert_eq!(launch.stage, LaunchStage::Failed);
    assert!(
        launch
            .detail
            .as_deref()
            .unwrap()
            .starts_with("it would not arm"),
        "{launch:?}"
    );
    // One notification for the outcome, not one per refused try.
    let titles: Vec<&str> = browser
        .state()
        .notifications
        .values()
        .map(|n| n.title.as_str())
        .collect();
    assert_eq!(titles, ["Simulated multicopter stayed on the ground"]);
}

#[test]
fn a_height_to_start_in_the_air_at_is_checked() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let mut place: Value = serde_json::from_str(PLACE).unwrap();
    place["inAirM"] = json!(2.0);
    let refused = page.call(&mut browser, "link/add", json!({ "kind": place }));
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("starts in the air at 10 to 500 m"),
        "{refused}"
    );
}

impl Page {
    /// Sends a request whose answer comes later, after the vehicle's.
    fn send(&mut self, browser: &mut Browser, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        let text = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let outputs = browser.receive(self.connection, &text.to_string());
        self.take(outputs);
        id
    }

    fn answer(&self, id: i64) -> Value {
        self.posts
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no answer to {id}"))
    }
}

fn waypoint(latitude_deg: f64, altitude_m: f64) -> Value {
    json!({
        "command": 16, "frame": 3,
        "param1": 0.0, "param2": 0.0, "param3": 0.0, "param4": 0.0,
        "latitudeDeg": latitude_deg, "longitudeDeg": 149.165_23,
        "altitudeM": altitude_m, "autocontinue": true
    })
}

/// A multicopter flying at 40 m, with `WPNAV_SPEED` changed, a two-waypoint
/// mission and `CIRCLE`, saved as "Over the ridge". Its link is removed.
fn snapshot_of_a_flight(
    browser: &mut Browser,
    page: &mut Page,
    clock: &TestClock,
    t_s: &mut f64,
) -> Value {
    let link_id = in_the_air(browser, page, 40.0);
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    fly(browser, page, &mut vehicle, &link_id, clock, t_s, 15);
    assert_eq!(vehicle.mode, 4, "holding in GUIDED");
    let vehicle_id = browser.state().vehicles.values().next().unwrap().id.clone();

    let set = page.send(
        browser,
        "param/set",
        json!({ "vehicleId": vehicle_id, "name": "WPNAV_SPEED", "value": 1500.0 }),
    );
    let mission = browser.state().missions[&vehicle_id].clone();
    let home = serde_json::to_value(uxduck_contracts::dto::mission::MissionItem::from(
        &mission.items[0],
    ))
    .unwrap();
    let upload = page.send(
        browser,
        "mission/upload",
        json!({
            "vehicleId": vehicle_id,
            "expectedVersion": mission.version,
            "items": [home, waypoint(-35.36, 60.0), waypoint(-35.35, 60.0)]
        }),
    );
    let mode = page.send(
        browser,
        "command/send",
        json!({ "vehicleId": vehicle_id, "command": { "type": "set-mode", "mode": "CIRCLE" } }),
    );
    fly(browser, page, &mut vehicle, &link_id, clock, t_s, 2);
    for id in [set, upload, mode] {
        let answer = page.answer(id);
        assert_eq!(answer["result"]["status"], "accepted", "{answer}");
    }

    let saved = page.call(
        browser,
        "sim/snapshot",
        json!({ "vehicleId": vehicle_id, "name": "Over the ridge" }),
    );
    assert!(saved.get("result").is_some(), "{saved}");
    // Saving says so wherever it was saved from.
    assert!(
        browser
            .state()
            .notifications
            .values()
            .any(|n| n.title == "Saved snapshot “Over the ridge”"),
        "a notification says it was saved"
    );
    page.call(browser, "sim/stop", json!({ "id": link_id }));
    saved["result"].clone()
}

#[test]
fn a_flying_snapshot_starts_again_in_the_air_with_its_parameters_mission_and_mode() {
    use uxduck_core::domain::link::LaunchStage;
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let mut t_s = 0.0;
    let snapshot = snapshot_of_a_flight(&mut browser, &mut page, &clock, &mut t_s);
    assert_eq!(snapshot["name"], "Over the ridge");
    assert_eq!(snapshot["flying"], true, "{snapshot}");
    assert_eq!(snapshot["mode"], "CIRCLE");
    assert_eq!(snapshot["missionCount"], 3);
    assert!((snapshot["heightM"].as_f64().unwrap() - 40.0).abs() < 0.5);
    assert!(snapshot["parameterCount"].as_u64().unwrap() >= 24);

    let started = page.call(
        &mut browser,
        "snapshot/start",
        json!({ "id": snapshot["id"] }),
    );
    let link = &started["result"];
    assert_eq!(link["kind"]["snapshotId"], snapshot["id"], "{started}");
    assert_eq!(link["kind"]["inAirM"], 40.0);
    assert_eq!(link["name"], "Over the ridge");
    let link_id = link["id"].as_str().unwrap().to_owned();
    let outputs = browser.link_status(&link_id, true, None);
    page.take(outputs);

    // A new vehicle with its defaults, on the ground.
    let mut vehicle = FakeVehicle::new(1, 0x5678_ef01);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(t_s + 2.0);
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        30,
    );

    let speed = vehicle
        .parameters
        .iter()
        .find(|(name, _, _)| name == "WPNAV_SPEED")
        .map(|(_, value, _)| *value);
    assert_eq!(speed, Some(1500.0), "its parameters came back");
    assert_eq!(vehicle.mission.len(), 3, "and its mission");
    assert!((vehicle.height_m - 40.0).abs() < 0.01);
    assert_eq!(vehicle.mode, 7, "back in CIRCLE");
    assert_eq!(launch_of(&browser, &link_id).stage, LaunchStage::Flying);
    assert!(browser.state().notifications.values().all(|n| n.state
        == uxduck_core::domain::notification::NotificationState::Resolved
        || !n.title.contains("stayed on the ground")));
}

#[test]
fn a_snapshot_starts_on_the_ground_renames_deletes_and_moves_with_the_work() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let mut t_s = 0.0;
    let snapshot = snapshot_of_a_flight(&mut browser, &mut page, &clock, &mut t_s);
    let id = snapshot["id"].as_str().unwrap().to_owned();

    let started = page.call(
        &mut browser,
        "snapshot/start",
        json!({ "id": id, "onGround": true }),
    );
    let link = &started["result"];
    assert!(link["kind"].get("inAirM").is_none(), "{started}");
    assert_eq!(link["launch"]["stage"], "restoring");
    let link_id = link["id"].as_str().unwrap().to_owned();
    let outputs = browser.link_status(&link_id, true, None);
    page.take(outputs);
    let mut vehicle = FakeVehicle::new(1, 0x5678_ef01);
    vehicle.on_ground = true;
    vehicle.prearm_from_s = Some(0.0);
    fly(
        &mut browser,
        &mut page,
        &mut vehicle,
        &link_id,
        &clock,
        &mut t_s,
        10,
    );
    assert!(!vehicle.armed, "it stays on the ground");
    assert_eq!(vehicle.mission.len(), 3);
    assert!(
        browser.state().links[&link_id].launch.is_none(),
        "nothing left to show"
    );

    // Renamed and moved with the rest of the browser's work.
    let renamed = page.call(
        &mut browser,
        "snapshot/rename",
        json!({ "id": id, "name": "Ridge, again" }),
    );
    assert_eq!(renamed["result"]["name"], "Ridge, again");
    let exported = browser.export();
    assert!(
        exported
            .iter()
            .any(|c| c.to_string().contains("Ridge, again")),
        "the snapshot goes with the work"
    );
    page.call(&mut browser, "snapshot/delete", json!({ "id": id }));
    assert!(browser.state().snapshots.is_empty());
    let missing = page.call(&mut browser, "snapshot/start", json!({ "id": id }));
    assert_eq!(missing["error"]["data"]["kind"], "not-found", "{missing}");
}

#[test]
fn missing_or_blank_catalogue_keys_report_public_verification_setup() {
    let clock = TestClock::default();
    for key in [None, Some(String::new()), Some("  ".into())] {
        let (mut browser, mut page) = start(&clock, None, "");
        browser.definitions_from(
            Some("https://definitions.example/catalogue.json".into()),
            key,
        );
        page.call(
            &mut browser,
            "param/definitions/list",
            json!({ "refresh": true }),
        );
        let result = page.call(&mut browser, "param/definitions/list", json!({}));
        assert_eq!(result["result"]["catalogue"], "failed", "{result}");
        assert_eq!(
            result["result"]["catalogueDetail"],
            uxduck_engine::param_definitions::files::NO_CATALOGUE_KEY
        );
        assert!(
            page.fetches.is_empty(),
            "untrusted catalogues are never fetched"
        );
    }
}

/// Parameter definitions (S67, ADR-0021 P4): the browser's daemon asks its
/// worker to fetch the signed catalogue and the set for a simulated
/// vehicle's version, and checks them itself.
#[test]
#[allow(clippy::too_many_lines, reason = "the demo in order")]
fn a_simulated_vehicle_is_described_from_the_catalogue_the_worker_fetches() {
    use std::io::Write as _;
    use uxduck_contracts::pdef;
    use uxduck_core::domain::param_definitions::{
        DefinitionSet, DefinitionSource, DefinitionsStatus, Firmware, Match,
    };

    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let url = "https://definitions.example/parameters/catalogue.json";
    browser.definitions_from(Some(url.into()), Some(pair.pk.to_base64()));

    let fixtures = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-contracts/fixtures/pdef"
    );
    let read = |name: &str| std::fs::read_to_string(format!("{fixtures}/{name}")).unwrap();
    let (_, xml) = pdef::from_xml(&read("copter.pdef.xml")).unwrap();
    let (_, json_defs) = pdef::from_json(&read("copter.pdef.json")).unwrap();
    let set = DefinitionSet {
        source: DefinitionSource::Ardupilot,
        firmware: Firmware::ArduCopter,
        version: Some("4.5.1".into()),
        commit: Some("0123456789".into()),
        parameters: pdef::merge(xml, &json_defs),
    };
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&serde_json::to_vec(&set).unwrap()).unwrap();
    let gz = gz.finish().unwrap();
    let set_url = "https://definitions.example/parameters/sets/arducopter-4.5.1.json.gz";
    let manifest = json!({
        "schema": 1,
        "publishedAt": "2026-10-01T00:00:00Z",
        "sets": [{
            "id": "arducopter-4.5.1",
            "firmware": "ArduCopter",
            "version": "4.5.1",
            "commit": "0123456789",
            "file": {
                "name": "arducopter-4.5.1.json.gz",
                "url": set_url,
                "size": gz.len(),
                "sha256": uxduck_engine::param_definitions::files::sha256(&gz)
            },
            "sourceUrl": "https://github.com/ArduPilot/ardupilot/tree/0123456789"
        }]
    })
    .to_string();
    let signature = minisign::sign(None, &pair.sk, manifest.as_bytes(), None, None)
        .unwrap()
        .into_string();

    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    page.take(browser.link_status(&link_id, true, None));
    // The fake says it runs 4.5.1.
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    page.take(browser.link_received(&link_id, &vehicle.heartbeat()));
    converse(&mut browser, &mut page, &mut vehicle, &link_id);

    let asked: Vec<String> = page.fetches.iter().map(|(_, u)| u.clone()).collect();
    assert_eq!(asked, [format!("{url}.minisig"), url.to_owned()]);
    let fetches = std::mem::take(&mut page.fetches);
    page.take(browser.fetched(fetches[0].0, Ok(signature.clone().into_bytes())));
    assert!(page.fetches.is_empty(), "waits for the manifest too");
    page.take(browser.fetched(fetches[1].0, Ok(manifest.clone().into_bytes())));
    let set_fetch = page.fetches.pop().expect("the set is fetched");
    assert_eq!(set_fetch.1, set_url);

    // A changed file is refused.
    let mut changed = gz.clone();
    changed[20] ^= 1;
    page.take(browser.fetched(set_fetch.0, Ok(changed)));
    let vehicle_id = browser.state().vehicles.keys().next().unwrap().clone();
    let chosen = |browser: &Browser| browser.state().param_definitions[&vehicle_id].clone();
    assert_eq!(chosen(&browser).status, DefinitionsStatus::Failed);
    assert!(
        chosen(&browser)
            .detail
            .unwrap()
            .contains("does not match the catalogue's checksum")
    );

    // A fresh visit, the worker answering well.
    let (mut browser, mut page) = start(&clock, None, "");
    browser.definitions_from(Some(url.into()), Some(pair.pk.to_base64()));
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    page.take(browser.link_status(&link_id, true, None));
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    page.take(browser.link_received(&link_id, &vehicle.heartbeat()));
    converse(&mut browser, &mut page, &mut vehicle, &link_id);
    let fetches = std::mem::take(&mut page.fetches);
    page.take(browser.fetched(fetches[0].0, Ok(signature.into_bytes())));
    page.take(browser.fetched(fetches[1].0, Ok(manifest.into_bytes())));
    let set_fetch = page.fetches.pop().unwrap();
    page.take(browser.fetched(set_fetch.0, Ok(gz)));
    let chosen = browser.state().param_definitions[&vehicle_id].clone();
    assert_eq!(chosen.status, DefinitionsStatus::Ready);
    assert_eq!(chosen.matched, Some(Match::Exact));
    assert_eq!(chosen.title.as_deref(), Some("Copter 4.5.1"));
}

/// The MAVLink inspector in the browser: what the simulator sent and what
/// was sent to it, decoded with ArduPilot's dialect table.
#[test]
fn the_inspector_shows_the_simulators_messages_decoded() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    page.take(browser.link_status(&link_id, true, None));
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    page.take(browser.link_received(&link_id, &vehicle.heartbeat()));
    converse(&mut browser, &mut page, &mut vehicle, &link_id);
    clock.advance_ms(250);
    page.take(browser.link_received(&link_id, &vehicle.heartbeat()));
    clock.advance_ms(250);
    let vehicle_id = browser.state().vehicles.values().next().unwrap().id.clone();

    let inspected = page.call(
        &mut browser,
        "mavlink/inspect",
        json!({ "vehicleId": vehicle_id }),
    );
    let result = &inspected["result"];
    assert_eq!(result["decoded"], true, "{inspected}");
    let messages = result["messages"].as_array().unwrap();
    let heartbeat = messages
        .iter()
        .find(|m| m["direction"] == "received" && m["name"] == "HEARTBEAT")
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(heartbeat["dialect"], "common");
    assert_eq!(heartbeat["fields"]["type"], 2, "a quadrotor");
    assert_eq!(heartbeat["fields"]["autopilot"], 3, "ArduPilot");
    assert_eq!(heartbeat["ageMs"], 250);
    assert!(heartbeat["count"].as_u64().unwrap() >= 2);
    let sent: Vec<&Value> = messages
        .iter()
        .filter(|m| m["direction"] == "sent")
        .collect();
    assert!(!sent.is_empty(), "{result}");
    assert!(
        sent.iter()
            .all(|m| m["systemId"] == 255 && m["fields"].is_object()),
        "{result}"
    );

    // Stepping back: the heartbeats newest first, a page at a time, from
    // memory; what was sent kept apart.
    let count = heartbeat["count"].as_u64().unwrap();
    let sent_id = sent[0]["id"].clone();
    let history = |page: &mut Page,
                   browser: &mut Browser,
                   direction: &str,
                   id: &Value,
                   before: Value| {
        page.call(
            browser,
            "mavlink/history",
            json!({ "vehicleId": vehicle_id, "direction": direction, "id": id, "before": before, "limit": 1 }),
        )["result"]
            .clone()
    };
    let first = history(&mut page, &mut browser, "received", &json!(0), Value::Null);
    assert_eq!(first["fromLog"], false, "{first}");
    let newest = &first["messages"][0];
    assert_eq!(newest["fields"]["type"], 2, "{first}");
    assert_eq!(newest["systemId"], 1);
    let older = first["older"].clone();
    assert!(older.is_string(), "{first}");
    let second = history(&mut page, &mut browser, "received", &json!(0), older);
    assert!(
        second["messages"][0]["atUs"].as_u64() < newest["atUs"].as_u64(),
        "{second}"
    );
    let all = page.call(
        &mut browser,
        "mavlink/history",
        json!({ "vehicleId": vehicle_id, "direction": "received", "id": 0 }),
    )["result"]
        .clone();
    assert_eq!(all["messages"].as_array().unwrap().len() as u64, count);
    assert!(all.get("older").is_none(), "{all}");
    let sent = history(&mut page, &mut browser, "sent", &sent_id, Value::Null);
    assert_eq!(sent["messages"][0]["systemId"], 255, "{sent}");
    let bad = page.call(
        &mut browser,
        "mavlink/history",
        json!({ "vehicleId": vehicle_id, "direction": "sent", "id": 0, "before": "nonsense" }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value", "{bad}");
}

/// UXDuck in the browser answers `daemon/about` with the wasm build's own
/// stamp (S87): no data folder or port, since it keeps its work in the
/// page's storage and listens on nothing.
#[test]
fn the_browser_says_which_uxduck_it_is_and_how_it_was_built() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    clock.advance_ms(90_000);
    let about = page.call(&mut browser, "daemon/about", json!({}));
    let result = &about["result"];
    assert_eq!(result["version"], "0.1.0", "{about}");
    assert_eq!(result["runsIn"], "browser");
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["target"], env!("UXDUCK_TARGET"));
    assert_eq!(
        result["commit"].as_str(),
        option_env!("UXDUCK_GIT_SHA"),
        "the commit built from, where git knows it"
    );
    assert!(result.get("dataDir").is_none());
    assert!(result.get("port").is_none());
    assert_eq!(result["uptimeUs"], 90_000_000);
    serde_json::from_value::<uxduck_contracts::dto::settings::DaemonAbout>(result.clone())
        .expect("a DaemonAbout");
}

#[test]
fn a_blimp_uses_its_own_program_and_survives_restart() {
    let clock = TestClock::default();
    let (mut browser, mut page) = start(&clock, None, "");
    let mut kind: Value = serde_json::from_str(PLACE).unwrap();
    kind["vehicle"] = json!("airship");
    kind["inAirM"] = json!(50);
    let refused = page.call(&mut browser, "link/add", json!({ "kind": kind }));
    assert!(refused["error"].is_object());
    kind.as_object_mut().unwrap().remove("inAirM");
    kind["autopilot"] = json!("px4");
    let refused = page.call(&mut browser, "link/add", json!({ "kind": kind }));
    assert!(refused["error"].is_object());
    kind.as_object_mut().unwrap().remove("autopilot");
    let added = page.call(&mut browser, "link/add", json!({ "kind": kind }));
    assert_eq!(added["result"]["kind"]["vehicle"], "airship", "{added}");
    assert!(
        matches!(&page.simulator[..], [Order::Start { program: "blimp", model, .. }] if model == "blimp")
    );
    let id = added["result"]["id"].as_str().unwrap();
    page.simulator.clear();
    page.call(&mut browser, "sim/restart", json!({ "id": id }));
    assert!(
        matches!(&page.simulator[..], [Order::Stop { .. }, Order::Start { program: "blimp", model, .. }] if model == "blimp")
    );
}

/// The bytes each save address was made for, as the worker's blob URLs.
static SAVED: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

fn save_url(bytes: &[u8]) -> String {
    let mut saved = SAVED.lock().unwrap();
    saved.push(bytes.to_vec());
    format!("blob:test/{}", saved.len())
}

fn revoke_url(_: &str) {}

#[test]
fn a_vehicles_file_downloads_into_the_browser_and_saves_from_a_blob() {
    let clock = TestClock::default();
    clock.advance_ms(1_700_000_000_000);
    let (mut browser, mut page) = start(&clock, None, "");
    let place: Value = serde_json::from_str(PLACE).unwrap();
    let link = page.call(&mut browser, "link/add", json!({ "kind": place }));
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    page.take(browser.link_status(&link_id, true, None));
    let mut vehicle = FakeVehicle::new(1, 0x1234_abcd);
    page.take(browser.link_received(&link_id, &vehicle.heartbeat()));
    converse(&mut browser, &mut page, &mut vehicle, &link_id);
    let vehicle_id = browser.state().vehicles.values().next().unwrap().id.clone();

    // Without a place for copies, as before: UXDuck Desktop's.
    let refused = page.call(
        &mut browser,
        "files/download",
        json!({ "vehicleId": vehicle_id, "path": "/APM/LOGS/00000002.BIN" }),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("UXDuck Desktop")
    );

    page.take(browser.keep_files(|bytes| Some(save_url(bytes)), revoke_url));
    let started = page.call(
        &mut browser,
        "files/download",
        json!({ "vehicleId": vehicle_id, "path": "/APM/LOGS/00000002.BIN" }),
    );
    let id = started["result"]["id"].as_str().unwrap().to_owned();
    for _ in 0..200 {
        converse(&mut browser, &mut page, &mut vehicle, &link_id);
        if browser.state().file_transfers[&id].state
            == uxduck_core::domain::files::TransferState::Done
        {
            break;
        }
        clock.advance_ms(100);
        page.take(browser.tick());
    }
    let saved = page.call(&mut browser, "files/save", json!({ "id": id }));
    let url = saved["result"]["url"].as_str().unwrap();
    assert!(url.starts_with("blob:test/"), "{saved}");
    assert_eq!(saved["result"]["name"], "00000002.BIN");
    let expected =
        uxduck_mavlink::fake_files::FakeFiles::default().files["/APM/LOGS/00000002.BIN"].clone();
    assert_eq!(SAVED.lock().unwrap().last().unwrap(), &expected);
}

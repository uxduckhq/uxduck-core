//! Markers on the map (S177, ADR-0035): named things besides vehicles,
//! the same in every window, read by the agent, changed with `config`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::float_cmp,
    reason = "test helpers"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{Clock, Forget, Ports, Random};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

#[derive(Clone, Default)]
struct FakeClock(Arc<AtomicU64>);

impl Clock for FakeClock {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Counter(u8);

impl Random for Counter {
    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_add(37);
            *byte = self.0;
        }
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
}

fn engine() -> Engine {
    let clock = FakeClock::default();
    clock.0.store(1_791_720_000_000_000, Ordering::SeqCst);
    Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:45000/".into(),
            checkpoint_every: 1000,
            models: vec!["test-model".into()],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(clock),
        Box::new(Counter(0)),
        Ports::with_persistence(Box::new(Forget)),
        Loaded::default(),
    )
}

fn join(engine: &mut Engine, kind: &str) -> Client {
    let (outbox, inbox) = unbounded();
    let mut client = Client {
        id: engine.connect(Transport::Socket, Trust::LocalUser, outbox),
        inbox,
    };
    call(
        engine,
        &mut client,
        "initialize",
        json!({ "clientInfo": { "name": "test", "version": "0.1.0", "kind": kind }, "capabilities": { "protocolVersion": 1 } }),
    );
    client
}

/// Every frame waiting, in order.
fn frames(client: &mut Client) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(text) = client.inbox.try_recv() {
        frames.push(serde_json::from_str(&text).expect("JSON"));
    }
    frames
}

/// The answer to one call; notifications sent before it are dropped.
fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
    );
    frames(client)
        .into_iter()
        .find(|frame| frame.get("id").is_some())
        .expect("an answer")
}

fn pushed(client: &mut Client, method: &str) -> Vec<Value> {
    frames(client)
        .into_iter()
        .filter(|frame| frame["method"] == method)
        .map(|frame| frame["params"].clone())
        .collect()
}

fn subscribe(engine: &mut Engine, client: &mut Client) -> Value {
    call(
        engine,
        client,
        "state/subscribe",
        json!({ "entities": ["marker"] }),
    )
}

#[test]
fn a_marker_added_in_one_window_reaches_the_others_and_every_new_subscription() {
    let mut engine = engine();
    let mut one = join(&mut engine, "operator-ui");
    let mut two = join(&mut engine, "operator-ui");
    subscribe(&mut engine, &mut two);

    let added = call(
        &mut engine,
        &mut one,
        "marker/add",
        json!({ "look": "flag", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    let marker = added["result"].clone();
    assert_eq!(marker["name"], "Flag", "named after its look");
    assert_eq!(marker["version"], 1);
    assert!(marker.get("headingDeg").is_none());

    let deltas = pushed(&mut two, "state/delta");
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0]["changes"][0]["entity"], "marker");
    assert_eq!(deltas[0]["changes"][0]["marker"]["name"], "Flag");

    let renamed = call(
        &mut engine,
        &mut one,
        "marker/update",
        json!({ "id": marker["id"], "expectedVersion": 1, "name": "  Launch site ", "headingDeg": 360 }),
    );
    assert_eq!(renamed["result"]["name"], "Launch site");
    assert_eq!(renamed["result"]["headingDeg"], 0.0, "360° is north");
    assert_eq!(renamed["result"]["latitudeDeg"], -35.36, "not moved");

    let stale = call(
        &mut engine,
        &mut two,
        "marker/update",
        json!({ "id": marker["id"], "expectedVersion": 1, "name": "Old" }),
    );
    assert_eq!(stale["error"]["data"]["kind"], "version-conflict");
    assert_eq!(stale["error"]["data"]["currentVersion"], 2);

    let moved = call(
        &mut engine,
        &mut two,
        "marker/update",
        json!({ "id": marker["id"], "expectedVersion": 2, "latitudeDeg": -35.37, "longitudeDeg": 149.17, "headingDeg": null }),
    );
    assert_eq!(moved["result"]["latitudeDeg"], -35.37);
    assert!(
        moved["result"].get("headingDeg").is_none(),
        "heading taken away"
    );

    let snapshot = subscribe(&mut engine, &mut one);
    assert_eq!(
        snapshot["result"]["snapshot"]["markers"][0]["name"],
        "Launch site"
    );
    assert_eq!(engine.state().markers.len(), 1);

    let removed = call(
        &mut engine,
        &mut one,
        "marker/remove",
        json!({ "id": marker["id"] }),
    );
    assert_eq!(removed["result"], json!({}));
    let deltas = pushed(&mut two, "state/delta");
    assert_eq!(deltas.last().unwrap()["changes"][0]["op"], "remove");
    let listed = call(&mut engine, &mut one, "marker/list", json!({}));
    assert_eq!(listed["result"]["markers"], json!([]));
}

#[test]
fn a_marker_needs_a_name_a_place_on_the_globe_and_a_heading_within_a_turn() {
    let mut engine = engine();
    let mut window = join(&mut engine, "operator-ui");
    for params in [
        json!({ "latitudeDeg": 91.0, "longitudeDeg": 0.0 }),
        json!({ "latitudeDeg": 0.0, "longitudeDeg": -181.0 }),
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "name": "   " }),
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "name": "x".repeat(101) }),
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "headingDeg": -1.0 }),
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "headingDeg": 361.0 }),
    ] {
        let refused = call(&mut engine, &mut window, "marker/add", params.clone());
        assert_eq!(
            refused["error"]["data"]["kind"], "invalid-value",
            "{params}"
        );
    }
    let added = call(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0 }),
    );
    let id = added["result"]["id"].clone();
    let half = call(
        &mut engine,
        &mut window,
        "marker/update",
        json!({ "id": id, "expectedVersion": 1, "latitudeDeg": 1.0 }),
    );
    assert_eq!(half["error"]["data"]["kind"], "invalid-value");
    let missing = call(
        &mut engine,
        &mut window,
        "marker/remove",
        json!({ "id": "mk-none" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");
    assert_eq!(engine.state().markers.len(), 1);
}

#[test]
fn the_agent_reads_markers_and_asks_before_adding_one() {
    let mut engine = engine();
    let mut window = join(&mut engine, "operator-ui");
    let mut agent = join(&mut engine, "agent");
    call(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "Boat", "look": "boat", "latitudeDeg": 50.0, "longitudeDeg": -1.0, "headingDeg": 90.0 }),
    );
    let listed = call(&mut engine, &mut agent, "marker/list", json!({}));
    assert_eq!(listed["result"]["markers"][0]["name"], "Boat");
    assert_eq!(listed["result"]["markers"][0]["headingDeg"], 90.0);

    engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "marker/add", "params": { "name": "Ramp", "latitudeDeg": 50.1, "longitudeDeg": -1.1 } })
            .to_string(),
    );
    assert!(
        frames(&mut agent).iter().all(|f| f.get("id").is_none()),
        "held for a person"
    );
    let approval = engine
        .state()
        .approvals
        .values()
        .find(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
        .cloned()
        .expect("an approval");
    assert_eq!(approval.summary, "add the marker Ramp to the map");
    assert_eq!(engine.state().markers.len(), 1, "nothing added yet");
}

/// A call's answer, and the `marker/live` notifications sent with it.
fn call_live(
    engine: &mut Engine,
    client: &mut Client,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
    );
    let all = frames(client);
    let answer = all
        .iter()
        .find(|frame| frame.get("id").is_some())
        .cloned()
        .expect("an answer");
    let live = all
        .into_iter()
        .filter(|frame| frame["method"] == "marker/live")
        .map(|frame| frame["params"].clone())
        .collect();
    (answer, live)
}

type Asked = Vec<Vec<(String, uxduck_core::domain::marker::MarkerSource)>>;

/// The receivers the engine asked the daemon to read, as it last asked.
#[derive(Clone, Default)]
struct Receivers(std::sync::Arc<std::sync::Mutex<Asked>>);

impl uxduck_engine::markers::MarkerSourcePort for Receivers {
    fn watch(&mut self, sources: Vec<(String, uxduck_core::domain::marker::MarkerSource)>) {
        self.0.lock().unwrap().push(sources);
    }
}

fn fix(latitude_deg: f64, heading_deg: Option<f64>) -> uxduck_engine::nmea::Fix {
    uxduck_engine::nmea::Fix {
        latitude_deg,
        longitude_deg: -1.0,
        heading_deg,
        satellites: Some(9),
        accuracy_m: Some(4.5),
    }
}

#[test]
fn a_marker_follows_its_gps_receiver_in_memory_and_keeps_its_last_position_by_hand() {
    let mut engine = engine();
    let receivers = Receivers::default();
    engine.set_marker_sources(Box::new(receivers.clone()));
    let mut window = join(&mut engine, "operator-ui");
    let mut agent = join(&mut engine, "agent");
    subscribe(&mut engine, &mut window);
    subscribe(&mut engine, &mut agent);

    let (boat, waiting) = call_live(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "Boat", "look": "boat", "latitudeDeg": 50.0, "longitudeDeg": -1.0,
                "source": { "type": "serial", "port": "/dev/cu.usbmodem1" } }),
    );
    let boat = boat["result"].clone();
    let id = boat["id"].as_str().unwrap().to_owned();
    assert_eq!(boat["source"]["type"], "serial");
    let asked = receivers.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(asked.len(), 1, "the daemon reads it");
    assert_eq!(waiting.last().unwrap()["live"]["state"], "waiting");

    engine.marker_fix(&id, &fix(50.1, Some(271.0)));
    let live = pushed(&mut window, "marker/live");
    assert_eq!(live.last().unwrap()["live"]["state"], "live");
    assert_eq!(live.last().unwrap()["live"]["fix"]["latitudeDeg"], 50.1);
    assert_eq!(live.last().unwrap()["live"]["fix"]["satellites"], 9);
    // Where people are is not the agent's to see without asking (ADR-0030 L4).
    assert!(pushed(&mut agent, "marker/live").is_empty());
    let listed = call(&mut engine, &mut agent, "marker/list", json!({}));
    assert!(listed["result"]["markers"][0].get("live").is_none());
    let listed = call(&mut engine, &mut window, "marker/list", json!({}));
    assert_eq!(
        listed["result"]["markers"][0]["live"]["fix"]["latitudeDeg"],
        50.1
    );
    // In memory only: the kept position has not moved, nor its version.
    assert_eq!(engine.state().markers[&id].latitude_deg, 50.0);
    assert_eq!(engine.state().markers[&id].version, 1);

    // A second fix within half a second is held back; the state is not.
    engine.marker_fix(&id, &fix(50.2, Some(271.0)));
    assert!(pushed(&mut window, "marker/live").is_empty());
    engine.marker_source_failed(
        &id,
        "The GPS receiver on /dev/cu.usbmodem1 was unplugged.".into(),
    );
    let failed = pushed(&mut window, "marker/live");
    assert_eq!(failed.last().unwrap()["live"]["state"], "failed");
    assert_eq!(
        failed.last().unwrap()["live"]["fix"]["latitudeDeg"],
        50.2,
        "last seen kept"
    );

    // "Keep it here": by hand, where it was last seen, facing as it was.
    let (kept, gone) = call_live(
        &mut engine,
        &mut window,
        "marker/update",
        json!({ "id": id, "expectedVersion": 1, "source": { "type": "manual" } }),
    );
    assert_eq!(kept["result"]["latitudeDeg"], 50.2);
    assert_eq!(kept["result"]["headingDeg"], 271.0);
    assert!(kept["result"].get("source").is_none());
    assert_eq!(
        receivers.0.lock().unwrap().last().unwrap().len(),
        0,
        "no longer read"
    );
    assert!(gone.last().unwrap().get("live").is_none());
    // A fix from a receiver no longer followed is ignored.
    engine.marker_fix(&id, &fix(60.0, None));
    assert!(pushed(&mut window, "marker/live").is_empty());
}

#[test]
fn a_marker_on_this_computer_follows_its_location_and_says_when_it_is_off() {
    let mut engine = engine();
    let mut tray = join(&mut engine, "operator-cli");
    call(&mut engine, &mut tray, "tray/menu", json!({}));
    let mut window = join(&mut engine, "operator-ui");
    subscribe(&mut engine, &mut window);
    let (_, off) = call_live(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "You", "look": "person", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
                "source": { "type": "computer" } }),
    );
    assert_eq!(off.last().unwrap()["live"]["state"], "waiting");
    assert!(
        off.last().unwrap()["live"]["reason"]
            .as_str()
            .unwrap()
            .contains("Show your location")
    );

    call(&mut engine, &mut window, "location/start", json!({}));
    call(
        &mut engine,
        &mut tray,
        "tray/location",
        json!({ "state": "on", "latitudeDeg": 51.5, "longitudeDeg": -0.12, "accuracyM": 30.0 }),
    );
    let on = pushed(&mut window, "marker/live");
    assert_eq!(on.last().unwrap()["live"]["state"], "live");
    assert_eq!(on.last().unwrap()["live"]["fix"]["latitudeDeg"], 51.5);
    assert_eq!(on.last().unwrap()["live"]["fix"]["accuracyM"], 30.0);
}

#[test]
fn a_gps_receiver_is_refused_without_a_port_or_at_an_odd_speed_and_failed_without_a_reader() {
    let mut engine = engine();
    let mut window = join(&mut engine, "operator-ui");
    subscribe(&mut engine, &mut window);
    for source in [
        json!({ "type": "serial", "port": " " }),
        json!({ "type": "serial", "port": "/dev/ttyUSB0", "baud": 1200 }),
        json!({ "type": "udp", "port": 0 }),
    ] {
        let refused = call(
            &mut engine,
            &mut window,
            "marker/add",
            json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "source": source.clone() }),
        );
        assert_eq!(
            refused["error"]["data"]["kind"], "invalid-value",
            "{source}"
        );
    }
    // No reader: the browser's UXDuck, say.
    let (_, live) = call_live(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "latitudeDeg": 0.0, "longitudeDeg": 0.0, "source": { "type": "udp", "port": 10110 } }),
    );
    assert_eq!(live.last().unwrap()["live"]["state"], "failed");
}

#[test]
fn one_device_shares_a_marker_until_it_stops_or_goes_and_another_waits_its_turn() {
    let mut engine = engine();
    let mut desktop = join(&mut engine, "operator-ui");
    let mut phone = join(&mut engine, "operator-ui");
    let mut tablet = join(&mut engine, "operator-ui");
    subscribe(&mut engine, &mut desktop);
    let (added, waiting) = call_live(
        &mut engine,
        &mut desktop,
        "marker/add",
        json!({ "name": "Jo", "look": "person", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
                "source": { "type": "device" } }),
    );
    let id = added["result"]["id"].clone();
    assert_eq!(waiting.last().unwrap()["live"]["state"], "waiting");

    let share = json!({ "id": id, "latitudeDeg": 51.5, "longitudeDeg": -0.12,
                        "headingDeg": 200.0, "accuracyM": 8.0, "device": "iPhone" });
    let shared = call(&mut engine, &mut phone, "marker/share", share.clone());
    assert_eq!(shared["result"], json!({}));
    let live = pushed(&mut desktop, "marker/live");
    assert_eq!(live.last().unwrap()["live"]["state"], "live");
    assert_eq!(live.last().unwrap()["live"]["from"], "iPhone");
    assert_eq!(live.last().unwrap()["live"]["fix"]["headingDeg"], 200.0);

    // Another device waits while the phone shares.
    let refused = call(
        &mut engine,
        &mut tablet,
        "marker/share",
        json!({ "id": id, "latitudeDeg": 1.0, "longitudeDeg": 1.0, "device": "iPad" }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "busy");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("iPhone")
    );

    // Stopping from another window does nothing; the phone's own stop does.
    call(
        &mut engine,
        &mut tablet,
        "marker/unshare",
        json!({ "id": id }),
    );
    assert!(pushed(&mut desktop, "marker/live").is_empty());
    call(
        &mut engine,
        &mut phone,
        "marker/unshare",
        json!({ "id": id }),
    );
    let stopped = pushed(&mut desktop, "marker/live");
    assert_eq!(stopped.last().unwrap()["live"]["state"], "failed");
    assert_eq!(
        stopped.last().unwrap()["live"]["reason"],
        "iPhone stopped sharing."
    );
    assert_eq!(stopped.last().unwrap()["live"]["fix"]["latitudeDeg"], 51.5);

    // Now the tablet may, and its going away stops it.
    let taken = call(
        &mut engine,
        &mut tablet,
        "marker/share",
        json!({ "id": id, "latitudeDeg": 1.0, "longitudeDeg": 1.0, "device": "iPad" }),
    );
    assert_eq!(taken["result"], json!({}));
    engine.disconnect(tablet.id);
    let gone = pushed(&mut desktop, "marker/live");
    assert!(
        gone.last().unwrap()["live"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("iPad stopped sharing")
    );

    // A marker set by hand cannot be shared.
    call(
        &mut engine,
        &mut desktop,
        "marker/update",
        json!({ "id": id, "expectedVersion": 1, "source": { "type": "manual" } }),
    );
    let refused = call(&mut engine, &mut phone, "marker/share", share);
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
}

#[test]
fn an_operator_says_which_marker_is_them_and_can_stop() {
    let mut engine = engine();
    let mut window = join(&mut engine, "operator-ui");
    let ana = call(
        &mut engine,
        &mut window,
        "operator/create",
        json!({ "name": "Ana" }),
    )["result"]
        .clone();
    let boat = call(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "Boat", "latitudeDeg": 50.0, "longitudeDeg": -1.0 }),
    )["result"]
        .clone();
    let me = call(
        &mut engine,
        &mut window,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": ana["version"], "meMarkerId": boat["id"] }),
    );
    assert_eq!(me["result"]["meMarkerId"], boat["id"]);
    let missing = call(
        &mut engine,
        &mut window,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": me["result"]["version"], "meMarkerId": "mk-none" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");
    let none = call(
        &mut engine,
        &mut window,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": me["result"]["version"], "meMarkerId": null }),
    );
    assert!(none["result"].get("meMarkerId").is_none(), "{none}");
}

#[test]
fn watch_links_show_the_markers_left_on_where_each_is_shown_now() {
    let mut engine = engine();
    let receivers = Receivers::default();
    engine.set_marker_sources(Box::new(receivers));
    let mut window = join(&mut engine, "operator-ui");
    let flag = call(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "Launch site", "look": "flag", "latitudeDeg": 50.0, "longitudeDeg": -1.0, "headingDeg": 270.0 }),
    )["result"]
        .clone();
    let boat = call(
        &mut engine,
        &mut window,
        "marker/add",
        json!({ "name": "Boat", "look": "boat", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
                "source": { "type": "udp", "port": 10110 } }),
    )["result"]
        .clone();
    engine.marker_fix(boat["id"].as_str().unwrap(), &fix(50.2, Some(90.0)));

    let shown = engine.edge_markers();
    assert_eq!(shown.len(), 2);
    let boat_shown = shown.iter().find(|m| m.name == "Boat").unwrap();
    assert_eq!(boat_shown.latitude_deg, 50.2, "where its source put it");
    assert_eq!(boat_shown.heading_deg, Some(90.0));
    assert!(boat_shown.fix_at_ms.is_some());
    let flag_shown = shown.iter().find(|m| m.name == "Launch site").unwrap();
    assert_eq!(flag_shown.heading_deg, Some(270.0));
    assert!(flag_shown.fix_at_ms.is_none(), "set by hand");

    let off = call(
        &mut engine,
        &mut window,
        "marker/update",
        json!({ "id": flag["id"], "expectedVersion": 1, "watchLinks": false }),
    );
    assert_eq!(off["result"]["watchLinks"], false);
    assert_eq!(engine.edge_markers().len(), 1);
}

#[test]
fn turning_off_account_watching_hides_all_markers() {
    let mut engine = engine();
    engine.set_marker_sources(Box::new(Receivers::default()));
    let mut window = join(&mut engine, "operator-ui");
    for marker in [
        json!({ "name": "Launch site", "latitudeDeg": 50.0, "longitudeDeg": -1.0 }),
        json!({ "name": "Boat", "latitudeDeg": 0.0, "longitudeDeg": 0.0,
            "source": { "type": "udp", "port": 10110 } }),
    ] {
        let added = call(&mut engine, &mut window, "marker/add", marker);
        assert!(added.get("error").is_none(), "{added}");
    }
    assert_eq!(engine.edge_markers().len(), 2);
    let off = call(
        &mut engine,
        &mut window,
        "settings/update",
        json!({ "expectedVersion": 0, "watchFromAccount": false }),
    );
    assert!(off.get("error").is_none(), "{off}");
    assert!(engine.edge_markers().is_empty(), "global watching is off");
    let on = call(
        &mut engine,
        &mut window,
        "settings/update",
        json!({ "expectedVersion": off["result"]["version"], "watchFromAccount": true }),
    );
    assert!(on.get("error").is_none(), "{on}");
    assert_eq!(engine.edge_markers().len(), 2);
}

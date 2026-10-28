//! Other aircraft near the vehicles (S168, ADR-0034): each vehicle's
//! receiver is a source of its own, listed whether or not it is used, off
//! until a person turns it on, and checked apart from the others.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::traffic::{TrafficCategory, TrafficProtocol, TrafficReport};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{Clock, Forget, Ports, Random};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const START_US: u64 = 1_791_720_000_000_000;

#[derive(Clone, Default)]
struct FakeClock(Arc<AtomicU64>);

impl Clock for FakeClock {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl FakeClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn advance(&self, us: u64) {
        self.0.fetch_add(us, Ordering::SeqCst);
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

fn engine() -> (Engine, FakeClock) {
    let clock = FakeClock::default();
    clock.0.store(START_US, Ordering::SeqCst);
    let engine = Engine::new(
        EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:45000/".into(),
            checkpoint_every: 1000,
            models: vec!["test-model".into()],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        Ports::with_persistence(Box::new(Forget)),
        Loaded::default(),
    );
    (engine, clock)
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

fn frames(client: &mut Client) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(text) = client.inbox.try_recv() {
        frames.push(serde_json::from_str(&text).expect("JSON"));
    }
    frames
}

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

/// Two vehicles on one link, as `vehicle/list` names them Alpha and Bravo.
fn two_vehicles(engine: &mut Engine, ui: &mut Client) {
    let link = call(
        engine,
        ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    let link_id = link["result"]["id"].as_str().unwrap().to_owned();
    for (id, system_id) in [("alpha", 1), ("bravo", 2)] {
        engine.vehicle_observed(Observation {
            id: id.into(),
            aliases: Vec::new(),
            link_id: link_id.clone(),
            peer: "127.0.0.1:5760".into(),
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
}

fn aircraft(address: u32, lat: f64, lon: f64, heard_us: u64) -> TrafficReport {
    TrafficReport {
        address,
        callsign: Some(format!("T{address}")),
        squawk: Some(1200),
        category: TrafficCategory::Light,
        protocol: TrafficProtocol::Adsb,
        position: Some((lat, lon)),
        altitude_m: Some(600.0),
        altitude_frame: Some(uxduck_core::domain::traffic::AltitudeFrame::Geometric),
        track_deg: Some(90.0),
        ground_speed_mps: Some(50.0),
        climb_mps: None,
        simulated: true,
        heard_us,
    }
}

fn status(engine: &mut Engine, client: &mut Client) -> Value {
    call(engine, client, "traffic/status", json!({}))["result"].clone()
}

fn source<'v>(status: &'v Value, id: &str) -> &'v Value {
    status["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no source {id} in {status}"))
}

#[test]
fn a_vehicles_traffic_is_listed_but_used_only_once_a_person_turns_it_on() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    let now = clock.now();
    for address in 1..=3 {
        engine.vehicle_traffic("alpha", aircraft(address, 51.5, -0.1, now));
    }

    let before = status(&mut engine, &mut ui);
    let mut window = join(&mut engine, "operator-ui");
    status(&mut engine, &mut window);
    assert_eq!(before["aircraft"], json!([]));
    let alpha = source(&before, "vehicle:alpha");
    assert_eq!(alpha["on"], false);
    assert_eq!(alpha["state"], "hearing");
    assert_eq!(alpha["aircraft"], 3);
    assert_eq!(before["sources"].as_array().unwrap().len(), 1);

    let mut agent = join(&mut engine, "agent");
    let refused = call(
        &mut engine,
        &mut agent,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": true }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "capability-denied",
        "{refused}"
    );

    let turned = call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": true }),
    );
    assert_eq!(turned["result"]["traffic"], true);
    let changed = pushed(&mut window, "traffic/changed");
    let last = changed.last().expect("traffic/changed");
    assert_eq!(last["aircraft"].as_array().unwrap().len(), 3);
    let first = &last["aircraft"][0];
    assert_eq!(first["address"], "000001");
    assert_eq!(first["squawk"], "1200");
    assert_eq!(first["sources"], json!(["vehicle:alpha"]));
    assert_eq!(first["simulated"], true);

    let listed = call(
        &mut engine,
        &mut ui,
        "vehicle/get",
        json!({ "id": "alpha" }),
    );
    assert_eq!(listed["result"]["traffic"], true);

    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": false }),
    );
    assert_eq!(status(&mut engine, &mut ui)["aircraft"], json!([]));
}

#[test]
fn bad_reports_are_refused_and_counted_against_their_own_source() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    for id in ["alpha", "bravo"] {
        call(
            &mut engine,
            &mut ui,
            "vehicle/traffic",
            json!({ "id": id, "on": true }),
        );
    }
    let now = clock.now();
    engine.vehicle_traffic("alpha", aircraft(1, 0.0, 0.0, now));
    let mut high = aircraft(2, 51.5, -0.1, now);
    high.altitude_m = Some(30_000.0);
    engine.vehicle_traffic("alpha", high);
    engine.vehicle_traffic("alpha", aircraft(3, 51.5, -0.1, now));
    // 100 km in a second.
    clock.advance(1_000_000);
    engine.vehicle_traffic("alpha", aircraft(3, 52.4, -0.1, clock.now()));
    engine.vehicle_traffic("bravo", aircraft(9, 51.6, -0.2, clock.now()));

    let now = status(&mut engine, &mut ui);
    let alpha = source(&now, "vehicle:alpha");
    assert_eq!(
        alpha["refused"],
        json!([
            { "reason": "position", "count": 1 },
            { "reason": "altitude", "count": 1 },
            { "reason": "jump", "count": 1 },
        ])
    );
    assert!(source(&now, "vehicle:bravo").get("refused").is_none());
    let addresses: Vec<&str> = now["aircraft"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["address"].as_str().unwrap())
        .collect();
    assert_eq!(addresses, ["000003", "000009"]);
    assert_eq!(now["aircraft"][0]["latitudeDeg"], 51.5);
}

#[test]
fn a_flooding_source_is_paused_and_named_while_the_others_carry_on() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    for id in ["alpha", "bravo"] {
        call(
            &mut engine,
            &mut ui,
            "vehicle/traffic",
            json!({ "id": id, "on": true }),
        );
    }
    let now = clock.now();
    engine.vehicle_traffic("bravo", aircraft(9, 51.6, -0.2, now));
    for i in 0..=2_000 {
        engine.vehicle_traffic("alpha", aircraft(1 + i % 400, 51.5, -0.1, now));
    }

    let paused = status(&mut engine, &mut ui);
    assert_eq!(source(&paused, "vehicle:alpha")["state"], "paused");
    assert_eq!(source(&paused, "vehicle:alpha")["aircraft"], 0);
    assert_eq!(paused["aircraft"].as_array().unwrap().len(), 1);
    assert_eq!(paused["aircraft"][0]["address"], "000009");
    let warned = engine
        .state()
        .notifications
        .values()
        .find(|n| n.condition.as_deref() == Some("traffic-flood:vehicle:alpha"))
        .expect("a warning naming Alpha");
    assert_eq!(warned.title, "Alpha's traffic is paused");

    clock.advance(31_000_000);
    engine.expire_traffic(clock.now());
    engine.vehicle_traffic("alpha", aircraft(1, 51.5, -0.1, clock.now()));
    let resumed = status(&mut engine, &mut ui);
    assert_eq!(source(&resumed, "vehicle:alpha")["state"], "hearing");
    assert!(
        resumed["aircraft"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["address"] == "000001")
    );
    assert!(
        !engine.state().notifications.values().any(|n| {
            n.condition.as_deref() == Some("traffic-flood:vehicle:alpha")
                && serde_json::to_value(n.state).unwrap() == json!("active")
        }),
        "the warning clears once Alpha is heard again"
    );
}

#[test]
fn an_aircraft_two_vehicles_hear_is_one_aircraft_at_its_freshest() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    for id in ["alpha", "bravo"] {
        call(
            &mut engine,
            &mut ui,
            "vehicle/traffic",
            json!({ "id": id, "on": true }),
        );
    }
    let now = clock.now();
    engine.vehicle_traffic("alpha", aircraft(7, 51.50, -0.1, now - 2_000_000));
    engine.vehicle_traffic("bravo", aircraft(7, 51.51, -0.1, now));

    let picture = status(&mut engine, &mut ui);
    assert_eq!(picture["aircraft"].as_array().unwrap().len(), 1);
    assert_eq!(picture["aircraft"][0]["latitudeDeg"], 51.51);
    assert_eq!(
        picture["aircraft"][0]["sources"],
        json!(["vehicle:alpha", "vehicle:bravo"])
    );
}

#[test]
fn aircraft_turn_grey_after_ten_seconds_and_go_after_a_minute() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": true }),
    );
    status(&mut engine, &mut ui);
    engine.vehicle_traffic("alpha", aircraft(1, 51.5, -0.1, clock.now()));

    clock.advance(11_000_000);
    engine.expire_traffic(clock.now());
    let grey = pushed(&mut ui, "traffic/changed");
    assert_eq!(grey.last().unwrap()["aircraft"][0]["stale"], true);

    clock.advance(50_000_000);
    engine.expire_traffic(clock.now());
    let gone = status(&mut engine, &mut ui);
    assert_eq!(gone["aircraft"], json!([]));
    assert_eq!(source(&gone, "vehicle:alpha")["state"], "quiet");
    assert_eq!(source(&gone, "vehicle:alpha")["aircraft"], 0);
}

fn at(engine: &mut Engine, id: &str, lat: f64, lon: f64, amsl_m: f64) {
    use uxduck_core::domain::telemetry::{
        Altitude, AltitudeFrame, PositionEstimate, VehicleTelemetry,
    };
    engine.telemetry_observed(VehicleTelemetry {
        vehicle_id: id.into(),
        positions: vec![PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: lat,
            longitude_deg: lon,
            altitudes: vec![Altitude {
                meters: amsl_m,
                frame: AltitudeFrame::Amsl,
            }],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: START_US,
        }],
        items: vec![],
    });
}

#[test]
fn our_own_vehicles_are_named_never_strangers() {
    use uxduck_core::domain::parameter::ParamKind;
    use uxduck_engine::transfers::Reported;

    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": true }),
    );
    // Bravo sends ADS-B out with its own address, and flies 2 km away.
    engine.parameter_reported(
        "bravo",
        Reported {
            name: "ADSB_ICAO_ID".into(),
            value: f64::from(0x00AB_CDEF_u32),
            kind: ParamKind::Int32,
        },
        false,
    );
    at(&mut engine, "bravo", 51.52, -0.1, 650.0);
    // Something without a known address, just where Bravo is.
    at(&mut engine, "alpha", 51.5, -0.1, 600.0);
    let now = clock.now();
    engine.vehicle_traffic("alpha", aircraft(0x00AB_CDEF, 51.52, -0.1, now));
    let mut beside = aircraft(5, 51.520_1, -0.1, now);
    beside.altitude_m = Some(640.0);
    engine.vehicle_traffic("alpha", beside);
    engine.vehicle_traffic("alpha", aircraft(6, 51.53, -0.1, now));

    let picture = status(&mut engine, &mut ui);
    let by: Vec<(&str, &Value, &Value)> = picture["aircraft"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["address"].as_str().unwrap(), &a["vehicleId"], &a["own"]))
        .collect();
    assert_eq!(
        by,
        [
            ("000005", &json!("bravo"), &json!("nearby")),
            ("000006", &Value::Null, &Value::Null),
            ("ABCDEF", &json!("bravo"), &json!("address")),
        ]
    );
}

#[test]
fn a_receiver_on_the_network_is_a_source_of_its_own_on_its_link() {
    use uxduck_core::domain::link::LinkStatus;
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    let wrong = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 30003 }, "carries": "sbs" }),
    );
    assert_eq!(wrong["error"]["data"]["kind"], "invalid-value", "{wrong}");
    let added = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "name": "Roof Pi", "kind": { "type": "tcp-client", "host": "pi.local", "port": 30003 }, "carries": "sbs" }),
    );
    let link = &added["result"];
    assert_eq!(link["carries"], "sbs");
    assert_eq!(link["policy"], "read", "nothing is sent to a receiver");
    let link_id = link["id"].as_str().unwrap().to_owned();
    let source_id = format!("receiver:{link_id}");

    let waiting = status(&mut engine, &mut ui);
    let row = source(&waiting, &source_id);
    assert_eq!(row["kind"], "receiver");
    assert_eq!(row["linkId"], link_id.as_str());
    assert_eq!(row["on"], true);
    assert_eq!(row["note"], "Connecting");

    engine.link_status(&link_id, LinkStatus::Up, None);
    engine.receiver_traffic(&link_id, vec![aircraft(7, 51.5, -0.1, clock.now())]);
    let hearing = status(&mut engine, &mut ui);
    assert_eq!(hearing["aircraft"][0]["sources"], json!([source_id]));
    assert!(source(&hearing, &source_id).get("note").is_none());

    engine.link_status(
        &link_id,
        LinkStatus::Down,
        Some("connection refused".into()),
    );
    let lost = status(&mut engine, &mut ui);
    assert_eq!(
        source(&lost, &source_id)["note"],
        "Cannot reach the receiver: connection refused"
    );
    assert_eq!(source(&lost, &source_id)["problem"], true);

    call(
        &mut engine,
        &mut ui,
        "link/disable",
        json!({ "id": link_id }),
    );
    let off = status(&mut engine, &mut ui);
    assert_eq!(source(&off, &source_id)["on"], false);
    assert_eq!(off["aircraft"], json!([]));
}

#[test]
fn a_usb_receiver_stick_is_one_read_only_sbs_receiver() {
    let (mut engine, _clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    let added = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "usb-receiver" } }),
    );
    let link = &added["result"];
    assert_eq!(link["name"], "USB receiver stick", "{added}");
    assert_eq!(link["carries"], "sbs");
    assert_eq!(link["policy"], "read");
    let source_id = format!("receiver:{}", link["id"].as_str().unwrap());
    let listed = status(&mut engine, &mut ui);
    assert_eq!(source(&listed, &source_id)["kind"], "receiver");
    let second = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "usb-receiver" } }),
    );
    assert_eq!(
        second["error"]["message"],
        "a USB receiver stick is already added"
    );
}

#[test]
fn traffic_alerts_ignore_arming_and_clear_once_past() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({ "id": "alpha", "on": true }),
    );
    at(&mut engine, "alpha", 51.5, -0.1, 600.0);
    let warned = |engine: &Engine| {
        engine
            .state()
            .notifications
            .values()
            .filter(|n| {
                n.condition.as_deref() == Some("traffic-near:alpha:000005")
                    && serde_json::to_value(n.state).unwrap() == json!("active")
            })
            .map(|n| (n.title.clone(), n.body.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
    };
    // Close while disarmed: warn according to the explicit settings.
    let mut near = aircraft(5, 51.51, -0.1, clock.now());
    near.altitude_m = Some(720.0);
    engine.vehicle_traffic("alpha", near.clone());
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    assert_eq!(warned(&engine).len(), 1);

    let mut armed = engine.state().vehicles["alpha"].clone();
    armed.armed = true;
    engine.vehicle_observed(uxduck_core::domain::vehicle::Observation {
        armed: true,
        ..observation_of(&armed)
    });
    near.heard_us = clock.now();
    engine.vehicle_traffic("alpha", near.clone());
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    assert_eq!(
        warned(&engine),
        [(
            "T5 near Alpha".to_owned(),
            "1.1 km away, 120 m above. Traffic is never complete: look out as well.".to_owned()
        )]
    );

    // Just beyond 2 km it holds; past a quarter more it clears.
    let mut further = near.clone();
    further.position = Some((51.5195, -0.1));
    further.heard_us = clock.now();
    engine.vehicle_traffic("alpha", further.clone());
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    assert_eq!(warned(&engine).len(), 1, "within the margin");
    further.position = Some((51.53, -0.1));
    further.heard_us = clock.now();
    engine.vehicle_traffic("alpha", further);
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    assert!(warned(&engine).is_empty(), "cleared once past");
    clock.advance(4_000_000); // Return at a plausible speed, not a refused jump.
    near.heard_us = clock.now();
    engine.vehicle_traffic("alpha", near);
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    assert_eq!(
        warned(&engine).len(),
        1,
        "the same aircraft warns again on re-entry"
    );
}

fn observation_of(v: &uxduck_core::domain::vehicle::Vehicle) -> Observation {
    Observation {
        id: v.id.clone(),
        aliases: v.aliases.clone(),
        link_id: v.link_id.clone(),
        peer: v.peer.clone(),
        system_id: v.system_id,
        component_ids: v.component_ids.clone(),
        autopilot: v.autopilot.clone(),
        vehicle_type: v.vehicle_type.clone(),
        uid: v.uid.clone(),
        firmware: v.firmware.clone(),
        firmware_commit: v.firmware_commit.clone(),
        presence: v.presence,
        last_heartbeat_us: v.last_heartbeat_us,
        armed: v.armed,
        mode: v.mode.clone(),
    }
}

#[test]
fn traffic_settings_preserve_explicit_choices_and_reject_invalid_ranges() {
    let (mut engine, _) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    let settings = serde_json::to_value(uxduck_contracts::convert::settings::settings_dto(
        &engine.state().settings,
        uxduck_core::domain::settings::ClosePolicy::KeepRunning,
    ))
    .unwrap();
    assert_eq!(settings["trafficControls"]["rangeM"], 10000);
    assert_eq!(settings["trafficControls"]["heightM"], 1000);
    assert_eq!(settings["trafficControls"]["refreshS"], 30);
    let changed = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({
            "expectedVersion": settings["version"],
            "trafficControls": {"rangeM": 15000, "heightM": 1500, "limitHeight": false,
                "allVehicles": false, "vehicleIds": ["bravo"], "deviceLocation": true,
                "sites": [{"id":"ground", "name":"Launch", "latitudeE7":515_000_000, "longitudeE7":-1_000_000, "altitudeM":40}],
                "alertsEnabled": false, "refreshS": 45}
        }),
    );
    assert!(changed.get("error").is_none(), "{changed}");
    let saved = serde_json::to_value(uxduck_contracts::convert::settings::settings_dto(
        &engine.state().settings,
        uxduck_core::domain::settings::ClosePolicy::KeepRunning,
    ))
    .unwrap();
    assert_eq!(saved["trafficControls"]["heightM"], 1500);
    assert_eq!(saved["trafficControls"]["limitHeight"], false);
    assert_eq!(saved["trafficControls"]["sites"][0]["name"], "Launch");
    for bad in [
        json!({"rangeM":0}),
        json!({"refreshS":0}),
        json!({"heightM":20001}),
        json!({"sites":[{"id":"x","name":"X","latitudeE7":910_000_000,"longitudeE7":0}]}),
    ] {
        let answer = call(
            &mut engine,
            &mut ui,
            "settings/update",
            json!({"expectedVersion":saved["version"],"trafficControls":bad}),
        );
        assert_eq!(answer["error"]["data"]["kind"], "invalid-value", "{answer}");
    }
    assert_eq!(
        serde_json::to_value(uxduck_contracts::convert::settings::settings_dto(
            &engine.state().settings,
            uxduck_core::domain::settings::ClosePolicy::KeepRunning
        ))
        .unwrap(),
        saved
    );
}

#[test]
fn pressure_height_stays_uncertain_and_alerts_are_independent_of_display_filters() {
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({"id":"alpha","on":true}),
    );
    at(&mut engine, "alpha", 51.5, -0.1, 600.0);
    let mut near = aircraft(5, 51.51, -0.1, clock.now());
    near.altitude_m = Some(9000.0);
    near.altitude_frame = Some(uxduck_core::domain::traffic::AltitudeFrame::Pressure);
    engine.vehicle_traffic("alpha", near);
    clock.advance(300_000);
    engine.expire_traffic(clock.now());
    let picture = status(&mut engine, &mut ui);
    assert!(picture["aircraft"][0]["amslM"].is_null());
    assert!(picture["aircraft"][0]["ellipsoidM"].is_null());
    assert_eq!(picture["aircraft"][0]["alertVehicleIds"], json!(["alpha"]));
    assert!(engine.state().notifications.values().any(|n| {
        n.body
            .as_deref()
            .is_some_and(|body| body.contains("altitude uncertain"))
    }));
    let version = engine.state().settings.version;
    let saved = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({"expectedVersion":version,"trafficControls":{"rangeM":100,"heightM":30,"allVehicles":false}}),
    );
    assert!(saved.get("error").is_none(), "{saved}");
    assert_eq!(
        status(&mut engine, &mut ui)["aircraft"][0]["alertVehicleIds"],
        json!(["alpha"])
    );
    let version = engine.state().settings.version;
    call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({"expectedVersion":version,"trafficControls":{"alertsEnabled":false}}),
    );
    assert!(status(&mut engine, &mut ui)["aircraft"][0]["alertVehicleIds"].is_null());
    assert!(
        !engine
            .state()
            .notifications
            .values()
            .any(
                |n| n.condition.as_deref() == Some("traffic-near:alpha:000005")
                    && serde_json::to_value(n.state).unwrap() == json!("active")
            )
    );
    clock.advance(61_000_000);
    assert!(
        status(&mut engine, &mut ui)["references"].is_null(),
        "stale vehicle locations are not used"
    );
}

#[test]
fn ellipsoid_and_mavlink_sea_level_reports_compare_at_the_same_height() {
    use uxduck_core::domain::traffic::{AltitudeFrame, geoid_height};
    let (mut engine, clock) = engine();
    let mut ui = join(&mut engine, "operator-ui");
    two_vehicles(&mut engine, &mut ui);
    call(
        &mut engine,
        &mut ui,
        "vehicle/traffic",
        json!({"id":"alpha","on":true}),
    );
    at(&mut engine, "alpha", 51.5, -0.1, 600.0);
    let mut msl = aircraft(5, 51.51, -0.1, clock.now());
    msl.altitude_m = Some(720.0);
    let mut ellipsoid = msl.clone();
    ellipsoid.address = 6;
    ellipsoid.altitude_frame = Some(AltitudeFrame::Ellipsoid);
    ellipsoid.altitude_m = Some(720.0 + geoid_height(51.51, -0.1).unwrap());
    engine.vehicle_traffic("alpha", msl);
    engine.vehicle_traffic("alpha", ellipsoid);
    let picture = status(&mut engine, &mut ui);
    for a in picture["aircraft"].as_array().unwrap() {
        assert!((a["amslM"].as_f64().unwrap() - 720.0).abs() < 0.01);
        assert!((a["relative"][0]["aboveM"].as_f64().unwrap() - 120.0).abs() < 0.01);
        assert_eq!(a["alertVehicleIds"], json!(["alpha"]));
    }
}

//! Camera streams (S38, ADR-0017): a new source notifies once and is the
//! same source when it comes back within 30 s, a vehicle claims it by its
//! sender's address or by hand, its counters are journaled sparingly, and a
//! watch ends with the connection that asked for it.

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
use uxduck_core::domain::notification::{Action, Subject};
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::domain::video::{
    RecordingState, VideoContainer, VideoCounters, VideoReport, VideoSource, VideoState, VideoTrack,
};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::video::VideoPort;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const NOW: u64 = 1_790_000_000_000_000;
const SECOND: u64 = 1_000_000;

#[derive(Clone, Default)]
struct Manual(Arc<AtomicU64>);

impl Manual {
    fn advance(&self, us: u64) {
        self.0.fetch_add(us, Ordering::SeqCst);
    }
}

impl Clock for Manual {
    fn now_us(&self) -> u64 {
        NOW + self.0.load(Ordering::SeqCst)
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

/// What the engine told the `/video` hub.
#[derive(Clone, Debug, Default)]
struct Hub(Arc<Mutex<Vec<String>>>);

impl VideoPort for Hub {
    fn allow(&mut self, token: &str, source_id: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("allow {token} {source_id}"));
    }
    fn revoke(&mut self, token: &str) {
        self.0.lock().unwrap().push(format!("revoke {token}"));
    }
    fn listen(&mut self, port: u16) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("listen {port}"));
        if port == 5602 {
            Err("UDP port 5602 is in use by another program".into())
        } else {
            Ok(())
        }
    }
    fn unlisten(&mut self, port: u16) {
        self.0.lock().unwrap().push(format!("unlisten {port}"));
    }
    fn open_rtsp(&mut self, source_id: &str, url: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("open {source_id} {url}"));
    }
    fn close_rtsp(&mut self, source_id: &str) {
        self.0.lock().unwrap().push(format!("close {source_id}"));
    }
    fn start_recording(&mut self, recording_id: &str, source_id: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("record {recording_id} {source_id}"));
    }
    fn stop_recording(&mut self, recording_id: &str) {
        self.0.lock().unwrap().push(format!("stop {recording_id}"));
    }
    fn recover_recording(&mut self, _recording_id: &str) -> Option<(u64, u64)> {
        Some((4096, 1_500_000))
    }
    fn allow_playback(&mut self, token: &str, recording_id: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("play {token} {recording_id}"));
    }
    fn reveal(&mut self, recording_id: Option<&str>) -> Result<String, String> {
        self.0
            .lock()
            .unwrap()
            .push(format!("reveal {}", recording_id.unwrap_or("-")));
        Ok("/Users/pilot/Movies/UXDuck".into())
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
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
    client
}

fn call(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            return frame;
        }
        client.queued.push_back(frame);
    }
    panic!("no reply to {method}")
}

fn engine() -> (Engine, Manual, Hub) {
    engine_from(Loaded::default())
}

fn engine_from(loaded: Loaded) -> (Engine, Manual, Hub) {
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
        links: Box::new(NoLinks),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    let clock = Manual::default();
    let mut engine = Engine::new(
        config,
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        ports,
        loaded,
    );
    let hub = Hub::default();
    engine.set_video(Box::new(hub.clone()));
    (engine, clock, hub)
}

fn report(sender: &str, datagrams: u64) -> VideoReport {
    VideoReport {
        port: 5600,
        sender: sender.into(),
        container: VideoContainer::RtpTs,
        codec: Some("hvc1.1.2.L120".into()),
        width: Some(1280),
        height: Some(720),
        frame_rate: Some(50.0),
        unsupported: None,
        klv: true,
        id: None,
        note: None,
        counters: VideoCounters {
            datagrams,
            bitrate_bps: 4_000_000,
            ..VideoCounters::default()
        },
    }
}

fn vehicle_at(engine: &mut Engine, id: &str, peer: &str) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: Vec::new(),
        link_id: "lk".into(),
        peer: peer.into(),
        system_id: 1,
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

fn video_notifications(engine: &Engine) -> Vec<String> {
    engine
        .state()
        .notifications
        .values()
        .filter(|n| matches!(n.subject, Subject::VideoSource { .. }))
        .map(|n| n.title.clone())
        .collect()
}

#[test]
fn a_new_source_notifies_once_and_is_the_same_source_back_within_30_s() {
    let (mut engine, clock, _) = engine();
    engine.video_observed(report("192.168.144.25:41000", 10));
    let source = &engine.state().video_sources["udp-5600-192.168.144.25"];
    assert_eq!(source.state, VideoState::Receiving);
    assert_eq!(source.codec.as_deref(), Some("hvc1.1.2.L120"));
    assert!(source.klv);
    let titles = video_notifications(&engine);
    assert_eq!(titles, ["Video from udp://192.168.144.25:5600"]);
    let notification = engine.state().notifications.values().next().unwrap();
    assert_eq!(
        notification.actions[0].action,
        Action::OpenVideo {
            source_id: "udp-5600-192.168.144.25".into()
        }
    );

    // Unplugged, and back within 30 s from another port: the same source.
    engine.video_stopped("udp-5600-192.168.144.25");
    assert_eq!(
        engine.state().video_sources["udp-5600-192.168.144.25"].state,
        VideoState::Stopped
    );
    clock.advance(20 * SECOND);
    engine.video_observed(report("192.168.144.25:41002", 20));
    assert_eq!(
        video_notifications(&engine).len(),
        1,
        "no second notification"
    );
    assert_eq!(engine.state().video_sources.len(), 1);

    // Gone for longer: a new stream, worth saying again.
    engine.video_stopped("udp-5600-192.168.144.25");
    clock.advance(31 * SECOND);
    engine.video_observed(report("192.168.144.25:41002", 30));
    assert_eq!(video_notifications(&engine).len(), 2);
}

#[test]
fn counters_are_journaled_sparingly_and_listed_live() {
    let (mut engine, clock, _) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    engine.video_observed(report("10.0.0.5:5000", 1));
    let seq = engine.state().seq;
    for n in 2..10 {
        clock.advance(SECOND);
        engine.video_observed(report("10.0.0.5:5000", n));
    }
    assert_eq!(engine.state().seq, seq, "nothing but counters changed");
    let listed = call(&mut engine, &mut ui, "video/list", json!({}));
    assert_eq!(listed["result"]["sources"][0]["counters"]["datagrams"], 9);
    clock.advance(30 * SECOND);
    engine.video_observed(report("10.0.0.5:5000", 40));
    assert_eq!(engine.state().seq, seq + 1, "written every half minute");
}

#[test]
fn a_vehicle_claims_a_camera_by_its_address_but_not_by_loopback() {
    let (mut engine, _, _) = engine();
    vehicle_at(&mut engine, "plane", "192.168.144.25:14550");
    vehicle_at(&mut engine, "sim", "127.0.0.1:5760");
    engine.video_observed(report("192.168.144.25:41000", 1));
    engine.video_observed(report("127.0.0.1:41000", 1));
    let sources = &engine.state().video_sources;
    assert_eq!(
        sources["udp-5600-192.168.144.25"].vehicle_id.as_deref(),
        Some("plane")
    );
    assert_eq!(sources["udp-5600-127.0.0.1"].vehicle_id, None);
    let plane = engine.state().vehicles["plane"].name.clone();
    let mut titles = video_notifications(&engine);
    titles.sort();
    let mut expected = vec![
        "Video from udp://127.0.0.1:5600".to_owned(),
        format!("Video from {plane}"),
    ];
    expected.sort();
    assert_eq!(titles, expected, "named after the vehicle it belongs to");
}

#[test]
fn assigned_by_hand_it_stays_until_given_back() {
    let (mut engine, _, _) = engine();
    vehicle_at(&mut engine, "plane", "192.168.144.25:14550");
    vehicle_at(&mut engine, "copter", "192.168.144.30:14550");
    let mut ui = connect(&mut engine, "operator-ui");
    engine.video_observed(report("192.168.144.25:41000", 1));
    let assigned = call(
        &mut engine,
        &mut ui,
        "video/assign",
        json!({ "sourceId": "udp-5600-192.168.144.25", "vehicleId": "copter" }),
    );
    assert_eq!(assigned["result"]["vehicleId"], "copter");
    assert_eq!(assigned["result"]["assignedByHand"], true);
    engine.video_observed(report("192.168.144.25:41000", 2));
    assert_eq!(
        engine.state().video_sources["udp-5600-192.168.144.25"]
            .vehicle_id
            .as_deref(),
        Some("copter"),
        "the address no longer decides"
    );
    let back = call(
        &mut engine,
        &mut ui,
        "video/assign",
        json!({ "sourceId": "udp-5600-192.168.144.25" }),
    );
    assert_eq!(back["result"]["vehicleId"], "plane");
    let unknown = call(
        &mut engine,
        &mut ui,
        "video/assign",
        json!({ "sourceId": "udp-5600-192.168.144.25", "vehicleId": "nope" }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found");
}

#[test]
fn a_watch_hands_out_a_token_and_ends_with_its_connection() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let missing = call(
        &mut engine,
        &mut ui,
        "video/watch",
        json!({ "sourceId": "udp-5600-10.0.0.5" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");

    engine.video_observed(report("10.0.0.5:5000", 1));
    let watch = call(
        &mut engine,
        &mut ui,
        "video/watch",
        json!({ "sourceId": "udp-5600-10.0.0.5" }),
    );
    let token = watch["result"]["token"].as_str().unwrap().to_owned();
    assert_eq!(watch["result"]["path"], format!("/video?token={token}"));
    let second = call(
        &mut engine,
        &mut ui,
        "video/watch",
        json!({ "sourceId": "udp-5600-10.0.0.5" }),
    );
    let token2 = second["result"]["token"].as_str().unwrap().to_owned();
    assert_ne!(token, token2);

    call(
        &mut engine,
        &mut ui,
        "video/unwatch",
        json!({ "token": token }),
    );
    engine.disconnect(ui.id);
    assert_eq!(
        *hub.0.lock().unwrap(),
        [
            format!("allow {token} udp-5600-10.0.0.5"),
            format!("allow {token2} udp-5600-10.0.0.5"),
            format!("revoke {token}"),
            format!("revoke {token2}"),
        ]
    );
}

#[test]
fn a_busy_port_is_explained_once_and_cleared_when_it_listens() {
    let (mut engine, _, _) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let busy = "UDP port 5600 is in use by QGroundControl (pid 812)";
    engine.video_listening(None, Some(busy.into()));
    engine.video_listening(None, Some(busy.into()));
    let warnings: Vec<_> = engine
        .state()
        .notifications
        .values()
        .filter(|n| n.title == "UXDuck can't receive video")
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].body.as_deref(), Some(busy));
    let listed = call(&mut engine, &mut ui, "video/list", json!({}));
    assert_eq!(listed["result"]["listenError"], busy);

    engine.video_listening(Some(5600), None);
    assert!(
        engine
            .state()
            .notifications
            .values()
            .all(|n| n.title != "UXDuck can't receive video"
                || n.state != uxduck_core::domain::notification::NotificationState::Active)
    );
    let listed = call(&mut engine, &mut ui, "video/list", json!({}));
    assert_eq!(listed["result"]["port"], 5600);
}

#[test]
fn sources_receiving_when_the_daemon_stopped_are_stopped_at_start() {
    let (mut engine, _, hub) = engine();
    engine.video_observed(report("10.0.0.5:5000", 1));
    engine.set_video(Box::new(hub));
    let source = &engine.state().video_sources["udp-5600-10.0.0.5"];
    assert_eq!(source.state, VideoState::Stopped);
    assert_eq!(source.counters.bitrate_bps, 0);
}

#[test]
fn an_rtsp_camera_added_by_hand_is_fetched_shown_without_its_password_and_removed() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let url = "rtsp://admin:secret@192.168.144.25:8554/main.264";
    let added = call(&mut engine, &mut ui, "video/add", json!({ "url": url }));
    let source = &added["result"]["source"];
    let id = source["id"].as_str().unwrap().to_owned();
    assert_eq!(source["url"], "rtsp://admin@192.168.144.25:8554/main.264");
    assert_eq!(source["addedByHand"], true);
    assert_eq!(source["port"], 8554);
    assert_eq!(source["state"], "stopped");
    assert!(hub.0.lock().unwrap().contains(&format!("open {id} {url}")));
    // The same address again is the same camera.
    let again = call(&mut engine, &mut ui, "video/add", json!({ "url": url }));
    assert_eq!(again["result"]["source"]["id"], id.as_str());

    // What the RTSP client reports lands on it, under its own id.
    let mut playing = report("192.168.144.25:8554", 5);
    playing.id = Some(id.clone());
    engine.video_observed(playing);
    assert_eq!(
        engine.state().video_sources[&id].state,
        VideoState::Receiving
    );
    engine.video_stopped_because(&id, Some("Can't reach it: refused".into()));
    let stopped = &engine.state().video_sources[&id];
    assert_eq!(stopped.state, VideoState::Stopped);
    assert_eq!(
        stopped.state_detail.as_deref(),
        Some("Can't reach it: refused")
    );

    // A camera that came by itself cannot be removed.
    engine.video_observed(report("10.0.0.5:5000", 1));
    let refused = call(
        &mut engine,
        &mut ui,
        "video/remove",
        json!({ "sourceId": "udp-5600-10.0.0.5" }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
    call(
        &mut engine,
        &mut ui,
        "video/remove",
        json!({ "sourceId": id }),
    );
    assert!(!engine.state().video_sources.contains_key(&id));
    assert!(hub.0.lock().unwrap().contains(&format!("close {id}")));
}

#[test]
fn an_rtsp_cameras_tracks_are_listed_and_another_is_chosen_on_its_address() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let url = "rtsp://admin:secret@192.168.144.25:8554/main";
    let added = call(&mut engine, &mut ui, "video/add", json!({ "url": url }));
    let id = added["result"]["source"]["id"].as_str().unwrap().to_owned();
    let track = |number: u32, codec: &str, playing: bool| VideoTrack {
        number,
        codec: codec.into(),
        width: None,
        height: None,
        frame_rate: None,
        playing,
    };
    engine.video_tracks(&id, vec![track(1, "H.264", true), track(2, "H.265", false)]);
    let listed = call(&mut engine, &mut ui, "video/list", json!({}));
    assert_eq!(
        listed["result"]["sources"][0]["tracks"],
        json!([
            { "number": 1, "codec": "H.264", "playing": true },
            { "number": 2, "codec": "H.265" },
        ])
    );

    let chosen = call(
        &mut engine,
        &mut ui,
        "video/track",
        json!({ "sourceId": id, "track": 2 }),
    );
    assert_eq!(
        chosen["result"]["url"],
        "rtsp://admin@192.168.144.25:8554/main#track=2"
    );
    assert_eq!(chosen["result"]["name"], chosen["result"]["url"]);
    assert_eq!(chosen["result"]["tracks"][1]["playing"], true);
    // Opened again with the track on the address, password and all.
    let log = hub.0.lock().unwrap().clone();
    assert!(log.contains(&format!("open {id} {url}#track=2")), "{log:?}");

    // Back to the first: the address without a track, as added.
    let first = call(
        &mut engine,
        &mut ui,
        "video/track",
        json!({ "sourceId": id, "track": 1 }),
    );
    assert_eq!(
        first["result"]["url"],
        "rtsp://admin@192.168.144.25:8554/main"
    );
    for (track, kind) in [(3, "invalid-value"), (0, "invalid-value")] {
        let refused = call(
            &mut engine,
            &mut ui,
            "video/track",
            json!({ "sourceId": id, "track": track }),
        );
        assert_eq!(refused["error"]["data"]["kind"], kind, "{refused}");
    }

    // A camera that came by itself over UDP has no tracks to choose.
    engine.video_observed(report("10.0.0.5:5000", 1));
    let refused = call(
        &mut engine,
        &mut ui,
        "video/track",
        json!({ "sourceId": "udp-5600-10.0.0.5", "track": 2 }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");

    // An address can name its track when it is added; anything else after # is refused.
    let named = call(
        &mut engine,
        &mut ui,
        "video/add",
        json!({ "url": "rtsp://192.168.144.26/live#track=2" }),
    );
    assert_eq!(
        named["result"]["source"]["url"],
        "rtsp://192.168.144.26/live#track=2"
    );
    let bad = call(
        &mut engine,
        &mut ui,
        "video/add",
        json!({ "url": "rtsp://192.168.144.26/live#stream=2" }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value");
}

#[test]
fn a_udp_port_added_by_hand_is_listened_on_kept_and_removed() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let added = call(
        &mut engine,
        &mut ui,
        "video/add",
        json!({ "url": "udp:5601" }),
    );
    assert_eq!(added["result"]["port"], 5601);
    assert_eq!(engine.state().settings.video_ports, [5601]);
    let busy = call(
        &mut engine,
        &mut ui,
        "video/add",
        json!({ "url": "udp://:5602" }),
    );
    assert_eq!(busy["error"]["data"]["kind"], "port-in-use");
    let listed = call(&mut engine, &mut ui, "video/list", json!({}));
    assert_eq!(
        listed["result"]["extraPorts"],
        json!([
            { "port": 5601 },
            { "port": 5602, "error": "UDP port 5602 is in use by another program" }
        ])
    );
    let bad = call(
        &mut engine,
        &mut ui,
        "video/add",
        json!({ "url": "http://cam" }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value");
    call(
        &mut engine,
        &mut ui,
        "video/remove",
        json!({ "port": 5601 }),
    );
    assert_eq!(engine.state().settings.video_ports, [5602]);
    let log = hub.0.lock().unwrap().clone();
    assert!(log.contains(&"listen 5601".to_owned()));
    assert!(log.contains(&"unlisten 5601".to_owned()));

    // The ports are listened on again when the daemon starts.
    let again = Hub::default();
    engine.set_video(Box::new(again.clone()));
    assert!(again.0.lock().unwrap().contains(&"listen 5602".to_owned()));
}

#[test]
fn a_camera_named_by_its_vehicle_brings_its_stream_with_the_vehicle() {
    let (mut engine, _, hub) = engine();
    vehicle_at(&mut engine, "plane", "127.0.0.1:14550");
    engine.video_stream_announced("plane", 100, 1, "rtsp://192.168.144.25:8554/main");
    let id = "cam-plane-100-1";
    let source = &engine.state().video_sources[id];
    assert_eq!(source.vehicle_id.as_deref(), Some("plane"));
    assert!(!source.added_by_hand);
    // Named again, as cameras answer every time they are asked: one fetch.
    engine.video_stream_announced("plane", 100, 1, "rtsp://192.168.144.25:8554/main");
    let opened = hub
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with(&format!("open {id}")))
        .count();
    assert_eq!(opened, 1);
    // Its first picture says where it came from, as a camera found by itself.
    let mut playing = report("192.168.144.25:8554", 5);
    playing.id = Some(id.into());
    engine.video_observed(playing);
    let plane = engine.state().vehicles["plane"].name.clone();
    assert!(video_notifications(&engine).contains(&format!("Video from {plane}")));

    // A UDP stream: the port is listened on, and a loopback sender there is
    // the plane's, which an address alone could not say.
    engine.video_stream_announced("plane", 100, 2, "udp://0.0.0.0:5605");
    assert!(hub.0.lock().unwrap().contains(&"listen 5605".to_owned()));
    let mut sent = report("127.0.0.1:40000", 1);
    sent.port = 5605;
    engine.video_observed(sent);
    assert_eq!(
        engine.state().video_sources["udp-5605-127.0.0.1"]
            .vehicle_id
            .as_deref(),
        Some("plane")
    );
}

fn armed(engine: &mut Engine, id: &str, peer: &str, armed: bool) {
    engine.vehicle_observed(Observation {
        id: id.into(),
        aliases: Vec::new(),
        link_id: "lk".into(),
        peer: peer.into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "Quadrotor".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed,
        mode: None,
    });
}

fn log_of(hub: &Hub, prefix: &str) -> Vec<String> {
    hub.0
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with(prefix))
        .cloned()
        .collect()
}

#[test]
fn a_camera_records_while_its_vehicle_is_armed() {
    let (mut engine, _, hub) = engine();
    armed(&mut engine, "plane", "192.168.144.25:14550", false);
    engine.video_observed(report("192.168.144.25:41000", 1));
    assert!(log_of(&hub, "record").is_empty(), "disarmed: nothing yet");

    armed(&mut engine, "plane", "192.168.144.25:14550", true);
    let started = log_of(&hub, "record");
    assert_eq!(started.len(), 1);
    let recording_id = started[0].split(' ').nth(1).unwrap().to_owned();
    let recording = &engine.state().video_recordings[&recording_id];
    assert_eq!(recording.vehicle_id.as_deref(), Some("plane"));
    assert_eq!(recording.state, RecordingState::Recording);
    // Reports while it sends start nothing more.
    engine.video_observed(report("192.168.144.25:41000", 2));
    assert_eq!(log_of(&hub, "record").len(), 1);

    armed(&mut engine, "plane", "192.168.144.25:14550", false);
    assert_eq!(log_of(&hub, "stop"), [format!("stop {recording_id}")]);
    engine.video_recording_ended(&recording_id, 1_000_000, 42_000_000, None);
    let done = &engine.state().video_recordings[&recording_id];
    assert_eq!(done.state, RecordingState::Done);
    assert_eq!((done.bytes, done.duration_us), (1_000_000, 42_000_000));
}

#[test]
fn by_hand_a_camera_records_until_stopped_or_it_stops_sending() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    engine.video_observed(report("10.0.0.5:5000", 1));
    call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "videoRecording": "by-hand" }),
    );
    assert!(log_of(&hub, "record").is_empty());
    let started = call(
        &mut engine,
        &mut ui,
        "video/record",
        json!({ "sourceId": "udp-5600-10.0.0.5", "on": true }),
    );
    assert!(started.get("result").is_some(), "{started}");
    assert_eq!(log_of(&hub, "record").len(), 1);
    engine.video_stopped("udp-5600-10.0.0.5");
    assert_eq!(log_of(&hub, "stop").len(), 1, "a camera that stops ends it");
    // Back, it does not record again: what was asked by hand ended with it.
    engine.video_observed(report("10.0.0.5:5000", 3));
    assert_eq!(log_of(&hub, "record").len(), 1);

    // Always: it records as soon as it sends.
    call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 1, "videoRecording": "always" }),
    );
    assert_eq!(log_of(&hub, "record").len(), 2);
}

#[test]
fn a_recording_interrupted_by_a_crash_is_recovered_and_can_be_played() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "videoRecording": "always" }),
    );
    engine.video_observed(report("10.0.0.5:5000", 1));
    let id = engine
        .state()
        .video_recordings
        .keys()
        .next()
        .unwrap()
        .clone();
    // The daemon starts again: the recording it was writing is recovered.
    engine.set_video(Box::new(hub.clone()));
    let recovered = &engine.state().video_recordings[&id];
    assert_eq!(recovered.state, RecordingState::Recovered);
    assert_eq!((recovered.bytes, recovered.duration_us), (4096, 1_500_000));

    let playback = call(
        &mut engine,
        &mut ui,
        "video/playback",
        json!({ "recordingId": id }),
    );
    let path = playback["result"]["path"].as_str().unwrap();
    assert!(
        path.starts_with(&format!("/recordings/{id}.mp4?token=vp")),
        "{path}"
    );
    assert_eq!(log_of(&hub, "play").len(), 1);
}

#[test]
fn a_cameras_klv_becomes_its_vehicles_telemetry_and_names_its_vehicle() {
    use uxduck_core::domain::video::CameraMetadata;
    let (mut engine, _, _) = engine();
    armed(&mut engine, "plane", "127.0.0.1:14550", false);
    let plane = engine.state().vehicles["plane"].name.clone();
    // From loopback, no address can say whose it is.
    engine.video_observed(report("127.0.0.1:41000", 1));
    let metadata = CameraMetadata {
        platform_designation: Some(format!(" {} ", plane.to_uppercase())),
        sensor_latitude_deg: Some(-35.36),
        frame_latitude_deg: Some(-35.361),
        frame_longitude_deg: Some(149.17),
        corners: Some([
            (-35.36, 149.16),
            (-35.36, 149.18),
            (-35.362, 149.18),
            (-35.362, 149.16),
        ]),
        ..CameraMetadata::default()
    };
    engine.video_klv("udp-5600-127.0.0.1", &metadata);
    assert_eq!(
        engine.state().video_sources["udp-5600-127.0.0.1"]
            .vehicle_id
            .as_deref(),
        Some("plane"),
        "the platform it names is that vehicle"
    );
    let telemetry = engine.telemetry("plane").expect("telemetry");
    let value = |key: &str| {
        telemetry
            .items
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.value)
    };
    assert_eq!(value("camera.frame_latitude_deg"), Some(-35.361));
    assert_eq!(value("camera.corner2_longitude_deg"), Some(149.18));
    assert_eq!(
        value("camera.longitude_deg"),
        None,
        "only what the packet had"
    );
    // Still the plane's after the next report.
    engine.video_observed(report("127.0.0.1:41000", 2));
    assert_eq!(
        engine.state().video_sources["udp-5600-127.0.0.1"]
            .vehicle_id
            .as_deref(),
        Some("plane")
    );

    // A camera no vehicle can be found for gives no telemetry.
    engine.video_observed(report("10.9.9.9:41000", 1));
    engine.video_klv(
        "udp-5600-10.9.9.9",
        &CameraMetadata {
            platform_designation: Some("MQ-9".into()),
            ..metadata
        },
    );
    assert_eq!(
        engine.state().video_sources["udp-5600-10.9.9.9"].vehicle_id,
        None
    );
}

#[test]
fn a_camera_is_named_by_its_address_until_renamed() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    engine.video_observed(report("127.0.0.1:41000", 1));
    let id = "udp-5600-127.0.0.1";
    assert_eq!(
        engine.state().video_sources[id].name,
        "udp://127.0.0.1:5600"
    );

    let renamed = call(
        &mut engine,
        &mut ui,
        "video/rename",
        json!({ "sourceId": id, "name": "  Front " }),
    );
    assert_eq!(renamed["result"]["name"], "Front");
    assert_eq!(renamed["result"]["namedByHand"], true);
    engine.video_observed(report("127.0.0.1:41005", 2));
    engine.set_video(Box::new(hub));
    assert_eq!(engine.state().video_sources[id].name, "Front", "kept");

    let back = call(
        &mut engine,
        &mut ui,
        "video/rename",
        json!({ "sourceId": id, "name": "" }),
    );
    assert_eq!(back["result"]["name"], "udp://127.0.0.1:5600");
    assert!(back["result"].get("namedByHand").is_none());
    let unknown = call(
        &mut engine,
        &mut ui,
        "video/rename",
        json!({ "sourceId": "nope", "name": "x" }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found");

    // Before 2026-10-05 a camera was named by its host alone.
    let mut old = engine.state().video_sources[id].clone();
    old.name = "127.0.0.1".into();
    let (fresh, _, _) = engine_with_source(old);
    assert_eq!(fresh.state().video_sources[id].name, "udp://127.0.0.1:5600");
}

/// An engine started from a checkpoint holding `source`, as after a restart.
fn engine_with_source(source: VideoSource) -> (Engine, Manual, Hub) {
    let (engine, _, _) = engine();
    let mut state = engine.state().clone();
    state.video_sources.insert(source.id.clone(), source);
    engine_from(Loaded {
        state,
        ..Loaded::default()
    })
}

#[test]
fn a_second_stream_from_one_address_is_named_by_its_port_and_forgotten_at_start() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    engine.video_observed(report("127.0.0.1:41000", 1));
    let second = "udp-5600-127.0.0.1-from-51234";
    engine.video_observed(VideoReport {
        id: Some(second.into()),
        ..report("127.0.0.1:51234", 1)
    });
    assert_eq!(
        engine.state().video_sources[second].name,
        "udp://127.0.0.1:5600 (from :51234)"
    );
    let third = "udp-5600-127.0.0.1-from-51300";
    engine.video_observed(VideoReport {
        id: Some(third.into()),
        ..report("127.0.0.1:51300", 1)
    });
    call(
        &mut engine,
        &mut ui,
        "video/rename",
        json!({ "sourceId": third, "name": "Rear" }),
    );
    // Its port is picked anew each time it starts: gone at start unless named.
    engine.set_video(Box::new(hub));
    let sources = &engine.state().video_sources;
    assert!(sources.contains_key("udp-5600-127.0.0.1"));
    assert!(!sources.contains_key(second));
    assert_eq!(sources[third].name, "Rear");
}

#[test]
fn the_recordings_folder_is_shown_with_a_recording_selected() {
    let (mut engine, _, hub) = engine();
    let mut ui = connect(&mut engine, "operator-ui");
    let shown = call(&mut engine, &mut ui, "video/reveal", json!({}));
    assert_eq!(shown["result"]["folder"], "/Users/pilot/Movies/UXDuck");
    let unknown = call(
        &mut engine,
        &mut ui,
        "video/reveal",
        json!({ "recordingId": "vr-nope" }),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not-found");
    assert!(hub.0.lock().unwrap().contains(&"reveal -".to_owned()));
}

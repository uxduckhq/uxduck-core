//! What a release stores is read by every later build. 0.8.1-beta.2 could not
//! read the camera counters 0.8.0 had stored and would not start (FB-58
//! follow-up), so each release keeps a sample of what it stores.
//!
//! `tests/stored/<version>/` holds the event log and the checkpoint that the
//! scenario below wrote with that release's own code: a vehicle, cameras, a
//! plan, markers, a place, chats in a folder, settings. Every later build must
//! read every sample with nothing left over. `cargo xtask stored-sample`
//! writes the current version's sample when a release is cut, and
//! `cargo xtask release-check` refuses a tag without one.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "a test scenario"
)]

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::domain::video::{VideoContainer, VideoCounters, VideoReport};
use uxduck_core::events::Event;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{Clock, Persistence, Ports, Random};
use uxduck_engine::state::{self, State};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const SAMPLES: &str = "tests/stored";
/// Writes the current version's sample instead of only checking it.
const WRITE: &str = "UXDUCK_WRITE_STORED_SAMPLE";
/// The checkpoint's collections the scenario must fill, so the samples
/// keep covering them.
const COVERED: &[&str] = &[
    "operators",
    "threads",
    "folders",
    "links",
    "vehicles",
    "notifications",
    "places",
    "markers",
    "plans",
    "videoSources",
];

#[test]
fn every_release_sample_is_read_in_full() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(SAMPLES);
    let mut versions: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    versions.sort();
    assert!(!versions.is_empty(), "no samples in {}", dir.display());
    for version in versions {
        let name = version.file_name().unwrap().to_string_lossy().into_owned();
        let log = std::fs::read_to_string(version.join("events.ndjson")).unwrap();
        let events: Vec<Event> = log
            .lines()
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str(line).unwrap_or_else(|e| {
                    panic!("{name}: event on line {} does not read: {e}", i + 1)
                })
            })
            .collect();
        let text = std::fs::read_to_string(version.join("checkpoint.json")).unwrap();
        let checkpoint: State = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{name}: the checkpoint does not read: {e}"));
        let last = events.last().map_or(0, |e| e.seq);
        assert_eq!(checkpoint.seq, last, "{name}: the checkpoint is at the end");

        let replayed = state::restore(None, events.clone());
        assert!(replayed.gap.is_none(), "{name}: {:?}", replayed.gap);
        assert_eq!(replayed.state.seq, last, "{name}: every event replays");
        let resumed = state::restore(Some(checkpoint), events);
        assert_eq!(resumed.replayed, 0, "{name}");
        for key in COVERED
            .iter()
            .filter(|key| text.contains(&format!("\"{key}\"")))
        {
            let stored = |state: &State| serde_json::to_value(state).unwrap()[key].clone();
            assert_eq!(
                count(&stored(&resumed.state)),
                count(&stored(&replayed.state)),
                "{name}: {key} from the checkpoint and from replaying the log"
            );
        }
    }
}

#[test]
fn the_scenario_fills_what_releases_store() {
    let (events, state) = scenario();
    let checkpoint = serde_json::to_string(&state).unwrap();
    let stored: Value = serde_json::from_str(&checkpoint).unwrap();
    let empty: Vec<_> = COVERED
        .iter()
        .filter(|key| count(&stored[**key]) == 0)
        .collect();
    assert!(empty.is_empty(), "the scenario no longer fills {empty:?}");
    assert!(
        stored["videoSources"]
            .as_object()
            .unwrap()
            .values()
            .any(|s| s["counters"]["pictures"].as_u64() > Some(0)),
        "a camera with counters"
    );
    if std::env::var_os(WRITE).is_none() {
        return;
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(SAMPLES)
        .join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&out).unwrap();
    let mut log = String::new();
    for event in &events {
        log.push_str(&serde_json::to_string(event).unwrap());
        log.push('\n');
    }
    std::fs::write(out.join("events.ndjson"), log).unwrap();
    std::fs::write(out.join("checkpoint.json"), checkpoint).unwrap();
}

fn count(value: &Value) -> usize {
    match value {
        Value::Object(map) => map.len(),
        Value::Array(list) => list.len(),
        _ => 0,
    }
}

/// The events and the final state of a short session that touches most of
/// what UXDuck stores.
fn scenario() -> (Vec<Event>, State) {
    let events = Recorder::default();
    let clock = Manual::default();
    let mut engine = Engine::new(
        EngineConfig {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:45000/".into(),
            checkpoint_every: 1000,
            models: vec!["model-a".into()],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        },
        Box::new(clock.clone()),
        Box::new(Counter(0)),
        Ports::with_persistence(Box::new(events.clone())),
        Loaded::default(),
    );
    let ui = &mut connect(&mut engine, "operator-ui");
    let ana = ok(&mut engine, ui, "operator/create", json!({ "name": "Ana" }));
    ok(
        &mut engine,
        ui,
        "client/setOperator",
        json!({ "operatorId": ana["id"] }),
    );
    ok(
        &mut engine,
        ui,
        "settings/update",
        json!({ "expectedVersion": 0, "liveSharing": false }),
    );

    let link = ok(
        &mut engine,
        ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    engine.vehicle_observed(Observation {
        connections: None,
        id: "vehicle-1".into(),
        aliases: Vec::new(),
        link_id: link["id"].as_str().unwrap().into(),
        peer: "192.168.144.25:14550".into(),
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
    ok(
        &mut engine,
        ui,
        "vehicle/label",
        json!({ "ids": ["vehicle-1"], "add": ["Survey"] }),
    );

    for datagrams in [10, 400] {
        engine.video_observed(VideoReport {
            port: 5600,
            sender: "192.168.144.25:41000".into(),
            container: VideoContainer::RtpTs,
            codec: Some("hvc1.1.2.L120".into()),
            width: Some(1280),
            height: Some(720),
            frame_rate: Some(30.0),
            unsupported: None,
            klv: true,
            id: None,
            note: None,
            audio: None,
            counters: VideoCounters {
                bitrate_bps: 4_000_000,
                datagrams,
                pictures: datagrams / 2,
                keyframes: 2,
                ..VideoCounters::default()
            },
        });
        clock.advance(60_000_000);
    }
    ok(
        &mut engine,
        ui,
        "video/add",
        json!({ "url": "rtsp://192.168.144.25:8554/main.264" }),
    );

    ok(
        &mut engine,
        ui,
        "marker/add",
        json!({ "look": "flag", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    ok(
        &mut engine,
        ui,
        "place/save",
        json!({ "name": "Field", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    let plan = ok(
        &mut engine,
        ui,
        "plan/create",
        json!({ "vehicleType": "multicopter" }),
    );
    let mut body = plan.clone();
    body["items"] = json!([{ "command": 22, "frame": 3, "params": [0, 0, 0, null], "latitudeDeg": -35.36, "longitudeDeg": 149.16, "altitudeM": 10, "autocontinue": true }]);
    ok(
        &mut engine,
        ui,
        "plan/update",
        json!({ "id": plan["id"], "expectedVersion": 1, "body": body }),
    );

    let folder = ok(&mut engine, ui, "folder/create", json!({ "name": "Ops" }));
    ok(
        &mut engine,
        ui,
        "thread/create",
        json!({ "title": "Survey", "folderId": folder["id"] }),
    );

    let state = engine.state().clone();
    let events = events.0.lock().unwrap().clone();
    (events, state)
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Event>>>);

impl Persistence for Recorder {
    fn append(&mut self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

#[derive(Clone, Default)]
struct Manual(Arc<AtomicU64>);

impl Manual {
    fn advance(&self, us: u64) {
        self.0.fetch_add(us, Ordering::SeqCst);
    }
}

impl Clock for Manual {
    fn now_us(&self) -> u64 {
        1_790_000_000_000_000 + self.0.load(Ordering::SeqCst)
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

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut client = Client {
        id,
        inbox,
        next_id: 0,
    };
    ok(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": kind, "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    client
}

fn ok(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    while let Ok(text) = client.inbox.try_recv() {
        let mut frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            assert!(frame.get("error").is_none(), "{method}: {frame}");
            return frame["result"].take();
        }
    }
    panic!("no reply to {method}")
}

//! Remote vehicles in the app (S86): the relay is asked for what windows
//! watch and nothing more, each window gets its own computer's view, and a
//! window that closes stops its watching.

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
use uxduck_contracts::dto::edge::RemoteView;
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::remote_view::{RemoteViewPort, Wanted};
use uxduck_engine::runtime::{
    Clock, Forget, MemoryCredentials, NoAgent, NoLinks, NoSystem, Ports, Random,
};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

const NOW: u64 = 1_790_000_000_000_000;

#[derive(Clone, Default)]
struct Manual(Arc<AtomicU64>);

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

/// What the engine asked the daemon's viewers for.
#[derive(Clone, Debug, Default)]
struct Viewers(Arc<Mutex<Vec<Wanted>>>, Arc<Mutex<Vec<Wanted>>>);

impl RemoteViewPort for Viewers {
    fn want(&mut self, wanted: Wanted) {
        self.0.lock().unwrap().push(wanted);
    }

    fn want_video(&mut self, cameras: Wanted) {
        self.1.lock().unwrap().push(cameras);
    }
}

impl Viewers {
    /// The cameras last asked to play, by computer (S84).
    fn video(&self) -> Vec<(String, Vec<String>)> {
        self.1
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(c, v)| (c, v.into_iter().collect()))
            .collect()
    }

    fn last(&self) -> Vec<(String, Vec<String>)> {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(c, v)| (c, v.into_iter().collect()))
            .collect()
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str, trust: Trust) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, trust, outbox);
    let mut client = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = send(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": kind, "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1, "approvals": true }
        }),
    )
    .expect("a reply");
    assert!(init.get("result").is_some(), "{init}");
    client
}

fn send(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Option<Value> {
    client.next_id += 1;
    let id = client.next_id;
    engine.receive(
        client.id,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    );
    let mut reply = None;
    while let Ok(text) = client.inbox.try_recv() {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["id"] == id {
            reply = Some(frame);
        } else {
            client.queued.push_back(frame);
        }
    }
    reply
}

fn drain(client: &mut Client) -> Vec<Value> {
    while let Ok(text) = client.inbox.try_recv() {
        client
            .queued
            .push_back(serde_json::from_str(&text).unwrap());
    }
    client.queued.drain(..).collect()
}

fn engine() -> (Engine, Viewers) {
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
    let mut engine = Engine::new(
        config,
        Box::new(Manual::default()),
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    );
    let viewers = Viewers::default();
    engine.set_remote_view(Box::new(viewers.clone()));
    (engine, viewers)
}

fn view(computer: &str, name: &str) -> RemoteView {
    serde_json::from_value(json!({
        "computerId": computer, "computerName": name, "online": true,
        "vehicles": [], "trails": [],
    }))
    .unwrap()
}

fn changed(client: &mut Client) -> Vec<Value> {
    drain(client)
        .into_iter()
        .filter(|f| f["method"] == "remote/changed")
        .map(|f| f["params"].clone())
        .collect()
}

#[test]
fn the_relay_is_asked_for_what_windows_watch_and_each_window_gets_its_computer() {
    let (mut engine, viewers) = engine();
    let mut one = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let mut two = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let watched = send(
        &mut engine,
        &mut one,
        "remote/watch",
        json!({ "computerId": "dvlaptop", "vehicleId": "v1" }),
    )
    .unwrap();
    assert!(watched["result"].is_object(), "{watched}");
    send(
        &mut engine,
        &mut two,
        "remote/watch",
        json!({ "computerId": "dvhangar" }),
    );
    assert_eq!(
        viewers.last(),
        [
            ("dvhangar".to_owned(), vec![]),
            ("dvlaptop".to_owned(), vec!["v1".to_owned()])
        ]
    );

    engine.remote_viewed(view("dvlaptop", "Laptop"));
    assert_eq!(changed(&mut one)[0]["computerName"], "Laptop");
    assert!(changed(&mut two).is_empty(), "not the computer it watches");

    // A second window on the laptop gets what is known at once.
    let watched = send(
        &mut engine,
        &mut two,
        "remote/watch",
        json!({ "computerId": "dvlaptop" }),
    );
    assert!(watched.is_some());
    assert_eq!(changed(&mut two)[0]["computerName"], "Laptop");
    assert_eq!(
        viewers.last(),
        [("dvlaptop".to_owned(), vec!["v1".to_owned()])]
    );

    // Windows that close or stop watching end it.
    engine.disconnect(one.id);
    assert_eq!(viewers.last(), [("dvlaptop".to_owned(), vec![])]);
    send(&mut engine, &mut two, "remote/watch", json!({}));
    assert!(viewers.last().is_empty());
    engine.remote_viewed(view("dvlaptop", "Laptop"));
    assert!(changed(&mut two).is_empty(), "nobody watches");
}

#[test]
fn a_camera_from_away_plays_while_a_window_asks_for_it() {
    let (mut engine, viewers) = engine();
    let mut one = connect(&mut engine, "operator-ui", Trust::LocalUser);
    let mut two = connect(&mut engine, "operator-ui", Trust::LocalUser);
    send(
        &mut engine,
        &mut one,
        "remote/watch",
        json!({ "computerId": "dvlaptop", "vehicleId": "v1", "cameraId": "udp-5600" }),
    );
    send(
        &mut engine,
        &mut two,
        "remote/watch",
        json!({ "computerId": "dvlaptop", "vehicleId": "v1" }),
    );
    assert_eq!(
        viewers.video(),
        [("dvlaptop".to_owned(), vec!["udp-5600".to_owned()])]
    );
    // The read grant comes back in the view, to every window on the laptop.
    let mut view = view("dvlaptop", "Laptop");
    view.video = vec![
        serde_json::from_value(json!({
            "cameraId": "udp-5600",
            "whepUrl": "https://lon1.media.test/live/s1/whep",
            "token": "grant",
        }))
        .unwrap(),
    ];
    engine.remote_viewed(view);
    assert_eq!(changed(&mut one)[0]["video"][0]["token"], "grant");
    // The same window choosing no camera stops it; so does closing.
    send(
        &mut engine,
        &mut one,
        "remote/watch",
        json!({ "computerId": "dvlaptop", "vehicleId": "v1" }),
    );
    assert!(viewers.video().is_empty());
    send(
        &mut engine,
        &mut two,
        "remote/watch",
        json!({ "computerId": "dvlaptop", "cameraId": "rtsp-1" }),
    );
    assert_eq!(
        viewers.video(),
        [("dvlaptop".to_owned(), vec!["rtsp-1".to_owned()])]
    );
    engine.disconnect(two.id);
    assert!(viewers.video().is_empty());
}

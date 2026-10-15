//! Restarts, settings and storage (S13): the safety rules on start, the
//! restart request, the close policy and the storage policy.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::command::{Command, Receipt, ReceiptStatus};
use uxduck_core::domain::controller::{Claim, Controller, TargetMode};
use uxduck_core::domain::link::Link;
use uxduck_core::domain::settings::StorageCategory;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Deleted, Forget, LinkPort, MemoryCredentials, NoAgent,
    Ports, Random, SerialPortInfo, StorageReport, System,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        1_000
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

#[derive(Clone, Default)]
struct Orders;

impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn route_controller(&mut self, _: &str, _: Option<ControlRoute>) {}
}

/// A disk with a given amount free, recording what it was asked to delete.
#[derive(Clone, Default)]
struct Disk(Arc<Mutex<DiskState>>);

#[derive(Default)]
struct DiskState {
    free: u64,
    deletions: Vec<(StorageCategory, u64)>,
}

impl System for Disk {
    fn check_udp_bind(&self, _: &str, _: u16) -> Result<(), String> {
        Ok(())
    }
    fn is_local_address(&self, _: &str) -> bool {
        true
    }
    fn serial_ports(&self) -> Vec<SerialPortInfo> {
        Vec::new()
    }
    fn wake_lock(&mut self, _: bool) {}
    fn storage(&self) -> StorageReport {
        StorageReport {
            data_dir: "/data".into(),
            categories: vec![(StorageCategory::Tlogs, 5_000_000, 3)],
            free_bytes: Some(self.0.lock().unwrap().free),
            total_bytes: Some(500_000_000_000),
        }
    }
    fn delete_older_than(&mut self, category: StorageCategory, cutoff_us: u64) -> Deleted {
        self.0.lock().unwrap().deletions.push((category, cutoff_us));
        Deleted {
            files: 2,
            bytes: 3_000_000,
        }
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
    send(engine, client, method, params).expect("a reply")
}

const DAY_US: u64 = 86_400_000_000;

fn engine(state: State, disk: &Disk) -> Engine {
    engine_with_links(state, disk, Box::new(Orders))
}

/// A data plane that remembers the ground station ids it was told.
#[derive(Clone, Default)]
struct GcsIds(Arc<Mutex<Vec<u8>>>);

impl LinkPort for GcsIds {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn gcs_system_id(&mut self, id: u8) {
        self.0.lock().unwrap().push(id);
    }
}

fn engine_with_links(state: State, disk: &Disk, links: Box<dyn LinkPort>) -> Engine {
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
        links,
        system: Box::new(disk.clone()),
        plugins: Box::new(uxduck_engine::runtime::NoPlugins),
    };
    Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded {
            state,
            sessions: vec![],
        },
    )
}

fn observe(engine: &mut Engine, link_id: &str, armed: bool) {
    engine.vehicle_observed(Observation {
        id: "v".into(),
        aliases: Vec::new(),
        link_id: link_id.into(),
        peer: "127.0.0.1:5760".into(),
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

#[test]
fn a_restart_interrupts_waiting_commands_and_asks_for_controllers_again() {
    let mut state = State::default();
    state.receipts.insert(
        "rc1".into(),
        Receipt {
            id: "rc1".into(),
            vehicle_id: "v".into(),
            command: Command::Rtl,
            status: ReceiptStatus::Sent,
            detail: None,
            reason: None,
            sent_without_recent_heartbeat: false,
            forced: false,
            attempts: 1,
            client_id: "c1".into(),
            operator_id: None,
            remote: None,
            created_at_us: 1,
            completed_at_us: None,
            version: 1,
        },
    );
    state.controllers.insert(
        "g1".into(),
        Controller {
            id: "g1".into(),
            name: "Xbox".into(),
            connected: true,
            claim: Some(Claim {
                client_id: "c1".into(),
                client_name: "uxduck-web".into(),
                operator_id: None,
            }),
            mode: TargetMode::Pinned,
            target: Some("v".into()),
            pending_target: None,
            mapping_id: "mode-2".into(),
            version: 4,
        },
    );
    let mut engine = engine(state, &Disk::default());
    let receipt = &engine.state().receipts["rc1"];
    assert_eq!(receipt.status, ReceiptStatus::Interrupted);
    assert_eq!(
        receipt.detail.as_deref(),
        Some("the daemon restarted before the vehicle answered")
    );
    let pad = &engine.state().controllers["g1"];
    assert!(pad.claim.is_none() && !pad.connected);
    let notice = |engine: &Engine| {
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "Xbox stopped driving after the restart")
    };
    assert!(notice(&engine));

    // Claiming it again clears the notice.
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    observe(&mut engine, link["result"]["id"].as_str().unwrap(), false);
    engine.controller_connected("g1", "Xbox", true);
    engine.controller_neutral("g1", true);
    let claimed = call(
        &mut engine,
        &mut ui,
        "controller/claim",
        json!({ "id": "g1", "mode": "pinned", "vehicleId": "v" }),
    );
    assert!(claimed.get("result").is_some(), "{claimed}");
    assert!(!notice(&engine));
}

#[test]
fn a_restart_is_asked_for_and_confirmed_while_armed() {
    let mut engine = engine(State::default(), &Disk::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    observe(&mut engine, link["result"]["id"].as_str().unwrap(), true);
    let armed = call(&mut engine, &mut ui, "daemon/restart", json!({}));
    assert_eq!(armed["error"]["data"]["kind"], "confirmation-required");
    assert!(
        armed["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Alpha is armed")
    );
    assert!(!engine.restart_requested());
    let mut agent = connect(&mut engine, "agent");
    let refused = call(
        &mut engine,
        &mut agent,
        "daemon/restart",
        json!({ "confirm": true }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "capability-denied");
    let done = call(
        &mut engine,
        &mut ui,
        "daemon/restart",
        json!({ "confirm": true }),
    );
    assert!(done.get("result").is_some());
    assert!(engine.restart_requested());
    let refused = call(&mut engine, &mut agent, "daemon/stop", json!({}));
    assert_eq!(refused["error"]["data"]["kind"], "capability-denied");
    assert!(!engine.stop_requested());
    call(&mut engine, &mut ui, "daemon/stop", json!({}));
    assert!(engine.stop_requested());
}

#[test]
fn the_close_policy_and_the_storage_policy_are_settings() {
    let disk = Disk::default();
    disk.0.lock().unwrap().free = 200_000_000;
    let mut engine = engine(State::default(), &disk);
    let mut ui = connect(&mut engine, "operator-ui");
    let settings = call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["settings"] }),
    )["result"]["snapshot"]["settings"]
        .clone();
    assert_eq!(
        settings,
        json!({
            "closePolicy": "keep-running",
            "closePolicySet": false,
            "autoDelete": [],
            // ADR-0013 P2: analytics and conversations on, flight data off.
            "usageAnalytics": true,
            "shareConversations": true,
            "shareFlightData": false,
            "videoPorts": [],
            "videoRecording": "while-armed",
            // S64: the tray on, starting at login off.
            "showInMenuBar": true,
            "connectToAccount": true,
            "watchFromAccount": true,
            "remoteControl": [],
            "supervisionGraceS": 10,
            // The usual ground station's, uxduck_mavlink::GCS_SYSTEM_ID.
            "gcsSystemId": 255,
            "storeFlights": true,
            "uploadWhileArmed": true,
            "uploadVideoWhileFlying": false,
            "startAtLogin": false,
            // S26: warnings and worse become notifications.
            "messageNotifySeverity": 4,
            // Last time's simulations are not remembered until turned on.
            "recreateSimulations": false,
            // S23: ground heights from ALOS until changed.
            "terrainDataset": "alos",
            "version": 0
        })
    );
    let refused = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "autoDelete": [{ "category": "event-log", "days": 30 }] }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
    let saved = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "closePolicy": "quit",
                "autoDelete": [{ "category": "tlogs", "days": 30 }] }),
    );
    assert_eq!(saved["result"]["closePolicy"], "quit", "{saved}");
    let stale = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "closePolicy": "keep-running" }),
    );
    assert_eq!(stale["error"]["data"]["kind"], "version-conflict");
    assert!(!engine.should_quit(), "a window is open");
    engine.disconnect(ui.id);
    assert!(engine.should_quit(), "quit, as set from the browser");

    // Storage: old tlogs go, and low disk space is a warning.
    engine.check_storage();
    assert_eq!(
        disk.0.lock().unwrap().deletions,
        [(StorageCategory::Tlogs, 1_000u64.saturating_sub(30 * DAY_US))]
    );
    let titles: Vec<String> = engine
        .state()
        .notifications
        .values()
        .map(|n| n.title.clone())
        .collect();
    assert!(
        titles.contains(&"Low disk space: 200 MB left".to_owned()),
        "{titles:?}"
    );
    disk.0.lock().unwrap().free = 50_000_000_000;
    engine.check_storage();
    assert!(
        !engine
            .state()
            .notifications
            .values()
            .any(|n| n.title.starts_with("Low disk"))
    );
    let mut cli = connect(&mut engine, "operator-cli");
    let usage = call(&mut engine, &mut cli, "storage/usage", json!({}));
    assert_eq!(usage["result"]["categories"][0]["bytes"], 5_000_000);
    assert_eq!(usage["result"]["categories"].as_array().unwrap().len(), 10);
    assert_eq!(
        usage["result"]["categories"][9]["category"],
        "vehicle-files"
    );
    assert_eq!(usage["result"]["categories"][9]["deletable"], false);
}

/// The ground station system id (ADR-0004): 255 unless set, 1 to 255,
/// told to the data plane at start and when it changes, and what the
/// failsafe check compares a vehicle's `SYSID_MYGCS` with.
#[test]
fn the_ground_station_system_id_is_a_setting_the_data_plane_is_told() {
    use uxduck_core::domain::parameter::ParamKind;
    use uxduck_engine::transfers::Reported;
    assert_eq!(
        uxduck_core::domain::settings::Settings::default().gcs_system_id,
        uxduck_mavlink::GCS_SYSTEM_ID,
        "the constant is the default"
    );
    let ids = GcsIds::default();
    let mut engine = engine_with_links(State::default(), &Disk::default(), Box::new(ids.clone()));
    assert_eq!(*ids.0.lock().unwrap(), [255], "told before any link opens");
    let mut ui = connect(&mut engine, "operator-ui");

    let zero = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "gcsSystemId": 0 }),
    );
    assert_eq!(zero["error"]["data"]["kind"], "invalid-value", "{zero}");
    let too_big = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "gcsSystemId": 256 }),
    );
    assert!(too_big.get("error").is_some(), "{too_big}");

    let saved = call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 0, "gcsSystemId": 42 }),
    );
    assert_eq!(saved["result"]["gcsSystemId"], 42, "{saved}");
    assert_eq!(engine.state().settings.gcs_system_id, 42);
    assert_eq!(*ids.0.lock().unwrap(), [255, 42]);
    // Another setting changing does not tell it again.
    call(
        &mut engine,
        &mut ui,
        "settings/update",
        json!({ "expectedVersion": 1, "showInMenuBar": false }),
    );
    assert_eq!(*ids.0.lock().unwrap(), [255, 42]);

    // A vehicle listening for 255 would not notice UXDuck, now 42, stop.
    observe(&mut engine, "l1", false);
    let sysid_mygcs = |value: f64| {
        Ok(vec![
            Reported {
                name: "FS_GCS_ENABLE".into(),
                value: 1.0,
                kind: ParamKind::Int8,
            },
            Reported {
                name: "SYSID_MYGCS".into(),
                value,
                kind: ParamKind::Int16,
            },
        ])
    };
    engine.parameters_downloaded("v", sysid_mygcs(255.0), &std::collections::BTreeMap::new());
    let vehicle = engine.vehicle("v").unwrap().clone();
    let problem = engine.failsafe_plan(&vehicle).problem.expect("a problem");
    assert!(problem.contains("Set SYSID_MYGCS to 42"), "{problem}");
    engine.parameters_downloaded("v", sysid_mygcs(42.0), &std::collections::BTreeMap::new());
    assert!(engine.failsafe_plan(&vehicle).unattended_ok());
}

/// `daemon/about` (S87): the handshake's version and the stage this
/// UXDuck runs at, with what the host said of its build and where it
/// listens, and how long it has run. Reading needs only `read`.
#[test]
fn daemon_about_says_which_uxduck_and_how_it_was_built() {
    let disk = Disk::default();
    let mut engine = Engine::new(
        EngineConfig {
            daemon_version: "0.3.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:47800/".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout {
                commit: Some("ce4c7dd".into()),
                built_at: Some("2026-10-05T09:30:00Z".into()),
                target: "aarch64-apple-darwin".into(),
                data_dir: Some("/data".into()),
                port: Some(47800),
            },
        },
        Box::new(Tick),
        Box::new(Counter(0)),
        Ports {
            persistence: Box::new(Forget),
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(Orders),
            system: Box::new(disk),
            plugins: Box::new(uxduck_engine::runtime::NoPlugins),
        },
        Loaded::default(),
    );
    let mut cli = connect(&mut engine, "operator-cli");
    let about = call(&mut engine, &mut cli, "daemon/about", json!({}));
    let result = &about["result"];
    assert_eq!(result["version"], "0.3.0");
    assert_eq!(result["runsIn"], "installed");
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["releaseStage"], "flight");
    assert_eq!(result["commit"], "ce4c7dd");
    assert_eq!(result["builtAt"], "2026-10-05T09:30:00Z");
    assert_eq!(result["target"], "aarch64-apple-darwin");
    assert_eq!(result["dataDir"], "/data");
    assert_eq!(result["port"], 47800);
    assert_eq!(result["dev"], cfg!(debug_assertions));
    // The test clock stands still, so it has run for no time at all.
    assert_eq!(result["uptimeUs"], 0);
    // It passes the contract's own DTO.
    serde_json::from_value::<uxduck_contracts::dto::settings::DaemonAbout>(result.clone())
        .expect("a DaemonAbout");
}

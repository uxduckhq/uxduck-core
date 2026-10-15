//! Plugins (S14): install as a client with the manifest's capabilities,
//! commands under the plugin's own id, failures, and no socket client
//! passing for a plugin.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::float_cmp,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::command::Command;
use uxduck_contracts::dto::command::Risk;
use uxduck_contracts::dto::initialize::Capability;
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::notification::Severity;
use uxduck_contracts::dto::plugin::{
    CommandCall, DeclaredCommand, Description, Effects, MapFeature, MessageSubscription, Panel,
    PluginDraft, PluginManifest, PluginNotification, PluginTestResult, ProducedItem, SendCommand,
    SendMessage, Subscriptions, Widget,
};
use uxduck_contracts::dto::telemetry::Unit;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::plugin::PluginStatus;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, DraftOrder, Forget, LinkPort, MemoryCredentials, MessageOrder, NoAgent,
    NoSystem, PluginLoad, PluginPort, Ports, Random, TestOrder,
};
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

/// What the engine asked the data plane and the plugin host for.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>, Drafts);

/// Agents' drafts, as the daemon's folder would hold them (S58), and the
/// test runs asked for.
#[derive(Clone, Default)]
struct Drafts(Arc<Mutex<(Vec<PluginManifest>, Vec<TestOrder>)>>);

impl Calls {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
    fn push(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
}

impl LinkPort for Calls {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, order: CommandOrder) {
        self.push(format!("command {} {:?}", order.vehicle_id, order.command));
    }
    fn message(&mut self, order: MessageOrder) {
        self.push(format!(
            "message {} {} {}",
            order.vehicle_id, order.name, order.plugin_id
        ));
    }
}

fn manifest(id: &str, capabilities: Vec<Capability>) -> PluginManifest {
    PluginManifest {
        abi: 2,
        id: id.into(),
        name: "Camera".into(),
        version: "0.2.0".into(),
        description: None,
        wasm: "camera_manager.wasm".into(),
        script: None,
        capabilities,
        subscribe: Subscriptions {
            messages: Some(MessageSubscription {
                names: vec!["COMMAND_ACK".into()],
                cadence_ms: 200,
            }),
            ..Subscriptions::default()
        },
        sends: vec!["MANUAL_CONTROL".into()],
        commands: vec![
            DeclaredCommand {
                name: "photo".into(),
                title: "Take a photo".into(),
                description: String::new(),
                parameters: None,
                risk: Risk::None,
                shortcut: Some("Alt+C".into()),
            },
            DeclaredCommand {
                name: "zoom".into(),
                title: "Zoom".into(),
                description: "Steps the camera's zoom in or out.".into(),
                parameters: Some(json!({
                    "type": "object",
                    "properties": { "step": { "type": "integer", "minimum": -10, "maximum": 10 } },
                    "required": ["step"]
                })),
                risk: Risk::None,
                shortcut: None,
            },
            DeclaredCommand {
                name: "drop".into(),
                title: "Drop the payload".into(),
                description: "Opens the release.".into(),
                parameters: None,
                risk: Risk::Dangerous,
                shortcut: None,
            },
        ],
        written_by: None,
        dialect: None,
    }
}

impl PluginPort for Calls {
    fn inspect(&self, path: &str) -> Result<PluginManifest, String> {
        match path {
            "/plugins/camera" => Ok(manifest("camera-manager", vec![Capability::Read])),
            "/plugins/v1" => Ok(PluginManifest {
                abi: 1,
                ..manifest("old", vec![Capability::Read])
            }),
            "/plugins/control" => Ok(manifest(
                "controller",
                vec![Capability::Read, Capability::Control],
            )),
            path => {
                let id = path.strip_prefix("/drafts/").unwrap_or_default();
                (self.1)
                    .0
                    .lock()
                    .unwrap()
                    .0
                    .iter()
                    .find(|m| m.id == id)
                    .cloned()
                    .ok_or_else(|| format!("{path}: no readable plugin.json"))
            }
        }
    }
    fn draft(&mut self, order: DraftOrder) -> Result<PluginDraft, String> {
        let mut held = (self.1).0.lock().unwrap();
        let version = held.0.iter().filter(|m| m.id == order.params.id).count();
        held.0.retain(|m| m.id != order.params.id);
        let manifest = PluginManifest {
            abi: 2,
            id: order.params.id.clone(),
            name: order.params.name.clone(),
            version: format!("0.1.{version}"),
            description: None,
            wasm: String::new(),
            script: Some("index.ts".into()),
            capabilities: order
                .params
                .capabilities
                .clone()
                .unwrap_or(vec![Capability::Read]),
            subscribe: order.params.subscribe.clone(),
            sends: Vec::new(),
            commands: Vec::new(),
            written_by: Some(order.written_by.clone()),
            dialect: None,
        };
        held.0.insert(0, manifest.clone());
        Ok(PluginDraft {
            id: manifest.id.clone(),
            name: manifest.name,
            version: manifest.version,
            path: format!("/drafts/{}", manifest.id),
            written_by: manifest.written_by,
            updated_at_us: 1,
        })
    }
    fn drafts(&self) -> Vec<PluginDraft> {
        (self.1)
            .0
            .lock()
            .unwrap()
            .0
            .iter()
            .map(|m| PluginDraft {
                id: m.id.clone(),
                name: m.name.clone(),
                version: m.version.clone(),
                path: format!("/drafts/{}", m.id),
                written_by: m.written_by.clone(),
                updated_at_us: 1,
            })
            .collect()
    }
    fn test(&mut self, order: TestOrder) -> bool {
        (self.1).0.lock().unwrap().1.push(order);
        true
    }
    fn load(&mut self, plugin: PluginLoad) {
        self.push(format!("load {} {}", plugin.id, plugin.module));
    }
    fn unload(&mut self, id: &str) {
        self.push(format!("unload {id}"));
    }
    fn command(&mut self, id: &str, call: CommandCall) {
        self.push(format!(
            "call {id} {} {} {}",
            call.command,
            call.vehicle_id.as_deref().unwrap_or("-"),
            call.params
        ));
    }
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
    /// Its client id, as `initialize` gave it.
    me: String,
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

/// The journal, as plain events.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<uxduck_core::events::Event>>>);

impl uxduck_engine::runtime::Persistence for Journal {
    fn append(&mut self, event: &uxduck_core::events::Event) {
        self.0.lock().unwrap().push(event.clone());
    }
    fn checkpoint(&mut self, _: &uxduck_engine::state::State) {}
    fn save_session(&mut self, _: &str, _: &uxduck_engine::pairing::Session) {}
}

fn setup() -> (Engine, Client, Calls) {
    setup_with(Box::new(Forget))
}

fn setup_with(
    persistence: Box<dyn uxduck_engine::runtime::Persistence>,
) -> (Engine, Client, Calls) {
    let calls = Calls::default();
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
        persistence,
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(calls.clone()),
        system: Box::new(NoSystem::default()),
        plugins: Box::new(calls.clone()),
    };
    let mut engine = Engine::new(
        config,
        Box::new(Tick),
        Box::new(Counter(0)),
        ports,
        Loaded::default(),
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let link = call(
        &mut engine,
        &mut ui,
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    engine.vehicle_observed(Observation {
        id: "v".into(),
        aliases: Vec::new(),
        link_id: link["result"]["id"].as_str().unwrap().into(),
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
        armed: false,
        mode: None,
    });
    calls.take();
    (engine, ui, calls)
}

fn photo() -> SendCommand {
    SendCommand {
        vehicle_id: "v".into(),
        command: Command::Long {
            command: 2000,
            params: vec![0.0, 0.0, 1.0],
        },
    }
}

#[test]
fn a_plugin_installs_as_a_client_and_acts_under_its_own_id() {
    let (mut engine, mut ui, calls) = setup();
    let mut agent = connect(&mut engine, "agent");
    // An agent's install waits for an operator (S58); this one is denied.
    let held = send(
        &mut engine,
        &mut agent,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    assert!(held.is_none(), "held for an operator");
    let asked = engine.state().approvals.values().next().cloned().unwrap();
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": asked.id, "allow": false }),
    );
    assert!(engine.state().plugins.is_empty());
    let missing = call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/nowhere" }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "invalid-value");

    let installed = call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    assert_eq!(installed["result"]["status"], "starting", "{installed}");
    assert_eq!(installed["result"]["clientId"], "plugin:camera-manager");
    assert_eq!(
        calls.take(),
        ["load camera-manager /plugins/camera/camera_manager.wasm"]
    );
    let record = engine
        .state()
        .clients
        .values()
        .find(|c| c.name == "Camera")
        .cloned()
        .unwrap();
    assert_eq!(
        record.capabilities,
        [uxduck_core::domain::client::Capability::Read]
    );

    engine.plugin_described(
        "camera-manager",
        Description {
            panels: vec![Panel {
                id: "camera".into(),
                title: "Camera".into(),
                widgets: vec![Widget::Button {
                    label: "Take a photo".into(),
                    command: "photo".into(),
                    params: None,
                    tone: None,
                    help: None,
                }],
                tab: false,
                vehicle: false,
            }],
        },
    );
    assert_eq!(
        engine.state().plugins["camera-manager"].status,
        PluginStatus::Running
    );
    call(
        &mut engine,
        &mut ui,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "photo", "vehicleId": "v" }),
    );
    assert_eq!(calls.take(), ["call camera-manager photo v {}"]);
    // Only the commands it declares, with their parameters.
    let undeclared = call(
        &mut engine,
        &mut ui,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "format-card", "vehicleId": "v" }),
    );
    assert_eq!(undeclared["error"]["data"]["kind"], "not-found");
    call(
        &mut engine,
        &mut ui,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "zoom", "vehicleId": "v",
                "params": { "step": 3 } }),
    );
    assert_eq!(calls.take(), [r#"call camera-manager zoom v {"step":3}"#]);

    // Its manifest gave it only `read`: its command waits for an operator.
    engine.plugin_effects(
        "camera-manager",
        Effects {
            commands: vec![photo()],
            items: vec![ProducedItem {
                vehicle_id: Some("v".into()),
                key: "camera.photos".into(),
                label: "Photos".into(),
                unit: Unit::Count,
                value: Some(1.0),
                text: None,
                rate_per_s: None,
            }],
            ..Effects::default()
        },
    );
    assert!(calls.take().is_empty(), "held, not sent");
    let approval = engine.state().approvals.values().next().cloned().unwrap();
    assert_eq!(approval.client_name, "Camera");
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );
    assert_eq!(calls.take().len(), 1, "sent once allowed");
    let receipt = engine.state().receipts.values().next().cloned().unwrap();
    assert_eq!(receipt.client_id, "plugin:camera-manager");
    let telemetry = call(&mut engine, &mut ui, "telemetry/get", json!({ "id": "v" }));
    assert!(
        telemetry["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["key"] == "camera.photos" && i["source"] == "plugin:camera-manager"),
        "{telemetry}"
    );
    // Allowed always: the next one goes straight out.
    engine.plugin_effects(
        "camera-manager",
        Effects {
            commands: vec![photo()],
            ..Effects::default()
        },
    );
    assert_eq!(calls.take().len(), 1);
}

#[test]
fn v2_effects_messages_the_manifest_allows_items_that_say_their_units_and_notices() {
    let (mut engine, mut ui, calls) = setup();
    let old = call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/v1" }),
    );
    assert_eq!(old["error"]["data"]["kind"], "invalid-value");
    assert!(
        old["error"]["message"].as_str().unwrap().contains("ABI v1"),
        "{old}"
    );

    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/control" }),
    );
    engine.plugin_described("controller", Description::default());
    calls.take();
    let message = |name: &str| SendMessage {
        vehicle_id: "v".into(),
        name: name.into(),
        fields: serde_json::Map::new(),
    };
    let item = |key: &str, unit: Unit| ProducedItem {
        vehicle_id: Some("v".into()),
        key: key.into(),
        label: key.into(),
        unit,
        value: Some(12.0),
        text: None,
        rate_per_s: None,
    };
    engine.plugin_effects(
        "controller",
        Effects {
            messages: vec![message("MANUAL_CONTROL"), message("HEARTBEAT")],
            items: vec![
                item("home.distance_m", Unit::Meter),
                item("home.distance", Unit::Meter),
            ],
            notifications: vec![PluginNotification {
                vehicle_id: Some("v".into()),
                severity: Severity::Warning,
                title: "Battery at 19 %".into(),
                body: None,
            }],
            ..Effects::default()
        },
    );
    // HEARTBEAT is not in its manifest's sends: refused.
    assert_eq!(calls.take(), ["message v MANUAL_CONTROL controller"]);
    let telemetry = call(&mut engine, &mut ui, "telemetry/get", json!({ "id": "v" }));
    let keys: Vec<&str> = telemetry["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"home.distance_m"), "{keys:?}");
    assert!(
        !keys.contains(&"home.distance"),
        "a key that does not say its unit"
    );
    assert!(
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "Camera: Battery at 19 %" && n.report.is_none()),
        "a plugin's say is not UXDuck going wrong: nothing to report"
    );

    // Without control, nothing it sends goes out.
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    engine.plugin_described("camera-manager", Description::default());
    calls.take();
    engine.plugin_effects(
        "camera-manager",
        Effects {
            messages: vec![message("MANUAL_CONTROL")],
            ..Effects::default()
        },
    );
    assert!(calls.take().is_empty());
}

#[test]
fn a_plugins_features_are_drawn_moved_and_removed_for_those_who_asked() {
    let (mut engine, mut ui, _) = setup();
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/control" }),
    );
    engine.plugin_described("controller", Description::default());
    let first = call(&mut engine, &mut ui, "plugin/features", json!({}));
    assert_eq!(first["result"]["layers"], json!([]));
    let point = |lon: f64| MapFeature {
        id: "home".into(),
        geometry: json!({ "type": "Point", "coordinates": [lon, -35.0] }),
        label: Some("Home".into()),
        tone: None,
        fill: false,
        dashed: false,
    };
    let changed = |ui: &mut Client| -> Vec<Value> {
        let mut frames: Vec<Value> = ui.queued.drain(..).collect();
        while let Ok(text) = ui.inbox.try_recv() {
            frames.push(serde_json::from_str(&text).unwrap());
        }
        frames
            .into_iter()
            .filter(|f| f["method"] == "plugin/featuresChanged")
            .map(|f| f["params"].clone())
            .collect()
    };
    engine.plugin_effects(
        "controller",
        Effects {
            features: vec![point(149.0)],
            ..Effects::default()
        },
    );
    engine.plugin_effects(
        "controller",
        Effects {
            features: vec![point(149.5)],
            ..Effects::default()
        },
    );
    let layers = changed(&mut ui);
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[1]["name"], "Camera");
    assert_eq!(
        layers[1]["features"].as_array().unwrap().len(),
        1,
        "replaced by id"
    );
    assert_eq!(
        layers[1]["features"][0]["geometry"]["coordinates"][0],
        149.5
    );
    let listed = call(&mut engine, &mut ui, "plugin/features", json!({}));
    assert_eq!(listed["result"]["layers"][0]["pluginId"], "controller");
    changed(&mut ui);

    engine.plugin_effects(
        "controller",
        Effects {
            removed_features: vec!["home".into()],
            ..Effects::default()
        },
    );
    assert_eq!(changed(&mut ui)[0]["features"], json!([]));
    engine.plugin_effects(
        "controller",
        Effects {
            features: vec![point(149.0)],
            ..Effects::default()
        },
    );
    changed(&mut ui);
    // Removing the plugin takes its layer off the map.
    call(
        &mut engine,
        &mut ui,
        "plugin/remove",
        json!({ "id": "controller" }),
    );
    assert_eq!(changed(&mut ui)[0]["features"], json!([]));
}

#[test]
fn a_plugins_own_items_and_tab_follow_no_vehicle() {
    let journal = Journal::default();
    let (mut engine, mut ui, calls) = setup_with(Box::new(journal.clone()));
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    // A panel that is a tab of its own, with a button that has parameters.
    engine.plugin_described(
        "camera-manager",
        Description {
            panels: vec![Panel {
                id: "timer".into(),
                title: "Timer".into(),
                widgets: vec![Widget::Button {
                    label: "5 min".into(),
                    command: "zoom".into(),
                    params: json!({ "step": 5 }).as_object().cloned(),
                    tone: None,
                    help: None,
                }],
                tab: true,
                vehicle: false,
            }],
        },
    );
    let plugin = &engine.state().plugins["camera-manager"];
    assert!(plugin.panels[0].tab);
    let wire = uxduck_contracts::dto::plugin::Plugin::from(plugin);
    assert_eq!(
        serde_json::to_value(&wire.panels[0].widgets[0]).unwrap()["params"],
        json!({ "step": 5 }),
        "a button's parameters are JSON again on the wire"
    );
    calls.take();

    let first = call(&mut engine, &mut ui, "plugin/items", json!({}));
    assert_eq!(first["result"]["plugins"], json!([]));
    let changed = |ui: &mut Client| -> Vec<Value> {
        let mut frames: Vec<Value> = ui.queued.drain(..).collect();
        while let Ok(text) = ui.inbox.try_recv() {
            frames.push(serde_json::from_str(&text).unwrap());
        }
        frames
            .into_iter()
            .filter(|f| f["method"] == "plugin/itemsChanged")
            .map(|f| f["params"].clone())
            .collect()
    };
    let elapsed = |s: f64| ProducedItem {
        vehicle_id: None,
        key: "stopwatch.elapsed_s".into(),
        label: "Stopwatch".into(),
        unit: Unit::Second,
        value: Some(s),
        text: None,
        rate_per_s: None,
    };
    let produce = |engine: &mut Engine, item: ProducedItem| {
        engine.plugin_effects(
            "camera-manager",
            Effects {
                items: vec![item],
                ..Effects::default()
            },
        );
    };
    produce(&mut engine, elapsed(1.0));
    // The same value again is not sent again; a key without its unit is refused.
    produce(&mut engine, elapsed(1.0));
    produce(
        &mut engine,
        ProducedItem {
            key: "stopwatch.elapsed".into(),
            ..elapsed(2.0)
        },
    );
    produce(&mut engine, elapsed(2.0));
    let sent = changed(&mut ui);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1]["pluginId"], "camera-manager");
    assert_eq!(sent[1]["items"][0]["key"], "stopwatch.elapsed_s");
    assert_eq!(sent[1]["items"][0]["unit"], "second");
    assert_eq!(sent[1]["items"][0]["value"], 2.0);
    assert_eq!(sent[1]["items"].as_array().unwrap().len(), 1);
    // None of it reached a vehicle's telemetry.
    let telemetry = call(&mut engine, &mut ui, "telemetry/get", json!({ "id": "v" }));
    assert!(!telemetry.to_string().contains("stopwatch"), "{telemetry}");
    let listed = call(&mut engine, &mut ui, "plugin/items", json!({}));
    assert_eq!(
        listed["result"]["plugins"][0]["items"][0]["value"], 2.0,
        "{listed}"
    );

    // A button in its tab calls it with no vehicle, journaled with none.
    let ran = call(
        &mut engine,
        &mut ui,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "zoom", "params": { "step": 5 } }),
    );
    assert!(ran.get("error").is_none(), "{ran}");
    assert_eq!(calls.take(), [r#"call camera-manager zoom - {"step":5}"#]);
    let called = journal
        .0
        .lock()
        .unwrap()
        .iter()
        .find_map(|e| match &e.change {
            uxduck_core::events::Change::PluginCalled { vehicle_id, .. } => {
                Some(vehicle_id.clone())
            }
            _ => None,
        });
    assert_eq!(called, Some(None));

    // Removing the plugin takes its items away.
    call(
        &mut engine,
        &mut ui,
        "plugin/remove",
        json!({ "id": "camera-manager" }),
    );
    assert_eq!(changed(&mut ui).last().unwrap()["items"], json!([]));
}

#[test]
fn the_agent_finds_a_plugins_command_and_runs_it_with_checked_parameters() {
    let journal = Journal::default();
    let (mut engine, mut ui, calls) = setup_with(Box::new(journal.clone()));
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    engine.plugin_described("camera-manager", Description::default());
    calls.take();
    let mut agent = connect(&mut engine, "agent");

    // Found by words, with its schema.
    let found = call(
        &mut engine,
        &mut agent,
        "tool/search",
        json!({ "query": "zoom in" }),
    );
    let tools = found["result"]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["command"], "zoom", "{found}");
    assert_eq!(tools[0]["pluginName"], "Camera");
    assert_eq!(tools[0]["parameters"]["required"], json!(["step"]));
    let none = call(
        &mut engine,
        &mut agent,
        "tool/search",
        json!({ "query": "parachute" }),
    );
    assert_eq!(none["result"]["tools"], json!([]));

    // Parameters its schema does not allow are refused before anything runs.
    for (params, why) in [
        (json!({}), "params.step: missing"),
        (json!({ "step": 99 }), "params.step: at most 10"),
    ] {
        let refused = call(
            &mut engine,
            &mut ui,
            "plugin/command",
            json!({ "pluginId": "camera-manager", "command": "zoom", "vehicleId": "v", "params": params }),
        );
        assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
        assert_eq!(refused["error"]["message"], why);
    }
    let extra = call(
        &mut engine,
        &mut ui,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "photo", "vehicleId": "v", "params": { "x": 1 } }),
    );
    assert_eq!(extra["error"]["message"], "params: this command takes none");
    assert!(calls.take().is_empty());

    // An agent without control waits for an operator, who sees what it asks.
    let held = send(
        &mut engine,
        &mut agent,
        "plugin/command",
        json!({ "pluginId": "camera-manager", "command": "zoom", "vehicleId": "v", "params": { "step": 2 } }),
    );
    assert!(held.is_none(), "held for an approval");
    let approval = engine.state().approvals.values().next().cloned().unwrap();
    assert_eq!(approval.summary, "run the Camera plugin's “Zoom” on Alpha");
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true }),
    );
    assert_eq!(calls.take(), [r#"call camera-manager zoom v {"step":2}"#]);

    // Always allowed, a command runs at once from then on, but one the
    // plugin declares dangerous still asks each time.
    let zoom = json!({ "pluginId": "camera-manager", "command": "zoom", "vehicleId": "v", "params": { "step": 1 } });
    assert!(send(&mut engine, &mut agent, "plugin/command", zoom.clone()).is_none());
    let pending = |engine: &Engine| {
        engine
            .state()
            .approvals
            .values()
            .find(|a| a.state == uxduck_core::domain::approval::ApprovalState::Pending)
            .cloned()
    };
    let asked = pending(&engine).expect("asked");
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": asked.id, "allow": true, "remember": "always" }),
    );
    calls.take();
    assert!(
        send(&mut engine, &mut agent, "plugin/command", zoom).is_some(),
        "at once"
    );
    assert_eq!(calls.take(), [r#"call camera-manager zoom v {"step":1}"#]);
    let drop = json!({ "pluginId": "camera-manager", "command": "drop", "vehicleId": "v" });
    assert!(
        send(&mut engine, &mut agent, "plugin/command", drop).is_none(),
        "held"
    );
    assert!(calls.take().is_empty());
    let dangerous = pending(&engine).expect("asked again");
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": dangerous.id, "allow": true }),
    );
    assert_eq!(calls.take(), ["call camera-manager drop v {}"]);
    // Journaled under the agent.
    let events = journal.0.lock().unwrap();
    let called = events
        .iter()
        .find(|e| matches!(e.change, uxduck_core::events::Change::PluginCalled { .. }))
        .expect("the call is journaled");
    assert_eq!(
        called.actor.client_id, agent.me,
        "under the agent that asked"
    );
}

#[test]
fn an_agent_drafts_tests_and_installs_a_plugin_only_when_an_operator_says_so() {
    let journal = Journal::default();
    let (mut engine, mut ui, calls) = setup_with(Box::new(journal.clone()));
    let thread = call(&mut engine, &mut ui, "thread/create", json!({}))["result"].clone();
    let thread_id = thread["id"].as_str().unwrap().to_owned();
    let mut agent = connect(&mut engine, "agent");
    // Even an agent the operator lets configure everything still asks.
    let record = engine
        .state()
        .clients
        .values()
        .find(|c| c.name == "agent")
        .cloned()
        .unwrap();
    call(
        &mut engine,
        &mut ui,
        "client/setCapabilities",
        json!({ "id": record.id, "expectedVersion": record.version,
                "capabilities": ["read", "control", "config"] }),
    );

    let drafted = call(
        &mut engine,
        &mut agent,
        "plugin/draft",
        json!({ "id": "distance-from-home", "name": "Distance from home",
                "script": "export function onItems() { return {}; }", "threadId": thread_id }),
    );
    assert_eq!(
        drafted["result"]["path"], "/drafts/distance-from-home",
        "{drafted}"
    );
    assert_eq!(drafted["result"]["writtenBy"]["agent"], "agent");
    assert_eq!(
        drafted["result"]["writtenBy"]["threadId"],
        thread_id.as_str()
    );
    let bad = call(
        &mut engine,
        &mut agent,
        "plugin/draft",
        json!({ "id": "Bad Id", "name": "x", "script": "" }),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid-value");

    // The test runs off the engine's thread and answers when it ends.
    let pending = send(
        &mut engine,
        &mut agent,
        "plugin/test",
        json!({ "id": "distance-from-home", "vehicleId": "v" }),
    );
    assert!(pending.is_none(), "answered later");
    let order = (calls.1).0.lock().unwrap().1.pop().unwrap();
    assert_eq!(order.path, "/drafts/distance-from-home");
    assert_eq!(order.flight.as_ref().unwrap().system_id, 1);
    engine.plugin_tested(
        order.connection,
        &order.request_id,
        Ok(PluginTestResult {
            flight_s: 300.0,
            ..PluginTestResult::default()
        }),
    );
    let tested: Value = std::iter::from_fn(|| agent.inbox.try_recv().ok())
        .map(|t| serde_json::from_str::<Value>(&t).unwrap())
        .find(|f| f.get("result").is_some())
        .unwrap();
    assert_eq!(tested["result"]["flightS"], 300.0);

    // Installing waits for an operator, worded with what it may do and the chat.
    let held = send(
        &mut engine,
        &mut agent,
        "plugin/install",
        json!({ "path": "/drafts/distance-from-home" }),
    );
    assert!(held.is_none(), "held");
    let approval = engine
        .state()
        .approvals
        .values()
        .find(|a| a.method == "plugin/install")
        .cloned()
        .unwrap();
    assert_eq!(
        approval.summary,
        format!(
            "install the plugin “Distance from home” 0.1.0, which may read, written in the chat “{}”",
            thread["title"].as_str().unwrap()
        )
    );
    // Only once: an install is never remembered.
    call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": approval.id, "allow": true, "remember": "always" }),
    );

    let plugin = engine.state().plugins["distance-from-home"].clone();
    assert_eq!(
        plugin.written_by.unwrap().thread_id.as_deref(),
        Some(thread_id.as_str())
    );
    let events = journal.0.lock().unwrap();
    let installed = events
        .iter()
        .rev()
        .find(|e| matches!(e.change, uxduck_core::events::Change::PluginUpserted { .. }))
        .unwrap();
    assert_eq!(
        installed.actor.client_id, agent.me,
        "the journal names the agent"
    );
    drop(events);
    let again = send(
        &mut engine,
        &mut agent,
        "plugin/install",
        json!({ "path": "/drafts/distance-from-home" }),
    );
    assert!(again.is_none(), "asked again");
}

#[test]
fn a_failed_plugin_is_stopped_and_said_so_and_sockets_cannot_be_plugins() {
    let (mut engine, mut ui, calls) = setup();
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    calls.take();
    engine.plugin_failed("camera-manager", "on_frames: wasm trap: out of fuel");
    let plugin = &engine.state().plugins["camera-manager"];
    assert_eq!(plugin.status, PluginStatus::Failed);
    assert!(
        engine
            .state()
            .notifications
            .values()
            .any(|n| n.title == "The Camera plugin stopped")
    );
    engine.plugin_effects(
        "camera-manager",
        Effects {
            commands: vec![photo()],
            ..Effects::default()
        },
    );
    assert!(calls.take().is_empty(), "a stopped plugin does nothing");
    let removed = call(
        &mut engine,
        &mut ui,
        "plugin/remove",
        json!({ "id": "camera-manager" }),
    );
    assert!(removed.get("result").is_some());
    assert!(engine.state().plugins.is_empty());

    let (outbox, mut inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    engine.receive(
        id,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "clientInfo": { "name": "sneaky", "version": "0.1.0", "kind": "plugin" },
            "capabilities": { "protocolVersion": 1 } } })
        .to_string(),
    );
    let reply: Value = serde_json::from_str(&inbox.try_recv().unwrap()).unwrap();
    assert_eq!(reply["error"]["data"]["kind"], "invalid-params");
}

/// S104: an operator turns a plugin off and on without removing it; an
/// agent may not; off, it stays off and drops what it used.
#[test]
fn an_operator_turns_a_plugin_off_and_on_and_it_stays_as_left() {
    let journal = Journal::default();
    let (mut engine, mut ui, calls) = setup_with(Box::new(journal.clone()));
    call(
        &mut engine,
        &mut ui,
        "plugin/install",
        json!({ "path": "/plugins/camera" }),
    );
    engine.plugin_described("camera-manager", Description::default());
    engine.plugins_used(vec![uxduck_contracts::dto::plugin::PluginUsage {
        plugin_id: "camera-manager".into(),
        calls_per_min: 12,
        busy_pct: 0.2,
        slowest_ms: 3.0,
        storage_bytes: 10,
        storage_limit_bytes: 65_536,
        memory_limit_bytes: 16_777_216,
    }]);
    let usage = call(&mut engine, &mut ui, "plugin/usage", json!({}));
    assert_eq!(usage["result"]["plugins"][0]["callsPerMin"], 12);
    calls.take();

    let mut agent = connect(&mut engine, "agent");
    let refused = call(
        &mut engine,
        &mut agent,
        "plugin/setEnabled",
        json!({ "id": "camera-manager", "enabled": false }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "capability-denied");

    let off = call(
        &mut engine,
        &mut ui,
        "plugin/setEnabled",
        json!({ "id": "camera-manager", "enabled": false }),
    );
    assert_eq!(off["result"]["status"], "off", "{off}");
    assert_eq!(off["result"]["enabled"], false);
    assert_eq!(calls.take(), ["unload camera-manager"]);
    let usage = call(&mut engine, &mut ui, "plugin/usage", json!({}));
    assert_eq!(usage["result"]["plugins"], json!([]));
    // A late description does not bring it back.
    engine.plugin_described("camera-manager", Description::default());
    assert_eq!(
        engine.state().plugins["camera-manager"].status,
        PluginStatus::Off
    );

    let on = call(
        &mut engine,
        &mut ui,
        "plugin/setEnabled",
        json!({ "id": "camera-manager", "enabled": true }),
    );
    assert_eq!(on["result"]["status"], "starting");
    assert!(on["result"].get("enabled").is_none(), "on is left out");
    assert_eq!(calls.take().len(), 1, "loaded again");
    assert!(engine.state().plugins["camera-manager"].enabled);
}

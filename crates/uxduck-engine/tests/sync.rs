//! Cloud sync (S16): what leaves, what is taken, and that two computers end
//! up alike whatever order the account hands them changes in.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers and scenarios"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::{Capability, DaemonCapabilities};
use uxduck_contracts::dto::plugin::{CommandCall, PluginManifest, Subscriptions};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::domain::link::Link;
use uxduck_core::domain::vehicle::{Observation, Presence};
use uxduck_core::events::{Change, Event};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    Clock, CommandOrder, ControlRoute, Deleted, LinkPort, MemoryCredentials, NoAgent, Persistence,
    PluginLoad, PluginPort, Ports, Random, SerialPortInfo, StorageReport, System,
};
use uxduck_engine::state::State;
use uxduck_engine::sync::{Outgoing, Pulled, SyncChange, SyncPort, outgoing};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

#[derive(Clone, Default)]
struct Time(Arc<AtomicU64>);

impl Clock for Time {
    fn now_us(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1_000
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

/// Keeps every event, as the journal would.
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
    fn after(&self, seq: u64) -> Vec<Event> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }
}

#[derive(Clone, Default)]
struct Wakes(Arc<AtomicU64>);

impl SyncPort for Wakes {
    fn wake(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Orders;

impl LinkPort for Orders {
    fn open(&mut self, _: &Link) {}
    fn close(&mut self, _: &str) {}
    fn command(&mut self, _: CommandOrder) {}
    fn route_controller(&mut self, _: &str, _: Option<ControlRoute>) {}
}

struct Disk;

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
            categories: Vec::new(),
            free_bytes: None,
            total_bytes: None,
        }
    }
    fn delete_older_than(
        &mut self,
        _: uxduck_core::domain::settings::StorageCategory,
        _: u64,
    ) -> Deleted {
        Deleted { files: 0, bytes: 0 }
    }
}

/// A plugin host whose every folder holds the flight-time plugin, which
/// asks to read and to command vehicles.
struct Plugins;

impl PluginPort for Plugins {
    fn inspect(&self, path: &str) -> Result<PluginManifest, String> {
        if !path.ends_with("flight-time") {
            return Err(format!("{path}: no readable plugin.json"));
        }
        Ok(PluginManifest {
            abi: 2,
            id: "flight-time".into(),
            name: "Flight time".into(),
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
        })
    }
    fn load(&mut self, _: PluginLoad) {}
    fn unload(&mut self, _: &str) {}
    fn command(&mut self, _: &str, _: CommandCall) {}
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    queued: VecDeque<Value>,
    next_id: i64,
}

/// One computer: its engine, its journal, an operator window and what the
/// engine asked of the sync task.
struct Computer {
    name: &'static str,
    engine: Engine,
    journal: Journal,
    ui: Client,
    wakes: Wakes,
}

fn computer(name: &'static str, seed: u64, time: &Time, with_sync: bool) -> Computer {
    let journal = Journal::default();
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
        persistence: Box::new(journal.clone()),
        agent: Box::new(NoAgent),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(Orders),
        system: Box::new(Disk),
        plugins: Box::new(Plugins),
    };
    let mut engine = Engine::new(
        config,
        Box::new(time.clone()),
        Box::new(Counter(seed)),
        ports,
        Loaded {
            state: State::default(),
            sessions: vec![],
        },
    );
    let wakes = Wakes::default();
    if with_sync {
        engine.set_sync(Box::new(wakes.clone()));
    }
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut ui = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    let init = call(
        &mut engine,
        &mut ui,
        "initialize",
        json!({
            "clientInfo": { "name": "operator-ui", "version": "0.1.0", "kind": "operator-ui" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    Computer {
        name,
        engine,
        journal,
        ui,
        wakes,
    }
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
    panic!("no reply to {method}");
}

impl Computer {
    fn call(&mut self, method: &str, params: Value) -> Value {
        call(&mut self.engine, &mut self.ui, method, params)
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params);
        reply
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method}: {reply}"))
    }

    /// What the exporter would send: the latest change per entity since the
    /// cursor, and the seq it reads to.
    fn outbox(&self) -> (Vec<SyncChange>, u64) {
        let events = self
            .journal
            .after(self.engine.sync_state().exported_through);
        let through = events.last().map_or(0, |e| e.seq);
        let mut latest: BTreeMap<(String, String), SyncChange> = BTreeMap::new();
        for event in &events {
            match outgoing(event) {
                Outgoing::Send(change) => {
                    latest.insert((change.entity.clone(), change.id.clone()), *change);
                }
                Outgoing::Withdraw { entity, id } => {
                    latest.remove(&(entity, id));
                }
                Outgoing::Ignore => {}
            }
        }
        (latest.into_values().collect(), through)
    }

    /// Sends everything to `account`, as the exporter does.
    fn push(&mut self, account: &mut Account) -> usize {
        let (changes, through) = self.outbox();
        let sent = changes.len();
        for change in changes {
            account.put(self.name, change);
        }
        self.engine.sync_exported(through, sent as u64);
        sent
    }

    /// Takes everything other computers sent.
    fn pull(&mut self, account: &Account) {
        let after = self.engine.sync_state().pulled_through;
        let (changes, cursor) = account.since(after, self.name);
        self.engine.sync_pulled(changes, cursor);
    }
}

/// The account: the latest copy of each entity by the same rule the
/// computers use, and a sequence of when each copy was replaced.
#[derive(Default)]
struct Account {
    next: u64,
    rows: BTreeMap<(String, String), (u64, &'static str, SyncChange)>,
}

impl Account {
    fn put(&mut self, device: &'static str, change: SyncChange) {
        let key = (change.entity.clone(), change.id.clone());
        let current = self
            .rows
            .get(&key)
            .map(|(_, _, c)| (c.version, c.digest.as_str()));
        if uxduck_engine::sync::wins((change.version, &change.digest), current) {
            self.next += 1;
            self.rows.insert(key, (self.next, device, change));
        }
    }

    fn since(&self, after: u64, device: &str) -> (Vec<Pulled>, u64) {
        let mut rows: Vec<_> = self
            .rows
            .values()
            .filter(|(seq, from, _)| *seq > after && *from != device)
            .collect();
        rows.sort_by_key(|(seq, ..)| *seq);
        let cursor = self.next.max(after);
        (
            rows.into_iter()
                .map(|(_, from, change)| Pulled {
                    device: (*from).to_owned(),
                    change: change.clone(),
                })
                .collect(),
            cursor,
        )
    }
}

fn thread_title(computer: &mut Computer, id: &str) -> String {
    let reply = computer.ok("state/subscribe", json!({ "entities": ["thread"] }));
    reply["snapshot"]["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .map(|t| t["title"].as_str().unwrap().to_owned())
        .unwrap_or_default()
}

#[test]
fn chats_and_places_reach_the_other_computer_and_a_tie_ends_alike() {
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    let mut b = computer("hangar", 2, &time, true);
    a.ok("sync/set", json!({ "enabled": true }));
    b.ok("sync/set", json!({ "enabled": true }));

    let thread = a.ok("thread/create", json!({ "title": "Pre-flight notes" }));
    let id = thread["id"].as_str().unwrap().to_owned();
    a.ok(
        "place/save",
        json!({ "name": "North field", "latitudeDeg": -35.36, "longitudeDeg": 149.16 }),
    );
    assert!(a.wakes.0.load(Ordering::SeqCst) > 0, "the task is told");
    assert_eq!(a.push(&mut account), 2, "the chat and the place");
    assert_eq!(a.push(&mut account), 0, "nothing new, nothing sent");

    b.pull(&account);
    assert_eq!(thread_title(&mut b, &id), "Pre-flight notes");
    let status = b.ok("sync/get", json!({}));
    assert_eq!(status["pulled"], 2);
    assert!(
        b.outbox().0.is_empty(),
        "what came from the account is not sent back"
    );

    // Both rename it at the same version, then each takes the other's.
    let version = a.ok("state/subscribe", json!({ "entities": ["thread"] }))["snapshot"]["threads"]
        [0]["version"]
        .clone();
    a.ok(
        "thread/update",
        json!({ "id": id, "expectedVersion": version, "title": "Laptop's title" }),
    );
    b.ok(
        "thread/update",
        json!({ "id": id, "expectedVersion": version, "title": "Hangar's title" }),
    );
    a.push(&mut account);
    b.push(&mut account);
    a.pull(&account);
    b.pull(&account);
    let (on_a, on_b) = (thread_title(&mut a, &id), thread_title(&mut b, &id));
    assert_eq!(on_a, on_b, "the same winner on both");
    assert!(["Laptop's title", "Hangar's title"].contains(&on_a.as_str()));
}

#[test]
fn a_saved_vehicle_goes_to_the_library_and_is_used_once_activated() {
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    let mut b = computer("hangar", 2, &time, true);
    a.ok("sync/set", json!({ "enabled": true }));
    b.ok("sync/set", json!({ "enabled": true }));

    let link = a.ok(
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    for (id, name) in [("v-survey", "Survey quad"), ("v-private", "Test rig")] {
        a.engine.vehicle_observed(Observation {
            id: id.into(),
            aliases: Vec::new(),
            link_id: link["id"].as_str().unwrap().into(),
            peer: "127.0.0.1:5760".into(),
            system_id: 1,
            component_ids: vec![1],
            autopilot: "Ardupilotmega".into(),
            vehicle_type: "Quadrotor".into(),
            uid: Some(format!("uid-{id}")),
            firmware: None,
            firmware_commit: None,
            presence: Presence::Online,
            last_heartbeat_us: 5,
            armed: false,
            mode: None,
        });
        a.ok("vehicle/save", json!({ "id": id, "name": name }));
    }
    a.ok(
        "vehicle/save",
        json!({ "id": "v-private", "localOnly": true }),
    );
    a.ok("vehicle/size", json!({ "id": "v-survey", "sizeCm": 320 }));
    let (outbox, _) = a.outbox();
    let vehicles: Vec<&SyncChange> = outbox.iter().filter(|c| c.entity == "vehicle").collect();
    assert_eq!(vehicles.len(), 1, "the local-only one stays: {vehicles:?}");
    let Change::VehicleUpserted { vehicle } = &vehicles[0].change else {
        panic!("a vehicle");
    };
    assert_eq!(vehicle.name, "Survey quad");
    assert!(
        vehicle.link_id.is_empty(),
        "its profile, not where it was heard"
    );
    a.push(&mut account);

    b.pull(&account);
    let status = b.ok("sync/get", json!({}));
    assert_eq!(status["library"][0]["name"], "Survey quad");
    assert_eq!(status["library"][0]["device"], "laptop");
    assert_eq!(status["library"][0]["active"], false);
    assert!(
        b.ok("vehicle/list", json!({}))["vehicles"]
            .as_array()
            .unwrap()
            .is_empty(),
        "pick: nothing appears unasked"
    );

    let status = b.ok("sync/activate", json!({ "id": "v-survey" }));
    assert_eq!(status["library"][0]["active"], true);
    let here = b.ok("vehicle/get", json!({ "id": "v-survey" }));
    assert_eq!(here["name"], "Survey quad");
    assert_eq!(here["sizeCm"], 320, "its size comes with it");
    assert_eq!(here["saved"], true);
    assert_eq!(here["presence"], "offline");
    assert!(b.outbox().0.is_empty(), "activating sends nothing back");

    // Renamed on the laptop: the hangar's copy follows.
    a.ok(
        "vehicle/save",
        json!({ "id": "v-survey", "name": "Survey quad 2" }),
    );
    a.push(&mut account);
    b.pull(&account);
    assert_eq!(
        b.ok("vehicle/get", json!({ "id": "v-survey" }))["name"],
        "Survey quad 2"
    );

    // Its size changed on the laptop, then given back: the hangar's follows.
    a.ok("vehicle/size", json!({ "id": "v-survey", "sizeCm": 450 }));
    a.push(&mut account);
    b.pull(&account);
    assert_eq!(
        b.ok("vehicle/get", json!({ "id": "v-survey" }))["sizeCm"],
        450
    );
    a.ok("vehicle/size", json!({ "id": "v-survey" }));
    a.push(&mut account);
    b.pull(&account);
    let here = b.ok("vehicle/get", json!({ "id": "v-survey" }));
    assert!(here.get("sizeCm").is_none(), "{here}");
    assert!(b.outbox().0.is_empty(), "taking it sends nothing back");
}

#[test]
fn mirror_all_uses_every_vehicle_in_the_account() {
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    let mut b = computer("hangar", 2, &time, true);
    a.ok("sync/set", json!({ "enabled": true }));
    b.ok("sync/set", json!({ "enabled": true }));
    let link = a.ok(
        "link/add",
        json!({ "kind": { "type": "udp-listen", "host": "0.0.0.0", "port": 14550 } }),
    );
    a.engine.vehicle_observed(Observation {
        id: "v1".into(),
        aliases: Vec::new(),
        link_id: link["id"].as_str().unwrap().into(),
        peer: "127.0.0.1:5760".into(),
        system_id: 1,
        component_ids: vec![1],
        autopilot: "Ardupilotmega".into(),
        vehicle_type: "FixedWing".into(),
        uid: None,
        firmware: None,
        firmware_commit: None,
        presence: Presence::Online,
        last_heartbeat_us: 5,
        armed: false,
        mode: None,
    });
    a.ok("vehicle/save", json!({ "id": "v1", "name": "Mapper" }));
    a.push(&mut account);
    b.pull(&account);
    assert!(
        b.call("vehicle/get", json!({ "id": "v1" }))
            .get("error")
            .is_some()
    );
    b.ok("sync/set", json!({ "activation": "mirror-all" }));
    assert_eq!(b.ok("vehicle/get", json!({ "id": "v1" }))["name"], "Mapper");
}

#[test]
fn off_by_default_and_unavailable_without_a_sync_task() {
    let time = Time::default();
    let mut a = computer("laptop", 1, &time, true);
    let status = a.ok("sync/get", json!({}));
    assert_eq!(status["available"], true);
    assert_eq!(status["enabled"], false);
    a.ok("thread/create", json!({ "title": "Local only" }));
    assert_eq!(a.wakes.0.load(Ordering::SeqCst), 0, "off: nothing to send");

    let mut browser = computer("browser", 3, &time, false);
    let status = browser.ok("sync/get", json!({}));
    assert_eq!(status["available"], false);
    assert!(status["problem"].as_str().unwrap().contains("installed"));
    let refused = browser.call("sync/set", json!({ "enabled": true }));
    assert!(refused.get("error").is_some(), "{refused}");
}

#[test]
fn the_cursor_is_journaled_only_when_something_was_sent() {
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    a.ok("sync/set", json!({ "enabled": true }));
    a.ok("thread/create", json!({ "title": "One" }));
    a.push(&mut account);
    let cursor = a.engine.sync_state().exported_through;
    let seq = a.journal.after(0).last().unwrap().seq;
    assert!(cursor > 0);
    assert_eq!(a.push(&mut account), 0);
    assert_eq!(
        a.journal.after(0).last().unwrap().seq,
        seq,
        "no upload receipt for an empty upload"
    );
    let journaled = a
        .journal
        .after(0)
        .into_iter()
        .filter(|e| matches!(e.change, Change::SyncChanged { .. }))
        .count();
    assert_eq!(journaled, 2, "turning it on, and the one upload");
}

fn plugin(computer: &mut Computer) -> Option<Value> {
    let reply = computer.ok("state/subscribe", json!({ "entities": ["plugin"] }));
    reply["snapshot"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "flight-time")
        .cloned()
}

/// What the sync task does with plugins in a round: upload what changed,
/// then fetch what is missing, with the bundle's digest standing in for its
/// files.
fn publish(computer: &mut Computer, digest: &str) {
    for plugin in computer.engine.plugins_to_publish() {
        if plugin.published.as_deref() != Some(digest) {
            computer.engine.plugin_published(&plugin.id, digest);
        }
    }
}

fn fetch(computer: &mut Computer) -> Vec<String> {
    let mut fetched = Vec::new();
    for plugin in computer.engine.plugins_to_fetch() {
        let path = format!("/synced/{}", plugin.id);
        assert!(
            computer
                .engine
                .plugin_fetched(&plugin.id, &plugin.digest, &path)
        );
        fetched.push(plugin.digest);
    }
    fetched
}

#[test]
fn a_plugin_follows_to_the_other_computer_where_control_waits_for_an_operator() {
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    let mut b = computer("hangar", 2, &time, true);
    a.ok("plugin/install", json!({ "path": "/plugins/flight-time" }));
    assert!(
        a.engine.plugins_to_publish().is_empty(),
        "sync off: nothing goes to the account"
    );
    a.ok("sync/set", json!({ "enabled": true }));
    b.ok("sync/set", json!({ "enabled": true }));

    publish(&mut a, "d1");
    publish(&mut a, "d1");
    assert_eq!(a.push(&mut account), 1, "the plugin, once");
    b.pull(&account);
    assert_eq!(fetch(&mut b), ["d1"]);
    assert!(fetch(&mut b).is_empty(), "installed: not fetched again");

    let here = plugin(&mut b).unwrap();
    assert_eq!(here["capabilities"], json!(["read"]));
    assert_eq!(here["waiting"], json!(["control"]));
    assert_eq!(here["origin"], json!({ "type": "synced", "digest": "d1" }));
    let notes = b.ok("state/subscribe", json!({ "entities": ["notification"] }));
    assert!(
        notes
            .to_string()
            .contains("The Flight time plugin is waiting for you"),
        "{notes}"
    );
    assert!(
        b.engine.plugins_to_publish().is_empty(),
        "a synced plugin is not sent back"
    );
    assert!(b.outbox().0.is_empty());

    let allowed = b.ok("plugin/allow", json!({ "id": "flight-time" }));
    assert_eq!(allowed["capabilities"], json!(["read", "control"]));
    assert!(allowed.get("waiting").is_none());

    // A new version waits again.
    publish(&mut a, "d2");
    a.push(&mut account);
    b.pull(&account);
    assert_eq!(fetch(&mut b), ["d2"]);
    assert_eq!(plugin(&mut b).unwrap()["waiting"], json!(["control"]));

    // Removed where it came from: removed everywhere.
    a.ok("plugin/remove", json!({ "id": "flight-time" }));
    a.push(&mut account);
    b.pull(&account);
    assert!(plugin(&mut b).is_none());
    assert!(fetch(&mut b).is_empty());

    // Installed again: it comes back; removed on the other computer, it
    // stays removed there until a new version.
    a.ok("plugin/install", json!({ "path": "/plugins/flight-time" }));
    publish(&mut a, "d3");
    a.push(&mut account);
    b.pull(&account);
    assert_eq!(fetch(&mut b), ["d3"]);
    b.ok("plugin/remove", json!({ "id": "flight-time" }));
    assert!(fetch(&mut b).is_empty(), "declined at d3");
    b.push(&mut account);
    a.pull(&account);
    assert!(
        plugin(&mut a).is_some(),
        "removing a synced copy stays there"
    );
    publish(&mut a, "d4");
    a.push(&mut account);
    b.pull(&account);
    assert_eq!(fetch(&mut b), ["d4"]);
}

#[test]
fn only_an_operator_allows_a_waiting_plugin() {
    let time = Time::default();
    let mut b = computer("hangar", 2, &time, true);
    let (outbox, inbox) = unbounded();
    let id = b
        .engine
        .connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut agent = Client {
        id,
        inbox,
        queued: VecDeque::new(),
        next_id: 0,
    };
    call(
        &mut b.engine,
        &mut agent,
        "initialize",
        json!({
            "clientInfo": { "name": "helper", "version": "0.1.0", "kind": "agent" },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    let refused = call(
        &mut b.engine,
        &mut agent,
        "plugin/allow",
        json!({ "id": "flight-time" }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "capability-denied",
        "{refused}"
    );
}

/// Definition orders, kept to read (S74).
#[derive(Clone, Default)]
struct Definitions(Arc<std::sync::Mutex<Vec<uxduck_engine::param_definitions::DefinitionsOrder>>>);

impl uxduck_engine::param_definitions::DefinitionsPort for Definitions {
    fn request(&mut self, order: uxduck_engine::param_definitions::DefinitionsOrder) {
        self.0.lock().unwrap().push(order);
    }
}

/// A person's definition set from a URL reaches their other computer,
/// which fetches it itself (S74).
#[test]
fn a_set_from_a_url_reaches_the_other_computer_which_fetches_it() {
    use uxduck_engine::param_definitions::DefinitionsOrder;
    let time = Time::default();
    let mut account = Account::default();
    let mut a = computer("laptop", 1, &time, true);
    let mut b = computer("hangar", 2, &time, true);
    let (on_a, on_b) = (Definitions::default(), Definitions::default());
    a.engine.set_definitions(Box::new(on_a.clone()));
    b.engine.set_definitions(Box::new(on_b.clone()));
    a.ok("sync/set", json!({ "enabled": true }));
    b.ok("sync/set", json!({ "enabled": true }));

    let added = a.ok(
        "param/definitions/add",
        json!({ "url": "https://ci.example.com/copter.pdef.json", "firmware": "ArduCopter" }),
    );
    let id = added["id"].as_str().unwrap().to_owned();
    assert_eq!(a.push(&mut account), 1, "the set with a URL");
    b.pull(&account);
    assert_eq!(
        b.engine.state().own_sets[&id].url.as_deref(),
        Some("https://ci.example.com/copter.pdef.json")
    );
    assert!(
        on_b.0
            .lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o, DefinitionsOrder::FetchOwn { id: asked, .. } if *asked == id)),
        "fetched there too"
    );
}

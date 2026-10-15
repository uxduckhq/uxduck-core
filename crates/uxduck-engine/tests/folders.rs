//! Folders, order, pins, archive, tombstones and bulk actions.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "test helpers and scenarios"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{
    AgentPort, Clock, Forget, MemoryCredentials, Ports, Random, TurnRequest,
};
use uxduck_engine::state::State;
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        1_000
    }
}

/// A 64-bit generator, so ids do not repeat within a test.
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
struct Turns(Arc<Mutex<Vec<String>>>);

impl AgentPort for Turns {
    fn start_turn(&mut self, turn: TurnRequest) {
        self.0.lock().unwrap().push(format!(
            "start {} {} {}",
            turn.turn, turn.thread_id, turn.text
        ));
    }
    fn cancel_turn(&mut self, thread_id: &str) {
        self.0.lock().unwrap().push(format!("cancel {thread_id}"));
    }
}

fn engine(turns: &Turns, provider: &str, state: State) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: vec!["model-a".into(), "model-b".into()],
        agent_provider: provider.into(),
        about: uxduck_engine::HostAbout::default(),
    };
    let ports = Ports {
        persistence: Box::new(Forget),
        agent: Box::new(turns.clone()),
        credentials: Box::new(MemoryCredentials::default()),
        links: Box::new(uxduck_engine::runtime::NoLinks),
        system: Box::new(uxduck_engine::runtime::NoSystem::default()),
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
    loop {
        let frame: Value =
            serde_json::from_str(&client.inbox.try_recv().expect("a reply")).unwrap();
        if frame["id"] == id {
            return frame;
        }
        client.queued.push_back(frame);
    }
}

fn notifications(client: &mut Client, method: &str) -> Vec<Value> {
    let mut frames: Vec<Value> = client.queued.drain(..).collect();
    frames.extend(
        std::iter::from_fn(|| client.inbox.try_recv().ok())
            .map(|t| serde_json::from_str::<Value>(&t).unwrap()),
    );
    frames
        .into_iter()
        .filter(|f| f["method"] == method)
        .map(|f| f["params"].clone())
        .collect()
}

fn call_ok(engine: &mut Engine, client: &mut Client, method: &str, params: Value) -> Value {
    let reply = call(engine, client, method, params);
    assert!(reply.get("result").is_some(), "{method}: {reply}");
    reply["result"].clone()
}

fn new_thread(engine: &mut Engine, ui: &mut Client, title: &str) -> Value {
    call_ok(engine, ui, "thread/create", json!({ "title": title }))
}

/// Live thread titles in a folder, in order.
fn in_folder(engine: &Engine, folder: Option<&str>) -> Vec<String> {
    let mut threads: Vec<_> = engine
        .state()
        .threads
        .values()
        .filter(|t| t.deleted_at_us.is_none() && t.folder_id.as_deref() == folder)
        .collect();
    threads.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));
    threads.iter().map(|t| t.title.clone()).collect()
}

fn setup() -> (Engine, Client) {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let ui = connect(&mut engine, "operator-ui");
    (engine, ui)
}

#[test]
fn folders_nest_three_deep_and_threads_move_and_reorder() {
    let (mut engine, mut ui) = setup();
    let top = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Ops" }),
    );
    let mid = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "2026", "parentId": top["id"] }),
    );
    let deep = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "September", "parentId": mid["id"] }),
    );
    assert_eq!(deep["parentId"], mid["id"]);

    let a = new_thread(&mut engine, &mut ui, "A");
    let b = new_thread(&mut engine, &mut ui, "B");
    let c = new_thread(&mut engine, &mut ui, "C");
    assert_eq!(in_folder(&engine, None), ["A", "B", "C"]);
    let deep_id = deep["id"].as_str().unwrap();
    for t in [&a, &b, &c] {
        let moved = call_ok(
            &mut engine,
            &mut ui,
            "thread/update",
            json!({ "id": t["id"], "expectedVersion": 1, "folderId": deep_id }),
        );
        assert_eq!(moved["folderId"], deep_id);
    }
    assert_eq!(in_folder(&engine, Some(deep_id)), ["A", "B", "C"]);
    // Drag C before A.
    call_ok(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": c["id"], "expectedVersion": 2, "beforeId": a["id"] }),
    );
    assert_eq!(in_folder(&engine, Some(deep_id)), ["C", "A", "B"]);
    // Back to unfiled with null.
    call_ok(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": b["id"], "expectedVersion": 2, "folderId": null }),
    );
    assert_eq!(in_folder(&engine, None), ["B"]);
    let wrong = call(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": a["id"], "expectedVersion": 2, "beforeId": b["id"] }),
    );
    assert_eq!(
        wrong["error"]["data"]["kind"], "invalid-value",
        "B is not in A's folder"
    );
}

#[test]
fn a_folder_cannot_move_inside_itself() {
    let (mut engine, mut ui) = setup();
    let top = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Ops" }),
    );
    let child = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Child", "parentId": top["id"] }),
    );
    let cycle = call(
        &mut engine,
        &mut ui,
        "folder/update",
        json!({ "id": top["id"], "expectedVersion": 1, "parentId": child["id"] }),
    );
    assert_eq!(cycle["error"]["data"]["kind"], "invalid-value");
    let own = call(
        &mut engine,
        &mut ui,
        "folder/update",
        json!({ "id": top["id"], "expectedVersion": 1, "parentId": top["id"] }),
    );
    assert_eq!(own["error"]["data"]["kind"], "invalid-value");
    let renamed = call_ok(
        &mut engine,
        &mut ui,
        "folder/update",
        json!({ "id": child["id"], "expectedVersion": 1, "name": "Renamed", "parentId": null }),
    );
    assert_eq!(
        (renamed["name"].as_str(), renamed.get("parentId")),
        (Some("Renamed"), None)
    );
}

#[test]
fn pin_archive_and_tombstones() {
    let (mut engine, mut ui) = setup();
    let t = new_thread(&mut engine, &mut ui, "Keep");
    let pinned = call_ok(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": t["id"], "expectedVersion": 1, "pinned": true, "archived": true }),
    );
    assert_eq!(
        (pinned["pinned"].as_bool(), pinned["archived"].as_bool()),
        (Some(true), Some(true))
    );

    let gone = new_thread(&mut engine, &mut ui, "Gone");
    let id = gone["id"].as_str().unwrap().to_owned();
    call_ok(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": id, "text": "hello" }),
    );
    let stale = call(
        &mut engine,
        &mut ui,
        "thread/delete",
        json!({ "id": id, "expectedVersion": 1 }),
    );
    assert_eq!(stale["error"]["data"]["kind"], "version-conflict");
    let version = engine.state().threads[&id].version;
    call_ok(
        &mut engine,
        &mut ui,
        "thread/delete",
        json!({ "id": id, "expectedVersion": version }),
    );
    assert!(
        engine.state().threads[&id].deleted_at_us.is_some(),
        "a tombstone, not a removal"
    );
    assert!(!engine.state().items_of(&id).is_empty(), "its items stay");
    assert!(!engine.turn_in_progress(&id), "a turn in progress stops");
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "thread/send",
            json!({ "threadId": id, "text": "again" })
        )["error"]["data"]["kind"],
        "not-found"
    );
}

#[test]
fn deleting_a_folder_moves_its_contents_up() {
    let (mut engine, mut ui) = setup();
    let top = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Top" }),
    );
    let doomed = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Doomed", "parentId": top["id"] }),
    );
    let inner = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Inner", "parentId": doomed["id"] }),
    );
    let t = new_thread(&mut engine, &mut ui, "Filed");
    call_ok(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": t["id"], "expectedVersion": 1, "folderId": doomed["id"] }),
    );
    call_ok(
        &mut engine,
        &mut ui,
        "folder/delete",
        json!({ "id": doomed["id"], "expectedVersion": 1 }),
    );

    let top_id = top["id"].as_str().unwrap();
    assert_eq!(in_folder(&engine, Some(top_id)), ["Filed"]);
    assert_eq!(
        engine.state().folders[inner["id"].as_str().unwrap()]
            .parent_id
            .as_deref(),
        Some(top_id)
    );
    assert!(
        engine.state().folders[doomed["id"].as_str().unwrap()]
            .deleted_at_us
            .is_some()
    );
    let into_deleted = call(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": t["id"], "expectedVersion": 3, "folderId": doomed["id"] }),
    );
    assert_eq!(into_deleted["error"]["data"]["kind"], "not-found");
}

#[test]
fn bulk_actions_apply_to_many_and_report_what_they_skipped() {
    let (mut engine, mut ui) = setup();
    let folder = call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Batch" }),
    );
    let ids: Vec<Value> = (0..10)
        .map(|i| new_thread(&mut engine, &mut ui, &format!("T{i}"))["id"].clone())
        .collect();
    let mut with_unknown = ids.clone();
    with_unknown.push(json!("thnope"));
    let moved = call_ok(
        &mut engine,
        &mut ui,
        "thread/bulk",
        json!({ "threadIds": with_unknown, "action": "move", "folderId": folder["id"] }),
    );
    assert_eq!(moved["changed"].as_array().unwrap().len(), 10);
    assert_eq!(moved["skipped"], json!(["thnope"]));
    assert_eq!(in_folder(&engine, folder["id"].as_str()).len(), 10);

    call_ok(
        &mut engine,
        &mut ui,
        "thread/bulk",
        json!({ "threadIds": &ids[..3], "action": "archive" }),
    );
    call_ok(
        &mut engine,
        &mut ui,
        "thread/bulk",
        json!({ "threadIds": &ids[..1], "action": "delete" }),
    );
    let archived = engine
        .state()
        .threads
        .values()
        .filter(|t| t.archived)
        .count();
    let deleted = engine
        .state()
        .threads
        .values()
        .filter(|t| t.deleted_at_us.is_some())
        .count();
    assert_eq!((archived, deleted), (3, 1));
    let again = call_ok(
        &mut engine,
        &mut ui,
        "thread/bulk",
        json!({ "threadIds": &ids[..1], "action": "unarchive" }),
    );
    assert_eq!(
        again["skipped"],
        json!([ids[0]]),
        "a deleted thread is skipped"
    );
}

#[test]
fn subscribers_get_folders_in_the_snapshot_and_as_deltas() {
    let (mut engine, mut ui) = setup();
    call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "Before" }),
    );
    let snapshot = call_ok(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["folder"] }),
    );
    assert_eq!(snapshot["snapshot"]["folders"][0]["name"], "Before");
    call_ok(
        &mut engine,
        &mut ui,
        "folder/create",
        json!({ "name": "After" }),
    );
    let deltas = notifications(&mut ui, "state/delta");
    assert_eq!(
        deltas.last().unwrap()["changes"][0]["folder"]["name"],
        "After"
    );
}

//! Pushed state and persistence through the engine's public API: subscribe,
//! deltas and their contiguity, versions, checkpoints and restore.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::needless_pass_by_value,
    reason = "test helpers"
)]

use std::sync::{Arc, Mutex};

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::DaemonCapabilities;
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::events::Event;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::pairing::Session;
use uxduck_engine::runtime::{Clock, Persistence, Ports, Random};
use uxduck_engine::state::{State, restore};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded};

struct Tick(u64);

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        self.0
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

/// Records what the engine persists, shared with the test.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Recorded>>);

#[derive(Default)]
struct Recorded {
    events: Vec<Event>,
    checkpoints: Vec<State>,
    sessions: Vec<(String, Session)>,
}

impl Persistence for Recorder {
    fn append(&mut self, event: &Event) {
        self.0.lock().unwrap().events.push(event.clone());
    }
    fn checkpoint(&mut self, state: &State) {
        self.0.lock().unwrap().checkpoints.push(state.clone());
    }
    fn save_session(&mut self, token_hash: &str, session: &Session) {
        self.0
            .lock()
            .unwrap()
            .sessions
            .push((token_hash.to_owned(), session.clone()));
    }
}

fn engine(recorder: &Recorder, loaded: Loaded, checkpoint_every: u64) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every,
        models: vec!["test-model".into()],
        agent_provider: "fake".into(),
        about: uxduck_engine::HostAbout::default(),
    };
    Engine::new(
        config,
        Box::new(Tick(1)),
        Box::new(Counter(0)),
        Ports::with_persistence(Box::new(recorder.clone())),
        loaded,
    )
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
    /// Notifications that arrived while waiting for a reply.
    queued: std::collections::VecDeque<Value>,
    next_id: i64,
}

fn connect(engine: &mut Engine, kind: &str) -> Client {
    let (outbox, inbox) = unbounded();
    let id = engine.connect(Transport::Socket, Trust::LocalUser, outbox);
    let mut client = Client {
        id,
        inbox,
        queued: std::collections::VecDeque::new(),
        next_id: 0,
    };
    let reply = call(
        engine,
        &mut client,
        "initialize",
        json!({
            "clientInfo": { "name": "t", "version": "0.1.0", "kind": kind },
            "capabilities": { "protocolVersion": 1 }
        }),
    );
    assert!(reply.get("result").is_some(), "{reply}");
    client
}

/// Calls a method and returns its reply; notifications stay queued.
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

fn deltas(client: &mut Client) -> Vec<Value> {
    let mut frames: Vec<Value> = client.queued.drain(..).collect();
    frames.extend(
        std::iter::from_fn(|| client.inbox.try_recv().ok())
            .map(|text| serde_json::from_str::<Value>(&text).unwrap()),
    );
    frames
        .into_iter()
        .filter(|frame| frame["method"] == "state/delta")
        .map(|frame| frame["params"].clone())
        .collect()
}

fn create(engine: &mut Engine, client: &mut Client, name: &str) -> Value {
    call(engine, client, "operator/create", json!({ "name": name }))["result"].clone()
}

#[test]
fn subscribe_returns_a_snapshot_and_then_contiguous_deltas() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut writer = connect(&mut engine, "operator-cli");
    create(&mut engine, &mut writer, "Ana");

    let mut reader = connect(&mut engine, "operator-ui");
    let subscribed = call(
        &mut engine,
        &mut reader,
        "state/subscribe",
        json!({ "entities": ["operator"] }),
    );
    assert_eq!(subscribed["result"]["seq"], 1);
    assert_eq!(
        subscribed["result"]["snapshot"]["operators"][0]["name"],
        "Ana"
    );

    let ben = create(&mut engine, &mut writer, "Ben");
    call(
        &mut engine,
        &mut writer,
        "operator/update",
        json!({ "id": ben["id"], "expectedVersion": 1, "name": "Ben K" }),
    );
    call(
        &mut engine,
        &mut writer,
        "operator/remove",
        json!({ "id": ben["id"], "expectedVersion": 2 }),
    );

    let seen = deltas(&mut reader);
    let seqs: Vec<u64> = seen
        .iter()
        .map(|delta| delta["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        seqs,
        [2, 3, 4],
        "the first delta is snapshot seq + 1, then no gaps"
    );
    assert_eq!(seen[0]["changes"][0]["op"], "upsert");
    assert_eq!(seen[1]["changes"][0]["operator"]["name"], "Ben K");
    assert_eq!(
        seen[2]["changes"][0],
        json!({ "entity": "operator", "id": ben["id"], "op": "remove" })
    );
    assert!(seen[0]["actor"]["clientId"].is_string());
    assert!(
        deltas(&mut writer).is_empty(),
        "the writer did not subscribe"
    );
}

#[test]
fn subscribing_while_changes_are_flowing_never_misses_or_repeats_one() {
    // The engine is the single writer, so subscribe is atomic by
    // construction. Interleave many writes with many (re)subscriptions and
    // check every subscriber sees snapshot seq + 1 onward with no holes.
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 50);
    let mut writer = connect(&mut engine, "operator-cli");
    let id = create(&mut engine, &mut writer, "Ana")["id"].clone();
    let mut readers: Vec<(Client, u64)> = Vec::new();
    for round in 0..200u64 {
        call(
            &mut engine,
            &mut writer,
            "operator/update",
            json!({ "id": id, "expectedVersion": round + 1, "name": format!("Ana {round}") }),
        );
        if round % 7 == 0 {
            let mut reader = connect(&mut engine, "operator-ui");
            let snapshot = call(
                &mut engine,
                &mut reader,
                "state/subscribe",
                json!({ "entities": ["operator"] }),
            );
            let seq = snapshot["result"]["seq"].as_u64().unwrap();
            readers.push((reader, seq));
        }
    }
    let last = engine.state().seq;
    assert_eq!(last, 201);
    for (reader, snapshot_seq) in &mut readers {
        let seqs: Vec<u64> = deltas(reader)
            .iter()
            .map(|d| d["seq"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = (*snapshot_seq + 1..=last).collect();
        assert_eq!(seqs, expected);
    }
}

#[test]
fn a_stale_expected_version_is_a_conflict_with_the_current_version() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut a = connect(&mut engine, "operator-ui");
    let mut b = connect(&mut engine, "operator-ui");
    let ana = create(&mut engine, &mut a, "Ana");
    call(
        &mut engine,
        &mut b,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": 1, "name": "Ana B" }),
    );
    let late = call(
        &mut engine,
        &mut a,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": 1, "name": "Ana A" }),
    );
    assert_eq!(late["error"]["code"], -32005);
    assert_eq!(late["error"]["data"]["kind"], "version-conflict");
    assert_eq!(late["error"]["data"]["currentVersion"], 2);
    assert_eq!(
        engine.state().operators.values().next().unwrap().name,
        "Ana B"
    );
}

#[test]
fn names_are_validated_and_capabilities_checked() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut engine, "operator-ui");
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "operator/create",
            json!({ "name": "  " })
        )["error"]["data"]["kind"],
        "invalid-value"
    );
    let mut agent = connect(&mut engine, "agent");
    assert_eq!(
        call(&mut engine, &mut agent, "operator/list", json!({}))["result"]["operators"],
        json!([])
    );
    // Config is beyond an agent: the call waits for an operator, who denies it.
    agent.next_id += 1;
    engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "id": agent.next_id, "method": "operator/create",
                 "params": { "name": "Bot" } })
        .to_string(),
    );
    assert!(agent.inbox.try_recv().is_err(), "held, not answered");
    let pending =
        call(&mut engine, &mut ui, "approval/list", json!({}))["result"]["approvals"].clone();
    assert_eq!(pending[0]["capability"], "config");
    let resolved = call(
        &mut engine,
        &mut ui,
        "approval/resolve",
        json!({ "id": pending[0]["id"], "allow": false }),
    );
    assert!(resolved.get("result").is_some(), "{resolved}");
    let denied: Value = std::iter::from_fn(|| agent.inbox.try_recv().ok())
        .map(|text| serde_json::from_str::<Value>(&text).unwrap())
        .find(|frame| frame["id"] == agent.next_id)
        .expect("the held call is answered");
    assert_eq!(denied["error"]["data"]["kind"], "capability-denied");
    let missing = call(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": "nope", "expectedVersion": 1 }),
    );
    assert_eq!(missing["error"]["data"]["kind"], "not-found");
}

#[test]
fn shortcuts_are_kept_per_operator_and_kept_small() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = create(&mut engine, &mut ui, "Ana");
    assert_eq!(ana["shortcuts"], json!([]));
    let changed = call(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": ana["version"],
                "shortcuts": [{ "action": "open-map", "shortcut": "Alt+G" },
                              { "action": "send-feedback", "shortcut": null }] }),
    );
    assert_eq!(
        changed["result"]["shortcuts"],
        json!([{ "action": "open-map", "shortcut": "Alt+G" },
               { "action": "send-feedback", "shortcut": null }])
    );
    let listed = call(&mut engine, &mut ui, "operator/list", json!({}));
    assert_eq!(
        listed["result"]["operators"][0]["shortcuts"][0]["shortcut"],
        "Alt+G"
    );
    let long = "x".repeat(65);
    let refused = call(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": changed["result"]["version"],
                "shortcuts": [{ "action": "open-map", "shortcut": long }] }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
}

#[test]
fn journal_entries_name_the_operator_the_client_acts_as() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = create(&mut engine, &mut ui, "Ana");
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "client/setOperator",
            json!({ "operatorId": "nope" })
        )["error"]["data"]["kind"],
        "not-found"
    );
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ana["id"] }),
    );
    create(&mut engine, &mut ui, "Ben");
    let events = &recorder.0.lock().unwrap().events;
    assert_eq!(events[0].actor.operator_id, None);
    assert_eq!(events[1].actor.operator_id.as_deref(), ana["id"].as_str());
}

#[test]
fn every_event_is_persisted_and_checkpoints_follow_the_policy() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 3);
    let mut cli = connect(&mut engine, "operator-cli");
    for name in ["A", "B", "C", "D", "E", "F", "G"] {
        create(&mut engine, &mut cli, name);
    }
    let persisted = recorder.0.lock().unwrap();
    assert_eq!(
        persisted.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        persisted
            .checkpoints
            .iter()
            .map(|c| c.seq)
            .collect::<Vec<_>>(),
        [3, 6]
    );
}

#[test]
fn a_restored_engine_continues_the_sequence_with_the_same_state() {
    let recorder = Recorder::default();
    let mut first = engine(&recorder, Loaded::default(), 2);
    let mut cli = connect(&mut first, "operator-cli");
    for name in ["A", "B", "C"] {
        create(&mut first, &mut cli, name);
    }
    let before = first.state().clone();
    drop(first);

    let (checkpoint, events) = {
        let persisted = recorder.0.lock().unwrap();
        (
            persisted.checkpoints.last().cloned(),
            persisted.events.clone(),
        )
    };
    assert_eq!(checkpoint.as_ref().map(|c| c.seq), Some(2));
    let restored = restore(checkpoint, events);
    assert_eq!(restored.replayed, 1);
    assert_eq!(restored.state, before);

    let mut second = engine(
        &recorder,
        Loaded {
            state: restored.state,
            sessions: vec![],
        },
        2,
    );
    let mut cli = connect(&mut second, "operator-cli");
    let listed = call(&mut second, &mut cli, "operator/list", json!({}));
    assert_eq!(listed["result"]["operators"].as_array().unwrap().len(), 3);
    create(&mut second, &mut cli, "D");
    assert_eq!(recorder.0.lock().unwrap().events.last().unwrap().seq, 4);
}

#[test]
fn sessions_are_persisted_by_hash_and_restored() {
    let recorder = Recorder::default();
    let mut first = engine(&recorder, Loaded::default(), 1000);
    let token = first.create_bootstrap();
    let session = first.exchange_bootstrap(&token).unwrap();
    let saved = recorder.0.lock().unwrap().sessions.clone();
    assert_eq!(saved.len(), 1);
    assert_ne!(saved[0].0, session, "the token itself is never stored");
    assert_eq!(saved[0].0, uxduck_engine::pairing::token_hash(&session));

    let second = engine(
        &recorder,
        Loaded {
            state: State::default(),
            sessions: saved,
        },
        1000,
    );
    assert_eq!(
        second.session(&session).map(|s| s.origin),
        Some("http://127.0.0.1:47800/".into())
    );
    assert!(second.session("forged").is_none());
}

#[test]
fn an_operator_starts_on_the_flight_layout_and_keeps_the_layout_it_is_given() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = create(&mut engine, &mut ui, "Ana");
    let group = |tabs: serde_json::Value| {
        let tabs: Vec<_> = tabs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| json!({ "id": t, "kind": t }))
            .collect();
        json!({ "type": "group", "tabs": tabs, "active": 0 })
    };
    // The flight layout with or without a vehicle, Home in the UI drawer
    // (maintainer, 2026-10-10).
    assert_eq!(ana["layout"]["dock"]["type"], "split");
    assert_eq!(ana["layout"]["flightOpened"], true);
    assert_eq!(ana["layout"]["arranged"], false);
    let fly = json!({
        "paneOpen": true,
        "dock": { "type": "split", "direction": "row", "children": [
            { "size": 300, "node": group(json!(["chat"])) },
            { "size": 700, "node": { "type": "split", "direction": "column", "children": [
                { "size": 600, "node": { "type": "group", "active": 0, "tabs": [
                    { "id": "map", "kind": "map" },
                    { "id": "t2", "kind": "map", "vehicleId": "v2" }
                ] } },
                { "size": 400, "node": group(json!(["telemetry"])) }
            ] } }
        ] },
        "saved": [{ "id": "ly1", "name": "Survey", "dock": group(json!(["chat"])) }],
        // Moved from the flight layout: the person's own now.
        "arranged": true,
        "flightOpened": true
    });
    let updated = call(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": 1, "layout": fly }),
    );
    assert_eq!(updated["result"]["layout"], fly);
    // A stored layout always renders: repeated tabs and empty groups go. The
    // chat may be left out: the person closed it.
    let odd = call(
        &mut engine,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": 2, "layout": {
            "paneOpen": true,
            "dock": { "type": "split", "direction": "row", "children": [
                { "size": 1, "node": group(json!([])) },
                { "size": 1, "node": { "type": "group", "tabs": [
                    { "id": "map", "kind": "map" },
                    { "id": "telemetry", "kind": "telemetry" },
                    { "id": "map", "kind": "mission" }
                ], "active": 7 } }
            ] },
            "saved": []
        } }),
    );
    assert_eq!(
        odd["result"]["layout"],
        json!({
            "paneOpen": true,
            "dock": group(json!(["map", "telemetry"])),
            "saved": [],
            "arranged": true,
            "flightOpened": true
        })
    );
    assert_eq!(updated["result"]["name"], "Ana", "other fields untouched");
}

/// The agent opens a tab when asked (S43): in its chat's operator's layout,
/// as "+" would; with no operator, the chat's window opens it itself.
#[test]
fn the_agent_opens_a_tab_in_its_chats_operators_layout() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut engine, "operator-ui");
    let ana = create(&mut engine, &mut ui, "Ana");
    let mut agent = connect(&mut engine, "agent");
    let layout_of = |engine: &mut Engine, ui: &mut Client, id: &Value| {
        call(engine, ui, "operator/list", json!({}))["result"]["operators"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == *id)
            .unwrap()["layout"]
            .clone()
    };
    let vehicles_tabs = |layout: &Value| layout["dock"].to_string().matches("\"vehicles\"").count();

    // A chat with no operator: its window opens it; no operator's layout changes.
    let page_thread = call(&mut engine, &mut ui, "thread/create", json!({}))["result"].clone();
    let opened = call(
        &mut engine,
        &mut agent,
        "layout/open",
        json!({ "kind": "vehicles", "threadId": page_thread["id"] }),
    );
    assert_eq!(
        opened["result"],
        json!({ "operatorId": null, "kind": "vehicles" })
    );
    assert_eq!(
        vehicles_tabs(&layout_of(&mut engine, &mut ui, &ana["id"])),
        0
    );

    // Ana's chat: her layout, open, the tab there once however often asked.
    call(
        &mut engine,
        &mut ui,
        "client/setOperator",
        json!({ "operatorId": ana["id"] }),
    );
    let thread = call(&mut engine, &mut ui, "thread/create", json!({}))["result"].clone();
    for _ in 0..2 {
        let opened = call(
            &mut engine,
            &mut agent,
            "layout/open",
            json!({ "kind": "vehicles", "threadId": thread["id"] }),
        );
        assert_eq!(opened["result"]["operatorId"], ana["id"], "{opened}");
    }
    let layout = layout_of(&mut engine, &mut ui, &ana["id"]);
    assert_eq!(layout["paneOpen"], true);
    assert_eq!(vehicles_tabs(&layout), 2, "one tab: its id and its kind");
    assert_eq!(layout["arranged"], true, "a tab added is arranging");

    // The chat is always open.
    let chat = call(
        &mut engine,
        &mut agent,
        "layout/open",
        json!({ "kind": "chat", "operatorId": ana["id"] }),
    );
    assert_eq!(chat["error"]["data"]["kind"], "invalid-params");
}

/// The layout the person is on (S66): kept on the operator with the rest,
/// through a restart, and gone when its saved layout is deleted.
#[test]
fn the_layout_you_are_on_survives_a_restart_and_goes_with_its_layout() {
    let recorder = Recorder::default();
    let mut first = engine(&recorder, Loaded::default(), 1000);
    let mut ui = connect(&mut first, "operator-ui");
    let ana = create(&mut first, &mut ui, "Ana");
    let survey = json!({ "type": "split", "direction": "row", "children": [
        { "size": 400, "node": { "type": "group", "tabs": [{ "id": "chat", "kind": "chat" }], "active": 0 } },
        { "size": 600, "node": { "type": "group", "tabs": [{ "id": "map", "kind": "map" }], "active": 0 } },
    ] });
    let mut resized = survey.clone();
    resized["children"][0]["size"] = json!(300);
    resized["children"][1]["size"] = json!(700);
    let layout = json!({
        "paneOpen": true,
        "dock": resized,
        "saved": [{ "id": "ly1", "name": "Survey", "dock": survey }],
        "current": "ly1",
    });
    let updated = call(
        &mut first,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": 1, "layout": layout }),
    );
    assert_eq!(updated["result"]["layout"]["current"], "ly1", "{updated}");
    assert_eq!(
        updated["result"]["layout"]["dock"]["children"][0]["size"], 300,
        "the change kept, the saved one untouched"
    );
    drop(first);

    let (checkpoint, events) = {
        let persisted = recorder.0.lock().unwrap();
        (
            persisted.checkpoints.last().cloned(),
            persisted.events.clone(),
        )
    };
    let restored = restore(checkpoint, events);
    let mut second = engine(
        &recorder,
        Loaded {
            state: restored.state,
            sessions: vec![],
        },
        1000,
    );
    let mut ui = connect(&mut second, "operator-ui");
    let listed = call(&mut second, &mut ui, "operator/list", json!({}));
    let after = &listed["result"]["operators"][0];
    assert_eq!(after["layout"]["current"], "ly1");
    assert_eq!(
        after["layout"]["saved"][0]["dock"]["children"][0]["size"],
        400
    );

    // Deleted: no layout is current.
    let mut without = after["layout"].clone();
    without["saved"] = json!([]);
    let updated = call(
        &mut second,
        &mut ui,
        "operator/update",
        json!({ "id": ana["id"], "expectedVersion": after["version"], "layout": without }),
    );
    assert!(
        updated["result"]["layout"].get("current").is_none(),
        "{updated}"
    );
}

/// Records what would be sent to the team.
#[derive(Clone, Default)]
struct Outbox(Arc<Mutex<Vec<uxduck_engine::feedback::FeedbackOrder>>>);

impl uxduck_engine::feedback::FeedbackPort for Outbox {
    fn send(&mut self, order: uxduck_engine::feedback::FeedbackOrder) {
        self.0.lock().unwrap().push(order);
    }
}

/// The agent writes a suggestion card (S73) and cannot send it: only an
/// operator's `feedback/send` sends, as a suggestion, credited only when
/// asked.
#[test]
fn the_agent_writes_a_suggestion_card_and_only_the_person_sends_it() {
    let recorder = Recorder::default();
    let mut engine = engine(&recorder, Loaded::default(), 1000);
    let outbox = Outbox::default();
    engine.set_feedback(Box::new(outbox.clone()));
    let mut agent = connect(&mut engine, "agent");
    let card = call(
        &mut engine,
        &mut agent,
        "feedback/suggest",
        json!({ "title": " Airspeed for rovers ", "text": "Airspeed in a rover's Telemetry." }),
    );
    assert_eq!(
        card["result"],
        json!({ "title": "Airspeed for rovers", "text": "Airspeed in a rover's Telemetry.", "sent": false })
    );
    let empty = call(
        &mut engine,
        &mut agent,
        "feedback/suggest",
        json!({ "title": "", "text": "x" }),
    );
    assert_eq!(empty["error"]["data"]["kind"], "invalid-value");
    assert!(outbox.0.lock().unwrap().is_empty(), "a card sends nothing");

    // The agent cannot send it, even trying.
    let report = json!({
        "reportId": "0192f3a4-5b6c-4d7e-8f90-a1b2c3d4e5f6",
        "kind": "suggestion",
        "text": "Airspeed for rovers\n\nAirspeed in a rover's Telemetry.",
        "includeDetails": false,
        "surface": "browser",
        "ui": { "route": "/", "theme": "light", "density": "comfortable", "mapEngine": "maplibre", "appVersion": "0.1.0", "locale": "en" },
        "fromAgent": true,
    });
    agent.next_id += 1;
    engine.receive(
        agent.id,
        &json!({ "jsonrpc": "2.0", "id": agent.next_id, "method": "feedback/send", "params": report })
            .to_string(),
    );
    assert!(outbox.0.lock().unwrap().is_empty(), "never from the agent");

    // The person sends it, as a suggestion, credited when they tick it.
    let mut ui = connect(&mut engine, "operator-ui");
    let mut credited = report.clone();
    credited["credit"] = json!(true);
    ui.next_id += 1;
    engine.receive(
        ui.id,
        &json!({ "jsonrpc": "2.0", "id": ui.next_id, "method": "feedback/send", "params": credited })
            .to_string(),
    );
    let sent = outbox.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].kind,
        uxduck_contracts::dto::feedback::FeedbackKind::Suggestion
    );
    assert!(sent[0].credit && sent[0].from_agent);
    assert!(
        sent[0].bundle.is_none() && sent[0].screenshot.is_none(),
        "nothing attached"
    );
}

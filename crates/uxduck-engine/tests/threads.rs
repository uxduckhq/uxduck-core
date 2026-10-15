//! Threads, items and the agent host's reports, through the engine's API.

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
use uxduck_core::domain::thread::ItemStatus;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::agent::ToolOutcome;
use uxduck_engine::runtime::{
    AgentPort, Clock, Forget, MemoryCredentials, Ports, Random, TurnRequest,
};
use uxduck_engine::state::State;
use uxduck_engine::{ANTHROPIC_KEY, ConnectionId, Engine, EngineConfig, Loaded};

struct Tick;

impl Clock for Tick {
    fn now_us(&self) -> u64 {
        1_000
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
    engine_offering(turns, provider, state, &["model-a", "model-b"])
}

fn engine_offering(turns: &Turns, provider: &str, state: State, models: &[&str]) -> Engine {
    let config = EngineConfig {
        daemon_version: "0.1.0".into(),
        capabilities: DaemonCapabilities::default(),
        ui_url: "http://127.0.0.1:47800/".into(),
        checkpoint_every: 1000,
        models: models.iter().map(|&m| m.to_owned()).collect(),
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

fn items(engine: &Engine, thread_id: &str) -> Vec<(String, String, ItemStatus)> {
    engine
        .state()
        .items_of(thread_id)
        .iter()
        .map(|item| (format!("{:?}", item.kind), item.text.clone(), item.status))
        .collect()
}

#[test]
fn a_message_starts_a_turn_and_the_agent_answers_through_the_engine() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let mut agent = connect(&mut engine, "agent");
    call(
        &mut engine,
        &mut ui,
        "state/subscribe",
        json!({ "entities": ["thread", "item"] }),
    );

    let thread = call(&mut engine, &mut ui, "thread/create", json!({}))["result"].clone();
    assert_eq!(thread["model"], "model-a");
    let thread_id = thread["id"].as_str().unwrap().to_owned();
    let sent = call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "  what vehicles do I have  " }),
    );
    assert_eq!(sent["result"]["kind"], "userMessage");
    assert_eq!(sent["result"]["text"], "what vehicles do I have");
    assert_eq!(
        turns.0.lock().unwrap().as_slice(),
        [format!("start 1 {thread_id} what vehicles do I have")]
    );
    assert_eq!(
        engine.state().threads[&thread_id].title,
        "what vehicles do I have",
        "named by its first message"
    );
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "thread/send",
            json!({ "threadId": thread_id, "text": "again" })
        )["error"]["data"]["kind"],
        "busy"
    );

    // The host's tool call goes through the agent's own connection.
    let tool = engine.agent_tool_start(agent.id, &thread_id, "vehicle/list", "{}".into());
    let listed = call(&mut engine, &mut agent, "vehicle/list", json!({}));
    engine.agent_tool_finish(
        agent.id,
        &tool,
        ToolOutcome::Result(listed["result"].to_string()),
    );

    let message = engine.agent_message_start(Some(agent.id), &thread_id);
    engine.agent_text_delta(&message, "You have ");
    // A subscriber joining now sees what has streamed so far.
    let mut late = connect(&mut engine, "operator-ui");
    let snapshot = call(
        &mut engine,
        &mut late,
        "state/subscribe",
        json!({ "entities": ["item"] }),
    );
    let streaming = snapshot["result"]["snapshot"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == message.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (streaming["status"].as_str(), streaming["text"].as_str()),
        (Some("inProgress"), Some("You have "))
    );
    engine.agent_text_delta(&message, "no vehicles.");
    engine.agent_message_finish(Some(agent.id), &message, ItemStatus::Completed, None);
    engine.agent_turn_finish(&thread_id, 1, Some("resp_1".into()));

    assert_eq!(
        items(&engine, &thread_id),
        [
            (
                "UserMessage".into(),
                "what vehicles do I have".into(),
                ItemStatus::Completed
            ),
            ("ToolCall".into(), String::new(), ItemStatus::Completed),
            (
                "AgentMessage".into(),
                "You have no vehicles.".into(),
                ItemStatus::Completed
            ),
        ]
    );
    let tool_item = engine.state().items_of(&thread_id)[1].clone();
    let call_receipt = tool_item.tool_call.unwrap();
    assert_eq!(call_receipt.method, "vehicle/list");
    assert_eq!(call_receipt.result.as_deref(), Some(r#"{"vehicles":[]}"#));
    assert!(
        call_receipt.client_id.starts_with('c'),
        "attributed to the agent's connection"
    );
    assert_eq!(
        engine.state().threads[&thread_id]
            .last_response_id
            .as_deref(),
        Some("resp_1")
    );
    assert!(!engine.turn_in_progress(&thread_id));

    let deltas: Vec<String> = notifications(&mut ui, "agent/itemDelta")
        .iter()
        .map(|d| d["delta"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(deltas, ["You have ", "no vehicles."]);
    assert_eq!(
        notifications(&mut late, "agent/itemDelta").len(),
        1,
        "only what streamed after its snapshot"
    );
}

#[test]
fn a_chip_reaches_the_agents_turn_and_stays_on_the_message() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let thread = call(&mut engine, &mut ui, "thread/create", json!({}))["result"].clone();
    let thread_id = thread["id"].as_str().unwrap().to_owned();
    let battery = json!({
        "title": "Battery voltage",
        "key": "battery.voltage_v",
        "value": "15.8",
        "unit": "V",
        "help": "The pack's voltage."
    });
    let sent = call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "is this enough for another ten minutes?",
                "attachments": [battery] }),
    );
    assert_eq!(
        sent["result"]["text"],
        "is this enough for another ten minutes?"
    );
    assert_eq!(sent["result"]["attachments"], json!([battery]));
    assert_eq!(
        turns.0.lock().unwrap().as_slice(),
        [format!(
            "start 1 {thread_id} is this enough for another ten minutes?\n\n\
             Attached by the operator:\n\
             - Battery voltage (`battery.voltage_v`): 15.8 V. UXDuck's help: The pack's voltage."
        )]
    );
    let item = &engine.state().items_of(&thread_id)[0];
    assert_eq!(item.attachments.len(), 1);
    assert_eq!(item.attachments[0].title, "Battery voltage");

    // Too many, or one without a title, is refused before the turn.
    let other = call(&mut engine, &mut ui, "thread/create", json!({}))["result"]["id"].clone();
    let refused = call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": other, "text": "hi", "attachments": [{ "title": " " }] }),
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
    let many: Vec<Value> = (0..9).map(|_| battery.clone()).collect();
    let refused = call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": other, "text": "hi", "attachments": many }),
    );
    assert_eq!(refused["error"]["data"]["kind"], "invalid-value");
    assert_eq!(turns.0.lock().unwrap().len(), 1);
}

#[test]
fn cancel_ends_the_turn_at_once_and_a_late_report_changes_nothing() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let thread_id = call(&mut engine, &mut ui, "thread/create", json!({}))["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "hello" }),
    );
    let message = engine.agent_message_start(None, &thread_id);
    engine.agent_text_delta(&message, "Hel");

    call(
        &mut engine,
        &mut ui,
        "thread/cancel",
        json!({ "threadId": thread_id }),
    );
    assert!(
        turns
            .0
            .lock()
            .unwrap()
            .contains(&format!("cancel {thread_id}"))
    );
    assert_eq!(
        items(&engine, &thread_id)[1],
        ("AgentMessage".into(), "Hel".into(), ItemStatus::Cancelled)
    );

    // A new turn starts; the first one's late finish must not end it.
    call(
        &mut engine,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "again" }),
    );
    engine.agent_turn_finish(&thread_id, 1, None);
    assert!(engine.turn_in_progress(&thread_id));
    engine.agent_turn_finish(&thread_id, 2, None);
    assert!(!engine.turn_in_progress(&thread_id));
}

#[test]
fn items_in_progress_at_a_restart_are_interrupted() {
    let turns = Turns::default();
    let mut first = engine(&turns, "fake", State::default());
    let mut ui = connect(&mut first, "operator-ui");
    let thread_id = call(&mut first, &mut ui, "thread/create", json!({}))["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    call(
        &mut first,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "hello" }),
    );
    first.agent_message_start(None, &thread_id);
    let state = first.state().clone();

    let mut second = engine(&turns, "fake", state);
    assert_eq!(items(&second, &thread_id)[1].2, ItemStatus::Interrupted);
    assert!(!second.turn_in_progress(&thread_id));
    let mut ui = connect(&mut second, "operator-ui");
    let sent = call(
        &mut second,
        &mut ui,
        "thread/send",
        json!({ "threadId": thread_id, "text": "still there?" }),
    );
    assert_eq!(sent["result"]["index"], 2, "the thread continues");
}

#[test]
fn validation_capabilities_and_models() {
    let turns = Turns::default();
    let mut engine = engine(&turns, "openai", State::default());
    let mut ui = connect(&mut engine, "operator-ui");
    let config = call(&mut engine, &mut ui, "agent/config", json!({}))["result"].clone();
    assert_eq!(
        config,
        json!({ "models": ["model-a", "model-b"], "defaultModel": "model-a", "provider": "openai", "ready": false, "readyModels": [] })
    );
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "thread/create",
            json!({ "model": "gpt-0" })
        )["error"]["data"]["kind"],
        "invalid-value"
    );
    let thread = call(
        &mut engine,
        &mut ui,
        "thread/create",
        json!({ "model": "model-b" }),
    )["result"]
        .clone();
    assert_eq!(
        call(
            &mut engine,
            &mut ui,
            "thread/send",
            json!({ "threadId": thread["id"], "text": " " })
        )["error"]["data"]["kind"],
        "invalid-value"
    );
    let updated = call(
        &mut engine,
        &mut ui,
        "thread/update",
        json!({ "id": thread["id"], "expectedVersion": 1, "model": "model-a" }),
    );
    assert_eq!(updated["result"]["model"], "model-a");

    let mut agent = connect(&mut engine, "agent");
    assert_eq!(
        call(&mut engine, &mut agent, "thread/create", json!({}))["error"]["data"]["kind"],
        "capability-denied"
    );
    assert_eq!(
        call(&mut engine, &mut agent, "vehicle/list", json!({}))["result"],
        json!({ "vehicles": [] })
    );

    let mut cli = connect(&mut engine, "operator-cli");
    call(
        &mut engine,
        &mut cli,
        "credentials/set",
        json!({ "name": "openai.key", "value": "sk-test" }),
    );
    assert_eq!(
        call(&mut engine, &mut ui, "agent/config", json!({}))["result"]["ready"],
        true
    );
}

#[test]
fn auto_provider_needs_each_models_own_key_or_an_account() {
    let turns = Turns::default();
    let mut engine = engine_offering(
        &turns,
        "auto",
        State::default(),
        &["gpt-6-sol", "claude-opus-5-5"],
    );
    let mut ui = connect(&mut engine, "operator-ui");
    let mut cli = connect(&mut engine, "operator-cli");
    call(
        &mut engine,
        &mut cli,
        "credentials/set",
        json!({ "name": ANTHROPIC_KEY, "value": "sk-ant-test" }),
    );

    let config = call(&mut engine, &mut ui, "agent/config", json!({}))["result"].clone();
    assert_eq!(config["provider"], "openai");
    assert_eq!(config["readyModels"], json!(["claude-opus-5-5"]));
    assert_eq!(
        config["ready"], false,
        "GPT needs the OpenAI key, or the hosted model's account"
    );

    call(
        &mut engine,
        &mut cli,
        "credentials/set",
        json!({ "name": "account.token", "value": "token" }),
    );
    let config = call(&mut engine, &mut ui, "agent/config", json!({}))["result"].clone();
    assert_eq!(
        config["readyModels"],
        json!(["gpt-6-sol", "claude-opus-5-5"])
    );
    assert_eq!(config["ready"], true);
}

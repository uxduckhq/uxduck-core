//! The router and handlers, driven through the engine's public API with a
//! fake clock and deterministic randomness. No runtime needed.

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
use uxduck_core::policy::admission::Trust;
use uxduck_engine::runtime::{Clock, Forget, Ports, Random};
use uxduck_engine::{ConnectionId, Engine, EngineConfig, Loaded, PairingState};

#[derive(Clone, Default)]
struct FakeClock(Arc<AtomicU64>);

impl Clock for FakeClock {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl FakeClock {
    fn advance_us(&self, us: u64) {
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

struct Harness {
    engine: Engine,
    clock: FakeClock,
}

struct Client {
    id: ConnectionId,
    inbox: UnboundedReceiver<String>,
}

impl Harness {
    fn new() -> Self {
        let clock = FakeClock::default();
        clock.advance_us(1_700_000_000_000_000);
        let config = EngineConfig {
            daemon_version: "0.1.0".into(),
            capabilities: DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:47800/".into(),
            checkpoint_every: 1000,
            models: vec!["test-model".into()],
            agent_provider: "fake".into(),
            about: uxduck_engine::HostAbout::default(),
        };
        Self {
            engine: Engine::new(
                config,
                Box::new(clock.clone()),
                Box::new(Counter(0)),
                Ports::with_persistence(Box::new(Forget)),
                Loaded::default(),
            ),
            clock,
        }
    }

    fn connect(&mut self, transport: Transport, trust: Trust) -> Client {
        let (outbox, inbox) = unbounded();
        Client {
            id: self.engine.connect(transport, trust, outbox),
            inbox,
        }
    }

    fn socket(&mut self) -> Client {
        self.connect(Transport::Socket, Trust::LocalUser)
    }

    fn call(&mut self, client: &mut Client, method: &str, params: Value) -> Value {
        self.engine.receive(
            client.id,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string(),
        );
        next(client)
    }

    fn initialize(&mut self, client: &mut Client, kind: &str) -> Value {
        self.call(
            client,
            "initialize",
            json!({ "clientInfo": { "name": "test", "version": "0.1.0", "kind": kind }, "capabilities": { "protocolVersion": 1 } }),
        )
    }
}

fn next(client: &mut Client) -> Value {
    let text = client.inbox.try_recv().expect("a frame");
    serde_json::from_str(&text).expect("JSON")
}

fn error_kind(reply: &Value) -> &str {
    reply["error"]["data"]["kind"]
        .as_str()
        .unwrap_or_else(|| panic!("not an error: {reply}"))
}

#[test]
fn initialize_admits_an_operator_on_the_socket() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    let reply = h.initialize(&mut cli, "operator-cli");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["daemonVersion"], "0.1.0");
    assert_eq!(reply["result"]["protocolVersion"], 1);
    assert_eq!(
        reply["result"]["granted"],
        json!(["read", "control", "config", "approve"])
    );
    assert!(
        reply["result"]["clientId"]
            .as_str()
            .is_some_and(|id| id.starts_with('c'))
    );
}

#[test]
fn an_agent_starts_with_read() {
    let mut h = Harness::new();
    let mut agent = h.socket();
    assert_eq!(
        h.initialize(&mut agent, "agent")["result"]["granted"],
        json!(["read"])
    );
}

#[test]
fn calls_before_initialize_are_refused() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    let reply = h.call(&mut cli, "pairing/list", json!({}));
    assert_eq!(reply["error"]["code"], -32000);
    assert_eq!(error_kind(&reply), "not-initialized");
}

#[test]
fn initialize_twice_is_refused() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    h.initialize(&mut cli, "operator-cli");
    assert_eq!(
        error_kind(&h.initialize(&mut cli, "operator-cli")),
        "already-initialized"
    );
}

#[test]
fn an_old_or_foreign_client_must_update() {
    let mut h = Harness::new();
    let mut old = h.socket();
    let reply = h.call(
        &mut old,
        "initialize",
        json!({ "clientInfo": { "name": "t", "version": "0.0.9", "kind": "operator-ui" }, "capabilities": { "protocolVersion": 1 } }),
    );
    assert_eq!(error_kind(&reply), "update-required");
    assert_eq!(reply["error"]["data"]["minClientVersion"], "0.1.0");

    let mut other = h.socket();
    let reply = h.call(
        &mut other,
        "initialize",
        json!({ "clientInfo": { "name": "t", "version": "9.0.0", "kind": "operator-ui" }, "capabilities": { "protocolVersion": 2 } }),
    );
    assert_eq!(error_kind(&reply), "update-required");
}

#[test]
fn protocol_errors_have_stable_codes() {
    let mut h = Harness::new();
    let mut c = h.socket();
    h.engine.receive(c.id, "not json");
    assert_eq!(next(&mut c)["error"]["code"], -32700);
    h.engine.receive(c.id, "[]");
    assert_eq!(next(&mut c)["error"]["code"], -32600);
    h.engine
        .receive(c.id, r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#);
    assert_eq!(next(&mut c)["error"]["code"], -32600);
    let reply = h.call(&mut c, "vehicle/fly", json!({}));
    assert_eq!(reply["error"]["code"], -32601);
    let reply = h.call(&mut c, "initialize", json!({ "clientInfo": 3 }));
    assert_eq!(reply["error"]["code"], -32602);
}

#[test]
fn notifications_are_never_answered() {
    let mut h = Harness::new();
    let mut c = h.socket();
    h.engine.receive(
        c.id,
        r#"{"jsonrpc":"2.0","method":"cancel","params":{"id":1}}"#,
    );
    h.engine
        .receive(c.id, r#"{"jsonrpc":"2.0","method":"nonsense"}"#);
    assert!(c.inbox.try_recv().is_err());
}

#[test]
fn trace_mirrors_other_connections_with_latency_and_redaction() {
    let mut h = Harness::new();
    let mut tracer = h.socket();
    h.initialize(&mut tracer, "operator-cli");
    assert_eq!(
        h.call(&mut tracer, "trace/subscribe", json!({}))["result"],
        json!({})
    );

    let mut cli = h.socket();
    h.engine.receive(
        cli.id,
        &json!({ "jsonrpc": "2.0", "id": "a", "method": "initialize",
                 "params": { "clientInfo": { "name": "t", "version": "0.1.0", "kind": "operator-cli" }, "capabilities": { "protocolVersion": 1 } } })
        .to_string(),
    );
    next(&mut cli);

    let request = next(&mut tracer);
    assert_eq!(request["method"], "trace/frame");
    assert_eq!(request["params"]["direction"], "in");
    assert_eq!(request["params"]["kind"], "request");
    assert_eq!(request["params"]["method"], "initialize");
    assert_eq!(request["params"]["id"], "a");
    assert_eq!(request["params"]["transport"], "socket");

    let response = next(&mut tracer);
    assert_eq!(response["params"]["direction"], "out");
    assert_eq!(response["params"]["kind"], "response");
    assert_eq!(response["params"]["latencyUs"], 0);
    assert_eq!(
        response["params"]["clientId"],
        request["params"]["clientId"]
    );

    // The bootstrap token in a result is redacted.
    h.clock.advance_us(5);
    h.call(&mut cli, "session/createBootstrap", json!({}));
    let _in = next(&mut tracer);
    let out = next(&mut tracer);
    assert_eq!(out["params"]["payload"]["result"]["token"], "[redacted]");
    assert!(
        tracer.inbox.try_recv().is_err(),
        "trace frames are not traced"
    );
}

#[test]
fn a_non_operator_cannot_trace_or_answer_pairing() {
    let mut h = Harness::new();
    let mut agent = h.socket();
    h.initialize(&mut agent, "agent");
    assert_eq!(
        error_kind(&h.call(&mut agent, "trace/subscribe", json!({}))),
        "capability-denied"
    );
    assert_eq!(
        error_kind(&h.call(&mut agent, "pairing/approve", json!({ "code": "000000" }))),
        "capability-denied"
    );
}

#[test]
fn pairing_round_trip() {
    let mut h = Harness::new();
    let (id, code) = h
        .engine
        .request_pairing("http://localhost:5173", Some("Firefox".into()));
    assert_eq!(code.len(), 6);
    assert_eq!(
        h.engine.poll_pairing(&id),
        PairingState::Pending { code: code.clone() }
    );

    let mut cli = h.socket();
    h.initialize(&mut cli, "operator-cli");
    let list = h.call(&mut cli, "pairing/list", json!(null));
    assert_eq!(list["result"]["requests"][0]["code"], code);
    assert_eq!(
        list["result"]["requests"][0]["origin"],
        "http://localhost:5173"
    );

    assert_eq!(
        error_kind(&h.call(&mut cli, "pairing/approve", json!({ "code": "999999" }))),
        "not-found"
    );
    // Every operator's app is asked, with Allow and Deny, until it is answered.
    let condition = format!("pairing:{code}");
    let asked = |h: &Harness| -> Vec<_> {
        h.engine
            .state()
            .notifications
            .values()
            .filter(|n| n.condition.as_deref() == Some(condition.as_str()))
            .cloned()
            .collect()
    };
    let question = asked(&h);
    assert_eq!(question.len(), 1);
    assert_eq!(
        question[0].title,
        "localhost:5173 asks to connect to UXDuck"
    );
    assert!(question[0].body.as_deref().unwrap().contains(&code[..3]));
    assert_eq!(question[0].actions.len(), 2);
    assert_eq!(
        h.call(&mut cli, "pairing/approve", json!({ "code": code }))["result"],
        json!({})
    );
    assert!(asked(&h).is_empty(), "answered, so resolved");

    let PairingState::Approved { session } = h.engine.poll_pairing(&id) else {
        panic!("not approved")
    };
    assert_eq!(
        h.engine.session(&session).map(|s| s.origin),
        Some("http://localhost:5173".into())
    );
    assert_eq!(
        h.engine.poll_pairing(&id),
        PairingState::Unknown,
        "a session is collected once"
    );
}

#[test]
fn pairing_can_be_denied_and_expires() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    h.initialize(&mut cli, "operator-cli");
    let (denied, code) = h.engine.request_pairing("http://localhost:5173", None);
    h.call(&mut cli, "pairing/deny", json!({ "code": code }));
    assert_eq!(h.engine.poll_pairing(&denied), PairingState::Denied);

    let (stale, _) = h.engine.request_pairing("http://localhost:5173", None);
    h.clock.advance_us(11 * 60 * 1_000_000);
    assert_eq!(h.engine.poll_pairing(&stale), PairingState::Unknown);
}

#[test]
fn one_question_per_browser_gone_once_it_stops_waiting() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    h.initialize(&mut cli, "operator-cli");
    let questions = |h: &Harness| -> Vec<String> {
        h.engine
            .state()
            .notifications
            .values()
            .filter_map(|n| n.condition.clone())
            .filter(|c| c.starts_with("pairing:"))
            .collect()
    };
    let chrome = Some("Chrome".to_owned());

    // Two tabs of one browser: one question, with the newest tab's code.
    let (first, _) = h
        .engine
        .request_pairing("http://localhost:5173", chrome.clone());
    h.clock.advance_us(1_000_000);
    let (second, code) = h
        .engine
        .request_pairing("http://localhost:5173", chrome.clone());
    assert_eq!(questions(&h), vec![format!("pairing:{code}")]);

    // Answering it lets both tabs in.
    h.call(&mut cli, "pairing/approve", json!({ "code": code }));
    assert!(questions(&h).is_empty());
    assert!(matches!(
        h.engine.poll_pairing(&first),
        PairingState::Approved { .. }
    ));
    assert!(matches!(
        h.engine.poll_pairing(&second),
        PairingState::Approved { .. }
    ));

    // A tab that stops polling (closed, reloaded) takes its question away.
    let (_gone, gone_code) = h.engine.request_pairing("http://localhost:5173", chrome);
    assert_eq!(questions(&h).len(), 1);
    h.clock.advance_us(16 * 1_000_000);
    h.engine.expire_pairings();
    assert!(questions(&h).is_empty(), "abandoned, so resolved");

    // Answering a question whose browser has gone says so and clears it.
    let reply = h.call(&mut cli, "pairing/approve", json!({ "code": gone_code }));
    assert_eq!(error_kind(&reply), "not-found");
    assert!(questions(&h).is_empty());
}

#[test]
fn bootstrap_tokens_work_once_within_a_minute_and_only_over_the_socket() {
    let mut h = Harness::new();
    let mut cli = h.socket();
    h.initialize(&mut cli, "operator-cli");
    let reply = h.call(&mut cli, "session/createBootstrap", json!({}));
    let token = reply["result"]["token"].as_str().expect("token").to_owned();
    assert_eq!(reply["result"]["uiUrl"], "http://127.0.0.1:47800/");
    assert!(h.engine.exchange_bootstrap(&token).is_some());
    assert!(h.engine.exchange_bootstrap(&token).is_none());

    let late = h.engine.create_bootstrap();
    h.clock.advance_us(61 * 1_000_000);
    assert!(h.engine.exchange_bootstrap(&late).is_none());

    let mut browser = h.connect(
        Transport::WebSocket,
        Trust::PairedBrowser {
            origin: "http://localhost:5173".into(),
        },
    );
    h.initialize(&mut browser, "operator-ui");
    assert_eq!(
        error_kind(&h.call(&mut browser, "session/createBootstrap", json!({}))),
        "capability-denied"
    );
}

#[test]
fn disconnect_forgets_the_connection() {
    let mut h = Harness::new();
    let mut tracer = h.socket();
    h.initialize(&mut tracer, "operator-cli");
    h.call(&mut tracer, "trace/subscribe", json!({}));
    assert_eq!(h.engine.connection_count(), 1);
    h.engine.disconnect(tracer.id);
    assert_eq!(h.engine.connection_count(), 0);
    let mut other = h.socket();
    h.initialize(&mut other, "operator-cli");
}

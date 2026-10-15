//! The Anthropic provider against a local server speaking the Messages
//! API's server-sent events: the cache breakpoints, a stateless tool round
//! trip that replays thinking unchanged, the hosted model's headers, and
//! retries and the fallback model through the resilient wrapper.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use uxduck_agent::provider::anthropic::AnthropicProvider;
use uxduck_agent::provider::resilient::Resilient;
use uxduck_agent::provider::{Sleep, hosted};
use uxduck_agent::{Host, Turn, Wire, run_turn};

fn sse(events: &[Value]) -> String {
    use std::fmt::Write as _;
    events.iter().fold(String::new(), |mut out, event| {
        let _ = write!(
            out,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        );
        out
    })
}

fn tool_round() -> String {
    sse(&[
        json!({ "type": "message_start", "message": { "id": "msg_1", "usage": {
            "input_tokens": 20, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 3000, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig-1" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "vehicle_list", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{}" } }),
        json!({ "type": "content_block_stop", "index": 1 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 30 } }),
        json!({ "type": "message_stop" }),
    ])
}

fn answer() -> String {
    sse(&[
        json!({ "type": "message_start", "message": { "id": "msg_2", "usage": {
            "input_tokens": 15, "cache_read_input_tokens": 3000, "cache_creation_input_tokens": 120, "output_tokens": 1 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "No vehicles." } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 5 } }),
        json!({ "type": "message_stop" }),
    ])
}

#[derive(Clone, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Value,
}

/// Serves `<path>`: `busy` overloaded answers first, then a tool round and
/// an answer; records each request.
async fn serve(path: &'static str, seen: Arc<Mutex<Vec<Seen>>>, busy: usize) -> String {
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        path,
        post(move |headers: HeaderMap, body: axum::Json<Value>| {
            let (seen, count) = (seen.clone(), count.clone());
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                seen.lock().unwrap().push(Seen {
                    headers,
                    body: body.0.clone(),
                });
                if n < busy || body.0["model"] == "claude-always-busy" {
                    let mut response = (
                        StatusCode::from_u16(529).unwrap(),
                        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                    )
                        .into_response();
                    response
                        .headers_mut()
                        .insert(header::RETRY_AFTER, "0".parse().unwrap());
                    return response;
                }
                let replies = body.0["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["content"].to_string().contains("tool_result"));
                let events = if replies { answer() } else { tool_round() };
                ([(header::CONTENT_TYPE, "text/event-stream")], events).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

#[derive(Default)]
struct Log;

impl Host for Log {
    fn message_start(&self) -> BoxFuture<'_, String> {
        async { "m".to_owned() }.boxed()
    }
    fn text_delta(&self, _: String, _: String) -> BoxFuture<'_, ()> {
        async {}.boxed()
    }
    fn message_finish(&self, _: String, _: Option<String>) -> BoxFuture<'_, ()> {
        async {}.boxed()
    }
    fn tool_start(&self, _: String, _: Value) -> BoxFuture<'_, String> {
        async { "t".to_owned() }.boxed()
    }
    fn call(&self, _: String, _: Value) -> BoxFuture<'_, Result<Value, Value>> {
        async { Ok(json!({ "vehicles": [] })) }.boxed()
    }
    fn tool_finish(&self, _: String, _: Result<Value, Value>) -> BoxFuture<'_, ()> {
        async {}.boxed()
    }
}

fn turn(model: &str) -> Turn {
    Turn {
        model: model.into(),
        text: "what vehicles do I have".into(),
        conversation: vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "hello" }] }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "Hi." }] }),
        ],
        context: Some("This thread is about Copter 1.".into()),
        context_sent: false,
        previous: None,
        cache_key: None,
    }
}

fn sleep() -> Sleep {
    Arc::new(|every: Duration| tokio::time::sleep(every).boxed())
}

#[tokio::test]
async fn a_stateless_tool_round_caches_and_replays_thinking_unchanged() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/messages", seen.clone(), 0).await;
    let provider = AnthropicProvider::new("sk-ant-test", Some(&format!("{base}/v1")));
    let outcome = run_turn(&provider, &Log, turn("claude-opus-5-5")).await;
    assert_eq!(outcome.result, Ok(()));
    assert_eq!(outcome.stats.usage.cache_write_tokens, 3120);
    assert_eq!(outcome.stats.usage.cache_read_tokens, 3000);

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].headers["x-api-key"], "sk-ant-test");
    assert!(
        seen[0].headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("thinking-binding-controls-2026-08-01")
    );
    let (first, second) = (&seen[0].body, &seen[1].body);
    assert_eq!(
        first["system"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(first["cache_control"], json!({ "type": "ephemeral" }));
    let first_messages = first["messages"].as_array().unwrap();
    assert_eq!(first_messages.len(), 4);
    assert_eq!(
        first_messages[3],
        json!({ "role": "system", "content": "This thread is about Copter 1." }),
        "context after the message, not in the cached system prompt"
    );
    let second_messages = second["messages"].as_array().unwrap();
    assert_eq!(second_messages[..4], first_messages[..], "append-only");
    assert_eq!(
        second_messages[4],
        json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "", "signature": "sig-1" },
            { "type": "tool_use", "id": "toolu_1", "name": "vehicle_list", "input": {} },
        ] }),
        "thinking replayed exactly as it came"
    );
    assert_eq!(second_messages[5]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(
        second["system"], first["system"],
        "the cached prefix stays the same"
    );
    assert_eq!(second["tools"], first["tools"]);
}

#[tokio::test]
async fn the_hosted_model_hears_which_response_a_round_continues() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/model/messages", seen.clone(), 0).await;
    let provider = hosted::provider(&base, "session-1", true, "th123456789", Wire::Anthropic);
    run_turn(provider.as_ref(), &Log, turn("claude-sonnet-5-5")).await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].headers["authorization"], "Bearer session-1");
    assert_eq!(seen[0].headers["x-uxduck-share"], "conversations");
    assert!(seen[0].headers.get("x-uxduck-continues").is_none());
    assert_eq!(seen[1].headers["x-uxduck-continues"], "msg_1");
}

#[tokio::test]
async fn an_overloaded_model_is_retried_and_one_that_stays_busy_falls_back() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/messages", seen.clone(), 2).await;
    let provider = Resilient::new(
        Box::new(AnthropicProvider::new("k", Some(&format!("{base}/v1")))),
        sleep(),
    );
    let outcome = run_turn(&provider, &Log, turn("claude-opus-5-5")).await;
    assert_eq!(outcome.result, Ok(()));
    assert_eq!(outcome.stats.retries, 2);
    assert!(!outcome.stats.fell_back);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/messages", seen.clone(), 0).await;
    let provider = Resilient::new(
        Box::new(AnthropicProvider::new("k", Some(&format!("{base}/v1")))),
        sleep(),
    )
    .with_fallback(Some("claude-sonnet-5-5"));
    let outcome = run_turn(&provider, &Log, turn("claude-always-busy")).await;
    assert_eq!(outcome.result, Ok(()));
    assert!(outcome.stats.fell_back);
    let models: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.body["model"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        models[..4],
        [
            "claude-always-busy",
            "claude-always-busy",
            "claude-always-busy",
            "claude-sonnet-5-5"
        ],
        "three tries, then the fallback"
    );
}

#[tokio::test]
async fn a_stalled_answer_is_ended() {
    let app = Router::new().route(
        "/v1/messages",
        post(|| async {
            let stalled = futures::stream::once(async {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
                ))
            })
            .chain(futures::stream::pending());
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(stalled),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = Resilient::new(
        Box::new(AnthropicProvider::new(
            "k",
            Some(&format!("http://{address}/v1")),
        )),
        sleep(),
    )
    .with_timeouts(Duration::from_millis(500), Duration::from_millis(200))
    .with_attempts(1);
    let outcome = run_turn(&provider, &Log, turn("claude-opus-5-5")).await;
    let why = outcome.result.unwrap_err().to_string();
    assert!(why.contains("no answer came in time"), "{why}");
}

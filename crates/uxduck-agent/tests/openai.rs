//! The OpenAI provider against a local server speaking the Responses API's
//! server-sent events: a stateless tool round trip, the hosted model's
//! headers, its refusals passed on, and redirects not followed.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use uxduck_agent::provider::openai::OpenAiProvider;
use uxduck_agent::provider::{ProviderError, hosted};
use uxduck_agent::{Host, Turn, TurnError, Wire, run_turn};

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

#[derive(Clone, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Value,
}

/// Serves `<path>`: a tool call, then text; records each request.
async fn serve(path: &'static str, seen: Arc<Mutex<Vec<Seen>>>) -> String {
    let app = Router::new().route(
        path,
        post(move |headers: HeaderMap, body: axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                let round = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(Seen {
                        headers,
                        body: body.0,
                    });
                    seen.len()
                };
                let events = if round == 1 {
                    let call = json!({ "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "vehicle_list", "arguments": "{}" });
                    vec![
                        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
                        json!({ "type": "response.output_item.done", "output_index": 1, "item": call }),
                        json!({ "type": "response.completed", "response": {
                            "id": "resp_1",
                            "output": [{ "type": "reasoning", "id": "rs_1", "encrypted_content": "secret" }, call],
                            "usage": { "input_tokens": 3000, "input_tokens_details": { "cached_tokens": 2048 }, "output_tokens": 12 }
                        } }),
                    ]
                } else {
                    vec![
                        json!({ "type": "response.output_text.delta", "delta": "No " }),
                        json!({ "type": "response.output_text.delta", "delta": "vehicles." }),
                        json!({ "type": "response.completed", "response": {
                            "id": "resp_2",
                            "output": [{ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "No vehicles." }] }],
                            "usage": { "input_tokens": 3100, "input_tokens_details": { "cached_tokens": 3000 }, "output_tokens": 4 }
                        } }),
                    ]
                };
                ([(header::CONTENT_TYPE, "text/event-stream")], sse(&events)).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Host for Log {
    fn message_start(&self) -> BoxFuture<'_, String> {
        async { "m".to_owned() }.boxed()
    }
    fn text_delta(&self, _: String, delta: String) -> BoxFuture<'_, ()> {
        self.0.lock().unwrap().push(delta);
        async {}.boxed()
    }
    fn message_finish(&self, _: String, response: Option<String>) -> BoxFuture<'_, ()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("finished in {}", response.unwrap_or_default()));
        async {}.boxed()
    }
    fn tool_start(&self, method: String, _: Value) -> BoxFuture<'_, String> {
        self.0.lock().unwrap().push(format!("tool {method}"));
        async { "t".to_owned() }.boxed()
    }
    fn call(&self, _: String, _: Value) -> BoxFuture<'_, Result<Value, Value>> {
        async { Ok(json!({ "vehicles": [] })) }.boxed()
    }
    fn tool_finish(&self, _: String, _: Result<Value, Value>) -> BoxFuture<'_, ()> {
        async {}.boxed()
    }
}

fn turn(conversation: Vec<Value>) -> Turn {
    Turn {
        model: "gpt-test".into(),
        text: "what vehicles do I have".into(),
        conversation,
        context: None,
        context_sent: false,
        previous: None,
        cache_key: Some("chat-1".into()),
    }
}

#[tokio::test]
async fn a_stateless_tool_round_trip_replays_the_conversation() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/responses", seen.clone()).await;
    let provider = OpenAiProvider::new("sk-test", Some(&format!("{base}/v1")));
    let host = Log::default();
    let earlier = vec![
        json!({ "role": "user", "content": "hello" }),
        json!({ "role": "assistant", "content": "Hi." }),
    ];
    let outcome = run_turn(&provider, &host, turn(earlier.clone())).await;
    assert_eq!(outcome.result, Ok(()));
    assert_eq!(outcome.response_id.as_deref(), Some("resp_2"));
    assert_eq!(
        host.0.lock().unwrap().as_slice(),
        [
            "tool vehicle/list",
            "No ",
            "vehicles.",
            "finished in resp_2"
        ]
    );
    assert_eq!(outcome.stats.rounds, 2);
    assert_eq!(outcome.stats.usage.cache_read_tokens, 5048);
    assert_eq!(outcome.stats.usage.input_tokens, 1052);

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].headers["authorization"], "Bearer sk-test");
    let (first, second) = (&seen[0].body, &seen[1].body);
    assert_eq!(first["store"], false);
    assert!(first.get("previous_response_id").is_none());
    assert_eq!(first["prompt_cache_key"], "chat-1");
    let first_input = first["input"].as_array().unwrap();
    assert_eq!(
        first_input[..2],
        earlier[..],
        "the earlier conversation, replayed"
    );
    assert_eq!(
        first_input[2],
        json!({ "role": "user", "content": "what vehicles do I have" })
    );
    let second_input = second["input"].as_array().unwrap();
    assert_eq!(
        second_input[..3],
        first_input[..],
        "the second request starts as the first"
    );
    assert_eq!(
        second_input[3]["encrypted_content"], "secret",
        "reasoning replayed"
    );
    assert_eq!(second_input[4]["call_id"], "call_1");
    assert_eq!(
        second_input[5],
        json!({ "type": "function_call_output", "call_id": "call_1", "output": r#"{"vehicles":[]}"# })
    );
    assert_eq!(outcome.conversation.len(), 7, "and the answer, to keep");
}

#[tokio::test]
async fn the_hosted_model_gets_the_session_and_which_response_a_round_continues() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = serve("/v1/model/responses", seen.clone()).await;
    let provider = hosted::provider(&base, "session-1", false, "th123456789", Wire::OpenAi);
    run_turn(provider.as_ref(), &Log::default(), turn(Vec::new())).await;
    let seen = seen.lock().unwrap();
    let get = |i: usize, name: &str| {
        seen[i]
            .headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
    };
    assert_eq!(get(0, "authorization").as_deref(), Some("Bearer session-1"));
    assert_eq!(get(0, "x-uxduck-share").as_deref(), Some("none"));
    assert_eq!(get(0, "x-uxduck-thread").as_deref(), Some("th123456789"));
    assert_eq!(
        get(0, "x-uxduck-continues"),
        None,
        "a new message starts a chat"
    );
    assert_eq!(get(1, "x-uxduck-continues").as_deref(), Some("resp_1"));
}

async fn answering(
    status: StatusCode,
    headers: Vec<(header::HeaderName, &'static str)>,
    body: &'static str,
) -> String {
    let app = Router::new().route(
        "/v1/model/responses",
        post(move || {
            let headers = headers.clone();
            async move {
                let mut response = (status, body).into_response();
                for (name, value) in headers {
                    response.headers_mut().insert(name, value.parse().unwrap());
                }
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

#[tokio::test]
async fn the_hosted_models_refusal_is_shown_as_written_and_a_redirect_is_not_followed() {
    let base = answering(
        StatusCode::PAYMENT_REQUIRED,
        vec![],
        r#"{"error":{"message":"Your trial chats are used up.","type":"uxduck","code":"no-allowance"}}"#,
    )
    .await;
    let provider = hosted::provider(&base, "s", true, "th123456789", Wire::OpenAi);
    let outcome = run_turn(provider.as_ref(), &Log::default(), turn(Vec::new())).await;
    assert_eq!(
        outcome.result,
        Err(TurnError::Provider(ProviderError::Account(
            "Your trial chats are used up.".into()
        )))
    );
    assert_eq!(
        outcome.result.unwrap_err().to_string(),
        "Your trial chats are used up."
    );

    let base = answering(
        StatusCode::FOUND,
        vec![(header::LOCATION, "https://orth.cloudflareaccess.com/login")],
        "",
    )
    .await;
    let provider = hosted::provider(&base, "s", true, "th123456789", Wire::OpenAi);
    let outcome = run_turn(provider.as_ref(), &Log::default(), turn(Vec::new())).await;
    let Err(TurnError::Provider(ProviderError::Network(why))) = outcome.result else {
        panic!("{:?}", outcome.result);
    };
    assert!(why.contains("sign in to it first"), "{why}");
}

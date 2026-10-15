//! Anthropic's Messages API. Stateless: each request carries the whole
//! conversation, which the host replays byte for byte (see
//! `conversation`). Built for the prompt cache:
//!
//! - the instructions are one system block with a breakpoint, which caches
//!   the tools and instructions together, the same for every chat;
//! - top-level automatic caching moves a second breakpoint to the end of
//!   each request, so each tool round and turn reads what the last wrote;
//! - per-chat context arrives as a `role: "system"` message in the
//!   conversation, never in the system prompt;
//! - thinking and effort are pinned per model, since changing them between
//!   requests would invalidate the cache.
//!
//! Earlier thinking is replayed unchanged. If the history ever fails
//! Anthropic's check of it, `drop_block` sets the reasoning aside instead
//! of failing the request, and the response says how much
//! (`Usage::thinking_dropped`). A refusal is retried on another model by
//! Anthropic itself (`fallbacks: "default"`); when that happens partway
//! through, only the text from before the switch is kept, as Anthropic
//! asks, and only the tools the model that finished asked for are run.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::http::{self, client};
use super::{Provider, ProviderError, ProviderEvent, ProviderRequest, Usage};
use crate::conversation::Wire;

const DEFAULT_API_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Tolerating a mismatched history, and refusal fallbacks.
const BETAS: &str = "thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01";

/// Models that think adaptively, so take `block_binding` and `effort`.
const THINKING: [&str; 10] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
];

/// Models whose refusals Anthropic may hand to another model.
const FALLS_BACK: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-opus-5",
    "claude-fable-5-1",
];

#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_base: String,
    headers: HeaderMap,
    /// Sends `X-UXDuck-Continues` (the hosted model only).
    hosted: bool,
}

impl AnthropicProvider {
    /// With a developer's own key; `api_base` defaults to Anthropic's.
    #[must_use]
    pub fn new(api_key: &str, api_base: Option<&str>) -> Self {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(api_key) {
            headers.insert(HeaderName::from_static("x-api-key"), value);
        }
        Self::build(headers, api_base, false)
    }

    /// Through UXDuck's hosted model, with the session and its headers.
    #[must_use]
    pub fn hosted(session: &str, api_base: &str, extra: &[(&'static str, &str)]) -> Self {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {session}")) {
            headers.insert(AUTHORIZATION, value);
        }
        for (name, value) in extra {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
        Self::build(headers, Some(api_base), true)
    }

    fn build(mut headers: HeaderMap, api_base: Option<&str>, hosted: bool) -> Self {
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static(BETAS),
        );
        Self {
            client: client(),
            api_base: api_base
                .unwrap_or(DEFAULT_API_BASE)
                .trim_end_matches('/')
                .to_owned(),
            headers,
            hosted,
        }
    }
}

fn is(models: &[&str], model: &str) -> bool {
    models.contains(&model)
}

/// The `POST /messages` body.
#[must_use]
pub fn body(request: &ProviderRequest) -> Value {
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_output_tokens,
        "system": [{
            "type": "text",
            "text": request.instructions,
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": request.conversation,
        "tools": tools,
        "stream": true,
        "cache_control": { "type": "ephemeral" },
    });
    if is(&THINKING, &request.model) {
        body["thinking"] = json!({
            "type": "adaptive",
            "block_binding": { "prefix_mismatch_behavior": "drop_block" },
        });
        if let Some(effort) = request.effort {
            body["output_config"] = json!({ "effort": effort });
        }
    }
    if is(&FALLS_BACK, &request.model) {
        body["fallbacks"] = json!("default");
    }
    body
}

/// One response as it streams.
#[derive(Debug, Default)]
struct Answer {
    id: Option<String>,
    blocks: BTreeMap<usize, Value>,
    partial: HashMap<usize, String>,
    usage: Usage,
}

fn block_index(value: &Value) -> Option<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
}

fn append(block: &mut Value, field: &str, delta: &Value, from: &str) {
    let current = block[field].as_str().unwrap_or_default().to_owned();
    let more = delta.get(from).and_then(Value::as_str).unwrap_or_default();
    block[field] = json!(format!("{current}{more}"));
}

fn usage_from(value: &Value, usage: &mut Usage) {
    let count = |name: &str| value.get(name).and_then(Value::as_u64);
    if let Some(n) = count("input_tokens") {
        usage.input_tokens = n;
    }
    if let Some(n) = count("cache_read_input_tokens") {
        usage.cache_read_tokens = n;
    }
    if let Some(n) = count("cache_creation_input_tokens") {
        usage.cache_write_tokens = n;
    }
    if let Some(n) = count("output_tokens") {
        usage.output_tokens = n;
    }
}

/// The response's id and its usage so far, with how much earlier
/// reasoning Anthropic set aside.
fn started(message: &Value, answer: &mut Answer) {
    answer.id = message["id"].as_str().map(str::to_owned);
    usage_from(&message["usage"], &mut answer.usage);
    let dropped = message["input_transformations"]
        .as_array()
        .map_or(0, |list| {
            list.iter()
                .filter(|t| t["type"] == "thinking_dropped")
                .count()
        });
    answer.usage.thinking_dropped = u32::try_from(dropped).unwrap_or(u32::MAX);
}

/// The answer's content to replay. Before the last `fallback` block (a
/// refusal handed to another model partway through) only text carries
/// over; the marker itself is dropped, as are empty text blocks.
fn replayable(blocks: Vec<Value>) -> Vec<Value> {
    let boundary = blocks.iter().rposition(|block| block["type"] == "fallback");
    blocks
        .into_iter()
        .enumerate()
        .filter(|(at, block)| match boundary {
            Some(boundary) if *at <= boundary => block["type"] == "text",
            _ => true,
        })
        .map(|(_, block)| block)
        .filter(|block| {
            block["type"] != "text" || block["text"].as_str().is_some_and(|t| !t.is_empty())
        })
        .collect()
}

/// One streamed event: what to pass on, if anything yet. Tool calls wait
/// for the end of the answer, since a fallback partway through voids the
/// ones before it.
fn event(value: &Value, answer: &mut Answer) -> Result<Vec<ProviderEvent>, ProviderError> {
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "message_start" => started(&value["message"], answer),
        "content_block_start" => {
            if let (Some(index), Some(block)) = (block_index(value), value.get("content_block")) {
                answer.blocks.insert(index, block.clone());
            }
        }
        "content_block_delta" => {
            let Some(index) = block_index(value) else {
                return Ok(Vec::new());
            };
            let delta = &value["delta"];
            let block = answer.blocks.entry(index).or_insert_with(|| json!({}));
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => {
                    append(block, "text", delta, "text");
                    let text = delta["text"].as_str().unwrap_or_default();
                    return Ok(vec![ProviderEvent::TextDelta(text.to_owned())]);
                }
                "input_json_delta" => answer
                    .partial
                    .entry(index)
                    .or_default()
                    .push_str(delta["partial_json"].as_str().unwrap_or_default()),
                "thinking_delta" => append(block, "thinking", delta, "thinking"),
                "signature_delta" => append(block, "signature", delta, "signature"),
                _ => {}
            }
        }
        "content_block_stop" => {
            let partial = block_index(value).and_then(|index| {
                Some((
                    answer.partial.remove(&index)?,
                    answer.blocks.get_mut(&index)?,
                ))
            });
            if let Some((partial, block)) = partial {
                block["input"] = serde_json::from_str(&partial).unwrap_or_else(|_| json!({}));
            }
        }
        "message_delta" => {
            usage_from(&value["usage"], &mut answer.usage);
            if value.pointer("/delta/stop_reason") == Some(&json!("refusal")) {
                return Err(ProviderError::Refused(
                    "Claude declined to answer this.".into(),
                ));
            }
        }
        "message_stop" => {
            let content = replayable(answer.blocks.values().cloned().collect());
            let mut out: Vec<ProviderEvent> = content
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .map(|block| ProviderEvent::ToolCall {
                    call_id: block["id"].as_str().unwrap_or_default().to_owned(),
                    name: block["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: block["input"].to_string(),
                })
                .collect();
            out.push(ProviderEvent::Completed {
                response_id: answer.id.clone().unwrap_or_default(),
                output: vec![json!({ "role": "assistant", "content": content })],
                usage: answer.usage.clone(),
            });
            return Ok(out);
        }
        "error" => {
            let kind = value.pointer("/error/type").and_then(Value::as_str);
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            return Err(match kind {
                Some("overloaded_error" | "rate_limit_error" | "api_error") => {
                    ProviderError::Busy {
                        message,
                        retry_after: None,
                    }
                }
                _ => ProviderError::Refused(message),
            });
        }
        _ => {}
    }
    Ok(Vec::new())
}

impl Provider for AnthropicProvider {
    fn wire(&self) -> Wire {
        Wire::Anthropic
    }

    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let client = self.client.clone();
        let mut headers = self.headers.clone();
        if self.hosted
            && let Some(previous) = request
                .continues
                .as_deref()
                .and_then(|id| HeaderValue::from_str(id).ok())
        {
            headers.insert(HeaderName::from_static("x-uxduck-continues"), previous);
        }
        let url = format!("{}/messages", self.api_base);
        let body = body(&request);
        stream::once(async move { http::post(&client, &url, headers, &body).await })
            .flat_map(|opened| match opened {
                Err(error) => stream::once(async move { Err(error) }).boxed(),
                Ok(response) => {
                    let mut answer = Answer::default();
                    let done = Arc::new(AtomicBool::new(false));
                    let seen = done.clone();
                    http::events(response)
                        .flat_map(move |next| {
                            let out: Vec<Result<ProviderEvent, ProviderError>> = match next {
                                Ok(value) => match event(&value, &mut answer) {
                                    Ok(events) => {
                                        if events
                                            .iter()
                                            .any(|e| matches!(e, ProviderEvent::Completed { .. }))
                                        {
                                            seen.store(true, Ordering::SeqCst);
                                        }
                                        events.into_iter().map(Ok).collect()
                                    }
                                    Err(error) => {
                                        seen.store(true, Ordering::SeqCst);
                                        vec![Err(error)]
                                    }
                                },
                                Err(error) => {
                                    seen.store(true, Ordering::SeqCst);
                                    vec![Err(error)]
                                }
                            };
                            stream::iter(out)
                        })
                        .chain(http::cut_short(done))
                        .boxed()
                }
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tools;

    fn request(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.into(),
            instructions: "be brief".into(),
            conversation: vec![
                json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
            ],
            tools: tools(),
            max_output_tokens: 32_000,
            effort: Some("medium"),
            continues: None,
            cache_key: None,
        }
    }

    #[test]
    fn the_request_caches_the_instructions_and_the_conversation() {
        let body = body(&request("claude-opus-5-5"));
        assert_eq!(
            body["system"],
            json!([{ "type": "text", "text": "be brief", "cache_control": { "type": "ephemeral" } }])
        );
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "block_binding": { "prefix_mismatch_behavior": "drop_block" } })
        );
        assert_eq!(body["output_config"], json!({ "effort": "medium" }));
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["max_tokens"], 32_000);
        assert!(body.get("temperature").is_none());
        assert!(
            body.get("tool_choice").is_none(),
            "forced tool use is a 400 on Opus 5.5"
        );
        assert_eq!(body["tools"][0]["name"], "vehicle_list");

        let haiku = super::body(&request("claude-haiku-4-5"));
        assert!(haiku.get("thinking").is_none());
        assert!(haiku.get("fallbacks").is_none());
    }

    #[test]
    fn a_response_becomes_the_assistant_turn_to_replay_with_its_usage() {
        let mut answer = Answer::default();
        let events = [
            json!({ "type": "message_start", "message": {
                "id": "msg_1",
                "usage": { "input_tokens": 12, "cache_read_input_tokens": 4000, "cache_creation_input_tokens": 300, "output_tokens": 1 },
                "input_transformations": [{ "type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch" }]
            } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } }),
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "sig" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": "Looking." } }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "vehicle_get", "input": {} } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "{\"id\":" } }),
            json!({ "type": "content_block_delta", "index": 2, "delta": { "type": "input_json_delta", "partial_json": "\"v1\"}" } }),
            json!({ "type": "content_block_stop", "index": 2 }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 40 } }),
            json!({ "type": "message_stop" }),
        ];
        let out: Vec<ProviderEvent> = events
            .iter()
            .flat_map(|value| event(value, &mut answer).unwrap())
            .collect();
        assert_eq!(out[0], ProviderEvent::TextDelta("Looking.".into()));
        assert_eq!(
            out[1],
            ProviderEvent::ToolCall {
                call_id: "toolu_1".into(),
                name: "vehicle_get".into(),
                arguments: r#"{"id":"v1"}"#.into()
            }
        );
        let ProviderEvent::Completed {
            response_id,
            output,
            usage,
        } = &out[2]
        else {
            panic!("{out:?}");
        };
        assert_eq!(response_id, "msg_1");
        assert_eq!(
            output[0],
            json!({ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "", "signature": "sig" },
                { "type": "text", "text": "Looking." },
                { "type": "tool_use", "id": "toolu_1", "name": "vehicle_get", "input": { "id": "v1" } },
            ] })
        );
        assert_eq!(
            usage,
            &Usage {
                input_tokens: 12,
                output_tokens: 40,
                cache_read_tokens: 4000,
                cache_write_tokens: 300,
                thinking_dropped: 1,
            }
        );
    }

    #[test]
    fn after_a_fallback_partway_only_the_earlier_text_and_the_new_models_answer_remain() {
        let mut answer = Answer::default();
        let events = [
            json!({ "type": "message_start", "message": { "id": "msg_1", "usage": {} } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "a" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "Checking" } }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_old", "name": "vehicle_arm", "input": {} } }),
            json!({ "type": "content_block_stop", "index": 2 }),
            json!({ "type": "content_block_start", "index": 3, "content_block": {
                "type": "fallback", "from": { "model": "claude-opus-5-5" }, "to": { "model": "claude-opus-5" } } }),
            json!({ "type": "content_block_stop", "index": 3 }),
            json!({ "type": "content_block_start", "index": 4, "content_block": { "type": "tool_use", "id": "toolu_new", "name": "vehicle_list", "input": {} } }),
            json!({ "type": "content_block_stop", "index": 4 }),
            json!({ "type": "message_stop" }),
        ];
        let out: Vec<ProviderEvent> = events
            .iter()
            .flat_map(|value| event(value, &mut answer).unwrap())
            .collect();
        assert_eq!(
            out[0],
            ProviderEvent::ToolCall {
                call_id: "toolu_new".into(),
                name: "vehicle_list".into(),
                arguments: "{}".into()
            },
            "the voided call is not run"
        );
        let ProviderEvent::Completed { output, .. } = &out[1] else {
            panic!("{out:?}");
        };
        assert_eq!(
            output[0]["content"],
            json!([
                { "type": "text", "text": "Checking" },
                { "type": "tool_use", "id": "toolu_new", "name": "vehicle_list", "input": {} },
            ])
        );
    }

    #[test]
    fn a_refusal_and_an_overload_in_the_stream_are_errors() {
        let mut answer = Answer::default();
        assert!(matches!(
            event(
                &json!({ "type": "message_delta", "delta": { "stop_reason": "refusal" } }),
                &mut answer
            ),
            Err(ProviderError::Refused(_))
        ));
        assert!(event(
            &json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } }),
            &mut answer
        )
        .unwrap_err()
        .retryable());
    }
}

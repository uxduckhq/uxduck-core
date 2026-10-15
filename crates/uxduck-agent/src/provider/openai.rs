//! OpenAI's Responses API, used statelessly: `store: false`, the whole
//! conversation in `input`, and each response's output items appended to
//! it exactly as they came, encrypted reasoning included, so the next
//! request continues the reasoning and shares its prefix with this one.
//! OpenAI caches repeated prefixes by itself; `prompt_cache_key` keeps one
//! chat's requests together. Nothing is kept at OpenAI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::http::{self, client};
use super::{Provider, ProviderError, ProviderEvent, ProviderRequest, Usage};
use crate::conversation::Wire;

const DEFAULT_API_BASE: &str = "https://api.openai.com/v1";

#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_base: String,
    headers: HeaderMap,
    /// Sends `X-UXDuck-Continues` (the hosted model only).
    hosted: bool,
}

impl OpenAiProvider {
    /// With a developer's own key; `api_base` defaults to OpenAI's.
    #[must_use]
    pub fn new(api_key: &str, api_base: Option<&str>) -> Self {
        Self::build(api_key, api_base, &[], false)
    }

    /// Through UXDuck's hosted model, with the session and its headers.
    #[must_use]
    pub fn hosted(session: &str, api_base: &str, extra: &[(&'static str, &str)]) -> Self {
        Self::build(session, Some(api_base), extra, true)
    }

    fn build(
        key: &str,
        api_base: Option<&str>,
        extra: &[(&'static str, &str)],
        hosted: bool,
    ) -> Self {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}")) {
            headers.insert(AUTHORIZATION, value);
        }
        for (name, value) in extra {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
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

/// The `POST /responses` body.
#[must_use]
pub fn body(request: &ProviderRequest) -> Value {
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": request.conversation,
        "tools": tools,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": request.max_output_tokens,
    });
    if let Some(key) = &request.cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    body
}

fn usage(value: &Value) -> Usage {
    let count = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    let cached = count("/input_tokens_details/cached_tokens");
    Usage {
        input_tokens: count("/input_tokens").saturating_sub(cached),
        output_tokens: count("/output_tokens"),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        thinking_dropped: 0,
    }
}

/// One streamed event, or `None` for the many this does not need.
///
/// # Errors
/// A failed or cut-off response, or an error event.
pub fn event(value: &Value) -> Result<Option<ProviderEvent>, ProviderError> {
    let message = |value: &Value| {
        value
            .pointer("/response/error/message")
            .or_else(|| value.pointer("/error/message"))
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned()
    };
    Ok(match value["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => value["delta"]
            .as_str()
            .map(|delta| ProviderEvent::TextDelta(delta.to_owned())),
        "response.output_item.done"
            if value.pointer("/item/type") == Some(&json!("function_call")) =>
        {
            let field = |name: &str| value["item"][name].as_str().unwrap_or_default().to_owned();
            Some(ProviderEvent::ToolCall {
                call_id: field("call_id"),
                name: field("name"),
                arguments: field("arguments"),
            })
        }
        "response.completed" => {
            let response = &value["response"];
            Some(ProviderEvent::Completed {
                response_id: response["id"].as_str().unwrap_or_default().to_owned(),
                output: response["output"].as_array().cloned().unwrap_or_default(),
                usage: usage(&response["usage"]),
            })
        }
        "response.incomplete" => {
            let why = value
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(ProviderError::Refused(format!(
                "the answer was cut off ({why})"
            )));
        }
        "response.failed" | "error" => {
            let code = value
                .pointer("/response/error/code")
                .or_else(|| value.pointer("/error/code"))
                .or_else(|| value.get("code"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(
                if matches!(
                    code,
                    "rate_limit_exceeded" | "server_error" | "server_is_overloaded"
                ) {
                    ProviderError::Busy {
                        message: message(value),
                        retry_after: None,
                    }
                } else {
                    ProviderError::Refused(message(value))
                },
            );
        }
        _ => None,
    })
}

impl Provider for OpenAiProvider {
    fn wire(&self) -> Wire {
        Wire::OpenAi
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
        let url = format!("{}/responses", self.api_base);
        let body = body(&request);
        stream::once(async move { http::post(&client, &url, headers, &body).await })
            .flat_map(|opened| match opened {
                Err(error) => stream::once(async move { Err(error) }).boxed(),
                Ok(response) => {
                    let done = Arc::new(AtomicBool::new(false));
                    let seen = done.clone();
                    http::events(response)
                        .filter_map(move |next| {
                            let result = match next {
                                Ok(value) => event(&value).transpose(),
                                Err(error) => Some(Err(error)),
                            };
                            if matches!(result, Some(Ok(ProviderEvent::Completed { .. }) | Err(_)))
                            {
                                seen.store(true, Ordering::SeqCst);
                            }
                            futures::future::ready(result)
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

    #[test]
    fn the_request_keeps_nothing_at_openai_and_replays_the_conversation() {
        let request = ProviderRequest {
            model: "gpt-6-sol".into(),
            instructions: "be brief".into(),
            conversation: vec![
                json!({ "role": "user", "content": "hi" }),
                json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "e" }),
                json!({ "type": "function_call", "call_id": "c1", "name": "vehicle_list", "arguments": "{}" }),
                json!({ "type": "function_call_output", "call_id": "c1", "output": "{}" }),
            ],
            tools: tools(),
            max_output_tokens: 16_000,
            effort: None,
            continues: Some("resp_1".into()),
            cache_key: Some("chat-1".into()),
        };
        let body = body(&request);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("previous_response_id").is_none(), "stateless");
        assert_eq!(body["input"].as_array().unwrap().len(), 4);
        assert_eq!(body["prompt_cache_key"], "chat-1");
        assert_eq!(body["max_output_tokens"], 16_000);
        assert_eq!(body["tools"][0]["name"], "vehicle_list");
    }

    #[test]
    fn stream_events_become_provider_events_with_cached_tokens() {
        assert_eq!(
            event(&json!({ "type": "response.output_text.delta", "delta": "Hi" })),
            Ok(Some(ProviderEvent::TextDelta("Hi".into())))
        );
        let call = json!({ "type": "response.output_item.done", "item": {
            "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "vehicle_list", "arguments": "{}" } });
        assert_eq!(
            event(&call),
            Ok(Some(ProviderEvent::ToolCall {
                call_id: "call_1".into(),
                name: "vehicle_list".into(),
                arguments: "{}".into()
            }))
        );
        let done = json!({ "type": "response.completed", "response": {
            "id": "resp_9",
            "output": [{ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Hi" }] }],
            "usage": { "input_tokens": 5000, "input_tokens_details": { "cached_tokens": 4096 }, "output_tokens": 20 }
        } });
        let Ok(Some(ProviderEvent::Completed {
            response_id,
            output,
            usage,
        })) = event(&done)
        else {
            panic!("completed");
        };
        assert_eq!(response_id, "resp_9");
        assert_eq!(output.len(), 1);
        assert_eq!(usage.input_tokens, 904);
        assert_eq!(usage.cache_read_tokens, 4096);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(event(&json!({ "type": "response.created" })), Ok(None));
        let failed = json!({ "type": "response.failed", "response": { "error": { "code": "server_error", "message": "oops" } } });
        assert!(event(&failed).unwrap_err().retryable());
        let refused = json!({ "type": "response.failed", "response": { "error": { "code": "invalid_prompt", "message": "no" } } });
        assert_eq!(event(&refused), Err(ProviderError::Refused("no".into())));
    }
}

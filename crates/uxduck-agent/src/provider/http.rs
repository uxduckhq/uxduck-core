//! What the two HTTP providers share: a client that never follows a
//! redirect (a redirect is a network's sign-in page or something else in
//! the way, and following it turns a request into a page that answers 200),
//! what a failed answer means, and server-sent events as JSON.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::Value;

use super::{ProviderError, ProviderEvent};

/// Said when the answer is a redirect or a page that is not the API.
pub const NOT_THE_API: &str = "the answer was not the model's: the network may want you to sign in to it first, or something else is in the way";

/// No total timeout: answers stream for as long as they take, and the
/// resilient wrapper times out a stalled one.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("uxduck-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

/// Posts a JSON body.
pub async fn post(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: &Value,
) -> Result<reqwest::Response, ProviderError> {
    let response = client
        .post(url)
        .headers(headers)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    // A sign-in page can answer 200 too; the model's answer is a stream.
    let streaming = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("text/event-stream"));
    if streaming {
        Ok(response)
    } else {
        Err(ProviderError::Network(NOT_THE_API.into()))
    }
}

/// What a failed answer means: busy and worth retrying, the hosted
/// model's own words, a refusal, or something in the way.
pub async fn failure(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after = retry_after(response.headers());
    let text = response.text().await.unwrap_or_default();
    explain(status.as_u16(), retry_after, &text)
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| Duration::from_secs_f64(seconds.min(60.0)))
}

/// The meaning of a failed answer with this status and body.
#[must_use]
pub fn explain(status: u16, retry_after: Option<Duration>, text: &str) -> ProviderError {
    if (300..400).contains(&status) {
        return ProviderError::Network(NOT_THE_API.into());
    }
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let kind = value.pointer("/error/type").and_then(Value::as_str);
    let code = value.pointer("/error/code").and_then(Value::as_str);
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let busy = matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
        || matches!(kind, Some("overloaded_error" | "rate_limit_error"));
    // Hosted budget/request/round limits are durable admission decisions.
    // Only explicitly temporary hosted 429s are worth retrying or falling back.
    let hosted_retryable =
        status == 429 && matches!(code, Some("rate-limited" | "in-flight" | "provider"));
    if busy && (kind != Some("uxduck") || hosted_retryable) {
        return ProviderError::Busy {
            message: message.unwrap_or_else(|| format!("HTTP {status}")),
            retry_after,
        };
    }
    match (kind, message) {
        (Some("uxduck"), Some(message)) => ProviderError::Account(message),
        (_, Some(message)) => ProviderError::Refused(message),
        // An HTML page, or nothing: not the API answering.
        (_, None) if text.trim_start().starts_with('<') => {
            ProviderError::Network(NOT_THE_API.into())
        }
        (_, None) => ProviderError::Refused(format!("HTTP {status}")),
    }
}

/// An error after the events, unless `done` says the answer finished (or
/// already failed): a stream that just stops is a dropped connection.
pub fn cut_short(
    done: Arc<AtomicBool>,
) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
    stream::once(async move { done.load(Ordering::SeqCst) })
        .filter_map(|done| async move {
            (!done).then(|| {
                Err(ProviderError::Transport(
                    "the answer stopped before it ended".into(),
                ))
            })
        })
        .boxed()
}

/// The JSON of each server-sent event, skipping empty ones and `[DONE]`.
pub fn events(response: reqwest::Response) -> BoxStream<'static, Result<Value, ProviderError>> {
    response
        .bytes_stream()
        .eventsource()
        .filter_map(|next| async move {
            match next {
                Ok(event) if event.data.trim().is_empty() || event.data.trim() == "[DONE]" => None,
                Ok(event) => Some(
                    serde_json::from_str::<Value>(&event.data)
                        .map_err(|error| ProviderError::Transport(error.to_string())),
                ),
                Err(error) => Some(Err(ProviderError::Transport(error.to_string()))),
            }
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_answer_is_explained_by_its_status_and_body() {
        let busy = explain(
            529,
            Some(Duration::from_secs(2)),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert_eq!(
            busy,
            ProviderError::Busy {
                message: "Overloaded".into(),
                retry_after: Some(Duration::from_secs(2))
            }
        );
        assert!(busy.retryable());
        let trial = explain(
            402,
            None,
            r#"{"error":{"type":"uxduck","message":"Your trial chats are used up."}}"#,
        );
        assert_eq!(
            trial,
            ProviderError::Account("Your trial chats are used up.".into())
        );
        assert!(!trial.retryable());
        let limited = explain(
            429,
            None,
            r#"{"error":{"type":"uxduck","code":"rate-limited","message":"Too many requests at once."}}"#,
        );
        assert!(
            limited.retryable(),
            "the hosted model's rate limit is worth waiting out"
        );
        assert_eq!(
            explain(
                400,
                None,
                r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#
            ),
            ProviderError::Refused("bad".into())
        );
        assert!(matches!(explain(302, None, ""), ProviderError::Network(_)));
        assert!(matches!(
            explain(200, None, "<html>sign in to the Wi-Fi</html>"),
            ProviderError::Network(_)
        ));
    }

    #[test]
    fn hosted_budget_and_round_limits_are_shown_without_busy_or_retries() {
        for code in ["daily-cap", "global-cap", "request-cap", "rounds"] {
            let message = "This request would exceed your remaining hosted AI allowance.";
            let body = serde_json::json!({
                "error": { "type": "uxduck", "code": code, "message": message }
            });
            let error = explain(429, None, &body.to_string());
            assert_eq!(error, ProviderError::Account(message.into()));
            assert_eq!(error.to_string(), message);
            assert!(!error.retryable(), "{code} cannot be fixed by retrying");
        }
    }

    #[test]
    fn temporary_hosted_limits_and_provider_overload_still_retry() {
        for code in ["rate-limited", "in-flight", "provider"] {
            let body = serde_json::json!({
                "error": { "type": "uxduck", "code": code, "message": "Try again shortly." }
            });
            assert!(explain(429, Some(Duration::from_secs(1)), &body.to_string()).retryable());
        }
        assert!(
            explain(
                429,
                None,
                r#"{"error":{"type":"rate_limit_error","message":"Busy"}}"#
            )
            .retryable()
        );
    }
}

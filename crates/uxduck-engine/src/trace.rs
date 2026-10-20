//! The inspector tap: builds a `TraceFrame` for every frame in and out, with
//! sensitive values redacted before they leave the engine (ADR-0001).

use serde_json::{Map, Value};
use uxduck_contracts::dto::trace::{FrameKind, RequestId};

/// Object keys whose string values are never shown by the tap.
const SENSITIVE_KEYS: [&str; 10] = [
    "token",
    "bootstrap",
    "session",
    "password",
    "secret",
    "apiKey",
    "authorization",
    "cookie",
    "code",
    "privateKey",
];

pub const REDACTED: &str = "[redacted]";

pub(crate) fn private_method(method: Option<&str>) -> bool {
    matches!(method, Some("place/search" | "place/locate"))
}

pub(crate) fn hide_place_reply(payload: &mut Value) {
    if let Some(result) = payload.get_mut("result") {
        *result = Value::String(REDACTED.into());
    }
    if let Some(message) = payload.get_mut("error").and_then(|e| e.get_mut("message")) {
        *message = Value::String(REDACTED.into());
    }
}

/// Longest raw text kept from a frame that is not JSON.
const MAX_INVALID_CHARS: usize = 256;

/// Replaces the string values of sensitive keys, at any depth. Numbers are
/// kept, so a JSON-RPC error `code` stays visible.
#[must_use]
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let sensitive = SENSITIVE_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key));
                    let value = if sensitive && value.is_string() {
                        Value::String(REDACTED.into())
                    } else {
                        redact(value)
                    };
                    (key.clone(), value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// What the tap reads from a frame before it is dispatched.
#[derive(Debug)]
pub struct Parsed {
    pub kind: FrameKind,
    pub method: Option<String>,
    pub id: Option<RequestId>,
    pub payload: Value,
}

#[must_use]
pub fn parse(text: &str) -> Parsed {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Parsed {
            kind: FrameKind::Invalid,
            method: None,
            id: None,
            payload: Value::String(text.chars().take(MAX_INVALID_CHARS).collect()),
        };
    };
    classify(&value)
}

#[must_use]
pub fn classify(value: &Value) -> Parsed {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = value
        .get("id")
        .and_then(|id| serde_json::from_value::<RequestId>(id.clone()).ok());
    let kind = if !value.is_object() {
        FrameKind::Invalid
    } else if method.is_some() {
        if value.get("id").is_some() {
            FrameKind::Request
        } else {
            FrameKind::Notification
        }
    } else if value.get("error").is_some() {
        FrameKind::Error
    } else if value.get("result").is_some() {
        FrameKind::Response
    } else {
        FrameKind::Invalid
    };
    let mut payload = redact(value);
    if private_method(method.as_deref()) {
        payload["params"] = Value::String(REDACTED.into());
    }
    Parsed {
        kind,
        method,
        id,
        payload,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redacts_sensitive_strings_at_any_depth_but_keeps_error_codes() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": { "token": "abc", "uiUrl": "http://127.0.0.1:45000/", "nested": [{ "Password": "x" }] },
            "error": { "code": -32601, "message": "nope" },
            "params": { "code": "123456" }
        });
        let redacted = redact(&frame);
        assert_eq!(redacted["result"]["token"], REDACTED);
        assert_eq!(redacted["result"]["nested"][0]["Password"], REDACTED);
        assert_eq!(redacted["result"]["uiUrl"], "http://127.0.0.1:45000/");
        assert_eq!(redacted["error"]["code"], -32601);
        assert_eq!(redacted["params"]["code"], REDACTED);
    }

    #[test]
    fn classifies_frames() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).kind,
            FrameKind::Request
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"cancel","params":{"id":1}}"#).kind,
            FrameKind::Notification
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).kind,
            FrameKind::Response
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"error":{}}"#).kind,
            FrameKind::Error
        );
        assert_eq!(parse("not json").kind, FrameKind::Invalid);
        assert_eq!(parse("[1,2]").kind, FrameKind::Invalid);
    }

    #[test]
    fn keeps_a_prefix_of_invalid_frames() {
        let long = "x".repeat(1000);
        let parsed = parse(&long);
        assert_eq!(parsed.payload.as_str().map(str::len), Some(256));
    }
}

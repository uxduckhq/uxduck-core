//! Parses a frame, finds the method, checks admission and calls a handler.

use serde::Serialize;
use serde_json::{Value, json};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientNotification, ClientRequest};

use crate::{ConnectionId, Engine};

/// Handles one frame and returns the reply, if it needs one.
pub(crate) fn dispatch(engine: &mut Engine, connection: ConnectionId, text: &str) -> Option<Value> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Some(error(
            &Value::Null,
            &ApiError::new(ErrorKind::ParseError, "invalid JSON"),
        ));
    };
    let Some(object) = value.as_object() else {
        return Some(error(
            &Value::Null,
            &ApiError::new(ErrorKind::InvalidRequest, "expected a JSON object"),
        ));
    };
    let id = object.get("id").cloned();
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error(
            id.as_ref().unwrap_or(&Value::Null),
            &ApiError::new(ErrorKind::InvalidRequest, "jsonrpc must be \"2.0\""),
        ));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A reply to a server request; there are none yet, so it is dropped.
        return None;
    };
    let params = match object.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(params) => params.clone(),
    };

    let Some(id) = id else {
        // Notifications are never answered, even when they are wrong.
        if let Ok(notification) = serde_json::from_value::<ClientNotification>(
            json!({ "method": method, "params": params }),
        ) {
            super::handlers::notify(engine, connection, &notification);
        }
        return None;
    };
    if !(id.is_string() || id.is_i64() || id.is_u64()) {
        return Some(error(
            &Value::Null,
            &ApiError::new(
                ErrorKind::InvalidRequest,
                "id must be a string or an integer",
            ),
        ));
    }
    if !ClientRequest::METHODS.contains(&method) {
        return Some(error(
            &id,
            &ApiError::new(
                ErrorKind::MethodNotFound,
                format!("unknown method {method}"),
            ),
        ));
    }
    let request = match serde_json::from_value::<ClientRequest>(
        json!({ "method": method, "params": params }),
    ) {
        Ok(request) => request,
        Err(reason) => {
            return Some(error(
                &id,
                &ApiError::new(ErrorKind::InvalidParams, reason.to_string()),
            ));
        }
    };
    engine.replying_to = Some((connection, id.clone()));
    let result = super::handlers::handle(engine, connection, request);
    // A handler that answers later (`command/send`) took the reply slot.
    engine.replying_to.take()?;
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(api_error) => error(&id, &api_error),
    })
}

#[must_use]
pub fn error(id: &Value, error: &ApiError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// A notification frame: the enum's `method` and `params` plus `jsonrpc`.
pub fn notification<T: Serialize>(notification: &T) -> Value {
    let mut value = serde_json::to_value(notification).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("jsonrpc".into(), json!("2.0"));
    }
    value
}

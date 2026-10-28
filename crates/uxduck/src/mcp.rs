//! `uxduck mcp`: a Model Context Protocol server on stdio for coding agents
//! such as Claude Code or Codex (ADR-0005, ADR-0009). Its tools are the
//! agent tools generated from the contracts; each call is an ordinary
//! request on a daemon connection of kind `agent`, under the MCP client's
//! own name, so the daemon's capabilities and approvals apply to it exactly
//! as to the in-app agent: reading needs nothing, control waits for an
//! operator's answer.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uxduck_agent::tools::{Tool, tools};

use crate::cli::client::{CliError, LocalClient};

mod catalogue;

/// The MCP revision this server speaks.
pub const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "UXDuck vehicle tools. Before advice, read vehicle_history and cite dated entries; read param_get and report describedBy; never guess undescribed parameters. Named simulation starts: place_search, place_locate, terrain_at, then sim_start; ask about ambiguous places. Reads are allowed; commands/configuration need capabilities and approvals. Give reasons for parameter writes; history records them. For reported physical work, offer one concise note and ask before vehicle_add_note unless asked to record it. Record completed work only; attribute reports, retain dates, read history to avoid duplicates, never edit/delete notes. Config permission is not note consent. After your changes, offer notes only for useful context absent from automatic history; never interrupt flying or unfinished work, or repeat declined offers. For telemetry_window report coverage, missing signals and historyTruncated; for packet/routing/log evidence report coverage and missing provenance. Empty or incomplete results do not prove nothing happened. vehicle_messages is a snapshot; poll for newer messages.";

fn reply(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Serves MCP on stdin and stdout until stdin closes.
///
/// # Errors
/// When stdin or stdout fail.
pub async fn run(data_dir: &Path, all_tools: bool) -> Result<(), CliError> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let catalogue = tools();
    let mut session = Session::default();
    loop {
        let answer = tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                session.dispatch(data_dir, &catalogue, all_tools, message).await?
            }
            frame = daemon_frame(&mut session.daemon) => {
                let frame = frame?.ok_or_else(|| CliError::Protocol("the daemon closed the connection".into()))?;
                frame["id"].as_i64().and_then(|id| session.pending.remove(&id)).map(|id| {
                    let result = frame.get("error").map_or_else(
                        || tool_result(frame.get("result").cloned().unwrap_or(Value::Null)),
                        api_error,
                    );
                    reply(&id, &result)
                })
            }
        };
        if let Some(answer) = answer {
            let mut text = answer.to_string();
            text.push('\n');
            stdout.write_all(text.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn daemon_frame(daemon: &mut Option<LocalClient>) -> Result<Option<Value>, CliError> {
    match daemon {
        Some(client) => client.read().await,
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct Session {
    daemon: Option<LocalClient>,
    pending: BTreeMap<i64, Value>,
}

impl Session {
    async fn dispatch(
        &mut self,
        data_dir: &Path,
        catalogue: &[Tool],
        all_tools: bool,
        message: Value,
    ) -> Result<Option<Value>, CliError> {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            if message["method"] == "notifications/cancelled"
                && let Some(request_id) = params.get("requestId")
                && let Some(daemon_id) = self
                    .pending
                    .iter()
                    .find_map(|(daemon_id, pending)| (pending == request_id).then_some(*daemon_id))
                && let Some(client) = self.daemon.as_mut()
            {
                client.cancel_request(daemon_id).await?;
                self.pending.remove(&daemon_id);
            }
            return Ok(None);
        };
        if !(id.is_string() || id.is_i64()) || self.pending.values().any(|pending| pending == &id) {
            return Ok(Some(failure(
                &id,
                -32600,
                "request id must be a unique string or integer",
            )));
        }
        let answer = match message["method"].as_str().unwrap_or_default() {
            "initialize" if self.daemon.is_some() => failure(&id, -32600, "already initialized"),
            "initialize" => {
                let name = params["clientInfo"]["name"]
                    .as_str()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or("MCP client")
                    .to_owned();
                match connect(data_dir, &name).await {
                    Ok(client) => {
                        self.daemon = Some(client);
                        reply(
                            &id,
                            &json!({
                                "protocolVersion": PROTOCOL,
                                "capabilities": { "tools": { "listChanged": false } },
                                "serverInfo": { "name": "uxduck", "version": env!("CARGO_PKG_VERSION") },
                                "instructions": if all_tools {
                                    INSTRUCTIONS.to_owned()
                                } else {
                                    format!("Find tool names and input schemas with uxduck_search_tools, then run them with uxduck_call_tool. Search exact names to fetch just one schema; reuse it instead of searching again. Use limits and filters for large results. {INSTRUCTIONS}")
                                }
                            }),
                        )
                    }
                    Err(error) => failure(&id, -32002, &error.to_string()),
                }
            }
            "ping" => reply(&id, &json!({})),
            "tools/list" => reply(&id, &catalogue::list(catalogue, all_tools)),
            "tools/call" => return self.invoke(catalogue, id, params).await,
            other => failure(&id, -32601, &format!("unknown method {other}")),
        };
        Ok(Some(answer))
    }

    async fn invoke(
        &mut self,
        catalogue: &[Tool],
        id: Value,
        params: Value,
    ) -> Result<Option<Value>, CliError> {
        let Some(client) = self.daemon.as_mut() else {
            return Ok(Some(failure(&id, -32002, "call initialize first")));
        };
        let Some(name) = params["name"].as_str() else {
            return Ok(Some(failure(&id, -32602, "tools/call needs a name")));
        };
        if name == catalogue::SEARCH {
            let result = catalogue::search(catalogue, arguments(&params))
                .map_or_else(|error| invalid_arguments(&error), tool_result);
            return Ok(Some(reply(&id, &result)));
        }
        let (method, arguments) = if name == catalogue::CALL {
            match catalogue::invocation(catalogue, arguments(&params)) {
                Ok(invocation) => invocation,
                Err(error) => return Ok(Some(reply(&id, &invalid_arguments(&error)))),
            }
        } else {
            let Some(method) = Tool::method_for(catalogue, name) else {
                return Ok(Some(failure(&id, -32602, &format!("no tool named {name}"))));
            };
            (method, arguments(&params))
        };
        if let Some(error) = tool_arguments_error(&method, &arguments) {
            return Ok(Some(reply(&id, &error)));
        }
        if self.pending.len() >= 64 {
            return Ok(Some(failure(
                &id,
                -32000,
                "too many pending tool calls; wait or cancel one",
            )));
        }
        let daemon_id = client.send_request(&method, arguments).await?;
        self.pending.insert(daemon_id, id);
        Ok(None)
    }
}

fn arguments(params: &Value) -> Value {
    params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn invalid_arguments(message: &str) -> Value {
    api_error(&json!({ "code": -32602, "message": message }))
}

/// MCP clients may consume either representation. Compact JSON saves tokens
/// in text-only clients without dropping fields, schemas or coverage evidence.
fn tool_result(result: Value) -> Value {
    let mut content = json!({
        "content": [{ "type": "text", "text": result.to_string() }],
        "isError": false
    });
    if result.is_object() {
        content["structuredContent"] = result;
    }
    content
}

// An agent connecting must never start UXDuck or its vehicle links implicitly.
async fn connect(data_dir: &Path, name: &str) -> Result<LocalClient, CliError> {
    let mut client = LocalClient::connect(data_dir).await.map_err(|error| match error {
        CliError::NoDaemon(directory) => CliError::Usage(format!(
            "UXDuck is not running for {directory}. Start UXDuck yourself with `uxduck daemon start`, using the same --dev or --data-dir options, then reconnect MCP."
        )),
        error => error,
    })?;
    client.initialize_as(name, "agent", false).await?;
    Ok(client)
}

fn tool_arguments_error(method: &str, arguments: &Value) -> Option<Value> {
    if method == "vehicle/messages" && arguments["subscribe"] == true {
        return Some(api_error(&json!({
            "code": -32602,
            "message": "MCP returns a bounded vehicle message snapshot; call vehicle_messages again for newer messages. subscribe:true is not supported.",
            "data": { "kind": "invalid-value", "field": "subscribe" }
        })));
    }
    None
}

/// Keep the complete daemon error in both representations: clients that
/// consume only text still see its code and details, and others can inspect
/// the original object without parsing the explanation.
fn api_error(error: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": error.to_string() }],
        "structuredContent": { "error": error },
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn startup_instructions_and_text_results_stay_compact_without_losing_data() {
        assert!(INSTRUCTIONS.len() < 1_200);
        let value = json!({ "coverage": { "truncated": true, "notes": ["missing packets"] }, "position": { "lat": 51.5, "lon": -0.1 }, "message": "line one\nline two" });
        let result = tool_result(value.clone());
        assert_eq!(result["structuredContent"], value);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, value.to_string());
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), value);
    }

    #[test]
    fn api_errors_keep_their_codes_and_details_for_every_mcp_client() {
        let error = json!({
            "code": -32004,
            "message": "No vehicle with this id",
            "data": { "kind": "not-found", "id": "missing", "retryable": false }
        });
        let result = api_error(&error);
        assert_eq!(result["isError"], true);
        assert_eq!(result["structuredContent"]["error"], error);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), error);
    }
}

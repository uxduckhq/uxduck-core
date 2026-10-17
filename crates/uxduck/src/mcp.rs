//! `uxduck mcp`: a Model Context Protocol server on stdio for coding agents
//! such as Claude Code or Codex (ADR-0005, ADR-0009). Its tools are the
//! agent tools generated from the contracts; each call is an ordinary
//! request on a daemon connection of kind `agent`, under the MCP client's
//! own name, so the daemon's capabilities and approvals apply to it exactly
//! as to the in-app agent: reading needs nothing, control waits for an
//! operator's answer.

use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uxduck_agent::tools::{Tool, tools};

use crate::cli::client::{CliError, LocalClient};

mod catalogue;

/// The MCP revision this server speaks.
pub const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "UXDuck vehicle tools. Read a vehicle's vehicle_history before diagnosis or advice; cite dated entries. Read param_get before parameter advice and report describedBy; do not guess undescribed parameters. Reading is allowed; commands and configuration still require UXDuck's capabilities and operator approvals. Include reasons in parameter writes, which are recorded automatically. For reported physical work, offer one concise history note; ask before vehicle_add_note unless explicitly asked to record it. Record only completed work, attribute user reports, preserve dates, read history to avoid duplicates, and never edit or delete notes. Configuration permission is not consent to add notes. Offer notes after your changes only for useful context missing from automatic history; do not interrupt active flying or unfinished work, or repeat declined offers. For telemetry_window report coverage, missing signals and historyTruncated; for packet, routing and log evidence report coverage and missing provenance. Empty or incomplete results do not prove nothing happened. vehicle_messages is a snapshot; poll again for newer messages.";

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
    let mut daemon: Option<LocalClient> = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications (no id) need no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let answer = match message["method"].as_str().unwrap_or_default() {
            "initialize" => {
                let name = params["clientInfo"]["name"]
                    .as_str()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or("MCP client")
                    .to_owned();
                match connect(data_dir, &name).await {
                    Ok(client) => {
                        daemon = Some(client);
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
            "tools/list" => reply(&id, &catalogue::list(&catalogue, all_tools)),
            "tools/call" => match (daemon.as_mut(), params["name"].as_str()) {
                (None, _) => failure(&id, -32002, "call initialize first"),
                (_, None) => failure(&id, -32602, "tools/call needs a name"),
                (Some(_), Some(catalogue::SEARCH)) => {
                    let result = catalogue::search(&catalogue, arguments(&params))
                        .map_or_else(|error| invalid_arguments(&error), tool_result);
                    reply(&id, &result)
                }
                (Some(client), Some(catalogue::CALL)) => {
                    let result = match catalogue::invocation(&catalogue, arguments(&params)) {
                        Ok((method, arguments)) => call(client, &method, arguments).await,
                        Err(error) => invalid_arguments(&error),
                    };
                    reply(&id, &result)
                }
                // Keep direct names callable for existing clients and scripts.
                (Some(client), Some(name)) => match Tool::method_for(&catalogue, name) {
                    None => failure(&id, -32602, &format!("no tool named {name}")),
                    Some(method) => reply(&id, &call(client, &method, arguments(&params)).await),
                },
            },
            other => failure(&id, -32601, &format!("unknown method {other}")),
        };
        let mut text = answer.to_string();
        text.push('\n');
        stdout.write_all(text.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
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

/// One tool call as a daemon request; an API error is a tool error the
/// model can read and explain, not a protocol failure.
async fn call(client: &mut LocalClient, method: &str, arguments: Value) -> Value {
    if method == "vehicle/messages" && arguments["subscribe"] == true {
        return api_error(&json!({
            "code": -32602,
            "message": "MCP returns a bounded vehicle message snapshot; call vehicle_messages again for newer messages. subscribe:true is not supported.",
            "data": { "kind": "invalid-value", "field": "subscribe" }
        }));
    }
    match client.call(method, arguments).await {
        Ok(result) => tool_result(result),
        Err(CliError::Api(error)) => api_error(&error),
        Err(error) => json!({
            "content": [{ "type": "text", "text": error.to_string() }],
            "isError": true
        }),
    }
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

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

/// The MCP revision this server speaks.
pub const PROTOCOL: &str = "2025-06-18";

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
pub async fn run(data_dir: &Path, dev: bool) -> Result<(), CliError> {
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
                match connect(data_dir, dev, &name).await {
                    Ok(client) => {
                        daemon = Some(client);
                        reply(
                            &id,
                            &json!({
                                "protocolVersion": PROTOCOL,
                                "capabilities": { "tools": { "listChanged": false } },
                                "serverInfo": { "name": "uxduck", "version": env!("CARGO_PKG_VERSION") },
                                "instructions": "Tools for the vehicles this UXDuck daemon hears. Reading is always allowed; commands and parameter changes wait until an operator allows them in UXDuck. Read a parameter with param_get before advising on it, and say which set its description is from (describedBy). Read a vehicle's history (vehicle_history) before diagnosing it, and write a note (vehicle_add_note) only when asked. For telemetry over time, use telemetry_window and report coverage, missing signals and historyTruncated; mavlink_inspect provides current message rates; mavlink_history with linkId and fromUs/toUs provides exact packet evidence, including sequence and fingerprints. Use router_events for routing/drop decisions and preserved loop incidents, link_diagnose to resolve connection IDs or names, socket_inspect for local process ownership, and daemon_logs for bounded redacted logs. Report coverage and missing provenance; an empty or incomplete result is not proof that nothing happened. vehicle_messages returns a snapshot, not a subscription. link_list and daemon_about diagnose the connection and build. For camera video, video_list says what is arriving and why it plays badly. find_vehicles looks for vehicles nearby and says why anything found is not one."
                            }),
                        )
                    }
                    Err(error) => failure(&id, -32002, &error.to_string()),
                }
            }
            "ping" => reply(&id, &json!({})),
            "tools/list" => reply(
                &id,
                &json!({
                    "tools": catalogue.iter().map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.parameters,
                    })).collect::<Vec<_>>()
                }),
            ),
            "tools/call" => match (daemon.as_mut(), params["name"].as_str()) {
                (None, _) => failure(&id, -32002, "call initialize first"),
                (_, None) => failure(&id, -32602, "tools/call needs a name"),
                (Some(client), Some(name)) => match Tool::method_for(&catalogue, name) {
                    None => failure(&id, -32602, &format!("no tool named {name}")),
                    Some(method) => {
                        let arguments = match params.get("arguments") {
                            Some(Value::Object(object)) => Value::Object(object.clone()),
                            _ => json!({}),
                        };
                        reply(&id, &call(client, &method, arguments).await)
                    }
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

async fn connect(data_dir: &Path, dev: bool, name: &str) -> Result<LocalClient, CliError> {
    let mut client = match LocalClient::connect(data_dir).await {
        Ok(client) => client,
        Err(CliError::NoDaemon(_)) => crate::cli::start_daemon(data_dir, dev).await?,
        Err(error) => return Err(error),
    };
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
        Ok(result) => {
            let text = serde_json::to_string_pretty(&result).unwrap_or_default();
            let mut content =
                json!({ "content": [{ "type": "text", "text": text }], "isError": false });
            if result.is_object() {
                content["structuredContent"] = result;
            }
            content
        }
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
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(error).unwrap_or_else(|_| error.to_string()) }],
        "structuredContent": { "error": error },
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

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

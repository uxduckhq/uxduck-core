//! Small, stable MCP entry points; vehicle tool schemas still come from contracts.

use serde::Deserialize;
use serde_json::{Value, json};
use uxduck_agent::tools::Tool;

mod search;

pub const SEARCH: &str = "uxduck_search_tools";
pub const CALL: &str = "uxduck_call_tool";

pub fn definition(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.parameters,
        "outputSchema": uxduck_agent::tools::mcp::output_schema(&tool.method),
        "annotations": uxduck_agent::tools::mcp::annotations(&tool.method),
    })
}

pub fn list(catalogue: &[Tool], all_tools: bool) -> Value {
    if all_tools {
        return json!({ "tools": catalogue.iter().map(definition).collect::<Vec<_>>() });
    }
    json!({ "tools": [
        {
            "name": SEARCH,
            "description": "Find UXDuck tools by name or words: vehicles, telemetry, messages, history, parameters, missions and plans, commands, simulations, places and addresses, links, terrain, files, video and plugins. Word searches return short candidates without schemas. Search an exact returned name for its full input/output schemas before calling it with uxduck_call_tool. No vehicle action.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "maxLength": 256 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5, "default": 3 },
                    "offset": { "type": "integer", "minimum": 0, "default": 0 }
                },
                "required": ["query"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": CALL,
            "description": "Run a UXDuck tool by the name and arguments from uxduck_search_tools. Uses the same vehicle permissions and operator approvals as a direct call. Only UXDuck's advertised vehicle tools are allowed; arbitrary daemon RPC is unavailable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "arguments": { "type": "object", "description": "Arguments matching the discovered inputSchema; omit for a tool with no parameters." }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }
    ] })
}

/// Search pages stay small even when a tool has a large settings schema.
fn summary(tool: &Tool) -> Value {
    let purpose = tool.description.split("\n\n").next().unwrap_or_default();
    let mut description = purpose.split_whitespace().collect::<Vec<_>>().join(" ");
    if description.len() > 360 {
        let boundary = description
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= 357)
            .last()
            .unwrap_or(0);
        description.truncate(boundary);
        description.push('…');
    }
    json!({ "name": tool.name, "description": description,
        "schemaQuery": tool.name, "annotations": uxduck_agent::tools::mcp::annotations(&tool.method) })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

const fn default_limit() -> usize {
    3
}

/// Local discovery only: never calls the daemon or broadens its allowlist.
pub fn search(catalogue: &[Tool], arguments: Value) -> Result<Value, String> {
    let search: Search = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
    let query = search.query.trim().to_lowercase();
    if query.is_empty() || search.query.chars().count() > 256 {
        return Err("query must contain 1–256 characters and cannot be blank".into());
    }
    let limit = search.limit;
    if !(1..=5).contains(&limit) {
        return Err("limit must be between 1 and 5".into());
    }
    let exact = catalogue.iter().find(|tool| tool.name == query);
    let identifier =
        query.contains('_') && query.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let matches: Vec<&Tool> = if let Some(tool) = exact {
        vec![tool]
    } else if identifier {
        catalogue
            .iter()
            .filter(|tool| tool.name.starts_with(&query))
            .collect()
    } else {
        search::ranked(catalogue, &query)
    };
    let total = matches.len();
    let mut tools = Vec::new();
    // Leave room for the page envelope. Full contracts are returned only
    // when explicitly requested by exact name, and are never truncated.
    let mut bytes = 256;
    for tool in matches.into_iter().skip(search.offset).take(limit) {
        let result = if exact.is_some() {
            definition(tool)
        } else {
            summary(tool)
        };
        bytes += result.to_string().len() + 1;
        if exact.is_none() && bytes > 4_000 {
            break;
        }
        tools.push(result);
    }
    let end = search.offset.saturating_add(tools.len());
    Ok(json!({
        "tools": tools,
        "total": total,
        "nextOffset": (end < total).then_some(end),
        "detail": if exact.is_some() { "schema" } else { "summary" },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    name: String,
    #[serde(default)]
    arguments: serde_json::Map<String, Value>,
}

pub fn invocation(catalogue: &[Tool], arguments: Value) -> Result<(String, Value), String> {
    let invocation: Invocation = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
    let method = Tool::method_for(catalogue, &invocation.name)
        .ok_or_else(|| format!("no tool named {}; find it with {SEARCH}", invocation.name))?;
    Ok((method, Value::Object(invocation.arguments)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;
    use uxduck_agent::tools::tools;

    #[test]
    fn diagnostic_questions_find_the_right_operation_before_configuration() {
        let catalogue = tools();
        for (query, expected) in [
            ("GPS fix quality", "telemetry_get"),
            ("gps", "telemetry_get"),
            ("read battery voltage", "telemetry_get"),
            ("battery voltage last minute", "telemetry_window"),
            ("change flight mode", "command_send"),
            ("arm vehicle", "command_send"),
            ("why will my vehicle not arm", "preflight_state"),
            ("prearm", "preflight_state"),
            ("inspect connection", "link_diagnose"),
            ("flight logs", "flight_list"),
            ("download simulator", "sim_download"),
        ] {
            let found = search(&catalogue, json!({"query":query})).unwrap();
            let names: Vec<_> = found["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert_eq!(names[0], expected, "{query}: {names:?}");
            assert_eq!(found["detail"], "summary");
        }
    }

    #[test]
    fn broad_searches_are_bounded_and_exact_searches_keep_complete_contracts() {
        let catalogue = tools();
        for query in [
            "change flight mode",
            "battery voltage last minute",
            "settings",
            "plugin",
            "vehicle",
        ] {
            let found = search(&catalogue, json!({"query":query,"limit":5})).unwrap();
            assert!(
                found.to_string().len() <= 4_000,
                "{query}: {} bytes",
                found.to_string().len()
            );
            for candidate in found["tools"].as_array().unwrap() {
                assert!(candidate.get("inputSchema").is_none());
                let exact = search(&catalogue, json!({"query":candidate["schemaQuery"]})).unwrap();
                assert_eq!(exact["detail"], "schema");
                assert!(exact["tools"][0]["inputSchema"].is_object());
                assert!(exact["tools"][0]["outputSchema"].is_object());
            }
        }
        let exact = search(&catalogue, json!({"query":"operator_update"})).unwrap();
        let original = catalogue
            .iter()
            .find(|tool| tool.name == "operator_update")
            .unwrap();
        assert_eq!(exact["tools"][0]["inputSchema"], original.parameters);
    }

    #[test]
    fn telemetry_window_advertises_the_enforced_numeric_and_selector_limits() {
        let catalogue = tools();
        let result = search(&catalogue, json!({"query":"telemetry_window"})).unwrap();
        let params = &result["tools"][0]["inputSchema"]["properties"];
        for (field, min, max) in [("durationMs", 1, 300_000), ("maxSamples", 1, 3000)] {
            assert_eq!(params[field]["minimum"], min);
            assert_eq!(params[field]["maximum"], max);
        }
        assert!(
            params["durationMs"].get("format").is_none(),
            "duration stays a JSON number in generated clients"
        );
        assert_eq!(params["keys"]["maxItems"], 32);
        assert_eq!(params["positionSources"]["maxItems"], 8);
    }

    #[test]
    fn default_catalogue_has_a_fixed_budget_and_every_tool_remains_discoverable() {
        let catalogue = tools();
        let compact = list(&catalogue, false);
        assert_eq!(compact["tools"].as_array().unwrap().len(), 2);
        let bytes = compact.to_string().len();
        assert!(bytes < 2_000, "{bytes} bytes in the startup catalogue");
        assert!(bytes * 20 < list(&catalogue, true).to_string().len());
        for tool in &catalogue {
            let found = search(&catalogue, json!({ "query": tool.name })).unwrap();
            assert_eq!(found["total"], 1, "{}", tool.name);
            assert_eq!(found["tools"][0], definition(tool));
            assert_eq!(
                invocation(&catalogue, json!({ "name": tool.name })).unwrap(),
                (tool.method.clone(), json!({}))
            );
        }
    }

    #[test]
    fn discovery_is_ranked_bounded_paged_and_explicit_when_empty() {
        let catalogue = tools();
        let found = search(
            &catalogue,
            json!({ "query": "mission history", "limit": 1 }),
        )
        .unwrap();
        assert_eq!(found["tools"][0]["name"], "mission_history");
        assert!(found["total"].as_u64().unwrap() > 1);
        assert_eq!(found["nextOffset"], 1);
        let next = search(
            &catalogue,
            json!({ "query": "mission history", "limit": 1, "offset": 1 }),
        )
        .unwrap();
        assert_ne!(next["tools"][0], found["tools"][0]);
        let arm = search(&catalogue, json!({ "query": "arm vehicle" })).unwrap();
        assert!(
            arm["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "command_send"),
            "{arm}\ncommand_send={}",
            definition(
                catalogue
                    .iter()
                    .find(|tool| tool.name == "command_send")
                    .unwrap()
            )
        );
        let survey = search(&catalogue, json!({ "query": "survey" })).unwrap();
        assert!(
            survey["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "plan_create")
        );
        let empty = search(&catalogue, json!({ "query": "nonexistentxyz" })).unwrap();
        assert_eq!(empty["tools"], json!([]));
        assert_eq!(empty["total"], 0);
        assert!(empty["nextOffset"].is_null());
        let missing_name = search(&catalogue, json!({ "query": "does_not_exist_uxduck" })).unwrap();
        assert_eq!(missing_name["total"], 0);
        for arguments in [
            json!({}),
            json!({ "query": " " }),
            json!({ "query": "x".repeat(257) }),
            json!({ "query": "vehicle", "limit": 0 }),
            json!({ "query": "vehicle", "limit": 6 }),
            json!({ "query": "vehicle", "limit": null }),
            json!({ "query": "vehicle", "offset": -1 }),
        ] {
            assert!(search(&catalogue, arguments).is_err());
        }
    }

    /// A camera's controls are found by asking for them (S405): the tool is
    /// not one of those sent every turn.
    #[test]
    fn a_cameras_command_is_found_by_what_it_does() {
        let catalogue = tools();
        let found = search(&catalogue, json!({ "query": "camera take photo" })).unwrap();
        assert!(
            found["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "camera_command"),
            "{found}"
        );
    }

    /// And a gimbal's (S406): pointing it and reading who has it.
    #[test]
    fn a_gimbals_command_is_found_by_what_it_does() {
        let catalogue = tools();
        for (query, name) in [
            ("point the gimbal", "gimbal_command"),
            ("who controls the gimbal", "gimbal_list"),
        ] {
            let found = search(&catalogue, json!({ "query": query })).unwrap();
            assert!(
                found["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == name),
                "{query}: {found}"
            );
        }
    }

    #[test]
    fn calls_cannot_escape_the_vehicle_tool_allowlist_or_hide_invalid_arguments() {
        let catalogue = tools();
        for arguments in [
            json!({ "name": "daemon_stop" }),
            json!({ "name": "vehicle/list" }),
            json!({ "name": SEARCH }),
            json!({ "name": CALL }),
            json!({ "name": "vehicle_list", "arguments": null }),
            json!({ "name": "vehicle_list", "arguments": [] }),
            json!({ "name": "vehicle_list", "method": "daemon/stop" }),
        ] {
            assert!(invocation(&catalogue, arguments).is_err());
        }
    }

    #[test]
    fn common_recovery_workflows_find_their_specific_tools() {
        let catalogue = tools();
        for (query, expected) in [
            ("flight logs", "flight_list"),
            ("download simulator", "sim_download"),
            ("param_refresh", "param_refresh"),
            ("sim_catalogue", "sim_catalogue"),
            ("tuning gains oscillating", "tuning_state"),
        ] {
            let result = search(&catalogue, json!({"query":query})).unwrap();
            assert_eq!(result["tools"][0]["name"], expected, "{query}: {result}");
        }
        // Tuning's samples are the tab's; its summaries are in tuning_state (S333).
        let graph = search(&catalogue, json!({ "query": "tuning_response" })).unwrap();
        assert_eq!(graph["total"], 0, "{graph}");
        assert_eq!(
            invocation(
                &catalogue,
                json!({ "name": "tuning_state", "arguments": { "vehicleId": "v1" } })
            )
            .unwrap(),
            ("tuning/state".to_owned(), json!({ "vehicleId": "v1" }))
        );
    }
}

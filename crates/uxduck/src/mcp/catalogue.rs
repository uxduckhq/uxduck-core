//! Small, stable MCP entry points; vehicle tool schemas still come from contracts.

use serde::Deserialize;
use serde_json::{Value, json};
use uxduck_agent::tools::Tool;

pub const SEARCH: &str = "uxduck_search_tools";
pub const CALL: &str = "uxduck_call_tool";

pub fn definition(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.parameters,
    })
}

pub fn list(catalogue: &[Tool], all_tools: bool) -> Value {
    if all_tools {
        return json!({ "tools": catalogue.iter().map(definition).collect::<Vec<_>>() });
    }
    json!({ "tools": [
        {
            "name": SEARCH,
            "description": "Find UXDuck tools by name or words: vehicles, telemetry, messages, history, parameters, missions and plans, commands, simulations, places and addresses, links, terrain, files, video and plugins. Returns matching names and full input schemas; use uxduck_call_tool to run one. Search an exact name for one schema. No vehicle action.",
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
    let words: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .collect();
    let exact = catalogue.iter().find(|tool| tool.name == query);
    // An unknown tool identifier must not expand into a broad word search:
    // `does_not_exist_uxduck` would otherwise match incidental schema prose.
    let identifier =
        query.contains('_') && query.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut matches: Vec<(usize, &Tool)> = if let Some(tool) = exact {
        vec![(1, tool)]
    } else {
        catalogue
            .iter()
            .filter_map(|tool| {
                if identifier {
                    return tool.name.starts_with(&query).then_some((1, tool));
                }
                let description = tool.description.to_lowercase();
                // Action names such as arm and pattern names such as survey
                // often occur only in the generated argument schema.
                let schema = tool.parameters.to_string().to_lowercase();
                let score: usize = words
                    .iter()
                    .map(|word| {
                        let name = tool.name.contains(*word);
                        let about = description.contains(*word);
                        let argument = schema.contains(*word);
                        // Prefer matching more query words over mentioning a
                        // single word in a high-weight field.
                        100 * usize::from(name || about || argument)
                            + 3 * usize::from(name)
                            + usize::from(about)
                            + usize::from(argument)
                    })
                    .sum();
                (score > 0).then_some((score, tool))
            })
            .collect()
    };
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let total = matches.len();
    let tools: Vec<_> = matches
        .into_iter()
        .skip(search.offset)
        .take(limit)
        .map(|(_, tool)| definition(tool))
        .collect();
    let end = search.offset.saturating_add(tools.len());
    Ok(json!({
        "tools": tools,
        "total": total,
        "nextOffset": (end < total).then_some(end),
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
                .any(|t| t["name"] == "command_send")
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
}

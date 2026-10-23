//! Tools, generated from the contracts (ADR-0009: "no hand-written tool
//! list"). Each tool is a `ClientRequest` variant: its description is the
//! variant's doc comment and its parameters are the variant's params schema.
//! Only the methods in [`AGENT_METHODS`] are offered.

use schemars::generate::SchemaSettings;
use serde_json::{Value, json};
use uxduck_contracts::rpc::ClientRequest;

/// The API methods the agent may call as tools. Adding one here is the
/// whole change; what it may do is still decided by its capabilities.
pub const AGENT_METHODS: &[&str] = &[
    "vehicle/list",
    "vehicle/get",
    "telemetry/get",
    // Read-only diagnostics and recorded MAVLink evidence.
    "daemon/about",
    "mavlink/inspect",
    "mavlink/history",
    "router/events",
    "link/diagnose",
    "socket/inspect",
    "daemon/logs",
    "telemetry/window",
    "link/list",
    "command/list",
    "command/send",
    "search/query",
    "place/search",
    "place/locate",
    // Where this computer is, once a person has turned it on (S137,
    // ADR-0030 L4); the agent cannot turn it on.
    "location/status",
    // Saved plans (ADR-0029): drafting and changing one touches no vehicle;
    // making or changing one asks first unless the agent has `config`.
    // Checking one is free; uploading one, or putting back a mission from a
    // vehicle's history, asks the operator first.
    "plan/list",
    "plan/get",
    "plan/create",
    "plan/update",
    "plan/check",
    "plan/examples",
    "plan/import",
    "plan/export",
    "plan/upload",
    "mission/refresh",
    "mission/history",
    "mission/restore",
    // Simulated vehicles (S17): starting and stopping one asks the operator
    // first, unless the agent was given the configuration capability.
    "sim/binaries",
    "sim/start",
    "sim/stop",
    // Starting one again after a crash asks too, and so does pausing one.
    "sim/restart",
    "sim/pause",
    // Practice (S100, ADR-0024 X12): listing and reading attempts; starting
    // one asks first; the agent never flies an exercise for the person.
    "practice/list",
    "practice/start",
    "practice/status",
    // The Get started guide on a simulated vehicle (S117): starting it asks
    // first.
    "practice/guide",
    // Snapshots (S34): saving one and starting from one.
    "sim/snapshot",
    "snapshot/list",
    "snapshot/start",
    // Parameters (S68, ADR-0021 P8): reading them and their definitions;
    // setting one asks the operator first.
    "param/search",
    "param/get",
    "param/list",
    "param/diff",
    "param/set",
    // A vehicle's history (S69, ADR-0022): reading it freely; offer useful
    // notes for completed work, write only with consent and config permission.
    "vehicle/history",
    "vehicle/addNote",
    // Labels (S106): putting them on and off asks the operator first.
    "vehicle/label",
    "vehicle/size",
    // Parameter changes with who made each and why (S74).
    "param/history",
    // A vehicle's own messages (S26): the folded log, freely.
    "vehicle/messages",
    // The ground's height at a point (S23), freely; a vehicle's own terrain
    // checked against it (S119).
    "terrain/at",
    "terrain/check",
    "terrain/status",
    // Areas kept offline (S120): listing and sizing freely; keeping one
    // asks first.
    "terrain/areas",
    "terrain/areaSize",
    "terrain/keepArea",
    // A vehicle's files (S121, ADR-0027 F6, F7): listing and reading small
    // ones freely, such as ArduPilot's `@SYS/uarts.txt`; sending and
    // deleting one asks the operator first.
    "files/list",
    "files/read",
    "files/upload",
    "files/remove",
    // Opening a tab when asked (S43), in the chat's operator's layout.
    "layout/open",
    // A suggestion card for what UXDuck cannot do (S73): the person reads
    // and sends it; the agent cannot send feedback.
    "feedback/suggest",
    // Camera video (S38, ADR-0017 V15): what is arriving and why it plays
    // badly; assigning or naming a camera asks the operator first.
    "video/list",
    "video/devices",
    "video/capture",
    "video/capture/enable",
    "video/record",
    "video/recordAll",
    "video/assign",
    "video/add",
    "video/rename",
    // Finding vehicles (S44, ADR-0018 F10): the network asks first.
    "find/vehicles",
    // Plugins (S57, ADR-0006 X11): their commands found by words, then run
    // as `plugin_call`; running one asks the operator first.
    "tool/search",
    "plugin/command",
    // Writing plugins (S58, ADR-0006 X12): drafts and tests change nothing;
    // installing always asks the operator.
    "plugin/draft",
    "plugin/test",
    "plugin/drafts",
    "plugin/install",
];

/// Methods the model is not sent each turn (maintainer, 2026-10-16: the
/// tools were 65 KB with every request): it finds them with `tool_search`
/// and runs them with `tool_call`. Every tool the instructions name stays
/// loaded. MCP discovery searches the full generated catalogue separately.
pub const DEFERRED_METHODS: &[&str] = &[
    "place/search",
    "place/locate",
    "video/devices",
    "video/capture",
    "video/capture/enable",
    "video/record",
    "video/recordAll",
    "location/status",
    "daemon/about",
    "mavlink/inspect",
    "mavlink/history",
    "router/events",
    "link/diagnose",
    "socket/inspect",
    "daemon/logs",
    "telemetry/window",
    "link/list",
    "plan/list",
    "plan/get",
    "plan/create",
    "plan/update",
    "plan/check",
    "plan/examples",
    "plan/import",
    "plan/export",
    "plan/upload",
    "mission/refresh",
    "mission/history",
    "mission/restore",
    "sim/binaries",
    "sim/start",
    "sim/stop",
    "sim/restart",
    "sim/pause",
    "sim/snapshot",
    "snapshot/list",
    "snapshot/start",
    "practice/guide",
    "param/list",
    "param/diff",
    "param/history",
    "vehicle/label",
    "vehicle/size",
    "terrain/at",
    "terrain/check",
    "terrain/status",
    "terrain/areas",
    "terrain/areaSize",
    "terrain/keepArea",
    "files/list",
    "files/read",
    "files/upload",
    "files/remove",
    "video/rename",
    "plugin/drafts",
];

/// The tool that runs a deferred one by name.
pub const TOOL_CALL: &str = "tool_call";

/// A method's provider-facing name: `/` as `_` and camelCase as snake
/// case, as providers want lowercase (`vehicle/addNote` is
/// `vehicle_add_note`).
#[must_use]
pub fn tool_name(method: &str) -> String {
    // ADR-0006 X11 names it for what the agent does with it.
    if method == "plugin/command" {
        return "plugin_call".into();
    }
    let mut name = String::with_capacity(method.len() + 4);
    for c in method.chars() {
        if c == '/' {
            name.push('_');
        } else if c.is_ascii_uppercase() {
            name.push('_');
            name.push(c.to_ascii_lowercase());
        } else {
            name.push(c);
        }
    }
    name
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The provider-facing name: the method with `/` as `_`, such as `vehicle_list`.
    pub name: String,
    /// The API method it calls, such as `vehicle/list`.
    pub method: String,
    pub description: String,
    /// JSON Schema of the params.
    pub parameters: Value,
}

impl Tool {
    /// The method for a provider-facing tool name.
    #[must_use]
    pub fn method_for(tools: &[Tool], name: &str) -> Option<String> {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.method.clone())
    }
}

/// What the model is sent each turn: the tools not deferred, and
/// `tool_call` to run a deferred one that `tool_search` found.
#[must_use]
pub fn model_tools() -> Vec<Tool> {
    let mut sent: Vec<Tool> = tools()
        .into_iter()
        .filter(|tool| !DEFERRED_METHODS.contains(&tool.method.as_str()))
        .collect();
    if let Some(search) = sent.iter_mut().find(|tool| tool.method == "tool/search") {
        search.description.push_str(
            " Also finds UXDuck's own tools that are not loaded until needed \
             (plans and missions, simulations, terrain, files, parameter lists and \
             history, link diagnostics) in uxduckTools; run those with tool_call.",
        );
    }
    sent.push(Tool {
        name: TOOL_CALL.into(),
        method: String::new(),
        description: "Runs one of UXDuck's own tools that tool_search found (plans and \
            missions, simulations, terrain, a vehicle's files, parameter lists and history, link \
            diagnostics), by its name with params its schema allows. It asks the operator first \
            where the tool itself would."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The tool's name, as tool_search gave it." },
                "params": { "type": "object", "description": "Its params, as its schema says." }
            },
            "required": ["name"]
        }),
    });
    sent
}

/// The deferred tools matching words, best first: each word in a tool's
/// name counts three times one in its description.
#[must_use]
pub fn find_deferred(query: &str, limit: usize) -> Vec<Tool> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(str::to_lowercase)
        .collect();
    let mut scored: Vec<(usize, Tool)> = tools()
        .into_iter()
        .filter(|tool| DEFERRED_METHODS.contains(&tool.method.as_str()))
        .filter_map(|tool| {
            let name = tool.name.to_lowercase();
            let about = tool.description.to_lowercase();
            let score: usize = words
                .iter()
                .map(|w| {
                    3 * usize::from(name.contains(w.as_str()))
                        + usize::from(about.contains(w.as_str()))
                })
                .sum();
            (score > 0).then_some((score, tool))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, tool)| tool)
        .collect()
}

/// Every agent tool, from `ClientRequest`'s schema.
#[must_use]
pub fn tools() -> Vec<Tool> {
    let settings =
        SchemaSettings::draft2020_12().with(|settings| settings.inline_subschemas = true);
    let schema = settings
        .into_generator()
        .into_root_schema_for::<ClientRequest>();
    let variants = schema
        .as_value()
        .get("oneOf")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    AGENT_METHODS
        .iter()
        .filter_map(|method| {
            let variant = variants
                .iter()
                .find(|v| v.pointer("/properties/method/const") == Some(&json!(method)))?;
            let mut parameters = variant
                .pointer("/properties/params")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object" }));
            if let Some(object) = parameters.as_object_mut() {
                object.remove("$schema");
                object.entry("properties").or_insert_with(|| json!({}));
            }
            // Agent calls return a bounded snapshot. Neither model providers
            // nor MCP forward the daemon's subscription notifications.
            if *method == "vehicle/messages"
                && let Some(properties) = parameters["properties"].as_object_mut()
            {
                properties.remove("subscribe");
            }
            let mut description = variant
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(method)
                .to_owned();
            if *method == "vehicle/messages" {
                // The doc comment wraps anywhere; match it as one line.
                description = description.split_whitespace().collect::<Vec<_>>().join(" ");
                description = description.replace(
                    "With `subscribe`, `vehicle/message` follows.",
                    "A bounded snapshot; call again for newer messages. Subscriptions are not supported by agent tools.",
                );
            }
            if *method == "tool/search" {
                // Only the in-app model_tools catalogue adds the turn loop's
                // uxduckTools/tool_call instructions; MCP has its own discovery.
                description = description.split_whitespace().collect::<Vec<_>>().join(" ");
            }
            Some(Tool {
                name: tool_name(method),
                method: (*method).to_owned(),
                description,
                parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn names_are_lowercase_with_underscores() {
        assert_eq!(tool_name("vehicle/list"), "vehicle_list");
        assert_eq!(tool_name("vehicle/addNote"), "vehicle_add_note");
    }

    /// The hosted model takes at most 64 tools, named in lowercase with
    /// underscores, in 128 KB (`apps/api/src/features/model/modelApp.ts`).
    #[test]
    fn the_hosted_model_takes_every_tool() {
        let tools = model_tools();
        assert!(tools.len() <= 64, "{} tools", tools.len());
        for tool in &tools {
            let mut chars = tool.name.chars();
            let fits = tool.name.len() <= 64
                && chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            assert!(fits, "{} would be refused", tool.name);
        }
        let size = serde_json::to_string(
            &tools
                .iter()
                .map(|t| (&t.name, &t.description, &t.parameters))
                .collect::<Vec<_>>(),
        )
        .map_or(usize::MAX, |json| json.len());
        assert!(size < 128 * 1024, "{size} bytes of tools");
        // Sent with every request: kept well under the 65 KB they were.
        assert!(size < 40 * 1024, "{size} bytes of tools sent each turn");
    }

    #[test]
    fn every_agent_method_is_a_tool_with_its_doc_and_params() {
        let tools = tools();
        assert_eq!(tools.len(), AGENT_METHODS.len());
        let list = &tools[0];
        assert_eq!(list.name, "vehicle_list");
        assert_eq!(list.method, "vehicle/list");
        assert!(
            list.description
                .starts_with("The vehicles the daemon hears"),
            "{}",
            list.description
        );
        assert_eq!(tools[1].name, "vehicle_get");
        assert_eq!(tools[1].parameters["required"], serde_json::json!(["id"]));
        assert_eq!(list.parameters["type"], "object");
        assert_eq!(
            Tool::method_for(&tools, "vehicle_list").as_deref(),
            Some("vehicle/list")
        );
    }

    #[test]
    fn diagnosis_tools_offer_history_and_bounded_message_snapshots() {
        let tools = tools();
        for (name, method) in [
            ("daemon_about", "daemon/about"),
            ("mavlink_inspect", "mavlink/inspect"),
            ("mavlink_history", "mavlink/history"),
            ("router_events", "router/events"),
            ("link_diagnose", "link/diagnose"),
            ("socket_inspect", "socket/inspect"),
            ("daemon_logs", "daemon/logs"),
            ("telemetry_window", "telemetry/window"),
            ("link_list", "link/list"),
        ] {
            assert_eq!(Tool::method_for(&tools, name).as_deref(), Some(method));
        }
        let messages = tools
            .iter()
            .find(|tool| tool.name == "vehicle_messages")
            .unwrap();
        assert!(messages.parameters["properties"].get("subscribe").is_none());
        assert!(messages.description.contains("bounded snapshot"));
        assert!(!messages.description.contains("`vehicle/message` follows"));
        let history = tools
            .iter()
            .find(|tool| tool.name == "mavlink_history")
            .unwrap();
        for property in [
            "vehicleId",
            "direction",
            "id",
            "before",
            "limit",
            "linkId",
            "fromUs",
            "toUs",
            "allMessages",
            "includeRaw",
        ] {
            assert!(history.parameters["properties"].get(property).is_some());
        }
    }
}

#[cfg(test)]
mod deferred_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn deferred_tools_are_agent_tools_found_by_words_and_never_named_in_the_instructions() {
        let all: Vec<String> = tools().into_iter().map(|t| t.method).collect();
        for method in DEFERRED_METHODS {
            assert!(
                all.contains(&(*method).to_owned()),
                "{method} is not an agent tool"
            );
            assert!(
                !crate::turn::INSTRUCTIONS.contains(&tool_name(method)),
                "{method} is named in the instructions but not sent"
            );
        }
        let sent = model_tools();
        assert!(
            sent.iter()
                .all(|t| !DEFERRED_METHODS.contains(&t.method.as_str()))
        );
        assert!(sent.iter().any(|t| t.name == TOOL_CALL));
        let found: Vec<String> = find_deferred("upload a plan", 5)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            found.first().map(String::as_str),
            Some("plan_upload"),
            "{found:?}"
        );
        let history = find_deferred("mission history", 3);
        assert_eq!(history[0].name, "mission_history");
    }
}

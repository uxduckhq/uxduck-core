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
    // Simulated vehicles (S17): starting and stopping one asks the operator
    // first, unless the agent was given the configuration capability.
    "sim/binaries",
    "sim/start",
    "sim/stop",
    // Starting one again after a crash asks too.
    "sim/restart",
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
    // A vehicle's history (S69, ADR-0022): reading it freely; a note only
    // when asked, and it asks the operator first.
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
        let tools = tools();
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

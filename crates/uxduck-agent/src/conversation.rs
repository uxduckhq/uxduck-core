//! A conversation as a provider sees it. Both APIs are used statelessly:
//! every request carries the whole conversation, in that provider's own
//! items, and the host keeps them so the next request replays them byte for
//! byte. That is what lets the provider's prompt cache hit, and what
//! Anthropic's check of earlier thinking requires (an edited history is a
//! 400 on accounts created since 2026-08-31). When there is nothing to
//! replay (another provider, a chat from another computer, a turn that did
//! not finish) the conversation is rebuilt from what UXDuck keeps of it:
//! its messages and tool calls, without the model's own reasoning.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Which wire format a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Wire {
    /// The Responses API: `input` items.
    OpenAi,
    /// The Messages API: `messages`.
    Anthropic,
}

impl Wire {
    /// The wire a model speaks: Claude through Messages, the rest through
    /// Responses.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        if model.starts_with("claude-") {
            Self::Anthropic
        } else {
            Self::OpenAi
        }
    }
}

/// One entry of a conversation as UXDuck keeps it, the same for every
/// provider.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User(String),
    Agent(String),
    /// A tool call and what it answered.
    Tool {
        id: String,
        name: String,
        arguments: Value,
        output: String,
        failed: bool,
    },
}

/// What a tool answered, to send back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    pub failed: bool,
}

/// Whether a Claude model takes a `role: "system"` message inside the
/// conversation, which keeps per-chat context out of the cached prefix.
#[must_use]
pub fn takes_system_messages(model: &str) -> bool {
    [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-fable-5",
    ]
    .iter()
    .any(|known| model == *known || model.starts_with(&format!("{known}-")))
        || model.starts_with("claude-fable-")
}

/// Appends a Messages API content block, joining it to the last message
/// when that has the same role (the API needs roles to alternate).
fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut()
        && last["role"] == role
        && let Some(blocks) = last["content"].as_array_mut()
    {
        blocks.push(block);
        return;
    }
    messages.push(json!({ "role": role, "content": [block] }));
}

/// The conversation so far, rebuilt from UXDuck's record of it.
#[must_use]
pub fn rebuild(wire: Wire, entries: &[Entry]) -> Vec<Value> {
    let mut items = Vec::new();
    for entry in entries {
        match (wire, entry) {
            (_, Entry::User(text) | Entry::Agent(text)) if text.is_empty() => {}
            (Wire::OpenAi, Entry::User(text)) => {
                items.push(json!({ "role": "user", "content": text }));
            }
            (Wire::OpenAi, Entry::Agent(text)) => {
                items.push(json!({ "role": "assistant", "content": text }));
            }
            (
                Wire::OpenAi,
                Entry::Tool {
                    id,
                    name,
                    arguments,
                    output,
                    ..
                },
            ) => {
                items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments.to_string(),
                }));
                items.push(
                    json!({ "type": "function_call_output", "call_id": id, "output": output }),
                );
            }
            (Wire::Anthropic, Entry::User(text)) => {
                push_block(&mut items, "user", json!({ "type": "text", "text": text }));
            }
            (Wire::Anthropic, Entry::Agent(text)) => {
                push_block(
                    &mut items,
                    "assistant",
                    json!({ "type": "text", "text": text }),
                );
            }
            (
                Wire::Anthropic,
                Entry::Tool {
                    id,
                    name,
                    arguments,
                    output,
                    failed,
                },
            ) => {
                push_block(
                    &mut items,
                    "assistant",
                    json!({ "type": "tool_use", "id": id, "name": name, "input": arguments }),
                );
                push_block(
                    &mut items,
                    "user",
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": output,
                        "is_error": failed,
                    }),
                );
            }
        }
    }
    items
}

/// A new message from the operator.
#[must_use]
pub fn user(wire: Wire, text: &str) -> Vec<Value> {
    match wire {
        Wire::OpenAi => vec![json!({ "role": "user", "content": text })],
        Wire::Anthropic => {
            vec![json!({ "role": "user", "content": [{ "type": "text", "text": text }] })]
        }
    }
}

/// Context for the rest of the chat, such as which vehicle it is about,
/// placed after the operator's message so the cached prefix stays the same.
/// A Claude model that takes no system messages gets it as a reminder in
/// the operator's turn instead.
#[must_use]
pub fn context(wire: Wire, model: &str, text: &str) -> Vec<Value> {
    match wire {
        Wire::OpenAi => vec![json!({ "role": "developer", "content": text })],
        Wire::Anthropic if takes_system_messages(model) => {
            vec![json!({ "role": "system", "content": text })]
        }
        Wire::Anthropic => vec![json!({
            "role": "user",
            "content": [{ "type": "text", "text": format!("<system-reminder>{text}</system-reminder>") }],
        })],
    }
}

/// The tools' answers, all in one message (the Messages API expects them
/// together after parallel calls).
#[must_use]
pub fn tool_results(wire: Wire, results: &[ToolResult]) -> Vec<Value> {
    match wire {
        Wire::OpenAi => results
            .iter()
            .map(|r| json!({ "type": "function_call_output", "call_id": r.call_id, "output": r.output }))
            .collect(),
        Wire::Anthropic if results.is_empty() => Vec::new(),
        Wire::Anthropic => vec![json!({
            "role": "user",
            "content": results
                .iter()
                .map(|r| json!({
                    "type": "tool_result",
                    "tool_use_id": r.call_id,
                    "content": r.output,
                    "is_error": r.failed,
                }))
                .collect::<Vec<_>>(),
        })],
    }
}

/// A rough count of a conversation's tokens: four characters each.
#[must_use]
pub fn estimate_tokens(items: &[Value]) -> usize {
    items
        .iter()
        .map(|item| item.to_string().len().div_ceil(4))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<Entry> {
        vec![
            Entry::User("what vehicles?".into()),
            Entry::Tool {
                id: "t1".into(),
                name: "vehicle_list".into(),
                arguments: json!({}),
                output: r#"{"vehicles":[]}"#.into(),
                failed: false,
            },
            Entry::Agent("None yet.".into()),
            Entry::User(String::new()),
        ]
    }

    #[test]
    fn a_history_rebuilds_as_responses_items() {
        assert_eq!(
            rebuild(Wire::OpenAi, &history()),
            [
                json!({ "role": "user", "content": "what vehicles?" }),
                json!({ "type": "function_call", "call_id": "t1", "name": "vehicle_list", "arguments": "{}" }),
                json!({ "type": "function_call_output", "call_id": "t1", "output": r#"{"vehicles":[]}"# }),
                json!({ "role": "assistant", "content": "None yet." }),
            ]
        );
    }

    #[test]
    fn a_history_rebuilds_as_alternating_messages() {
        assert_eq!(
            rebuild(Wire::Anthropic, &history()),
            [
                json!({ "role": "user", "content": [{ "type": "text", "text": "what vehicles?" }] }),
                json!({ "role": "assistant", "content": [
                    { "type": "tool_use", "id": "t1", "name": "vehicle_list", "input": {} }
                ] }),
                json!({ "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": r#"{"vehicles":[]}"#, "is_error": false }
                ] }),
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "None yet." }] }),
            ]
        );
    }

    #[test]
    fn context_goes_in_a_system_message_where_the_model_takes_one() {
        assert_eq!(
            context(Wire::Anthropic, "claude-opus-5-5", "About Copter 1."),
            [json!({ "role": "system", "content": "About Copter 1." })]
        );
        assert_eq!(
            context(Wire::Anthropic, "claude-haiku-4-5", "About Copter 1.")[0]["role"],
            "user"
        );
        assert_eq!(
            context(Wire::OpenAi, "gpt-6-sol", "About Copter 1.")[0]["role"],
            "developer"
        );
    }

    #[test]
    fn tool_results_go_back_together() {
        let results = [
            ToolResult {
                call_id: "a".into(),
                output: "1".into(),
                failed: false,
            },
            ToolResult {
                call_id: "b".into(),
                output: "no".into(),
                failed: true,
            },
        ];
        let anthropic = tool_results(Wire::Anthropic, &results);
        assert_eq!(anthropic.len(), 1, "one user message");
        assert_eq!(anthropic[0]["content"][1]["is_error"], true);
        assert_eq!(tool_results(Wire::OpenAi, &results).len(), 2);
    }
}

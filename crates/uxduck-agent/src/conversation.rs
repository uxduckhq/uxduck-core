//! A conversation as a provider sees it. Both APIs are used statelessly:
//! every request carries the whole conversation, in that provider's own
//! items, and the host keeps them so the next request replays them byte for
//! byte. That is what lets the provider's prompt cache hit, and what
//! Anthropic's check of earlier thinking requires (an edited history is a
//! 400 on accounts created since 2026-08-31). When there is nothing to
//! replay (another provider, a chat from another computer, a turn that did
//! not finish) the conversation is rebuilt from what UXDuck keeps of it:
//! its messages and tool calls, without the model's own reasoning.
//!
//! Replaying only pays while the provider still caches the conversation.
//! Once that has lapsed (`cached_for_ms` after it was last sent: an hour's
//! cache for Claude, half an hour's for GPT-6 and 5 minutes for other models)
//! the host rebuilds instead, and a rebuild leaves out the long tool results of
//! earlier messages (`slim`), which the whole history would otherwise carry
//! on every request until the chat is summarised.

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
    /// Images the model is shown after `output` (a camera's frames,
    /// FB-244).
    pub images: Vec<Shown>,
}

/// An image a tool's result shows the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The JPEG, base64.
    pub jpeg: String,
    /// What it is, told just before it, such as "Image from Front at
    /// 14:02:07 UTC".
    pub label: String,
}

/// Stands in for an image once the turn that took it has ended, so a chat
/// keeps no camera picture and sends it only while that turn runs.
pub const IMAGE_NOT_KEPT: &str =
    "[Not kept after the turn that took it. Look through the camera again to see it now.]";

/// What one image costs a model, in tokens, for estimates: Claude reads a
/// 1024 × 576 picture as about 790, GPT as 765 at high detail.
pub const IMAGE_TOKENS: usize = 800;

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

/// How long a model before GPT-6, or an open-weight model, likely keeps a
/// conversation in its prompt cache after it was last sent: a prefix lasts
/// about 5 minutes from its last use. After that, replaying the
/// conversation as it was sent saves nothing.
pub const CACHE_LIFETIME_MS: u64 = 5 * 60 * 1000;

/// How long a GPT-6 chat is replayed: OpenAI keeps a prompt of GPT-5.6 or
/// later cached for at least 30 minutes from its last use by default
/// (<https://developers.openai.com/api/docs/guides/prompt-caching>, read
/// 2026-11-15), on a developer's own key and on the hosted model alike, and
/// 5 minutes of that are left for the turn's own rounds.
pub const HALF_HOUR_CACHE_REPLAY_MS: u64 = 25 * 60 * 1000;

/// How long a Claude chat is replayed: on a developer's own key and on the
/// hosted model alike it is cached for an hour from the start of its last
/// request (`anthropic`, the API's `modelApp.ts`), and 5 minutes of that
/// are left for the turn's own rounds.
pub const HOUR_CACHE_REPLAY_MS: u64 = 55 * 60 * 1000;

/// How long after its last request a conversation with `model` is likely
/// still cached, on a developer's own key or the hosted model.
#[must_use]
pub fn cached_for_ms(model: &str) -> u64 {
    match Wire::for_model(model) {
        Wire::Anthropic => HOUR_CACHE_REPLAY_MS,
        Wire::OpenAi if model.starts_with("gpt-6") => HALF_HOUR_CACHE_REPLAY_MS,
        Wire::OpenAi => CACHE_LIFETIME_MS,
    }
}

/// Whether a conversation last sent at `sent_ms` is likely still in the
/// cache of a provider that keeps it `cached_for_ms` (`CACHE_LIFETIME_MS`,
/// `HALF_HOUR_CACHE_REPLAY_MS` or `HOUR_CACHE_REPLAY_MS`) at `now_ms`, all in
/// milliseconds on the same clock.
#[must_use]
pub fn cache_warm(cached_for_ms: u64, sent_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(sent_ms) <= cached_for_ms
}

/// The longest tool result of an earlier message a rebuild keeps whole, in
/// characters.
pub const KEEP_WHOLE_CHARS: usize = 600;

/// Stands in for a long tool result of an earlier message.
pub const ELIDED: &str = r#"{"elided":"An earlier result, left out to save context. Call the tool again for current values."}"#;

/// Stands in for a long tool error of an earlier message, so a wire without
/// an error flag still says the call failed.
pub const ELIDED_ERROR: &str = r#"{"elided":"An earlier error, left out to save context. Call the tool again for current values."}"#;

/// A chat's earlier messages, slimmed for a rebuild: each tool result over
/// `KEEP_WHOLE_CHARS` becomes a short stub, and the call, its arguments and
/// whether it failed stay. Give it only messages whose turns have ended: the
/// current turn's results are added after the rebuild, and stay whole.
#[must_use]
pub fn slim(mut entries: Vec<Entry>) -> Vec<Entry> {
    for entry in &mut entries {
        if let Entry::Tool { output, failed, .. } = entry
            && output.chars().count() > KEEP_WHOLE_CHARS
        {
            if *failed { ELIDED_ERROR } else { ELIDED }.clone_into(output);
        }
    }
    entries
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

/// Whether `items` hold the call `call_id`: a Responses API
/// `function_call`, or a Messages API `tool_use` block.
#[must_use]
pub fn has_call(items: &[Value], call_id: &str) -> bool {
    items.iter().any(|item| {
        (item["type"] == "function_call" && item["call_id"] == call_id)
            || item["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block["type"] == "tool_use" && block["id"] == call_id)
            })
    })
}

/// The tools' answers, all in one message (the Messages API expects them
/// together after parallel calls).
#[must_use]
pub fn tool_results(wire: Wire, results: &[ToolResult]) -> Vec<Value> {
    match wire {
        Wire::OpenAi => results
            .iter()
            .map(|r| {
                let output = if r.images.is_empty() {
                    json!(r.output)
                } else {
                    std::iter::once(json!({ "type": "input_text", "text": r.output }))
                        .chain(r.images.iter().flat_map(|image| {
                            [
                                json!({ "type": "input_text", "text": format!("{}:", image.label) }),
                                json!({
                                    "type": "input_image",
                                    "image_url": format!("data:image/jpeg;base64,{}", image.jpeg),
                                    "detail": "high",
                                }),
                            ]
                        }))
                        .collect()
                };
                json!({ "type": "function_call_output", "call_id": r.call_id, "output": output })
            })
            .collect(),
        Wire::Anthropic if results.is_empty() => Vec::new(),
        Wire::Anthropic => vec![json!({
            "role": "user",
            "content": results
                .iter()
                .map(|r| {
                    let content = if r.images.is_empty() {
                        json!(r.output)
                    } else {
                        std::iter::once(json!({ "type": "text", "text": r.output }))
                            .chain(r.images.iter().flat_map(|image| [
                                json!({ "type": "text", "text": format!("{}:", image.label) }),
                                json!({
                                    "type": "image",
                                    "source": { "type": "base64", "media_type": "image/jpeg", "data": image.jpeg },
                                }),
                            ]))
                            .collect()
                    };
                    json!({
                        "type": "tool_result",
                        "tool_use_id": r.call_id,
                        "content": content,
                        "is_error": r.failed,
                    })
                })
                .collect::<Vec<_>>(),
        })],
    }
}

/// Puts `IMAGE_NOT_KEPT` in place of every image in tool results, on
/// either wire, once the turn that took them has ended.
pub fn forget_images(items: &mut [Value]) {
    fn forget(output: Option<&mut Value>) {
        for part in output.and_then(Value::as_array_mut).into_iter().flatten() {
            match part["type"].as_str() {
                Some("input_image") => {
                    *part = json!({ "type": "input_text", "text": IMAGE_NOT_KEPT });
                }
                Some("image") => *part = json!({ "type": "text", "text": IMAGE_NOT_KEPT }),
                _ => {}
            }
        }
    }
    for item in items {
        if item["type"] == "function_call_output" {
            forget(item.get_mut("output"));
        } else if let Some(blocks) = item.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks
                .iter_mut()
                .filter(|block| block["type"] == "tool_result")
            {
                forget(block.get_mut("content"));
            }
        }
    }
}

/// Whether any tool result in `items` carries an image: a camera's frames.
#[must_use]
pub fn has_images(items: &[Value]) -> bool {
    let images = |output: Option<&Value>| {
        output.and_then(Value::as_array).is_some_and(|parts| {
            parts
                .iter()
                .any(|part| matches!(part["type"].as_str(), Some("input_image" | "image")))
        })
    };
    items.iter().any(|item| {
        (item["type"] == "function_call_output" && images(item.get("output")))
            || item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block["type"] == "tool_result" && images(block.get("content")))
                })
    })
}

/// Stands in for a tool result left out so the chat still fits a request.
pub const LEFT_OUT: &str = "[Left out to keep this chat within the request size limit. Call the tool again if you need this result.]";

/// Leaves out tool results in the items before `before`, oldest first,
/// until `fits` says the conversation does. Whether it fits in the end.
pub fn leave_out_tool_results(
    items: &mut [Value],
    before: usize,
    fits: impl Fn(&[Value]) -> bool,
) -> bool {
    let mut outputs = Vec::new();
    for (index, item) in items.iter().enumerate().take(before) {
        if item["type"] == "function_call_output" {
            outputs.push((index, None));
        } else if item["role"] == "user"
            && let Some(blocks) = item["content"].as_array()
        {
            outputs.extend(
                blocks
                    .iter()
                    .enumerate()
                    .filter(|(_, block)| block["type"] == "tool_result")
                    .map(|(block, _)| (index, Some(block))),
            );
        }
    }
    if fits(items) {
        return true;
    }
    for (index, block) in outputs {
        let output = match block {
            None => &mut items[index]["output"],
            Some(block) => &mut items[index]["content"][block]["content"],
        };
        // A result with images is always worth leaving out.
        if output
            .as_str()
            .map_or(output.is_array(), |text| text.len() > LEFT_OUT.len())
        {
            *output = json!(LEFT_OUT);
            if fits(items) {
                return true;
            }
        }
    }
    false
}

/// Where the latest round starts: just after the model's last output, so
/// the results it is waiting for.
#[must_use]
pub fn latest_round(items: &[Value]) -> usize {
    items
        .iter()
        .rposition(|item| {
            item["role"] == "assistant"
                || matches!(item["type"].as_str(), Some("function_call" | "reasoning"))
        })
        .map_or(0, |index| index + 1)
}

/// The images in `value`, and the characters they take.
fn images(value: &Value) -> (usize, usize) {
    match value {
        Value::Object(fields)
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some("input_image" | "image")
            ) =>
        {
            (1, value.to_string().len())
        }
        Value::Object(fields) => fields
            .values()
            .map(images)
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        Value::Array(items) => items
            .iter()
            .map(images)
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        _ => (0, 0),
    }
}

/// How many images a conversation carries.
#[must_use]
pub fn image_count(items: &[Value]) -> usize {
    items.iter().map(|item| images(item).0).sum()
}

/// A rough count of a conversation's tokens: four characters each, and an
/// image's `IMAGE_TOKENS` rather than its bytes.
#[must_use]
pub fn estimate_tokens(items: &[Value]) -> usize {
    items
        .iter()
        .map(|item| {
            let (count, bytes) = images(item);
            (item.to_string().len() - bytes).div_ceil(4) + count * IMAGE_TOKENS
        })
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
                images: Vec::new(),
            },
            ToolResult {
                call_id: "b".into(),
                output: "no".into(),
                failed: true,
                images: Vec::new(),
            },
        ];
        let anthropic = tool_results(Wire::Anthropic, &results);
        assert_eq!(anthropic.len(), 1, "one user message");
        assert_eq!(anthropic[0]["content"][1]["is_error"], true);
        assert_eq!(tool_results(Wire::OpenAi, &results).len(), 2);
    }

    /// Two finished messages, with a long result, a long error and a short
    /// result.
    fn earlier_messages() -> Vec<Entry> {
        let tool = |id: &str, output: String, failed: bool| Entry::Tool {
            id: id.into(),
            name: "param_search".into(),
            arguments: json!({ "query": id }),
            output,
            failed,
        };
        vec![
            Entry::User("Why does it climb?".into()),
            tool(
                "long",
                format!(r#"{{"params":"{}"}}"#, "p".repeat(5_000)),
                false,
            ),
            tool("short", r#"{"vehicles":[]}"#.into(), false),
            Entry::Agent("Its hover throttle is low.".into()),
            Entry::User("And the failsafe?".into()),
            tool(
                "error",
                format!(r#"{{"error":"{}"}}"#, "e".repeat(700)),
                true,
            ),
            tool("edge", "x".repeat(KEEP_WHOLE_CHARS), false),
            Entry::Agent("It could not read them.".into()),
        ]
    }

    /// Every call answered by its own result, and on the Messages API roles
    /// that alternate with each result right after its call.
    fn assert_paired(wire: Wire, items: &[Value]) {
        match wire {
            Wire::OpenAi => {
                let ids = |kind: &str| -> Vec<String> {
                    items
                        .iter()
                        .filter(|item| item["type"] == kind)
                        .map(|item| item["call_id"].as_str().unwrap().to_owned())
                        .collect()
                };
                assert_eq!(ids("function_call"), ids("function_call_output"));
            }
            Wire::Anthropic => {
                for pair in items.windows(2) {
                    assert_ne!(pair[0]["role"], pair[1]["role"], "roles alternate");
                }
                for (index, item) in items.iter().enumerate() {
                    let blocks = |item: &Value, kind: &str, field: &str| -> Vec<String> {
                        item["content"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|block| block["type"] == kind)
                            .map(|block| block[field].as_str().unwrap().to_owned())
                            .collect()
                    };
                    let calls = blocks(item, "tool_use", "id");
                    if !calls.is_empty() {
                        assert_eq!(
                            calls,
                            blocks(&items[index + 1], "tool_result", "tool_use_id")
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_slimmed_rebuild_stubs_only_the_long_results_of_earlier_messages() {
        let entries = slim(earlier_messages());
        let outputs: Vec<(&str, &str, bool)> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Tool {
                    id, output, failed, ..
                } => Some((id.as_str(), output.as_str(), *failed)),
                _ => None,
            })
            .collect();
        assert_eq!(
            outputs[..3],
            [
                ("long", ELIDED, false),
                ("short", r#"{"vehicles":[]}"#, false),
                ("error", ELIDED_ERROR, true),
            ]
        );
        assert_eq!(
            outputs[3].1.len(),
            KEEP_WHOLE_CHARS,
            "at the limit it stays"
        );
        assert!(entries.iter().all(|entry| match entry {
            Entry::Tool { id, arguments, .. } => arguments == &json!({ "query": id }),
            _ => true,
        }));
        for text in [ELIDED, ELIDED_ERROR] {
            assert!(serde_json::from_str::<Value>(text).is_ok());
            assert!(!text.contains(';'));
        }

        for wire in [Wire::OpenAi, Wire::Anthropic] {
            let whole = rebuild(wire, &earlier_messages());
            let slimmed = rebuild(wire, &entries);
            assert_eq!(whole.len(), slimmed.len(), "{wire:?}");
            assert_paired(wire, &slimmed);
            let text = Value::Array(slimmed.clone()).to_string();
            assert!(!text.contains(&"p".repeat(5_000)), "{wire:?}");
            assert!(text.contains(r#"{\"vehicles\":[]}"#), "{wire:?}");
            assert!(
                Value::Array(slimmed).to_string().len() + 5_000
                    < Value::Array(whole).to_string().len(),
                "{wire:?}"
            );
        }
        let messages = rebuild(Wire::Anthropic, &entries);
        assert_eq!(
            messages[8]["content"][0],
            json!({ "type": "tool_result", "tool_use_id": "error", "content": ELIDED_ERROR, "is_error": true })
        );
        assert_eq!(
            messages[7]["content"][0],
            json!({ "type": "tool_use", "id": "error", "name": "param_search", "input": { "query": "error" } })
        );
    }

    #[test]
    fn the_cache_is_taken_as_warm_for_five_minutes_after_the_last_send() {
        let sent = 1_700_000_000_000;
        let warm = |now| cache_warm(CACHE_LIFETIME_MS, sent, now);
        assert!(warm(sent));
        assert!(warm(sent + CACHE_LIFETIME_MS));
        assert!(!warm(sent + CACHE_LIFETIME_MS + 1));
        assert!(
            !cache_warm(CACHE_LIFETIME_MS, 0, sent),
            "never sent, or not known when"
        );
        assert!(warm(sent - 1_000), "a clock set back");
    }

    #[test]
    fn claude_chats_are_replayed_for_55_minutes_gpt_6_chats_for_25_and_others_for_5() {
        assert_eq!(cached_for_ms("claude-opus-5-5"), HOUR_CACHE_REPLAY_MS);
        assert_eq!(HALF_HOUR_CACHE_REPLAY_MS, 25 * 60 * 1000);
        for gpt in ["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"] {
            assert_eq!(cached_for_ms(gpt), HALF_HOUR_CACHE_REPLAY_MS, "{gpt}");
        }
        for other in [
            "gpt-5.4",
            "moonshotai/kimi-k3.1-pro",
            "deepseek/deepseek-v4-pro",
        ] {
            assert_eq!(cached_for_ms(other), CACHE_LIFETIME_MS, "{other}");
        }
    }

    #[test]
    fn an_hour_cache_is_taken_as_warm_for_55_minutes_after_the_last_send() {
        let sent = 1_700_000_000_000;
        let warm = |now| cache_warm(HOUR_CACHE_REPLAY_MS, sent, now);
        assert_eq!(HOUR_CACHE_REPLAY_MS, 55 * 60 * 1000);
        assert!(warm(sent + CACHE_LIFETIME_MS + 1));
        assert!(warm(sent + HOUR_CACHE_REPLAY_MS));
        assert!(!warm(sent + HOUR_CACHE_REPLAY_MS + 1));
    }

    #[test]
    fn tool_results_are_left_out_oldest_first_until_the_chat_fits() {
        let long = "r".repeat(10_000);
        let entries: Vec<Entry> = (0..3)
            .map(|i| Entry::Tool {
                id: format!("t{i}"),
                name: "flight_series".into(),
                arguments: json!({}),
                output: long.clone(),
                failed: false,
            })
            .collect();
        for wire in [Wire::OpenAi, Wire::Anthropic] {
            let mut items = rebuild(wire, &entries);
            let size = |items: &[Value]| Value::Array(items.to_vec()).to_string().len();
            let whole = size(&items);
            let all = items.len();
            assert!(leave_out_tool_results(&mut items, all, |items| size(items) < whole - 15_000));
            let text = Value::Array(items.clone()).to_string();
            assert_eq!(text.matches(LEFT_OUT).count(), 2, "{wire:?}");
            assert!(text.contains(&long), "the newest result stays");
            assert!(!leave_out_tool_results(&mut items, all, |_| false));
            assert_eq!(Value::Array(items).to_string().matches(LEFT_OUT).count(), 3);

            let mut items = rebuild(wire, &entries);
            let latest = latest_round(&items);
            assert!(latest > 0 && latest < items.len(), "{wire:?}");
            assert!(!leave_out_tool_results(&mut items, latest, |_| false));
            let text = Value::Array(items).to_string();
            assert_eq!(text.matches(LEFT_OUT).count(), 2, "{wire:?}");
            assert!(text.contains(&long), "the latest round is never left out");
        }
    }
}

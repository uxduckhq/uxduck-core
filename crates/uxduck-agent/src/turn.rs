//! One turn: the operator's message in; streamed text and tool calls out,
//! the calls run as API calls through the host; their results back; until
//! the model answers without calling a tool. The conversation comes in and
//! goes out whole, as the provider's own items (see `conversation`), for
//! the host to keep and replay next turn, with what the turn used.

use futures::StreamExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};

use crate::conversation::{self, ToolResult};
use crate::models;
use crate::provider::{Provider, ProviderError, ProviderEvent, ProviderRequest, Usage};
use crate::tools::{TOOL_CALL, Tool, find_deferred, model_tools, tools};

/// Tool rounds allowed in one turn, so a confused model cannot loop forever.
pub const MAX_ROUNDS: usize = 8;

/// The most of one tool result the model gets; a parameter list can run to
/// thousands of entries.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 24_000;

/// What the agent is told about where it is. The same in every request, so
/// it caches; the hosted model sends its own copy, which must match.
pub const INSTRUCTIONS: &str = "You are the assistant inside UXDuck, a ground control station for \
MAVLink vehicles. Answer from the tools, which read the daemon's live state; never guess vehicle \
state. Be brief. Before advising on a parameter, \
read it with param_get (find it with param_search): describe it only as its definition does, \
name the set it comes from (describedBy), and say when that set is approximate or missing, or \
when a parameter has no definition; never guess what an undescribed parameter does. Before \
diagnosing a vehicle or advising a change, read its recent history (vehicle_history) and cite \
what bears on it with its date. When the person reports completed maintenance, repairs, \
motor or propeller replacements, or other physical work, offer to record it in vehicle history. \
After your own completed changes, offer a note only for useful context missing from automatic \
history. Read recent history first to avoid duplicates; include reasons in parameter writes, \
which UXDuck records automatically. Batch related work into one concise proposed note and \
ask before calling vehicle_add_note; wait for the person's agreement. An explicit request to \
record the work already gives consent, so do not ask again. Configuration permission alone \
is not consent to add a note. Do not interrupt active flying or an unfinished task with an \
offer, or repeat an offer the person declined. Record only completed work, attribute user \
reports as reported rather than verified, and preserve supplied dates and details without \
inventing any. Never edit or delete a note. To know what the vehicle itself said, such as why it \
will not arm, read its messages (vehicle_messages) and quote them. When asked to show something, open its tab with layout_open, such as flight-display for the flight instruments (speed, altitude and heading, the horizon and what is wrong) or instruments for the classic, Mission Planner-style ones when asked for those, and only when asked. When someone asks for something UXDuck cannot do, do what you can, then offer once to suggest it to the UXDuck team; only when they say yes, write it up with feedback_suggest, which shows them a card to read, change and send, and sends nothing. Never offer while a vehicle is armed or in the middle of a task, and not again in a chat after they say no. For camera video, video_list says what is arriving, its codec \
and its counters: explain a bad picture from them, and assign a camera to a vehicle \
(video_assign) or add one (video_add) only when asked. When someone cannot connect a \
vehicle, look with find_vehicles (scope nearby; network only when they agree) and explain \
each reason it gives. More of UXDuck's tools load when asked for: plans and missions (checking, \
uploading, examples, a vehicle's missions over time), simulations and snapshots, terrain, a \
vehicle's files, parameter lists and history, and link diagnostics. Find one with tool_search \
(its uxduckTools), then run it with tool_call, giving its name and params. To use an installed \
plugin, find its commands with tool_search, then \
run one with plugin_call, giving params its schema allows; say which plugin did it. When asked \
for a value or a panel UXDuck lacks, write a plugin: draft one TypeScript file with plugin_draft, \
test it with plugin_test against the vehicle's last minutes and say what it produced, and \
install it with plugin_install only when asked; an operator always approves an install. For practice exercises, practice_list lists them and practice_status reads a vehicle's attempt; start one with practice_start only when asked, and never fly an exercise for the person: your commands do not count, so explain the step instead.";

/// Asked of the model when a chat has grown too long to send whole.
pub const SUMMARY_REQUEST: &str = "Before we go on: summarise our conversation so far for your own \
later use, in plain prose. Keep what the operator asked and decided, what you found with the tools \
(vehicle names and ids, values, positions, times), and anything still open. Do not call any tools.";

/// What starts a chat's conversation after it was summarised.
pub const SUMMARY_PREFIX: &str = "This chat grew long, so here is a summary of it so far:";

/// What the host records, and how it calls the API as the agent client.
/// The local host calls the daemon in-process; the cloud host (S16) calls it
/// over the edge connection.
pub trait Host: Send + Sync {
    /// Starts an agent message; returns its item id.
    fn message_start(&self) -> BoxFuture<'_, String>;
    fn text_delta(&self, item: String, delta: String) -> BoxFuture<'_, ()>;
    /// Ends a message; `response` is the provider's id for the response it
    /// came in, which feedback on it refers to (S29).
    fn message_finish(&self, item: String, response: Option<String>) -> BoxFuture<'_, ()>;
    /// Records a tool call about to be made; returns its item id.
    fn tool_start(&self, method: String, params: Value) -> BoxFuture<'_, String>;
    /// Calls an API method as the agent. `Err` holds the `ApiError` object.
    fn call(&self, method: String, params: Value) -> BoxFuture<'_, Result<Value, Value>>;
    fn tool_finish(&self, item: String, outcome: Result<Value, Value>) -> BoxFuture<'_, ()>;
    /// Milliseconds on some steady clock, for the time to the first token.
    fn now_ms(&self) -> u64 {
        0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub model: String,
    pub text: String,
    /// The conversation so far: replayed as it was sent, or rebuilt.
    pub conversation: Vec<Value>,
    /// What the chat is about, such as its vehicle.
    pub context: Option<String>,
    /// Whether `conversation` already carries `context`.
    pub context_sent: bool,
    /// The previous turn's last response, which a summary continues.
    pub previous: Option<String>,
    /// Groups this chat's requests for OpenAI's cache.
    pub cache_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(
        "The agent was stopped automatically because it reached the limit of {MAX_ROUNDS} tool rounds."
    )]
    TooManyRounds,
}

/// What one turn used and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnStats {
    pub rounds: u32,
    pub usage: Usage,
    pub tool_calls: u32,
    pub tool_errors: u32,
    pub retries: u32,
    pub fell_back: bool,
    pub compacted: bool,
    /// From the first request to the first streamed text or call.
    pub first_token_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub result: Result<(), TurnError>,
    /// The last response, which the next turn's summary would continue.
    pub response_id: Option<String>,
    /// The whole conversation after this turn, to keep and replay. Only a
    /// turn that ended well should be kept.
    pub conversation: Vec<Value>,
    pub context_sent: bool,
    pub stats: TurnStats,
}

/// One model response, as the turn loop sees it.
#[derive(Default)]
struct Round {
    calls: Vec<(String, String, String)>,
    response_id: Option<String>,
    output: Vec<Value>,
    text: String,
}

/// Sends one request and follows its stream. With `show`, text goes into
/// an agent message in the chat; without, it is only collected.
async fn round(
    provider: &dyn Provider,
    host: &dyn Host,
    request: ProviderRequest,
    stats: &mut TurnStats,
    started: u64,
    show: bool,
) -> Result<Round, ProviderError> {
    let mut events = provider.respond(request);
    let mut out = Round::default();
    let mut message: Option<String> = None;
    let first = |stats: &mut TurnStats| {
        if stats.first_token_ms.is_none() {
            stats.first_token_ms = Some(host.now_ms().saturating_sub(started));
        }
    };
    let failure = loop {
        let Some(event) = events.next().await else {
            break None;
        };
        match event {
            Err(error) => break Some(error),
            Ok(ProviderEvent::TextDelta(delta)) => {
                first(stats);
                out.text.push_str(&delta);
                if show {
                    let item = if let Some(item) = &message {
                        item.clone()
                    } else {
                        let item = host.message_start().await;
                        message = Some(item.clone());
                        item
                    };
                    host.text_delta(item, delta).await;
                }
            }
            Ok(ProviderEvent::ToolCall {
                call_id,
                name,
                arguments,
            }) => {
                first(stats);
                out.calls.push((call_id, name, arguments));
            }
            Ok(ProviderEvent::Completed {
                response_id,
                output,
                usage,
            }) => {
                stats.rounds += 1;
                stats.usage.add(&usage);
                out.response_id = Some(response_id);
                out.output = output;
            }
            Ok(ProviderEvent::Retried) => stats.retries += 1,
            Ok(ProviderEvent::FellBack { .. }) => stats.fell_back = true,
        }
    };
    if let Some(item) = message {
        host.message_finish(item, out.response_id.clone()).await;
    }
    match failure {
        Some(error) => Err(error),
        None if out.response_id.is_none() => Err(ProviderError::Transport(
            "the answer stopped before it ended".into(),
        )),
        None => Ok(out),
    }
}

/// A tool result the model can take: cut, with a note, when too long.
fn capped(output: String) -> String {
    match output.char_indices().nth(MAX_TOOL_OUTPUT_CHARS) {
        None => output,
        Some((at, _)) => {
            let rest = output[at..].chars().count();
            format!(
                "{}\n[cut: {rest} more characters. Ask for less, such as one vehicle or fewer fields.]",
                &output[..at]
            )
        }
    }
}

/// Calls the tools a round asked for, in order, recording each in the chat.
async fn run_tools(
    host: &dyn Host,
    tools: &[Tool],
    calls: Vec<(String, String, String)>,
    stats: &mut TurnStats,
) -> Vec<ToolResult> {
    let mut results = Vec::new();
    for (call_id, name, arguments) in calls {
        let params: Value = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
        stats.tool_calls += 1;
        // A deferred tool, run by name with `tool_call` (or called as itself).
        let (name, params) = if name == TOOL_CALL {
            (
                params["name"].as_str().unwrap_or_default().to_owned(),
                params.get("params").cloned().unwrap_or_else(|| json!({})),
            )
        } else {
            (name, params)
        };
        let (output, failed) = match Tool::method_for(tools, &name) {
            Some(method) => {
                let query = params["query"].as_str().unwrap_or_default().to_owned();
                let limit = params["limit"].as_u64().map_or(10, |n| n.clamp(1, 50));
                let item = host.tool_start(method.clone(), params.clone()).await;
                let mut outcome = host.call(method.clone(), params).await;
                // UXDuck's own deferred tools, beside the plugins' commands.
                if method == "tool/search"
                    && let Ok(found) = &mut outcome
                {
                    #[allow(clippy::cast_possible_truncation, reason = "at most 50")]
                    let own: Vec<Value> = find_deferred(&query, limit as usize)
                        .into_iter()
                        .map(|t| json!({ "name": t.name, "description": t.description, "parameters": t.parameters }))
                        .collect();
                    found["uxduckTools"] = json!(own);
                }
                host.tool_finish(item, outcome.clone()).await;
                match outcome {
                    Ok(value) => (value.to_string(), false),
                    Err(value) => (value.to_string(), true),
                }
            }
            None => (
                format!(
                    r#"{{"error":"No tool named {name}. Find UXDuck's tools and plugins' commands with tool_search"}}"#
                ),
                true,
            ),
        };
        if failed {
            stats.tool_errors += 1;
        }
        results.push(ToolResult {
            call_id,
            output: capped(output),
            failed,
        });
    }
    results
}

/// Runs one turn.
pub async fn run_turn(provider: &dyn Provider, host: &dyn Host, turn: Turn) -> TurnOutcome {
    let wire = provider.wire();
    let settings = models::settings(&turn.model);
    // Every tool may be run; the deferred ones are not sent (`model_tools`).
    let tools: Vec<Tool> = tools();
    let sent: Vec<Tool> = model_tools();
    let started = host.now_ms();
    let mut stats = TurnStats::default();
    let mut conversation = turn.conversation;
    let mut context_sent = turn.context_sent;
    let request = |conversation: &Vec<Value>, continues: Option<String>| ProviderRequest {
        model: turn.model.clone(),
        instructions: INSTRUCTIONS.into(),
        conversation: conversation.clone(),
        tools: sent.clone(),
        max_output_tokens: settings.max_output_tokens,
        effort: settings.effort,
        continues,
        cache_key: turn.cache_key.clone(),
    };

    // A chat too long to send whole is summarised, and starts again from
    // the summary: no earlier turn or reasoning is replayed after it.
    if conversation::estimate_tokens(&conversation) > settings.compact_at_tokens {
        let mut asking = conversation.clone();
        asking.extend(conversation::user(wire, SUMMARY_REQUEST));
        let summary = round(
            provider,
            host,
            request(&asking, turn.previous.clone()),
            &mut stats,
            started,
            false,
        )
        .await;
        match summary {
            Ok(summary) if !summary.text.trim().is_empty() => {
                conversation = conversation::user(
                    wire,
                    &format!("{SUMMARY_PREFIX}\n\n{}", summary.text.trim()),
                );
                context_sent = false;
                stats.compacted = true;
            }
            // Sent whole instead; the model's window is far larger than
            // the threshold, which is about cost.
            Ok(_) | Err(_) => {}
        }
    }

    conversation.extend(conversation::user(wire, &turn.text));
    if let Some(context) = &turn.context
        && !context_sent
    {
        conversation.extend(conversation::context(wire, &turn.model, context));
        context_sent = true;
    }

    let mut last: Option<String> = None;
    let mut result = Err(TurnError::TooManyRounds);
    for _ in 0..MAX_ROUNDS {
        let continues = last.clone();
        let answered = round(
            provider,
            host,
            request(&conversation, continues),
            &mut stats,
            started,
            true,
        )
        .await;
        let answered = match answered {
            Ok(answered) => answered,
            Err(error) => {
                result = Err(error.into());
                break;
            }
        };
        last.clone_from(&answered.response_id);
        conversation.extend(answered.output);
        if answered.calls.is_empty() {
            result = Ok(());
            break;
        }
        let results = run_tools(host, &tools, answered.calls, &mut stats).await;
        conversation.extend(conversation::tool_results(wire, &results));
    }
    TurnOutcome {
        result,
        response_id: last,
        conversation,
        context_sent,
        stats,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::FutureExt;

    use super::*;
    use crate::provider::fake::FakeProvider;

    /// Records what the loop does, in order.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Recorder {
        fn log(&self, line: String) {
            self.0.lock().unwrap().push(line);
        }
    }

    impl Host for Recorder {
        fn message_start(&self) -> BoxFuture<'_, String> {
            self.log("start".into());
            async { "m1".to_owned() }.boxed()
        }
        fn text_delta(&self, _: String, delta: String) -> BoxFuture<'_, ()> {
            self.log(format!("text {delta}"));
            async {}.boxed()
        }
        fn message_finish(&self, _: String, response: Option<String>) -> BoxFuture<'_, ()> {
            self.log(format!("done {}", response.unwrap_or_default()));
            async {}.boxed()
        }
        fn tool_start(&self, method: String, _: Value) -> BoxFuture<'_, String> {
            self.log(format!("tool {method}"));
            async { "t1".to_owned() }.boxed()
        }
        fn call(&self, method: String, _: Value) -> BoxFuture<'_, Result<Value, Value>> {
            self.log(format!("call {method}"));
            async { Ok(json!({ "vehicles": [] })) }.boxed()
        }
        fn tool_finish(&self, _: String, outcome: Result<Value, Value>) -> BoxFuture<'_, ()> {
            self.log(format!("result {}", outcome.is_ok()));
            async {}.boxed()
        }
    }

    fn turn(text: &str) -> Turn {
        Turn {
            model: "fake".into(),
            text: text.into(),
            conversation: Vec::new(),
            context: Some("This thread is about Copter 1.".into()),
            context_sent: false,
            previous: None,
            cache_key: None,
        }
    }

    #[test]
    fn a_tool_round_then_an_answer_and_the_conversation_to_keep() {
        let host = Recorder::default();
        let outcome = run_turn(&FakeProvider::new(), &host, turn("what vehicles do I have"))
            .now_or_never()
            .unwrap();
        assert_eq!(outcome.result, Ok(()));
        let log = host.0.lock().unwrap().clone();
        assert_eq!(
            &log[..4],
            [
                "tool vehicle/list",
                "call vehicle/list",
                "result true",
                "start"
            ]
        );
        assert!(log.last().unwrap().starts_with("done fake_resp_2"));
        assert_eq!(outcome.stats.rounds, 2);
        assert_eq!(outcome.stats.tool_calls, 1);
        assert!(outcome.context_sent);
        let kinds: Vec<String> = outcome
            .conversation
            .iter()
            .map(|item| {
                item["role"]
                    .as_str()
                    .or_else(|| item["type"].as_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "user",
                "developer",
                "function_call",
                "function_call_output",
                "assistant"
            ],
            "the message, the context, the call, its result and the answer, in order"
        );
    }

    #[test]
    fn a_long_chat_is_summarised_and_starts_again_from_the_summary() {
        let host = Recorder::default();
        let mut long = turn("hello");
        long.conversation = vec![json!({ "role": "user", "content": "x".repeat(500_000) })];
        long.context_sent = true;
        let outcome = run_turn(&FakeProvider::new(), &host, long)
            .now_or_never()
            .unwrap();
        assert!(outcome.stats.compacted);
        assert!(
            outcome.conversation[0]["content"]
                .as_str()
                .unwrap()
                .starts_with(SUMMARY_PREFIX)
        );
        assert_eq!(
            outcome.conversation[2]["role"], "developer",
            "the context is sent again after a summary"
        );
    }

    #[test]
    fn a_deferred_tool_runs_by_name_and_tool_search_finds_uxduck_s_own() {
        let host = Recorder::default();
        let mut stats = TurnStats::default();
        let results = run_tools(
            &host,
            &tools(),
            vec![
                (
                    "c1".into(),
                    TOOL_CALL.into(),
                    r#"{"name":"plan_upload","params":{"planId":"p1","vehicleId":"v1"}}"#.into(),
                ),
                (
                    "c2".into(),
                    "tool_search".into(),
                    r#"{"query":"mission history"}"#.into(),
                ),
                (
                    "c3".into(),
                    TOOL_CALL.into(),
                    r#"{"name":"no_such_tool"}"#.into(),
                ),
            ],
            &mut stats,
        )
        .now_or_never()
        .unwrap();
        let log = host.0.lock().unwrap().clone();
        assert!(log.contains(&"call plan/upload".to_owned()), "{log:?}");
        assert!(log.contains(&"call tool/search".to_owned()), "{log:?}");
        // Not sent each turn, yet found and named for tool_call.
        assert!(!model_tools().iter().any(|t| t.name == "mission_history"));
        let found: Value = serde_json::from_str(&results[1].output).unwrap();
        assert_eq!(
            found["uxduckTools"][0]["name"], "mission_history",
            "{found}"
        );
        assert!(results[2].failed);
        assert!(results[2].output.contains("tool_search"));
    }

    #[test]
    fn a_long_tool_result_is_cut_with_a_note() {
        let cut = capped("é".repeat(MAX_TOOL_OUTPUT_CHARS + 5));
        assert!(cut.ends_with(
            "[cut: 5 more characters. Ask for less, such as one vehicle or fewer fields.]"
        ));
        assert_eq!(capped("short".into()), "short");
    }
}

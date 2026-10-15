//! A scripted provider for development, tests and CI: no network and no key,
//! the same stream shape as a real one, speaking the Responses API's items.
//! It calls `vehicle_list` when asked about vehicles and answers from the
//! result, and searches and reads parameters when asked why it climbs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{Value, json};

pub use super::Sleep;
use super::{Provider, ProviderError, ProviderEvent, ProviderRequest, Usage};
use crate::conversation::Wire;

/// What the latest request added: the operator's message or tool results.
enum Input {
    UserText(String),
    ToolOutput { call_id: String, output: String },
}

/// The items after the model's last output: what this request is about.
fn latest(conversation: &[Value]) -> Vec<Input> {
    let start = conversation
        .iter()
        .rposition(|item| item["role"] == "assistant" || item["type"] == "function_call")
        .map_or(0, |i| i + 1);
    conversation[start..]
        .iter()
        .filter_map(|item| {
            if item["type"] == "function_call_output" {
                Some(Input::ToolOutput {
                    call_id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    output: item["output"].as_str().unwrap_or_default().to_owned(),
                })
            } else if item["role"] == "user" {
                item["content"]
                    .as_str()
                    .map(|text| Input::UserText(text.to_owned()))
            } else {
                None
            }
        })
        .collect()
}

pub struct FakeProvider {
    responses: AtomicU64,
    sleep: Option<(Sleep, Duration)>,
}

impl std::fmt::Debug for FakeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeProvider")
            .field("responses", &self.responses)
            .finish_non_exhaustive()
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: AtomicU64::new(0),
            sleep: None,
        }
    }

    /// Waits `every` between words.
    #[must_use]
    pub fn with_delay(mut self, sleep: Sleep, every: Duration) -> Self {
        self.sleep = Some((sleep, every));
        self
    }

    fn answer(input: &[Input]) -> Result<Vec<ProviderEvent>, String> {
        if let Some(events) = Self::attached(input) {
            return Ok(events);
        }
        if let Some(events) = Self::plugin(input)? {
            return Ok(events);
        }
        for respond in [
            Self::writes,
            Self::parameters,
            Self::history,
            Self::instruments,
            Self::suggestion,
        ] {
            if let Some(events) = respond(input)? {
                return Ok(events);
            }
        }
        for item in input {
            // Asked to land: the vehicles came back; land the first.
            if let Input::ToolOutput { call_id, output } = item
                && call_id == LAND_LIST
            {
                let vehicles: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
                let Some(vehicle) = vehicles["vehicles"].as_array().and_then(|l| l.first()) else {
                    return Ok(vec![ProviderEvent::TextDelta(
                        "There is no vehicle to land.".into(),
                    )]);
                };
                return Ok(vec![ProviderEvent::ToolCall {
                    call_id: LAND_SEND.into(),
                    name: "command_send".into(),
                    arguments: serde_json::json!({
                        "vehicleId": vehicle["id"],
                        "command": { "type": "land" }
                    })
                    .to_string(),
                }]);
            }
            if let Input::ToolOutput { call_id, output } = item
                && call_id == LAND_SEND
            {
                let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
                let text = match answer["status"].as_str() {
                    Some(status) => format!(
                        "I asked it to land: {status}{}.",
                        answer["detail"]
                            .as_str()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default()
                    ),
                    None => format!(
                        "I could not send the landing command: {}",
                        answer["message"].as_str().unwrap_or("it was refused")
                    ),
                };
                return Ok(vec![ProviderEvent::TextDelta(text)]);
            }
            if let Input::ToolOutput { call_id, output } = item {
                let vehicles: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
                let described: Vec<String> = vehicles["vehicles"]
                    .as_array()
                    .map(|list| list.iter().map(describe).collect())
                    .unwrap_or_default();
                let _ = call_id;
                let text = if described.is_empty() {
                    "You have no vehicles yet. Add a link to one and it will appear here."
                        .to_owned()
                } else {
                    format!(
                        "You have {} vehicle(s): {}.",
                        described.len(),
                        described.join("; ")
                    )
                };
                return Ok(vec![ProviderEvent::TextDelta(text)]);
            }
        }
        let asked = input.iter().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        if asked.as_deref().is_some_and(|text| text.contains("land")) {
            return Ok(vec![ProviderEvent::ToolCall {
                call_id: LAND_LIST.into(),
                name: "vehicle_list".into(),
                arguments: "{}".into(),
            }]);
        }
        if asked
            .as_deref()
            .is_some_and(|text| text.contains("vehicle"))
        {
            return Ok(vec![ProviderEvent::ToolCall {
                call_id: "call_fake_1".into(),
                name: "vehicle_list".into(),
                arguments: "{}".into(),
            }]);
        }
        Ok(vec![ProviderEvent::TextDelta(
            "This is the development agent. Ask me what vehicles you have.".into(),
        )])
    }
}

impl FakeProvider {
    /// Asked for the distance from home in the panel (S58): draft a
    /// plugin, test it, and say what it produced; asked to also warn below
    /// 100 m, a new version; asked to install it, list the drafts and
    /// install the newest, which waits for the operator.
    fn writes(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            let Input::ToolOutput { call_id, output } = item else {
                continue;
            };
            let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
            let failed = answer["message"].as_str();
            match call_id.as_str() {
                DRAFT => {
                    if let Some(why) = failed {
                        return Ok(Some(vec![ProviderEvent::TextDelta(format!(
                            "The draft did not check: {why}"
                        ))]));
                    }
                    return Ok(Some(vec![ProviderEvent::ToolCall {
                        call_id: DRAFT_TEST.into(),
                        name: "plugin_test".into(),
                        arguments: json!({ "id": answer["id"] }).to_string(),
                    }]));
                }
                DRAFT_TEST => {
                    let text = failed.map_or_else(
                        || tested_text(&answer),
                        |why| format!("The test did not run: {why}"),
                    );
                    return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
                }
                DRAFTS => {
                    let Some(path) = answer["drafts"][0]["path"].as_str() else {
                        return Ok(Some(vec![ProviderEvent::TextDelta(
                            "There is no draft to install.".into(),
                        )]));
                    };
                    return Ok(Some(vec![ProviderEvent::ToolCall {
                        call_id: INSTALL.into(),
                        name: "plugin_install".into(),
                        arguments: json!({ "path": path }).to_string(),
                    }]));
                }
                INSTALL => {
                    let text = match failed {
                        Some(why) => format!("It was not installed: {why}"),
                        None => format!(
                            "Installed {} {}.",
                            answer["name"].as_str().unwrap_or("it"),
                            answer["version"].as_str().unwrap_or("")
                        ),
                    };
                    return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
                }
                _ => {}
            }
        }
        let asked = input.iter().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        let Some(asked) = asked else {
            return Ok(None);
        };
        let draft = |warn: bool| ProviderEvent::ToolCall {
            call_id: DRAFT.into(),
            name: "plugin_draft".into(),
            arguments: json!({
                "id": "distance-from-home",
                "name": "Distance from home",
                "script": home_script(warn),
                "subscribe": { "messages": { "names": ["GLOBAL_POSITION_INT"], "cadenceMs": 500 } }
            })
            .to_string(),
        };
        Ok(if asked.contains("install") {
            Some(vec![ProviderEvent::ToolCall {
                call_id: DRAFTS.into(),
                name: "plugin_drafts".into(),
                arguments: "{}".into(),
            }])
        } else if asked.contains("warn below") {
            Some(vec![draft(true)])
        } else if asked.contains("distance from home") {
            Some(vec![draft(false)])
        } else {
            None
        })
    }

    /// Asked for a photo (S57): list the vehicles, search the plugins'
    /// commands, run the first found on the first vehicle, and say what
    /// came of it. The vehicle rides in the search's call id.
    fn plugin(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            let Input::ToolOutput { call_id, output } = item else {
                continue;
            };
            let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
            if call_id == PHOTO_LIST {
                let Some(vehicle) = answer["vehicles"][0]["id"].as_str() else {
                    return Ok(Some(vec![ProviderEvent::TextDelta(
                        "There is no vehicle to take a photo with.".into(),
                    )]));
                };
                return Ok(Some(vec![ProviderEvent::ToolCall {
                    call_id: format!("{PHOTO_SEARCH}{vehicle}"),
                    name: "tool_search".into(),
                    arguments: json!({ "query": "photo" }).to_string(),
                }]));
            }
            if let Some(vehicle) = call_id.strip_prefix(PHOTO_SEARCH) {
                let Some(tool) = answer["tools"].as_array().and_then(|t| t.first()) else {
                    return Ok(Some(vec![ProviderEvent::TextDelta(
                        "No installed plugin takes photos.".into(),
                    )]));
                };
                return Ok(Some(vec![ProviderEvent::ToolCall {
                    call_id: format!("{PHOTO_CALL}{}", tool["pluginName"].as_str().unwrap_or("a")),
                    name: "plugin_call".into(),
                    arguments: json!({
                        "pluginId": tool["pluginId"],
                        "command": tool["command"],
                        "vehicleId": vehicle,
                    })
                    .to_string(),
                }]));
            }
            if let Some(plugin) = call_id.strip_prefix(PHOTO_CALL) {
                let text = match answer["message"].as_str() {
                    Some(why) => format!("The {plugin} plugin could not take it: {why}"),
                    None => format!("I asked the {plugin} plugin to take a photo."),
                };
                return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
            }
        }
        let asked = input.iter().any(
            |item| matches!(item, Input::UserText(text) if text.to_lowercase().contains("photo")),
        );
        Ok(asked.then(|| {
            vec![ProviderEvent::ToolCall {
                call_id: PHOTO_LIST.into(),
                name: "vehicle_list".into(),
                arguments: "{}".into(),
            }]
        }))
    }

    /// A message with chips (ADR-0020 H2): answers from the first one, so a
    /// test sees what reached the model.
    fn attached(input: &[Input]) -> Option<Vec<ProviderEvent>> {
        input.iter().find_map(|item| {
            let Input::UserText(text) = item else {
                return None;
            };
            let (_, attached) = text.split_once("\n\nAttached by the operator:\n- ")?;
            let first = attached.lines().next()?;
            let what = first.split_once(". ").map_or(first, |(what, _)| what);
            Some(vec![ProviderEvent::TextDelta(format!(
                "From what you attached: {what}."
            ))])
        })
    }

    /// Asked why it climbs before returning (S68): search the parameters,
    /// read the best match, and answer from its definition, naming the set.
    fn parameters(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            let Input::ToolOutput { call_id, output } = item else {
                continue;
            };
            let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
            if call_id == PARAM_SEARCH {
                let Some(name) = answer["hits"][0]["name"].as_str() else {
                    return Ok(Some(vec![ProviderEvent::TextDelta(
                        "I found no parameter for that.".into(),
                    )]));
                };
                return Ok(Some(vec![ProviderEvent::ToolCall {
                    call_id: PARAM_GET.into(),
                    name: "param_get".into(),
                    arguments: json!({ "names": [name] }).to_string(),
                }]));
            }
            if call_id == PARAM_GET {
                let parameter = &answer["parameters"][0];
                let name = parameter["name"].as_str().unwrap_or("It");
                let value = parameter["value"]
                    .as_f64()
                    .map_or_else(|| "not reported".to_owned(), |v| v.to_string());
                let text = match parameter.get("definition") {
                    Some(definition) => format!(
                        "{name} is {value}{}: {}. {} (From {}.)",
                        definition["units"]
                            .as_str()
                            .map(|u| format!(" {u}"))
                            .unwrap_or_default(),
                        definition["displayName"].as_str().unwrap_or(name),
                        definition["description"].as_str().unwrap_or_default(),
                        answer["describedBy"]["definitions"]["title"]
                            .as_str()
                            .unwrap_or("its set"),
                    ),
                    None => format!(
                        "{name} is {value}, but this vehicle's set does not describe it, so I cannot say what it does."
                    ),
                };
                return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
            }
        }
        let asked = input.iter().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        Ok(asked.is_some_and(|text| text.contains("climb")).then(|| {
            vec![ProviderEvent::ToolCall {
                call_id: PARAM_SEARCH.into(),
                name: "param_search".into(),
                arguments: json!({ "query": "return altitude", "limit": 3 }).to_string(),
            }]
        }))
    }
}

impl FakeProvider {
    /// Asked whether anything was done to a vehicle recently (S69): read its
    /// history and cite the newest note with its date.
    fn history(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            let Input::ToolOutput { call_id, output } = item else {
                continue;
            };
            if call_id != HISTORY {
                continue;
            }
            let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
            let note = answer["entries"]
                .as_array()
                .and_then(|all| all.iter().find(|e| e["kind"]["type"] == "note"));
            let text = match note {
                Some(note) => {
                    let days = note["happenedAtUs"].as_u64().unwrap_or_default() / 86_400_000_000;
                    format!(
                        "Yes: “{}”, noted for day {days} since 1970.",
                        note["kind"]["text"].as_str().unwrap_or_default()
                    )
                }
                None => "Its history has no notes, only what UXDuck recorded.".to_owned(),
            };
            return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
        }
        let asked = input.iter().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        Ok(asked
            .is_some_and(|text| text.contains("recently"))
            .then(|| {
                vec![ProviderEvent::ToolCall {
                    call_id: HISTORY.into(),
                    name: "vehicle_history".into(),
                    arguments: json!({ "limit": 10 }).to_string(),
                }]
            }))
    }
}

impl FakeProvider {
    /// Asked to show the instruments (S43): open their tab, and say so.
    fn instruments(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            if let Input::ToolOutput { call_id, output } = item
                && call_id == INSTRUMENTS
            {
                let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
                let text = answer["message"].as_str().map_or_else(
                    || "The instruments are open beside the map.".to_owned(),
                    |why| format!("I could not open them: {why}"),
                );
                return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
            }
        }
        let asked = input.iter().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        Ok(asked
            .filter(|text| text.contains("instruments"))
            .map(|text| {
                vec![ProviderEvent::ToolCall {
                    call_id: INSTRUMENTS.into(),
                    name: "layout_open".into(),
                    // Instruments (S77), or the classic ones when asked (S43).
                    arguments: json!({
                        "kind": if text.contains("classic") { "instruments" } else { "flight-display" }
                    })
                    .to_string(),
                }]
            }))
    }
}

impl FakeProvider {
    /// Asked for airspeed on a rover (S73): offer to suggest it; told to
    /// suggest it, write the card and say it waits for them.
    fn suggestion(input: &[Input]) -> Result<Option<Vec<ProviderEvent>>, String> {
        for item in input {
            if let Input::ToolOutput { call_id, output } = item
                && call_id == SUGGEST
            {
                let answer: Value = serde_json::from_str(output).map_err(|e| e.to_string())?;
                let text = answer["message"].as_str().map_or_else(
                    || {
                        "Here it is: read it, change anything, and send it if you want to."
                            .to_owned()
                    },
                    |why| format!("I could not write it up: {why}"),
                );
                return Ok(Some(vec![ProviderEvent::TextDelta(text)]));
            }
        }
        let asked = input.iter().rev().find_map(|item| match item {
            Input::UserText(text) => Some(text.to_lowercase()),
            Input::ToolOutput { .. } => None,
        });
        Ok(match asked {
            Some(text) if text.contains("suggest it") => Some(vec![ProviderEvent::ToolCall {
                call_id: SUGGEST.into(),
                name: "feedback_suggest".into(),
                arguments: json!({
                    "title": "Airspeed for rovers",
                    "text": "I want to see airspeed in a rover's Telemetry, like a plane's.\nWhy: wind matters to my rover in the open."
                })
                .to_string(),
            }]),
            Some(text) if text.contains("airspeed") && text.contains("rover") => {
                Some(vec![ProviderEvent::TextDelta(
                    "UXDuck does not show airspeed for rovers yet. Would you like me to suggest it to the UXDuck team?".into(),
                )])
            }
            _ => None,
        })
    }
}

/// The tool call id of a suggestion card.
const SUGGEST: &str = "call_suggest";

/// The tool call id of a request to show the instruments.
const INSTRUMENTS: &str = "call_instruments";

/// The script the fake agent drafts: the distance from home, and with
/// `warn` a notice once it is within 100 m.
fn home_script(warn: bool) -> String {
    let notice = if warn {
        r#"
    if (d < 100 && !warned.has(m.vehicleId)) {
      warned.add(m.vehicleId);
      notifications.push(notify(m.vehicleId, "warning", "Within 100 m of home"));
    }"#
    } else {
        ""
    };
    format!(
        r#"import type {{ Description, Effects, MessageBatch, PluginNotification, ProducedItem }} from "@uxduck/sdk";
import {{ notify, value }} from "@uxduck/sdk";

const home = new Map<string, [number, number]>();
const warned = new Set<string>();

function distance(a: [number, number], b: [number, number]): number {{
  const rad = (d: number) => (d * Math.PI) / 180;
  const h = Math.sin(rad(b[0] - a[0]) / 2) ** 2 +
    Math.cos(rad(a[0])) * Math.cos(rad(b[0])) * Math.sin(rad(b[1] - a[1]) / 2) ** 2;
  return 2 * 6371000 * Math.asin(Math.sqrt(h));
}}

export function describe(): Description {{
  return {{ panels: [{{ id: "home", title: "Distance from home", widgets: [
    {{ type: "value", item: "home.distance_m", label: "From home" }},
  ] }}] }};
}}

export function onMessages(batch: MessageBatch): Effects {{
  const items: ProducedItem[] = [];
  const notifications: PluginNotification[] = [];
  void warned;
  for (const m of batch.messages) {{
    const here: [number, number] = [(m.fields.lat as number) / 1e7, (m.fields.lon as number) / 1e7];
    const start = home.get(m.vehicleId) ?? here;
    home.set(m.vehicleId, start);
    const d = distance(start, here);
    items.push(value(m.vehicleId, "home.distance_m", "From home", "meter", d));{notice}
  }}
  return {{ items, notifications }};
}}
"#
    )
}

/// What a draft's test produced, for the operator.
fn tested_text(answer: &Value) -> String {
    let items: Vec<String> = answer["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            format!(
                "{} ({}) {:.0}",
                i["label"].as_str().unwrap_or(""),
                i["key"].as_str().unwrap_or(""),
                i["value"].as_f64().unwrap_or(0.0)
            )
        })
        .collect();
    format!(
        "I drafted it and tested it against the last {:.0} s: it produced {}{}. Say \"install it\" to add it.",
        answer["flightS"].as_f64().unwrap_or(0.0),
        if items.is_empty() {
            "nothing".to_owned()
        } else {
            items.join(", ")
        },
        answer["notifications"]
            .as_array()
            .filter(|n| !n.is_empty())
            .map(|n| format!(", and {} notice(s)", n.len()))
            .unwrap_or_default()
    )
}

/// Tool call ids that carry a plugin's writing across turns (S58).
const DRAFT: &str = "call_draft";
const DRAFT_TEST: &str = "call_draft_test";
const DRAFTS: &str = "call_drafts";
const INSTALL: &str = "call_install";

/// Tool call ids that carry a photo request across turns (S57).
const PHOTO_LIST: &str = "call_photo_list";
const PHOTO_SEARCH: &str = "call_photo_search:";
const PHOTO_CALL: &str = "call_photo_call:";

/// The tool call id of a history question.
const HISTORY: &str = "call_history";

/// Tool call ids that carry a parameter question across turns.
const PARAM_SEARCH: &str = "call_param_search";
const PARAM_GET: &str = "call_param_get";

/// Tool call ids that carry the intent across turns of a "land" request.
const LAND_LIST: &str = "call_land_list";
const LAND_SEND: &str = "call_land_send";

/// "Copter 3, a Quadrotor on Ardupilotmega 4.5.1, online".
fn describe(vehicle: &Value) -> String {
    let text = |key: &str| vehicle[key].as_str().unwrap_or("unknown");
    let firmware = vehicle["firmware"]
        .as_str()
        .map(|f| format!(" {f}"))
        .unwrap_or_default();
    format!(
        "{}, a {} on {}{firmware}, {}",
        text("name"),
        text("vehicleType"),
        text("autopilot"),
        text("presence")
    )
}

impl Provider for FakeProvider {
    fn wire(&self) -> Wire {
        Wire::OpenAi
    }

    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let n = self.responses.fetch_add(1, Ordering::SeqCst) + 1;
        let events = match Self::answer(&latest(&request.conversation)) {
            Ok(events) => events,
            Err(error) => {
                return stream::once(async move { Err(ProviderError::Refused(error)) }).boxed();
            }
        };
        // Split text into words so it streams, and give back the items a
        // real provider would, to replay.
        let mut chunks: Vec<ProviderEvent> = Vec::new();
        let mut output = Vec::new();
        for event in events {
            match event {
                ProviderEvent::TextDelta(text) => {
                    output.push(json!({ "role": "assistant", "content": text }));
                    chunks.extend(
                        text.split_inclusive(' ')
                            .map(|word| ProviderEvent::TextDelta(word.to_owned())),
                    );
                }
                ProviderEvent::ToolCall {
                    call_id,
                    name,
                    arguments,
                } => {
                    output.push(json!({
                        "type": "function_call", "call_id": call_id, "name": name, "arguments": arguments,
                    }));
                    chunks.push(ProviderEvent::ToolCall {
                        call_id,
                        name,
                        arguments,
                    });
                }
                other => chunks.push(other),
            }
        }
        chunks.push(ProviderEvent::Completed {
            response_id: format!("fake_resp_{n}"),
            output,
            usage: Usage::default(),
        });
        let sleep = self.sleep.clone();
        stream::iter(chunks)
            .then(move |event| {
                let sleep = sleep.clone();
                async move {
                    if let (Some((sleep, every)), ProviderEvent::TextDelta(_)) = (&sleep, &event) {
                        sleep(*every).await;
                    }
                    Ok(event)
                }
            })
            .boxed()
    }
}

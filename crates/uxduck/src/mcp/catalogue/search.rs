//! Rank purpose and action words, never arbitrary nested configuration fields.

use std::collections::BTreeSet;

use serde_json::Value;
use uxduck_agent::tools::Tool;

fn words(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .replace("pre-arm", "prearm")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .filter(|word| {
            !matches!(
                *word,
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "from"
                    | "this"
                    | "that"
                    | "what"
                    | "which"
                    | "why"
                    | "how"
                    | "can"
                    | "could"
                    | "would"
                    | "should"
                    | "will"
                    | "won"
                    | "not"
                    | "does"
                    | "is"
                    | "are"
                    | "my"
                    | "me"
                    | "it"
                    | "its"
                    | "of"
                    | "to"
                    | "in"
                    | "on"
                    | "at"
                    | "an"
                    | "as"
                    | "be"
                    | "do"
                    | "please"
                    | "show"
                    | "read"
                    | "get"
                    | "need"
                    | "needs"
                    | "result"
                    | "uxduck"
            )
        })
        .map(|word| match word {
            "arming" | "armed" => "arm".into(),
            "preflight" => "prearm".into(),
            "vehicles" => "vehicle".into(),
            "batteries" => "battery".into(),
            _ => word
                .strip_suffix('s')
                .filter(|stem| stem.len() > 3 && !word.ends_with("ss"))
                .unwrap_or(word)
                .to_owned(),
        })
        .collect()
}

// Enum/const values contain actions and patterns (arm, survey, zoom).
// Ignore free prose and arbitrary argument names, including preferences.
fn actions(schema: &Value, found: &mut BTreeSet<String>) {
    match schema {
        Value::Object(fields) => {
            if let Some(value) = fields.get("const").and_then(Value::as_str) {
                found.extend(words(value));
            }
            if let Some(values) = fields.get("enum").and_then(Value::as_array) {
                for value in values.iter().filter_map(Value::as_str) {
                    found.extend(words(value));
                }
            }
            for value in fields.values() {
                actions(value, found);
            }
        }
        Value::Array(values) => {
            for value in values {
                actions(value, found);
            }
        }
        _ => {}
    }
}

pub(super) fn ranked<'a>(catalogue: &'a [Tool], query: &str) -> Vec<&'a Tool> {
    let mut query_words = words(query);
    // Failed arming is a request for pre-arm evidence, not an arm action.
    if query_words.contains("arm")
        && query
            .split(|c: char| !c.is_alphanumeric() && c != '\'')
            .any(|word| {
                matches!(
                    word,
                    "why" | "not" | "won't" | "can't" | "cannot" | "fail" | "failed" | "unable"
                )
            })
    {
        query_words.insert("prearm".into());
    }
    let documents: Vec<_> = catalogue
        .iter()
        .map(|tool| {
            let mut action = BTreeSet::new();
            actions(&tool.parameters, &mut action);
            // The appended plugin-authoring guide is useful after discovery,
            // but is not the purpose of the draft-writing operation.
            let purpose = tool.description.split("\n\n").next().unwrap_or_default();
            (tool, words(&tool.name), words(purpose), action)
        })
        .collect();
    let weighted: Vec<_> = query_words
        .iter()
        .map(|word| {
            let frequency = documents
                .iter()
                .filter(|(_, n, d, a)| n.contains(word) || d.contains(word) || a.contains(word))
                .count();
            (word, 1_000 / (frequency + 1))
        })
        .collect();
    let mut scored: Vec<_> = documents
        .iter()
        .filter_map(|(tool, name, about, action)| {
            let mut score = 0;
            let mut matched = 0;
            for (word, specificity) in &weighted {
                let weight = 8 * usize::from(name.contains(*word))
                    + 4 * usize::from(about.contains(*word))
                    + usize::from(action.contains(*word));
                matched += usize::from(weight > 0);
                score += specificity * weight;
            }
            let read = uxduck_agent::tools::mcp::annotations(&tool.method)["readOnlyHint"] == true;
            (score > 0).then_some(((matched, score, read), *tool))
        })
        .collect();
    // Match the whole request before weighting its individual words: "flight
    // logs" must not be beaten by a tool that only has "logs" in its name.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, tool)| tool).collect()
}

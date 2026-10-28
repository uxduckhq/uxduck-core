//! `uxduck checklist` (S193, ADR-0038 K7 to K11): a vehicle's own
//! checklist from a terminal: list it, add a manual step or a rule, tick
//! and clear, export it as a file and import a file or a Markdown list,
//! copy another vehicle's, and set the parameter that makes the vehicle
//! hold a rule itself.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Args)]
pub struct ChecklistArgs {
    /// The vehicle, by name or id.
    pub vehicle: String,
    /// Print the checklist (or the file) as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: Option<ChecklistAction>,
}

#[derive(Debug, Subcommand)]
pub enum ChecklistAction {
    /// The checklist, one line per step: what it is, whether it passes,
    /// and who ticked it when (the default).
    List,
    /// Adds a step: a manual one ("Propellers tight"), or a rule with
    /// `--rule "battery.voltage_v >= 22.2"`.
    Add {
        /// What the step says.
        title: String,
        /// A rule: an item or parameter, an operator and a value, such as
        /// `battery.cell_v >= 3.8` or `ARMING_CHECK == 1`.
        #[arg(long, value_name = "RULE")]
        rule: Option<String>,
        /// Seconds the rule's value must hold before it passes.
        #[arg(long, requires = "rule", value_name = "SECONDS")]
        hold: Option<f64>,
        /// Only warn: the step never stops arming.
        #[arg(long)]
        warn: bool,
    },
    /// Removes a step, by its number, id or title.
    Remove { step: String },
    /// Ticks a manual step, by its number, id or title.
    Tick {
        step: String,
        /// Clears its tick instead.
        #[arg(long)]
        clear: bool,
    },
    /// Clears every tick.
    Reset,
    /// Writes the checklist as a file (`uxduck.checklist/1`), without
    /// ticks; to standard output without a path.
    Export { path: Option<PathBuf> },
    /// Copies a checklist file, or a Markdown or text list (one step a
    /// line), into the vehicle's checklist; `-` reads standard input.
    Import {
        path: PathBuf,
        /// Replaces the vehicle's steps instead of adding after them.
        #[arg(long)]
        replace: bool,
    },
    /// Copies another vehicle's checklist: a copy, not linked.
    Copy {
        /// The vehicle to copy from, by name or id.
        #[arg(long)]
        from: String,
        #[arg(long)]
        replace: bool,
    },
    /// Sets the parameter that makes the vehicle hold a rule itself
    /// (`BATT_ARM_VOLT` for a battery rule), from the rule's step.
    SetParam { step: String },
}

#[allow(
    clippy::too_many_lines,
    reason = "one match arm per checklist subcommand"
)]
pub async fn run(data_dir: &Path, args: ChecklistArgs) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let id = super::vehicle::resolve(&mut client, &args.vehicle).await?;
    let state = client
        .call("preflight/state", json!({ "vehicleId": id }))
        .await?;
    let state = match args.action.unwrap_or(ChecklistAction::List) {
        ChecklistAction::List => state,
        ChecklistAction::Add {
            title,
            rule,
            hold,
            warn,
        } => {
            let step = new_step(&title, rule.as_deref(), hold, warn)?;
            client
                .call(
                    "checklist/update",
                    json!({ "vehicleId": id, "add": [step] }),
                )
                .await?
        }
        ChecklistAction::Remove { step } => {
            let step = find(&state, &step)?;
            client
                .call(
                    "checklist/update",
                    json!({ "vehicleId": id, "remove": [step["id"]] }),
                )
                .await?
        }
        ChecklistAction::Tick { step, clear } => {
            let step = find(&state, &step)?;
            client
                .call(
                    "checklist/tick",
                    json!({ "vehicleId": id, "stepId": step["id"], "ticked": !clear }),
                )
                .await?
        }
        ChecklistAction::Reset => {
            client
                .call("checklist/reset", json!({ "vehicleId": id }))
                .await?
        }
        ChecklistAction::Export { path } => {
            let file = client
                .call("checklist/export", json!({ "vehicleId": id }))
                .await?;
            let text = serde_json::to_string_pretty(&file).unwrap_or_default();
            match path {
                Some(path) => {
                    std::fs::write(&path, format!("{text}\n")).map_err(|e| {
                        CliError::Usage(format!("couldn't write {}: {e}", path.display()))
                    })?;
                    eprintln!("Wrote {}.", path.display());
                }
                None => println!("{text}"),
            }
            return Ok(());
        }
        ChecklistAction::Import { path, replace } => {
            let (text, name) = if path.as_os_str() == "-" {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                    .map_err(|e| CliError::Usage(format!("couldn't read the list: {e}")))?;
                (text, None)
            } else {
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    CliError::Usage(format!("couldn't read {}: {e}", path.display()))
                })?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                (text, name)
            };
            let mut params = Map::new();
            params.insert("vehicleId".into(), json!(id));
            match serde_json::from_str::<Value>(&text) {
                Ok(file) if file.is_object() => params.insert("file".into(), file),
                _ => params.insert("markdown".into(), json!(text)),
            };
            if let Some(name) = name {
                params.insert("sourceName".into(), json!(name));
            }
            if replace {
                params.insert("replace".into(), json!(true));
            }
            client.call("checklist/copy", Value::Object(params)).await?
        }
        ChecklistAction::Copy { from, replace } => {
            let from = super::vehicle::resolve(&mut client, &from).await?;
            client
                .call(
                    "checklist/copy",
                    json!({ "vehicleId": id, "fromVehicleId": from, "replace": replace }),
                )
                .await?
        }
        ChecklistAction::SetParam { step } => {
            let step = find(&state, &step)?;
            let offer = state["checklist"]["offers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["stepId"] == step["id"])
                .cloned()
                .ok_or_else(|| {
                    CliError::Usage(format!(
                        "the vehicle has no parameter that holds “{}”",
                        step["title"].as_str().unwrap_or_default()
                    ))
                })?;
            let reason = format!("Checklist: {}", step["title"].as_str().unwrap_or_default());
            client
                .call(
                    "param/set",
                    json!({ "vehicleId": id, "name": offer["name"], "value": offer["value"], "reason": reason }),
                )
                .await?;
            eprintln!(
                "Set {} to {}.",
                offer["name"].as_str().unwrap_or_default(),
                offer["value"]
            );
            client
                .call("preflight/state", json!({ "vehicleId": id }))
                .await?
        }
    };
    if args.json {
        println!("{}", state["checklist"]);
    } else {
        print!("{}", lines(&state));
    }
    Ok(())
}

/// A step by its number in the list (from 1), its id, or its title.
fn find(state: &Value, wanted: &str) -> Result<Value, CliError> {
    let steps = state["checklist"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Ok(n) = wanted.parse::<usize>()
        && let Some(step) = n.checked_sub(1).and_then(|i| steps.get(i))
    {
        return Ok(step.clone());
    }
    steps
        .iter()
        .find(|s| s["id"] == wanted)
        .or_else(|| {
            steps.iter().find(|s| {
                s["title"]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case(wanted.trim()))
            })
        })
        .cloned()
        .ok_or_else(|| CliError::Usage(format!("no step “{wanted}” in the checklist")))
}

/// A step to add, from the command line.
fn new_step(
    title: &str,
    rule: Option<&str>,
    hold: Option<f64>,
    warn: bool,
) -> Result<Value, CliError> {
    let severity = if warn { "warn" } else { "block" };
    let Some(rule) = rule else {
        return Ok(json!({ "kind": "manual", "title": title, "severity": severity }));
    };
    let (reads, op, value) = parse_rule(rule)?;
    let mut step =
        json!({ "kind": "rule", "title": title, "severity": severity, "op": op, "value": value });
    if reads.contains('.') {
        step["item"] = json!(reads);
    } else {
        step["param"] = json!(reads);
    }
    if let Some(hold) = hold {
        step["holdS"] = json!(hold);
    }
    Ok(step)
}

/// `battery.voltage_v >= 22.2` into what it reads, its operator and value.
fn parse_rule(rule: &str) -> Result<(String, &'static str, f64), CliError> {
    const OPS: [&str; 6] = [">=", "<=", "==", "!=", ">", "<"];
    let usage = || {
        CliError::Usage(format!(
            "--rule takes an item or parameter, an operator and a number, such as \"battery.voltage_v >= 22.2\", not “{rule}”"
        ))
    };
    let (at, op) = OPS
        .iter()
        .filter_map(|op| rule.find(op).map(|at| (at, *op)))
        .min_by_key(|(at, op)| (*at, std::cmp::Reverse(op.len())))
        .ok_or_else(usage)?;
    let reads = rule[..at].trim();
    let number: String = rule[at + op.len()..]
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
        .collect();
    let value = number.parse::<f64>().map_err(|_| usage())?;
    if reads.is_empty() {
        return Err(usage());
    }
    Ok((reads.to_owned(), op, value))
}

fn status_word(status: &str) -> &'static str {
    match status {
        "pass" => "ok",
        "fail" => "FAIL",
        "waiting" => "waiting",
        "unknown" => "unknown",
        "to-do" => "to do",
        _ => "?",
    }
}

/// A time as the terminal reads it, in the computer's time zone.
fn when(us: i64) -> String {
    jiff::Timestamp::from_microsecond(us).map_or_else(
        |_| "?".into(),
        |t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// `battery.voltage_v >= 22.2 V, held 3 s`.
fn rule_words(step: &Value) -> String {
    let reads = step["item"]
        .as_str()
        .or_else(|| step["param"].as_str())
        .unwrap_or_default();
    let mut words = format!(
        "{reads} {} {}",
        step["op"].as_str().unwrap_or_default(),
        number(&step["value"])
    );
    if let Some(unit) = step["unit"].as_str() {
        words.push(' ');
        words.push_str(unit);
    }
    if let Some(hold) = step["holdS"].as_f64() {
        let _ = std::fmt::Write::write_fmt(
            &mut words,
            format_args!(", held {} s", number(&json!(hold))),
        );
    }
    words
}

/// 22.2, 3 (not 3.0).
fn number(value: &Value) -> String {
    match value.as_f64() {
        #[allow(clippy::cast_possible_truncation, reason = "whole numbers only")]
        Some(v) if v.fract() == 0.0 && v.abs() < 1e15 => format!("{}", v as i64),
        Some(v) => format!("{v}"),
        None => "?".into(),
    }
}

/// The checklist as the terminal shows it.
#[must_use]
pub fn lines(state: &Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let steps = state["checklist"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if steps.is_empty() {
        let _ = writeln!(
            out,
            "No checklist yet. Add a step: uxduck checklist <vehicle> add \"Propellers tight\""
        );
        return out;
    }
    if let Some(from) = state["checklist"]["copiedFrom"].as_str() {
        let _ = writeln!(out, "Copied from {from}; changed only here since.");
    }
    let checks = state["checks"].as_array().cloned().unwrap_or_default();
    for (n, step) in steps.iter().enumerate() {
        let id = step["id"].as_str().unwrap_or_default();
        let check = checks
            .iter()
            .find(|c| c["id"] == format!("checklist:{id}"))
            .cloned()
            .unwrap_or(Value::Null);
        let blocks = if step["severity"] == "block" {
            "blocks"
        } else {
            "warns"
        };
        let _ = write!(
            out,
            "{:>2}. {:<8} {:<6} {}",
            n + 1,
            status_word(check["status"].as_str().unwrap_or_default()),
            blocks,
            step["title"].as_str().unwrap_or_default()
        );
        if step["kind"] == "rule" {
            let _ = write!(out, " ({})", rule_words(step));
            if let Some(detail) = check["detail"].as_str() {
                let _ = write!(out, ": {detail}");
            }
        }
        out.push('\n');
        if let Some(tick) = step["tick"].as_object() {
            let by = tick["by"]["operatorName"]
                .as_str()
                .or_else(|| tick["by"]["clientName"].as_str())
                .unwrap_or("someone");
            let at = tick["atUs"].as_i64().map(when).unwrap_or_default();
            let since = if step["flownSince"] == true {
                ", a flight since"
            } else {
                ""
            };
            let _ = writeln!(out, "                    ticked by {by} at {at}{since}");
        }
    }
    for offer in state["checklist"]["offers"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if offer["held"] == true {
            continue;
        }
        let n = steps
            .iter()
            .position(|s| s["id"] == offer["stepId"])
            .map_or(0, |i| i + 1);
        let name = offer["name"].as_str().unwrap_or_default();
        match offer["note"].as_str() {
            Some(note) => {
                let _ = writeln!(out, "{name}: {note}");
            }
            None => {
                let _ = writeln!(
                    out,
                    "The vehicle can hold step {n} itself, from its radio too: {name} is {}; set it to {} with `uxduck checklist <vehicle> set-param {n}`.",
                    number(&offer["current"]),
                    number(&offer["value"])
                );
            }
        }
    }
    let holding: Vec<&str> = checks
        .iter()
        .filter(|c| c["source"] == "checklist" && c["gates"] == true)
        .filter_map(|c| c["title"].as_str())
        .collect();
    if holding.is_empty() {
        let _ = writeln!(out, "Nothing on the checklist holds arming.");
    } else {
        let _ = writeln!(out, "Holding arming: {}.", holding.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_from_the_command_line() {
        assert_eq!(
            parse_rule("battery.voltage_v >= 22.2").unwrap(),
            ("battery.voltage_v".into(), ">=", 22.2)
        );
        assert_eq!(
            parse_rule("ARMING_CHECK==1").unwrap(),
            ("ARMING_CHECK".into(), "==", 1.0)
        );
        assert_eq!(parse_rule("battery.cell_v>3.8V").unwrap().1, ">");
        assert!(parse_rule("battery.voltage_v 22").is_err());
        assert!(parse_rule(">= 22").is_err());
        let step = new_step("Arming checks", Some("ARMING_CHECK == 1"), None, true).unwrap();
        assert_eq!(step["param"], "ARMING_CHECK");
        assert_eq!(step["severity"], "warn");
        let step = new_step(
            "Battery",
            Some("battery.voltage_v >= 22.2"),
            Some(3.0),
            false,
        )
        .unwrap();
        assert_eq!(step["item"], "battery.voltage_v");
        assert_eq!(step["holdS"], 3.0);
    }

    #[test]
    fn the_list_in_lines() {
        let state = json!({
            "checks": [
                { "id": "checklist:a", "source": "checklist", "title": "Propellers tight", "status": "pass", "gates": false },
                { "id": "checklist:b", "source": "checklist", "title": "Battery at least 22.2 V", "status": "fail",
                  "detail": "Battery is below 22.2 V.", "gates": true }
            ],
            "checklist": {
                "steps": [
                    { "id": "a", "kind": "manual", "title": "Propellers tight", "severity": "block",
                      "tick": { "by": { "clientName": "uxduck-web", "operatorName": "Ben" }, "atUs": 0 }, "flownSince": true },
                    { "id": "b", "kind": "rule", "title": "Battery at least 22.2 V", "severity": "block",
                      "item": "battery.voltage_v", "op": ">=", "value": 22.2, "unit": "V", "holdS": 3.0 }
                ],
                "offers": [{ "stepId": "b", "name": "BATT_ARM_VOLT", "value": 22.2, "current": 0.0, "held": false }],
                "version": 3
            }
        });
        let text = lines(&state);
        assert!(
            text.contains(" 1. ok       blocks Propellers tight\n"),
            "{text}"
        );
        assert!(text.contains("ticked by Ben at "), "{text}");
        assert!(text.contains(", a flight since"), "{text}");
        assert!(
            text.contains(" 2. FAIL     blocks Battery at least 22.2 V (battery.voltage_v >= 22.2 V, held 3 s): Battery is below 22.2 V.\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "BATT_ARM_VOLT is 0; set it to 22.2 with `uxduck checklist <vehicle> set-param 2`"
            ),
            "{text}"
        );
        assert!(
            text.ends_with("Holding arming: Battery at least 22.2 V.\n"),
            "{text}"
        );
        let found = find(&state, "propellers tight").unwrap();
        assert_eq!(found["id"], "a");
        assert_eq!(find(&state, "2").unwrap()["id"], "b");
        assert!(find(&state, "3").is_err());
    }
}

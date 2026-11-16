//! `uxduck checklist` (S193 and S353, ADR-0038 K7 to K11, K16, K17): a
//! vehicle's own checklist from a terminal: list it by group, add a manual
//! step or a rule, under a group, ticked by a person too or not, tick
//! and clear, export it as a file and import a file or a Markdown list,
//! copy another vehicle's, and set the parameter that makes the vehicle
//! hold a rule itself. A preflight is a run of it (S350, K18, K19): start
//! a new one, finish it, and read the earlier ones.

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
        /// Seconds the rule's value must hold before it passes: any
        /// time, 0 or more.
        #[arg(long, requires = "rule", value_name = "SECONDS")]
        hold: Option<f64>,
        /// Hold the launch (a take-off, or a mission's start) until the
        /// step passes. Without it the step only warns. Arming is never
        /// held.
        #[arg(long = "hold-launch")]
        hold_launch: bool,
        /// The step counts at the launch, where and when the vehicle takes
        /// off, and not in the preflight.
        #[arg(long)]
        launch: bool,
        /// Only warn, which every step does unless `--hold-launch`.
        #[arg(long, hide = true, conflicts_with = "hold_launch")]
        warn: bool,
        /// The concept the step sits under, such as Battery or Controller.
        #[arg(long, value_name = "NAME")]
        group: Option<String>,
        /// A person ticks the rule too, once it passes.
        #[arg(long, requires = "rule")]
        confirm: bool,
    },
    /// Removes a step, by its number, id or title.
    Remove { step: String },
    /// Ticks a manual step, or a rule added with `--confirm` once it
    /// passes, by its number, id or title.
    Tick {
        step: String,
        /// Clears its tick instead.
        #[arg(long)]
        clear: bool,
    },
    /// Starts a new preflight: every tick is cleared. The preflight
    /// before it stays in the vehicle's history.
    #[command(alias = "reset")]
    New,
    /// Finishes the preflight: it is stamped with who and when, kept in
    /// the vehicle's history, and locked until it is reopened or a new
    /// one starts.
    Finish,
    /// Reopens the finished preflight, with its ticks, to change it and
    /// finish it again. What its finish wrote to the history stays.
    Reopen,
    /// The vehicle's earlier preflights, newest first.
    Runs {
        /// At most this many (20 unless given, at most 100).
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },
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
            hold_launch,
            launch,
            warn: _,
            group,
            confirm,
        } => {
            let mut step = new_step(&title, rule.as_deref(), hold, hold_launch)?;
            if launch {
                step["when"] = json!("launch");
            }
            if let Some(group) = group {
                step["group"] = json!(group);
            }
            if confirm {
                step["confirm"] = json!(true);
            }
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
        ChecklistAction::New => {
            client
                .call("checklist/reset", json!({ "vehicleId": id }))
                .await?
        }
        ChecklistAction::Finish => {
            client
                .call("checklist/finish", json!({ "vehicleId": id }))
                .await?
        }
        ChecklistAction::Reopen => {
            client
                .call("checklist/reopen", json!({ "vehicleId": id }))
                .await?
        }
        ChecklistAction::Runs { limit } => {
            let mut params = json!({ "vehicleId": id });
            if let Some(limit) = limit {
                params["limit"] = json!(limit);
            }
            let runs = client.call("checklist/runs", params).await?["runs"].take();
            if args.json {
                println!("{runs}");
            } else {
                print!("{}", run_lines(runs.as_array().map_or(&[], Vec::as_slice)));
            }
            return Ok(());
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
    hold_launch: bool,
) -> Result<Value, CliError> {
    let severity = if hold_launch { "block" } else { "warn" };
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
        // JSON carries no infinity: it would go as no hold at all.
        if !(hold.is_finite() && hold >= 0.0) {
            return Err(CliError::Usage(format!(
                "--hold takes 0 seconds or more, not {hold}"
            )));
        }
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

/// The person behind a tick or a stamp, or the client when no operator
/// was known.
fn who(tick: &Value) -> &str {
    tick["by"]["operatorName"]
        .as_str()
        .or_else(|| tick["by"]["clientName"].as_str())
        .unwrap_or("someone")
}

/// 2026-11-14 14:02 by Ben.
fn stamp(tick: &Value) -> String {
    format!(
        "{} by {}",
        tick["atUs"].as_i64().map(when).unwrap_or_default(),
        who(tick)
    )
}

/// A run's earlier finishes that a person reopened, oldest first, after
/// a lead: " Before that: finished 14:14 by Ben, reopened 14:20 by Ada."
fn reopenings(run: &Value, lead: &str) -> String {
    let each: Vec<String> = run["reopenings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            format!(
                "finished {}, reopened {}",
                stamp(&r["finished"]),
                stamp(&r["reopened"])
            )
        })
        .collect();
    if each.is_empty() {
        String::new()
    } else {
        format!(" {lead}: {}.", each.join(", "))
    }
}

/// The preflight under way or finished, in a line.
fn run_line(run: &Value) -> String {
    if !run["finished"].is_object() {
        return format!(
            "Preflight started {}.{} Finish it with `uxduck checklist <vehicle> finish`.\n",
            stamp(&run["started"]),
            reopenings(run, "Since then")
        );
    }
    let restarted = run["restartedAtUs"]
        .as_i64()
        .zip(run["finished"]["atUs"].as_i64())
        .is_some_and(|(restarted, finished)| restarted > finished);
    format!(
        "Preflight finished {}.{}{} Its steps and preflight ticks are locked until `uxduck checklist <vehicle> reopen` or `new`.\n",
        stamp(&run["finished"]),
        reopenings(run, "Before that"),
        if restarted {
            " The vehicle has restarted since."
        } else {
            ""
        }
    )
}

/// A vehicle's earlier preflights as the terminal shows them: how each
/// ended, each step as it stood, and the checks that did not pass.
#[must_use]
pub fn run_lines(runs: &[Value]) -> String {
    use std::fmt::Write;
    if runs.is_empty() {
        return "No earlier preflights. One is kept each time it is finished, or when a new one starts and it has ticks.\n".into();
    }
    let mut out = String::new();
    for (n, run) in runs.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let started = stamp(&run["started"]);
        if run["finished"].is_object() {
            let _ = writeln!(
                out,
                "Finished {}. Started {started}.{}",
                stamp(&run["finished"]),
                reopenings(run, "In between")
            );
        } else {
            let _ = writeln!(
                out,
                "Left unfinished. Started {started}.{}",
                reopenings(run, "Since then")
            );
        }
        for step in run["steps"].as_array().into_iter().flatten() {
            let _ = write!(
                out,
                "    {:<8} {}{}{}",
                status_word(step["status"].as_str().unwrap_or_default()),
                step["group"]
                    .as_str()
                    .map(|group| format!("{group} · "))
                    .unwrap_or_default(),
                step["title"].as_str().unwrap_or_default(),
                if step["when"] == "launch" {
                    " [at launch]"
                } else {
                    ""
                }
            );
            if !step["value"].is_null() {
                let _ = write!(
                    out,
                    " (read {}{})",
                    number(&step["value"]),
                    step["unit"]
                        .as_str()
                        .map(|unit| format!(" {unit}"))
                        .unwrap_or_default()
                );
            }
            if step["tick"].is_object() {
                let _ = write!(out, ", ticked {}", stamp(&step["tick"]));
            }
            out.push('\n');
        }
        // The launch's checks are for the place and the minute of the
        // launch, not for the preflight.
        let failing: Vec<&str> = run["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["when"] != "launch" && c["status"] != "pass")
            .filter_map(|c| c["title"].as_str())
            .collect();
        if !failing.is_empty() {
            let _ = writeln!(out, "    Not passing: {}.", failing.join(", "));
        }
    }
    out
}

/// A step's line: its number, status, severity, group and words, a
/// rule's comparison, and under it who ticked it.
fn step_lines(n: usize, step: &Value, check: &Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let blocks = if step["severity"] == "block" {
        "holds"
    } else {
        "warns"
    };
    let _ = write!(
        out,
        "{n:>2}. {:<8} {blocks:<6} {}{}{}",
        status_word(check["status"].as_str().unwrap_or_default()),
        step["group"]
            .as_str()
            .map(|group| format!("{group} · "))
            .unwrap_or_default(),
        step["title"].as_str().unwrap_or_default(),
        if step["when"] == "launch" {
            " [at launch]"
        } else {
            ""
        }
    );
    if step["kind"] == "rule" {
        let _ = write!(
            out,
            " ({}{})",
            rule_words(step),
            if step["confirm"] == true {
                ", then ticked"
            } else {
                ""
            }
        );
        if let Some(detail) = check["detail"].as_str() {
            let _ = write!(out, ": {detail}");
        }
    }
    out.push('\n');
    if step["tick"].is_object() {
        let by = who(&step["tick"]);
        let at = step["tick"]["atUs"].as_i64().map(when).unwrap_or_default();
        let since = if step["flownSince"] == true {
            ", a flight since"
        } else {
            ""
        };
        let _ = writeln!(out, "                    ticked by {by} at {at}{since}");
    }
    out
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
    if state["checklist"]["run"].is_object() {
        out.push_str(&run_line(&state["checklist"]["run"]));
    }
    let checks = state["checks"].as_array().cloned().unwrap_or_default();
    for (n, step) in steps.iter().enumerate() {
        let id = step["id"].as_str().unwrap_or_default();
        let check = checks
            .iter()
            .find(|c| c["id"] == format!("checklist:{id}"))
            .cloned()
            .unwrap_or(Value::Null);
        out.push_str(&step_lines(n + 1, step, &check));
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
        let _ = writeln!(out, "Nothing on the checklist holds the launch.");
    } else {
        let _ = writeln!(out, "Holding the launch: {}.", holding.join(", "));
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
        let step = new_step("Arming checks", Some("ARMING_CHECK == 1"), None, false).unwrap();
        assert!(step.get("group").is_none() && step.get("confirm").is_none());
        assert_eq!(step["param"], "ARMING_CHECK");
        assert_eq!(step["severity"], "warn");
        let step = new_step(
            "Battery",
            Some("battery.voltage_v >= 22.2"),
            Some(3.0),
            true,
        )
        .unwrap();
        assert_eq!(step["severity"], "block", "set to hold the launch");
        assert_eq!(step["item"], "battery.voltage_v");
        assert_eq!(step["holdS"], 3.0);
    }

    /// A hold has no upper end. One that JSON cannot carry went as no hold
    /// at all, without a word: `--hold inf` added a rule that passes at
    /// once.
    #[test]
    fn a_hold_is_any_time_and_one_that_cannot_be_kept_is_refused() {
        let rule = Some("battery.voltage_v >= 22.2");
        let day = new_step("Battery", rule, Some(86_400.0), true).unwrap();
        assert_eq!(day["holdS"], 86_400.0);
        for hold in [f64::INFINITY, f64::NAN, -1.0] {
            let Err(CliError::Usage(why)) = new_step("Battery", rule, Some(hold), true) else {
                panic!("--hold {hold} was taken");
            };
            assert_eq!(why, format!("--hold takes 0 seconds or more, not {hold}"));
        }
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
                      "item": "battery.voltage_v", "op": ">=", "value": 22.2, "unit": "V", "holdS": 3.0 },
                    { "id": "c", "kind": "rule", "title": "Charged and strapped", "group": "Battery", "confirm": true,
                      "severity": "block", "when": "launch", "item": "battery.cell_v", "op": ">=", "value": 4.1, "unit": "V" }
                ],
                "offers": [{ "stepId": "b", "name": "BATT_ARM_VOLT", "value": 22.2, "current": 0.0, "held": false }],
                "version": 3
            }
        });
        let text = lines(&state);
        assert!(
            text.contains(" 1. ok       holds  Propellers tight\n"),
            "{text}"
        );
        assert!(text.contains("ticked by Ben at "), "{text}");
        assert!(text.contains(", a flight since"), "{text}");
        assert!(
            text.contains(" 2. FAIL     holds  Battery at least 22.2 V (battery.voltage_v >= 22.2 V, held 3 s): Battery is below 22.2 V.\n"),
            "{text}"
        );
        assert!(
            text.contains(
                " 3. ?        holds  Battery · Charged and strapped [at launch] (battery.cell_v >= 4.1 V, then ticked)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "BATT_ARM_VOLT is 0; set it to 22.2 with `uxduck checklist <vehicle> set-param 2`"
            ),
            "{text}"
        );
        assert!(
            text.ends_with("Holding the launch: Battery at least 22.2 V.\n"),
            "{text}"
        );
        assert!(!text.contains("Preflight "), "no run, no line: {text}");
        let found = find(&state, "propellers tight").unwrap();
        assert_eq!(found["id"], "a");
        assert_eq!(find(&state, "2").unwrap()["id"], "b");
        assert!(find(&state, "4").is_err());
    }

    #[test]
    fn a_preflight_and_the_earlier_ones_in_lines() {
        let ben = |at_us: i64| json!({ "by": { "clientName": "uxduck-web", "operatorName": "Ben" }, "atUs": at_us });
        let mut state = json!({
            "checks": [],
            "checklist": {
                "steps": [{ "id": "a", "kind": "manual", "title": "Propellers tight", "severity": "warn" }],
                "run": { "id": "r2", "started": ben(0) }
            }
        });
        let under_way = lines(&state);
        assert!(under_way.starts_with("Preflight started "), "{under_way}");
        assert!(
            under_way.contains(" by Ben. Finish it with `uxduck checklist <vehicle> finish`.\n"),
            "{under_way}"
        );
        state["checklist"]["run"]["finished"] = ben(60_000_000);
        let finished = lines(&state);
        assert!(finished.starts_with("Preflight finished "), "{finished}");
        assert!(
            finished.contains(
                " by Ben. Its steps and preflight ticks are locked until `uxduck checklist <vehicle> reopen` or `new`.\n"
            ),
            "{finished}"
        );
        // A restart before it was finished is not news. One since is.
        state["checklist"]["run"]["restartedAtUs"] = json!(30_000_000);
        assert_eq!(lines(&state), finished);
        state["checklist"]["run"]["restartedAtUs"] = json!(90_000_000);
        assert!(
            lines(&state).contains(" by Ben. The vehicle has restarted since. Its steps and"),
            "{}",
            lines(&state)
        );
        // Reopened and finished again, it says each in turn.
        let ada = json!({ "by": { "clientName": "uxduck-web", "operatorName": "Ada" }, "atUs": 120_000_000 });
        state["checklist"]["run"]["restartedAtUs"] = Value::Null;
        state["checklist"]["run"]["reopenings"] =
            json!([{ "finished": ben(60_000_000), "reopened": ada }]);
        state["checklist"]["run"]["finished"] = Value::Null;
        let reopened = lines(&state);
        assert!(reopened.starts_with("Preflight started "), "{reopened}");
        assert!(
            reopened.contains(" by Ben. Since then: finished "),
            "{reopened}"
        );
        assert!(
            reopened.contains(" by Ben, reopened ")
                && reopened.contains(" by Ada. Finish it with "),
            "{reopened}"
        );
        state["checklist"]["run"]["finished"] = ben(180_000_000);
        let again = lines(&state);
        assert!(again.starts_with("Preflight finished "), "{again}");
        assert!(again.contains(" by Ben. Before that: finished "), "{again}");
        assert!(again.contains(" by Ada. Its steps and"), "{again}");

        assert!(run_lines(&[]).starts_with("No earlier preflights."));
        let runs = [
            json!({ "id": "r2", "started": ben(0), "steps": [
                { "title": "Propellers tight", "kind": "manual", "severity": "warn", "status": "to-do" }
            ] }),
            json!({ "id": "r1", "started": ben(0), "finished": ben(60_000_000),
                "steps": [
                    { "title": "Propellers tight", "kind": "manual", "severity": "warn", "status": "pass", "tick": ben(0) },
                    { "title": "At least 22.2 V", "kind": "rule", "group": "Battery", "severity": "block",
                      "status": "pass", "unit": "V", "value": 22.6 },
                    { "title": "Area clear", "kind": "manual", "severity": "warn", "when": "launch", "status": "to-do" }
                ],
                "checks": [
                    { "id": "uxduck:link", "title": "Link", "status": "fail" },
                    { "id": "uxduck:setup", "title": "Setup", "status": "pass" },
                    { "id": "uxduck:home", "title": "Home", "status": "unknown", "when": "launch" }
                ] }),
        ];
        let text = run_lines(&runs);
        let (first, second) = text.split_once("\n\n").unwrap();
        assert!(first.starts_with("Left unfinished. Started "), "{text}");
        assert!(first.ends_with("    to do    Propellers tight"), "{text}");
        assert!(second.starts_with("Finished "), "{text}");
        assert!(second.contains(" by Ben. Started "), "{text}");
        assert!(
            second.contains("    ok       Propellers tight, ticked "),
            "{text}"
        );
        assert!(
            second.contains("    ok       Battery · At least 22.2 V (read 22.6 V)\n"),
            "{text}"
        );
        assert!(
            second.contains("    to do    Area clear [at launch]\n"),
            "{text}"
        );
        assert!(second.ends_with("    Not passing: Link.\n"), "{text}");
        let twice = run_lines(&[
            json!({ "id": "r1", "started": ben(0), "finished": ben(180_000_000),
            "reopenings": [{ "finished": ben(60_000_000), "reopened": ben(120_000_000) }], "steps": [] }),
        ]);
        assert!(twice.starts_with("Finished "), "{twice}");
        assert!(
            twice.contains(" by Ben. Started ") && twice.contains(" by Ben. In between: finished "),
            "{twice}"
        );
        assert!(twice.trim_end().ends_with(" by Ben."), "{twice}");
    }
}

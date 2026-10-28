//! `uxduck preflight` (S192, ADR-0038): a vehicle's preflight checks, the
//! same list the window and the agent read, and setting UXDuck's own.

use std::path::Path;

use clap::Args;
use serde_json::{Map, Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Args)]
pub struct PreflightArgs {
    /// The vehicle, by name or id.
    pub vehicle: String,
    /// Print the list as JSON.
    #[arg(long)]
    pub json: bool,
    /// One of UXDuck's own checks to change: setup, restart, link,
    /// controller, plan, terrain or recording.
    #[arg(long, value_name = "CHECK")]
    pub check: Option<String>,
    /// Turns the check on.
    #[arg(long, requires = "check", conflicts_with = "off")]
    pub on: bool,
    /// Turns the check off.
    #[arg(long, requires = "check")]
    pub off: bool,
    /// `block` holds arming while it does not pass; `warn` only says so.
    #[arg(long, requires = "check", value_name = "block|warn")]
    pub severity: Option<String>,
    /// The check's threshold, such as the least radio signal in percent;
    /// `default` puts it back.
    #[arg(long, requires = "check", value_name = "N|default")]
    pub threshold: Option<String>,
    /// The plan for the next flight, by id; `none` for none.
    #[arg(long, value_name = "ID|none")]
    pub plan: Option<String>,
}

pub async fn run(data_dir: &Path, args: PreflightArgs) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let id = super::vehicle::resolve(&mut client, &args.vehicle).await?;
    let changing = args.check.is_some() || args.plan.is_some();
    let state = if changing {
        let mut params = Map::new();
        params.insert("vehicleId".into(), json!(id));
        if let Some(check) = &args.check {
            params.insert("check".into(), json!(check));
        }
        if args.on || args.off {
            params.insert("on".into(), json!(args.on));
        }
        if let Some(severity) = &args.severity {
            params.insert("severity".into(), json!(severity));
        }
        if let Some(threshold) = &args.threshold {
            let value = if threshold == "default" {
                Value::Null
            } else {
                json!(threshold.parse::<f64>().map_err(|_| {
                    CliError::Usage(format!(
                        "--threshold takes a number or default, not {threshold}"
                    ))
                })?)
            };
            params.insert("threshold".into(), value);
        }
        if let Some(plan) = &args.plan {
            let value = if plan == "none" {
                Value::Null
            } else {
                json!(plan)
            };
            params.insert("planId".into(), value);
        }
        client
            .call("preflight/configure", Value::Object(params))
            .await?
    } else {
        client
            .call("preflight/state", json!({ "vehicleId": id }))
            .await?
    };
    if args.json {
        println!("{state}");
    } else {
        print!("{}", lines(&state));
    }
    Ok(())
}

/// A status in words, for the first column.
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

/// A check's title; a plugin's names its plugin (S194).
fn title_of(check: &Value) -> String {
    let title = check["title"].as_str().unwrap_or_default();
    match check["plugin"]["name"].as_str() {
        Some(plugin) => format!("{title} ({plugin})"),
        None => title.to_owned(),
    }
}

/// The list as the terminal shows it: one line per check, the vehicle's
/// own first, then a line saying whether UXDuck holds arming.
#[must_use]
pub fn lines(state: &Value) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let checks = state["checks"].as_array().cloned().unwrap_or_default();
    if state["evaluating"] == false {
        let _ = writeln!(out, "Armed: the checks as they stood when it armed.");
    }
    for check in &checks {
        let status = check["status"].as_str().unwrap_or_default();
        let blocks = if check["severity"] == "block" {
            "blocks"
        } else {
            "warns"
        };
        let source = if check["source"] == "vehicle" {
            "vehicle"
        } else {
            blocks
        };
        let _ = write!(
            out,
            "{:<8} {:<7} {}",
            status_word(status),
            source,
            title_of(check)
        );
        if let Some(detail) = check["detail"].as_str() {
            let _ = write!(out, ": {detail}");
        }
        // It asks to block; nobody has allowed it yet (K13).
        if check["plugin"]["heldBack"] == true {
            let _ = write!(
                out,
                " (warns until you allow it: uxduck plugin checks {} block)",
                check["plugin"]["id"].as_str().unwrap_or_default()
            );
        }
        out.push('\n');
    }
    let off: Vec<&str> = state["settings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["on"] == false)
        .filter_map(|s| s["title"].as_str())
        .collect();
    if !off.is_empty() {
        let _ = writeln!(out, "Off: {}", off.join(", "));
    }
    let gating: Vec<String> = checks
        .iter()
        .filter(|c| c["gates"] == true)
        .map(title_of)
        .collect();
    if gating.is_empty() {
        let _ = writeln!(out, "UXDuck holds nothing back.");
    } else {
        let _ = writeln!(
            out,
            "UXDuck holds arming: {}. `uxduck command arm --arm-anyway` arms past them.",
            gating.join(", ")
        );
    }
    out
}

/// The ids of the checks holding arming now, for `--arm-anyway`.
pub async fn gating(client: &mut LocalClient, vehicle_id: &str) -> Result<Vec<Value>, CliError> {
    let state = client
        .call("preflight/state", json!({ "vehicleId": vehicle_id }))
        .await?;
    Ok(state["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["gates"] == true)
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn the_list_in_the_terminal() {
        let state = json!({
            "evaluating": true,
            "checks": [
                { "id": "vehicle:ready", "source": "vehicle", "title": "Alpha's own checks", "severity": "block", "status": "pass", "detail": "It says it is ready to arm.", "gates": false },
                { "id": "uxduck:restart", "source": "uxduck", "title": "No restart due", "severity": "block", "status": "fail", "detail": "The compass calibration takes effect once Alpha restarts.", "gates": true },
                { "id": "uxduck:plan", "source": "uxduck", "title": "Plan on the vehicle", "severity": "warn", "status": "fail", "detail": "Ridge survey is chosen, but it isn't on Alpha yet.", "gates": false },
                { "id": "plugin:wind-check:wind", "source": "plugin", "title": "Wind under 8 m/s", "severity": "block", "status": "unknown", "detail": "The Wind check plugin isn't running: it's turned off.", "gates": true, "plugin": { "id": "wind-check", "name": "Wind check" } },
                { "id": "plugin:gusts:gusts", "source": "plugin", "title": "No gusts", "severity": "warn", "status": "fail", "detail": "Gusting.", "gates": false, "plugin": { "id": "gusts", "name": "Gusts", "heldBack": true } }
            ],
            "settings": [ { "title": "Terrain loaded", "on": false } ]
        });
        let text = super::lines(&state);
        assert_eq!(
            text,
            "ok       vehicle Alpha's own checks: It says it is ready to arm.\n\
             FAIL     blocks  No restart due: The compass calibration takes effect once Alpha restarts.\n\
             FAIL     warns   Plan on the vehicle: Ridge survey is chosen, but it isn't on Alpha yet.\n\
             unknown  blocks  Wind under 8 m/s (Wind check): The Wind check plugin isn't running: it's turned off.\n\
             FAIL     warns   No gusts (Gusts): Gusting. (warns until you allow it: uxduck plugin checks gusts block)\n\
             Off: Terrain loaded\n\
             UXDuck holds arming: No restart due, Wind under 8 m/s (Wind check). `uxduck command arm --arm-anyway` arms past them.\n"
        );
    }
}

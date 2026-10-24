//! `uxduck traffic` (S168, ADR-0034): other aircraft near the vehicles, the
//! sources that report them, and turning a vehicle's traffic on or off.

use std::collections::BTreeMap;
use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};
use super::vehicle::resolve;

#[derive(Debug, Subcommand)]
pub enum TrafficCommand {
    /// Every source, on or off: how many aircraft each hears and what it
    /// sent that was refused.
    Sources {
        #[arg(long)]
        json: bool,
    },
    /// Uses the traffic a vehicle's ADS-B receiver reports.
    On {
        /// The vehicle, by name or id.
        vehicle: String,
    },
    /// Stops using a vehicle's traffic; it stays listed as a source.
    Off {
        /// The vehicle, by name or id.
        vehicle: String,
    },
}

/// `uxduck traffic` alone lists the aircraft; with a command, as above.
pub async fn run(
    data_dir: &Path,
    command: Option<TrafficCommand>,
    json: bool,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        None => {
            let status = client.call("traffic/status", json!({})).await?;
            if json {
                println!("{}", status["aircraft"]);
            } else {
                print_aircraft(&status);
            }
        }
        Some(TrafficCommand::Sources { json }) => {
            let status = client.call("traffic/status", json!({})).await?;
            if json {
                println!("{}", status["sources"]);
            } else {
                let names = names(&mut client).await?;
                print_sources(&status, &names);
            }
        }
        Some(TrafficCommand::On { vehicle }) => switch(&mut client, &vehicle, true).await?,
        Some(TrafficCommand::Off { vehicle }) => switch(&mut client, &vehicle, false).await?,
    }
    Ok(())
}

async fn switch(client: &mut LocalClient, wanted: &str, on: bool) -> Result<(), CliError> {
    let id = resolve(client, wanted).await?;
    let vehicle = client
        .call("vehicle/traffic", json!({ "id": id, "on": on }))
        .await?;
    let name = vehicle["name"].as_str().unwrap_or(wanted);
    if on {
        println!("Using {name}'s traffic.");
    } else {
        println!("Not using {name}'s traffic.");
    }
    Ok(())
}

/// Vehicles' and plugins' names by id, keyed `vehicle:<id>` and `plugin:<id>`.
async fn names(client: &mut LocalClient) -> Result<BTreeMap<String, String>, CliError> {
    let list = client.call("vehicle/list", json!({})).await?;
    let state = client
        .call("state/subscribe", json!({ "entities": ["plugin"] }))
        .await?;
    let named = |list: &Value, key: &str, prefix: &str| -> Vec<(String, String)> {
        list[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                Some((
                    format!("{prefix}:{}", v["id"].as_str()?),
                    v["name"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };
    let links = client.call("link/list", json!({})).await?;
    Ok(named(&list, "vehicles", "vehicle")
        .into_iter()
        .chain(named(&state["snapshot"], "plugins", "plugin"))
        .chain(named(&links, "links", "receiver"))
        .collect())
}

fn print_aircraft(status: &Value) {
    let aircraft = status["aircraft"].as_array().cloned().unwrap_or_default();
    if aircraft.is_empty() {
        let any_on = status["sources"]
            .as_array()
            .is_some_and(|s| s.iter().any(|s| s["on"] == true));
        if any_on {
            println!("No aircraft heard.");
        } else {
            println!("No traffic source is on. `uxduck traffic sources` lists them.");
        }
        return;
    }
    for a in aircraft {
        let name = a["callsign"]
            .as_str()
            .unwrap_or_else(|| a["address"].as_str().unwrap_or("?"));
        let place = match (a["latitudeDeg"].as_f64(), a["longitudeDeg"].as_f64()) {
            (Some(lat), Some(lon)) => format!("{lat:.5}, {lon:.5}"),
            _ => "no position".into(),
        };
        let altitude = a["altitudeM"]
            .as_f64()
            .map_or_else(String::new, |m| format!(", {m:.0} m"));
        let stale = if a["stale"] == true {
            " (not heard lately)"
        } else {
            ""
        };
        println!("{name}: {place}{altitude}{stale}");
    }
}

fn print_sources(status: &Value, names: &BTreeMap<String, String>) {
    let sources = status["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        println!("No vehicle has reported traffic.");
        return;
    }
    for s in sources {
        let id = s["id"].as_str().unwrap_or_default();
        let name = names.get(id).map_or(id, String::as_str);
        let what = match s["kind"].as_str() {
            Some("plugin") => "plugin",
            Some("receiver") => "receiver",
            _ => "ADS-B receiver",
        };
        let state = match s["state"].as_str() {
            Some("hearing") => format!("hears {} aircraft", s["aircraft"]),
            Some("paused") => "paused: it sent too much at once".into(),
            _ => "quiet".into(),
        };
        let on = if s["on"] == true { "on" } else { "off" };
        println!("{name} · {what} · {state} · {on}");
        if let Some(note) = s["note"].as_str() {
            println!("  {note}");
        }
        for r in s["refused"].as_array().into_iter().flatten() {
            println!(
                "  refused {} for {}",
                r["count"],
                r["reason"].as_str().unwrap_or("?")
            );
        }
    }
}

//! `uxduck vehicle`: a vehicle's history from the command line (S69,
//! ADR-0022 H8), for people at a bench and for scripts, and its labels
//! (S106).

use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

const DAY_US: i64 = 86_400_000_000;

#[derive(Debug, Subcommand)]
pub enum VehicleCommand {
    /// Its history, newest first: notes and what UXDuck recorded.
    History {
        /// Its name or id; with none, the only vehicle there is.
        vehicle: Option<String>,
        /// Only words in the text, such as `motor`.
        #[arg(long)]
        text: Option<String>,
        /// `note` or `event`.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Writes a note in its history.
    Note {
        /// Its name or id.
        vehicle: String,
        text: String,
        /// When it happened: `today`, `yesterday` or `2026-09-28`.
        #[arg(long)]
        when: Option<String>,
        /// maintenance, repair, damage, configuration, calibration,
        /// firmware, test or other.
        #[arg(long)]
        category: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Puts labels on vehicles and takes them off, such as
    /// `uxduck vehicle label Alpha Bravo --add Survey`; with neither, lists
    /// their labels.
    Label {
        /// Their names or ids.
        #[arg(required = true)]
        vehicles: Vec<String>,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        remove: Vec<String>,
    },
}

pub async fn run(data_dir: &Path, command: VehicleCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        VehicleCommand::History {
            vehicle,
            text,
            kind,
            limit,
            json: as_json,
        } => {
            let vehicle_id = match vehicle {
                Some(v) => Some(resolve(&mut client, &v).await?),
                None => None,
            };
            let params = super::strip_nulls(json!({
                "vehicleId": vehicle_id,
                "text": text,
                "kind": kind,
                "limit": limit,
            }));
            let result = client.call("vehicle/history", params).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            } else {
                print_history(&result);
            }
            Ok(())
        }
        VehicleCommand::Note {
            vehicle,
            text,
            when,
            category,
            tags,
        } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            let now = now_us();
            let happened = when.as_deref().map(|w| parse_when(w, now)).transpose()?;
            let params = super::strip_nulls(json!({
                "vehicleId": vehicle_id,
                "text": text,
                "category": category,
                "tags": tags,
                "happenedAtUs": happened,
            }));
            let entry = client.call("vehicle/addNote", params).await?;
            println!(
                "Noted for {}: {}",
                date(entry["happenedAtUs"].as_i64().unwrap_or_default()),
                entry["kind"]["text"].as_str().unwrap_or_default()
            );
            Ok(())
        }
        VehicleCommand::Label {
            vehicles,
            add,
            remove,
        } => {
            let mut ids = Vec::new();
            for vehicle in &vehicles {
                ids.push(resolve(&mut client, vehicle).await?);
            }
            if !add.is_empty() || !remove.is_empty() {
                client
                    .call(
                        "vehicle/label",
                        json!({ "ids": ids, "add": add, "remove": remove }),
                    )
                    .await?;
            }
            for id in &ids {
                let vehicle = client.call("vehicle/get", json!({ "id": id })).await?;
                println!("{}", labels_line(&vehicle));
            }
            Ok(())
        }
    }
}

/// "Alpha: Site B, Survey", or "Alpha: no labels".
fn labels_line(vehicle: &Value) -> String {
    let name = vehicle["name"].as_str().unwrap_or("The vehicle");
    let labels: Vec<&str> = vehicle["labels"]
        .as_array()
        .map(|all| all.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if labels.is_empty() {
        format!("{name}: no labels")
    } else {
        format!("{name}: {}", labels.join(", "))
    }
}

/// A vehicle by id, or by name in any case.
pub(crate) async fn resolve(client: &mut LocalClient, wanted: &str) -> Result<String, CliError> {
    let list = client.call("vehicle/list", json!({})).await?;
    let vehicles = list["vehicles"].as_array().cloned().unwrap_or_default();
    vehicles
        .iter()
        .find(|v| v["id"] == wanted)
        .or_else(|| {
            vehicles.iter().find(|v| {
                v["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
            })
        })
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .ok_or_else(|| CliError::Usage(format!("no vehicle called {wanted}")))
}

fn print_history(result: &Value) {
    let name = result["vehicleName"].as_str().unwrap_or("The vehicle");
    let entries = result["entries"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        println!("{name} has no history yet.");
        return;
    }
    println!("History of {name}:");
    for entry in &entries {
        let when = date(entry["happenedAtUs"].as_i64().unwrap_or_default());
        let kind = &entry["kind"];
        let line = if kind["type"] == "note" {
            let category = kind["category"]
                .as_str()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            let by = entry["author"]["operatorName"]
                .as_str()
                .or_else(|| entry["author"]["clientName"].as_str())
                .unwrap_or("someone");
            format!(
                "Note{category}: {} — {by}",
                kind["text"].as_str().unwrap_or_default()
            )
        } else {
            event_text(&kind["event"])
        };
        println!("  {when}  {line}");
    }
    let total = result["total"].as_u64().unwrap_or_default();
    if total > entries.len() as u64 {
        println!("  … {} more (--limit)", total - entries.len() as u64);
    }
}

/// An event as the History tab says it.
fn event_text(event: &Value) -> String {
    let text = |key: &str| event[key].as_str().unwrap_or_default().to_owned();
    match event["type"].as_str().unwrap_or_default() {
        "first-seen" => format!("First seen, as a {}", text("vehicleType")),
        "renamed" => format!("Renamed from “{}” to “{}”", text("from"), text("to")),
        "firmware-changed" => match event["from"].as_str() {
            Some(from) => format!("Firmware {from} → {}", text("to")),
            None => format!("Firmware {}", text("to")),
        },
        "type-changed" => format!("Type changed from {} to {}", text("from"), text("to")),
        "snapshot-saved" => format!("Snapshot “{}” saved", text("name")),
        other => other.to_owned(),
    }
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// `today`, `yesterday` or `YYYY-MM-DD`, as noon UTC that day, so the date
/// reads the same in most time zones; never later than now.
fn parse_when(when: &str, now: i64) -> Result<i64, CliError> {
    let usage = || CliError::Usage(format!("--when {when}: today, yesterday or 2026-09-28"));
    let day = match when {
        "today" => now.div_euclid(DAY_US),
        "yesterday" => now.div_euclid(DAY_US) - 1,
        _ => {
            let parts: Vec<i64> = when
                .split('-')
                .map(str::parse)
                .collect::<Result<_, _>>()
                .map_err(|_| usage())?;
            let [year, month, day] = parts[..] else {
                return Err(usage());
            };
            if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return Err(usage());
            }
            days_from_civil(year, month, day)
        }
    };
    Ok((day * DAY_US + DAY_US / 2).min(now))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` of a time, in UTC.
fn date(us: i64) -> String {
    let z = us.div_euclid(DAY_US) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_go_both_ways() {
        let at = parse_when("2026-09-28", i64::MAX).unwrap();
        assert_eq!(date(at), "2026-09-28");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(
            date(parse_when("2000-02-29", i64::MAX).unwrap()),
            "2000-02-29"
        );
        let now = parse_when("2026-10-02", i64::MAX).unwrap();
        assert_eq!(date(parse_when("yesterday", now).unwrap()), "2026-10-01");
        assert_eq!(
            parse_when("2030-01-01", now).unwrap(),
            now,
            "never later than now"
        );
        assert!(parse_when("28/09/2026", now).is_err());
        assert!(parse_when("2026-13-01", now).is_err());
    }
}

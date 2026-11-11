//! `uxduck mavlink dialects` (S252, ADR-0044): the MAVLink XML a fork adds
//! or changes messages with, used by builds of its commits or by the vehicles
//! it is chosen for (F13).

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum MavlinkCommand {
    /// Dialects of a fork's own messages: `uxduck mavlink dialects add
    /// acme.xml --commit 2a3dc4b7`.
    Dialects {
        #[command(subcommand)]
        command: DialectsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DialectsCommand {
    /// The dialects you added, newest first, or those a vehicle uses.
    List {
        /// Only the dialects this vehicle uses, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Adds a dialect from its XML files (a file it includes is given with
    /// it). Vehicles built from `--commit` use it; `--vehicle` chooses it
    /// for one more.
    Add {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Its name; the first file's name by default.
        #[arg(long)]
        label: Option<String>,
        /// Also choose it for this vehicle, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        /// Used by vehicles whose firmware was built from this commit;
        /// give it once for each build.
        #[arg(long = "commit")]
        commits: Vec<String>,
        /// Used by every vehicle, for a dialect that only adds messages.
        #[arg(long, conflicts_with = "commits")]
        every_vehicle: bool,
    },
    /// Adds the messages of an ArduPilot checkout: its
    /// `ardupilotmega.xml` and the files it includes, used by builds of the
    /// checkout's commit.
    Generate {
        /// The ArduPilot checkout.
        checkout: PathBuf,
        /// Its name; the checkout's folder and commit by default.
        #[arg(long)]
        label: Option<String>,
        /// Also choose it for this vehicle, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        /// Builds of these commits use it, instead of the checkout's own.
        #[arg(long = "commit")]
        commits: Vec<String>,
    },
    /// Changes which vehicles use a dialect by themselves: builds of its
    /// commits, or every vehicle. With neither, only those it is chosen for.
    Bind {
        id: String,
        /// A commit whose builds use it; give it once for each build.
        #[arg(long = "commit")]
        commits: Vec<String>,
        /// Used by every vehicle, for a dialect that only adds messages.
        #[arg(long, conflicts_with = "commits")]
        every_vehicle: bool,
    },
    /// Chooses which dialects a vehicle uses; with no flags, says which.
    Use {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Those whose commits match its firmware, the default.
        #[arg(long, conflicts_with_all = ["dialects", "none"])]
        automatic: bool,
        /// A dialect it uses whatever it runs, by id or name; give it once
        /// for each.
        #[arg(long = "dialect")]
        dialects: Vec<String>,
        /// None but those for every vehicle.
        #[arg(long, conflicts_with = "dialects")]
        none: bool,
    },
    /// Removes a dialect you added, by its id.
    Remove { id: String },
}

pub async fn run(data_dir: &Path, command: MavlinkCommand) -> Result<(), CliError> {
    let MavlinkCommand::Dialects { command } = command;
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        DialectsCommand::List { vehicle, json } => {
            let vehicle_id = resolve(&mut client, vehicle).await?;
            let result = client
                .call(
                    "mavlink/dialects/list",
                    super::strip_nulls(json!({ "vehicleId": vehicle_id })),
                )
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
                return Ok(());
            }
            let dialects = result["dialects"].as_array().cloned().unwrap_or_default();
            if dialects.is_empty() {
                println!("No dialects.");
            }
            for dialect in &dialects {
                print_dialect(dialect);
            }
            Ok(())
        }
        DialectsCommand::Add {
            files,
            label,
            vehicle,
            commits,
            every_vehicle,
        } => {
            let mut given = Vec::new();
            for file in &files {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| CliError::Usage(format!("{}: {e}", file.display())))?;
                let name = file.file_name().map_or_else(
                    || file.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                given.push(json!({ "name": name, "text": text }));
            }
            let vehicle_id = resolve(&mut client, vehicle).await?;
            let params = json!({
                "files": given,
                "label": label,
                "vehicleId": vehicle_id,
                "commits": commits,
                "everyVehicle": every_vehicle,
            });
            let dialect = client
                .call("mavlink/dialects/add", super::strip_nulls(params))
                .await?;
            print_dialect(&dialect);
            Ok(())
        }
        DialectsCommand::Generate {
            checkout,
            label,
            vehicle,
            commits,
        } => {
            let mut params = checkout_dialect(&checkout, label, commits)?;
            params["vehicleId"] = json!(resolve(&mut client, vehicle).await?);
            let dialect = client
                .call("mavlink/dialects/add", super::strip_nulls(params))
                .await?;
            print_dialect(&dialect);
            Ok(())
        }
        DialectsCommand::Bind {
            id,
            commits,
            every_vehicle,
        } => {
            let params = json!({ "id": id, "commits": commits, "everyVehicle": every_vehicle });
            let dialect = client.call("mavlink/dialects/bind", params).await?;
            print_dialect(&dialect);
            Ok(())
        }
        DialectsCommand::Use {
            vehicle,
            automatic,
            dialects,
            none,
        } => use_dialects(&mut client, &vehicle, automatic, none, &dialects).await,
        DialectsCommand::Remove { id } => {
            client
                .call("mavlink/dialects/remove", json!({ "id": id }))
                .await?;
            println!("Removed {id}.");
            Ok(())
        }
    }
}

async fn resolve(
    client: &mut LocalClient,
    vehicle: Option<String>,
) -> Result<Option<String>, CliError> {
    match vehicle {
        Some(v) => Ok(Some(super::vehicle::resolve(client, &v).await?)),
        None => Ok(None),
    }
}

/// `mavlink/dialects/add`'s params for a checkout: its `ardupilotmega.xml`
/// and the files it includes from the same folder, labelled with its folder's
/// name and commit, for builds of that commit unless `commits` says.
fn checkout_dialect(
    checkout: &Path,
    label: Option<String>,
    commits: Vec<String>,
) -> Result<Value, CliError> {
    let folder = checkout.join("modules/mavlink/message_definitions/v1.0");
    let first = folder.join("ardupilotmega.xml");
    if !first.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not an ArduPilot checkout: no {} (run git submodule update --init)",
            checkout.display(),
            first.display()
        )));
    }
    let mut files: Vec<Value> = Vec::new();
    let mut read: Vec<String> = Vec::new();
    let mut waiting = vec!["ardupilotmega.xml".to_owned()];
    while let Some(name) = waiting.pop() {
        if read.contains(&name) {
            continue;
        }
        let path = folder.join(&name);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
        waiting.extend(includes(&text));
        files.push(json!({ "name": name, "text": text }));
        read.push(name);
    }
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|c| !c.is_empty());
    let name = checkout
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "ArduPilot".to_owned());
    let label = label.unwrap_or_else(|| match &commit {
        Some(c) => format!("{name} {}", &c[..c.len().min(8)]),
        None => name,
    });
    let commits = if commits.is_empty() {
        commit.into_iter().collect()
    } else {
        commits
    };
    Ok(json!({ "files": files, "label": label, "commits": commits }))
}

/// The files an XML file includes, by name, from its own folder only.
fn includes(text: &str) -> Vec<String> {
    text.split("<include>")
        .skip(1)
        .filter_map(|rest| rest.split("</include>").next())
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && !name.contains('/')
                && !name.contains('\\')
                && Path::new(name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
        })
        .map(str::to_owned)
        .collect()
}

/// `uxduck mavlink dialects use`: sets a vehicle's choice when one is
/// given, then says which dialects it uses.
async fn use_dialects(
    client: &mut LocalClient,
    vehicle: &str,
    automatic: bool,
    none: bool,
    dialects: &[String],
) -> Result<(), CliError> {
    let vehicle_id = super::vehicle::resolve(client, vehicle).await?;
    let result = if automatic || none || !dialects.is_empty() {
        let choice = if automatic {
            json!({ "type": "automatic" })
        } else {
            let all = client.call("mavlink/dialects/list", json!({})).await?;
            let mut ids = Vec::new();
            for wanted in dialects {
                ids.push(find_dialect(&all, wanted)?);
            }
            json!({ "type": "chosen", "ids": ids })
        };
        client
            .call(
                "mavlink/dialects/use",
                json!({ "vehicleId": vehicle_id, "choice": choice }),
            )
            .await?
    } else {
        client
            .call("mavlink/dialects/list", json!({ "vehicleId": vehicle_id }))
            .await?
    };
    println!("{}", choice_line(vehicle, &result));
    for dialect in result["dialects"].as_array().into_iter().flatten() {
        print_dialect(dialect);
    }
    Ok(())
}

/// A dialect named by its id or its label.
fn find_dialect(all: &Value, wanted: &str) -> Result<String, CliError> {
    let dialects = all["dialects"].as_array().cloned().unwrap_or_default();
    dialects
        .iter()
        .find(|d| d["id"] == wanted)
        .or_else(|| {
            dialects.iter().find(|d| {
                d["label"]
                    .as_str()
                    .is_some_and(|l| l.eq_ignore_ascii_case(wanted))
            })
        })
        .and_then(|d| d["id"].as_str().map(str::to_owned))
        .ok_or_else(|| CliError::Usage(format!("no dialect {wanted}")))
}

/// "Alpha uses the dialects matching its firmware's commit."
fn choice_line(vehicle: &str, result: &Value) -> String {
    let count = result["dialects"].as_array().map_or(0, Vec::len);
    let how = if result["choice"]["type"] == "chosen" {
        "the dialects chosen for it"
    } else {
        "the dialects matching its firmware's commit"
    };
    match count {
        0 => format!("{vehicle} uses {how}: none now."),
        1 => format!("{vehicle} uses {how}: 1 dialect."),
        n => format!("{vehicle} uses {how}: {n} dialects."),
    }
}

/// Which vehicles use a dialect by themselves: "commit 2a3dc4b7",
/// "every vehicle", or "the vehicles it is chosen for".
fn reach(dialect: &Value) -> String {
    if dialect["everyVehicle"] == true {
        return "every vehicle".to_owned();
    }
    let commits: Vec<&str> = dialect["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match commits.as_slice() {
        [] => "the vehicles it is chosen for".to_owned(),
        [one] => format!("builds of {one}"),
        many => format!("builds of {}", many.join(", ")),
    }
}

/// A dialect in a few lines: its name, who uses it, what it adds and
/// changes, and the standard messages UXDuck's own readers no longer read.
fn print_dialect(dialect: &Value) {
    let bound = reach(dialect);
    println!(
        "{} ({}), for {bound}: {}",
        dialect["label"].as_str().unwrap_or_default(),
        dialect["id"].as_str().unwrap_or_default(),
        changes(dialect)
    );
    for message in dialect["messages"].as_array().into_iter().flatten() {
        println!(
            "  {:>6} {} ({})",
            message["id"],
            message["name"].as_str().unwrap_or_default(),
            message["change"].as_str().unwrap_or_default()
        );
    }
    let unread: Vec<&str> = dialect["unread"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !unread.is_empty() {
        println!(
            "  UXDuck's own telemetry no longer reads {} from these vehicles.",
            unread.join(", ")
        );
    }
    if dialect["addAgain"] == true {
        println!(
            "  Added by an older UXDuck. Add it again so its commands without a place are verified."
        );
    }
}

/// "1 new, 1 changed (extensions)".
fn changes(dialect: &Value) -> String {
    let messages = dialect["messages"].as_array().cloned().unwrap_or_default();
    let count = |change: &str| messages.iter().filter(|m| m["change"] == change).count();
    let mut parts = Vec::new();
    if count("new") > 0 {
        parts.push(format!("{} new", count("new")));
    }
    if count("layout") > 0 {
        parts.push(format!("{} changed", count("layout")));
    }
    if count("extensions") > 0 {
        parts.push(format!("{} changed (extensions)", count("extensions")));
    }
    if parts.is_empty() {
        "no messages".to_owned()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_are_counted_by_kind() {
        let dialect = json!({ "messages": [
            { "change": "new" }, { "change": "extensions" }
        ]});
        assert_eq!(changes(&dialect), "1 new, 1 changed (extensions)");
        assert_eq!(changes(&json!({ "messages": [] })), "no messages");
    }

    #[test]
    fn who_uses_a_dialect() {
        assert_eq!(reach(&json!({ "everyVehicle": true })), "every vehicle");
        assert_eq!(
            reach(&json!({ "commits": ["2a3dc4b7", "9f00aa12"] })),
            "builds of 2a3dc4b7, 9f00aa12"
        );
        assert_eq!(reach(&json!({})), "the vehicles it is chosen for");
        assert_eq!(
            includes(
                "<mavlink><include>common.xml</include> <include> uAvionix.xml </include><include>../x.xml</include></mavlink>"
            ),
            ["common.xml", "uAvionix.xml"]
        );
        let all = json!({ "dialects": [{ "id": "dialect-1", "label": "Acme" }] });
        assert_eq!(find_dialect(&all, "acme").unwrap(), "dialect-1");
        assert!(find_dialect(&all, "other").is_err());
    }
}

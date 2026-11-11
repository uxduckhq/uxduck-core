//! `uxduck params definitions`: parameter definition sets for a fork's own
//! parameters (S67, ADR-0021 P5). `uxduck params sets|save|apply|export`:
//! a vehicle's saved parameter sets (S217, ADR-0037 V19).

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Subcommand;
use uxduck_contracts::pdef;
use uxduck_core::domain::param_definitions::{DefinitionSet, DefinitionSource, Firmware};

use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum ParamsCommand {
    /// What the parameters mean, per firmware: the catalogue's sets, and
    /// your own for a fork.
    Definitions {
        #[command(subcommand)]
        command: DefinitionsCommand,
    },
    /// A vehicle's saved parameter sets, newest first, with how many of its
    /// parameters differ from each now.
    Sets {
        /// The vehicle, by name or id.
        vehicle: String,
        #[arg(long)]
        json: bool,
    },
    /// Saves the vehicle's parameters as they are now under a name, or
    /// keeps a .param file as a set (nothing is written until you apply it).
    Save {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Its name; "Saved <date>", or the file's name, by default.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// A .param file to keep as a set instead.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Applies a saved set: the vehicle's values are saved as "Before
    /// <set>" first, then every parameter that differs is written.
    Apply {
        /// The vehicle, by name or id.
        vehicle: String,
        /// The set, by name or id.
        set: String,
    },
    /// Writes a saved set as a .param file.
    Export {
        /// The vehicle, by name or id.
        vehicle: String,
        /// The set, by name or id.
        set: String,
        /// Where to write it; the terminal by default.
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DefinitionsCommand {
    /// Your own sets, and the catalogue's state.
    List {
        /// Fetch the catalogue again.
        #[arg(long)]
        refresh: bool,
    },
    /// Adds a set from a file, or from the URL your CI publishes it at
    /// (fetched again every day): the `apm.pdef.json` or `apm.pdef.xml` your
    /// fork's `param_parse.py` writes, or a UXDuck set. It describes the
    /// vehicle, commit or firmware you give, ahead of ArduPilot's.
    Add {
        file: PathBuf,
        #[command(flatten)]
        bind: Bind,
    },
    /// Generates a set from an ArduPilot source tree with its own
    /// `param_parse.py` (Python 3 and lxml needed; `PYTHON` names another
    /// interpreter), and adds it, bound to the tree's commit unless you say
    /// otherwise.
    Generate {
        /// The ArduPilot checkout.
        checkout: PathBuf,
        /// `Copter`, `Plane`, `Rover`, `Sub`, `Tracker` or `Blimp`.
        #[arg(long)]
        vehicle: String,
        #[command(flatten)]
        bind: Bind,
    },
    /// Removes a set you added, by its id.
    Remove { id: String },
    /// Chooses which descriptions a vehicle uses (yours are read over
    /// ArduPilot's); without --ardupilot or --own, says which it uses now.
    Use {
        /// The vehicle, by name or id.
        vehicle: String,
        /// `automatic`, `none`, or one of the catalogue's versions for its
        /// firmware, such as `4.5.6` or `latest` (or a set's id).
        #[arg(long)]
        ardupilot: Option<String>,
        /// `automatic`, `none`, or one of your sets, by label or id.
        #[arg(long)]
        own: Option<String>,
    },
}

/// What a set describes, and what it is called.
#[derive(Debug, clap::Args)]
pub struct Bind {
    /// Its name; the firmware and version or commit by default.
    #[arg(long)]
    label: Option<String>,
    /// Describe only this vehicle, by id.
    #[arg(long)]
    vehicle_id: Option<String>,
    /// Describe vehicles built from this commit.
    #[arg(long)]
    commit: Option<String>,
    /// The firmware, when the file does not say: `Copter`, `Plane`, ...
    #[arg(long)]
    firmware: Option<String>,
    /// The version it documents, such as `4.5.7`.
    #[arg(long)]
    version: Option<String>,
}

pub async fn run(data_dir: &Path, command: ParamsCommand) -> Result<(), CliError> {
    let command = match command {
        ParamsCommand::Definitions { command } => command,
        sets => return run_sets(data_dir, sets).await,
    };
    match command {
        DefinitionsCommand::List { refresh } => {
            let params = serde_json::json!({ "refresh": refresh }).to_string();
            super::rpc_call(data_dir, "param/definitions/list", Some(&params)).await
        }
        DefinitionsCommand::Add { file, bind } => {
            // A URL a CI publishes the set at, fetched again daily (S74).
            let source = file.to_string_lossy();
            if source.starts_with("https://") || source.starts_with("http://") {
                return add_url(data_dir, &source, bind).await;
            }
            let text = std::fs::read_to_string(&file)
                .map_err(|e| CliError::Usage(format!("{}: {e}", file.display())))?;
            add(data_dir, text, bind).await
        }
        DefinitionsCommand::Generate {
            checkout,
            vehicle,
            mut bind,
        } => {
            let firmware = Firmware::parse(&vehicle).ok_or_else(|| {
                CliError::Usage(format!(
                    "--vehicle {vehicle}: Copter, Plane, Rover, Sub, Tracker or Blimp"
                ))
            })?;
            let set = generate(&checkout, firmware)?;
            if bind.commit.is_none() && bind.vehicle_id.is_none() {
                bind.commit.clone_from(&set.commit);
            }
            let text = serde_json::to_string(&set).map_err(|e| CliError::Usage(e.to_string()))?;
            add(data_dir, text, bind).await
        }
        DefinitionsCommand::Remove { id } => {
            let params = serde_json::json!({ "id": id }).to_string();
            super::rpc_call(data_dir, "param/definitions/remove", Some(&params)).await
        }
        DefinitionsCommand::Use {
            vehicle,
            ardupilot,
            own,
        } => use_sets(data_dir, &vehicle, ardupilot.as_deref(), own.as_deref()).await,
    }
}

/// `automatic` or `none`, else the set `find` names.
fn choice(text: &str, find: impl FnOnce() -> Result<String, CliError>) -> Result<Value, CliError> {
    Ok(match text.to_ascii_lowercase().as_str() {
        "automatic" | "auto" => json!({ "type": "automatic" }),
        "none" | "off" => json!({ "type": "none" }),
        _ => json!({ "type": "set", "id": find()? }),
    })
}

/// Which descriptions a vehicle uses, as the Parameters tab says it.
fn sets_line(status: &Value) -> String {
    if let Some(detail) = status["detail"].as_str() {
        return detail.to_owned();
    }
    let loading = status["status"] == "loading";
    match status["title"].as_str() {
        Some(title) if loading => format!("Loading descriptions from {title}."),
        Some(title) => format!("Descriptions from {title}."),
        None if loading => "Loading descriptions.".to_owned(),
        None => "No descriptions: names and values only.".to_owned(),
    }
}

async fn use_sets(
    data_dir: &Path,
    vehicle: &str,
    ardupilot: Option<&str>,
    own: Option<&str>,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let id = super::vehicle::resolve(&mut client, vehicle).await?;
    if ardupilot.is_none() && own.is_none() {
        let now = client
            .call("param/definitions", json!({ "vehicleId": id }))
            .await?;
        println!("{}", sets_line(&now["status"]));
        return Ok(());
    }
    let about = client.call("vehicle/get", json!({ "id": id })).await?;
    let firmware = about["vehicleType"]
        .as_str()
        .and_then(Firmware::of)
        .ok_or_else(|| {
            CliError::Usage(format!("{vehicle} has no parameter descriptions to choose"))
        })?;
    let listed = client
        .call(
            "param/definitions/list",
            json!({ "firmware": firmware.name() }),
        )
        .await?;
    let ardupilot = ardupilot
        .map(|text| {
            choice(text, || {
                listed["sets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| s["version"] == text || s["id"] == text)
                    .and_then(|s| s["id"].as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        CliError::Usage(if listed["catalogue"] == "ready" {
                            format!(
                                "The catalogue has no {} {text}: see uxduck params definitions list.",
                                firmware.tag_prefix()
                            )
                        } else {
                            "The catalogue is not here yet; try again in a moment.".to_owned()
                        })
                    })
            })
        })
        .transpose()?;
    let own = own
        .map(|text| {
            choice(text, || {
                listed["own"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| {
                        s["id"] == text
                            || s["label"]
                                .as_str()
                                .is_some_and(|l| l.eq_ignore_ascii_case(text))
                    })
                    .and_then(|s| s["id"].as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        CliError::Usage(format!(
                            "You have no set called {text:?}: see uxduck params definitions list."
                        ))
                    })
            })
        })
        .transpose()?;
    let params = super::strip_nulls(json!({
        "vehicleId": id,
        "ardupilot": ardupilot,
        "own": own,
    }));
    let status = client.call("param/definitions/use", params).await?;
    println!("{}", sets_line(&status));
    Ok(())
}

async fn add_url(data_dir: &Path, url: &str, bind: Bind) -> Result<(), CliError> {
    let firmware = bind
        .firmware
        .as_deref()
        .map(|f| {
            Firmware::parse(f)
                .map(Firmware::name)
                .ok_or_else(|| CliError::Usage(format!("--firmware {f}: Copter, Plane, ...")))
        })
        .transpose()?;
    let params = serde_json::json!({
        "url": url,
        "label": bind.label,
        "firmware": firmware,
        "version": bind.version,
        "commit": bind.commit,
        "vehicleId": bind.vehicle_id,
    });
    let params = super::strip_nulls(params).to_string();
    super::rpc_call(data_dir, "param/definitions/add", Some(&params)).await
}

async fn add(data_dir: &Path, text: String, bind: Bind) -> Result<(), CliError> {
    let firmware = bind
        .firmware
        .as_deref()
        .map(|f| {
            Firmware::parse(f)
                .map(Firmware::name)
                .ok_or_else(|| CliError::Usage(format!("--firmware {f}: Copter, Plane, ...")))
        })
        .transpose()?;
    let params = serde_json::json!({
        "text": text,
        "label": bind.label,
        "firmware": firmware,
        "version": bind.version,
        "commit": bind.commit,
        "vehicleId": bind.vehicle_id,
    });
    let params = super::strip_nulls(params).to_string();
    super::rpc_call(data_dir, "param/definitions/add", Some(&params)).await
}

/// Runs the checkout's own `param_parse.py` for both of its formats, which
/// the set needs (`uxduck_contracts::pdef`), and reads the commit.
fn generate(checkout: &Path, firmware: Firmware) -> Result<DefinitionSet, CliError> {
    // The parser runs in a temporary output folder, so a checkout supplied
    // relative to the caller must be resolved before changing directories.
    let checkout = checkout
        .canonicalize()
        .map_err(|e| CliError::Usage(format!("{}: {e}", checkout.display())))?;
    let script = checkout.join("Tools/autotest/param_metadata/param_parse.py");
    if !script.is_file() {
        return Err(CliError::Usage(format!(
            "{} is not an ArduPilot checkout: no {}",
            checkout.display(),
            script.display()
        )));
    }
    // Rover's folder was APMrover2 before 4.1.
    let folder = if firmware == Firmware::Rover && !checkout.join("Rover").is_dir() {
        "APMrover2"
    } else {
        firmware.name()
    };
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_owned());
    let work = std::env::temp_dir().join(format!("uxduck-pdef-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| CliError::Usage(e.to_string()))?;
    let result = (|| {
        for format in ["json", "xml"] {
            // `-B`: nothing written into the checkout.
            let output = Command::new(&python)
                .arg("-B")
                .arg(&script)
                .args(["--vehicle", folder, "--format", format])
                .current_dir(&work)
                .output()
                .map_err(|e| {
                    CliError::Usage(format!("cannot run python3 ({e}); Python 3 is needed"))
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let last = stderr.lines().last().unwrap_or_default();
                return Err(CliError::Usage(format!(
                    "param_parse.py --format {format} failed: {last}"
                )));
            }
        }
        let read = |name: &str| {
            std::fs::read_to_string(work.join(name))
                .map_err(|e| CliError::Usage(format!("param_parse.py wrote no {name}: {e}")))
        };
        let (_, xml) = pdef::from_xml(&read("apm.pdef.xml")?).map_err(CliError::Usage)?;
        let (_, json) = pdef::from_json(&read("apm.pdef.json")?).map_err(CliError::Usage)?;
        Ok(pdef::merge(xml, &json))
    })();
    let _ = std::fs::remove_dir_all(&work);
    let parameters = result?;
    let commit = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|c| !c.is_empty());
    let label = checkout
        .file_name()
        .map_or_else(|| "own".to_owned(), |n| n.to_string_lossy().into_owned());
    Ok(DefinitionSet {
        source: DefinitionSource::Own { label },
        firmware,
        version: None,
        commit,
        parameters,
    })
}

/// A set in one line, as `param/sets` lists it.
fn set_line(set: &Value) -> String {
    let differ = match set["differ"].as_u64() {
        Some(0) => "matches the vehicle".to_owned(),
        Some(1) => "1 parameter differs".to_owned(),
        Some(n) => format!("{n} parameters differ"),
        None => "parameters not read".to_owned(),
    };
    format!(
        "{}  {} parameters, {differ}  ({})",
        set["name"].as_str().unwrap_or_default(),
        set["count"],
        set["id"].as_str().unwrap_or_default()
    )
}

/// A set of the vehicle's by name or id.
async fn find_set(client: &mut LocalClient, vehicle: &str, set: &str) -> Result<Value, CliError> {
    let sets = client
        .call("param/sets", json!({ "vehicleId": vehicle }))
        .await?;
    let all = sets["sets"].as_array().cloned().unwrap_or_default();
    all.iter()
        .find(|s| s["id"] == set)
        .or_else(|| all.iter().find(|s| s["name"] == set))
        .or_else(|| {
            all.iter().find(|s| {
                s["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(set))
            })
        })
        .cloned()
        .ok_or_else(|| {
            CliError::Usage(format!(
                "No saved set named {set:?}: see uxduck params sets."
            ))
        })
}

#[allow(clippy::too_many_lines, reason = "one arm per command")]
async fn run_sets(data_dir: &Path, command: ParamsCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        ParamsCommand::Definitions { .. } => unreachable!("run handles definitions"),
        ParamsCommand::Sets { vehicle, json } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let sets = client
                .call("param/sets", json!({ "vehicleId": id }))
                .await?;
            if json {
                println!("{sets}");
            } else if sets["sets"].as_array().is_none_or(Vec::is_empty) {
                println!("No saved parameter sets yet.");
            } else {
                for set in sets["sets"].as_array().into_iter().flatten() {
                    println!("{}", set_line(set));
                }
            }
        }
        ParamsCommand::Save {
            vehicle,
            name,
            note,
            file,
        } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let text = file
                .as_ref()
                .map(|f| {
                    std::fs::read_to_string(f)
                        .map_err(|e| CliError::Usage(format!("{}: {e}", f.display())))
                })
                .transpose()?;
            let name = name.or_else(|| {
                file.as_ref()
                    .and_then(|f| f.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            });
            let params = super::strip_nulls(json!({
                "vehicleId": id,
                "name": name,
                "note": note,
                "text": text,
            }));
            let set = client.call("param/setSave", params).await?;
            println!("Saved {}", set_line(&set));
        }
        ParamsCommand::Apply { vehicle, set } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let set = find_set(&mut client, &id, &set).await?;
            let result = client
                .call(
                    "param/setApply",
                    json!({ "vehicleId": id, "id": set["id"], "confirm": true }),
                )
                .await?;
            let list = |key: &str| -> Vec<String> {
                result[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| {
                        v.as_str()
                            .map_or_else(|| v["name"].to_string(), str::to_owned)
                    })
                    .collect()
            };
            let written = list("written");
            if result.get("before").is_none() {
                println!("Nothing to write: the vehicle already has {}.", set["name"]);
            } else {
                println!(
                    "Wrote {} change{}; the values before are saved as {}.",
                    written.len(),
                    if written.len() == 1 { "" } else { "s" },
                    result["before"]["name"]
                );
            }
            for refused in result["refused"].as_array().into_iter().flatten() {
                println!(
                    "Not written: {}: {}",
                    refused["name"].as_str().unwrap_or_default(),
                    refused["detail"].as_str().unwrap_or_default()
                );
            }
            let skipped = list("skipped");
            if !skipped.is_empty() {
                println!("Skipped: {}", skipped.join(", "));
            }
            let reboot = list("rebootRequired");
            if !reboot.is_empty() {
                println!(
                    "{} take effect once the vehicle restarts: uxduck setup restart {vehicle}",
                    reboot.join(", ")
                );
            }
        }
        ParamsCommand::Export { vehicle, set, out } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let set = find_set(&mut client, &id, &set).await?;
            let file = client
                .call("param/setExport", json!({ "id": set["id"] }))
                .await?;
            let text = file["text"].as_str().unwrap_or_default();
            match out {
                Some(path) => {
                    std::fs::write(&path, text)
                        .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
                    println!("Wrote {}", path.display());
                }
                None => print!("{text}"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_generate_from_a_relative_checkout() {
        let current = std::env::current_dir().unwrap();
        let checkout = tempfile::Builder::new()
            .prefix("uxduck-pdef-checkout-")
            .tempdir_in(&current)
            .unwrap();
        let metadata = checkout.path().join("Tools/autotest/param_metadata");
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::write(
            metadata.join("param_parse.py"),
            "from pathlib import Path\nimport sys\nformat = sys.argv[sys.argv.index('--format') + 1]\nsource = Path(__file__).parent / ('copter.pdef.' + format)\nPath('apm.pdef.' + format).write_text(source.read_text())\n",
        )
        .unwrap();
        for (format, contents) in [
            (
                "json",
                include_str!("../../../uxduck-contracts/fixtures/pdef/copter.pdef.json"),
            ),
            (
                "xml",
                include_str!("../../../uxduck-contracts/fixtures/pdef/copter.pdef.xml"),
            ),
        ] {
            std::fs::write(metadata.join(format!("copter.pdef.{format}")), contents).unwrap();
        }
        let relative = checkout.path().strip_prefix(&current).unwrap();
        let set = generate(relative, Firmware::ArduCopter).unwrap();
        assert_eq!(set.firmware, Firmware::ArduCopter);
        assert_eq!(
            set.find("RTL_ALT", false).unwrap().units.as_deref(),
            Some("cm")
        );
        assert!(
            set.find("ATC_RAT_PIT_D", true)
                .unwrap()
                .variant
                .as_deref()
                .is_some_and(|v| v.contains("Heli")),
            "the generator merges JSON and XML variants"
        );
        assert!(
            !checkout.path().join("apm.pdef.json").exists(),
            "generated output never lands in the source checkout"
        );
    }
}

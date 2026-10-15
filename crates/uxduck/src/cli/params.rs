//! `uxduck params definitions`: parameter definition sets for a fork's own
//! parameters (S67, ADR-0021 P5).

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Subcommand;
use uxduck_contracts::pdef;
use uxduck_core::domain::param_definitions::{DefinitionSet, DefinitionSource, Firmware};

use super::client::CliError;

#[derive(Debug, Subcommand)]
pub enum ParamsCommand {
    /// What the parameters mean, per firmware: the catalogue's sets, and
    /// your own for a fork.
    Definitions {
        #[command(subcommand)]
        command: DefinitionsCommand,
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
    let ParamsCommand::Definitions { command } = command;
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
    }
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

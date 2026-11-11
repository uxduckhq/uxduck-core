//! The CLI requests missing simulators just as the Start button does.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use uxduck_contracts::dto::link::{SimAutopilot, SimVehicle};
use uxduck_contracts::dto::sim::{CatalogueEntry, SimBinariesResult, SimCatalogueResult};

use super::client::{CliError, LocalClient};

pub(super) fn program(
    vehicle: SimVehicle,
    autopilot: SimAutopilot,
) -> Result<&'static str, CliError> {
    if autopilot == SimAutopilot::Px4 {
        return match vehicle {
            SimVehicle::Multicopter | SimVehicle::Plane | SimVehicle::Vtol | SimVehicle::Rover => {
                Ok("px4")
            }
            _ => Err(CliError::Usage("PX4 does not support this vehicle".into())),
        };
    }
    Ok(match vehicle {
        SimVehicle::Multicopter => "arducopter",
        SimVehicle::Plane | SimVehicle::Vtol | SimVehicle::Jet => "arduplane",
        SimVehicle::Rover | SimVehicle::Boat => "ardurover",
        SimVehicle::Submarine => "ardusub",
        SimVehicle::Airship => "blimp",
    })
}

fn newest<'a>(
    entries: &'a [CatalogueEntry],
    firmware: &str,
    program: &str,
) -> Option<&'a CatalogueEntry> {
    entries
        .iter()
        .filter(|e| e.build.firmware == firmware && e.build.program == program)
        .max_by_key(|e| crate::sim::catalogue::build_order(&e.build.version))
}

pub async fn ensure(data_dir: &Path, vehicle: &str, autopilot: &str) -> Result<(), CliError> {
    let vehicle: SimVehicle =
        serde_json::from_value(json!(vehicle)).map_err(|e| CliError::Usage(e.to_string()))?;
    let firmware: SimAutopilot =
        serde_json::from_value(json!(autopilot)).map_err(|e| CliError::Usage(e.to_string()))?;
    let program = program(vehicle, firmware)?;
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let binaries: SimBinariesResult = decode(client.call("sim/binaries", json!({})).await?)?;
    if binaries
        .binaries
        .iter()
        .any(|b| b.autopilot == firmware && b.vehicles.contains(&vehicle))
    {
        return Ok(());
    }
    eprintln!("Looking for the simulator…");
    tokio::time::timeout(
        Duration::from_mins(15),
        download(&mut client, autopilot, program),
    )
    .await
    .map_err(|_| {
        CliError::Usage(
            "The simulator download timed out. Run the command again to resume it.".into(),
        )
    })?
}

fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, CliError> {
    serde_json::from_value(value).map_err(|e| CliError::Protocol(e.to_string()))
}

async fn catalogue(client: &mut LocalClient) -> Result<SimCatalogueResult, CliError> {
    decode(client.call("sim/catalogue", json!({})).await?)
}

async fn download(client: &mut LocalClient, firmware: &str, program: &str) -> Result<(), CliError> {
    // Join the daemon's startup prefetch rather than starting a competing refresh.
    let mut result = catalogue(client).await?;
    while result.state == "loading" {
        tokio::time::sleep(Duration::from_millis(250)).await;
        result = catalogue(client).await?;
    }
    if result.state != "ready" {
        return Err(CliError::Usage(format!(
            "Couldn't get the simulator catalogue: {}",
            result.detail.as_deref().unwrap_or("try again")
        )));
    }
    let entry = newest(&result.builds, firmware, program)
        .ok_or_else(|| CliError::Usage(format!("No {program} simulator is available for this computer. Use --binary to choose your own build.")))?;
    if entry.state == "installed" {
        return Ok(());
    }
    let id = entry.build.id.clone();
    if entry.state != "downloading" {
        // A startup prefetch can win between catalogue and download. Only
        // ignore its rejection if the daemon confirms that same download.
        if let Err(error) = client.call("sim/download", json!({"id": id})).await {
            let now = catalogue(client).await?;
            if !now.builds.iter().any(|e| {
                e.build.id == id && matches!(e.state.as_str(), "downloading" | "installed")
            }) {
                return Err(error);
            }
        }
    }
    eprintln!("Downloading {id}…");
    let mut last_progress = Instant::now();
    loop {
        let result = catalogue(client).await?;
        let entry = result
            .builds
            .iter()
            .find(|e| e.build.id == id)
            .ok_or_else(|| {
                CliError::Usage("The simulator disappeared from the catalogue. Try again.".into())
            })?;
        match entry.state.as_str() {
            "installed" => return Ok(()),
            "failed" => {
                return Err(CliError::Usage(format!(
                    "Couldn't download the simulator: {}",
                    entry.detail.as_deref().unwrap_or("try again")
                )));
            }
            "downloading" => {}
            _ => {
                return Err(CliError::Usage(
                    "The simulator download stopped. Try again.".into(),
                ));
            }
        }
        if last_progress.elapsed() >= Duration::from_secs(2) {
            eprintln!(
                "{} / {} bytes",
                entry.received_bytes.unwrap_or(0),
                entry.total_bytes
            );
            last_progress = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use uxduck_contracts::dto::sim::{CatalogueBuild, CatalogueEntry};

    fn entry(version: &str) -> CatalogueEntry {
        CatalogueEntry {
            build: CatalogueBuild {
                id: format!("ardupilot-arducopter-{version}-x86_64-linux"),
                firmware: "ardupilot".into(),
                program: "arducopter".into(),
                version: version.into(),
                platform: "x86_64-linux".into(),
                files: Vec::new(),
                source_url: String::new(),
            },
            state: "available".into(),
            received_bytes: None,
            total_bytes: 0,
            detail: None,
        }
    }

    /// The catalogue on 2026-11-07: the digest's digits made the CLI take
    /// the build from run 40438362054, which has no `--local-only`, over
    /// the newest, run 41446300470, which New simulation takes.
    #[test]
    fn the_newest_build_is_the_latest_run_as_new_simulation_takes_it() {
        let entries: Vec<CatalogueEntry> = [
            "4.7.2",
            "d5420ff97c84-uxduck-4f484623ea81-41162534597.1",
            "d5420ff97c84-uxduck-5595e8ee8c50-40438362054.1",
            "d5420ff97c84-uxduck-5cebdc8b7722-39842652339.1",
            "d5420ff97c84-uxduck-5cebdc8b7722-40306143760.1",
            "d5420ff97c84-uxduck-a1dcf436565f-41260150753.1",
            "d5420ff97c84-uxduck-a1dcf436565f-41446300470.1",
        ]
        .into_iter()
        .map(entry)
        .collect();
        let newest = super::newest(&entries, "ardupilot", "arducopter").unwrap();
        assert_eq!(
            newest.build.version,
            "d5420ff97c84-uxduck-a1dcf436565f-41446300470.1"
        );
        let releases = [entry("4.6.9"), entry("4.7.10"), entry("4.7.2")];
        let newest = super::newest(&releases, "ardupilot", "arducopter").unwrap();
        assert_eq!(newest.build.version, "4.7.10");
    }
}

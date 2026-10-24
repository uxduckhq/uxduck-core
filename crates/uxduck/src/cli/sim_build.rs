//! The CLI requests missing simulators just as the Start button does.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use uxduck_contracts::dto::link::{SimAutopilot, SimVehicle};
use uxduck_contracts::dto::sim::{CatalogueEntry, SimBinariesResult, SimCatalogueResult};

use super::client::{CliError, LocalClient};

fn program(vehicle: SimVehicle, autopilot: SimAutopilot) -> Result<&'static str, CliError> {
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
    let numbers = |version: &str| -> Vec<u64> {
        version
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|n| n.parse().ok())
            .collect()
    };
    entries
        .iter()
        .filter(|e| e.build.firmware == firmware && e.build.program == program)
        .max_by_key(|e| numbers(&e.build.version))
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

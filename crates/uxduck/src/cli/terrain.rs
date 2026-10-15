//! `uxduck terrain` (S23, ADR-0026 T6): the ground's height at a point, as
//! the map menu and the agent read it.

use std::path::Path;

use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};
use super::vehicle::resolve;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Dataset {
    Alos,
    Copernicus,
}

#[derive(Debug, Subcommand)]
pub enum TerrainCommand {
    /// The ground's height above sea level at a point, with its dataset.
    At {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// The dataset; Settings › Terrain's default when left out.
        #[arg(long, value_enum)]
        dataset: Option<Dataset>,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Asks a vehicle for its own terrain height and compares it with its
    /// dataset (S119): at a point, or at home and under it.
    Check {
        /// The vehicle, by name or id.
        #[arg(long)]
        vehicle: String,
        #[arg(long, allow_hyphen_values = true, requires = "lon")]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        lon: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Keeps an area's terrain on this computer for use offline (S120):
    /// its south-west and north-east corners.
    Keep {
        #[arg(long)]
        name: String,
        #[arg(long, allow_hyphen_values = true)]
        south: f64,
        #[arg(long, allow_hyphen_values = true)]
        west: f64,
        #[arg(long, allow_hyphen_values = true)]
        north: f64,
        #[arg(long, allow_hyphen_values = true)]
        east: f64,
    },
    /// The areas kept offline and how each is going.
    Areas {
        #[arg(long)]
        json: bool,
    },
    /// Stops keeping an area, by its id from `uxduck terrain areas`.
    Forget { id: String },
}

pub async fn run(data_dir: &Path, command: TerrainCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        TerrainCommand::At {
            lat,
            lon,
            dataset,
            json,
        } => {
            let dataset = dataset.map(|d| match d {
                Dataset::Alos => "alos",
                Dataset::Copernicus => "copernicus",
            });
            let answer = client
                .call(
                    "terrain/at",
                    json!({ "latitudeDeg": lat, "longitudeDeg": lon, "dataset": dataset }),
                )
                .await?;
            if json {
                println!("{answer}");
            } else {
                println!("{}", line(&answer));
            }
        }
        TerrainCommand::Keep {
            name,
            south,
            west,
            north,
            east,
        } => {
            let bounds =
                json!({ "southDeg": south, "westDeg": west, "northDeg": north, "eastDeg": east });
            let size = client.call("terrain/areaSize", bounds.clone()).await?;
            let mut params = bounds;
            params["name"] = json!(name);
            let area = client.call("terrain/keepArea", params).await?;
            println!(
                "Keeping {} ({}): {}",
                area["name"].as_str().unwrap_or_default(),
                area["id"].as_str().unwrap_or_default(),
                size_text(&size)
            );
        }
        TerrainCommand::Areas { json } => {
            let areas = client.call("terrain/areas", json!({})).await?;
            if json {
                println!("{areas}");
            } else {
                for line in area_lines(&areas) {
                    println!("{line}");
                }
            }
        }
        TerrainCommand::Forget { id } => {
            client
                .call("terrain/removeArea", json!({ "id": id }))
                .await?;
            println!("No longer keeping {id}");
        }
        TerrainCommand::Check {
            vehicle,
            lat,
            lon,
            json,
        } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            let answer = client
                .call(
                    "terrain/check",
                    json!({ "vehicleId": vehicle_id, "latitudeDeg": lat, "longitudeDeg": lon }),
                )
                .await?;
            if json {
                println!("{answer}");
            } else {
                for line in check_lines(&answer) {
                    println!("{line}");
                }
            }
        }
    }
    Ok(())
}

/// "64 chunks, 3.2 MB", or why the size is not known.
fn size_text(size: &Value) -> String {
    if let Some(detail) = size["detail"].as_str() {
        return detail.to_owned();
    }
    #[allow(clippy::cast_precision_loss, reason = "megabytes to show")]
    let mb = size["bytes"].as_u64().unwrap_or_default() as f64 / 1e6;
    format!(
        "{} chunks, {mb:.1} MB",
        size["chunks"].as_u64().unwrap_or_default()
    )
}

/// One line per area: its name, state and progress.
fn area_lines(areas: &Value) -> Vec<String> {
    let mut lines: Vec<String> = areas["areas"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let state = match a["state"].as_str() {
                Some("kept") => "kept".to_owned(),
                Some("fetching") => format!(
                    "fetching, {} of {}",
                    a["done"].as_u64().unwrap_or_default(),
                    a["chunks"].as_u64().unwrap_or_default()
                ),
                _ => a["detail"].as_str().unwrap_or("incomplete").to_owned(),
            };
            format!(
                "{}  {}: {state}",
                a["area"]["id"].as_str().unwrap_or_default(),
                a["area"]["name"].as_str().unwrap_or_default()
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("No areas kept offline".into());
    }
    lines
}

fn dataset_name(id: &Value) -> &'static str {
    match id.as_str() {
        Some("copernicus") => "Copernicus",
        _ => "ALOS",
    }
}

/// The verdict, then each point: the vehicle's height and its dataset's.
fn check_lines(answer: &Value) -> Vec<String> {
    let ours = dataset_name(&answer["dataset"]);
    let theirs = if ours == "ALOS" { "Copernicus" } else { "ALOS" };
    let by = answer["differenceM"].as_f64().unwrap_or_default();
    let mut lines = vec![match answer["verdict"].as_str() {
        Some("agrees") => format!("Agrees with {ours}"),
        Some("matchesOther") => format!("Matches {theirs}, not {ours} ({by:.0} m from {ours})"),
        Some("differs") => format!("Differs from {ours} by {by:.0} m"),
        Some("noTerrain") => "The vehicle has no terrain there".to_owned(),
        Some("noReply") => "The vehicle did not answer".to_owned(),
        _ => format!("{ours} has no height there to compare with"),
    }];
    for point in answer["points"].as_array().into_iter().flatten() {
        let metres = |v: &Value| v.as_f64().map_or("-".to_owned(), |m| format!("{m:.0} m"));
        lines.push(format!(
            "  {:.5}, {:.5}: vehicle {}, {ours} {}",
            point["latitudeDeg"].as_f64().unwrap_or_default(),
            point["longitudeDeg"].as_f64().unwrap_or_default(),
            metres(&point["vehicleM"]),
            metres(&point["expectedM"]),
        ));
    }
    lines
}

/// "584 m above sea level · ALOS", or why there is no height.
fn line(answer: &Value) -> String {
    let name = match answer["dataset"].as_str() {
        Some("copernicus") => "Copernicus",
        _ => "ALOS",
    };
    match (answer["heightM"].as_f64(), answer["missing"].as_str()) {
        (Some(_), _) if answer["sea"] == true => format!("Sea level · {name}"),
        (Some(height), _) => format!("{height:.0} m above sea level · {name}"),
        (None, Some("offline")) => format!("Ground height not available offline · {name}"),
        (None, Some("noData")) => format!("No ground height here · {name}"),
        (None, _) => format!(
            "No ground height: {}",
            answer["detail"]
                .as_str()
                .unwrap_or("terrain is unavailable")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_read_one_a_line() {
        let areas = json!({ "areas": [
            { "area": { "id": "ta1", "name": "Snowdonia" }, "state": "kept", "chunks": 64, "done": 64 },
            { "area": { "id": "ta2", "name": "Lakes" }, "state": "fetching", "chunks": 30, "done": 12 }
        ]});
        assert_eq!(
            area_lines(&areas),
            ["ta1  Snowdonia: kept", "ta2  Lakes: fetching, 12 of 30"]
        );
        assert_eq!(
            size_text(&json!({ "chunks": 64, "bytes": 3_200_000 })),
            "64 chunks, 3.2 MB"
        );
    }

    #[test]
    fn a_check_reads_as_a_verdict_and_its_points() {
        let lines = check_lines(&json!({
            "dataset": "alos",
            "verdict": "matchesOther",
            "differenceM": 4.2,
            "points": [{ "latitudeDeg": -35.36, "longitudeDeg": 149.16, "vehicleM": 588.0, "expectedM": 584.0, "answered": true }]
        }));
        assert_eq!(lines[0], "Matches Copernicus, not ALOS (4 m from ALOS)");
        assert_eq!(
            lines[1],
            "  -35.36000, 149.16000: vehicle 588 m, ALOS 584 m"
        );
    }

    #[test]
    fn heights_read_as_the_map_menu_reads_them() {
        assert_eq!(
            line(&json!({ "dataset": "alos", "heightM": 583.6 })),
            "584 m above sea level · ALOS"
        );
        assert_eq!(
            line(&json!({ "dataset": "copernicus", "heightM": 0.0, "sea": true })),
            "Sea level · Copernicus"
        );
        assert_eq!(
            line(&json!({ "dataset": "alos", "missing": "offline", "detail": "x" })),
            "Ground height not available offline · ALOS"
        );
        assert_eq!(
            line(&json!({ "dataset": "alos", "missing": "unavailable", "detail": "no manifest" })),
            "No ground height: no manifest"
        );
    }
}

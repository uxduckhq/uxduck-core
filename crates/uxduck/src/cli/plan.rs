//! `uxduck plan` (S125, ADR-0029): saved plans, read from and written to
//! QGroundControl and Mission Planner files, checked (S129) and uploaded
//! (S127).

use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    /// One QGroundControl `.plan` with waypoints, fence and rally points.
    Qgc,
    /// Mission Planner's waypoint, fence and rally files.
    MissionPlanner,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum VehicleType {
    Multicopter,
    Plane,
    Vtol,
    Rover,
    Boat,
    Submarine,
}

#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// The saved plans, the most recently changed first.
    List {
        /// Plans in Deleted too.
        #[arg(long)]
        deleted: bool,
        #[arg(long)]
        json: bool,
    },
    /// One plan's items, fence and rally points, by name or id.
    Show {
        plan: String,
        #[arg(long)]
        json: bool,
    },
    /// Reads files into one new plan: a `.plan`, Mission Planner's
    /// waypoint, fence and rally files together, ArduPilot's fence text or
    /// KML.
    Import {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// The vehicle type when the files do not say.
        #[arg(long, value_enum)]
        vehicle_type: Option<VehicleType>,
    },
    /// What stands in a plan's way (S129): errors stop its upload,
    /// warnings do not. With a vehicle, against that vehicle's ground.
    Check {
        plan: String,
        /// The vehicle, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Uploads a plan to a vehicle as its mission, then reads it back and
    /// says whether it matches (S127). Replaces what the vehicle holds.
    Upload {
        plan: String,
        /// The vehicle, by name or id.
        #[arg(long)]
        vehicle: String,
    },
    /// Writes a plan as files into a folder (the current one by default).
    Export {
        plan: String,
        #[arg(long, value_enum, default_value = "qgc")]
        format: Format,
        #[arg(long, default_value = ".")]
        to: PathBuf,
    },
}

#[allow(clippy::too_many_lines, reason = "one arm per command, each short")]
pub async fn run(data_dir: &Path, command: PlanCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        PlanCommand::List { deleted, json } => {
            let list = client
                .call("plan/list", json!({ "deleted": deleted }))
                .await?;
            if json {
                println!("{list}");
            } else if list["plans"].as_array().is_none_or(Vec::is_empty) {
                println!("No plans yet.");
            } else {
                for plan in list["plans"].as_array().into_iter().flatten() {
                    println!("{}", line(plan));
                }
            }
        }
        PlanCommand::Show { plan, json } => {
            let id = resolve(&mut client, &plan).await?;
            let plan = client.call("plan/get", json!({ "id": id })).await?;
            if json {
                println!("{plan}");
            } else {
                println!("{}", line(&plan));
                for (index, item) in plan["items"].as_array().into_iter().flatten().enumerate() {
                    println!(
                        "  {:>3}  command {:<5} {:>11.7} {:>12.7} {:>8.1} m",
                        index + 1,
                        item["command"],
                        item["latitudeDeg"].as_f64().unwrap_or_default(),
                        item["longitudeDeg"].as_f64().unwrap_or_default(),
                        item["altitudeM"].as_f64().unwrap_or_default(),
                    );
                }
            }
        }
        PlanCommand::Import {
            files,
            vehicle_type,
        } => {
            let mut read = Vec::new();
            for path in &files {
                let text = std::fs::read_to_string(path).map_err(|error| {
                    CliError::Usage(format!("cannot read {}: {error}", path.display()))
                })?;
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                read.push(json!({ "name": name, "text": text }));
            }
            let vehicle_type = vehicle_type.map(|t| match t {
                VehicleType::Multicopter => "multicopter",
                VehicleType::Plane => "plane",
                VehicleType::Vtol => "vtol",
                VehicleType::Rover => "rover",
                VehicleType::Boat => "boat",
                VehicleType::Submarine => "submarine",
            });
            let result = client
                .call(
                    "plan/import",
                    json!({ "files": read, "vehicleType": vehicle_type }),
                )
                .await?;
            println!("Opened {}", line(&result["plan"]));
            for skipped in result["skipped"].as_array().into_iter().flatten() {
                println!("  {}", skipped.as_str().unwrap_or_default());
            }
        }
        PlanCommand::Check {
            plan,
            vehicle,
            json,
        } => {
            let id = resolve(&mut client, &plan).await?;
            let vehicle_id = match vehicle {
                Some(v) => Some(super::vehicle::resolve(&mut client, &v).await?),
                None => None,
            };
            let result = client
                .call("plan/check", json!({ "id": id, "vehicleId": vehicle_id }))
                .await?;
            let checks = result["checks"].as_array().cloned().unwrap_or_default();
            if json {
                println!("{result}");
            } else if checks.is_empty() {
                println!("Nothing to fix.");
            } else {
                for check in &checks {
                    println!(
                        "{:<8} {}",
                        check["severity"].as_str().unwrap_or_default(),
                        check["message"].as_str().unwrap_or_default()
                    );
                }
            }
        }
        PlanCommand::Upload { plan, vehicle } => {
            let id = resolve(&mut client, &plan).await?;
            let vehicle_id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let result = client
                .call(
                    "plan/upload",
                    json!({ "planId": id, "vehicleId": vehicle_id }),
                )
                .await?;
            let status = result["status"].as_str().unwrap_or_default();
            println!(
                "{}{}",
                match status {
                    "verified" => "Uploaded and verified",
                    "different" => "Uploaded, but it read back different",
                    "cancelled" => "Cancelled",
                    _ => "Not uploaded",
                },
                result["detail"]
                    .as_str()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
            for note in result["notes"].as_array().into_iter().flatten() {
                println!("  {}", note.as_str().unwrap_or_default());
            }
        }
        PlanCommand::Export { plan, format, to } => {
            let id = resolve(&mut client, &plan).await?;
            let format = match format {
                Format::Qgc => "qgc",
                Format::MissionPlanner => "mission-planner",
            };
            let result = client
                .call("plan/export", json!({ "id": id, "format": format }))
                .await?;
            for file in result["files"].as_array().into_iter().flatten() {
                let path = to.join(file["name"].as_str().unwrap_or("plan"));
                std::fs::write(&path, file["text"].as_str().unwrap_or_default()).map_err(
                    |error| CliError::Usage(format!("cannot write {}: {error}", path.display())),
                )?;
                println!("Wrote {}", path.display());
            }
        }
    }
    Ok(())
}

/// A plan by id, or by its name when exactly one plan has it.
async fn resolve(client: &mut LocalClient, plan: &str) -> Result<String, CliError> {
    let list = client.call("plan/list", json!({ "deleted": true })).await?;
    let plans = list["plans"].as_array().cloned().unwrap_or_default();
    if plans.iter().any(|p| p["id"] == plan) {
        return Ok(plan.to_string());
    }
    let named: Vec<&Value> = plans
        .iter()
        .filter(|p| p["name"].as_str() == Some(plan))
        .collect();
    match named.as_slice() {
        [one] => Ok(one["id"].as_str().unwrap_or_default().to_string()),
        [] => Err(CliError::Usage(format!(
            "no plan named {plan}; see `uxduck plan list`"
        ))),
        _ => Err(CliError::Usage(format!(
            "more than one plan is named {plan}; use its id"
        ))),
    }
}

/// One plan in a line: "Ridge survey (plan1a2b…) · vtol · 14 items · fence".
fn line(plan: &Value) -> String {
    let mut parts = vec![
        format!(
            "{} ({})",
            plan["name"].as_str().unwrap_or_default(),
            plan["id"].as_str().unwrap_or_default()
        ),
        plan["vehicleType"].as_str().unwrap_or_default().to_string(),
        format!("{} items", plan["items"].as_array().map_or(0, Vec::len)),
    ];
    let fence = plan["fence"]["polygons"].as_array().map_or(0, Vec::len)
        + plan["fence"]["circles"].as_array().map_or(0, Vec::len);
    if fence > 0 {
        parts.push("fence".into());
    }
    let rally = plan["rally"].as_array().map_or(0, Vec::len);
    if rally > 0 {
        parts.push(if rally == 1 {
            "1 rally point".into()
        } else {
            format!("{rally} rally points")
        });
    }
    if let Some(place) = plan["placeName"].as_str() {
        parts.push(place.to_string());
    }
    if plan.get("deletedAtUs").is_some_and(Value::is_u64) {
        parts.push("in Deleted".into());
    }
    parts.join(" · ")
}

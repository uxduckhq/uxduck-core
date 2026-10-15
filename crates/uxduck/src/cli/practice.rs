//! `uxduck practice` (S100, ADR-0024): the exercises, starting one on a
//! simulated vehicle, and how it is going, from the command line.

use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};
use super::vehicle::resolve;

#[derive(Debug, Subcommand)]
pub enum PracticeCommand {
    /// The exercises, with whether the vehicle can run each and your best.
    List {
        /// A simulated vehicle, by name or id.
        #[arg(long)]
        vehicle: Option<String>,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Starts an exercise on a simulated vehicle: it starts again where the
    /// exercise needs it, then you fly it from Control.
    Start {
        /// The exercise, such as `bring-it-home`, from `uxduck practice list`.
        exercise: String,
        /// A simulated vehicle, by name or id.
        #[arg(long)]
        vehicle: String,
    },
    /// The vehicle's attempt: running, passed or failed, and why.
    Status {
        #[arg(long)]
        vehicle: String,
        #[arg(long)]
        json: bool,
    },
    /// Gives up the vehicle's attempt; the vehicle stays where it is.
    Stop {
        #[arg(long)]
        vehicle: String,
    },
    /// The Get started guide on a simulated vehicle: started on it as it
    /// is, with no restart, or the one already running.
    Guide {
        #[arg(long)]
        vehicle: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(data_dir: &Path, command: PracticeCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        PracticeCommand::List { vehicle, json } => {
            let vehicle_id = match vehicle {
                Some(wanted) => Some(resolve(&mut client, &wanted).await?),
                None => None,
            };
            let listed = client
                .call("practice/list", json!({ "vehicleId": vehicle_id }))
                .await?;
            if json {
                println!("{listed}");
                return Ok(());
            }
            for exercise in listed["exercises"].as_array().into_iter().flatten() {
                let best = exercise["best"]["line"].as_str().unwrap_or("Not yet");
                println!(
                    "{:<20} {}  ({best})",
                    exercise["id"].as_str().unwrap_or_default(),
                    exercise["title"].as_str().unwrap_or_default(),
                );
                if let Some(why) = exercise["unavailable"].as_str() {
                    println!("{:<20} {why}", "");
                }
            }
        }
        PracticeCommand::Start { exercise, vehicle } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            let started = client
                .call(
                    "practice/start",
                    json!({ "exerciseId": exercise, "vehicleId": vehicle_id }),
                )
                .await?;
            println!(
                "Started {}: the vehicle is starting again. Follow it with `uxduck practice status --vehicle {vehicle}`.",
                started["exerciseId"].as_str().unwrap_or_default()
            );
        }
        PracticeCommand::Status { vehicle, json } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            let status = client
                .call("practice/status", json!({ "vehicleId": vehicle_id }))
                .await?;
            if json {
                println!("{status}");
                return Ok(());
            }
            print_status(&status["attempt"]);
        }
        PracticeCommand::Stop { vehicle } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            client
                .call("practice/stop", json!({ "vehicleId": vehicle_id }))
                .await?;
            println!("Given up. The vehicle stays where it is.");
        }
        PracticeCommand::Guide { vehicle, json } => {
            let vehicle_id = resolve(&mut client, &vehicle).await?;
            let attempt = client
                .call("practice/guide", json!({ "vehicleId": vehicle_id }))
                .await?;
            if json {
                println!("{attempt}");
                return Ok(());
            }
            print_status(&attempt);
        }
    }
    Ok(())
}

fn print_status(attempt: &Value) {
    if attempt.is_null() {
        println!("No practice on this vehicle yet.");
        return;
    }
    let exercise = attempt["exerciseId"].as_str().unwrap_or_default();
    match attempt["state"].as_str() {
        Some("starting") => println!("{exercise}: starting the vehicle again…"),
        Some("running") => println!(
            "{exercise}: running, step {}",
            attempt["step"].as_u64().unwrap_or(0) + 1
        ),
        _ => println!(
            "{exercise}: {}",
            attempt["line"].as_str().unwrap_or("ended")
        ),
    }
}

//! `uxduck mission` (S134): a vehicle's missions over time.

use std::path::Path;

use clap::Subcommand;
use serde_json::json;

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum MissionCommand {
    /// Each different mission a vehicle has held, newest first, with how it
    /// came: found, uploaded (from which plan, verified or not), its
    /// simulation's example, or changed by another ground station.
    History {
        /// The vehicle, by name or id.
        vehicle: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(data_dir: &Path, command: MissionCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        MissionCommand::History { vehicle, json } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let result = client.call("mission/history", json!({ "id": id })).await?;
            if json {
                println!("{result}");
                return Ok(());
            }
            let records = result["records"].as_array().cloned().unwrap_or_default();
            if records.is_empty() {
                println!("No missions seen on {vehicle} yet.");
            }
            for (index, record) in records.iter().enumerate() {
                let verified = match record["origin"]["verified"].as_bool() {
                    Some(true) => " · verified",
                    Some(false) => " · read back different",
                    None => "",
                };
                println!(
                    "{}{} · {}{}",
                    if index == 0 { "Now: " } else { "" },
                    record["originText"].as_str().unwrap_or_default(),
                    record["counts"].as_str().unwrap_or_default(),
                    verified
                );
            }
        }
    }
    Ok(())
}

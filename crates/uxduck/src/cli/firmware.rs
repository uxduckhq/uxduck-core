//! `uxduck firmware` (S254, ADR-0044 F12): a fork's firmware profiles, one
//! URL its CI publishes naming its parameter descriptions and dialects.

use std::path::Path;
use std::time::Duration;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum FirmwareCommand {
    /// The firmware profiles you added, newest first.
    List {
        /// Fetch each again first.
        #[arg(long)]
        refresh: bool,
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Adds a fork's firmware profile by its URL and waits for it to be
    /// read: its parameters and dialects then describe the vehicles built
    /// from its commit.
    Add {
        url: String,
        /// What to call it, instead of the label the profile gives itself.
        #[arg(long)]
        name: Option<String>,
    },
    /// Renames a profile, with the parameters and dialect it brought.
    Rename { id: String, name: String },
    /// Removes a profile, with the parameters and dialect it brought.
    Remove { id: String },
}

pub async fn run(data_dir: &Path, command: FirmwareCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        FirmwareCommand::List { refresh, json } => {
            if refresh {
                client
                    .call("firmware/list", json!({ "refresh": true }))
                    .await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let result = client.call("firmware/list", json!({})).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
                return Ok(());
            }
            let profiles = result["profiles"].as_array().cloned().unwrap_or_default();
            if profiles.is_empty() {
                println!("No firmware profiles.");
            }
            for profile in &profiles {
                print_profile(profile);
            }
            Ok(())
        }
        FirmwareCommand::Add { url, name } => {
            let added = client
                .call("firmware/add", json!({ "url": url, "name": name }))
                .await?;
            let id = added["id"].as_str().unwrap_or_default().to_owned();
            let mut profile = added;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let listed = client.call("firmware/list", json!({})).await?;
                if let Some(found) = listed["profiles"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|p| p["id"] == id.as_str())
                {
                    profile = found.clone();
                }
                let read = profile.get("fetchedAtUs").is_some();
                let dialect_due =
                    read && profile.get("dialectId").is_none() && profile.get("problem").is_none();
                if profile.get("problem").is_some() || (read && !dialect_due) {
                    break;
                }
            }
            print_profile(&profile);
            if profile.get("fetchedAtUs").is_none() {
                return Err(CliError::Usage(format!(
                    "{url} could not be read; the profile stays added and is fetched again daily (uxduck firmware remove {id} to drop it)"
                )));
            }
            Ok(())
        }
        FirmwareCommand::Rename { id, name } => {
            let renamed = client
                .call("firmware/rename", json!({ "id": id, "name": name }))
                .await?;
            print_profile(&renamed);
            Ok(())
        }
        FirmwareCommand::Remove { id } => {
            client.call("firmware/remove", json!({ "id": id })).await?;
            println!("Removed {id}, with its parameters and dialect.");
            Ok(())
        }
    }
}

/// A profile in a few lines: its label, firmware and commit, what it
/// brought, and why its last refresh failed.
fn print_profile(profile: &Value) {
    let text = |key: &str| profile[key].as_str().unwrap_or_default();
    let firmware = [text("firmware"), text("version")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let bound = match profile["commit"].as_str() {
        Some(commit) => format!("vehicles built from {commit}"),
        None => "every vehicle of its firmware".to_owned(),
    };
    println!("{} ({}), for {bound}", text("label"), text("id"));
    println!("  {}", text("url"));
    if !firmware.is_empty() {
        println!("  {firmware}");
    }
    if let Some(set) = profile["ownSetId"].as_str() {
        println!("  Parameters: {set}");
    }
    if let Some(dialect) = profile["dialectId"].as_str() {
        println!("  Dialect: {dialect}");
    }
    if let Some(problem) = profile["problem"].as_str() {
        println!("  Last refresh failed: {problem}");
    }
}

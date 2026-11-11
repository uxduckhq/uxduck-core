//! `uxduck operation` (ADR-0039 R16): operations, the flights of one event,
//! listed, named and gathered from the command line.

use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum OperationCommand {
    /// Operations, newest first, with their flights.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Makes an operation of flights, by their ids from `flight/list`.
    Create {
        #[arg(long)]
        name: Option<String>,
        flights: Vec<String>,
    },
    /// Names an operation.
    Rename { id: String, name: String },
    /// Adds flights to an operation.
    Add { id: String, flights: Vec<String> },
    /// Takes flights out of an operation.
    Remove { id: String, flights: Vec<String> },
}

pub async fn run(data_dir: &Path, command: OperationCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let answer = match command {
        OperationCommand::List { json } => {
            let listed = client.call("operation/list", json!({})).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&listed).unwrap_or_default()
                );
            } else {
                let operations = listed["operations"].as_array().cloned().unwrap_or_default();
                if operations.is_empty() {
                    println!(
                        "No operations: they are made where two or more vehicles work together."
                    );
                }
                for operation in operations {
                    println!("{}", line(&operation));
                }
            }
            return Ok(());
        }
        OperationCommand::Create { name, flights } => {
            client
                .call(
                    "operation/create",
                    json!({ "name": name, "flightIds": flights }),
                )
                .await?
        }
        OperationCommand::Rename { id, name } => {
            client
                .call("operation/rename", json!({ "id": id, "name": name }))
                .await?
        }
        OperationCommand::Add { id, flights } => {
            client
                .call(
                    "operation/addFlights",
                    json!({ "id": id, "flightIds": flights }),
                )
                .await?
        }
        OperationCommand::Remove { id, flights } => {
            client
                .call(
                    "operation/removeFlights",
                    json!({ "id": id, "flightIds": flights }),
                )
                .await?;
            println!("Removed.");
            return Ok(());
        }
    };
    println!("{}", line(&answer));
    Ok(())
}

/// "op… Night survey: 3 flights, 2 vehicles, 1 in your account".
fn line(operation: &Value) -> String {
    let flights = operation["flightIds"].as_array().map_or(0, Vec::len);
    let name = operation["name"].as_str().unwrap_or("(made by itself)");
    format!(
        "{} {name}: {flights} flight{}, {} vehicles, {} in your account",
        operation["id"].as_str().unwrap_or("?"),
        if flights == 1 { "" } else { "s" },
        operation["vehicles"],
        operation["kept"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operation_is_one_line() {
        let operation = json!({
            "id": "op1", "name": "Night survey", "flightIds": ["a", "b", "c"],
            "vehicles": 2, "kept": 1,
        });
        assert_eq!(
            line(&operation),
            "op1 Night survey: 3 flights, 2 vehicles, 1 in your account"
        );
    }
}

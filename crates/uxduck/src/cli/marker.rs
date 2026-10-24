//! `uxduck marker` (S177, ADR-0035): named things on the map besides
//! vehicles, such as a launch site or a ground station.

use std::path::Path;

use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use super::client::{CliError, LocalClient};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Look {
    Person,
    GroundStation,
    Boat,
    Car,
    Flag,
    Pin,
}

impl Look {
    const fn wire(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::GroundStation => "ground-station",
            Self::Boat => "boat",
            Self::Car => "car",
            Self::Flag => "flag",
            Self::Pin => "pin",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum MarkerCommand {
    /// Every marker, by name.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Adds a marker: `uxduck marker add --lat 53.07 --lon -4.08 --look flag --name "Launch site"`.
    Add {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// Named after its look when not given.
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        look: Option<Look>,
        /// Degrees clockwise from north.
        #[arg(long)]
        heading: Option<f64>,
    },
    /// Changes a marker, by name or id: only what is given.
    Set {
        marker: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        look: Option<Look>,
        #[arg(long, allow_hyphen_values = true, requires = "lon")]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true, requires = "lat")]
        lon: Option<f64>,
        /// Degrees clockwise from north.
        #[arg(long, conflicts_with = "no_heading")]
        heading: Option<f64>,
        /// Takes its heading away.
        #[arg(long)]
        no_heading: bool,
    },
    /// Removes a marker, by name or id.
    Remove { marker: String },
}

pub async fn run(data_dir: &Path, command: MarkerCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        MarkerCommand::List { json } => {
            let list = client.call("marker/list", json!({})).await?;
            if json {
                println!("{list}");
            } else if list["markers"].as_array().is_none_or(Vec::is_empty) {
                println!("No markers yet.");
            } else {
                for marker in list["markers"].as_array().into_iter().flatten() {
                    println!("{}", line(marker));
                }
            }
        }
        MarkerCommand::Add {
            lat,
            lon,
            name,
            look,
            heading,
        } => {
            let mut params = json!({ "latitudeDeg": lat, "longitudeDeg": lon });
            set(&mut params, "name", name.map(Value::from));
            set(&mut params, "look", look.map(|l| Value::from(l.wire())));
            set(&mut params, "headingDeg", heading.map(Value::from));
            let marker = client.call("marker/add", params).await?;
            println!("{}", line(&marker));
        }
        MarkerCommand::Set {
            marker,
            name,
            look,
            lat,
            lon,
            heading,
            no_heading,
        } => {
            let current = find(&mut client, &marker).await?;
            let mut params = json!({ "id": current["id"], "expectedVersion": current["version"] });
            set(&mut params, "name", name.map(Value::from));
            set(&mut params, "look", look.map(|l| Value::from(l.wire())));
            set(&mut params, "latitudeDeg", lat.map(Value::from));
            set(&mut params, "longitudeDeg", lon.map(Value::from));
            if no_heading {
                set(&mut params, "headingDeg", Some(Value::Null));
            } else {
                set(&mut params, "headingDeg", heading.map(Value::from));
            }
            let marker = client.call("marker/update", params).await?;
            println!("{}", line(&marker));
        }
        MarkerCommand::Remove { marker } => {
            let current = find(&mut client, &marker).await?;
            client
                .call("marker/remove", json!({ "id": current["id"] }))
                .await?;
            println!("Removed {}.", current["name"].as_str().unwrap_or_default());
        }
    }
    Ok(())
}

fn set(params: &mut Value, key: &str, value: Option<Value>) {
    if let (Some(value), Some(map)) = (value, params.as_object_mut()) {
        Map::insert(map, key.to_owned(), value);
    }
}

/// A marker by id, or by its name when only one has it.
async fn find(client: &mut LocalClient, marker: &str) -> Result<Value, CliError> {
    let list = client.call("marker/list", json!({})).await?;
    let markers = list["markers"].as_array().cloned().unwrap_or_default();
    if let Some(one) = markers.iter().find(|m| m["id"] == marker) {
        return Ok(one.clone());
    }
    let named: Vec<&Value> = markers
        .iter()
        .filter(|m| {
            m["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(marker))
        })
        .collect();
    match named.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(CliError::Usage(format!(
            "no marker named {marker}; see `uxduck marker list`"
        ))),
        _ => Err(CliError::Usage(format!(
            "more than one marker is named {marker}; use its id"
        ))),
    }
}

/// One marker in a line: "Launch site (mk1a2b…) · flag · 53.0700000, -4.0800000 · facing 270°".
fn line(marker: &Value) -> String {
    let mut parts = vec![
        format!(
            "{} ({})",
            marker["name"].as_str().unwrap_or_default(),
            marker["id"].as_str().unwrap_or_default()
        ),
        marker["look"].as_str().unwrap_or_default().to_string(),
        format!(
            "{:.7}, {:.7}",
            marker["latitudeDeg"].as_f64().unwrap_or_default(),
            marker["longitudeDeg"].as_f64().unwrap_or_default()
        ),
    ];
    if let Some(heading) = marker["headingDeg"].as_f64() {
        parts.push(format!("facing {heading:.0}°"));
    }
    parts.join(" · ")
}

//! `uxduck find` (ADR-0018 F12, S44): what is plugged in or reachable, and
//! why anything that is not a vehicle is not, after one round.

use std::path::Path;

use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

pub async fn run(
    data_dir: &Path,
    network: bool,
    all_serial: bool,
    as_json: bool,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let scope = if network { "network" } else { "nearby" };
    eprintln!(
        "Looking for vehicles{}…",
        if network { " on this network too" } else { "" }
    );
    let found = client
        .call(
            "find/vehicles",
            json!({ "scope": scope, "allSerial": all_serial }),
        )
        .await?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&found).unwrap_or_default()
        );
        return Ok(());
    }
    let finds = found["finds"].as_array().cloned().unwrap_or_default();
    if finds.is_empty() {
        println!(
            "Nothing found. Check the cable carries data, not only power; that the vehicle is on; \
             and, for a telemetry radio, that its other end is powered. `uxduck find --all-serial` \
             looks at every serial port, `--network` at this network."
        );
    }
    for f in &finds {
        println!("{}", line(f));
    }
    Ok(())
}

fn line(f: &Value) -> String {
    let text = |v: &Value| v.as_str().unwrap_or("").to_owned();
    let what = match (&f["vehicle"], f["reason"].as_str()) {
        (v, _) if v.is_object() => {
            let name = v["name"]
                .as_str()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            format!("{} {}{name}", text(&v["vehicleType"]), v["systemId"])
        }
        (_, Some(reason)) => format!("not a vehicle: {reason}"),
        _ => "something".into(),
    };
    let detail = f["detail"]
        .as_str()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    format!(
        "{}  {what}  {}{detail}",
        text(&f["id"]),
        text(&f["evidence"])
    )
}

//! `uxduck replay` (ADR-0039 R9): a flight played back from the terminal,
//! the same replay the app shows. `uxduck replay` alone is the replay
//! open now, else the last flight; `uxduck replay fl…` one flight;
//! `uxduck replay flight.tlog` a file.

use std::path::Path;

use clap::Args;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[allow(clippy::struct_excessive_bools, reason = "command-line switches")]
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// A flight's id, as Flights and `flight/list` give it, or a tlog file.
    /// The replay open now when absent, else the last flight that ended.
    pub source: Option<String>,
    /// Starts it playing.
    #[arg(long)]
    pub play: bool,
    /// How fast: 0.1 to 16 times real time.
    #[arg(long)]
    pub speed: Option<f64>,
    /// Plays the replay open now from its start again.
    #[arg(long, conflicts_with_all = ["source", "play", "at", "close"])]
    pub restart: bool,
    /// Seconds into the replay to go to.
    #[arg(long, value_name = "SECONDS")]
    pub at: Option<f64>,
    /// Closes the replay open now and shows live.
    #[arg(long, conflicts_with_all = ["source", "play", "speed", "at"])]
    pub close: bool,
    /// Leaves UXDuck's window where it is.
    #[arg(long)]
    pub no_open: bool,
    /// The replay's state as JSON.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(data_dir: &Path, args: ReplayArgs) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    if args.close {
        client.call("replay/close", json!({})).await?;
        println!("Closed the replay.");
        return Ok(());
    }
    if args.restart {
        let mut state = client.call("replay/restart", json!({})).await?;
        if state["open"] != true {
            return Err(CliError::Usage("no replay is open".into()));
        }
        if let Some(speed) = args.speed {
            state = client
                .call("replay/speed", json!({ "speed": speed }))
                .await?;
        }
        println!("{}", summary(&state));
        return Ok(());
    }
    let params = match &args.source {
        None => {
            let state = client.call("replay/state", json!({})).await?;
            if state["open"] == true {
                return finish(&mut client, &args, state).await;
            }
            json!({ "flightId": last_flight(&mut client).await? })
        }
        Some(source) if Path::new(source).is_file() => {
            let path = std::fs::canonicalize(source)
                .map_err(|e| CliError::Usage(format!("cannot read {source}: {e}")))?;
            json!({ "path": path.display().to_string() })
        }
        Some(source)
            if Path::new(source)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tlog")) =>
        {
            return Err(CliError::Usage(format!("no file {source}")));
        }
        Some(flight) => json!({ "flightId": flight }),
    };
    let state = client.call("replay/open", params).await?;
    finish(&mut client, &args, state).await
}

/// Goes where `args` asks in the replay open now, says where it is and
/// shows UXDuck's window.
async fn finish(
    client: &mut LocalClient,
    args: &ReplayArgs,
    mut state: Value,
) -> Result<(), CliError> {
    if let Some(at) = args.at {
        let start = state["startUs"].as_u64().unwrap_or(0);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "seconds into a replay"
        )]
        let to = start + (at.max(0.0) * 1_000_000.0) as u64;
        state = client.call("replay/seek", json!({ "timeUs": to })).await?;
    }
    if let Some(speed) = args.speed {
        state = client
            .call("replay/speed", json!({ "speed": speed }))
            .await?;
    }
    if args.play {
        state = client.call("replay/play", json!({})).await?;
    }
    if args.json {
        if let Some(object) = state.as_object_mut() {
            object.remove("paths");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&state).unwrap_or_default()
        );
    } else {
        println!("{}", summary(&state));
    }
    if !args.no_open {
        show(client).await?;
    }
    Ok(())
}

/// Brings forward what shows the replay. "Open a window" is the default
/// choice, so where no desktop window is installed a page already open
/// comes forward instead of failing that the window is missing (UXD-169).
async fn show(client: &mut LocalClient) -> Result<(), CliError> {
    let desktop = client.call("desktopApp/status", json!({})).await?;
    if desktop["chosen"] == true
        && desktop["state"] != "installed"
        && client.call("app/focus", json!({})).await?["focused"] == true
    {
        return Ok(());
    }
    let opened = client.call("app/open", json!({ "reopened": true })).await?;
    if let Some(url) = opened["url"].as_str() {
        let _ = crate::adapters::browser::open(url);
    }
    Ok(())
}

/// The newest flight that has ended.
async fn last_flight(client: &mut LocalClient) -> Result<String, CliError> {
    let listed = client.call("flight/list", json!({ "limit": 10 })).await?;
    listed["flights"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["end"] != "open")
        .and_then(|f| f["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| CliError::Usage("no flight has ended on this computer yet".into()))
}

/// "Replaying Alpha: 1:12 long, at +0:00, paused." Its vehicles are all
/// replays, so the line says so once.
fn summary(state: &Value) -> String {
    let start = state["startUs"].as_u64().unwrap_or(0);
    let end = state["endUs"].as_u64().unwrap_or(start);
    let at = state["timeUs"].as_u64().unwrap_or(start);
    let names: Vec<&str> = state["vehicles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["name"].as_str())
        .collect();
    let minutes = |us: u64| format!("{}:{:02}", us / 60_000_000, us / 1_000_000 % 60);
    let who = if names.is_empty() {
        "a log with no vehicle".to_owned()
    } else {
        names.join(", ")
    };
    format!(
        "Replaying {}: {} long, at +{}, {}.",
        // Its name, when a person gave one (R21), with whom it shows.
        state["name"]
            .as_str()
            .map_or_else(|| who.clone(), |name| format!("{name} ({who})")),
        minutes(end.saturating_sub(start)),
        minutes(at.saturating_sub(start)),
        if state["playing"] == true {
            format!("playing at {}×", state["speed"])
        } else {
            "paused".to_owned()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_says_who_how_long_and_where() {
        let state = json!({
            "startUs": 0, "endUs": 72_000_000, "timeUs": 12_000_000,
            "playing": true, "speed": 5.0,
            "vehicles": [{ "name": "Alpha" }],
        });
        assert_eq!(
            summary(&state),
            "Replaying Alpha: 1:12 long, at +0:12, playing at 5.0×."
        );
        let named = json!({
            "startUs": 0, "endUs": 72_000_000, "timeUs": 0,
            "name": "Hill test", "vehicles": [{ "name": "Alpha" }, { "name": "Bravo" }],
        });
        assert_eq!(
            summary(&named),
            "Replaying Hill test (Alpha, Bravo): 1:12 long, at +0:00, paused."
        );
    }
}

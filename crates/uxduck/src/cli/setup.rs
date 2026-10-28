//! `uxduck setup` (S184, ADR-0037): a vehicle's setup rows, and calibrating
//! its compasses from the terminal while someone turns it, or from its
//! known heading (S185), its accelerometers in six positions or level
//! (S186), its radio (S187), installing firmware (S189), and a plane's
//! airspeed sensor: zeroed, its pitot checked, its ratio found in flight
//! (S190).

use std::path::Path;

use clap::Subcommand;
use serde_json::{Value, json};

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// What is calibrated on a vehicle, its compasses, and the procedure
    /// running or the last one.
    Status {
        /// The vehicle, by name or id.
        vehicle: String,
        #[arg(long)]
        json: bool,
    },
    /// Calibrates every compass: turn the vehicle slowly round every axis
    /// until each reaches 100%. Ctrl-C cancels it.
    Compass {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Cancels the calibration running instead.
        #[arg(long)]
        cancel: bool,
    },
    /// Calibrates every compass at once from the vehicle's true heading,
    /// for a vehicle too large to turn: point it, then give the heading.
    Heading {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Its true heading, degrees clockwise from true north.
        #[arg(allow_negative_numbers = true)]
        degrees: f64,
        /// Where it is, "latitude, longitude", when it has no 3D GPS fix.
        #[arg(long, allow_hyphen_values = true)]
        at: Option<String>,
    },
    /// Calibrates the accelerometers: hold the vehicle still in each of
    /// six positions as it asks, pressing Enter for each. Ctrl-C cancels.
    Accel {
        /// The vehicle, by name or id.
        vehicle: String,
    },
    /// Takes the attitude the vehicle rests in now as level: set it on a
    /// level surface first.
    Level {
        /// The vehicle, by name or id.
        vehicle: String,
    },
    /// Calibrates the radio from its channels: centre the sticks, move
    /// everything to its ends, then each stick as asked; the ranges, and
    /// any fix you accept, are written as one parameter change. Ctrl-C
    /// cancels it.
    Radio {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Accept every fix offered (a stick's channel or reversal) without
        /// asking.
        #[arg(long)]
        accept_fixes: bool,
    },
    /// A plane's airspeed sensor: zeroed with its pitot covered (on the
    /// ground, disarmed), its pitot checked by blowing into it, or its
    /// ratio found in flight by the vehicle's automatic calibration while
    /// it circles, kept once it settles. Ctrl-C cancels it.
    Airspeed {
        /// The vehicle, by name or id.
        vehicle: String,
        /// Check the pitot instead: blow gently into it.
        #[arg(long, conflicts_with = "ratio")]
        check: bool,
        /// Find the ratio in flight instead: take off first, then circle.
        #[arg(long)]
        ratio: bool,
    },
    /// Installs firmware on a board on this computer's USB: a release of
    /// ArduPilot from its firmware server for the board its bootloader
    /// names, or a .apj/.px4 file. For a vehicle, its parameters are saved
    /// to its Files first and it is restarted into its bootloader; for a
    /// serial port with no vehicle, plug the board in when asked. Ctrl-C
    /// cancels it until erasing starts.
    Firmware {
        /// The vehicle, by name or id, or a serial port such as
        /// /dev/cu.usbmodem1101 for a board with no vehicle.
        target: String,
        /// A .apj or .px4 file to install.
        #[arg(long, conflicts_with_all = ["release", "list"])]
        file: Option<std::path::PathBuf>,
        /// copter, heli, plane, rover, sub, blimp or tracker; the vehicle's
        /// own type by default.
        #[arg(long)]
        vehicle_type: Option<String>,
        /// stable, beta, dev, or an earlier stable's version (4.6.2).
        #[arg(long)]
        release: Option<String>,
        /// A variant of the board by the firmware server's name for it
        /// (CubeOrange-bdshot).
        #[arg(long)]
        platform: Option<String>,
        /// Lists the releases for the vehicle's board instead.
        #[arg(long)]
        list: bool,
    },
}

#[allow(clippy::too_many_lines, reason = "one arm per command")]
pub async fn run(data_dir: &Path, command: SetupCommand) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    match command {
        SetupCommand::Status { vehicle, json } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let state = client
                .call("setup/state", json!({ "vehicleId": id }))
                .await?;
            if json {
                println!("{state}");
            } else {
                print_state(&state);
            }
        }
        SetupCommand::Compass { vehicle, cancel } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            if cancel {
                client
                    .call("setup/cancel", json!({ "vehicleId": id }))
                    .await?;
                println!("Cancelled.");
                return Ok(());
            }
            client
                .call("setup/start", json!({ "vehicleId": id, "kind": "compass" }))
                .await?;
            println!("Keep it away from cars, metal and phones; at the field if you can.");
            follow(&mut client, &id).await?;
        }
        SetupCommand::Heading {
            vehicle,
            degrees,
            at,
        } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let mut heading = json!({ "degrees": degrees });
            if let Some(at) = at {
                let (lat, lon) = at
                    .split_once(',')
                    .and_then(|(lat, lon)| {
                        Some((
                            lat.trim().parse::<f64>().ok()?,
                            lon.trim().parse::<f64>().ok()?,
                        ))
                    })
                    .ok_or_else(|| {
                        CliError::Usage(format!(
                            "--at takes \"latitude, longitude\" in degrees, not {at:?}."
                        ))
                    })?;
                heading["latitudeDeg"] = json!(lat);
                heading["longitudeDeg"] = json!(lon);
            }
            client
                .call(
                    "setup/start",
                    json!({ "vehicleId": id, "kind": "compass-heading", "heading": heading }),
                )
                .await?;
            follow(&mut client, &id).await?;
        }
        SetupCommand::Accel { vehicle } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            client
                .call(
                    "setup/start",
                    json!({ "vehicleId": id, "kind": "accelerometer" }),
                )
                .await?;
            println!("Hold it still in each position it asks for.");
            positions(&mut client, &id).await?;
            follow(&mut client, &id).await?;
        }
        SetupCommand::Radio {
            vehicle,
            accept_fixes,
        } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            client
                .call("setup/start", json!({ "vehicleId": id, "kind": "radio" }))
                .await?;
            radio(&mut client, &id, accept_fixes).await?;
            follow(&mut client, &id).await?;
        }
        SetupCommand::Firmware {
            target,
            file,
            vehicle_type,
            release,
            platform,
            list,
        } => {
            if looks_like_a_port(&target) {
                drop(client);
                return crate::firmware::install_on_port(
                    data_dir,
                    &target,
                    file.as_deref(),
                    vehicle_type.as_deref(),
                    release.as_deref(),
                    platform.as_deref(),
                )
                .await
                .map_err(CliError::Usage);
            }
            let id = super::vehicle::resolve(&mut client, &target).await?;
            if list {
                let mut params = json!({ "vehicleId": id, "refresh": false });
                if let Some(kind) = &vehicle_type {
                    params["vehicleType"] = json!(kind);
                }
                if let Some(platform) = &platform {
                    params["platform"] = json!(platform);
                }
                let releases = client.call("setup/firmwareReleases", params).await?;
                print_releases(&releases);
                return Ok(());
            }
            let mut params = json!({ "vehicleId": id });
            if let Some(path) = file {
                use base64::Engine as _;
                let bytes = std::fs::read(&path)
                    .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
                let name = path.file_name().map_or_else(
                    || "the file".to_owned(),
                    |n| n.to_string_lossy().into_owned(),
                );
                params["file"] = json!({
                    "name": name,
                    "contents": base64::engine::general_purpose::STANDARD.encode(bytes),
                });
            } else {
                params["release"] = json!(release.as_deref().unwrap_or("stable"));
                if let Some(kind) = &vehicle_type {
                    params["vehicleType"] = json!(kind);
                }
                if let Some(platform) = &platform {
                    params["platform"] = json!(platform);
                }
            }
            client.call("setup/firmware", params).await?;
            follow_install(&mut client, &id).await?;
        }
        SetupCommand::Airspeed {
            vehicle,
            check,
            ratio,
        } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            let (kind, first) = if ratio {
                (
                    "airspeed-ratio",
                    "Fly a few circles, in LOITER or a loiter in AUTO, until the ratio settles.",
                )
            } else if check {
                ("airspeed-check", "Blow gently into the pitot.")
            } else {
                ("airspeed-zero", "Cover the pitot from the wind.")
            };
            client
                .call("setup/start", json!({ "vehicleId": id, "kind": kind }))
                .await?;
            println!("{first}");
            follow(&mut client, &id).await?;
        }
        SetupCommand::Level { vehicle } => {
            let id = super::vehicle::resolve(&mut client, &vehicle).await?;
            client
                .call("setup/start", json!({ "vehicleId": id, "kind": "level" }))
                .await?;
            follow(&mut client, &id).await?;
        }
    }
    Ok(())
}

/// The accelerometer calibration's positions (S186): each one the vehicle
/// asks for, confirmed with Enter, until it ends. Ctrl-C cancels it.
async fn positions(client: &mut LocalClient, id: &str) -> Result<(), CliError> {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut shown = String::new();
    loop {
        let state = client
            .call("setup/state", json!({ "vehicleId": id }))
            .await?;
        let procedure = &state["procedure"];
        if !matches!(
            procedure["step"].as_str().unwrap_or_default(),
            "starting" | "running"
        ) {
            return Ok(());
        }
        // PX4 finds the side itself (S191): nothing to confirm.
        if procedure["sides"].is_object() {
            println!("It finds the side itself: hold it still on each side in turn.");
            return Ok(());
        }
        let position = procedure["position"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let waiting = !position.is_empty() && procedure["placed"] != true;
        if waiting && position != shown {
            shown.clone_from(&position);
            println!("{} Then press Enter.", position_line(&position));
            tokio::select! {
                line = lines.next_line() => {
                    if line?.is_none() {
                        return Err(CliError::Usage("Standard input closed.".into()));
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    client.call("setup/cancel", json!({ "vehicleId": id })).await?;
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            let confirmed = client
                .call(
                    "setup/confirm",
                    json!({ "vehicleId": id, "position": position }),
                )
                .await;
            if let Err(error) = confirmed {
                println!("{error}");
                shown.clear();
            }
            continue;
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(300)) => {}
            _ = tokio::signal::ctrl_c() => {
                client.call("setup/cancel", json!({ "vehicleId": id })).await?;
                println!("Cancelled.");
                return Ok(());
            }
        }
    }
}

/// The radio calibration's steps (S187): Enter for the centre and the
/// ends, each stick found as it moves, then the ranges and each fix, and
/// Enter to write them. Ctrl-C cancels it.
async fn radio(client: &mut LocalClient, id: &str, accept_fixes: bool) -> Result<(), CliError> {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut shown = String::new();
    loop {
        let state = client
            .call("setup/state", json!({ "vehicleId": id }))
            .await?;
        let procedure = &state["procedure"];
        if !matches!(
            procedure["step"].as_str().unwrap_or_default(),
            "starting" | "running"
        ) {
            return Ok(());
        }
        let radio = &procedure["radio"];
        let step = radio["step"].as_str().unwrap_or_default().to_owned();
        let asked = radio["asked"].as_str().unwrap_or_default();
        let running = procedure["step"] == "running";
        let key = format!("{running}{step}{asked}");
        let prompt = match step.as_str() {
            _ if !running => None,
            "centre" => Some("Centre the sticks, throttle down, then press Enter."),
            "ends" => Some("Move every stick and switch to both ends, then press Enter."),
            "review" => Some("Press Enter to write them."),
            _ => None,
        };
        if key != shown {
            shown.clone_from(&key);
            if step == "sticks" {
                println!("{}", stick_line(asked));
            }
            if step == "review" {
                print_ranges(radio);
                if let Some(detail) = procedure["detail"].as_str() {
                    println!("{detail}");
                }
            }
            if let Some(prompt) = prompt {
                let mut accept = Vec::new();
                for fix in radio["fixes"].as_array().into_iter().flatten() {
                    let name = fix["name"].as_str().unwrap_or_default();
                    let line = format!(
                        "{name} {} → {}",
                        fix["from"].as_f64().unwrap_or_default(),
                        fix["to"].as_f64().unwrap_or_default()
                    );
                    if accept_fixes {
                        println!("Accepting {line}.");
                        accept.push(name.to_owned());
                    } else {
                        println!("Fix {line}? Type y and Enter to accept.");
                        if read_line(client, id, &mut lines).await?.trim() == "y" {
                            accept.push(name.to_owned());
                        }
                    }
                }
                if step != "review" || !accept_fixes {
                    println!("{prompt}");
                    read_line(client, id, &mut lines).await?;
                }
                let confirmed = client
                    .call(
                        "setup/confirm",
                        json!({ "vehicleId": id, "radioStep": step, "accept": accept }),
                    )
                    .await;
                if let Err(error) = confirmed {
                    println!("{error}");
                    shown.clear();
                }
                continue;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
            _ = tokio::signal::ctrl_c() => {
                client.call("setup/cancel", json!({ "vehicleId": id })).await?;
                println!("Cancelled.");
                return Ok(());
            }
        }
    }
}

/// A line from standard input; Ctrl-C cancels the procedure instead.
async fn read_line(
    client: &mut LocalClient,
    id: &str,
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>,
) -> Result<String, CliError> {
    tokio::select! {
        line = lines.next_line() => {
            line?.ok_or_else(|| CliError::Usage("Standard input closed.".into()))
        }
        _ = tokio::signal::ctrl_c() => {
            client.call("setup/cancel", json!({ "vehicleId": id })).await?;
            Err(CliError::Usage("Cancelled.".into()))
        }
    }
}

/// What to do for a stick: "Roll right, and hold it."
fn stick_line(stick: &str) -> String {
    let how = match stick {
        "roll" => "Roll right",
        "pitch" => "Push pitch forward",
        "throttle" => "Throttle up",
        "yaw" => "Yaw right",
        other => other,
    };
    format!("{how}, and hold it.")
}

/// Each channel that moved: its range, and the stick found on it.
fn print_ranges(radio: &Value) {
    for channel in radio["channels"].as_array().into_iter().flatten() {
        let (min, max) = (
            channel["min"].as_u64().unwrap_or_default(),
            channel["max"].as_u64().unwrap_or_default(),
        );
        if max.saturating_sub(min) < 400 {
            continue;
        }
        let number = channel["channel"].as_u64().unwrap_or_default();
        let stick = radio["sticks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["channel"].as_u64() == Some(number))
            .and_then(|s| s["stick"].as_str())
            .unwrap_or("");
        println!(
            "  channel {number:<2} {min}–{max} µs, centre {} {stick}",
            channel["centre"]
        );
    }
}

/// What to do for a position: "Hold it on its left side."
fn position_line(position: &str) -> String {
    let how = match position {
        "level" => "level, as it sits to fly",
        "left" => "on its left side",
        "right" => "on its right side",
        "nose-down" => "nose down",
        "nose-up" => "nose up",
        "back" => "on its back, upside down",
        other => other,
    };
    format!("Hold it {how}.")
}

/// A serial port rather than a vehicle's name.
fn looks_like_a_port(target: &str) -> bool {
    target.starts_with("/dev/")
        || target
            .strip_prefix("COM")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn print_releases(releases: &Value) {
    if let Some(installed) = releases["installed"].as_str() {
        println!("Installed: {installed}");
    }
    if let Some(platform) = releases["platform"].as_str() {
        let id = releases["boardId"].as_u64().unwrap_or_default();
        println!("Board: {platform} ({id})");
    }
    for release in releases["releases"].as_array().into_iter().flatten() {
        let channel = match release["channel"].as_str().unwrap_or_default() {
            "stable" => "Stable",
            "beta" => "Beta",
            "dev" => "Dev",
            _ => "Older",
        };
        println!(
            "  {channel:<7} {}",
            release["version"].as_str().unwrap_or_default()
        );
    }
    if let Some(detail) = releases["detail"].as_str() {
        println!("{detail}");
    }
}

/// A firmware install's stage in words, with its progress.
pub(crate) fn install_line(firmware: &Value) -> String {
    let percent = |done: u64, total: u64| {
        (done.min(total) * 100)
            .checked_div(total)
            .map_or_else(String::new, |p| format!(" {p}%"))
    };
    let done = firmware["done"].as_u64().unwrap_or_default();
    let total = firmware["total"].as_u64().unwrap_or_default();
    match firmware["stage"].as_str().unwrap_or_default() {
        "restarting" => "Restarting into its bootloader…".into(),
        "waiting" => "Waiting for its bootloader…".into(),
        "downloading" => format!("Downloading{}", percent(done, total)),
        "erasing" => "Erasing…".into(),
        "writing" => format!("Writing{}", percent(done, total)),
        "verifying" => "Verifying…".into(),
        "starting" => "Restarting…".into(),
        "reconnecting" => "Reconnecting…".into(),
        other => other.to_owned(),
    }
}

/// Follows an install until it ends; Ctrl-C cancels it while it can be.
async fn follow_install(client: &mut LocalClient, id: &str) -> Result<(), CliError> {
    let mut last = String::new();
    loop {
        let state = client
            .call("setup/state", json!({ "vehicleId": id }))
            .await?;
        let procedure = &state["procedure"];
        let step = procedure["step"].as_str().unwrap_or_default();
        let mut line = install_line(&procedure["firmware"]);
        if step == "running" || step == "starting" {
            if procedure["instruction"] == "replug" {
                line.push_str(" Unplug the board and plug it back in.");
            }
            if line != last {
                println!("{line}");
                last = line;
            }
        } else {
            if let Some(detail) = procedure["detail"].as_str() {
                println!("{detail}");
            }
            let firmware = &procedure["firmware"];
            if step == "succeeded" && firmware["failure"].is_null() {
                println!(
                    "Installed: {} → {}.",
                    firmware["before"].as_str().unwrap_or("unknown"),
                    firmware["after"].as_str().unwrap_or("unknown")
                );
            }
            if let Some(file) = firmware["parameters"].as_str() {
                println!("Its parameters from before are in its Files: {file}");
            }
            return if step == "succeeded" {
                Ok(())
            } else {
                Err(CliError::Usage(format!("The install {step}.")))
            };
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            _ = tokio::signal::ctrl_c() => {
                match client.call("setup/cancel", json!({ "vehicleId": id })).await {
                    Ok(_) => {
                        println!("Cancelled: the board keeps its firmware.");
                        return Ok(());
                    }
                    Err(error) => println!("{error}"),
                }
            }
        }
    }
}

/// Prints the procedure as it goes, until it ends; Ctrl-C cancels it.
async fn follow(client: &mut LocalClient, id: &str) -> Result<(), CliError> {
    let mut last = String::new();
    loop {
        let state = client
            .call("setup/state", json!({ "vehicleId": id }))
            .await?;
        let procedure = &state["procedure"];
        let line = progress_line(procedure);
        if line != last {
            println!("{line}");
            last = line;
        }
        let step = procedure["step"].as_str().unwrap_or_default();
        if !matches!(step, "starting" | "running") {
            for sensor in procedure["sensors"].as_array().into_iter().flatten() {
                println!("  {}", sensor_line(sensor));
            }
            if let Some(detail) = procedure["detail"].as_str() {
                println!("{detail}");
            }
            if let Some(words) = procedure["instruction"].as_str().and_then(instruction) {
                println!("{words}");
            }
            return if step == "succeeded" {
                Ok(())
            } else {
                Err(CliError::Usage(format!("The calibration {step}.")))
            };
        }
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            _ = tokio::signal::ctrl_c() => {
                client.call("setup/cancel", json!({ "vehicleId": id })).await?;
                println!("Cancelled.");
                return Ok(());
            }
        }
    }
}

fn progress_line(procedure: &Value) -> String {
    let sensors: Vec<String> = procedure["sensors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            format!(
                "compass {} {}%",
                s["index"].as_u64().unwrap_or_default() + 1,
                s["progress"].as_u64().unwrap_or_default()
            )
        })
        .collect();
    let kind = procedure["kind"].as_str().unwrap_or_default();
    if kind == "firmware" {
        return match procedure["step"].as_str().unwrap_or_default() {
            "starting" | "running" => install_line(&procedure["firmware"]),
            "succeeded" => format!(
                "Firmware installed: {} → {}.",
                procedure["firmware"]["before"]
                    .as_str()
                    .unwrap_or("unknown"),
                procedure["firmware"]["after"].as_str().unwrap_or("unknown")
            ),
            "cancelled" => "Firmware install cancelled.".into(),
            _ => format!(
                "Firmware install failed: {}",
                procedure["detail"].as_str().unwrap_or("see its history")
            ),
        };
    }
    if kind.starts_with("airspeed-") {
        return airspeed_line(procedure);
    }
    // PX4's sides (S191): the one it measures, and those done.
    if procedure["step"] == "running" && procedure["sides"].is_object() {
        return sides_line(procedure);
    }
    match procedure["step"].as_str().unwrap_or_default() {
        // PX4 answers before it levels (S191).
        "starting" | "running" if kind == "level" => "Levelling…".into(),
        "running" if kind == "radio" => "Writing…".into(),
        "succeeded" if kind == "radio" => "Calibrated and written.".into(),
        "succeeded" if kind == "level" => "Level set.".into(),
        "running" if kind == "accelerometer" => match procedure["position"].as_str() {
            Some(_) if procedure["placed"] == true => "Measuring: hold it still.".into(),
            Some(position) => position_line(position),
            None => "Waiting for the vehicle.".into(),
        },
        "starting" if procedure["kind"] == "compass-heading" => "Calibrating…".into(),
        "starting" => "Starting…".into(),
        "succeeded" if procedure["kind"] == "compass-heading" => format!(
            "Calibrated from {}°.",
            procedure["heading"]["degrees"].as_f64().unwrap_or_default()
        ),
        "running" if sensors.is_empty() => "Turn it slowly round every axis.".into(),
        "running" => format!("Turning: {}", sensors.join(" · ")),
        "succeeded" => "Calibrated.".into(),
        "cancelled" => "Cancelled.".into(),
        _ => "Failed.".into(),
    }
}

/// A PX4 calibration's sides in a line (S191): "Hold it nose down still
/// (2 of 6 sides done, 33%)".
fn sides_line(procedure: &Value) -> String {
    let sides = &procedure["sides"];
    let count = |key: &str| sides[key].as_array().map_or(0, Vec::len);
    let done = format!(
        "{} of {} sides done, {}%",
        count("done"),
        count("needed"),
        procedure["progress"].as_u64().unwrap_or_default()
    );
    let side = procedure["position"].as_str();
    let what = match (procedure["instruction"].as_str(), side) {
        (Some("turn-on-this-side"), Some(side)) => {
            format!(
                "{} Turn it slowly round, keeping it so.",
                position_line(side)
            )
        }
        (_, Some(side)) => format!("{} Measuring: hold it still.", position_line(side)),
        _ => "Hold it still on a side not done yet.".to_owned(),
    };
    format!("{what} ({done})")
}

/// The airspeed's zero, check or ratio in a line (S190).
fn airspeed_line(procedure: &Value) -> String {
    let run = &procedure["airspeed"];
    let mps = |key: &str| run[key].as_f64().map(|v| format!("{v:.1} m/s"));
    let ratio = |key: &str| run[key].as_f64().map(|v| format!("{v:.3}"));
    let step = procedure["step"].as_str().unwrap_or_default();
    match (procedure["kind"].as_str().unwrap_or_default(), step) {
        // PX4's asks for a blow after zeroing (S191).
        ("airspeed-zero", "running") if procedure["instruction"] == "blow-into-pitot" => {
            "Zeroed: now blow gently into the pitot…".into()
        }
        ("airspeed-zero", "starting" | "running") => "Zeroing: keep the pitot covered…".into(),
        // Its reading settles over the next second: the Setup tab shows it.
        ("airspeed-zero", "succeeded") => "Zeroed.".into(),
        ("airspeed-check", "running") => format!(
            "Blow gently into the pitot: {} so far.",
            mps("peakMps").unwrap_or_else(|| "nothing".into())
        ),
        ("airspeed-check", "succeeded") => format!(
            "The pitot works: it read {}.",
            mps("peakMps").unwrap_or_default()
        ),
        ("airspeed-ratio", "starting") => "Turning on its automatic airspeed calibration…".into(),
        ("airspeed-ratio", "running") if run["settled"] == true => format!(
            "Settled at {}: keeping it…",
            ratio("ratio").unwrap_or_default()
        ),
        ("airspeed-ratio", "running") => match ratio("ratio") {
            Some(now) => format!(
                "Circling: ratio {now}, moved {}% over {} s.",
                run["spreadPct"]
                    .as_f64()
                    .map_or("?".into(), |s| format!("{s:.1}")),
                run["gatheredS"].as_u64().unwrap_or_default()
            ),
            None => "Circling: waiting for its estimate…".into(),
        },
        ("airspeed-ratio", "succeeded") => format!(
            "Ratio kept: {} (was {}).",
            ratio("ratio").unwrap_or_default(),
            ratio("ratioBefore").unwrap_or_else(|| "unknown".into())
        ),
        (_, "cancelled") => "Cancelled.".into(),
        _ => "Failed.".into(),
    }
}

fn sensor_line(sensor: &Value) -> String {
    let number = sensor["index"].as_u64().unwrap_or_default() + 1;
    let fitness = sensor["fitness"]
        .as_f64()
        .map(|f| format!(" (fitness {f:.1})"))
        .unwrap_or_default();
    match sensor["state"].as_str() {
        Some("succeeded") => {
            let quality = match sensor["quality"].as_str() {
                Some("good") => "Good",
                Some("acceptable") => "Acceptable",
                Some("poor") => "Poor",
                _ => "Calibrated",
            };
            format!("Compass {number}: {quality}{fitness}")
        }
        Some("failed") => {
            let why = match sensor["failure"].as_str() {
                Some("bad-orientation") => "its orientation looks wrong",
                Some("bad-radius") => "the field it measured is wrong",
                Some("offsets") => "its offsets are too large",
                Some("scaling") => "its scaling is out of range",
                Some("residuals") => "too much is left after the fit",
                _ => "no good fit",
            };
            format!("Compass {number}: failed, {why}{fitness}")
        }
        _ => format!("Compass {number}: did not finish"),
    }
}

fn instruction(code: &str) -> Option<&'static str> {
    Some(match code {
        "turn-around" => "Turn it slowly round every axis.",
        "restart" => "Restart it to use the new calibration: hold Restart in its Setup tab.",
        "turn-slower" => "Try again, turning it more slowly and steadily.",
        "check-orientation" => "Check how the compass is mounted (COMPASS_ORIENT), then try again.",
        "move-away" => {
            "Move away from metal and magnets and try again; if it fails again, the compass may be faulty."
        }
        "try-again" => "Try again.",
        "give-position" => "Give its position: --at \"latitude, longitude\".",
        "place-as-shown" => "Hold it in the position asked for, then press Enter.",
        "hold-still" => "Hold it still.",
        "set-level" => "Set it on a level surface, or as it flies level, and try again.",
        "centre-sticks" => "Centre the sticks, throttle down.",
        "move-to-ends" => "Move every stick and switch to both ends.",
        "move-stick" => "Move the stick asked for.",
        "check-radio" => "Check the ranges and fixes, then write them.",
        "replug" => "Unplug the board and plug it back in.",
        "keep-plugged" => "Keep it plugged in.",
        "cover-pitot" => "Cover the pitot from the wind.",
        "blow-into-pitot" => "Blow gently into the pitot.",
        "check-pitot" => {
            "Check the pitot's tubes are clear and not swapped, and the sensor's wiring, then try again."
        }
        "fly-circles" => "Fly a few circles, in LOITER or a loiter in AUTO.",
        "hold-on-a-side" => "Hold it still on a side not done yet: it finds the side itself.",
        "turn-on-this-side" => "Keep that side down and turn it slowly round.",
        _ => return None,
    })
}

fn print_state(state: &Value) {
    for row in state["rows"].as_array().into_iter().flatten() {
        let name = match row["row"].as_str().unwrap_or_default() {
            "firmware" => "Firmware",
            "accelerometer" => "Accelerometer",
            "compass" => "Compass",
            "radio" => "Radio",
            "airspeed" => "Airspeed",
            "motors" => "Motors",
            other => other,
        };
        let words = match row["state"].as_str().unwrap_or_default() {
            "running" => "Calibrating".to_owned(),
            "not-calibrated" => "Not calibrated".to_owned(),
            "needs-restart" => "Needs a restart".to_owned(),
            "calibrated" => "Calibrated".to_owned(),
            "installed" => format!("Installed {}", row["detail"].as_str().unwrap_or_default()),
            _ => "Unknown".to_owned(),
        };
        println!("{name:<14} {words}");
    }
    for compass in state["compasses"].as_array().into_iter().flatten() {
        let place = match compass["external"].as_bool() {
            Some(true) => "external",
            Some(false) => "internal",
            None => "",
        };
        let used = if compass["used"].as_bool().unwrap_or(true) {
            "used"
        } else {
            "not used"
        };
        println!(
            "  compass {} · {place} · {used} · id {}",
            compass["index"].as_u64().unwrap_or_default() + 1,
            compass["deviceId"]
        );
    }
    if state["procedure"].is_object() {
        println!("Last: {}", progress_line(&state["procedure"]));
    }
}

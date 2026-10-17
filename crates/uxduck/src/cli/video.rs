//! `uxduck video` (S38, ADR-0017 V15): the cameras UXDuck receives, and a
//! probe that says what is arriving on a port and why it does or does not
//! play.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use clap::Subcommand;
use serde_json::{Value, json};
use uxduck_media::source::{Source, Status};
use uxduck_media::udp::Container;

use super::client::{CliError, LocalClient};

#[derive(Debug, Subcommand)]
pub enum VideoCommand {
    /// Webcams, USB cameras and capture adapters attached to UXDuck's computer.
    Devices {
        #[arg(long)]
        json: bool,
    },
    /// Capture a local camera in the daemon, including with every window closed.
    Capture {
        /// Exact id from `uxduck video devices`, or an unambiguous camera name.
        device: String,
        #[arg(long, default_value = "auto", value_parser = ["auto", "pal", "ntsc"])]
        standard: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        frame_rate: Option<f64>,
        /// Bob deinterlace each field (on automatically for PAL/NTSC).
        #[arg(long)]
        deinterlace: bool,
    },
    /// Pause a configured local camera, closing its device and recording segment.
    Pause { source: String },
    /// Resume a local camera, reconnecting when it is plugged in.
    Resume { source: String },
    /// Start recording a camera; it continues with every UI window closed.
    Record {
        source: String,
        #[arg(long)]
        stop: bool,
    },
    /// The camera streams arriving at UXDuck, with their codec, picture and
    /// counters.
    List {
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// What is arriving on a UDP port and why it is or is not playable.
    /// Reads UXDuck's own counters when it listens there; otherwise listens
    /// itself for a few seconds.
    Probe {
        #[arg(long, default_value_t = 5600)]
        port: u16,
        /// How long to listen when UXDuck is not.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Adds a camera by hand: an RTSP address, such as
    /// `rtsp://admin:password@192.168.144.25/main`, or another UDP port to
    /// listen on, such as `udp:5601`. A camera with several video tracks
    /// plays its first; `#track=2` on the address plays the second, and
    /// `uxduck video list` shows what each camera offers.
    Add { url: String },
    /// Removes a camera added by hand (its id from `uxduck video list`) or a
    /// UDP port (`udp:5601`).
    Remove { what: String },
    /// Recordings, newest first, with where each file is (S40).
    Recordings {
        /// The answer as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(data_dir: &Path, command: VideoCommand) -> Result<(), CliError> {
    match command {
        VideoCommand::Devices { json: as_json } => print_devices(data_dir, as_json).await,
        VideoCommand::Capture {
            device,
            standard,
            width,
            height,
            frame_rate,
            deinterlace,
        } => {
            capture(
                data_dir,
                &device,
                &standard,
                width,
                height,
                frame_rate,
                deinterlace,
            )
            .await
        }
        VideoCommand::Pause { source } => enable(data_dir, source, false).await,
        VideoCommand::Resume { source } => enable(data_dir, source, true).await,
        VideoCommand::Record { source, stop } => {
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            client
                .call("video/record", json!({ "sourceId": source, "on": !stop }))
                .await?;
            println!(
                "Recording {}.",
                if stop {
                    "stopped"
                } else {
                    "started; closing the UI does not stop it"
                }
            );
            Ok(())
        }
        VideoCommand::List { json: as_json } => {
            let listed = list(data_dir).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&listed).unwrap_or_default()
                );
            } else {
                print_list(&listed);
            }
            Ok(())
        }
        VideoCommand::Probe { port, seconds } => probe(data_dir, port, seconds).await,
        VideoCommand::Add { url } => {
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            let added = client.call("video/add", json!({ "url": url })).await?;
            match (added["source"]["id"].as_str(), added["port"].as_u64()) {
                (Some(id), _) => println!(
                    "Added {id}, connecting to {}.",
                    added["source"]["url"].as_str().unwrap_or("")
                ),
                (None, Some(port)) => println!("Listening for video on UDP {port}."),
                _ => println!("Added."),
            }
            Ok(())
        }
        VideoCommand::Recordings { json: as_json } => print_recordings(data_dir, as_json).await,
        VideoCommand::Remove { what } => {
            let params = match what
                .strip_prefix("udp:")
                .map(|p| p.trim_start_matches("//:"))
            {
                Some(port) => json!({
                    "port": port
                        .parse::<u16>()
                        .map_err(|_| CliError::Usage(format!("{port} is not a port")))?
                }),
                None => json!({ "sourceId": what }),
            };
            let mut client = LocalClient::connect(data_dir).await?;
            client.initialize().await?;
            client.call("video/remove", params).await?;
            println!("Removed.");
            Ok(())
        }
    }
}

async fn list(data_dir: &Path) -> Result<Value, CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client.call("video/list", json!({})).await
}

fn print_list(listed: &Value) {
    match (listed["port"].as_u64(), listed["listenError"].as_str()) {
        (Some(port), _) => println!("Listening for video on UDP {port}."),
        (None, Some(error)) => println!("Not listening for video: {error}."),
        (None, None) => println!("Not listening for video."),
    }
    for extra in listed["extraPorts"].as_array().into_iter().flatten() {
        match extra["error"].as_str() {
            Some(error) => println!("Not listening on UDP {}: {error}.", extra["port"]),
            None => println!("Also listening on UDP {} (added by hand).", extra["port"]),
        }
    }
    let sources = listed["sources"].as_array().cloned().unwrap_or_default();
    if sources.is_empty() {
        println!("No camera has sent anything yet.");
    }
    for source in &sources {
        print_source(source);
    }
    for socket in listed["sockets"].as_array().into_iter().flatten() {
        println!(
            "Watched: {}, {} sent, {} dropped",
            socket["sourceId"].as_str().unwrap_or(""),
            socket["sent"],
            socket["dropped"]
        );
    }
}

fn print_source(s: &Value) {
    let text = |key: &str| s[key].as_str().unwrap_or("").to_owned();
    if let Some(url) = s["url"].as_str() {
        println!("{}  added by hand: {url}", text("id"));
    }
    let picture = match (s["width"].as_u64(), s["height"].as_u64()) {
        (Some(w), Some(h)) => format!(" {w}×{h}"),
        _ => String::new(),
    };
    let rate = s["frameRate"]
        .as_f64()
        .map(|r| format!(" {r:.0} fps"))
        .unwrap_or_default();
    println!(
        "{}  {}  from {} on UDP {}, {}{picture}{rate}  {}",
        text("id"),
        text("state"),
        text("sender"),
        s["port"],
        s["codec"].as_str().unwrap_or("codec not known yet"),
        s["vehicleId"]
            .as_str()
            .map(|v| format!("vehicle {v}"))
            .unwrap_or_default(),
    );
    if let Some(detail) = s["stateDetail"].as_str() {
        println!("  {detail}");
    }
    for track in s["tracks"].as_array().into_iter().flatten() {
        let size = match (track["width"].as_u64(), track["height"].as_u64()) {
            (Some(w), Some(h)) => format!(" {w}×{h}"),
            _ => String::new(),
        };
        let rate = track["frameRate"]
            .as_f64()
            .map(|r| format!(" {r:.0} fps"))
            .unwrap_or_default();
        println!(
            "  track {}: {}{size}{rate}{}",
            track["number"],
            track["codec"].as_str().unwrap_or(""),
            if track["playing"] == true {
                "  (playing)"
            } else {
                ""
            }
        );
    }
    let c = &s["counters"];
    println!(
        "  {} datagrams, {} pictures ({} keyframes), {} lost, {} damaged, {} NAL units dropped, {} PES lengths wrong (by up to {} bytes)",
        c["datagrams"],
        c["pictures"],
        c["keyframes"],
        c["lost"],
        c["damaged"],
        c["nalsDropped"],
        c["pesLengthMismatches"],
        c["pesLengthMaxExcess"],
    );
}

async fn probe(data_dir: &Path, port: u16, seconds: u64) -> Result<(), CliError> {
    // UXDuck holds the port: its own counters say it all.
    if let Ok(listed) = list(data_dir).await
        && listed["port"].as_u64() == Some(u64::from(port))
    {
        let sources: Vec<Value> = listed["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["port"].as_u64() == Some(u64::from(port)))
            .cloned()
            .collect();
        println!("UXDuck listens on UDP {port}.");
        if sources.is_empty() {
            println!("Nothing has arrived on it.");
        }
        for source in &sources {
            print_source(source);
        }
        return Ok(());
    }
    let socket = crate::video::listener::bind(port).map_err(CliError::Usage)?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    println!("Listening on UDP {port} for {seconds} s…");
    let mut senders: BTreeMap<String, Source> = BTreeMap::new();
    let mut buffer = vec![0u8; 65_536];
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        if let Ok((len, from)) = socket.recv_from(&mut buffer) {
            senders
                .entry(from.to_string())
                .or_default()
                .push(&buffer[..len]);
        }
    }
    if senders.is_empty() {
        println!(
            "Nothing arrived. Check the camera sends to this computer's address on UDP {port}."
        );
    }
    for (sender, source) in &senders {
        print_status(sender, &source.status());
    }
    Ok(())
}

fn print_status(sender: &str, s: &Status) {
    let container = match s.container {
        Some(Container::RtpTs) => "RTP carrying MPEG-TS",
        Some(Container::RawTs) => "MPEG-TS",
        Some(Container::RtpVideo) => "RTP, its codec not known yet",
        Some(Container::RtpH264) => "RTP carrying H.264",
        Some(Container::RtpH265) => "RTP carrying H.265",
        None => "neither MPEG-TS nor RTP",
    };
    println!("From {sender}: {container}");
    match (&s.unsupported, &s.codec) {
        (Some(why), _) => println!("  Not playable: {why}"),
        (None, Some(codec)) => println!(
            "  Playable: {codec}{}",
            match (s.width, s.height) {
                (Some(w), Some(h)) => format!(", {w}×{h}"),
                _ => String::new(),
            }
        ),
        (None, None) => println!("  No keyframe with its parameter sets yet: listen longer."),
    }
    let c = &s.counters;
    println!(
        "  {} datagrams, {} pictures ({} keyframes), {} lost, {} damaged, {} NAL units dropped, {} PES lengths wrong (by up to {} bytes){}",
        c.datagrams,
        c.pictures,
        c.keyframes,
        c.lost,
        c.damaged,
        c.nals_dropped,
        c.pes_length_mismatches,
        c.pes_length_max_excess,
        if s.klv { ", with KLV" } else { "" },
    );
}

async fn enable(data_dir: &Path, source: String, enabled: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    client
        .call(
            "video/capture/enable",
            json!({ "sourceId": source, "enabled": enabled }),
        )
        .await?;
    println!("Capture {}.", if enabled { "resuming" } else { "paused" });
    Ok(())
}

async fn devices(
    client: &mut LocalClient,
) -> Result<uxduck_contracts::dto::video::VideoDevicesResult, CliError> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let result = client.call("video/devices", json!({})).await?;
        let listed: uxduck_contracts::dto::video::VideoDevicesResult =
            serde_json::from_value(result).map_err(|e| CliError::Usage(e.to_string()))?;
        if !listed.scanning {
            if let Some(error) = &listed.error {
                return Err(CliError::Usage(error.clone()));
            }
            return Ok(listed);
        }
        if Instant::now() >= until {
            return Err(CliError::Usage(
                "Camera discovery timed out. Check the capture helper and device driver.".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn print_devices(data_dir: &Path, as_json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let devices = devices(&mut client).await?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&devices).unwrap_or_default()
        );
    } else if devices.devices.is_empty() {
        println!("No local cameras found. Connect a camera and check its OS driver.");
    } else {
        for device in devices.devices {
            println!(
                "{}  {}{}",
                device.id,
                device.name,
                if device.persistent {
                    ""
                } else {
                    " (device number; verify after reconnecting)"
                }
            );
        }
    }
    Ok(())
}

async fn capture(
    data_dir: &Path,
    device: &str,
    standard: &str,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f64>,
    deinterlace: bool,
) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let listed = devices(&mut client).await?;
    let matches: Vec<_> = listed
        .devices
        .iter()
        .filter(|d| d.id == device || d.name == device)
        .collect();
    if matches.len() != 1 {
        return Err(CliError::Usage(
            "Choose one exact device id from uxduck video devices.".into(),
        ));
    }
    let selected = matches[0];
    let source = client.call("video/capture", json!({ "input": {
                "deviceId": selected.id, "name": selected.name, "enabled": true, "standard": standard,
                "width": width, "height": height, "frameRate": frame_rate, "deinterlace": deinterlace || standard != "auto"
            }})).await?;
    println!(
        "{}  {}",
        source["id"].as_str().unwrap_or(""),
        source["name"].as_str().unwrap_or("")
    );
    Ok(())
}

async fn print_recordings(data_dir: &Path, as_json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(data_dir).await?;
    client.initialize().await?;
    let state = client
        .call(
            "state/subscribe",
            json!({ "entities": ["video-recording"] }),
        )
        .await?;
    let mut all = state["snapshot"]["videoRecordings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    all.sort_by_key(|r| std::cmp::Reverse(r["startedAtUs"].as_u64().unwrap_or(0)));
    if as_json {
        println!("{}", serde_json::to_string_pretty(&all).unwrap_or_default());
        return Ok(());
    }
    if all.is_empty() {
        println!("No recordings yet.");
    }
    for r in &all {
        let id = r["id"].as_str().unwrap_or("");
        let seconds = r["durationUs"].as_u64().unwrap_or(0) / 1_000_000;
        println!(
            "{id}  {}  {}:{:02}  {} MB  {}",
            r["state"].as_str().unwrap_or(""),
            seconds / 60,
            seconds % 60,
            r["bytes"].as_u64().unwrap_or(0) / 1_000_000,
            crate::paths::recordings_dir(data_dir)
                .join(format!("{id}.mp4"))
                .display()
        );
    }
    Ok(())
}

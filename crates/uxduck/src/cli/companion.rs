//! `uxduck companion` (S295, ADR-0049 O6): a computer on the vehicle that
//! carries no vehicle traffic, such as a second Raspberry Pi for vision,
//! says how it is doing. Once a second it sends its heartbeat and
//! `ONBOARD_COMPUTER_STATUS` as the vehicle's system id and its own onboard
//! computer id to the ground station, which shows it with that vehicle.
//!
//! It answers the `TIMESYNC` requests that reach it addressed to it, the
//! ground station's through a router such as mavlink-router or straight
//! from the ground station, with this computer's clock (S356), and stamps
//! each status with that clock as it goes.

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use uxduck_contracts::dto::link::LinkKind;
use uxduck_mavlink::framing::split;

use super::client::CliError;
use super::link_spec;
use crate::daemon::onboard::{Computer, Onboard};
use crate::video::companion::File;

#[derive(Debug, clap::Args)]
pub struct CompanionArgs {
    /// Where to send: the ground station's UDP link, `udp:HOST:PORT`.
    /// Repeat for several.
    #[arg(
        long = "to",
        value_name = "LINK",
        required_unless_present = "config",
        requires = "computer_id"
    )]
    to: Vec<String>,
    /// The vehicle's system id (`SYSID_THISMAV` on `ArduPilot`).
    #[arg(long, default_value_t = 1)]
    system_id: u8,
    /// This computer's onboard computer id, 191 to 194, its own on the
    /// vehicle. `uxduck forward --onboard` takes the first free one, 191
    /// unless pinned, so a companion usually takes 192 or later.
    #[arg(long, value_parser = clap::value_parser!(u8).range(191..=194))]
    computer_id: Option<u8>,
    /// A file of the features this computer runs, such as
    /// /etc/uxduck/companion.toml. Its [video] section sends the cameras
    /// named under [cameras.<path>] over WebRTC, their bitrate following the
    /// connection. A camera with a `record_path` also records on this
    /// computer's disk, by its `record` mode: armed, always or off. SIGHUP,
    /// or systemctl reload, applies a changed record mode at once.
    #[arg(long, value_name = "FILE")]
    config: Option<std::path::PathBuf>,
}

/// Runs until stopped. With `--config`, SIGHUP (`systemctl reload`) reads the
/// file again and applies its recording modes. A file that does not read
/// then keeps the running configuration, and says why.
///
/// # Errors
/// A destination that does not parse, no UDP socket, or a configuration
/// that does not read or whose feature fails.
pub async fn run(args: CompanionArgs) -> Result<(), CliError> {
    // Before anything else, so a reload never meets SIGHUP's default, which
    // ends the program.
    #[cfg(unix)]
    let hangups = match &args.config {
        Some(_) => Some(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .map_err(|e| CliError::Usage(format!("cannot listen for a reload: {e}")))?,
        ),
        None => None,
    };
    let file = match &args.config {
        Some(path) => Some(File::read(path).map_err(CliError::Usage)?),
        None => None,
    };
    let (recording_tx, recording_rx) = watch::channel(BTreeMap::new());
    let video = file
        .as_ref()
        .and_then(|f| Some((f.video.clone()?, f.sender_cameras())));
    let video = async move {
        match video {
            Some((video, cameras)) => crate::video::sender::run(video, cameras, recording_rx)
                .await
                .map_err(|e| CliError::Usage(format!("the video sender: {e}"))),
            None => std::future::pending().await,
        }
    };
    let (modes_tx, recorder) = match file.as_ref().and_then(File::recorder) {
        Some(recorder) => {
            let (modes_tx, modes) = watch::channel(recorder.clone());
            (Some(modes_tx), Some((recorder, modes)))
        }
        None => (None, None),
    };
    let recorder = async move {
        match recorder {
            Some((recorder, modes)) => {
                crate::video::onboard_recording::run(recorder, modes, recording_tx)
                    .await
                    .map_err(|e| CliError::Usage(format!("the recorder: {e}")))
            }
            None => std::future::pending().await,
        }
    };
    let config_path = args.config.clone();
    let reload = async move {
        #[cfg(unix)]
        if let (Some(mut hangups), Some(path), Some(mut running)) = (hangups, config_path, file) {
            while hangups.recv().await.is_some() {
                match File::read(&path) {
                    Ok(newer) => {
                        if running.beyond_modes(&newer) {
                            tracing::warn!(
                                "reloaded {}: only the recording modes apply on a reload, restart uxduck-companion for the rest",
                                path.display()
                            );
                        } else {
                            tracing::info!("reloaded {}", path.display());
                        }
                        if let (Some(modes_tx), Some(modes)) = (&modes_tx, newer.recorder()) {
                            modes_tx.send_replace(modes);
                        }
                        running = newer;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the reload kept the running configuration");
                    }
                }
            }
        }
        #[cfg(not(unix))]
        let _ = (modes_tx, file, config_path);
        std::future::pending::<Result<(), CliError>>().await
    };
    let report = async move {
        if args.to.is_empty() {
            std::future::pending().await
        } else {
            report(args).await
        }
    };
    tokio::select! {
        ended = video => ended,
        ended = recorder => ended,
        ended = reload => ended,
        ended = report => ended,
    }
}

/// Sends this computer's status and answers `TIMESYNC` until stopped.
async fn report(args: CompanionArgs) -> Result<(), CliError> {
    let computer_id = args
        .computer_id
        .ok_or_else(|| CliError::Usage("--to needs --computer-id".into()))?;
    let targets = args
        .to
        .iter()
        .map(
            |spec| match link_spec::output(spec).map_err(CliError::Usage)? {
                LinkKind::UdpForward { host, port } => Ok((host, port)),
                _ => Err(CliError::Usage(format!("{spec}: expected udp:HOST:PORT"))),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| CliError::Usage(format!("cannot open a UDP socket: {e}")))?;
    let mut computer = Computer::new(Onboard {
        component_id: Some(computer_id),
    });
    eprintln!(
        "uxduck companion: sending this computer's status as system {} component {} to {}",
        args.system_id,
        computer_id,
        args.to.join(", ")
    );
    let mut unreachable = vec![false; targets.len()];
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    let mut buffer = [0_u8; 2048];
    loop {
        tokio::select! {
            _ = timer.tick() => {}
            received = socket.recv_from(&mut buffer) => {
                // A router or the ground station asking for this computer's
                // clock; an error is an earlier send that went nowhere.
                if let Ok((length, from)) = received {
                    for frame in split(&buffer[..length]).frames {
                        for reply in computer.answer(frame) {
                            let _ = socket.send_to(&reply, from).await;
                        }
                    }
                }
                continue;
            }
        }
        let now_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        let frames = computer.frames([args.system_id], now_us);
        for ((host, port), said) in targets.iter().zip(&mut unreachable) {
            // Resolved each time: a Pi often starts before its network.
            let sent = match resolve(host, *port) {
                Ok(address) => send(&socket, address, &frames).await,
                Err(error) => Err(error),
            };
            match sent {
                Ok(()) if *said => {
                    tracing::info!(%host, port, "sending again");
                    *said = false;
                }
                Err(error) if !*said => {
                    tracing::warn!(%host, port, %error, "cannot send this computer's status");
                    *said = true;
                }
                _ => {}
            }
        }
    }
}

fn resolve(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("cannot resolve {host}")))
}

async fn send(
    socket: &tokio::net::UdpSocket,
    address: SocketAddr,
    frames: &[Vec<u8>],
) -> std::io::Result<()> {
    for frame in frames {
        socket.send_to(frame, address).await?;
    }
    Ok(())
}

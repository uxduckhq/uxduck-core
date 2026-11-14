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

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uxduck_contracts::dto::link::LinkKind;
use uxduck_mavlink::framing::split;

use super::client::CliError;
use super::link_spec;
use crate::daemon::onboard::{Computer, Onboard};

#[derive(Debug, clap::Args)]
pub struct CompanionArgs {
    /// Where to send: the ground station's UDP link, `udp:HOST:PORT`.
    /// Repeat for several.
    #[arg(long = "to", required = true, value_name = "LINK")]
    to: Vec<String>,
    /// The vehicle's system id (`SYSID_THISMAV` on `ArduPilot`).
    #[arg(long, default_value_t = 1)]
    system_id: u8,
    /// This computer's onboard computer id, 191 to 194, its own on the
    /// vehicle. `uxduck forward --onboard` takes the first free one, 191
    /// unless pinned, so a companion usually takes 192 or later.
    #[arg(long, value_parser = clap::value_parser!(u8).range(191..=194))]
    computer_id: u8,
}

/// Runs until stopped.
///
/// # Errors
/// A destination that does not parse, or no UDP socket.
pub async fn run(args: CompanionArgs) -> Result<(), CliError> {
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
        component_id: Some(args.computer_id),
    });
    eprintln!(
        "uxduck companion: sending this computer's status as system {} component {} to {}",
        args.system_id,
        args.computer_id,
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

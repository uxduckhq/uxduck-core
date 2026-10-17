//! "Phones and tablets on this network" (S97, ADR-0001 D4): while the
//! setting names one of this computer's addresses, a second listener on that
//! address and the daemon's port serves the page, pairing and `/rpc` to
//! devices on the same network (`api::listeners::http::network_router`).
//!
//! It never listens on every address (`0.0.0.0`), only on the one chosen, so
//! a network the computer joins later is not let in by itself. When that
//! address goes, the listener stops, its sockets close, and `network/status`
//! says so until the person chooses again. Plain HTTP for now: on a network
//! the person trusts, and without what browsers keep for secure pages.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use uxduck_contracts::dto::network::{NetworkAddress, NetworkStatus};

use crate::api::listeners::http;
use crate::daemon::engine_task::EngineHandle;
use crate::interface::Interfaces;

/// How often the setting and the computer's addresses are looked at.
const EVERY: Duration = Duration::from_secs(2);

/// What the network listener serves besides the engine: as the loopback
/// listener has them.
#[derive(Clone)]
pub struct Served {
    pub interfaces: Arc<Interfaces>,
    pub video: Arc<crate::video::hub::Hub>,
    pub files: crate::vehicle_files::Store,
    /// This computer's own page, sent as the referrer of relayed map
    /// requests: the map keys allow it.
    pub referrer: String,
}

/// Follows the setting for as long as the engine runs.
pub fn start(engine: EngineHandle, port: u16, served: Served) {
    tokio::spawn(supervise(engine, port, served));
}

/// A listener on one address; dropping it stops it and closes its sockets.
struct Listening {
    address: Ipv4Addr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}

async fn supervise(engine: EngineHandle, port: u16, served: Served) {
    let mut listening: Option<Listening> = None;
    let mut said: Option<String> = None;
    loop {
        let Ok(wanted) = engine
            .run(|e| e.state().settings.network_address.clone())
            .await
        else {
            return;
        };
        let wanted: Option<Ipv4Addr> = wanted.and_then(|address| address.parse().ok());
        let addresses = addresses();
        let present = |ip: Ipv4Addr| addresses.iter().any(|a| a.address == ip.to_string());
        if listening
            .as_ref()
            .is_some_and(|l| Some(l.address) != wanted || !present(l.address))
        {
            let address = listening.take().map(|l| l.address);
            tracing::info!(?address, "stopped listening on the network");
        }
        let mut problem = None;
        if let Some(ip) = wanted
            && listening.is_none()
        {
            if present(ip) {
                match listen(&engine, ip, port, &served).await {
                    Ok(started) => {
                        tracing::info!(%ip, port, "listening for phones and tablets on the network");
                        listening = Some(started);
                    }
                    Err(error) => {
                        problem = Some(format!("UXDuck couldn't listen at {ip}:{port}: {error}"));
                    }
                }
            } else {
                problem = Some(format!(
                    "This computer isn't at {ip} now. Choose its address on the network you're on."
                ));
            }
        }
        if problem != said {
            if let Some(problem) = &problem {
                tracing::warn!(code = "network-listener", "{problem}");
            }
            said.clone_from(&problem);
        }
        let status = NetworkStatus {
            addresses,
            url: listening
                .as_ref()
                .map(|l| format!("http://{}:{port}", l.address)),
            problem,
            // The engine adds who is connected when asked.
            connected: Vec::new(),
        };
        if engine.run(move |e| e.set_network(status)).await.is_err() {
            return;
        }
        tokio::time::sleep(EVERY).await;
    }
}

async fn listen(
    engine: &EngineHandle,
    ip: Ipv4Addr,
    port: u16,
    served: &Served,
) -> std::io::Result<Listening> {
    let tcp = tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), port)).await?;
    let address = tcp.local_addr()?;
    let (stop, stopped) = watch::channel(false);
    let app = http::network_router(
        engine.clone(),
        http::Network::new(address, stopped, served.referrer.clone()),
        served.interfaces.clone(),
        served.video.clone(),
        served.files.clone(),
    );
    let task = tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(error) = axum::serve(tcp, service).await {
            tracing::error!(%error, code = "listener-stopped", "the network listener stopped");
        }
    });
    Ok(Listening {
        address: ip,
        stop,
        task,
    })
}

/// This computer's IPv4 addresses another device could reach: not loopback
/// or link-local. The one its usual route goes out of comes first.
fn addresses() -> Vec<NetworkAddress> {
    let usual = crate::find::local_ipv4();
    let mut found: Vec<NetworkAddress> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() => {
                Some(NetworkAddress {
                    address: ip.to_string(),
                    usual: usual == Some(ip),
                    interface: interface.name,
                })
            }
            _ => None,
        })
        .collect();
    found.sort_by(|a, b| {
        (!a.usual, &a.interface, &a.address).cmp(&(!b.usual, &b.interface, &b.address))
    });
    found.dedup_by(|a, b| a.address == b.address);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_reachable_ones_with_the_usual_first() {
        let found = addresses();
        for address in &found {
            let ip: Ipv4Addr = address.address.parse().unwrap();
            assert!(!ip.is_loopback() && !ip.is_link_local());
        }
        assert!(found.iter().skip(1).all(|a| !a.usual));
    }
}

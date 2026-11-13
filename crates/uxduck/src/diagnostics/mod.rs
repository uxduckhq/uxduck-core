//! Bounded packet provenance, recent decisions and durable loop evidence.
mod packets;
mod query;
mod routing;
pub use packets::history;
pub(crate) use query::{bounds, cursor, limit};
pub use routing::{events, last_incident, record_routing};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use uxduck_contracts::dto::diagnostics::{PacketEndpoint, PacketEvidence, RouterEvent};
use uxduck_contracts::dto::inspect::InspectDirection;
use uxduck_core::domain::link::{Link, LinkKind};
use uxduck_engine::runtime::Clock as _;
use uxduck_mavlink::framing::Header;
const FRAMES: usize = 16_384;
const EVENTS: usize = 4096;
const INCIDENTS: usize = 256;
#[derive(Clone)]
struct Captured {
    seq: u64,
    direction: InspectDirection,
    packet: PacketEvidence,
    bytes: Vec<u8>,
    socket_send: Option<uxduck_contracts::dto::diagnostics::SocketSend>,
}
struct Capture {
    run: u64,
    seq: u64,
    frames: VecDeque<Captured>,
    events: VecDeque<RouterEvent>,
    incidents: VecDeque<RouterEvent>,
    links: HashMap<String, (String, Option<String>)>,
    transports: HashMap<String, uxduck_contracts::dto::diagnostics::TransportDiagnostics>,
    frame_loss: bool,
    event_loss: bool,
    path: Option<PathBuf>,
    persistence_error: Option<String>,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            run: crate::adapters::system::SystemClock.now_us(),
            seq: 0,
            frames: VecDeque::new(),
            events: VecDeque::new(),
            incidents: VecDeque::new(),
            links: HashMap::new(),
            transports: HashMap::new(),
            frame_loss: false,
            event_loss: false,
            path: None,
            persistence_error: None,
        }
    }
}
static DATA: LazyLock<Mutex<Capture>> = LazyLock::new(|| Mutex::new(Capture::default()));
fn capture() -> std::sync::MutexGuard<'static, Capture> {
    DATA.lock().unwrap_or_else(PoisonError::into_inner)
}
pub fn configure(dir: &Path) -> std::io::Result<()> {
    let dir = dir.join("diagnostics");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("router-incidents.json");
    let incidents = if path.exists() {
        if std::fs::metadata(&path)?.len() > 8 << 20 {
            return Err(std::io::Error::other(
                "router-incidents.json exceeds 8 MiB. Move it aside before starting UXDuck",
            ));
        }
        serde_json::from_slice::<VecDeque<RouterEvent>>(&std::fs::read(&path)?).map_err(|e| {
            std::io::Error::other(format!(
                "Invalid router-incidents.json: {e}. Move it aside before starting UXDuck"
            ))
        })?
    } else {
        VecDeque::new()
    };
    let mut data = capture();
    *data = Capture {
        incidents,
        path: Some(path),
        ..Capture::default()
    };
    while data.incidents.len() > INCIDENTS {
        data.incidents.pop_front();
    }
    Ok(())
}
pub fn register(link: &Link) {
    let peer = match &link.kind {
        LinkKind::UdpForward { host, port }
        | LinkKind::UdpClient { host, port }
        | LinkKind::TcpClient { host, port } => Some(format!("{host}:{port}")),
        _ => None,
    };
    let mut data = capture();
    if data.links.len() >= 1024 && !data.links.contains_key(&link.id) {
        data.links.clear();
    }
    data.links.insert(
        link.id.clone(),
        (link.name.chars().take(256).collect(), peer),
    );
}
fn endpoint(data: &Capture, link: &str, peer: Option<&str>) -> PacketEndpoint {
    let known = data.links.get(link);
    PacketEndpoint {
        link_id: link.to_owned(),
        name: known.map(|(name, _)| name.clone()),
        peer: peer
            .map(str::to_owned)
            .or_else(|| known.and_then(|(_, peer)| peer.clone())),
    }
}
fn evidence(
    data: &Capture,
    link: &str,
    peer: Option<&str>,
    bytes: &[u8],
    header: Header,
    at_us: u64,
) -> PacketEvidence {
    PacketEvidence {
        at_us,
        endpoint: endpoint(data, link, peer),
        message_id: header.message_id,
        system_id: header.system_id,
        component_id: header.component_id,
        sequence: header.sequence,
        fingerprint: uxduck_mavlink::router::fingerprint(bytes),
        raw_hex: None,
    }
}
pub fn record(
    link: &str,
    peer: Option<&str>,
    direction: InspectDirection,
    bytes: &[u8],
    at_us: u64,
) {
    let Some(header) = uxduck_mavlink::framing::header(bytes) else {
        return;
    };
    let mut data = capture();
    data.seq += 1;
    let frame = Captured {
        seq: data.seq,
        direction,
        packet: evidence(&data, link, peer, bytes, header, at_us),
        bytes: bytes.to_vec(),
        socket_send: None,
    };
    if data.frames.len() == FRAMES {
        data.frames.pop_front();
        data.frame_loss = true;
    }
    data.frames.push_back(frame);
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
fn persisted_packet(data: &Capture, mut packet: PacketEvidence) -> PacketEvidence {
    packet.raw_hex = data
        .frames
        .iter()
        .rev()
        .find(|frame| {
            frame.packet.at_us == packet.at_us
                && frame.packet.endpoint.link_id == packet.endpoint.link_id
                && frame.packet.fingerprint == packet.fingerprint
        })
        .map(|frame| hex(&frame.bytes));
    packet
}

/// Snapshot in the same diagnostics store as packet history. JSON is the
/// additive core/wire boundary and is checked by the contract regressions.
pub fn transport(link: &str, report: uxduck_core::domain::transport::TransportDiagnostics) {
    for sent in &report.sends {
        if !sent.registration_bytes.is_empty() {
            let seen = {
                let data = capture();
                // An evicted packet must not be inserted again on the next
                // snapshot just because its attempt remains in socket history.
                data.transports.get(link).is_some_and(|previous| {
                    previous
                        .sends
                        .iter()
                        .any(|s| s.id == sent.id && s.generation == sent.generation)
                }) || data.frames.iter().any(|frame| {
                    frame.packet.endpoint.link_id == link
                        && frame
                            .socket_send
                            .as_ref()
                            .is_some_and(|s| s.id == sent.id && s.generation == sent.generation)
                })
            };
            if !seen {
                record(
                    link,
                    Some(&sent.endpoint),
                    InspectDirection::Sent,
                    &sent.registration_bytes,
                    sent.at_us,
                );
            }
        }
    }
    let result: Result<uxduck_contracts::dto::diagnostics::TransportDiagnostics, _> =
        serde_json::to_value(report).and_then(serde_json::from_value);
    match result {
        Ok(mut report) => {
            let mut data = capture();
            if data.transports.len() >= 1024 && !data.transports.contains_key(link) {
                data.transports.clear();
            }
            if let Some(previous) = data
                .transports
                .get(link)
                .filter(|p| p.generation == report.generation)
            {
                report
                    .local_network_permission
                    .clone_from(&previous.local_network_permission);
                report.local_network_checked_us = previous.local_network_checked_us;
            }
            for sent in &report.sends {
                let existing = data.frames.iter_mut().find(|frame| {
                    frame.packet.endpoint.link_id == link
                        && frame
                            .socket_send
                            .as_ref()
                            .is_some_and(|s| s.id == sent.id && s.generation == sent.generation)
                });
                if let Some(frame) = existing {
                    frame.socket_send = Some(sent.clone());
                } else if let Some(frame) = data.frames.iter_mut().find(|frame| {
                    frame.direction == InspectDirection::Sent
                        && frame.socket_send.is_none()
                        && frame.packet.endpoint.link_id == link
                        && frame.packet.fingerprint == sent.fingerprint
                        && frame.packet.at_us <= sent.at_us
                        && frame.packet.endpoint.peer.as_deref() == Some(sent.endpoint.as_str())
                }) {
                    frame.socket_send = Some(sent.clone());
                }
            }
            #[cfg(target_os = "macos")]
            if report.send_error.as_ref().is_some_and(|e| e.active)
                && report.local_network_checked_us.is_none_or(|at| {
                    crate::adapters::system::SystemClock
                        .now_us()
                        .saturating_sub(at)
                        >= 30_000_000
                })
                && let Some(remote) = report
                    .remote_endpoint
                    .as_ref()
                    .and_then(|a| a.parse::<std::net::SocketAddr>().ok())
            {
                let now = crate::adapters::system::SystemClock.now_us();
                report.local_network_checked_us = Some(now);
                let (link, generation) = (link.to_owned(), report.generation);
                tokio::task::spawn_blocking(move || {
                    let permission = local_network_path(remote);
                    if let Some(current) = capture()
                        .transports
                        .get_mut(&link)
                        .filter(|r| r.generation == generation)
                    {
                        current.local_network_permission = Some(permission.into());
                        current.local_network_checked_us = Some(now);
                    }
                });
            }
            data.transports.insert(link.to_owned(), report);
        }
        Err(error) => tracing::error!(%error, "Transport diagnostics contract mismatch"),
    }
}
pub fn transport_of(
    link: &str,
) -> Option<uxduck_contracts::dto::diagnostics::TransportDiagnostics> {
    capture().transports.get(link).cloned()
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "bounded in-process Network.framework diagnosis, no payload sent"
)]
fn local_network_path(remote: std::net::SocketAddr) -> &'static str {
    unsafe extern "C" {
        fn uxduck_local_network_path(
            host: *const std::ffi::c_char,
            port: *const std::ffi::c_char,
        ) -> std::ffi::c_int;
    }
    let (Ok(host), Ok(port)) = (
        std::ffi::CString::new(remote.ip().to_string()),
        std::ffi::CString::new(remote.port().to_string()),
    ) else {
        return "unknown";
    };
    // SAFETY: the C function copies both strings synchronously, retains all
    // callback objects and returns within 750 ms. Nothing outlives Rust data.
    match unsafe { uxduck_local_network_path(host.as_ptr(), port.as_ptr()) } {
        1 => "denied",
        0 => "path-available",
        _ => "unknown",
    }
}

#[cfg(test)]
mod transport_tests {
    #![allow(clippy::unwrap_used, reason = "transport evidence assertions")]
    use super::*;
    use uxduck_core::domain::transport::{SocketSend, TransportDiagnostics};

    #[test]
    fn socket_outcomes_update_the_original_packet_and_registration_is_counted_once() {
        let link = "transport-correlation-test";
        let endpoint = "127.0.0.1:14550";
        let bytes = uxduck_mavlink::sim::gcs_heartbeat_frame(255, 190, 1);
        record(link, Some(endpoint), InspectDirection::Sent, &bytes, 10);
        let sent = SocketSend {
            registration_bytes: Vec::new(),
            id: 1,
            generation: 100,
            at_us: 11,
            completed_us: None,
            endpoint: endpoint.into(),
            registration: false,
            fingerprint: uxduck_mavlink::router::fingerprint(&bytes),
            message_id: Some(0),
            outcome: "attempted".into(),
            error: None,
        };
        let mut report = TransportDiagnostics {
            generation: 100,
            sends: vec![sent],
            ..Default::default()
        };
        transport(link, report.clone());
        report.sends[0].outcome = "accepted-by-os".into();
        report.sends[0].completed_us = Some(12);
        let mut registration = report.sends[0].clone();
        registration.id = 2;
        registration.at_us = 13;
        registration.registration = true;
        registration.registration_bytes = bytes;
        report.sends.push(registration);
        transport(link, report.clone());
        transport(link, report);
        let data = capture();
        let packets = data
            .frames
            .iter()
            .filter(|f| f.packet.endpoint.link_id == link)
            .collect::<Vec<_>>();
        assert_eq!(packets.len(), 2);
        assert_eq!(
            packets[0].packet.at_us, 10,
            "retain original queue evidence"
        );
        assert_eq!(
            packets[0].socket_send.as_ref().unwrap().outcome,
            "accepted-by-os"
        );
        assert!(packets[1].socket_send.as_ref().unwrap().registration);
    }
}

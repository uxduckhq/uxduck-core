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
}
struct Capture {
    run: u64,
    seq: u64,
    frames: VecDeque<Captured>,
    events: VecDeque<RouterEvent>,
    incidents: VecDeque<RouterEvent>,
    links: HashMap<String, (String, Option<String>)>,
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
                "router-incidents.json exceeds 8 MiB; move it aside before starting UXDuck",
            ));
        }
        serde_json::from_slice::<VecDeque<RouterEvent>>(&std::fs::read(&path)?).map_err(|e| {
            std::io::Error::other(format!(
                "Invalid router-incidents.json: {e}; move it aside before starting UXDuck"
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

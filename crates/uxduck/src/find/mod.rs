//! Finding vehicles on this computer and nearby (ADR-0018, S44): the
//! engine's `Discovery` port. While someone looks, rounds run every few
//! seconds: USB devices the system reports, the serial probe on those that
//! may be vehicles, UDP 14550 and 15010, TCP 5760 to 5763 on this computer, the
//! default gateway, and, when asked and approved, this computer's /24.
//! Nothing is held between tries (F6); nothing is connected (F8).

mod gateway;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uxduck_core::domain::find::{Find, FindPlace, FindReason, FindScope, FoundVehicle};
use uxduck_core::domain::link::{LinkKind, UsbId};
use uxduck_engine::finds::{DiscoveryPort, Look};
use uxduck_engine::runtime::Clock;
use uxduck_mavlink::probe::{Heard, Outcome, Plan, Timing, heartbeat_in, probe};
use uxduck_mavlink::usb_ids::{UsbKind, lookup};

use crate::adapters::system::SystemClock;
use crate::daemon::engine_task::EngineHandle;
use crate::ports::{Protocol, in_use_message};

/// Where looking nearby goes; tests move it off the usual ports.
#[derive(Clone, Debug)]
pub struct Places {
    /// 14550, where vehicles and routers send, and 15010, where another
    /// UXDuck's `uxduck forward` sends (ADR-0004).
    pub udp_ports: Vec<u16>,
    pub tcp_ports: Vec<u16>,
    /// The gateway is tried; off in tests.
    pub gateway: bool,
}

impl Default for Places {
    fn default() -> Self {
        Self {
            udp_ports: vec![14_550, 15_010],
            tcp_ports: vec![5760, 5761, 5762, 5763],
            gateway: true,
        }
    }
}

const LISTEN: Duration = Duration::from_millis(1500);
const CONNECT: Duration = Duration::from_millis(300);
const BETWEEN_ROUNDS: Duration = Duration::from_secs(2);
/// Network search pacing (F9).
const HOSTS_PER_SECOND: u32 = 50;

#[derive(Debug)]
pub struct Finder {
    engine: EngineHandle,
    runtime: tokio::runtime::Handle,
    places: Arc<Places>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Finder {
    #[must_use]
    pub fn new(engine: EngineHandle, places: Places) -> Self {
        Self {
            engine,
            runtime: tokio::runtime::Handle::current(),
            places: Arc::new(places),
            task: None,
        }
    }
}

impl DiscoveryPort for Finder {
    fn look(&mut self, look: Option<Look>) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let Some(look) = look else {
            return;
        };
        let (engine, places) = (self.engine.clone(), self.places.clone());
        self.task = Some(self.runtime.spawn(async move {
            let mut searched_network = false;
            loop {
                let started = Instant::now();
                let look2 = look.clone();
                let places2 = places.clone();
                let network = look.scope == FindScope::Network && !searched_network;
                let finds = tokio::task::spawn_blocking(move || round(&look2, &places2, network))
                    .await
                    .unwrap_or_default();
                searched_network |= network;
                let scope = look.scope;
                engine.post(move |e| {
                    e.finds_observed(finds);
                    e.find_round_done(scope);
                });
                while started.elapsed() < BETWEEN_ROUNDS {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    engine.post(uxduck_engine::Engine::finds_tick);
                }
            }
        }));
    }

    fn subnets(&self) -> Vec<String> {
        local_ipv4()
            .map(|ip| {
                let [a, b, c, _] = ip.octets();
                vec![format!("{a}.{b}.{c}.0/24")]
            })
            .unwrap_or_default()
    }
}

fn now_us() -> u64 {
    SystemClock.now_us()
}

fn hex_id(usb: &UsbId) -> String {
    format!("{:04X}:{:04X}", usb.vendor_id, usb.product_id)
}

/// This computer's address on its usual route, found without sending
/// anything: a UDP socket "connected" to a documentation address.
pub(crate) fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn found(heard: &Heard) -> FoundVehicle {
    FoundVehicle {
        system_id: heard.system_id,
        vehicle_type: heard.vehicle_type.clone(),
        autopilot: heard.autopilot.clone(),
        armed: heard.armed,
        vehicle_id: heard
            .uid
            .and_then(|(uid, uid2)| uxduck_mavlink::identity::vid_from_uid(uid, uid2)),
        name: None,
    }
}

fn find(id: String, place: FindPlace, evidence: String) -> Find {
    Find {
        id,
        place,
        link: None,
        evidence,
        usb: None,
        device_name: None,
        vehicle: None,
        reason: None,
        detail: None,
        seen_us: now_us(),
    }
}

/// One whole round of looking.
fn round(look: &Look, places: &Places, network: bool) -> Vec<Find> {
    let mut finds = serial_round(look);
    if look.scope >= FindScope::Nearby {
        let mut tries: Vec<std::thread::JoinHandle<Option<Find>>> = Vec::new();
        for &port in &places.udp_ports {
            if !look.owned.udp_ports.contains(&port) {
                tries.push(std::thread::spawn(move || udp_listen(port)));
            }
        }
        for &port in &places.tcp_ports {
            if !look
                .owned
                .tcp
                .iter()
                .any(|(h, p)| *p == port && (h == "127.0.0.1" || h == "localhost"))
            {
                tries.push(std::thread::spawn(move || {
                    tcp(Ipv4Addr::LOCALHOST, port, FindPlace::ThisComputer)
                }));
            }
        }
        if places.gateway {
            let gcs = look.gcs_system_id;
            tries.push(std::thread::spawn(move || gateway_try(gcs)));
        }
        finds.extend(tries.into_iter().filter_map(|t| t.join().ok().flatten()));
    }
    if network {
        finds.extend(sweep(look.gcs_system_id));
    }
    finds
}

/// USB devices, and the serial probe on those that may be vehicles.
fn serial_round(look: &Look) -> Vec<Find> {
    let mut finds = Vec::new();
    let mut probes = Vec::new();
    for port in uxduck_mavlink::links::serial_ports() {
        if look.owned.devices.contains(&port.device) {
            continue;
        }
        let usb = port.usb.clone();
        let owned_usb = usb.as_ref().is_some_and(|u| {
            look.owned.usb.iter().any(|o| {
                o.vendor_id == u.vendor_id
                    && o.product_id == u.product_id
                    && (o.serial_number.is_none() || o.serial_number == u.serial_number)
            })
        });
        if owned_usb {
            continue;
        }
        let known = usb.as_ref().and_then(|u| lookup(u.vendor_id, u.product_id));
        let id = format!("serial:{}", port.device);
        let mut base = find(
            id,
            FindPlace::Usb,
            usb.as_ref().map_or_else(
                || "a serial port".to_owned(),
                |u| format!("USB id {}", hex_id(u)),
            ),
        );
        base.usb.clone_from(&usb);
        base.device_name = known.map(|k| k.name.to_owned());
        let plan = match known.map(|k| k.kind) {
            Some(UsbKind::Bootloader) => {
                base.reason = Some(FindReason::Bootloader);
                finds.push(base);
                continue;
            }
            Some(UsbKind::FlightController) => Plan::KnownBoard,
            Some(UsbKind::Adapter) => Plan::Adapter,
            None if look.all_serial => Plan::Adapter,
            None => continue,
        };
        if look.scope == FindScope::Detect {
            if plan == Plan::KnownBoard {
                base.link = Some(LinkKind::Serial {
                    device: port.device.clone(),
                    baud: 115_200,
                    usb: usb.clone(),
                });
                finds.push(base);
            }
            continue;
        }
        let device = port.device.clone();
        let gcs = look.gcs_system_id;
        probes.push(std::thread::spawn(move || {
            let outcome = probe(&device, plan, &Timing::default(), gcs);
            serial_find(base, &device, usb, outcome)
        }));
    }
    finds.extend(probes.into_iter().filter_map(|p| p.join().ok()));
    finds
}

fn serial_find(mut base: Find, device: &str, usb: Option<UsbId>, outcome: Outcome) -> Find {
    match outcome {
        Outcome::Heard { baud, heard } => {
            base.evidence = format!("{}, a heartbeat at {baud} baud", base.evidence);
            base.link = Some(LinkKind::Serial {
                device: device.to_owned(),
                baud,
                usb,
            });
            base.vehicle = Some(found(&heard));
        }
        Outcome::NoMavlink => {
            base.evidence = format!(
                "{}, nothing at 57600, 115200, 921600 or 460800 baud",
                base.evidence
            );
            base.reason = Some(FindReason::NoMavlink);
        }
        Outcome::SilentKnownBoard => base.reason = Some(FindReason::SilentKnownBoard),
        Outcome::Busy => base.reason = Some(FindReason::Busy),
        Outcome::Permission(fix) => {
            base.reason = Some(FindReason::Permission);
            base.detail = Some(fix);
        }
        Outcome::Failed(why) => {
            base.reason = Some(FindReason::NoMavlink);
            base.detail = Some(why);
        }
    }
    base
}

/// Listens on UDP for a vehicle's heartbeat, briefly, and lets go.
fn udp_listen(port: u16) -> Option<Find> {
    let id = format!("udp:{port}");
    let socket = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(socket) => socket,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let mut busy = find(id, FindPlace::ThisComputer, format!("UDP port {port}"));
            busy.reason = Some(FindReason::Busy);
            busy.detail = Some(in_use_message(Protocol::Udp, port));
            return Some(busy);
        }
        Err(_) => return None,
    };
    let (heard, from) = listen_udp(&socket, LISTEN, None)?;
    let place = if from.ip().is_loopback() {
        FindPlace::ThisComputer
    } else {
        FindPlace::Network
    };
    let mut f = find(id, place, format!("a heartbeat on UDP {port} from {from}"));
    f.link = Some(LinkKind::UdpListen {
        host: "0.0.0.0".into(),
        port,
    });
    f.vehicle = Some(found(&heard));
    Some(f)
}

/// The first heartbeat on a socket within `long`, from `only` if given.
fn listen_udp(
    socket: &UdpSocket,
    long: Duration,
    only: Option<IpAddr>,
) -> Option<(Heard, SocketAddr)> {
    let until = Instant::now() + long;
    let mut buffer = [0u8; 2048];
    while Instant::now() < until {
        let _ = socket.set_read_timeout(Some(
            until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10)),
        ));
        let Ok((n, from)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        if only.is_some_and(|ip| ip != from.ip()) {
            continue;
        }
        if let Some(heard) = heartbeat_in(&buffer[..n]) {
            return Some((heard, from));
        }
    }
    None
}

/// Connects to a TCP port and listens for a heartbeat, briefly.
fn tcp(host: Ipv4Addr, port: u16, place: FindPlace) -> Option<Find> {
    use std::io::Read;
    let address = SocketAddr::from((host, port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT).ok()?;
    let until = Instant::now() + LISTEN;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    while Instant::now() < until {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(heard) = heartbeat_in(&bytes) {
                    let mut f = find(
                        format!("tcp:{host}:{port}"),
                        place,
                        format!("a heartbeat on TCP {port}"),
                    );
                    f.link = Some(LinkKind::TcpClient {
                        host: host.to_string(),
                        port,
                    });
                    f.vehicle = Some(found(&heard));
                    return Some(f);
                }
            }
            Err(_) => {}
        }
    }
    None
}

/// The default gateway, when it is a private address: a vehicle's own
/// hotspot answers there (F4). The hello comes from ground station `gcs`.
fn gateway_try(gcs: u8) -> Option<Find> {
    let gateway = gateway::default_gateway()?;
    if !gateway.is_private() {
        return None;
    }
    if let Some(mut f) = tcp(gateway, 5760, FindPlace::Wifi) {
        f.evidence = format!("{} at the gateway {gateway}", f.evidence);
        return Some(f);
    }
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    let hello = uxduck_mavlink::sim::gcs_heartbeat_frame(gcs, uxduck_mavlink::GCS_COMPONENT_ID, 0);
    let _ = socket.send_to(&hello, (gateway, 14_550));
    let id = format!("gateway:{gateway}");
    if let Some((heard, _)) = listen_udp(&socket, LISTEN, Some(IpAddr::V4(gateway))) {
        let mut f = find(
            id,
            FindPlace::Wifi,
            format!("a heartbeat on UDP 14550 from the gateway {gateway}"),
        );
        f.link = Some(LinkKind::UdpListen {
            host: "0.0.0.0".into(),
            port: 14_550,
        });
        f.vehicle = Some(found(&heard));
        return Some(f);
    }
    let mut f = find(
        id,
        FindPlace::Wifi,
        format!("the gateway {gateway}, on TCP 5760 and UDP 14550"),
    );
    f.reason = Some(FindReason::Unreachable);
    Some(f)
}

/// Every host of this computer's /24: one TCP connect to 5760 and one
/// heartbeat to UDP 14550 each, paced (F9), from ground station `gcs`.
fn sweep(gcs: u8) -> Vec<Find> {
    let Some(me) = local_ipv4() else {
        return Vec::new();
    };
    let [a, b, c, own] = me.octets();
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return Vec::new();
    };
    let hello = uxduck_mavlink::sim::gcs_heartbeat_frame(gcs, uxduck_mavlink::GCS_COMPONENT_ID, 0);
    let mut tries = Vec::new();
    for d in 1..=254u8 {
        if d == own {
            continue;
        }
        let host = Ipv4Addr::new(a, b, c, d);
        let _ = socket.send_to(&hello, (host, 14_550));
        tries.push(std::thread::spawn(move || {
            tcp(host, 5760, FindPlace::Network)
        }));
        std::thread::sleep(Duration::from_secs(1) / HOSTS_PER_SECOND);
    }
    let mut finds: Vec<Find> = tries
        .into_iter()
        .filter_map(|t| t.join().ok().flatten())
        .collect();
    // Heartbeats that came back to the one socket.
    let until = Instant::now() + LISTEN;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        let Some((heard, from)) = listen_udp(&socket, left, None) else {
            break;
        };
        let mut f = find(
            format!("udp:{}:14550", from.ip()),
            FindPlace::Network,
            format!("a heartbeat on UDP 14550 from {}", from.ip()),
        );
        f.vehicle = Some(found(&heard));
        finds.push(f);
    }
    finds
}

/// Watches USB serial devices come and go (F2, F3): a known flight
/// controller plugged in is announced; devices already there at start are
/// not.
pub fn watch_usb(engine: EngineHandle) {
    let spawned = std::thread::Builder::new()
        .name("usb-watch".into())
        .spawn(move || {
            let devices = || -> Vec<(String, UsbId)> {
                uxduck_mavlink::links::serial_ports()
                    .into_iter()
                    .filter_map(|p| Some((p.device, p.usb?)))
                    .collect()
            };
            let mut known = devices();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let now = devices();
                for (device, usb) in now.iter().filter(|d| !known.contains(d)) {
                    if let Some(board) = lookup(usb.vendor_id, usb.product_id)
                        .filter(|k| k.kind == UsbKind::FlightController)
                    {
                        let (device, usb, name) = (device.clone(), usb.clone(), board.name);
                        engine.post(move |e| e.usb_plugged(&device, &usb, name));
                    }
                }
                known = now;
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "not watching USB devices");
    }
}

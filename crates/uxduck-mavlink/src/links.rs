//! Link transports (the `native` feature): UDP listener, UDP target (a
//! forward, or a client of a UDP server), TCP client, TCP server and serial
//! device. Each runs on its own task (serial on its own
//! thread, ADR-0001 D15), reports what it hears and its state as
//! [`LinkEvent`]s, and sends what it is given. They carry bytes only: framing,
//! routing and recording happen in the caller. Links recover by themselves
//! (ADR-0004): TCP reconnects with backoff; serial reopens, finding a USB
//! adapter by its identity when its path changed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};

mod udp;
pub use udp::{NetworkChanges, RegistrationGate, udp_client_host};
use uxduck_core::domain::link::UsbId;
use uxduck_core::domain::link_failure::{LinkFailure, LinkFailureKind, LinkOperation};

/// What a link reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    Up,
    /// Why, in plain words, and what the operating system said, when it
    /// said something (FB-267).
    Down {
        detail: String,
        error: Option<LinkFailure>,
    },
    /// Bytes from a peer: one datagram, or a piece of a stream.
    Received {
        peer: String,
        bytes: Vec<u8>,
    },
    /// One connection of a TCP server closed; the link stays up.
    Left {
        peer: String,
    },
}

/// Bytes to send, to a peer or, with `None`, to the link's own destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub peer: Option<String>,
    pub bytes: Vec<u8>,
}

/// A running link. Dropping it stops it.
#[derive(Debug)]
pub struct LinkHandle {
    send: mpsc::UnboundedSender<Outgoing>,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Whether it carries a byte stream (serial, TCP) rather than datagrams.
    pub stream: bool,
    pub diagnostics: Option<watch::Receiver<uxduck_core::domain::transport::TransportDiagnostics>>,
}

impl LinkHandle {
    pub fn send(&self, outgoing: Outgoing) {
        let _ = self.send.send(outgoing);
    }

    /// A sender another task can keep; it stops working once the link is
    /// dropped.
    #[must_use]
    pub fn sender(&self) -> Sender {
        Sender(self.send.clone())
    }
}

/// Sends on a link from another task.
#[derive(Clone, Debug)]
pub struct Sender(mpsc::UnboundedSender<Outgoing>);

impl Sender {
    /// False once the link is gone.
    #[must_use]
    pub fn send(&self, outgoing: Outgoing) -> bool {
        self.0.send(outgoing).is_ok()
    }
}

impl Drop for LinkHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A transport receipt timestamp, captured before waiting for queue space.
#[derive(Clone, Debug)]
pub struct TimedEvent {
    pub event: LinkEvent,
    pub at_us: u64,
    pub socket_generation: Option<u64>,
}

impl TimedEvent {
    fn new(event: LinkEvent) -> Self {
        let at_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
            });
        Self {
            event,
            at_us,
            socket_generation: None,
        }
    }
}

/// Maximum transport events retained per link: at most 4 MiB of UDP
/// payloads, or 256 KiB of TCP/serial bytes. Streams wait rather than
/// dropping chunks, which could lose acknowledgements or split frames.
pub const EVENT_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct Events(mpsc::Sender<TimedEvent>);

impl Events {
    async fn send(&self, event: LinkEvent) -> Result<(), mpsc::error::SendError<TimedEvent>> {
        self.0.send(TimedEvent::new(event)).await
    }

    #[cfg(test)]
    fn blocking_send(&self, event: LinkEvent) -> Result<(), mpsc::error::SendError<TimedEvent>> {
        self.0.blocking_send(TimedEvent::new(event))
    }
}

/// A bounded incoming queue for one transport.
#[must_use]
pub fn event_channel() -> (Events, mpsc::Receiver<TimedEvent>) {
    let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
    (Events(sender), receiver)
}

const BUFFER: usize = 4096;

/// Listens on a UDP address; every source is a peer. Never binds with address
/// reuse, so it cannot silently share a port with another program.
///
/// # Errors
/// When the address cannot be bound, such as a port in use.
pub async fn udp_listen(address: SocketAddr, events: Events) -> std::io::Result<LinkHandle> {
    let socket = UdpSocket::bind(address).await?;
    Ok(udp_task(socket, None, None, events))
}

/// Sends to one UDP destination from an ephemeral port and hears its replies.
///
/// # Errors
/// When no local socket can be bound.
pub async fn udp_target(destination: SocketAddr, events: Events) -> std::io::Result<LinkHandle> {
    let socket = ephemeral(destination).await?;
    Ok(udp_task(socket, Some(destination), None, events))
}

/// How long a UDP client goes without hearing its server before it says
/// hello again.
pub const UDP_CLIENT_QUIET: Duration = Duration::from_secs(3);

/// A client of a UDP server, such as a telemetry bridge or mavlink-router
/// in server mode, which sends only to whoever has spoken to it: sends to
/// one destination from an ephemeral port and hears its replies, like a
/// target, and says hello (a ground station heartbeat, once a second)
/// whenever the server has been quiet for [`UDP_CLIENT_QUIET`], so a
/// server that restarted and forgot us hears us again. The hello comes
/// from the system id `gcs` holds when it is sent, the daemon's setting.
///
/// # Errors
/// When no local socket can be bound.
pub async fn udp_client(
    destination: SocketAddr,
    gcs: Arc<AtomicU8>,
    events: Events,
) -> std::io::Result<LinkHandle> {
    let socket = ephemeral(destination).await?;
    Ok(udp_task(socket, Some(destination), Some(gcs), events))
}

async fn ephemeral(destination: SocketAddr) -> std::io::Result<UdpSocket> {
    let local: SocketAddr = if destination.is_ipv4() {
        "0.0.0.0:0".parse().map_err(std::io::Error::other)?
    } else {
        "[::]:0".parse().map_err(std::io::Error::other)?
    };
    UdpSocket::bind(local).await
}

fn udp_task(
    socket: UdpSocket,
    destination: Option<SocketAddr>,
    hello: Option<Arc<AtomicU8>>,
    events: Events,
) -> LinkHandle {
    udp::start(
        socket,
        destination,
        None,
        hello,
        RegistrationGate::unrestricted(),
        events,
    )
}

/// How long to wait before each reconnect; the last repeats.
pub const TCP_BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];

/// Connects to a TCP server, and reconnects with backoff whenever the
/// connection fails or drops.
#[must_use]
pub fn tcp_client(host: String, port: u16, events: Events) -> LinkHandle {
    let (send, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let stop = Arc::new(AtomicBool::new(false));
    let peer = format!("{host}:{port}");
    let task = tokio::spawn(async move {
        let mut attempt = 0usize;
        loop {
            match connect(&host, port).await {
                Ok(stream) => {
                    attempt = 0;
                    let _ = stream.set_nodelay(true);
                    let (reader, writer) = stream.into_split();
                    let receiving = async {
                        let _ = events.send(LinkEvent::Up).await;
                        read_tcp(reader, &peer, &events).await
                    };
                    let failed = tokio::select! {
                        failed = receiving => failed,
                        failed = write_tcp(writer, &mut outgoing) => match failed {
                            Some(failed) => Err(failed),
                            None => return,
                        },
                    };
                    let event = match failed {
                        Ok(detail) => LinkEvent::Down {
                            detail,
                            error: None,
                        },
                        Err(error) => down(&error, &host, port),
                    };
                    let _ = events.send(event).await;
                }
                Err(failure) => {
                    let detail = failure.words(&host, port);
                    let _ = events
                        .send(LinkEvent::Down {
                            detail,
                            error: Some(failure),
                        })
                        .await;
                }
            }
            let wait = TCP_BACKOFF[attempt.min(TCP_BACKOFF.len() - 1)];
            attempt += 1;
            // Frames queued while disconnected are stale; drop them.
            let sleep = tokio::time::sleep(wait);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    () = &mut sleep => break,
                    next = outgoing.recv() => if next.is_none() { return },
                }
            }
        }
    });
    LinkHandle {
        send,
        stop,
        task: Some(task),
        stream: true,
        diagnostics: None,
    }
}

/// Looks the host up, then connects to the first of its addresses that
/// answers. A failed lookup is a name not found, whatever the system's
/// words for it.
async fn connect(host: &str, port: u16) -> Result<TcpStream, LinkFailure> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| LinkFailure::name_not_found(&error))?
        .collect();
    if addresses.is_empty() {
        return Err(LinkFailure::name_not_found(&std::io::Error::other(
            format!("no addresses found for {host}"),
        )));
    }
    TcpStream::connect(addresses.as_slice())
        .await
        .map_err(|error| LinkFailure::from_io(LinkOperation::Connect, &error))
}

/// A connection that failed once open, in plain words with the system's
/// own kept.
fn down(error: &std::io::Error, host: &str, port: u16) -> LinkEvent {
    let failure = LinkFailure::from_io(LinkOperation::Connected, error);
    LinkEvent::Down {
        detail: failure.words(host, port),
        error: Some(failure),
    }
}

/// Only this future waits for incoming event capacity; the writer runs
/// alongside it even while telemetry is backpressured. Why it stopped:
/// the server closing it, or the error.
async fn read_tcp(
    mut reader: OwnedReadHalf,
    peer: &str,
    events: &Events,
) -> Result<String, std::io::Error> {
    let mut buffer = vec![0u8; BUFFER];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok("the server closed the connection".into()),
            Ok(len) => {
                if events
                    .send(LinkEvent::Received {
                        peer: peer.to_owned(),
                        bytes: buffer[..len].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return Ok("the event receiver closed".into());
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn write_tcp(
    mut writer: OwnedWriteHalf,
    outgoing: &mut mpsc::UnboundedReceiver<Outgoing>,
) -> Option<std::io::Error> {
    while let Some(Outgoing { bytes, .. }) = outgoing.recv().await {
        if let Err(error) = writer.write_all(&bytes).await {
            return Some(error);
        }
    }
    None
}

/// Listens for TCP connections, such as a companion computer or another
/// program that connects out; every connection is a peer with its own byte
/// stream. Never binds with address reuse, as a UDP listener does not.
/// Frames sent to no peer go to every connection.
///
/// # Errors
/// When the address cannot be bound, such as a port in use.
pub async fn tcp_server(address: SocketAddr, events: Events) -> std::io::Result<LinkHandle> {
    let listener = TcpListener::bind(address).await?;
    let (send, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let stop = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(async move {
        let _ = events.send(LinkEvent::Up).await;
        // Each connection's writer; its task ends with its peer when it
        // closes.
        let mut writers: HashMap<String, mpsc::UnboundedSender<Outgoing>> = HashMap::new();
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, from)) => {
                        let peer = from.to_string();
                        let _ = stream.set_nodelay(true);
                        let (reader, writer) = stream.into_split();
                        let (to_peer, mut queued) = mpsc::unbounded_channel::<Outgoing>();
                        writers.insert(peer.clone(), to_peer);
                        let events = events.clone();
                        connections.spawn(async move {
                            tokio::select! {
                                _ = read_tcp(reader, &peer, &events) => {},
                                _ = write_tcp(writer, &mut queued) => {},
                            }
                            // The connection is gone. Close its write queue
                            // before waiting to report Left, and leave the
                            // listener free to send on all other connections.
                            drop(queued);
                            let _ = events.send(LinkEvent::Left { peer: peer.clone() }).await;
                            peer
                        });
                    }
                    // Too many open files and the like: wait, and keep
                    // listening.
                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                },
                Some(Ok(peer)) = connections.join_next() => {
                    writers.remove(&peer);
                }
                next = outgoing.recv() => {
                    let Some(Outgoing { peer, bytes }) = next else { break };
                    match peer {
                        // Gone since: dropped, as for a UDP peer.
                        Some(peer) => {
                            if let Some(writer) = writers.get(&peer) {
                                let _ = writer.send(Outgoing { peer: None, bytes });
                            }
                        }
                        None => {
                            for writer in writers.values() {
                                let _ = writer.send(Outgoing { peer: None, bytes: bytes.clone() });
                            }
                        }
                    }
                }
            }
        }
        connections.abort_all();
    });
    Ok(LinkHandle {
        send,
        stop,
        task: Some(task),
        stream: true,
        diagnostics: None,
    })
}

/// A serial port the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialPortInfo {
    pub device: String,
    pub usb: Option<UsbId>,
    pub description: Option<String>,
}

/// Serial ports on this machine. On macOS each device appears twice; only
/// the `/dev/cu.*` call-out device is listed, since opening `tty.*` waits
/// for carrier detect.
#[must_use]
pub fn serial_ports() -> Vec<SerialPortInfo> {
    let Ok(ports) = serialport::available_ports() else {
        return Vec::new();
    };
    ports
        .into_iter()
        .filter(|p| !p.port_name.starts_with("/dev/tty."))
        .filter(|p| !built_in_and_empty(&p.port_name))
        .map(|p| {
            let (usb, description) = match p.port_type {
                serialport::SerialPortType::UsbPort(info) => {
                    let description = match (info.manufacturer, info.product) {
                        (Some(m), Some(p)) => Some(format!("{m} {p}")),
                        (m, p) => m.or(p),
                    };
                    (
                        Some(UsbId {
                            vendor_id: info.vid,
                            product_id: info.pid,
                            serial_number: info.serial_number,
                        }),
                        description,
                    )
                }
                serialport::SerialPortType::BluetoothPort => (None, Some("Bluetooth".into())),
                _ => (None, None),
            };
            SerialPortInfo {
                device: p.port_name,
                usb,
                description,
            }
        })
        .collect()
}

/// A port built into the computer with no chip behind it. Linux lists
/// ttyS0 to ttyS31 whatever the computer has, so they aren't offered
/// (FB-287). One the kernel can't say about stays.
fn built_in_and_empty(device: &str) -> bool {
    #[cfg(target_os = "linux")]
    return device.starts_with("/dev/ttyS")
        && crate::serial_access::hardware(device) == Some(false);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
        false
    }
}

/// Where to open a serial link now: the port with its USB identity, wherever
/// it enumerated, or else its configured path.
fn resolve(device: &str, usb: Option<&UsbId>) -> String {
    if let Some(usb) = usb
        && let Some(found) = serial_ports().into_iter().find(|p| {
            p.usb.as_ref().is_some_and(|id| {
                id.vendor_id == usb.vendor_id
                    && id.product_id == usb.product_id
                    && (usb.serial_number.is_none() || id.serial_number == usb.serial_number)
            })
        })
    {
        return found.device;
    }
    device.to_owned()
}

/// Opens a serial device. A pseudo-terminal has no line speed, and macOS
/// refuses to set one ("Not a typewriter"), so it is opened with speed 0,
/// which leaves the speed alone (virtual serial ports, and the tests).
fn open_serial(path: &str, baud: u32) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    let open = |baud| {
        serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
    };
    match open(baud) {
        Err(error) if error.description.contains("typewriter") => open(0),
        other => other,
    }
}

/// EIO, ENXIO and ENODEV on open: a device node with nothing working behind
/// it, such as a built-in port with no chip (FB-287). The library keeps only
/// these descriptions, which are its own and the same on every system.
const NO_HARDWARE: [&str; 3] = ["I/O error", "No such device or address", "No such device"];
/// EPERM on open, which the library reports only by this description: a
/// security policy or a container's device rules (FB-287).
const NOT_PERMITTED: &str = "Operation not permitted";

/// A serial device that could not open, by the kind its library told. The
/// library drops the system's code behind `NoDevice`: on Windows that is
/// a port that isn't there as well as one another program holds or this
/// account may not open, so the words cover all three (FB-267). Elsewhere
/// it is a port another program holds.
fn open_failure(error: &serialport::Error) -> LinkFailure {
    let kind = match error.kind {
        serialport::ErrorKind::NoDevice if cfg!(windows) => LinkFailureKind::DeviceUnavailable,
        serialport::ErrorKind::NoDevice => LinkFailureKind::DeviceBusy,
        serialport::ErrorKind::Io(kind) => return device_failure(kind, None, &error.description),
        serialport::ErrorKind::Unknown if NO_HARDWARE.contains(&error.description.as_str()) => {
            LinkFailureKind::NoHardware
        }
        serialport::ErrorKind::Unknown if error.description == NOT_PERMITTED => {
            LinkFailureKind::AccessDenied
        }
        _ => LinkFailureKind::Other,
    };
    LinkFailure::device(kind, error.description.clone())
}

/// A serial device that failed with a system error.
fn device_failure(kind: std::io::ErrorKind, os_code: Option<i32>, message: &str) -> LinkFailure {
    let kind = match kind {
        std::io::ErrorKind::NotFound => LinkFailureKind::DeviceMissing,
        std::io::ErrorKind::PermissionDenied => LinkFailureKind::AccessDenied,
        _ => LinkFailureKind::Other,
    };
    LinkFailure {
        os_code,
        ..LinkFailure::device(kind, message.to_owned())
    }
}

fn io_failure(error: &std::io::Error) -> LinkFailure {
    device_failure(error.kind(), error.raw_os_error(), &error.to_string())
}

fn report_down(
    events: &Events,
    last: &mut Option<String>,
    reason: String,
    error: Option<LinkFailure>,
    stopped: &AtomicBool,
) {
    if last.as_ref() != Some(&reason) {
        let mut pending = TimedEvent::new(LinkEvent::Down {
            detail: reason.clone(),
            error,
        });
        while !stopped.load(Ordering::Relaxed) {
            match events.0.try_send(pending) {
                Ok(()) => {
                    *last = Some(reason);
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(event)) => pending = event,
            }
            // The port is already down; wait for delivery or cancellation.
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// How often a closed serial link tries to open again.
pub const SERIAL_RETRY: Duration = Duration::from_secs(1);

/// A serial device on its own thread, reopened whenever it fails or goes
/// away, found by USB identity when given.
#[must_use]
pub fn serial(device: String, baud: u32, usb: Option<UsbId>, events: Events) -> LinkHandle {
    let (send, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::Builder::new()
        .name(format!("serial {device}"))
        .spawn(move || {
            // Reported once per reason, not once a second while unplugged.
            let mut last_down: Option<String> = None;
            while !stopped.load(Ordering::Relaxed) {
                let path = resolve(&device, usb.as_ref());
                match open_serial(&path, baud) {
                    Ok(mut port) => {
                        last_down = None;
                        let mut pending = Some(TimedEvent::new(LinkEvent::Up));
                        let mut buffer = vec![0u8; BUFFER];
                        let reason = loop {
                            if stopped.load(Ordering::Relaxed) {
                                return;
                            }
                            let mut failed = None;
                            while let Ok(Outgoing { bytes, .. }) = outgoing.try_recv() {
                                if let Err(error) = port.write_all(&bytes) {
                                    failed = Some(io_failure(&error));
                                    break;
                                }
                            }
                            if let Some(failure) = failed {
                                break (
                                    crate::serial_access::words(&failure, &path),
                                    Some(failure),
                                );
                            }
                            if let Some(event) = pending.take() {
                                match events.0.try_send(event) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                                    Err(mpsc::error::TrySendError::Full(event)) => {
                                        pending = Some(event);
                                        // Only while backpressured; service
                                        // outgoing control again within 5 ms.
                                        std::thread::sleep(Duration::from_millis(5));
                                        continue;
                                    }
                                }
                            }
                            match port.read(&mut buffer) {
                                // With a timeout, a quiet port times out; no
                                // bytes at all means the other end hung up.
                                Ok(0) => break (format!("{path}: the device went away"), None),
                                Ok(len) => {
                                    pending = Some(TimedEvent::new(LinkEvent::Received {
                                        peer: path.clone(),
                                        bytes: buffer[..len].to_vec(),
                                    }));
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                                Err(error) => {
                                    let failure = io_failure(&error);
                                    break (
                                        crate::serial_access::words(&failure, &path),
                                        Some(failure),
                                    );
                                }
                            }
                        };
                        let (reason, failure) = reason;
                        report_down(&events, &mut last_down, reason, failure, &stopped);
                    }
                    Err(error) => {
                        let failure = open_failure(&error);
                        report_down(
                            &events,
                            &mut last_down,
                            crate::serial_access::words(&failure, &path),
                            Some(failure),
                            &stopped,
                        );
                    }
                }
                // Anything queued while closed is stale.
                while outgoing.try_recv().is_ok() {}
                std::thread::sleep(SERIAL_RETRY);
            }
        })
        .ok();
    LinkHandle {
        send,
        stop,
        task: None,
        stream: true,
        diagnostics: None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "deterministic tests"
    )]

    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// FB-267: the serial library's `NoDevice` drops the system's code,
    /// so on Windows the words cover a port that isn't there, one another
    /// program holds and one this account may not open, and keep the
    /// library's message for diagnostics.
    #[test]
    fn a_serial_port_that_wont_open_reads_without_its_code() {
        let no_device =
            serialport::Error::new(serialport::ErrorKind::NoDevice, "Access is denied.");
        let failure = open_failure(&no_device);
        #[cfg(windows)]
        assert_eq!(failure.kind, LinkFailureKind::DeviceUnavailable);
        #[cfg(not(windows))]
        assert_eq!(failure.kind, LinkFailureKind::DeviceBusy);
        assert_eq!(failure.os_code, None);
        assert_eq!(failure.message, "Access is denied.");
        let windows = LinkFailure::device(LinkFailureKind::DeviceUnavailable, String::new());
        assert_eq!(
            windows.device_words("COM4"),
            "UXDuck can't open COM4. It may be unplugged or in use by another program, or this account may not be allowed to open it."
        );
    }

    /// FB-287: EACCES and EPERM are both "not allowed", and the link says
    /// why in UXDuck's words rather than the system's.
    #[cfg(unix)]
    #[test]
    fn a_serial_port_this_account_may_not_open_says_why() {
        // As the library reports them on open: EACCES with its kind, EPERM
        // only by its description.
        for error in [
            serialport::Error::from(std::io::Error::from_raw_os_error(13)),
            serialport::Error::new(serialport::ErrorKind::Unknown, "Operation not permitted"),
        ] {
            let failure = open_failure(&error);
            assert_eq!(failure.kind, LinkFailureKind::AccessDenied, "{error:?}");
            assert!(
                crate::serial_access::words(&failure, "/dev/ttyACM0")
                    .starts_with("UXDuck isn't allowed to open /dev/ttyACM0."),
            );
        }
    }

    /// FB-287: "/dev/ttyS15: I/O error". EIO, ENXIO and ENODEV on open are
    /// a port with nothing behind it, by the descriptions the library keeps.
    #[test]
    fn a_port_with_nothing_behind_it_reads_as_no_hardware() {
        for description in ["I/O error", "No such device or address", "No such device"] {
            let error = serialport::Error::new(serialport::ErrorKind::Unknown, description);
            let failure = open_failure(&error);
            assert_eq!(failure.kind, LinkFailureKind::NoHardware, "{description}");
            assert_eq!(failure.message, description, "kept for diagnostics");
            let words = crate::serial_access::words(&failure, "/dev/ttyS15");
            assert!(
                words.contains("no hardware is behind it")
                    || words.contains("has no serial hardware behind it"),
                "{words}"
            );
        }
        let odd = serialport::Error::new(serialport::ErrorKind::Unknown, "Not a typewriter");
        assert_eq!(open_failure(&odd).kind, LinkFailureKind::Other);
    }

    /// A device file only its owner may use, opened for real. Root may open
    /// anything, so the check needs another account.
    #[cfg(unix)]
    #[test]
    fn opening_a_device_this_account_may_not_use_is_denied() {
        use std::os::unix::fs::PermissionsExt;
        let file = std::env::temp_dir().join(format!("uxduck-denied-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&file).is_ok() {
            std::fs::remove_file(&file).unwrap();
            eprintln!("skipped: this account may open any file, as root may");
            return;
        }
        let path = file.to_str().unwrap().to_owned();
        let opened = open_serial(&path, 115_200);
        std::fs::remove_file(&file).unwrap();
        let Err(error) = opened else {
            panic!("{path} opened although this account may not read it");
        };
        let failure = open_failure(&error);
        assert_eq!(failure.kind, LinkFailureKind::AccessDenied);
        assert!(
            crate::serial_access::words(&failure, &path)
                .starts_with(&format!("UXDuck isn't allowed to open {path}."))
        );
    }

    #[tokio::test]
    async fn incoming_events_are_bounded_and_timestamped_before_waiting() {
        let (events, mut received) = event_channel();
        for _ in 0..EVENT_CAPACITY {
            events.send(LinkEvent::Up).await.unwrap();
        }
        let payload = LinkEvent::Received {
            peer: "peer".into(),
            bytes: vec![1, 2, 3],
        };
        let mut pending = std::pin::pin!(events.send(payload.clone()));
        let mut context = Context::from_waker(Waker::noop());
        // Poll once: it captures receipt time but cannot enqueue yet.
        assert!(matches!(pending.as_mut().poll(&mut context), Poll::Pending));
        let before_drain = TimedEvent::new(LinkEvent::Up).at_us;
        assert_eq!(received.len(), EVENT_CAPACITY);
        received.recv().await.unwrap();
        pending.await.unwrap();
        for _ in 1..EVENT_CAPACITY {
            received.recv().await.unwrap();
        }
        let last = received.recv().await.unwrap();
        assert_eq!(last.event, payload);
        assert!(
            last.at_us <= before_drain,
            "waiting must not make old bytes fresh"
        );
    }

    #[test]
    fn closing_the_incoming_queue_releases_a_blocked_serial_sender() {
        let (events, received) = event_channel();
        for _ in 0..EVENT_CAPACITY {
            events.blocking_send(LinkEvent::Up).unwrap();
        }
        let (finished, done) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = events.blocking_send(LinkEvent::Up);
            finished.send(result.is_err()).unwrap();
        });
        drop(received);
        assert!(done.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
    }
}

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
use uxduck_core::domain::link::UsbId;

/// What a link reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    Up,
    /// Why: the error, or "unplugged".
    Down(String),
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
}

impl TimedEvent {
    fn new(event: LinkEvent) -> Self {
        let at_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
            });
        Self { event, at_us }
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
    let (send, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let stop = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(async move {
        let (heard, last_heard) = watch::channel(None::<tokio::time::Instant>);
        let (backpressured, pressure) = watch::channel(false);
        let receiving = async {
            if events.send(LinkEvent::Up).await.is_err() {
                return;
            }
            let mut buffer = vec![0u8; 65_536];
            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((len, from)) if destination.is_none_or(|d| d == from) => {
                        heard.send_replace(Some(tokio::time::Instant::now()));
                        backpressured.send_replace(true);
                        let delivered = events
                            .send(LinkEvent::Received {
                                peer: from.to_string(),
                                bytes: buffer[..len].to_vec(),
                            })
                            .await;
                        backpressured.send_replace(false);
                        if delivered.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                    Err(error) => {
                        if events
                            .send(LinkEvent::Down(error.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        };
        let sending = async {
            let mut every = tokio::time::interval(Duration::from_secs(1));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut sequence = 0u8;
            loop {
                tokio::select! {
                    _ = every.tick(), if hello.is_some() => {
                        // A full receive queue is not a silent UDP server.
                        // Its hello must not mask a withheld GCS heartbeat.
                        let quiet = !*pressure.borrow()
                            && last_heard.borrow().is_none_or(|at| at.elapsed() >= UDP_CLIENT_QUIET);
                        if quiet && let Some(to) = destination && let Some(gcs) = &hello {
                            let bytes = crate::sim::gcs_heartbeat_frame(
                                gcs.load(Ordering::Relaxed), crate::GCS_COMPONENT_ID, sequence,
                            );
                            sequence = sequence.wrapping_add(1);
                            let _ = socket.send_to(&bytes, to).await;
                        }
                    }
                    next = outgoing.recv() => {
                        let Some(Outgoing { peer, bytes }) = next else { return };
                        let to = peer.and_then(|p| p.parse::<SocketAddr>().ok()).or(destination);
                        if let Some(to) = to { let _ = socket.send_to(&bytes, to).await; }
                    }
                }
            }
        };
        // Both futures live in this task and are cancelled together on drop.
        // Waiting for receive capacity never suspends the sending future.
        tokio::select! { () = receiving => {}, () = sending => {} }
    });
    LinkHandle {
        send,
        stop,
        task: Some(task),
        stream: false,
    }
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
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(stream) => {
                    attempt = 0;
                    let _ = stream.set_nodelay(true);
                    let (reader, writer) = stream.into_split();
                    let receiving = async {
                        let _ = events.send(LinkEvent::Up).await;
                        read_tcp(reader, &peer, &events).await
                    };
                    let reason = tokio::select! {
                        reason = receiving => reason,
                        reason = write_tcp(writer, &mut outgoing) => match reason {
                            Some(reason) => reason,
                            None => return,
                        },
                    };
                    let _ = events.send(LinkEvent::Down(reason)).await;
                }
                Err(error) => {
                    let _ = events.send(LinkEvent::Down(error.to_string())).await;
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
    }
}

/// Only this future waits for incoming event capacity; the writer runs
/// alongside it even while telemetry is backpressured.
async fn read_tcp(mut reader: OwnedReadHalf, peer: &str, events: &Events) -> String {
    let mut buffer = vec![0u8; BUFFER];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return "the server closed the connection".into(),
            Ok(len) => {
                if events
                    .send(LinkEvent::Received {
                        peer: peer.to_owned(),
                        bytes: buffer[..len].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return "the event receiver closed".into();
                }
            }
            Err(error) => return error.to_string(),
        }
    }
}

async fn write_tcp(
    mut writer: OwnedWriteHalf,
    outgoing: &mut mpsc::UnboundedReceiver<Outgoing>,
) -> Option<String> {
    while let Some(Outgoing { bytes, .. }) = outgoing.recv().await {
        if let Err(error) = writer.write_all(&bytes).await {
            return Some(error.to_string());
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

fn report_down(events: &Events, last: &mut Option<String>, reason: String, stopped: &AtomicBool) {
    if last.as_ref() != Some(&reason) {
        let mut pending = TimedEvent::new(LinkEvent::Down(reason.clone()));
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
                                    failed = Some(format!("{path}: {error}"));
                                    break;
                                }
                            }
                            if let Some(reason) = failed {
                                break reason;
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
                                Ok(0) => break format!("{path}: the device went away"),
                                Ok(len) => {
                                    pending = Some(TimedEvent::new(LinkEvent::Received {
                                        peer: path.clone(),
                                        bytes: buffer[..len].to_vec(),
                                    }));
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                                Err(error) => break format!("{path}: {error}"),
                            }
                        };
                        report_down(&events, &mut last_down, reason, &stopped);
                    }
                    Err(error) => report_down(
                        &events,
                        &mut last_down,
                        format!("{path}: {error}"),
                        &stopped,
                    ),
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

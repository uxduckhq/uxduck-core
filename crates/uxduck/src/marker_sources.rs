//! GPS receivers for live markers (ADR-0035, S179): a serial port read on
//! its own thread, found at its speed when none is given, or NMEA over UDP.
//! Each line goes through the engine's NMEA reader; fixes and failures go to
//! the engine. Reopened while it fails, until the marker stops following it.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use uxduck_core::domain::marker::MarkerSource;
use uxduck_engine::markers::{BAUDS, MarkerSourcePort};
use uxduck_engine::nmea::{Fix, Nmea};

use crate::daemon::engine_task::EngineHandle;

/// How long a speed is listened to for NMEA before the next is tried.
const LISTEN_FOR: Duration = Duration::from_secs(3);
/// How long a failed source waits before opening again.
const RETRY: Duration = Duration::from_secs(1);

enum Event {
    Fix(Fix),
    Failed(String),
}

struct Reader {
    source: MarkerSource,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub struct Host {
    readers: BTreeMap<String, Reader>,
    events: mpsc::UnboundedSender<(String, Event)>,
}

impl Host {
    /// Must be made inside the daemon's runtime: it forwards to the engine
    /// on a task of its own.
    #[must_use]
    pub fn new(engine: EngineHandle) -> Self {
        let (events, mut received) = mpsc::unbounded_channel::<(String, Event)>();
        tokio::spawn(async move {
            while let Some((id, event)) = received.recv().await {
                let sent = engine
                    .run(move |e| match event {
                        Event::Fix(fix) => e.marker_fix(&id, &fix),
                        Event::Failed(reason) => e.marker_source_failed(&id, reason),
                    })
                    .await;
                if sent.is_err() {
                    return;
                }
            }
        });
        Self {
            readers: BTreeMap::new(),
            events,
        }
    }
}

impl MarkerSourcePort for Host {
    fn watch(&mut self, sources: Vec<(String, MarkerSource)>) {
        self.readers.retain(|id, reader| {
            sources
                .iter()
                .any(|(s, src)| s == id && *src == reader.source)
        });
        for (id, source) in sources {
            if self.readers.contains_key(&id) {
                continue;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let events = self.events.clone();
            let task = match &source {
                MarkerSource::Serial { port, baud } => {
                    let (port, baud, stop, id) = (port.clone(), *baud, stop.clone(), id.clone());
                    let _ = std::thread::Builder::new()
                        .name(format!("gps {port}"))
                        .spawn(move || serial(&id, &port, baud, &stop, &events));
                    None
                }
                MarkerSource::Udp { port } => {
                    Some(tokio::spawn(udp(id.clone(), *port, events)).abort_handle())
                }
                MarkerSource::Manual | MarkerSource::Computer | MarkerSource::Device => continue,
            };
            self.readers.insert(id, Reader { source, stop, task });
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Says a failure once, not once a second while it lasts.
fn fail(
    id: &str,
    events: &mpsc::UnboundedSender<(String, Event)>,
    last: &mut Option<String>,
    reason: String,
) {
    if last.as_ref() != Some(&reason) {
        let _ = events.send((id.to_owned(), Event::Failed(reason.clone())));
        *last = Some(reason);
    }
}

fn serial(
    id: &str,
    port: &str,
    baud: Option<u32>,
    stop: &AtomicBool,
    events: &mpsc::UnboundedSender<(String, Event)>,
) {
    let speeds: Vec<u32> = baud.map_or_else(|| BAUDS.to_vec(), |b| vec![b]);
    let mut last_failure: Option<String> = None;
    // The speed tried now; it stays once NMEA is heard at it.
    let mut at = 0;
    while !stop.load(Ordering::Relaxed) {
        let speed = speeds[at];
        let opened = serialport::new(port, speed)
            .timeout(Duration::from_millis(100))
            .open()
            .or_else(|error| {
                // A pseudo-terminal has no line speed (macOS: "Not a typewriter").
                if error.description.contains("typewriter") {
                    serialport::new(port, 0)
                        .timeout(Duration::from_millis(100))
                        .open()
                } else {
                    Err(error)
                }
            });
        let mut device = match opened {
            Ok(device) => device,
            Err(error) => {
                let text = error.description.to_lowercase();
                let reason = if text.contains("busy") || text.contains("in use") {
                    format!(
                        "{port} is in use by another program. Close it, then UXDuck tries again."
                    )
                } else if text.contains("no such") || text.contains("not found") {
                    format!("{port} isn't plugged in. Plug in the GPS receiver.")
                } else {
                    format!("UXDuck couldn't open {port}: {}", error.description)
                };
                fail(id, events, &mut last_failure, reason);
                std::thread::sleep(RETRY);
                continue;
            }
        };
        let mut nmea = Nmea::default();
        let mut line = Vec::new();
        let mut buffer = [0_u8; 512];
        let opened_at = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // Found by speed: nothing NMEA at this one, try the next.
            if baud.is_none() && !nmea.heard && opened_at.elapsed() >= LISTEN_FOR {
                at = (at + 1) % speeds.len();
                if at == 0 {
                    fail(
                        id,
                        events,
                        &mut last_failure,
                        format!("No GPS sentences from {port}. Check it is a GPS receiver."),
                    );
                }
                break;
            }
            match device.read(&mut buffer) {
                Ok(read) => {
                    for &byte in &buffer[..read] {
                        if byte == b'\n' {
                            let text = String::from_utf8_lossy(&line).into_owned();
                            line.clear();
                            if let Some(fix) = nmea.line(&text, now_ms()) {
                                last_failure = None;
                                let _ = events.send((id.to_owned(), Event::Fix(fix)));
                            }
                        } else if line.len() < 200 {
                            line.push(byte);
                        } else {
                            line.clear();
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    fail(
                        id,
                        events,
                        &mut last_failure,
                        format!("The GPS receiver on {port} was unplugged. Plug it in again."),
                    );
                    std::thread::sleep(RETRY);
                    break;
                }
            }
        }
    }
}

async fn udp(id: String, port: u16, events: mpsc::UnboundedSender<(String, Event)>) {
    let mut last_failure: Option<String> = None;
    let socket = loop {
        match tokio::net::UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(socket) => break socket,
            Err(error) => {
                let reason = if error.kind() == std::io::ErrorKind::AddrInUse {
                    format!("UDP port {port} is in use. Choose another, or close what uses it.")
                } else {
                    format!("UXDuck couldn't listen on UDP port {port}: {error}")
                };
                fail(&id, &events, &mut last_failure, reason);
                tokio::time::sleep(RETRY).await;
            }
        }
    };
    let mut nmea = Nmea::default();
    let mut buffer = vec![0_u8; 2048];
    loop {
        let Ok(read) = socket.recv(&mut buffer).await else {
            tokio::time::sleep(RETRY).await;
            continue;
        };
        let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
        for line in text.lines() {
            if let Some(fix) = nmea.line(line, now_ms()) {
                let _ = events.send((id.clone(), Event::Fix(fix)));
            }
        }
    }
}

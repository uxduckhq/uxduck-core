//! UDP recovery owns both directions and its socket generation. Receiving
//! never waits for a downstream consumer, and recovery never keeps controls.
use super::{Events, LinkEvent, LinkHandle, Outgoing, TimedEvent, ephemeral};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uxduck_core::domain::transport::{SocketSend, TransportDiagnostics, TransportError};

const QUIET_RECOVERY: Duration = Duration::from_secs(30);
const BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
/// The longest wait, in seconds, after a failure that only says the network
/// cannot reach the host right now. The socket is sound, and the path can
/// return with no event to announce it, so this cap is the delay left once
/// it has. It is the schedule's own second step.
const UNREACHABLE_BACKOFF: u64 = 2;
/// `EHOSTDOWN`, which has no `io::ErrorKind`: `WSAEHOSTDOWN` on Windows, 112
/// on Linux and 64 on macOS and the BSDs.
const HOST_DOWN: i32 = if cfg!(windows) {
    10064
} else if cfg!(any(target_os = "linux", target_os = "android")) {
    112
} else {
    64
};
/// Ends a recovery's reason once a network change has cut a wait short, and
/// stays until the socket hears its peer again.
const RETRIED_ON_CHANGE: &str =
    ". The network changed during this recovery and the connection retried at once";
const SEND_HISTORY: usize = 128;
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// A registration is also a GCS heartbeat. The data plane renews this lease
/// only while processing and only when the peer's supervision allows it.
#[derive(Clone, Debug, Default)]
pub struct RegistrationGate(Arc<Mutex<Lease>>);
#[derive(Debug, Default)]
enum Lease {
    #[default]
    Withheld,
    Until(Instant),
    Unrestricted,
}
impl RegistrationGate {
    pub fn renew(&self, allowed: bool) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = if allowed {
            Lease::Until(Instant::now() + Duration::from_secs(2))
        } else {
            Lease::Withheld
        };
    }
    pub(super) fn unrestricted() -> Self {
        Self(Arc::new(Mutex::new(Lease::Unrestricted)))
    }
    fn allowed(&self) -> bool {
        match *self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            Lease::Withheld => false,
            Lease::Until(until) => Instant::now() < until,
            Lease::Unrestricted => true,
        }
    }
}

/// Says that the computer's network changed: an interface came up or went
/// down, or an address was added or removed. The daemon's watcher announces
/// it. A test announces it by hand, so recovery keeps its controlled clock.
#[derive(Clone, Debug, Default)]
pub struct NetworkChanges(watch::Sender<u64>);
impl NetworkChanges {
    /// A UDP client waiting to retry does so at once, on a fresh schedule.
    /// One that has heard its peer since the last change is left alone.
    pub fn announce(&self) {
        self.0
            .send_modify(|changes| *changes = changes.wrapping_add(1));
    }
}

/// Whether an OS error only says the network cannot reach the host right
/// now: no interface, route or address for it, or no answer to ARP. The
/// socket is sound, and the path can return at any moment.
fn is_unreachable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NetworkDown
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::AddrNotAvailable
    ) || error.raw_os_error() == Some(HOST_DOWN)
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}
fn failure(operation: &str, endpoint: Option<String>, error: &io::Error) -> TransportError {
    TransportError {
        at_us: now_us(),
        operation: operation.into(),
        kind: format!("{:?}", error.kind()),
        os_code: error.raw_os_error(),
        endpoint,
        message: error.to_string(),
        active: true,
    }
}
fn clear(error: &mut Option<TransportError>) {
    if let Some(error) = error {
        error.active = false;
    }
}

/// DNS and bind failures are recoverable too. Keep the hostname for every
/// recovery, and greet from the same new socket that will receive replies.
#[must_use]
pub fn udp_client_host(
    host: String,
    port: u16,
    gcs: Arc<AtomicU8>,
    gate: RegistrationGate,
    network: &NetworkChanges,
    events: Events,
) -> LinkHandle {
    run(
        None,
        None,
        Some((host, port)),
        Some(gcs),
        gate,
        network,
        events,
    )
}

pub(super) fn start(
    socket: UdpSocket,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    events: Events,
) -> LinkHandle {
    run(
        Some(socket),
        destination,
        host,
        hello,
        gate,
        &NetworkChanges::default(),
        events,
    )
}

type Resolve = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = io::Result<SocketAddr>> + Send>>
        + Send
        + Sync,
>;
fn resolver() -> Resolve {
    Arc::new(|host, port| {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await?
                .next()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "No address found for the UDP destination",
                    )
                })
        })
    })
}

/// The OS error a send meets in place of the socket's own answer. Only a
/// controlled test gives one, because no real socket fails as an unreachable
/// host does on every system.
type SendFault = Arc<dyn Fn() -> Option<io::Error> + Send + Sync>;

fn run(
    initial: Option<UdpSocket>,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    network: &NetworkChanges,
    events: Events,
) -> LinkHandle {
    run_resolved(
        initial,
        destination,
        host,
        hello,
        gate,
        network,
        events,
        resolver(),
        Arc::new(|| None),
    )
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one cancellable socket lifecycle owns send and receive evidence"
)]
fn run_resolved(
    initial: Option<UdpSocket>,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    network: &NetworkChanges,
    events: Events,
    resolve: Resolve,
    send_fault: SendFault,
) -> LinkHandle {
    let (commands, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let (diagnostics, updates) = watch::channel(TransportDiagnostics::default());
    let stop = Arc::new(AtomicBool::new(false));
    let mut network = network.0.subscribe();
    let task = tokio::spawn(async move {
        let mut state = TransportDiagnostics {
            configured_destination: host
                .as_ref()
                .map(|(h, p)| format!("{h}:{p}"))
                .or_else(|| destination.map(|d| d.to_string())),
            ..TransportDiagnostics::default()
        };
        let local = initial.as_ref().and_then(|s| s.local_addr().ok());
        let mut socket = initial;
        let mut remote = destination;
        let mut attempt = 0usize;
        // Whether the failure that closed the socket only said the host is
        // unreachable.
        let mut unreachable = false;
        // Whether a network change cut a wait short since the socket last
        // heard its peer.
        let mut retried_on_change = false;
        let mut sequence = 0u8;
        let mut send_id = 0u64;
        loop {
            if socket.is_none() {
                state.recreation_attempts += 1;
                let previous_remote = remote;
                let opening = async {
                    let target = if let Some((host, port)) = &host {
                        Some(resolve(host.clone(), *port).await?)
                    } else {
                        previous_remote
                    };
                    let next =
                        if let Some(target) = target {
                            ephemeral(target).await?
                        } else {
                            UdpSocket::bind(local.ok_or_else(|| {
                                io::Error::other("UDP listener has no bind address")
                            })?)
                            .await?
                        };
                    Ok::<_, io::Error>((next, target))
                };
                let opening = tokio::time::timeout(Duration::from_secs(10), opening);
                tokio::pin!(opening);
                // Discard frames arriving during DNS/bind, just as during backoff.
                let result = loop {
                    tokio::select! {
                        result = &mut opening => break result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "UDP destination resolution or bind timed out"))),
                        next = outgoing.recv() => if next.is_none() { return; },
                    }
                };
                match result {
                    Ok((next, target)) => {
                        socket = Some(next);
                        remote = target;
                        clear(&mut state.recovery_error);
                    }
                    Err(error) => {
                        unreachable = is_unreachable(&error);
                        state.recovery_error = Some(failure(
                            "resolve-or-bind",
                            state.configured_destination.clone(),
                            &error,
                        ));
                        state.recovery_reason = Some(format!("Could not open UDP socket: {error}"));
                    }
                }
            }
            if let Some(active) = socket.as_ref() {
                // Anything queued before this generation is stale.
                while outgoing.try_recv().is_ok() {}
                state.generation = GENERATION.fetch_add(1, Ordering::Relaxed);
                state.socket_open = true;
                state.local_endpoint = active.local_addr().ok().map(|a| a.to_string());
                state.wildcard_bind = active.local_addr().is_ok_and(|a| a.ip().is_unspecified());
                state.remote_endpoint = remote.map(|a| a.to_string());
                state.backoff_ms = 0;
                state.next_retry_us = None;
                let _ = events.0.try_send(TimedEvent {
                    event: LinkEvent::Up,
                    at_us: now_us(),
                    socket_generation: Some(state.generation),
                });
                diagnostics.send_replace(state.clone());
                let mut buffer = vec![0u8; 65_536];
                let mut last_received = None::<Instant>;
                let mut registration_since = None::<Instant>;
                let mut failures = 0usize;
                let mut every = tokio::time::interval(Duration::from_secs(1));
                every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut held = None::<TimedEvent>;
                loop {
                    let mut packet = None;
                    tokio::select! {
                        permit = events.0.reserve(), if held.is_some() => {
                            let Ok(permit) = permit else { return; };
                            if let Some(event) = held.take() { permit.send(event); }
                        },
                        received = active.recv_from(&mut buffer), if held.is_none() => match received {
                            Ok((length, from)) if remote.is_none_or(|r| r == from) => {
                                let at_us = now_us();
                                last_received = Some(Instant::now());
                                registration_since = None;
                                if !state.send_error.as_ref().is_some_and(|e| e.active) {
                                    // Healthy. An earlier network change is spent.
                                    attempt = 0;
                                    retried_on_change = false;
                                    network.mark_unchanged();
                                }
                                state.last_receive_us = Some(at_us);
                                clear(&mut state.receive_error);
                                if !state.send_error.as_ref().is_some_and(|e| e.active) { state.recovery_reason = None; }
                                let bytes = &buffer[..length];
                                for frame in crate::framing::split(bytes).frames {
                                    if crate::decode::check(frame) == crate::framing::Check::Valid {
                                        state.last_valid_mavlink_us = Some(at_us);
                                        if crate::framing::header(frame).is_some_and(|h| h.message_id == 0) { state.last_heartbeat_us = Some(at_us); }
                                    }
                                }
                                let event = TimedEvent { event: LinkEvent::Received { peer: from.to_string(), bytes: bytes.to_vec() }, at_us, socket_generation: Some(state.generation) };
                                match events.0.try_send(event) {
                                    Ok(()) => {},
                                    Err(mpsc::error::TrySendError::Full(event)) => held = Some(event),
                                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                                }
                            }
                            Ok(_) => {},
                            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {},
                            Err(error) => {
                                unreachable = is_unreachable(&error);
                                state.receive_error = Some(failure("receive", state.local_endpoint.clone(), &error));
                                state.recovery_reason = Some(format!("UDP receive failed: {error}"));
                                break;
                            }
                        },
                        next = outgoing.recv() => {
                            let Some(next) = next else { return; };
                            let to = next.peer.as_deref().and_then(|p| p.parse::<SocketAddr>().ok()).or(remote);
                            if let Some(to) = to { packet = Some((next.bytes, to, false)); }
                        },
                        _ = every.tick() => {
                            state.registration_suppressed = hello.is_some() && !gate.allowed();
                            if state.registration_suppressed { registration_since = None; }
                            let quiet = events.0.capacity() > 0 && last_received.is_none_or(|at| at.elapsed() >= super::UDP_CLIENT_QUIET);
                            if let (true, false, Some(gcs), Some(to)) = (quiet, state.registration_suppressed, &hello, remote) {
                                // A never-answering server gets lightweight registration for
                                // 30 seconds before a new socket, not one rebind per hello.
                                if registration_since.is_some_and(|at| at.elapsed() >= QUIET_RECOVERY) {
                                    unreachable = false;
                                    state.recovery_reason = Some("No reply to UDP registration for 30 seconds".into());
                                    break;
                                }
                                registration_since.get_or_insert_with(Instant::now);
                                state.registration_attempts += 1;
                                let bytes = crate::sim::gcs_heartbeat_frame(gcs.load(Ordering::Relaxed), crate::GCS_COMPONENT_ID, sequence);
                                sequence = sequence.wrapping_add(1);
                                packet = Some((bytes, to, true));
                            }
                        }
                    }
                    if let Some((bytes, to, registration)) = packet {
                        send_id += 1;
                        let at_us = now_us();
                        state.last_send_attempt_us = Some(at_us);
                        let mut sent = SocketSend {
                            registration_bytes: if registration {
                                bytes.clone()
                            } else {
                                Vec::new()
                            },
                            id: send_id,
                            generation: state.generation,
                            at_us,
                            completed_us: None,
                            endpoint: to.to_string(),
                            registration,
                            fingerprint: crate::router::fingerprint(&bytes),
                            message_id: crate::framing::header(&bytes).map(|h| h.message_id),
                            outcome: "attempted".into(),
                            error: None,
                        };
                        state.sends.push(sent.clone());
                        if state.sends.len() > SEND_HISTORY {
                            state.sends.remove(0);
                        }
                        diagnostics.send_replace(state.clone());
                        let outcome = match send_fault() {
                            Some(error) => Err(error),
                            None => active.send_to(&bytes, to).await,
                        };
                        match outcome {
                            Ok(_) => {
                                sent.outcome = "accepted-by-os".into();
                                state.last_socket_send_us = Some(now_us());
                                clear(&mut state.send_error);
                                failures = 0;
                            }
                            Err(error) => {
                                sent.outcome = "failed".into();
                                unreachable = is_unreachable(&error);
                                let error = failure(
                                    if registration {
                                        "registration-send"
                                    } else {
                                        "packet-send"
                                    },
                                    Some(to.to_string()),
                                    &error,
                                );
                                sent.error = Some(error.clone());
                                state.send_error = Some(error);
                                if remote.is_some() {
                                    failures += 1;
                                }
                            }
                        }
                        sent.completed_us = Some(now_us());
                        if let Some(last) = state.sends.last_mut() {
                            *last = sent;
                        }
                        if failures >= 3 {
                            state.recovery_reason =
                                Some("Three consecutive UDP socket sends failed".into());
                            break;
                        }
                    }
                    diagnostics.send_replace(state.clone());
                }
            }
            // Drop before backoff. No old generation can send or receive again.
            socket = None;
            state.socket_open = false;
            let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
            // An unreachable host is not a broken socket. It waits at most
            // the cap and never moves the schedule past it.
            let wait = if unreachable {
                wait.min(UNREACHABLE_BACKOFF)
            } else {
                wait
            };
            if !unreachable || wait < UNREACHABLE_BACKOFF {
                attempt = attempt.saturating_add(1);
            }
            state.backoff_ms = wait * 1000;
            if retried_on_change && let Some(reason) = &mut state.recovery_reason {
                reason.push_str(RETRIED_ON_CHANGE);
            }
            state.next_retry_us = Some(now_us().saturating_add(state.backoff_ms * 1000));
            diagnostics.send_replace(state.clone());
            let sleep = tokio::time::sleep(Duration::from_millis(state.backoff_ms));
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    () = &mut sleep => break,
                    // The path may be back, and earlier failures say little
                    // about it. Frames queued so far are still discarded.
                    Ok(()) = network.changed() => {
                        attempt = 0;
                        if !retried_on_change && let Some(reason) = &mut state.recovery_reason {
                            reason.push_str(RETRIED_ON_CHANGE);
                        }
                        retried_on_change = true;
                        break;
                    }
                    next = outgoing.recv() => if next.is_none() { return; },
                }
            }
        }
    });
    LinkHandle {
        send: commands,
        stop,
        task: Some(task),
        stream: false,
        diagnostics: Some(updates),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "fault test assertions")]
    use super::*;
    use std::sync::atomic::AtomicUsize;
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }
    async fn step(seconds: u64) {
        tokio::time::advance(Duration::from_secs(seconds)).await;
        settle().await;
    }
    fn snapshot(link: &LinkHandle) -> TransportDiagnostics {
        link.diagnostics.as_ref().unwrap().borrow().clone()
    }
    /// The wait each of the next closures announces, a second at a time.
    async fn waits(link: &LinkHandle, count: usize) -> Vec<u64> {
        let mut waits = Vec::new();
        let mut open = snapshot(link).socket_open;
        while waits.len() < count {
            step(1).await;
            let state = snapshot(link);
            if open && !state.socket_open {
                waits.push(state.backoff_ms);
            }
            open = state.socket_open;
        }
        waits
    }
    /// The send fault a test turns on and off.
    type Fault = Arc<Mutex<Option<fn() -> io::Error>>>;
    fn fault() -> (Fault, SendFault) {
        let chosen = Fault::default();
        let asked = chosen.clone();
        (
            chosen,
            Arc::new(move || asked.lock().unwrap().map(|error| error())),
        )
    }
    fn host_down() -> io::Error {
        io::Error::from_raw_os_error(HOST_DOWN)
    }
    fn denied() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }

    #[tokio::test(start_paused = true)]
    async fn registration_stops_when_the_data_plane_stops_renewing_its_lease() {
        let gate = RegistrationGate::default();
        assert!(!gate.allowed());
        gate.renew(true);
        assert!(gate.allowed());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!gate.allowed());
        gate.renew(true);
        gate.renew(false);
        assert!(!gate.allowed());
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_resolves_again_discards_old_controls_and_obeys_the_gate() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = server.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let resolve: Resolve = Arc::new(move |host, port| {
            assert_eq!((host.as_str(), port), ("vehicle.test", 14550));
            let attempt = count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if attempt == 0 {
                    Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "interface unavailable",
                    ))
                } else {
                    Ok(destination)
                }
            })
        });
        let (tx, mut events) = super::super::event_channel();
        let gate = RegistrationGate::default();
        let link = run_resolved(
            None,
            None,
            Some(("vehicle.test".into(), 14550)),
            Some(Arc::new(AtomicU8::new(255))),
            gate.clone(),
            &NetworkChanges::default(),
            tx,
            resolve,
            Arc::new(|| None),
        );
        settle().await;
        assert_eq!(link.diagnostics.as_ref().unwrap().borrow().backoff_ms, 1000);
        link.send(Outgoing {
            peer: None,
            bytes: b"stale movement".to_vec(),
        });
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        let state = link.diagnostics.as_ref().unwrap().borrow().clone();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            state.remote_endpoint.as_deref(),
            Some(destination.to_string().as_str())
        );
        assert!(state.socket_open);
        assert!(state.registration_suppressed);
        assert_eq!(state.registration_attempts, 0);
        let mut buffer = [0; 512];
        assert!(
            server.try_recv_from(&mut buffer).is_err(),
            "neither withheld hello nor stale movement can escape"
        );
        gate.renew(true);
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        let (len, client) = server.try_recv_from(&mut buffer).unwrap();
        assert_eq!(
            crate::framing::header(&buffer[..len]).unwrap().message_id,
            0
        );
        assert_eq!(
            client.port(),
            state
                .local_endpoint
                .unwrap()
                .parse::<SocketAddr>()
                .unwrap()
                .port()
        );
        server.send_to(b"recovered", client).await.unwrap();
        settle().await;
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e.event,LinkEvent::Received{bytes,..} if bytes == b"recovered"))
        );
        drop(link);
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::Relaxed), 2, "removal cancels recovery");
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_unanswered_registration_changes_destination_and_receive_filter() {
        let old = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let new = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addresses = [old.local_addr().unwrap(), new.local_addr().unwrap()];
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let resolve: Resolve = Arc::new(move |_, _| {
            let address = addresses[count.fetch_add(1, Ordering::Relaxed).min(1)];
            Box::pin(async move { Ok(address) })
        });
        let (tx, mut events) = super::super::event_channel();
        let link = run_resolved(
            None,
            None,
            Some(("moving.test".into(), 14550)),
            Some(Arc::new(AtomicU8::new(255))),
            RegistrationGate::unrestricted(),
            &NetworkChanges::default(),
            tx,
            resolve,
            Arc::new(|| None),
        );
        settle().await;
        let generation = link.diagnostics.as_ref().unwrap().borrow().generation;
        for _ in 0..32 {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
        }
        let state = link.diagnostics.as_ref().unwrap().borrow().clone();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(state.generation > generation);
        assert_eq!(state.remote_endpoint, Some(addresses[1].to_string()));
        let mut buffer = [0; 512];
        let (_, client) = new.try_recv_from(&mut buffer).unwrap();
        old.send_to(b"old destination", client).await.unwrap();
        new.send_to(b"new destination", client).await.unwrap();
        settle().await;
        let received = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|e| match e.event {
                LinkEvent::Received { bytes, .. } => Some(bytes),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(received, vec![b"new destination".to_vec()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_change_retries_at_once_and_restarts_the_backoff() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = server.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        // The fifth call is the retry the change causes. It fails too, as the
        // retry at link-up did on 2026-11-13, and the sixth succeeds.
        let resolve: Resolve = Arc::new(move |_, _| {
            let call = count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if call < 5 {
                    Err(io::Error::other("no resolver yet"))
                } else {
                    Ok(destination)
                }
            })
        });
        let (tx, mut events) = super::super::event_channel();
        let gate = RegistrationGate::default();
        let network = NetworkChanges::default();
        let link = run_resolved(
            None,
            None,
            Some(("radio.test".into(), 15010)),
            Some(Arc::new(AtomicU8::new(255))),
            gate.clone(),
            &network,
            tx,
            resolve,
            Arc::new(|| None),
        );
        settle().await;
        for wait in [1, 2, 5] {
            assert_eq!(snapshot(&link).backoff_ms, wait * 1000);
            step(wait).await;
        }
        let waiting = snapshot(&link);
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        assert_eq!(waiting.backoff_ms, 10_000);
        assert_eq!(
            waiting.recovery_reason.as_deref(),
            Some("Could not open UDP socket: no resolver yet")
        );
        link.send(Outgoing {
            peer: None,
            bytes: b"stale movement".to_vec(),
        });
        settle().await;
        // No time passes. The change alone ends the ten-second wait.
        network.announce();
        settle().await;
        let retried = snapshot(&link);
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        assert_eq!(retried.recreation_attempts, waiting.recreation_attempts + 1);
        assert_eq!(retried.backoff_ms, 1000, "the schedule starts again");
        assert_eq!(
            retried.recovery_reason.as_deref(),
            Some(
                "Could not open UDP socket: no resolver yet. The network changed during this recovery and the connection retried at once"
            )
        );
        step(1).await;
        let open = snapshot(&link);
        assert_eq!(calls.load(Ordering::Relaxed), 6);
        assert!(open.socket_open);
        assert!(open.generation > waiting.generation);
        assert_eq!(open.recovery_reason, retried.recovery_reason);
        let mut buffer = [0; 512];
        assert!(
            server.try_recv_from(&mut buffer).is_err(),
            "the hello still needs its lease and the queued frame is gone"
        );
        gate.renew(true);
        step(1).await;
        let (len, client) = server.try_recv_from(&mut buffer).unwrap();
        assert_eq!(
            crate::framing::header(&buffer[..len]).unwrap().message_id,
            0
        );
        server.send_to(b"recovered", client).await.unwrap();
        settle().await;
        assert_eq!(snapshot(&link).recovery_reason, None);
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|e| matches!(e.event,LinkEvent::Received{bytes,..} if bytes == b"recovered"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_change_leaves_a_receiving_connection_alone() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = server.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let resolve: Resolve = Arc::new(move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(destination) })
        });
        let (failing, send_fault) = fault();
        let (tx, _events) = super::super::event_channel();
        let network = NetworkChanges::default();
        let link = run_resolved(
            None,
            None,
            Some(("radio.test".into(), 15010)),
            Some(Arc::new(AtomicU8::new(255))),
            RegistrationGate::unrestricted(),
            &network,
            tx,
            resolve,
            send_fault,
        );
        settle().await;
        let mut buffer = [0; 512];
        let (_, client) = server.try_recv_from(&mut buffer).unwrap();
        server.send_to(b"telemetry", client).await.unwrap();
        settle().await;
        let receiving = snapshot(&link);
        assert!(receiving.last_receive_us.is_some());
        network.announce();
        step(1).await;
        let after = snapshot(&link);
        assert!(after.socket_open);
        assert_eq!(after.generation, receiving.generation);
        assert_eq!(after.recreation_attempts, receiving.recreation_attempts);
        // It hears its peer after the change, so the change is spent. A
        // failure long after waits out its own backoff.
        server.send_to(b"telemetry", client).await.unwrap();
        settle().await;
        *failing.lock().unwrap() = Some(denied);
        assert_eq!(waits(&link, 1).await, [1000]);
        let closed = snapshot(&link);
        assert!(!closed.socket_open);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            closed.recovery_reason.as_deref(),
            Some("Three consecutive UDP socket sends failed")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_change_while_sends_fail_counts_when_the_socket_closes() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = server.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let resolve: Resolve = Arc::new(move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(destination) })
        });
        let (failing, send_fault) = fault();
        *failing.lock().unwrap() = Some(denied);
        let (tx, _events) = super::super::event_channel();
        let network = NetworkChanges::default();
        let link = run_resolved(
            None,
            None,
            Some(("radio.test".into(), 15010)),
            Some(Arc::new(AtomicU8::new(255))),
            RegistrationGate::unrestricted(),
            &network,
            tx,
            resolve,
            send_fault,
        );
        settle().await;
        assert_eq!(waits(&link, 2).await, [1000, 2000]);
        step(2).await;
        let open = snapshot(&link);
        assert!(open.socket_open);
        // The change does not close an open socket, whatever its sends meet.
        network.announce();
        settle().await;
        assert_eq!(snapshot(&link).generation, open.generation);
        // It has not heard its peer since, so the change still counts when the
        // socket closes: it retries at once and the next wait is the first.
        assert_eq!(waits(&link, 1).await, [1000]);
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        assert_eq!(
            snapshot(&link).recovery_reason.as_deref(),
            Some(
                "Three consecutive UDP socket sends failed. The network changed during this recovery and the connection retried at once"
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_host_never_waits_longer_than_two_seconds() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = server.local_addr().unwrap();
        let resolve: Resolve = Arc::new(move |_, _| Box::pin(async move { Ok(destination) }));
        let (failing, send_fault) = fault();
        // "Host is down", as every send met on 2026-11-13 while the radio
        // bridge was not forwarding yet.
        *failing.lock().unwrap() = Some(host_down);
        let (tx, _events) = super::super::event_channel();
        let link = run_resolved(
            None,
            None,
            Some(("radio.test".into(), 15010)),
            Some(Arc::new(AtomicU8::new(255))),
            RegistrationGate::unrestricted(),
            &NetworkChanges::default(),
            tx,
            resolve,
            send_fault,
        );
        settle().await;
        assert_eq!(waits(&link, 5).await, [1000, 2000, 2000, 2000, 2000]);
        let unreachable = snapshot(&link);
        assert_eq!(
            unreachable.recovery_reason.as_deref(),
            Some("Three consecutive UDP socket sends failed")
        );
        let error = unreachable.send_error.unwrap();
        assert_eq!(error.operation, "registration-send");
        assert_eq!(error.os_code, Some(HOST_DOWN));
        // The path returns with nothing to announce it. The wait in progress
        // is all that is left before the hello goes out.
        *failing.lock().unwrap() = None;
        step(2).await;
        let mut buffer = [0; 512];
        let (len, client) = server.try_recv_from(&mut buffer).unwrap();
        assert_eq!(
            crate::framing::header(&buffer[..len]).unwrap().message_id,
            0
        );
        server.send_to(b"reachable", client).await.unwrap();
        settle().await;
        let receiving = snapshot(&link);
        assert!(receiving.socket_open);
        assert_eq!(receiving.recovery_reason, None);
        // Any other failure keeps the whole schedule.
        *failing.lock().unwrap() = Some(denied);
        assert_eq!(
            waits(&link, 6).await,
            [1000, 2000, 5000, 10_000, 30_000, 30_000]
        );
    }

    #[test]
    fn only_an_error_that_says_the_host_is_unreachable_caps_the_wait() {
        // ENETDOWN, ENETUNREACH, EHOSTDOWN, EHOSTUNREACH and EADDRNOTAVAIL as
        // this system numbers them: Winsock, Linux, or macOS and the BSDs.
        let codes = if cfg!(windows) {
            [10050, 10051, 10064, 10065, 10049]
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            [100, 101, 112, 113, 99]
        } else {
            [50, 51, 64, 65, 49]
        };
        for code in codes {
            let error = io::Error::from_raw_os_error(code);
            assert!(is_unreachable(&error), "{error}");
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::TimedOut,
            io::ErrorKind::NotFound,
        ] {
            assert!(!is_unreachable(&io::Error::from(kind)), "{kind}");
        }
        assert!(!is_unreachable(&io::Error::other("no resolver yet")));
    }
}

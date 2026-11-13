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
    events: Events,
) -> LinkHandle {
    run(None, None, Some((host, port)), Some(gcs), gate, events)
}

pub(super) fn start(
    socket: UdpSocket,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    events: Events,
) -> LinkHandle {
    run(Some(socket), destination, host, hello, gate, events)
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

fn run(
    initial: Option<UdpSocket>,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    events: Events,
) -> LinkHandle {
    run_resolved(initial, destination, host, hello, gate, events, resolver())
}

#[allow(
    clippy::too_many_lines,
    reason = "one cancellable socket lifecycle owns send and receive evidence"
)]
fn run_resolved(
    initial: Option<UdpSocket>,
    destination: Option<SocketAddr>,
    host: Option<(String, u16)>,
    hello: Option<Arc<AtomicU8>>,
    gate: RegistrationGate,
    events: Events,
    resolve: Resolve,
) -> LinkHandle {
    let (commands, mut outgoing) = mpsc::unbounded_channel::<Outgoing>();
    let (diagnostics, updates) = watch::channel(TransportDiagnostics::default());
    let stop = Arc::new(AtomicBool::new(false));
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
                                if !state.send_error.as_ref().is_some_and(|e| e.active) { attempt = 0; }
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
                        match active.send_to(&bytes, to).await {
                            Ok(_) => {
                                sent.outcome = "accepted-by-os".into();
                                state.last_socket_send_us = Some(now_us());
                                clear(&mut state.send_error);
                                failures = 0;
                            }
                            Err(error) => {
                                sent.outcome = "failed".into();
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
            state.backoff_ms = BACKOFF[attempt.min(BACKOFF.len() - 1)] * 1000;
            attempt = attempt.saturating_add(1);
            state.next_retry_us = Some(now_us().saturating_add(state.backoff_ms * 1000));
            diagnostics.send_replace(state.clone());
            let sleep = tokio::time::sleep(Duration::from_millis(state.backoff_ms));
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    () = &mut sleep => break,
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
            tx,
            resolve,
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
            tx,
            resolve,
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
}

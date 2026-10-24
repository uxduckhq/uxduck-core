//! This computer's connection to its account (S78, ADR-0003 C7 and "The
//! edge connection"): one outbound WebSocket to the relay, a Durable Object
//! per computer in `apps/api`'s Worker, while UXDuck is signed in and
//! "Connect this computer to my account" is on. Signed out, nothing leaves.
//!
//! It says hello, then pings every few seconds; each answer gives the round
//! trip and the relay's clock against this one's, which later slices use to
//! age values and commands. A connection that goes quiet is dropped, and
//! every reconnect waits a backoff with jitter, so a Worker deploy that
//! drops every computer at once does not bring them all back at once.
//!
//! Cameras watched from away (S84) are published while this connection
//! lasts (`live_video`); while any is, pings go every second, since their
//! round trip is how video learns that the uplink is queueing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use uxduck_contracts::dto::edge::{
    EdgeDemand, EdgePong, EdgePublish, EdgeState, EdgeStatus, RemoteMarker, RemoteVehicle,
};

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// How often the computer pings the relay.
const PING: Duration = Duration::from_secs(5);
/// How often while live video is sent: the round trip steers its rung (S84).
const PING_LIVE: Duration = Duration::from_secs(1);
/// Silence for this long and the connection is taken for dead.
const QUIET: Duration = Duration::from_secs(15);
/// How often it looks whether it should still be connected.
const CHECK: Duration = Duration::from_secs(1);
/// A connection up this long resets the backoff.
const STEADY: Duration = Duration::from_secs(30);
/// How often vehicles open in detail somewhere are sent, at most (S79).
const DETAIL: Duration = Duration::from_millis(100);
/// How often the list's vehicles are sent, at most.
const SUMMARY: Duration = Duration::from_secs(1);

/// What the computer sends of the vehicles watched (S79, ADR-0003 C8): the
/// latest values, only what changed, the list's at most once a second.
/// There is no queue: a slow link sends the newest values less often.
#[derive(Debug, Default)]
pub struct Publisher {
    demand: EdgeDemand,
    seq: u64,
    /// What was last sent of each vehicle, without its date, and when.
    sent: HashMap<String, (String, tokio::time::Instant)>,
    /// What was last sent of the markers watchers see (S182), and when.
    sent_markers: Option<(String, tokio::time::Instant)>,
}

impl Publisher {
    /// The relay says what is watched now: everything is sent again.
    pub fn set_demand(&mut self, demand: EdgeDemand) {
        self.demand = demand;
        self.sent.clear();
        self.sent_markers = None;
    }

    #[must_use]
    pub fn demand(&self) -> &EdgeDemand {
        &self.demand
    }

    /// Whether anyone is watching; nobody, and nothing is sent.
    #[must_use]
    pub fn active(&self) -> bool {
        self.demand.summary || !self.demand.detail.is_empty()
    }

    /// The publish to send now, if anything changed.
    pub fn next(
        &mut self,
        vehicles: Vec<RemoteVehicle>,
        markers: Vec<RemoteMarker>,
        now: tokio::time::Instant,
    ) -> Option<EdgePublish> {
        let present: HashSet<String> = vehicles.iter().map(|v| v.id.clone()).collect();
        let mut changed = Vec::new();
        for vehicle in vehicles {
            let key = serde_json::to_string(&RemoteVehicle {
                at_ms: 0,
                ..vehicle.clone()
            })
            .unwrap_or_default();
            let detail = self.demand.detail.contains(&vehicle.id)
                || vehicle
                    .telemetry
                    .as_ref()
                    .is_some_and(|t| !t.items.is_empty());
            match self.sent.get(&vehicle.id) {
                Some((last, _)) if *last == key => continue,
                Some((_, at)) if !detail && now.duration_since(*at) < SUMMARY => continue,
                _ => {}
            }
            self.sent.insert(vehicle.id.clone(), (key, now));
            changed.push(vehicle);
        }
        let removed: Vec<String> = self
            .sent
            .keys()
            .filter(|id| !present.contains(*id))
            .cloned()
            .collect();
        for id in &removed {
            self.sent.remove(id);
        }
        // The markers, all of them when any changed, as often as the list.
        let key = serde_json::to_string(&markers).unwrap_or_default();
        let markers = match &self.sent_markers {
            Some((last, _)) if *last == key => None,
            Some((_, at)) if now.duration_since(*at) < SUMMARY => None,
            _ => {
                self.sent_markers = Some((key, now));
                Some(markers)
            }
        };
        if changed.is_empty() && removed.is_empty() && markers.is_none() {
            return None;
        }
        self.seq += 1;
        Some(EdgePublish {
            seq: self.seq,
            vehicles: changed,
            removed,
            markers,
        })
    }
}

/// Starts the task; it lives as long as the daemon.
pub fn start(data_dir: &std::path::Path, credentials: FileCredentials, engine: EngineHandle) {
    let Some(api) = crate::feedback::api_url() else {
        return;
    };
    tokio::spawn(run(api, data_dir.to_owned(), credentials, engine));
}

/// The wait before reconnect `attempt` (0 first): doubling from a second to
/// a minute, then a random point in its upper half, so computers dropped
/// together spread out but none comes back at once.
#[must_use]
pub fn backoff(attempt: u32, unit: f64) -> Duration {
    let ceiling = Duration::from_secs(1u64 << attempt.min(6)).min(Duration::from_secs(60));
    ceiling.mul_f64(0.5 + 0.5 * unit.clamp(0.0, 1.0))
}

/// A random number from 0 to 1.
pub(crate) fn unit() -> f64 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The relay's clock minus this one's, from a ping sent at `sent` and
/// answered at `relay` by the relay, received at `received`: the answer is
/// taken to be dated halfway through the round trip.
#[must_use]
#[allow(
    clippy::cast_possible_wrap,
    reason = "milliseconds since 1970 fit in an i64"
)]
pub fn clock_offset(sent: u64, relay: u64, received: u64) -> i64 {
    let midpoint = sent + received.saturating_sub(sent) / 2;
    relay as i64 - midpoint as i64
}

fn status(state: EdgeState, problem: Option<&str>) -> EdgeStatus {
    EdgeStatus {
        state,
        round_trip_ms: None,
        clock_offset_ms: None,
        problem: problem.map(str::to_owned),
    }
}

/// Why a connection ended, or did not start.
enum Ended {
    /// Not wanted any more: signed out, or switched off.
    Unwanted,
    /// The relay refused the session: sign in again.
    Refused,
    Dropped(String),
}

async fn run(
    api: String,
    data_dir: std::path::PathBuf,
    credentials: FileCredentials,
    engine: EngineHandle,
) {
    let name = crate::sync::computer_name();
    let mut attempt = 0u32;
    let mut last = None;
    loop {
        // What the person wants, read again every time round.
        let token = credentials
            .get(crate::account::TOKEN)
            .filter(|t| !t.is_empty());
        let Ok(wanted) = engine.run(|e| e.edge_wanted()).await else {
            return;
        };
        let report = |next: EdgeStatus| {
            let engine = engine.clone();
            async move {
                let _ = engine.run(move |e| e.edge_changed(next)).await;
            }
        };
        let idle = match (&token, wanted) {
            (None, _) => Some(status(EdgeState::SignedOut, None)),
            (Some(_), false) => Some(status(EdgeState::Off, None)),
            _ => None,
        };
        if let Some(idle) = idle {
            if last.as_ref() != Some(&idle.state) {
                last = Some(idle.state);
                report(idle).await;
            }
            attempt = 0;
            tokio::time::sleep(CHECK * 2).await;
            continue;
        }
        let Some(token) = token else { continue };
        let Ok(device) = engine.run(uxduck_engine::Engine::edge_device_id).await else {
            return;
        };
        // Idle states are said again after any connection.
        last = None;
        report(status(EdgeState::Connecting, None)).await;
        let started = tokio::time::Instant::now();
        let (mut remote, answers) = crate::remote_users::RemoteUsers::new(
            engine.clone(),
            data_dir.clone(),
            api.clone(),
            device.clone(),
        );
        let ended = session(
            &api,
            &token,
            &device,
            &name,
            &credentials,
            &engine,
            (&mut remote, answers),
        )
        .await;
        // Everyone away goes with the connection, and their leases with them.
        remote.close();
        if started.elapsed() >= STEADY {
            attempt = 0;
        }
        let (problem, wait) = match ended {
            Ended::Unwanted => continue,
            Ended::Refused => (
                "UXDuck's server did not accept this sign-in. Sign in again.".to_owned(),
                Duration::from_secs(60),
            ),
            Ended::Dropped(why) => (why, backoff(attempt, unit())),
        };
        attempt = attempt.saturating_add(1);
        report(status(EdgeState::Connecting, Some(&problem))).await;
        tracing::debug!(%problem, ?wait, "edge connection: waiting to reconnect");
        tokio::time::sleep(wait).await;
    }
}

/// One connection, from dialling to its end.
#[allow(
    clippy::too_many_lines,
    reason = "one connection's life, read top to bottom"
)]
async fn session(
    api: &str,
    token: &str,
    device: &str,
    name: &str,
    credentials: &FileCredentials,
    engine: &EngineHandle,
    (remote, mut answers): (
        &mut crate::remote_users::RemoteUsers,
        futures::channel::mpsc::UnboundedReceiver<String>,
    ),
) -> Ended {
    let base = api
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let url = format!(
        "{base}/v1/computers/edge?device={device}&name={}",
        encode(name)
    );
    let Ok(mut request) = url.into_client_request() else {
        return Ended::Dropped("UXDuck's server address is not valid".into());
    };
    let headers = request.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(header::AUTHORIZATION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("uxduck/{}", env!("CARGO_PKG_VERSION"))) {
        headers.insert(header::USER_AGENT, value);
    }
    let socket = match tokio::time::timeout(QUIET, tokio_tungstenite::connect_async(request)).await
    {
        Err(_) => return Ended::Dropped("UXDuck's server did not answer".into()),
        Ok(Err(WsError::Http(response))) if response.status() == StatusCode::UNAUTHORIZED => {
            return Ended::Refused;
        }
        Ok(Err(WsError::Http(response))) => {
            return Ended::Dropped(format!(
                "UXDuck's server refused the connection ({})",
                response.status()
            ));
        }
        Ok(Err(_)) => return Ended::Dropped("UXDuck cannot reach its server".into()),
        Ok(Ok((socket, _))) => socket,
    };
    let (mut sink, mut stream) = socket.split();
    let mut next_id = 1u64;
    let mut request_frame = |method: &str, params: Value| {
        let id = next_id;
        next_id += 1;
        (
            id,
            Message::text(
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                    .to_string(),
            ),
        )
    };
    let (_, hello) = request_frame(
        "edge/hello",
        json!({ "daemonVersion": env!("CARGO_PKG_VERSION") }),
    );
    if sink.send(hello).await.is_err() {
        return Ended::Dropped("the connection closed as it opened".into());
    }
    let mut current = EdgeStatus {
        state: EdgeState::Connected,
        round_trip_ms: None,
        clock_offset_ms: None,
        problem: None,
    };
    {
        let connected = current.clone();
        let _ = engine.run(move |e| e.edge_changed(connected)).await;
    }
    tracing::info!(device, "connected this computer to its account");
    // The keys for control from away, ready before anyone asks (S80).
    remote.refresh_keys().await;
    let mut pinging = tokio::time::interval(PING);
    let mut check = tokio::time::interval(CHECK);
    let mut heard = tokio::time::Instant::now();
    // Pings waiting for their answer, by id.
    let mut pending: Vec<(u64, u64)> = Vec::new();
    let mut publisher = Publisher::default();
    // Cameras watched from away (S84): ends with this connection.
    let live = crate::live_video::Live::start(api, token, device, engine);
    let mut live_pinging = tokio::time::interval(PING_LIVE);
    let mut publishing = tokio::time::interval(DETAIL);
    publishing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            // The engine's answers to people away (S80).
            Some(answer) = answers.next() => {
                if sink.send(Message::text(answer)).await.is_err() {
                    return Ended::Dropped("the connection to UXDuck's server closed".into());
                }
            }
            _ = publishing.tick(), if publisher.active() => {
                let demand = publisher.demand().clone();
                let cameras = live.cameras();
                let Ok((vehicles, markers)) = engine
                    .run(move |e| (e.edge_vehicles(&demand, &cameras), e.edge_markers()))
                    .await
                else {
                    return Ended::Unwanted;
                };
                if let Some(publish) = publisher.next(vehicles, markers, tokio::time::Instant::now()) {
                    let frame = json!({ "jsonrpc": "2.0", "method": "edge/publish", "params": publish });
                    if sink.send(Message::text(frame.to_string())).await.is_err() {
                        return Ended::Dropped("the connection to UXDuck's server closed".into());
                    }
                }
            }
            _ = live_pinging.tick(), if live.active() => {
                let sent = now_ms();
                let (id, frame) = request_frame("edge/ping", json!({ "sentAtMs": sent }));
                pending.push((id, sent));
                if sink.send(frame).await.is_err() {
                    return Ended::Dropped("the connection to UXDuck's server closed".into());
                }
            }
            _ = pinging.tick() => {
                crate::wakes::count("edge: ping");
                let sent = now_ms();
                let (id, frame) = request_frame("edge/ping", json!({ "sentAtMs": sent }));
                pending.push((id, sent));
                pending.retain(|(_, at)| sent.saturating_sub(*at) < 60_000);
                if sink.send(frame).await.is_err() {
                    return Ended::Dropped("the connection to UXDuck's server closed".into());
                }
            }
            _ = check.tick() => {
                if heard.elapsed() > QUIET {
                    return Ended::Dropped("UXDuck's server went quiet".into());
                }
                let signed_in = credentials
                    .get(crate::account::TOKEN)
                    .is_some_and(|t| t == token);
                let wanted = engine.run(|e| e.edge_wanted()).await.unwrap_or(false);
                if !signed_in || !wanted {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ended::Unwanted;
                }
            }
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Close(frame))) => {
                        return Ended::Dropped(frame.map_or_else(
                            || "UXDuck's server closed the connection".into(),
                            |f| format!("UXDuck's server closed the connection: {}", f.reason),
                        ));
                    }
                    Some(Ok(_)) => {
                        heard = tokio::time::Instant::now();
                        continue;
                    }
                    Some(Err(_)) | None => {
                        return Ended::Dropped("the connection to UXDuck's server dropped".into());
                    }
                };
                heard = tokio::time::Instant::now();
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                // Someone away (S80): a command, the lease, a receipt, or gone.
                if message["method"] == "edge/command" {
                    // Commands go first on the uplink: video does not climb now.
                    live.command();
                }
                if message["method"].as_str().is_some_and(|m| matches!(m, "edge/command" | "edge/lease" | "edge/receipt" | "edge/left")) {
                    if let Some(answer) = remote.request(&message).await
                        && sink.send(Message::text(answer)).await.is_err()
                    {
                        return Ended::Dropped("the connection to UXDuck's server closed".into());
                    }
                    continue;
                }
                // What anyone watching asked for (S79).
                if message["method"] == "edge/demand" {
                    if let Ok(demand) = serde_json::from_value::<EdgeDemand>(message["params"].clone()) {
                        live.want(demand.video.clone());
                        // Guests per watch link, for "N watching" (S95).
                        let viewers = demand.viewers.clone();
                        engine.post(move |e| e.watch_viewers(viewers));
                        publisher.set_demand(demand);
                    }
                    continue;
                }
                let id = message["id"].as_u64();
                let Some(index) = id.and_then(|id| pending.iter().position(|(p, _)| *p == id)) else {
                    continue;
                };
                let (_, sent) = pending.remove(index);
                let Ok(answer) = serde_json::from_value::<EdgePong>(message["result"].clone()) else {
                    continue;
                };
                let received = now_ms();
                live.round_trip(received.saturating_sub(sent));
                current.round_trip_ms = Some(received.saturating_sub(sent));
                current.clock_offset_ms = Some(clock_offset(sent, answer.relay_at_ms, received));
                let update = current.clone();
                let _ = engine.run(move |e| e.edge_changed(update)).await;
            }
        }
    }
}

/// `name` for a query string.
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_spread_out_and_never_come_at_once() {
        assert_eq!(backoff(0, 0.0), Duration::from_millis(500));
        assert_eq!(backoff(0, 1.0), Duration::from_secs(1));
        assert_eq!(backoff(3, 0.5), Duration::from_secs(6));
        assert_eq!(
            backoff(20, 1.0),
            Duration::from_secs(60),
            "a minute at most"
        );
        assert_eq!(backoff(20, 0.0), Duration::from_secs(30));
    }

    #[test]
    fn the_relay_clock_is_read_at_the_middle_of_the_round_trip() {
        // Sent at 1000, answered at 1600 by the relay, back at 1200: the
        // relay is 500 ms ahead.
        assert_eq!(clock_offset(1000, 1600, 1200), 500);
        assert_eq!(clock_offset(1000, 900, 1200), -200);
    }

    fn vehicle(id: &str, mode: &str, items: usize) -> RemoteVehicle {
        RemoteVehicle {
            id: id.into(),
            name: id.into(),
            vehicle_type: "Quadrotor".into(),
            autopilot: "Ardupilotmega".into(),
            size_cm: None,
            look: None,
            presence: uxduck_contracts::dto::vehicle::Presence::Online,
            armed: false,
            mode: Some(mode.into()),
            telemetry: (items > 0).then(|| uxduck_contracts::dto::telemetry::VehicleTelemetry {
                vehicle_id: id.into(),
                positions: Vec::new(),
                items: Vec::new(),
            }),
            notices: Vec::new(),
            lease: None,
            cameras: Vec::new(),
            at_ms: 1,
        }
    }

    #[test]
    fn only_what_changed_is_sent_and_the_list_once_a_second() {
        let start = tokio::time::Instant::now();
        let mut publisher = Publisher::default();
        assert!(!publisher.active(), "nobody watching: nothing is sent");
        publisher.set_demand(EdgeDemand {
            detail: vec!["a".into()],
            summary: true,
            video: Vec::new(),
            viewers: Vec::new(),
        });
        let first = publisher
            .next(
                vec![vehicle("a", "LOITER", 0), vehicle("b", "HOLD", 0)],
                Vec::new(),
                start,
            )
            .unwrap();
        assert_eq!((first.seq, first.vehicles.len()), (1, 2));
        // The same values, dated later: nothing.
        let mut later = vehicle("a", "LOITER", 0);
        later.at_ms = 99;
        assert!(
            publisher
                .next(vec![later, vehicle("b", "HOLD", 0)], Vec::new(), start)
                .is_none()
        );
        // Open in detail, a change goes at once; the list's waits a second.
        let soon = start + Duration::from_millis(100);
        let changed = publisher
            .next(
                vec![vehicle("a", "RTL", 0), vehicle("b", "LAND", 0)],
                Vec::new(),
                soon,
            )
            .unwrap();
        assert_eq!(
            changed
                .vehicles
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        let second = start + Duration::from_secs(1);
        let listed = publisher
            .next(
                vec![vehicle("a", "RTL", 0), vehicle("b", "LAND", 0)],
                Vec::new(),
                second,
            )
            .unwrap();
        assert_eq!(listed.vehicles[0].id, "b");
        // Gone from the computer: said once.
        let gone = publisher
            .next(vec![vehicle("a", "RTL", 0)], Vec::new(), second)
            .unwrap();
        assert_eq!(gone.removed, ["b"]);
        assert_eq!(gone.seq, 4);
        // A new demand sends everything again.
        publisher.set_demand(EdgeDemand::default());
        assert!(!publisher.active());
    }

    #[test]
    fn markers_go_to_watchers_when_they_change_at_most_once_a_second() {
        let start = tokio::time::Instant::now();
        let mut publisher = Publisher::default();
        publisher.set_demand(EdgeDemand {
            summary: true,
            ..EdgeDemand::default()
        });
        let flag = |latitude_deg: f64| RemoteMarker {
            id: "mk1".into(),
            name: "Launch site".into(),
            look: uxduck_contracts::dto::marker::MarkerLook::Flag,
            latitude_deg,
            longitude_deg: -1.0,
            heading_deg: None,
            fix_at_ms: None,
        };
        let first = publisher.next(Vec::new(), vec![flag(50.0)], start).unwrap();
        assert_eq!(first.markers.unwrap()[0].name, "Launch site");
        assert!(
            publisher
                .next(Vec::new(), vec![flag(50.0)], start)
                .is_none(),
            "unchanged"
        );
        let soon = start + Duration::from_millis(200);
        assert!(
            publisher.next(Vec::new(), vec![flag(50.1)], soon).is_none(),
            "a second apart"
        );
        let second = start + Duration::from_secs(1);
        let moved = publisher
            .next(Vec::new(), vec![flag(50.1)], second)
            .unwrap();
        assert!((moved.markers.unwrap()[0].latitude_deg - 50.1).abs() < 1e-9);
        // Watch links turned off for it: the empty list goes, so it goes.
        let later = start + Duration::from_secs(2);
        let none = publisher.next(Vec::new(), Vec::new(), later).unwrap();
        assert_eq!(none.markers, Some(Vec::new()));
    }

    #[test]
    fn names_go_in_the_address_safely() {
        assert_eq!(encode("Ben's Mac"), "Ben%27s%20Mac");
        assert_eq!(encode("pi-4"), "pi-4");
    }
}

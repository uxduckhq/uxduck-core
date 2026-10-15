//! The daemon's viewers on the relay (S86, ADR-0003 C8): while a window
//! watches another of the account's computers, one viewer connection per
//! computer, as this computer's signed-in account, the same as `uxduck
//! account watch`. Snapshots and deltas become `RemoteView`s for the engine,
//! with each vehicle's time moved to this computer's clock; a gap in the
//! deltas asks for a snapshot again, and a dropped connection comes back
//! after a short wait.
//!
//! A window playing one of that computer's cameras (S84): the viewer tells
//! the relay (`edge/watchVideo`), so the computer publishes it, and fetches
//! a read grant from the media coordinator for the page, again every minute
//! and whenever the computer moves the camera to another media node. The
//! account's token stays here; the page gets a grant for one stream.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use uxduck_contracts::dto::edge::{
    EdgeDelta, EdgeSnapshot, MediaWatchGrant, MediaWatchRequest, RemoteVehicle, RemoteVideo,
    RemoteView, TrailPoint, VehicleTrail,
};
use uxduck_engine::remote_view::{RemoteViewPort, Wanted};

use crate::daemon::engine_task::EngineHandle;
use crate::edge::now_ms;

/// A trail keeps this many points, as the relay does.
const TRAIL_POINTS: usize = 2_000;
/// Read grants last two minutes; a fresh one is fetched every minute (S84).
const REGRANT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
pub struct Port(watch::Sender<Wanted>, watch::Sender<Wanted>);

impl RemoteViewPort for Port {
    fn want(&mut self, wanted: Wanted) {
        let _ = self.0.send(wanted);
    }

    fn want_video(&mut self, cameras: Wanted) {
        let _ = self.1.send(cameras);
    }
}

pub fn start(data_dir: PathBuf, engine: EngineHandle) -> Port {
    let (wants, wanted) = watch::channel(Wanted::new());
    let (plays, played) = watch::channel(Wanted::new());
    tokio::spawn(run(data_dir, engine, wanted, played));
    Port(wants, plays)
}

type Viewer = (
    tokio::task::JoinHandle<()>,
    watch::Sender<Option<String>>,
    watch::Sender<BTreeSet<String>>,
);

async fn run(
    data_dir: PathBuf,
    engine: EngineHandle,
    mut wanted: watch::Receiver<Wanted>,
    mut played: watch::Receiver<Wanted>,
) {
    let mut viewers: BTreeMap<String, Viewer> = BTreeMap::new();
    loop {
        let now = wanted.borrow_and_update().clone();
        let cameras_now = played.borrow_and_update().clone();
        viewers.retain(|computer, (task, _, _)| {
            let keep = now.contains_key(computer);
            if !keep {
                task.abort();
            }
            keep
        });
        for (computer, details) in &now {
            // The relay opens one vehicle in detail per viewer: the first asked.
            let detail = details.iter().next().cloned();
            let cameras = cameras_now.get(computer).cloned().unwrap_or_default();
            if let Some((_, open, play)) = viewers.get(computer) {
                open.send_if_modified(|d| {
                    let changed = *d != detail;
                    d.clone_from(&detail);
                    changed
                });
                play.send_if_modified(|c| {
                    let changed = *c != cameras;
                    c.clone_from(&cameras);
                    changed
                });
                continue;
            }
            let (open, opened) = watch::channel(detail);
            let (play, playing) = watch::channel(cameras);
            let task = tokio::spawn(view(
                data_dir.clone(),
                computer.clone(),
                (opened, playing),
                engine.clone(),
            ));
            viewers.insert(computer.clone(), (task, open, play));
        }
        let ended = tokio::select! {
            changed = wanted.changed() => changed.is_err(),
            changed = played.changed() => changed.is_err(),
        };
        if ended {
            for (task, _, _) in viewers.values() {
                task.abort();
            }
            return;
        }
    }
}

type Asked = (
    watch::Receiver<Option<String>>,
    watch::Receiver<BTreeSet<String>>,
);

/// One computer, watched until aborted, connecting again after a drop.
async fn view(data_dir: PathBuf, computer: String, mut asked: Asked, engine: EngineHandle) {
    let mut attempt = 0;
    loop {
        crate::wakes::count("remote vehicles: connect");
        let mut seen = Seen::new(&computer);
        match watch_once(&data_dir, &mut seen, &mut asked, &engine).await {
            Ok(()) => attempt = 0,
            Err(problem) => {
                seen.problem = Some(problem);
                engine.post(move |e| e.remote_viewed(seen.view()));
                attempt += 1;
            }
        }
        tokio::time::sleep(crate::edge::backoff(attempt, crate::edge::unit())).await;
    }
}

/// What a viewer has seen of a computer.
struct Seen {
    computer_id: String,
    computer_name: String,
    online: bool,
    problem: Option<String>,
    seq: u64,
    /// The relay's clock minus this one's.
    offset_ms: i64,
    vehicles: BTreeMap<String, RemoteVehicle>,
    trails: BTreeMap<String, Vec<TrailPoint>>,
    /// What the page needs to play each camera asked for (S84).
    video: Vec<RemoteVideo>,
    /// The media node each camera's grant was fetched for.
    granted_for: BTreeMap<String, Option<String>>,
}

impl Seen {
    fn new(computer: &str) -> Self {
        Self {
            computer_id: computer.to_owned(),
            computer_name: computer.to_owned(),
            online: false,
            problem: None,
            seq: 0,
            offset_ms: 0,
            vehicles: BTreeMap::new(),
            trails: BTreeMap::new(),
            video: Vec::new(),
            granted_for: BTreeMap::new(),
        }
    }

    /// The media node the computer publishes a camera to, as it last said.
    fn live_on(&self, camera: &str) -> Option<String> {
        self.vehicles
            .values()
            .flat_map(|v| &v.cameras)
            .find(|c| c.id == camera)
            .and_then(|c| c.live_on.clone())
    }

    /// Whether a camera moved to another node since its grant.
    fn moved(&self, cameras: &BTreeSet<String>) -> bool {
        cameras.iter().any(|c| {
            self.granted_for
                .get(c)
                .is_some_and(|n| *n != self.live_on(c))
        })
    }

    fn clock(&mut self, relay_at_ms: u64) {
        self.offset_ms =
            i64::try_from(relay_at_ms).unwrap_or(0) - i64::try_from(now_ms()).unwrap_or(0);
    }

    /// A vehicle's time on this computer's clock.
    fn here(&self, mut vehicle: RemoteVehicle) -> RemoteVehicle {
        vehicle.at_ms =
            u64::try_from(i64::try_from(vehicle.at_ms).unwrap_or(0) - self.offset_ms).unwrap_or(0);
        vehicle
    }

    fn take(&mut self, snapshot: EdgeSnapshot) {
        self.clock(snapshot.relay_at_ms);
        self.seq = snapshot.seq;
        self.online = snapshot.computer.online;
        self.computer_name = snapshot.computer.name;
        self.vehicles = snapshot
            .vehicles
            .into_iter()
            .map(|v| (v.id.clone(), self.here(v)))
            .collect();
        self.trails = snapshot
            .trails
            .into_iter()
            .map(|t| (t.vehicle_id, t.points))
            .collect();
    }

    /// A delta in order, or `false` for a gap.
    fn apply(&mut self, delta: EdgeDelta) -> bool {
        if delta.seq != self.seq + 1 {
            return false;
        }
        self.seq = delta.seq;
        self.clock(delta.relay_at_ms);
        for vehicle in delta.vehicles {
            let at_ms = vehicle.at_ms;
            if let Some(p) = vehicle.telemetry.as_ref().and_then(|t| t.positions.first()) {
                let trail = self.trails.entry(vehicle.id.clone()).or_default();
                let moved = trail.last().is_none_or(|last| {
                    // About a centimetre.
                    (last.latitude_deg - p.latitude_deg).abs() > 1e-7
                        || (last.longitude_deg - p.longitude_deg).abs() > 1e-7
                });
                if moved {
                    trail.push(TrailPoint {
                        at_ms,
                        latitude_deg: p.latitude_deg,
                        longitude_deg: p.longitude_deg,
                    });
                    if trail.len() > TRAIL_POINTS {
                        trail.remove(0);
                    }
                }
            }
            let vehicle = self.here(vehicle);
            self.vehicles.insert(vehicle.id.clone(), vehicle);
        }
        for id in delta.removed {
            self.vehicles.remove(&id);
            self.trails.remove(&id);
        }
        true
    }

    fn view(&self) -> RemoteView {
        RemoteView {
            computer_id: self.computer_id.clone(),
            computer_name: self.computer_name.clone(),
            online: self.online,
            problem: self.problem.clone(),
            vehicles: self.vehicles.values().cloned().collect(),
            trails: self
                .trails
                .iter()
                .map(|(id, points)| VehicleTrail {
                    vehicle_id: id.clone(),
                    points: points.clone(),
                })
                .collect(),
            video: self.video.clone(),
        }
    }
}

/// A read grant for one camera from the media coordinator.
async fn media_grant(
    data_dir: &std::path::Path,
    request: &MediaWatchRequest,
) -> Result<MediaWatchGrant, String> {
    let api = crate::feedback::api_url().ok_or("this UXDuck has no account server")?;
    let token = crate::adapters::credentials::FileCredentials::new(data_dir)
        .get(crate::account::TOKEN)
        .filter(|t| !t.is_empty())
        .ok_or("Not signed in")?;
    let answer = crate::online::client(std::time::Duration::from_secs(15))
        .ok_or("UXDuck cannot make a web client here")?
        .post(format!("{api}/v1/media/watch"))
        .bearer_auth(&token)
        .json(request)
        .send()
        .await
        .map_err(|_| "UXDuck cannot reach its server")?;
    match answer.status() {
        s if s.is_success() => answer
            .json()
            .await
            .map_err(|_| "the server's answer was not understood".to_owned()),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            Err("Live video from away is not set up on UXDuck's server yet".into())
        }
        s => Err(format!("UXDuck's server refused the camera ({s})")),
    }
}

/// Fresh grants for the cameras asked for, at the node each is on.
async fn grant_video(data_dir: &std::path::Path, seen: &mut Seen, cameras: &BTreeSet<String>) {
    let mut video = Vec::new();
    seen.granted_for.clear();
    for camera in cameras {
        let node = seen.live_on(camera);
        let request = MediaWatchRequest {
            device_id: seen.computer_id.clone(),
            camera_id: camera.clone(),
            node_id: node.clone(),
        };
        seen.granted_for.insert(camera.clone(), node);
        video.push(match media_grant(data_dir, &request).await {
            Ok(grant) => RemoteVideo {
                camera_id: camera.clone(),
                whep_url: Some(grant.whep_url),
                token: Some(grant.grant),
                ice_servers: grant.ice_servers,
                expires_at_ms: Some(grant.expires_at_ms),
                problem: None,
            },
            Err(problem) => RemoteVideo {
                camera_id: camera.clone(),
                whep_url: None,
                token: None,
                ice_servers: Vec::new(),
                expires_at_ms: None,
                problem: Some(problem),
            },
        });
    }
    seen.video = video;
}

fn watch_video(id: u64, cameras: &BTreeSet<String>) -> Message {
    Message::text(
        json!({ "jsonrpc": "2.0", "id": id, "method": "edge/watchVideo", "params": { "cameras": cameras } })
            .to_string(),
    )
}

fn watch_request(id: u64, vehicle: Option<&String>) -> Message {
    Message::text(
        json!({ "jsonrpc": "2.0", "id": id, "method": "edge/watch", "params": { "vehicleId": vehicle } })
            .to_string(),
    )
}

/// One connection: `Ok` when the relay closed it after it worked.
#[allow(
    clippy::too_many_lines,
    reason = "one connection's life, read top to bottom"
)]
async fn watch_once(
    data_dir: &std::path::Path,
    seen: &mut Seen,
    (detail, playing): &mut Asked,
    engine: &EngineHandle,
) -> Result<(), String> {
    let (socket, chosen) = crate::watch::open(data_dir, Some(&seen.computer_id)).await?;
    seen.computer_name = chosen.name;
    seen.online = chosen.online;
    let (mut sink, mut stream) = socket.split();
    let mut asked = 1;
    let vehicle = detail.borrow_and_update().clone();
    sink.send(watch_request(asked, vehicle.as_ref()))
        .await
        .map_err(|e| e.to_string())?;
    let mut cameras = playing.borrow_and_update().clone();
    if !cameras.is_empty() {
        asked += 1;
        sink.send(watch_video(asked, &cameras))
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut regrant = tokio::time::interval(REGRANT);
    regrant.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = playing.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                cameras.clone_from(&playing.borrow_and_update());
                asked += 1;
                sink.send(watch_video(asked, &cameras)).await.map_err(|e| e.to_string())?;
                grant_video(data_dir, seen, &cameras).await;
                let view = seen.view();
                engine.post(move |e| e.remote_viewed(view));
            }
            _ = regrant.tick(), if !cameras.is_empty() => {
                grant_video(data_dir, seen, &cameras).await;
                let view = seen.view();
                engine.post(move |e| e.remote_viewed(view));
            }
            changed = detail.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                asked += 1;
                let vehicle = detail.borrow_and_update().clone();
                sink.send(watch_request(asked, vehicle.as_ref())).await.map_err(|e| e.to_string())?;
            }
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else {
                    return Ok(());
                };
                let Message::Text(text) = frame else { continue };
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                let params = message["params"].clone();
                match message["method"].as_str() {
                    Some("edge/presence") => seen.online = params["online"] == true,
                    Some("edge/snapshot") => {
                        let Ok(snapshot) = serde_json::from_value(params) else { continue };
                        seen.take(snapshot);
                    }
                    Some("edge/delta") => {
                        let Ok(delta) = serde_json::from_value::<EdgeDelta>(params) else { continue };
                        if !seen.apply(delta) {
                            // A gap: start again from a snapshot.
                            asked += 1;
                            let vehicle = detail.borrow().clone();
                            sink.send(watch_request(asked, vehicle.as_ref())).await.map_err(|e| e.to_string())?;
                            continue;
                        }
                    }
                    None if message["result"].is_object() => {
                        let Ok(snapshot) = serde_json::from_value(message["result"].clone()) else { continue };
                        seen.take(snapshot);
                    }
                    _ => continue,
                }
                seen.problem = None;
                // The camera went live on another node: a grant for that one.
                if seen.moved(&cameras) {
                    grant_video(data_dir, seen, &cameras).await;
                }
                let view = seen.view();
                engine.post(move |e| e.remote_viewed(view));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vehicle(id: &str, at_ms: u64) -> RemoteVehicle {
        serde_json::from_value(json!({
            "id": id, "name": "Copter 1", "vehicleType": "Quadrotor", "autopilot": "ArduPilotMega",
            "presence": "online", "armed": false, "atMs": at_ms,
        }))
        .unwrap()
    }

    #[test]
    fn deltas_apply_in_order_a_gap_is_refused_and_times_move_to_this_clock() {
        let mut seen = Seen::new("dv1");
        let relay_now = now_ms() + 5_000;
        seen.take(EdgeSnapshot {
            seq: 4,
            computer: serde_json::from_value(
                json!({ "id": "dv1", "name": "Laptop", "online": true, "lastSeenAtMs": 1 }),
            )
            .unwrap(),
            vehicles: vec![vehicle("v1", relay_now - 400)],
            trails: Vec::new(),
            relay_at_ms: relay_now,
        });
        let view = seen.view();
        assert_eq!(view.computer_name, "Laptop");
        // 400 ms old on the relay's clock is 400 ms old on this one.
        let age = now_ms().saturating_sub(view.vehicles[0].at_ms);
        assert!((350..1_000).contains(&age), "{age}");
        let delta = |seq: u64| EdgeDelta {
            seq,
            source_seq: seq,
            vehicles: vec![vehicle("v1", relay_now)],
            removed: Vec::new(),
            relay_at_ms: relay_now,
        };
        assert!(!seen.apply(delta(6)), "a gap");
        assert!(seen.apply(delta(5)));
        assert_eq!(seen.seq, 5);
        assert!(seen.apply(EdgeDelta {
            removed: vec!["v1".into()],
            vehicles: Vec::new(),
            ..delta(6)
        }));
        assert!(seen.view().vehicles.is_empty());
    }
}

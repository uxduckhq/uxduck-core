//! The keeper against a fake `MediaMTX` API: the configured settings put back
//! at the start, what the rule wants applied, applied again after a
//! restart, and the settings a sender read before kept across its own
//! restart.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::routing::{get, patch};

use super::*;

/// `MediaMTX`'s API as the sender uses it: `/v3/info`'s `started`, each path's
/// configuration, and changes kept in memory until a restart, as `MediaMTX`
/// keeps them.
#[derive(Clone)]
pub struct Fake {
    pub url: String,
    state: Arc<Mutex<FakeState>>,
}

struct FakeState {
    started: u32,
    configured: BTreeMap<String, serde_json::Value>,
    paths: BTreeMap<String, serde_json::Value>,
    patches: Vec<(String, Settings)>,
    records: Vec<(String, bool)>,
    /// The paths someone publishes to, ready to record.
    published: std::collections::BTreeSet<String>,
    /// The paths added through the API, in order.
    added: Vec<String>,
}

pub fn rpi(settings: Settings) -> serde_json::Value {
    serde_json::json!({
        "source": "rpiCamera",
        "rpiCameraBitrate": settings.bitrate,
        "rpiCameraFPS": settings.fps,
        "rpiCameraIDRPeriod": settings.idr,
        "rpiCameraWidth": 1280,
    })
}

impl Fake {
    #[allow(clippy::too_many_lines, reason = "one route for each part of the API")]
    pub async fn start(paths: &[(&str, serde_json::Value)]) -> Self {
        let configured: BTreeMap<String, serde_json::Value> = paths
            .iter()
            .map(|(p, v)| ((*p).to_owned(), v.clone()))
            .collect();
        let state = Arc::new(Mutex::new(FakeState {
            started: 1,
            paths: configured.clone(),
            configured,
            patches: Vec::new(),
            records: Vec::new(),
            published: std::collections::BTreeSet::new(),
            added: Vec::new(),
        }));
        let router = axum::Router::new()
            .route(
                "/v3/info",
                get(|State(s): State<Arc<Mutex<FakeState>>>| async move {
                    let started = s.lock().unwrap().started;
                    Json(serde_json::json!({ "version": "v1.21.1", "started": format!("2026-11-22T10:00:0{started}Z") }))
                }),
            )
            .route(
                "/v3/config/paths/get/{*path}",
                get(
                    |State(s): State<Arc<Mutex<FakeState>>>, UrlPath(path): UrlPath<String>| async move {
                        match s.lock().unwrap().paths.get(&path) {
                            Some(conf) => Ok(Json(conf.clone())),
                            None => Err(axum::http::StatusCode::NOT_FOUND),
                        }
                    },
                ),
            )
            .route(
                "/v3/config/paths/add/{*path}",
                axum::routing::post(
                    |State(s): State<Arc<Mutex<FakeState>>>,
                     UrlPath(path): UrlPath<String>,
                     Json(body): Json<serde_json::Value>| async move {
                        let mut s = s.lock().unwrap();
                        if s.paths.contains_key(&path) {
                            return Err(axum::http::StatusCode::BAD_REQUEST);
                        }
                        s.paths.insert(path.clone(), body);
                        s.added.push(path);
                        Ok(())
                    },
                ),
            )
            .route(
                "/v3/paths/get/{*path}",
                get(
                    |State(s): State<Arc<Mutex<FakeState>>>, UrlPath(path): UrlPath<String>| async move {
                        let s = s.lock().unwrap();
                        if !s.paths.contains_key(&path) {
                            return Err(axum::http::StatusCode::NOT_FOUND);
                        }
                        Ok(Json(serde_json::json!({ "name": path, "ready": s.published.contains(&path) })))
                    },
                ),
            )
            .route(
                "/v3/config/paths/patch/{*path}",
                patch(
                    |State(s): State<Arc<Mutex<FakeState>>>,
                     UrlPath(path): UrlPath<String>,
                     Json(body): Json<serde_json::Value>| async move {
                        let mut s = s.lock().unwrap();
                        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
                        if let Some(record) = body.get("record").and_then(serde_json::Value::as_bool) {
                            assert_eq!(keys, ["record"], "record alone");
                            let Some(conf) = s.paths.get_mut(&path) else {
                                return Err(axum::http::StatusCode::NOT_FOUND);
                            };
                            conf["record"] = record.into();
                            s.records.push((path, record));
                            return Ok(());
                        }
                        assert_eq!(
                            keys,
                            ["rpiCameraBitrate", "rpiCameraFPS", "rpiCameraIDRPeriod"],
                            "only the three live settings"
                        );
                        let settings = Settings {
                            bitrate: body["rpiCameraBitrate"].as_u64().unwrap(),
                            fps: body["rpiCameraFPS"].as_f64().unwrap(),
                            idr: body["rpiCameraIDRPeriod"].as_u64().unwrap(),
                        };
                        let conf = s.paths.entry(path.clone()).or_insert_with(|| rpi(settings));
                        for (key, value) in body.as_object().unwrap() {
                            conf[key] = value.clone();
                        }
                        s.patches.push((path, settings));
                        Ok(())
                    },
                ),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self { url, state }
    }

    pub fn patches(&self) -> Vec<(String, Settings)> {
        self.state.lock().unwrap().patches.clone()
    }

    pub fn records(&self) -> Vec<(String, bool)> {
        self.state.lock().unwrap().records.clone()
    }

    /// The path's configuration as the API gives it now.
    pub fn path(&self, path: &str) -> serde_json::Value {
        self.state.lock().unwrap().paths[path].clone()
    }

    /// Waits up to `within` for the record changes to reach `count`.
    pub async fn wait_for_records(&self, count: usize, within: Duration) -> Vec<(String, bool)> {
        let until = tokio::time::Instant::now() + within;
        while self.records().len() < count && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.records()
    }

    /// `MediaMTX` restarted: its changes and added paths gone, its
    /// publishers too, `started` new.
    pub fn restart(&self) {
        let mut s = self.state.lock().unwrap();
        s.started += 1;
        s.paths = s.configured.clone();
        s.published.clear();
    }

    /// Someone publishes to `path` now, or stopped.
    pub fn publish(&self, path: &str, on: bool) {
        let mut s = self.state.lock().unwrap();
        if on {
            s.published.insert(path.to_owned());
        } else {
            s.published.remove(path);
        }
    }

    /// The paths added through the API, in order.
    pub fn added(&self) -> Vec<String> {
        self.state.lock().unwrap().added.clone()
    }

    /// Whether `MediaMTX` has the path now.
    pub fn has(&self, path: &str) -> bool {
        self.state.lock().unwrap().paths.contains_key(path)
    }

    /// The person changed a camera's configuration, which a restart applies.
    pub fn configure(&self, path: &str, conf: serde_json::Value) {
        self.state
            .lock()
            .unwrap()
            .configured
            .insert(path.to_owned(), conf);
    }

    /// Waits up to `within` for the settings changes to reach `count`.
    pub async fn wait_for(&self, count: usize, within: Duration) -> Vec<(String, Settings)> {
        let until = tokio::time::Instant::now() + within;
        while self.patches().len() < count && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.patches()
    }
}

pub const FRONT: Settings = Settings {
    bitrate: 1_000_000,
    fps: 30.0,
    idr: 30,
};

#[tokio::test]
async fn a_start_puts_the_configured_settings_back_and_a_restart_applies_the_rule_again() {
    let fake = Fake::start(&[
        ("front", rpi(FRONT)),
        (
            "usb",
            serde_json::json!({ "source": "rtsp://10.0.0.9/cam" }),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("video-sender.json");
    let api = Api::new(&fake.url).unwrap();
    let paths = vec!["front".to_owned(), "usb".to_owned()];
    let (started, cameras) = configured(&api, &state_file, &paths).await.unwrap();
    assert_eq!(cameras["front"], Camera::RpiCamera(FRONT));
    assert_eq!(
        cameras["usb"],
        Camera::Other {
            source: "rtsp://10.0.0.9/cam".into()
        }
    );
    let (configured_tx, mut configured_rx) = watch::channel(cameras);
    let (wanted, wanted_rx) = watch::channel(BTreeMap::new());
    tokio::spawn(keep(api, state_file, started, configured_tx, wanted_rx));

    // The start puts the Raspberry Pi camera back, and leaves the other.
    let patches = fake.wait_for(1, Duration::from_secs(3)).await;
    assert_eq!(patches, [("front".to_owned(), FRONT)]);

    let follows = Settings {
        bitrate: 500_000,
        fps: 15.0,
        idr: 15,
    };
    wanted.send_replace(BTreeMap::from([("front".to_owned(), Some(follows))]));
    let patches = fake.wait_for(2, Duration::from_secs(2)).await;
    assert_eq!(patches[1], ("front".to_owned(), follows));

    // A restart with the person's new bitrate: read again, said, and the
    // rule's settings applied again.
    let changed = Settings {
        bitrate: 2_000_000,
        ..FRONT
    };
    fake.configure("front", rpi(changed));
    fake.restart();
    let patches = fake.wait_for(3, Duration::from_secs(3)).await;
    assert_eq!(patches[2], ("front".to_owned(), follows));
    assert_eq!(
        configured_rx.borrow_and_update()["front"],
        Camera::RpiCamera(changed)
    );

    // The last viewer left: the configured settings back.
    wanted.send_replace(BTreeMap::from([("front".to_owned(), None)]));
    let patches = fake.wait_for(4, Duration::from_secs(2)).await;
    assert_eq!(patches[3], ("front".to_owned(), changed));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.patches().len(), 4, "nothing more once it is back");
}

#[tokio::test]
async fn a_sender_started_again_keeps_the_settings_it_read_until_mediamtx_restarts() {
    let fake = Fake::start(&[("front", rpi(FRONT))]).await;
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("video-sender.json");
    let api = Api::new(&fake.url).unwrap();
    let paths = vec!["front".to_owned()];
    configured(&api, &state_file, &paths).await.unwrap();
    // The sender stopped while the camera followed a poor connection.
    let low = Settings {
        bitrate: 300_000,
        fps: 6.0,
        idr: 6,
    };
    api.set("front", low).await.unwrap();
    let (_, cameras) = configured(&api, &state_file, &paths).await.unwrap();
    assert_eq!(
        cameras["front"],
        Camera::RpiCamera(FRONT),
        "not its own change"
    );
    // Once MediaMTX restarts, what it reads is the person's.
    fake.restart();
    let (_, cameras) = configured(&api, &state_file, &paths).await.unwrap();
    assert_eq!(cameras["front"], Camera::RpiCamera(FRONT));
    assert!(api.camera("nowhere").await.unwrap_err().contains("404"));
    // A state file it cannot write is said.
    let error = configured(&api, &dir.path().join("missing/state.json"), &paths)
        .await
        .unwrap_err();
    assert!(
        error.contains("cannot write the video sender's state file"),
        "{error}"
    );
}

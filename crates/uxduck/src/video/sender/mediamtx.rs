//! The camera computer's `MediaMTX` API (ADR-0064 W6, W8): what each
//! camera is set to, live changes of its bitrate, frame rate and keyframe
//! interval, and putting them back.
//!
//! `MediaMTX` keeps an API change in memory only, so a restart of it puts
//! the camera back to its configured settings. The keeper reads `/v3/info`
//! once a second and, when `started` changes, reads the settings again and
//! applies what the rule wants again. When the sender starts, and when a
//! camera's last viewer leaves, it puts the configured settings back.
//!
//! The configured settings are kept in a state file with the `started` they
//! were read at. A sender that stopped while it had a camera changed, and
//! starts again with the same `MediaMTX` running, would otherwise read its
//! own change back as what the person chose.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::rule::Settings;

/// Each request: `MediaMTX` is on this computer or its LAN and answers in
/// milliseconds, so a request this slow has gone wrong.
const ASK_FOR: Duration = Duration::from_secs(5);
/// How often `started` is read, and a failed change tried again (W8).
pub(super) const WATCH_EVERY: Duration = Duration::from_secs(1);

/// A camera as `MediaMTX` has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Camera {
    /// A Raspberry Pi camera, and the settings it is configured with.
    RpiCamera(Settings),
    /// Any other source, sent at its own rate.
    Other { source: String },
}

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    base: String,
    user: Option<String>,
    password: Option<String>,
}

impl Api {
    /// `http://[user[:password]@]host:port`.
    ///
    /// # Errors
    /// An address that is not one.
    pub fn new(url: &str) -> Result<Self, String> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| format!("the MediaMTX API address {url} is not one: {e}"))?;
        if !["http", "https"].contains(&parsed.scheme()) || parsed.host_str().is_none() {
            return Err(format!(
                "the MediaMTX API address {url} must be http://host:port, such as http://127.0.0.1:9997"
            ));
        }
        let user =
            (!parsed.username().is_empty()).then(|| super::super::rtsp::decode(parsed.username()));
        let password = parsed.password().map(super::super::rtsp::decode);
        let mut base = parsed.clone();
        let _ = base.set_username("");
        let _ = base.set_password(None);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(ASK_FOR)
            .build()
            .map_err(|e| format!("UXDuck could not set up its MediaMTX client: {e}"))?;
        Ok(Self {
            client,
            base: base.as_str().trim_end_matches('/').to_owned(),
            user,
            password,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.client.request(method, format!("{}{path}", self.base));
        match &self.user {
            Some(user) => request.basic_auth(user, self.password.as_ref()),
            None => request,
        }
    }

    async fn json(&self, path: &str) -> Result<serde_json::Value, String> {
        self.json_if_any(path)
            .await?
            .ok_or_else(|| format!("MediaMTX's API answered {path} with 404 Not Found"))
    }

    /// The JSON at `path`, or None when `MediaMTX` has nothing there.
    async fn json_if_any(&self, path: &str) -> Result<Option<serde_json::Value>, String> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| format!("MediaMTX's API at {} did not answer: {e}", self.base))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "MediaMTX's API answered {path} with {status}: {}",
                body.trim()
            ));
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| format!("MediaMTX's API answered {path} with something not JSON: {e}"))
    }

    /// When `MediaMTX` started, which changes when it restarts.
    ///
    /// # Errors
    /// No answer, or one without it.
    pub async fn started(&self) -> Result<String, String> {
        self.json("/v3/info")
            .await?
            .get("started")
            .and_then(|s| s.as_str())
            .map(str::to_owned)
            .ok_or_else(|| "MediaMTX's /v3/info has no started time".to_owned())
    }

    /// The camera at `path` as configured now.
    ///
    /// # Errors
    /// No answer, or no such path.
    pub async fn camera(&self, path: &str) -> Result<Camera, String> {
        let conf = self.path(path).await?;
        let source = conf
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_owned();
        if source != "rpiCamera" {
            return Ok(Camera::Other { source });
        }
        let number = |key: &str| {
            conf.get(key)
                .and_then(serde_json::Value::as_f64)
                .filter(|v| *v > 0.0)
                .ok_or_else(|| format!("MediaMTX gives {path} no {key}"))
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "MediaMTX keeps these as whole numbers"
        )]
        Ok(Camera::RpiCamera(Settings {
            bitrate: number("rpiCameraBitrate")? as u64,
            fps: number("rpiCameraFPS")?,
            idr: number("rpiCameraIDRPeriod")? as u64,
        }))
    }

    /// The whole configuration of the path at `path`.
    ///
    /// # Errors
    /// No answer, or no such path.
    pub async fn path(&self, path: &str) -> Result<serde_json::Value, String> {
        self.json(&format!("/v3/config/paths/get/{path}")).await
    }

    /// The whole configuration of the path at `path`, or None when
    /// `MediaMTX` has no such path, as after a restart forgot one the API
    /// added.
    ///
    /// # Errors
    /// No answer.
    pub async fn path_if_any(&self, path: &str) -> Result<Option<serde_json::Value>, String> {
        self.json_if_any(&format!("/v3/config/paths/get/{path}"))
            .await
    }

    /// Whether someone publishes to the path at `path` now, its tracks ready
    /// to read or record.
    ///
    /// # Errors
    /// No answer.
    pub async fn ready(&self, path: &str) -> Result<bool, String> {
        Ok(self
            .json_if_any(&format!("/v3/paths/get/{path}"))
            .await?
            .and_then(|p| p.get("ready").and_then(serde_json::Value::as_bool))
            .unwrap_or(false))
    }

    /// Adds a path live, which `MediaMTX` keeps until it restarts.
    ///
    /// # Errors
    /// No answer, or a refusal.
    pub async fn add(&self, path: &str, conf: &serde_json::Value) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v3/config/paths/add/{path}"),
            )
            .json(conf)
            .send()
            .await
            .map_err(|e| format!("MediaMTX's API at {} did not answer: {e}", self.base))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "MediaMTX refused to add the path {path} with {status}: {}",
            body.trim()
        ))
    }

    /// Sets the camera's encoder live. Only these three settings, which
    /// `MediaMTX` changes without stopping the camera (W6).
    ///
    /// # Errors
    /// No answer, or a refusal.
    pub async fn set(&self, path: &str, settings: Settings) -> Result<(), String> {
        self.patch(
            path,
            &serde_json::json!({
                "rpiCameraBitrate": settings.bitrate,
                "rpiCameraFPS": settings.fps,
                "rpiCameraIDRPeriod": settings.idr,
            }),
        )
        .await
    }

    /// Changes the path at `path` live, as far as `MediaMTX` keeps it.
    ///
    /// # Errors
    /// No answer, or a refusal.
    pub async fn patch(&self, path: &str, change: &serde_json::Value) -> Result<(), String> {
        let response = self
            .request(
                reqwest::Method::PATCH,
                &format!("/v3/config/paths/patch/{path}"),
            )
            .json(change)
            .send()
            .await
            .map_err(|e| format!("MediaMTX's API at {} did not answer: {e}", self.base))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "MediaMTX refused the change to {path} with {status}: {}",
            body.trim()
        ))
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    started: String,
    cameras: BTreeMap<String, Camera>,
}

/// The cameras' configured settings and when `MediaMTX` started: kept from the
/// state file when `MediaMTX` has not restarted since, read from it otherwise,
/// and written back.
///
/// # Errors
/// `MediaMTX` not answering, a path it does not have, or a state file that
/// cannot be written.
pub async fn configured(
    api: &Api,
    state_file: &Path,
    paths: &[String],
) -> Result<(String, BTreeMap<String, Camera>), String> {
    let started = api.started().await?;
    let kept: State = std::fs::read(state_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(|kept: &State| kept.started == started)
        .unwrap_or_default();
    let mut cameras = BTreeMap::new();
    for path in paths {
        let camera = match kept.cameras.get(path) {
            Some(camera) => camera.clone(),
            None => api.camera(path).await?,
        };
        cameras.insert(path.clone(), camera);
    }
    write_state(
        state_file,
        &State {
            started: started.clone(),
            cameras: cameras.clone(),
        },
    )?;
    Ok((started, cameras))
}

fn write_state(file: &Path, state: &State) -> Result<(), String> {
    let mut temporary = PathBuf::from(file);
    temporary.as_mut_os_string().push(".new");
    let bytes = serde_json::to_vec_pretty(state).unwrap_or_default();
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, file))
        .map_err(|e| {
            format!(
                "UXDuck cannot write the video sender's state file {}: {e}",
                file.display()
            )
        })
}

/// Keeps each Raspberry Pi camera at what `wanted` says, its configured settings
/// when that is `None`, through restarts of `MediaMTX`. Its first pass puts
/// every camera back to its configured settings. `configured` hears the settings
/// read again after a restart. Runs until `wanted`'s sender is dropped.
pub async fn keep(
    api: Api,
    state_file: PathBuf,
    mut started: String,
    configured: watch::Sender<BTreeMap<String, Camera>>,
    mut wanted: watch::Receiver<BTreeMap<String, Option<Settings>>>,
) {
    let paths: Vec<String> = configured.borrow().keys().cloned().collect();
    // What MediaMTX has now, as far as the keeper knows: nothing at first,
    // so the configured settings go back.
    let mut applied: BTreeMap<String, Settings> = BTreeMap::new();
    let mut said: BTreeMap<String, String> = BTreeMap::new();
    let mut timer = tokio::time::interval(WATCH_EVERY);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = timer.tick() => {
                match api.started().await {
                    Ok(now) if now != started => {
                        tracing::info!(%now, "MediaMTX restarted: reading the cameras again");
                        if let Ok((now, cameras)) = self::configured(&api, &state_file, &paths)
                            .await
                            .inspect_err(|error| tracing::warn!(%error, "cannot read the cameras again"))
                        {
                            started = now;
                            applied = cameras
                                .iter()
                                .filter_map(|(path, camera)| match camera {
                                    Camera::RpiCamera(s) => Some((path.clone(), *s)),
                                    Camera::Other { .. } => None,
                                })
                                .collect();
                            configured.send_replace(cameras);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!(%error, "MediaMTX's API did not answer");
                        continue;
                    }
                }
            }
        }
        let wanted_now = wanted.borrow_and_update().clone();
        let cameras = configured.borrow().clone();
        for (path, camera) in &cameras {
            let Camera::RpiCamera(own) = camera else {
                continue;
            };
            let want = wanted_now.get(path).copied().flatten().unwrap_or(*own);
            if applied.get(path) == Some(&want) {
                continue;
            }
            match api.set(path, want).await {
                Ok(()) => {
                    tracing::info!(
                        %path,
                        bitrate = want.bitrate,
                        fps = want.fps,
                        idr = want.idr,
                        configured = want == *own,
                        "set the camera's encoder"
                    );
                    applied.insert(path.clone(), want);
                    said.remove(path);
                }
                Err(error) => {
                    // Tried again each second; said once until it works.
                    if said.get(path) != Some(&error) {
                        tracing::warn!(%path, %error, "cannot set the camera's encoder");
                        said.insert(path.clone(), error);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(in crate::video) mod tests;

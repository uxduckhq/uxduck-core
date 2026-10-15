//! Camera video (ADR-0017, S38, S39): UDP listeners, RTSP cameras, the hub
//! that relays pictures to `/video` sockets, and the socket itself. The
//! daemon parses and relays; it never decodes a picture (V1).

pub mod hub;
pub mod listener;
pub mod recorder;
pub mod rtsp;
pub mod socket;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use uxduck_contracts::dto::video::VideoSocket;

use crate::daemon::engine_task::EngineHandle;

/// The engine's end: listeners and RTSP cameras added by hand, and the hub.
#[derive(Debug)]
pub struct VideoHost {
    hub: Arc<hub::Hub>,
    engine: EngineHandle,
    runtime: tokio::runtime::Handle,
    /// Ports added by hand, each with its listener's stop flag.
    listeners: HashMap<u16, Arc<AtomicBool>>,
    rtsp: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl uxduck_engine::video::VideoPort for VideoHost {
    fn allow(&mut self, token: &str, source_id: &str) {
        self.hub.allow(token, source_id);
    }

    fn revoke(&mut self, token: &str) {
        self.hub.revoke(token);
    }

    fn sockets(&self) -> Vec<VideoSocket> {
        self.hub.sockets()
    }

    fn listen(&mut self, port: u16) -> Result<(), String> {
        if self.listeners.contains_key(&port) {
            return Ok(());
        }
        let socket = listener::bind(port)?;
        let stop = Arc::new(AtomicBool::new(false));
        listener::spawn(socket, self.hub.clone(), self.engine.clone(), stop.clone());
        self.listeners.insert(port, stop);
        tracing::info!(port, "listening for video on a port added by hand");
        Ok(())
    }

    fn unlisten(&mut self, port: u16) {
        if let Some(stop) = self.listeners.remove(&port) {
            stop.store(true, Ordering::Relaxed);
        }
    }

    fn open_rtsp(&mut self, source_id: &str, url: &str) {
        self.close_rtsp(source_id);
        let task = rtsp::spawn(
            &self.runtime,
            source_id.to_owned(),
            url.to_owned(),
            self.hub.clone(),
            self.engine.clone(),
        );
        self.rtsp.insert(source_id.to_owned(), task);
    }

    fn close_rtsp(&mut self, source_id: &str) {
        if let Some(task) = self.rtsp.remove(source_id) {
            task.abort();
        }
    }

    fn start_recording(&mut self, recording_id: &str, source_id: &str) {
        let (to, from) = std::sync::mpsc::channel();
        self.hub.record(recording_id, source_id, to);
        recorder::spawn(
            self.hub.dir().to_owned(),
            recording_id.to_owned(),
            from,
            self.engine.clone(),
        );
    }

    fn stop_recording(&mut self, recording_id: &str) {
        self.hub.stop_recording(recording_id);
    }

    fn recover_recording(&mut self, recording_id: &str) -> Option<(u64, u64)> {
        recorder::recover(self.hub.dir(), recording_id)
    }

    fn allow_playback(&mut self, token: &str, recording_id: &str) {
        self.hub.allow_playback(token, recording_id);
    }

    fn reveal(&mut self, recording_id: Option<&str>) -> Result<String, String> {
        let dir = self.hub.dir();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = recording_id
            .map(|id| recorder::mp4_path(dir, id))
            .filter(|path| path.exists());
        crate::adapters::browser::reveal(dir, file.as_deref())
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(dir.display().to_string())
    }
}

/// The daemon's one hub, for the live publisher (S84), which starts with
/// the account's connection before the hub exists.
static HUB: std::sync::OnceLock<Arc<hub::Hub>> = std::sync::OnceLock::new();

/// The hub, once video has started.
pub fn hub() -> Option<Arc<hub::Hub>> {
    HUB.get().cloned()
}

/// Starts listening on `port` (0 for any free port, `None` for off), gives
/// the engine its end, which starts the ports and cameras added by hand.
pub async fn start(
    port: Option<u16>,
    engine: &EngineHandle,
    dir: std::path::PathBuf,
) -> Arc<hub::Hub> {
    let hub = Arc::new(hub::Hub::new(dir));
    let _ = HUB.set(hub.clone());
    let host = VideoHost {
        hub: hub.clone(),
        engine: engine.clone(),
        runtime: tokio::runtime::Handle::current(),
        listeners: HashMap::new(),
        rtsp: HashMap::new(),
    };
    let _ = engine.run(move |e| e.set_video(Box::new(host))).await;
    if let Some(port) = port {
        let (listening, error) = match listener::bind(port) {
            Ok(socket) => {
                let bound = socket.local_addr().map_or(port, |a| a.port());
                // The main port lives as long as the daemon.
                let stop = Arc::new(AtomicBool::new(false));
                listener::spawn(socket, hub.clone(), engine.clone(), stop);
                tracing::info!(port = bound, "listening for video");
                (Some(bound), None)
            }
            Err(error) => {
                tracing::warn!(%error, "not listening for video");
                (None, Some(error))
            }
        };
        let _ = engine
            .run(move |e| e.video_listening(listening, error))
            .await;
    }
    hub
}

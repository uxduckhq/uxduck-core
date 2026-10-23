//! Camera video (ADR-0017, S38, S39): UDP listeners, RTSP cameras, the hub
//! that relays pictures to `/video` sockets, and the socket itself. The
//! daemon parses and relays; it never decodes a picture (V1).

pub mod capture;
pub mod demos;
pub mod file;
pub mod folder;
pub mod hub;
pub mod listener;
pub mod permission;
pub mod playback;
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
    captures: HashMap<String, tokio::task::JoinHandle<()>>,
    selections: HashMap<String, permission::Selection>,
    devices: Arc<capture::Devices>,
    files: HashMap<String, tokio::task::JoinHandle<()>>,
    demos: Arc<demos::Demos>,
}

impl uxduck_engine::video::VideoPort for VideoHost {
    fn demand(&mut self, source_id: &str, needed: bool) {
        self.hub.demand(source_id, needed);
    }

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

    fn devices(&mut self) -> uxduck_contracts::dto::video::VideoDevicesResult {
        self.devices.list(&self.runtime)
    }

    fn check_capture(
        &self,
        input: &uxduck_core::domain::video::CaptureInput,
    ) -> Result<(), String> {
        self.devices.check(input)
    }

    fn open_capture(
        &mut self,
        source_id: &str,
        input: &uxduck_core::domain::video::CaptureInput,
        epoch: u64,
    ) {
        if let Some(task) = self.captures.remove(source_id) {
            task.abort();
        }
        let task = capture::spawn(
            &self.runtime,
            source_id.to_owned(),
            input.clone(),
            epoch,
            self.hub.clone(),
            self.engine.clone(),
            self.selections.get(source_id).map(|s| s.answer.clone()),
        );
        self.captures.insert(source_id.to_owned(), task);
    }

    fn close_capture(&mut self, source_id: &str) {
        self.selections.remove(source_id);
        if let Some(task) = self.captures.remove(source_id) {
            task.abort();
        }
    }

    fn select_capture(
        &mut self,
        source_id: &str,
        input: &uxduck_core::domain::video::CaptureInput,
    ) {
        if !permission::required() {
            return;
        }
        self.selections.insert(
            source_id.to_owned(),
            permission::Selection::start(&self.runtime, self.devices.wants_sound(input)),
        );
    }

    fn capture_settings(&mut self, microphone: bool) -> Result<(), String> {
        permission::settings(microphone)
    }

    fn check_file(&self, path: &str) -> Result<(), String> {
        file::check(path)
    }
    fn open_file(
        &mut self,
        id: &str,
        input: &uxduck_core::domain::video::VideoFileInput,
        epoch: u64,
    ) {
        self.close_file(id);
        self.files.insert(
            id.to_owned(),
            file::spawn(
                &self.runtime,
                id.to_owned(),
                input.clone(),
                epoch,
                self.hub.clone(),
                self.engine.clone(),
            ),
        );
    }
    fn close_file(&mut self, id: &str) {
        if let Some(task) = self.files.remove(id) {
            task.abort();
        }
    }
    fn demos(
        &mut self,
        params: &uxduck_contracts::dto::video::VideoDemosParams,
    ) -> Result<uxduck_contracts::dto::video::VideoDemosResult, String> {
        self.demos.request(&self.runtime, params)
    }
    fn demo_file(&self, id: &str) -> Result<uxduck_core::domain::video::VideoFileInput, String> {
        self.demos.file(id)
    }

    fn start_recording(
        &mut self,
        recording_id: &str,
        source_id: &str,
        // MPEG-TS keeps a camera's sound as it came (S151).
        _sound: bool,
        stem: &str,
    ) -> String {
        let (to, from) = std::sync::mpsc::channel();
        self.hub.record(recording_id, source_id, to);
        let dir = self.hub.dir().to_owned();
        // The name is taken now, so the engine keeps the one on disk.
        let file = recorder::reserve(&dir, stem);
        let name = file
            .as_ref()
            .map_or_else(|_| format!("{stem}.ts"), |(name, _)| name.clone());
        recorder::spawn(
            dir.clone(),
            playback::cache_dir(&dir),
            recording_id.to_owned(),
            file,
            from,
            self.engine.clone(),
        );
        name
    }

    fn utc_offset_minutes(&self, at_us: u64) -> i32 {
        utc_offset_minutes(at_us)
    }

    fn stop_recording(&mut self, recording_id: &str) {
        self.hub.stop_recording(recording_id);
    }

    fn recover_recording(&mut self, file: &str) -> Option<(u64, u64)> {
        recorder::recover(self.hub.dir(), file)
    }

    fn allow_playback(&mut self, token: &str, recording_id: &str, file: &str) {
        self.hub.allow_playback(token, recording_id, file);
    }

    fn reveal(&mut self, file: Option<&str>) -> Result<String, String> {
        let dir = self.hub.dir();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = file.map(|name| dir.join(name)).filter(|path| path.exists());
        crate::adapters::browser::reveal(dir, file.as_deref())
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(dir.display().to_string())
    }

    fn save_photo(&mut self, stem: &str, jpeg: &[u8]) -> Result<String, String> {
        save_photo(self.hub.dir(), stem, jpeg)
    }

    fn allow_photo(&mut self, token: &str, photo_id: &str, file_name: &str) {
        self.hub.allow_photo(token, photo_id, file_name);
    }

    fn reveal_photo(&mut self, file_name: &str) -> Result<String, String> {
        let dir = self.hub.dir();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = Some(dir.join(file_name)).filter(|path| path.exists());
        crate::adapters::browser::reveal(dir, file.as_deref())
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(dir.display().to_string())
    }
}

/// The computer's offset from UTC at `at_us`, in minutes, from its time
/// zone; 0 when it has none to read (S150).
fn utc_offset_minutes(at_us: u64) -> i32 {
    let at = i64::try_from(at_us).unwrap_or(i64::MAX);
    jiff::Timestamp::from_microsecond(at).map_or(0, |t| {
        jiff::tz::TimeZone::system().to_offset(t).seconds() / 60
    })
}

/// Writes `<stem>.jpg` into `dir`, or `<stem> 2.jpg` and on when a file
/// has that name, never replacing one: the name it got.
fn save_photo(dir: &std::path::Path, stem: &str, jpeg: &[u8]) -> Result<String, String> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for n in 1..1000 {
        let name = if n == 1 {
            format!("{stem}.jpg")
        } else {
            format!("{stem} {n}.jpg")
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&name))
        {
            Ok(mut file) => {
                return file
                    .write_all(jpeg)
                    .and_then(|()| file.sync_all())
                    .map(|()| name)
                    .map_err(|e| format!("UXDuck couldn't save the photo: {e}"));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("UXDuck couldn't save the photo: {e}")),
        }
    }
    Err("UXDuck couldn't find a free name for the photo.".into())
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
    // Playback copies unused for two weeks go (S151).
    let cache = playback::cache_dir(hub.dir());
    tokio::task::spawn_blocking(move || {
        let freed = playback::clean(&cache, playback::UNUSED_FOR);
        if freed > 0 {
            tracing::info!(freed, "old playback copies removed");
        }
    });
    let host = VideoHost {
        hub: hub.clone(),
        engine: engine.clone(),
        runtime: tokio::runtime::Handle::current(),
        listeners: HashMap::new(),
        rtsp: HashMap::new(),
        captures: HashMap::new(),
        selections: HashMap::new(),
        devices: Arc::new(capture::Devices::default()),
        files: HashMap::new(),
        demos: Arc::new(demos::Demos::new(
            playback::cache_dir(hub.dir()).with_file_name("demo-videos"),
        )),
    };
    let _ = engine.run(move |e| e.set_video(Box::new(host))).await;
    // Files deleted in Finder leave the list (S151).
    folder::watch(
        hub.dir().to_owned(),
        playback::cache_dir(hub.dir()),
        engine.clone(),
    );
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

impl Drop for VideoHost {
    fn drop(&mut self) {
        for task in self.files.values() {
            task.abort();
        }
        for task in self.captures.values() {
            task.abort();
        }
        for task in self.rtsp.values() {
            task.abort();
        }
        for stop in self.listeners.values() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_photo_never_replaces_another() {
        let dir = tempfile::tempdir().expect("temp dir");
        let at = dir.path().join("UXDuck");
        assert_eq!(
            super::save_photo(&at, "2026-10-18 10.42.03 Alpha", b"one").unwrap(),
            "2026-10-18 10.42.03 Alpha.jpg"
        );
        assert_eq!(
            super::save_photo(&at, "2026-10-18 10.42.03 Alpha", b"two").unwrap(),
            "2026-10-18 10.42.03 Alpha 2.jpg"
        );
        assert_eq!(
            std::fs::read(at.join("2026-10-18 10.42.03 Alpha.jpg")).unwrap(),
            b"one"
        );
    }
}

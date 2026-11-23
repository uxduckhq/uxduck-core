//! Camera video (ADR-0017, S38, S39): UDP listeners, RTSP cameras, the hub
//! that relays pictures to `/video` sockets, and the socket itself. The
//! daemon parses and relays; it never decodes a picture (V1).

pub mod capture;
pub mod companion;
pub mod created;
pub mod demos;
pub mod file;
pub mod folder;
pub mod hub;
pub mod listener;
pub mod look;
pub mod onboard_recording;
pub mod permission;
pub mod playback;
pub mod recorder;
pub mod reencode;
pub mod rtsp;
pub mod sender;
pub mod slice;
pub mod socket;
pub mod thumbnail;
pub mod whep;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use uxduck_contracts::dto::video::VideoSocket;

use crate::daemon::engine_task::EngineHandle;

/// How many of the files and sockets the computer lets one program hold open
/// are left alone when a UDP port added by hand asks for one more. There is
/// no fixed number of ports (maintainer, 2026-11-15) and each holds a socket
/// open, so a long list could take every one the computer allows, and then
/// UXDuck opens nothing else. Measured on 2026-11-15 with 300 ports where a
/// program may hold 256, which is what a macOS shell gives: 234 opened, one
/// was left, and the next start ended with "cannot use the data directory:
/// Too many open files". UXDuck holds about 22 open with nothing connected.
/// Its links, pages, cameras, recordings and logs each take more as they
/// come, so about three times that stays for them.
const OPEN_FILES_KEPT: usize = 64;

/// How many files and sockets this program holds open, and how many the
/// computer lets it. Nothing where the computer names no such number, as on
/// Windows, or does not say.
#[cfg(unix)]
fn open_files() -> Option<(usize, usize)> {
    let most = rustix::process::getrlimit(rustix::process::Resource::Nofile).current?;
    // Linux and macOS list them here. The listing itself holds one open.
    let open = std::fs::read_dir("/dev/fd").ok()?.count().saturating_sub(1);
    Some((open, usize::try_from(most).unwrap_or(usize::MAX)))
}

#[cfg(not(unix))]
fn open_files() -> Option<(usize, usize)> {
    None
}

/// Whether one more port added by hand may be opened. The port stays in the
/// list either way. One not opened says why, with the computer's own number
/// and how many are open now.
fn room_for_a_port(port: u16, open: usize, most: usize) -> Result<(), String> {
    if most.saturating_sub(open) > OPEN_FILES_KEPT {
        return Ok(());
    }
    Err(format!(
        "UXDuck is not listening on UDP port {port}. This computer lets a program hold {most} \
         files and sockets open at once, and UXDuck holds {open}. It leaves the last \
         {OPEN_FILES_KEPT} for its vehicles, pages and files. Remove ports you don't use, or \
         raise the computer's open-file limit"
    ))
}

/// The engine's end: listeners and RTSP cameras added by hand, and the hub.
#[derive(Debug)]
pub struct VideoHost {
    hub: Arc<hub::Hub>,
    engine: EngineHandle,
    runtime: tokio::runtime::Handle,
    /// Ports added by hand, each with its listener's stop flag.
    listeners: HashMap<u16, Arc<AtomicBool>>,
    rtsp: HashMap<String, rtsp::Worker>,
    captures: HashMap<String, tokio::task::JoinHandle<()>>,
    selections: HashMap<String, permission::Selection>,
    devices: Arc<capture::Devices>,
    files: HashMap<String, tokio::task::JoinHandle<()>>,
    demos: Arc<demos::Demos>,
    /// Looks being made, to stop one whose request was cancelled.
    looks: HashMap<String, tokio::task::AbortHandle>,
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
        if let Some((open, most)) = open_files() {
            room_for_a_port(port, open, most)?;
        }
        let socket = listener::bind(port)?;
        let stop = Arc::new(AtomicBool::new(false));
        listener::spawn(socket, self.hub.clone(), self.engine.clone(), stop.clone())?;
        self.listeners.insert(port, stop);
        tracing::info!(port, "listening for video on a port added by hand");
        Ok(())
    }

    fn unlisten(&mut self, port: u16) {
        if let Some(stop) = self.listeners.remove(&port) {
            stop.store(true, Ordering::Relaxed);
        }
    }

    fn open_rtsp(&mut self, source_id: &str, urls: &[String], epoch: u64) {
        let previous = self.rtsp.remove(source_id);
        self.rtsp.retain(|_, worker| !worker.finished());
        let task = rtsp::spawn(
            &self.runtime,
            source_id.to_owned(),
            urls.to_vec(),
            epoch,
            previous,
            self.hub.clone(),
            self.engine.clone(),
        );
        self.rtsp.insert(source_id.to_owned(), task);
    }

    fn close_rtsp(&mut self, source_id: &str) {
        if let Some(task) = self.rtsp.get(source_id) {
            task.task.abort();
        }
    }

    fn steer_rtsp(&mut self, source_id: &str, index: usize) {
        if let Some(worker) = self.rtsp.get(source_id) {
            worker.steer(index);
        }
    }

    fn route_rtsp(
        &mut self,
        source_id: &str,
        routes: &[Option<uxduck_engine::video::AddressRoute>],
    ) {
        if let Some(worker) = self.rtsp.get(source_id) {
            worker.route(routes);
        }
    }

    fn rtsp_may_return(&mut self, source_id: &str, to: &[usize]) {
        if let Some(worker) = self.rtsp.get(source_id) {
            worker.may_return(to);
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
            self.devices.clone(),
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

    fn delete_recording(&mut self, recording_id: &str, file: &str) -> Result<(), String> {
        let dir = self.hub.dir();
        match std::fs::remove_file(dir.join(file)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("Cannot delete {file}: {e}."));
            }
            _ => {}
        }
        let _ = std::fs::remove_file(recorder::index_path(dir, file));
        playback::remove_copies(&playback::cache_dir(dir), recording_id);
        Ok(())
    }

    fn recover_recording(&mut self, file: &str) -> Option<(u64, u64)> {
        recorder::recover(self.hub.dir(), file)
    }

    fn allow_playback(
        &mut self,
        token: &str,
        recording_id: &str,
        at: &uxduck_engine::runtime::VideoAt,
        still_written: bool,
    ) {
        self.hub
            .allow_playback(token, recording_id, at.clone(), still_written);
    }

    fn reveal(&mut self, file: Option<&str>) -> Result<String, String> {
        if let Some(elsewhere) = file.map(std::path::Path::new).filter(|p| p.is_absolute()) {
            if !elsewhere.exists() {
                return Err(format!(
                    "{} is not on this computer now. Plug in its disk to show it.",
                    elsewhere.display()
                ));
            }
            let folder = elsewhere.parent().unwrap_or(elsewhere);
            crate::adapters::browser::reveal(folder, Some(elsewhere))
                .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
            return Ok(folder.display().to_string());
        }
        let dir = self.hub.dir();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = file.map(|name| dir.join(name)).filter(|path| path.exists());
        crate::adapters::browser::reveal(dir, file.as_deref())
            .map_err(|e| format!("UXDuck can't open the folder: {e}"))?;
        Ok(dir.display().to_string())
    }

    fn look(&mut self, order: uxduck_engine::looks::LookOrder) -> Result<(), String> {
        let program = capture::program()?;
        let _entered = self.runtime.enter();
        self.looks.retain(|_, look| !look.is_finished());
        let id = order.id.clone();
        let look = look::start(self.hub.clone(), self.engine.clone(), program, order);
        self.looks.insert(id, look);
        Ok(())
    }

    fn stop_look(&mut self, id: &str) {
        if let Some(look) = self.looks.remove(id) {
            look.abort();
        }
    }

    fn save_photo(&mut self, stem: &str, jpeg: &[u8]) -> Result<String, String> {
        save_photo(self.hub.dir(), stem, jpeg)
    }

    fn allow_photo(&mut self, token: &str, photo_id: &str, file_name: &str) {
        self.hub.allow_photo(token, photo_id, file_name);
    }

    fn allow_thumbnails(&mut self, token: &str, photos: &[(String, String)]) {
        self.hub.allow_thumbnails(token, photos);
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
    // Cameras that send no IDR are re-encoded while a browser waits on one.
    let (reencode, wanted) = tokio::sync::mpsc::unbounded_channel();
    hub.reencode_with(reencode);
    tokio::spawn(reencode::manage(hub.clone(), engine.clone(), wanted));
    // Playback copies unused for two weeks go (S151).
    let cache = playback::cache_dir(hub.dir());
    tokio::task::spawn_blocking(move || {
        let freed = playback::clean(&cache, playback::UNUSED_FOR);
        if freed > 0 {
            tracing::info!(freed, "old playback copies removed");
        }
    });
    // Tokens' copies of recordings still being written (FB-243) go once
    // they are not needed, from the start too: no token outlives the
    // daemon, and a copy given since start belongs to a token it knows.
    let (cache, pins) = (playback::cache_dir(hub.dir()), hub.clone());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(playback::PIN_SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let (cache, pins) = (cache.clone(), pins.clone());
            let _ = tokio::task::spawn_blocking(move || {
                playback::sweep_pins(
                    &cache,
                    |token| pins.playback_asked(token),
                    playback::PIN_IDLE,
                )
            })
            .await;
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
        looks: HashMap::new(),
    };
    let _ = engine.run(move |e| e.set_video(Box::new(host))).await;
    // Files deleted in Finder leave the list (S151).
    folder::watch(
        hub.dir().to_owned(),
        playback::cache_dir(hub.dir()),
        engine.clone(),
    );
    if let Some(port) = port {
        let started = listener::bind(port).and_then(|socket| {
            let bound = socket.local_addr().map_or(port, |a| a.port());
            // The main port lives as long as the daemon.
            let stop = Arc::new(AtomicBool::new(false));
            listener::spawn(socket, hub.clone(), engine.clone(), stop)?;
            Ok(bound)
        });
        let (listening, error) = match started {
            Ok(bound) => {
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
            task.task.abort();
        }
        for stop in self.listeners.values() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    /// A port added by hand is opened while the computer has room for it
    /// and for UXDuck's own work, and one that is not says the computer's
    /// number and how many are open.
    #[test]
    fn a_port_is_not_opened_into_the_last_files_the_computer_allows() {
        assert_eq!(super::room_for_a_port(5700, 22, 256), Ok(()));
        assert_eq!(super::room_for_a_port(5700, 191, 256), Ok(()));
        assert_eq!(
            super::room_for_a_port(5870, 192, 256).unwrap_err(),
            "UXDuck is not listening on UDP port 5870. This computer lets a program hold 256 \
             files and sockets open at once, and UXDuck holds 192. It leaves the last 64 for \
             its vehicles, pages and files. Remove ports you don't use, or raise the \
             computer's open-file limit"
        );
        assert!(super::room_for_a_port(5700, 300, 256).is_err());
        assert_eq!(super::room_for_a_port(5700, 4_000, 524_288), Ok(()));
    }

    /// The count is of this program's own files: at least its three
    /// standard ones, and never more than the computer allows.
    #[cfg(unix)]
    #[test]
    fn the_open_files_are_counted_within_what_the_computer_allows() {
        let (open, most) = super::open_files().expect("a limit and a count");
        assert!(open >= 3, "{open}");
        assert!(open <= most, "{open} of {most}");
        let held = std::fs::File::open("/dev/null").expect("a file");
        let (more, _) = super::open_files().expect("a count");
        // Other tests open and close files at the same time, so only that it is counted at all.
        assert!(more >= 4, "{more}");
        drop(held);
    }

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

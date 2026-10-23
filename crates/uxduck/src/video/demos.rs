//! Lazy optional assets. Downloads stream to a temporary file, verify length and
//! SHA-256, then atomically enter the cache. No media network request at startup.
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uxduck_contracts::dto::video::{
    DemoAction, DemoDownload, DemoVideo, VideoDemosParams, VideoDemosResult,
};
use uxduck_core::domain::video::VideoFileInput;

const MAX_CATALOGUE: usize = 512 * 1024;
const MAX_CLIP: u64 = 8 * 1024 * 1024 * 1024;
fn default_base() -> &'static str {
    if env!("CARGO_PKG_VERSION").contains("-beta.") {
        "https://app-staging.uxduck.com/demo-video"
    } else {
        "https://app.uxduck.com/demo-video"
    }
}

#[derive(Debug, Default)]
struct State {
    result: VideoDemosResult,
    loaded: bool,
    refreshed: Option<Instant>,
    jobs: BTreeMap<String, tokio::task::JoinHandle<()>>,
}
#[derive(Debug)]
pub struct Demos {
    dir: PathBuf,
    state: Mutex<State>,
    base: String,
}
impl Demos {
    pub fn new(dir: PathBuf) -> Self {
        let base = std::env::var("UXDUCK_DEMO_VIDEO_URL").unwrap_or_else(|_| default_base().into());
        Self {
            dir,
            state: Mutex::new(State::default()),
            base: base.trim_end_matches('/').to_owned(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn path(&self, clip: &DemoVideo) -> PathBuf {
        self.dir.join(format!("{}.mp4", clip.sha256))
    }
    fn ready(&self, clip: &DemoVideo) -> bool {
        self.path(clip)
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == clip.bytes)
    }
    pub fn file(&self, id: &str) -> Result<VideoFileInput, String> {
        let mut state = self.lock();
        self.load_saved(&mut state);
        let clip = state
            .result
            .clips
            .iter()
            .find(|c| c.id == id)
            .ok_or("This demo is no longer in the catalogue. Refresh it.")?;
        if !self.ready(clip) {
            return Err("Download this demo before playing it.".into());
        }
        super::file::check(&self.path(clip).to_string_lossy())?;
        Ok(VideoFileInput {
            path: self.path(clip).to_string_lossy().into_owned(),
            name: clip.name.clone(),
            repeat: true,
            demo_id: Some(id.to_owned()),
        })
    }
    fn load_saved(&self, state: &mut State) {
        if !state.loaded {
            state.loaded = true;
            // A killed process cannot run NamedTempFile's cleanup. Only our
            // own incomplete files in this cache are eligible on restart.
            if let Ok(entries) = std::fs::read_dir(&self.dir) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(".tmp")
                        && let Err(why) = std::fs::remove_file(entry.path())
                    {
                        state.result.error = Some(format!(
                            "Could not remove an interrupted demo download: {why}"
                        ));
                    }
                }
            }
            let cached = self.dir.join("catalogue.json");
            if cached.is_file() {
                match cached
                    .metadata()
                    .map_err(|e| e.to_string())
                    .and_then(|m| {
                        if m.len() > MAX_CATALOGUE as u64 {
                            Err("Saved demo catalogue is too large".into())
                        } else {
                            std::fs::read(&cached).map_err(|e| e.to_string())
                        }
                    })
                    .and_then(|b| catalogue(&b))
                {
                    Ok(clips) => state.result.clips = clips,
                    Err(e) => {
                        state.result.error = Some(format!(
                            "The saved demo catalogue could not be read: {e}. Refresh to repair it."
                        ));
                    }
                }
            }
        }
    }

    pub fn request(
        self: &Arc<Self>,
        runtime: &tokio::runtime::Handle,
        params: &VideoDemosParams,
    ) -> Result<VideoDemosResult, String> {
        let mut state = self.lock();
        self.load_saved(&mut state);
        if (params.action == DemoAction::Refresh
            || (params.action == DemoAction::List
                && state
                    .refreshed
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(300))))
            && !state.result.refreshing
        {
            state.result.refreshing = true;
            state.refreshed = Some(Instant::now());
            let this = self.clone();
            runtime.spawn(async move {
                    let result = this.refresh().await;
                    let mut state = this.lock(); state.result.refreshing = false;
                    match result {
                        Ok(clips) => { state.result.clips = clips; state.result.error = None; }
                        Err(why) => state.result.error = Some(format!("Could not refresh demo footage: {why}. Downloaded videos remain available offline.")),
                    }
                });
        }
        if !matches!(params.action, DemoAction::List | DemoAction::Refresh) {
            let id = params.id.as_deref().ok_or("Choose a demo video.")?;
            let clip = state
                .result
                .clips
                .iter()
                .find(|c| c.id == id)
                .cloned()
                .ok_or("This demo is not in the catalogue.")?;
            match params.action {
                DemoAction::Download => {
                    if state
                        .jobs
                        .get(id)
                        .is_none_or(tokio::task::JoinHandle::is_finished)
                        && !self.ready(&clip)
                    {
                        if state.jobs.values().filter(|j| !j.is_finished()).count() >= 2 {
                            return Err("Two videos are downloading. Wait or cancel one before starting another.".into());
                        }
                        state.result.downloads.retain(|d| d.id != id);
                        state.result.downloads.push(DemoDownload {
                            id: id.into(),
                            downloading: true,
                            ..Default::default()
                        });
                        let this = self.clone();
                        let owned = id.to_owned();
                        let task = runtime.spawn(async move {
                            let result = this.download(&clip).await;
                            let mut state = this.lock();
                            if let Some(d) =
                                state.result.downloads.iter_mut().find(|d| d.id == owned)
                            {
                                d.downloading = false;
                                d.ready = result.is_ok();
                                d.error = result.err();
                            }
                        });
                        state.jobs.insert(id.into(), task);
                    }
                }
                DemoAction::Cancel | DemoAction::Remove => {
                    if let Some(task) = state.jobs.remove(id) {
                        task.abort();
                    }
                    state.result.downloads.retain(|d| d.id != id);
                    if params.action == DemoAction::Remove {
                        match std::fs::remove_file(self.path(&clip)) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(format!("Could not remove this download: {e}")),
                        }
                    }
                }
                _ => {}
            }
        }
        state.jobs.retain(|_, task| !task.is_finished());
        let mut result = state.result.clone();
        for clip in &result.clips {
            let ready = self.ready(clip);
            if let Some(d) = result.downloads.iter_mut().find(|d| d.id == clip.id) {
                d.ready = ready;
            } else {
                result.downloads.push(DemoDownload {
                    id: clip.id.clone(),
                    ready,
                    ..Default::default()
                });
            }
        }
        Ok(result)
    }
    fn client(&self) -> Result<reqwest::Client, String> {
        let url = reqwest::Url::parse(&self.base).map_err(|e| e.to_string())?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            return Err("The demo catalogue needs an HTTPS address.".into());
        }
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())
    }
    async fn refresh(&self) -> Result<Vec<DemoVideo>, String> {
        let mut response = self
            .client()?
            .get(format!("{}/catalogue.json", self.base))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > MAX_CATALOGUE {
                return Err("Demo catalogue is too large.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut clips = catalogue(&bytes)?;
        {
            // Keep a downloaded or downloading revision addressable until the
            // user removes it. Catalogue updates cannot strand an offline file
            // or change what an in-progress "Download and use" will play.
            let state = self.lock();
            for previous in &state.result.clips {
                let keep = self.ready(previous)
                    || state
                        .jobs
                        .get(&previous.id)
                        .is_some_and(|job| !job.is_finished());
                if keep {
                    if let Some(next) = clips.iter_mut().find(|c| c.id == previous.id) {
                        *next = previous.clone();
                    } else {
                        clips.push(previous.clone());
                    }
                }
            }
        }
        let bytes = serde_json::to_vec(&clips).map_err(|e| e.to_string())?;
        catalogue(&bytes)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.persist(self.dir.join("catalogue.json"))
            .map_err(|e| e.to_string())?;
        Ok(clips)
    }
    async fn download(&self, clip: &DemoVideo) -> Result<(), String> {
        let response = self
            .client()?
            .get(format!("{}/{}.mp4", self.base, clip.sha256))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let file = tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| e.to_string())?;
        // NamedTempFile removes partial content when the task is cancelled.
        let mut output = tokio::fs::File::from_std(file.reopen().map_err(|e| e.to_string())?);
        let mut response = response;
        let mut digest = Sha256::new();
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            received += chunk.len() as u64;
            if received > clip.bytes {
                return Err("Demo download exceeded its advertised size.".into());
            }
            digest.update(&chunk);
            tokio::io::AsyncWriteExt::write_all(&mut output, &chunk)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(d) = self
                .lock()
                .result
                .downloads
                .iter_mut()
                .find(|d| d.id == clip.id)
            {
                d.received_bytes = received;
            }
        }
        if received != clip.bytes
            || format!("{:x}", uxduck_core::encoding::Hex(&digest.finalize())) != clip.sha256
        {
            return Err("Demo download failed verification. Download it again.".into());
        }
        output.sync_all().await.map_err(|e| e.to_string())?;
        drop(output);
        file.persist(self.path(clip)).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn catalogue(bytes: &[u8]) -> Result<Vec<DemoVideo>, String> {
    if bytes.len() > MAX_CATALOGUE {
        return Err("Demo catalogue is too large.".into());
    }
    let clips: Vec<DemoVideo> =
        serde_json::from_slice(bytes).map_err(|e| format!("Invalid demo catalogue: {e}"))?;
    if clips.len() > 100 {
        return Err("Too many demo videos.".into());
    }
    let mut ids = BTreeSet::new();
    let mut hashes = BTreeSet::new();
    for c in &clips {
        if c.id.is_empty()
            || c.id.len() > 80
            || !c.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !ids.insert(&c.id)
            || c.sha256.len() != 64
            || !c
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || !hashes.insert(&c.sha256)
            || c.bytes == 0
            || c.bytes > MAX_CLIP
            || c.duration_seconds == 0
            || c.name.is_empty()
            || c.name.len() > 200
            || c.credit.is_empty()
            || c.license.is_empty()
            || !c.source_url.starts_with("https://")
            || c.thumbnail != format!("{}.jpg", c.sha256)
        {
            return Err(
                "A demo has invalid identity, size, duration, credits or thumbnail.".into(),
            );
        }
    }
    Ok(clips)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogue_rejects_traversal_duplicates_and_oversized_assets() {
        let clip = DemoVideo {
            id: "thermal".into(),
            name: "Thermal".into(),
            description: "A boat".into(),
            duration_seconds: 329,
            bytes: 500,
            sha256: "a".repeat(64),
            credit: "US Coast Guard".into(),
            source_url: "https://www.dvidshub.net/video/758413/vsl-hot-ticket-coast-guard-sar"
                .into(),
            license: "Public domain".into(),
            thumbnail: format!("{}.jpg", "a".repeat(64)),
        };
        assert!(catalogue(&serde_json::to_vec(&vec![clip.clone()]).unwrap()).is_ok());
        assert!(
            catalogue(&serde_json::to_vec(&vec![clip.clone(), clip.clone()]).unwrap()).is_err()
        );
        for field in ["id", "sha256", "thumbnail"] {
            let mut bad = serde_json::to_value(vec![clip.clone()]).unwrap();
            bad[0][field] = "../outside".into();
            assert!(catalogue(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        let mut bad = clip;
        bad.bytes = MAX_CLIP + 1;
        assert!(catalogue(&serde_json::to_vec(&vec![bad]).unwrap()).is_err());
    }

    #[test]
    fn choosing_a_saved_demo_loads_the_catalogue_without_listing_first() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let clip = DemoVideo {
            id: "flight".into(),
            name: "Flight".into(),
            description: "Forward flight".into(),
            duration_seconds: 30,
            bytes: 500,
            sha256: hash.clone(),
            credit: "US Air Force".into(),
            source_url: "https://www.dvidshub.net/".into(),
            license: "Public domain".into(),
            thumbnail: format!("{hash}.jpg"),
        };
        std::fs::write(
            dir.path().join("catalogue.json"),
            serde_json::to_vec(&vec![clip]).unwrap(),
        )
        .unwrap();
        let demos = Demos::new(dir.path().to_owned());
        // The entry resolves before any list request; only its missing media
        // prevents playback, without needing a packaged helper in this test.
        assert_eq!(
            demos.file("flight").unwrap_err(),
            "Download this demo before playing it."
        );
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use axum::{Router, extract::State, response::IntoResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone)]
    struct Fixture {
        bytes: Vec<u8>,
        catalogue: Vec<u8>,
        videos: Arc<AtomicUsize>,
    }
    async fn endpoint(State(f): State<Fixture>, uri: axum::http::Uri) -> axum::response::Response {
        if uri.path().ends_with("catalogue.json") {
            f.catalogue.into_response()
        } else {
            f.videos.fetch_add(1, Ordering::SeqCst);
            f.bytes.into_response()
        }
    }
    #[tokio::test]
    async fn optional_download_verifies_before_offline_reuse_and_removal() {
        let bytes = b"fixture video bytes".to_vec();
        let sha = format!("{:x}", uxduck_core::encoding::Hex(&Sha256::digest(&bytes)));
        let clip = DemoVideo {
            id: "thermal".into(),
            name: "Thermal".into(),
            description: "Boat".into(),
            duration_seconds: 329,
            bytes: bytes.len() as u64,
            sha256: sha.clone(),
            credit: "US Coast Guard".into(),
            source_url: "https://www.dvidshub.net/".into(),
            license: "Public domain".into(),
            thumbnail: format!("{sha}.jpg"),
        };
        let videos = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(endpoint).with_state(Fixture {
            bytes: bytes.clone(),
            catalogue: serde_json::to_vec(&vec![clip.clone()]).unwrap(),
            videos: videos.clone(),
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut demos = Demos::new(dir.path().to_owned());
        demos.base = base.clone();
        let demos = Arc::new(demos);
        let rt = tokio::runtime::Handle::current();
        demos.request(&rt, &VideoDemosParams::default()).unwrap();
        for _ in 0..200 {
            if !demos.lock().result.refreshing {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(demos.lock().result.clips.len(), 1);
        assert_eq!(
            videos.load(Ordering::SeqCst),
            0,
            "listing never downloads media"
        );
        demos
            .request(
                &rt,
                &VideoDemosParams {
                    action: DemoAction::Download,
                    id: Some("thermal".into()),
                },
            )
            .unwrap();
        for _ in 0..200 {
            if demos.ready(&clip) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read(demos.path(&clip)).unwrap(), bytes);
        assert_eq!(videos.load(Ordering::SeqCst), 1);
        // The server sends the original bytes, so this advertised hash must fail.
        let mut wrong = clip.clone();
        wrong.sha256 = "b".repeat(64);
        assert!(
            demos
                .download(&wrong)
                .await
                .unwrap_err()
                .contains("verification")
        );
        assert!(!demos.path(&wrong).exists());
        server.abort();
        let mut reopened = Demos::new(dir.path().to_owned());
        reopened.base = base;
        let reopened = Arc::new(reopened);
        let result = reopened.request(&rt, &VideoDemosParams::default()).unwrap();
        assert!(
            result.downloads[0].ready,
            "download remains available after restart offline"
        );
        reopened
            .request(
                &rt,
                &VideoDemosParams {
                    action: DemoAction::Remove,
                    id: Some("thermal".into()),
                },
            )
            .unwrap();
        assert!(!reopened.path(&clip).exists());
        // Wrong bytes must never become a playable final cache file.
    }
}

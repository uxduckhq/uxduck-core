//! The latest interface, also offline (S109, ADR-0025 D3 and D4; the
//! design is `docs/plan/interface-updates.md`).
//!
//! Online, UXDuck follows the interface app.uxduck.com publishes: it
//! checks the signed manifest 10 s after start, every hour and when a page
//! asks (`interface/check`), downloads the newest entry that talks this
//! UXDuck's protocol, checks the archive and every file in it, and keeps it
//! in `<data dir>/interface/`. Offline it serves the last one it followed;
//! never online, the copy built into `uxduck`. Not a PWA: the browser and
//! the desktop app just load `http://127.0.0.1:45000`.
//!
//! An open page is never swapped under the person: the daemon pushes
//! `interface/updated` and the page offers a reload. A version whose page
//! reports a failed start, or loads and never says it is ready within
//! 60 s, is marked bad on this computer, the previous one is served again
//! and the failure is reported (ADR-0016).

pub mod built_in;
pub mod choose;
pub mod store;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use tower_http::services::ServeFile;
use uxduck_contracts::dto::interface::{
    INTERFACE_MANIFEST_SCHEMA, InterfaceEntry, InterfaceManifest, InterfaceStatus,
    MAX_ARCHIVE_BYTES,
};
use uxduck_contracts::version::PROTOCOL_VERSION;

use crate::daemon::engine_task::EngineHandle;
use choose::{BuiltIn, Known, Source};
use store::{Bad, Followed, Kept, Store};

/// Where the production manifest is published.
pub const DEFAULT_URL: &str = "https://app.uxduck.com/interface/manifest.json";

/// How long a page has, after loading `index.html`, to say it is ready.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// What follows the interface, and how often.
#[derive(Clone, Debug)]
pub struct Config {
    /// The manifest; none switches fetching off (`UXDUCK_INTERFACE_URL=none`).
    pub url: Option<String>,
    /// The release key; none in builds without it, which never fetch.
    pub key: Option<String>,
    /// `--dev`: the Vite origin that serves the interface; nothing is
    /// fetched or kept.
    pub dev: Option<String>,
    pub first_check: Duration,
    pub every: Duration,
    pub ready_timeout: Duration,
}

impl Config {
    /// The production manifest and the key built into this binary. A
    /// development build also takes `UXDUCK_INTERFACE_KEY` and shorter
    /// times (`UXDUCK_INTERFACE_FIRST_CHECK_MS`, `UXDUCK_INTERFACE_READY_MS`),
    /// for tests.
    #[must_use]
    pub fn from_env(dev: Option<String>) -> Self {
        let url = match std::env::var("UXDUCK_INTERFACE_URL") {
            Ok(url) if url == "none" => None,
            Ok(url) => Some(url),
            Err(_) => Some(DEFAULT_URL.to_owned()),
        };
        let debug_env = |name: &str| std::env::var(name).ok().filter(|_| cfg!(debug_assertions));
        let ms = |name: &str| {
            debug_env(name)
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
        };
        Self {
            url,
            key: debug_env("UXDUCK_INTERFACE_KEY")
                .or_else(|| crate::update::PUBLIC_KEY.map(str::to_owned)),
            dev,
            first_check: ms("UXDUCK_INTERFACE_FIRST_CHECK_MS").unwrap_or(Duration::from_secs(10)),
            every: Duration::from_hours(1),
            ready_timeout: ms("UXDUCK_INTERFACE_READY_MS").unwrap_or(READY_TIMEOUT),
        }
    }
}

#[derive(Debug)]
struct Serving {
    source: Source,
    version: String,
    published_at: Option<String>,
    /// The folder served from, for a kept interface.
    dir: Option<PathBuf>,
    /// Every other kept folder, newest first: where an open page on an
    /// earlier version finds a chunk it has not loaded yet.
    others: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct Run {
    /// The manifest accepted this run, and the entry it chose.
    manifest: Option<InterfaceManifest>,
    followed: Option<InterfaceEntry>,
    etag: Option<String>,
    update_uxduck: bool,
    checked_at_ms: Option<u64>,
    problem: Option<String>,
    /// Versions a page said were ready this run.
    proven: BTreeSet<String>,
    /// Pages that loaded a version not yet proven, and when.
    pending: Vec<(String, Instant)>,
}

#[derive(Debug)]
struct Inner {
    serving: Serving,
    run: Run,
}

/// The interface store and what is served now.
#[derive(Debug)]
pub struct Interfaces {
    config: Config,
    /// None in `--dev`.
    store: Option<Store>,
    files: built_in::Files,
    built_in: Option<BuiltIn>,
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
    engine: OnceLock<EngineHandle>,
}

#[derive(Debug, thiserror::Error)]
enum CheckError {
    #[error("no connection")]
    Offline,
    #[error("{0}")]
    Answer(String),
    #[error("the answer wasn't signed by UXDuck")]
    Signature,
    #[error("the answer couldn't be read: {0}")]
    Invalid(String),
    #[error("the answer was older than one already followed")]
    Replayed,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn rfc3339_now() -> String {
    crate::analytics::rfc3339(now_ms() * 1000)
}

impl Interfaces {
    /// Opens the store in `<data dir>/interface/` (none in `--dev`) and
    /// chooses what to serve from what is on disk.
    ///
    /// # Errors
    /// When the store's folder cannot be made or read.
    pub fn open(data_dir: &Path, ui_dir: Option<PathBuf>, config: Config) -> std::io::Result<Self> {
        let store = if config.dev.is_some() {
            None
        } else {
            Some(Store::open(data_dir.join("interface"))?)
        };
        let files = built_in::Files::new(ui_dir);
        let built_in = files.info();
        let interfaces = Self {
            config,
            store,
            files,
            built_in,
            inner: Mutex::new(Inner {
                serving: Serving {
                    source: Source::Nothing,
                    version: String::new(),
                    published_at: None,
                    dir: None,
                    others: Vec::new(),
                },
                run: Run::default(),
            }),
            wake: tokio::sync::Notify::new(),
            engine: OnceLock::new(),
        };
        interfaces.apply(false);
        Ok(interfaces)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this UXDuck follows the published interface now.
    fn updates_on(&self) -> bool {
        self.config.url.is_some()
            && self.config.key.is_some()
            && self
                .store
                .as_ref()
                .is_some_and(|store| store.settings().updates != "off")
    }

    /// What `interface/status` answers.
    #[must_use]
    pub fn status(&self) -> InterfaceStatus {
        let inner = self.lock();
        InterfaceStatus {
            version: inner.serving.version.clone(),
            published_at: inner.serving.published_at.clone(),
            built_in: inner.serving.source == Source::BuiltIn,
            updates: self.updates_on(),
            update_uxduck: inner.run.update_uxduck,
            checked_at_ms: inner.run.checked_at_ms,
            problem: inner.run.problem.clone(),
        }
    }

    fn push(&self) {
        if let Some(engine) = self.engine.get() {
            let status = self.status();
            engine.post(move |e| e.interface_status(status));
        }
    }

    /// Chooses what to serve from the store and this run; after an
    /// install, deletes all but the served interface and the two before it.
    fn apply(&self, installed: bool) {
        let Some(store) = &self.store else {
            let mut inner = self.lock();
            inner.serving = self.built_in_serving();
            return;
        };
        let kept = store.kept();
        let bad = store.bad();
        let current = store.current();
        let mut inner = self.lock();
        let followed = inner.run.followed.clone();
        let manifest_at = inner
            .run
            .manifest
            .as_ref()
            .map(|m| m.published_at.clone())
            .unwrap_or_default();
        let source = choose::choose(&Known {
            protocol: PROTOCOL_VERSION,
            followed: followed.as_ref().map(|entry| (entry, manifest_at.as_str())),
            current: current.as_deref(),
            kept: &kept,
            built_in: self.built_in.as_ref(),
            bad: &bad,
        });
        let serving = match &source {
            Source::Kept(version) => {
                let chosen = kept.iter().find(|k| &k.entry.version == version);
                Serving {
                    source: source.clone(),
                    version: version.clone(),
                    published_at: chosen.map(|k| k.entry.published_at.clone()),
                    dir: chosen.map(|k| k.dir.clone()),
                    others: kept
                        .iter()
                        .filter(|k| &k.entry.version != version)
                        .map(|k| k.dir.clone())
                        .collect(),
                }
            }
            _ => Serving {
                others: kept.iter().map(|k| k.dir.clone()).collect(),
                ..self.built_in_serving()
            },
        };
        if !serving.version.is_empty()
            && current.as_deref() != Some(serving.version.as_str())
            && let Err(error) = store.set_current(&serving.version)
        {
            tracing::warn!(%error, "could not write the current interface");
        }
        if installed {
            let mut keep: BTreeSet<String> = BTreeSet::new();
            if let Source::Kept(version) = &source {
                keep.insert(version.clone());
            }
            for k in &kept {
                if keep.len() >= 3 {
                    break;
                }
                keep.insert(k.entry.version.clone());
            }
            store.keep_only(&keep);
            inner.serving = Serving {
                others: serving
                    .others
                    .into_iter()
                    .filter(|dir| dir.is_dir())
                    .collect(),
                ..serving
            };
        } else {
            inner.serving = serving;
        }
    }

    fn built_in_serving(&self) -> Serving {
        match &self.built_in {
            Some(built_in) => Serving {
                source: Source::BuiltIn,
                version: built_in.version.clone(),
                published_at: (!built_in.published_at.is_empty())
                    .then(|| built_in.published_at.clone()),
                dir: None,
                others: Vec::new(),
            },
            None => Serving {
                source: Source::Nothing,
                version: String::new(),
                published_at: None,
                dir: None,
                others: Vec::new(),
            },
        }
    }

    /// Hands the engine its port and status, then checks 10 s after start
    /// and every hour, and watches for pages that never say they are ready.
    pub fn start(self: &Arc<Self>, engine: &EngineHandle) {
        let _ = self.engine.set(engine.clone());
        let port = Port(self.clone());
        engine.post(move |e| e.set_interface(Box::new(port)));
        self.push();
        if self.config.dev.is_some() {
            return;
        }
        if self.config.key.is_none() {
            tracing::info!("this build has no release key: it serves the interface built into it");
        }
        let checker = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(checker.config.first_check).await;
            loop {
                checker.check().await;
                tokio::select! {
                    () = tokio::time::sleep(checker.config.every) => {}
                    () = checker.wake.notified() => {}
                }
            }
        });
        let watcher = self.clone();
        tokio::spawn(async move {
            let step = (watcher.config.ready_timeout / 12).max(Duration::from_millis(100));
            loop {
                tokio::time::sleep(step).await;
                crate::wakes::count("interface: ready watch");
                watcher.overdue();
            }
        });
    }

    /// Checks the manifest and installs what it names, then chooses again.
    pub async fn check(&self) {
        if !self.updates_on() {
            // Pinned by `interface.updates off`, or no key or URL.
            self.push();
            return;
        }
        let outcome = self.check_inner().await;
        {
            let mut inner = self.lock();
            inner.run.checked_at_ms = Some(now_ms());
            inner.run.problem = match &outcome {
                Ok(()) => None,
                Err(problem) => Some(problem.clone()),
            };
        }
        if let Err(problem) = &outcome {
            tracing::info!(%problem, "interface check");
        }
        self.push();
    }

    async fn check_inner(&self) -> Result<(), String> {
        let (Some(store), Some(url), Some(key)) = (&self.store, &self.config.url, &self.config.key)
        else {
            return Ok(());
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_mins(10))
            .user_agent(concat!("uxduck/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let manifest = self
            .fetch_manifest(&client, url, key, store)
            .await
            .map_err(|e| format!("Couldn't check for a new interface: {e}"))?;
        let bad = store.bad();
        let pick = choose::pick(&manifest, PROTOCOL_VERSION, &bad);
        if let Some(entry) = &pick.entry {
            let have = self
                .built_in
                .as_ref()
                .is_some_and(|b| b.version == entry.version)
                || store
                    .kept()
                    .iter()
                    .any(|k| k.entry.version == entry.version);
            if !have {
                let installed = self.download_and_install(&client, store, entry).await;
                if let Err(error) = installed {
                    // Keep serving what was served; follow it next time.
                    let mut inner = self.lock();
                    inner.run.manifest = Some(manifest.clone());
                    inner.run.update_uxduck = pick.update_uxduck;
                    drop(inner);
                    return Err(format!("Couldn't install a new interface: {error}"));
                }
            }
        }
        if let Err(error) = store.set_followed(&Followed {
            published_at: manifest.published_at.clone(),
            version: pick.entry.as_ref().map(|e| e.version.clone()),
        }) {
            tracing::warn!(%error, "could not remember the interface manifest");
        }
        let before = self.lock().serving.version.clone();
        {
            let mut inner = self.lock();
            inner.run.manifest = Some(manifest);
            inner.run.followed = pick.entry;
            inner.run.update_uxduck = pick.update_uxduck;
        }
        self.apply(true);
        let after = self.lock().serving.version.clone();
        if before != after {
            tracing::info!(from = %before, to = %after, "a new interface is current");
        }
        Ok(())
    }

    async fn fetch_manifest(
        &self,
        client: &reqwest::Client,
        url: &str,
        key: &str,
        store: &Store,
    ) -> Result<InterfaceManifest, CheckError> {
        let (etag, known) = {
            let inner = self.lock();
            (inner.run.etag.clone(), inner.run.manifest.clone())
        };
        let mut request = client.get(url);
        if let (Some(etag), Some(_)) = (&etag, &known) {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|e| fetch_error(&e))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(known) = known
        {
            return Ok(known);
        }
        if !response.status().is_success() {
            return Err(CheckError::Answer(format!(
                "the server answered {}",
                response.status()
            )));
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes().await.map_err(|e| fetch_error(&e))?;
        let signature = client
            .get(format!("{url}.minisig"))
            .send()
            .await
            .map_err(|e| fetch_error(&e))?;
        if !signature.status().is_success() {
            return Err(CheckError::Signature);
        }
        let signature = signature.text().await.map_err(|e| fetch_error(&e))?;
        let manifest = verify(&bytes, &signature, key)?;
        let followed = store.followed().map(|f| f.published_at).unwrap_or_default();
        // RFC 3339 in UTC compares as text.
        if manifest.published_at < followed {
            return Err(CheckError::Replayed);
        }
        self.lock().run.etag = etag;
        Ok(manifest)
    }

    async fn download_and_install(
        &self,
        client: &reqwest::Client,
        store: &Store,
        entry: &InterfaceEntry,
    ) -> Result<Kept, String> {
        if !store::valid_version(&entry.version) {
            return Err(format!("the version {:?} is not valid", entry.version));
        }
        if entry.archive.size > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "it is larger than UXDuck downloads ({} bytes)",
                entry.archive.size
            ));
        }
        let path = store.download_path(&entry.version);
        let downloaded = download(client, &entry.archive.url, &path, entry.archive.size).await;
        if let Err(error) = downloaded {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let store = store.clone();
        let entry = entry.clone();
        tokio::task::spawn_blocking(move || store.install(&entry, &path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// `POST /interface/ready`: a page of `version` rendered its first
    /// screen.
    pub fn ready(&self, version: &str) {
        let mut inner = self.lock();
        inner.run.proven.insert(version.to_owned());
        inner.run.pending.retain(|(v, _)| v != version);
    }

    /// A page loaded `index.html` of the interface served now.
    fn loaded(&self, version: &str) {
        let mut inner = self.lock();
        if !inner.run.proven.contains(version) {
            inner.run.pending.push((version.to_owned(), Instant::now()));
        }
    }

    fn overdue(&self) {
        let timeout = self.config.ready_timeout;
        let late: Vec<String> = {
            let mut inner = self.lock();
            let late: BTreeSet<String> = inner
                .run
                .pending
                .iter()
                .filter(|(_, at)| at.elapsed() >= timeout)
                .map(|(v, _)| v.clone())
                .collect();
            inner.run.pending.retain(|(v, _)| !late.contains(v));
            late.into_iter().collect()
        };
        for version in late {
            self.failed(
                &version,
                &format!(
                    "a page loaded it and did not start within {} s",
                    timeout.as_secs()
                ),
            );
        }
    }

    /// `POST /interface/boot-failed`, or a page that never said it was
    /// ready: marks a downloaded version bad on this computer, goes back to
    /// the previous good one and reports it (ADR-0016). The built-in copy
    /// cannot be marked bad; its failure is only reported.
    pub fn failed(&self, version: &str, reason: &str) {
        let Some(store) = &self.store else {
            return;
        };
        if self.lock().run.proven.contains(version) || store.bad().contains_key(version) {
            // Errors after ready never mark a version bad; one marked
            // already is not reported again.
            return;
        }
        let downloaded = store.kept().iter().any(|k| k.entry.version == version)
            && self.built_in.as_ref().is_none_or(|b| b.version != version);
        if !downloaded {
            tracing::error!(code = "interface-built-in-failed", %version, %reason, "the built-in interface did not start");
            return;
        }
        let manifest_published_at = {
            let inner = self.lock();
            inner
                .run
                .manifest
                .as_ref()
                .map(|m| m.published_at.clone())
                .or_else(|| store.followed().map(|f| f.published_at))
                .unwrap_or_default()
        };
        if let Err(error) = store.mark_bad(
            version,
            Bad {
                reason: reason.to_owned(),
                at: rfc3339_now(),
                manifest_published_at,
            },
        ) {
            tracing::warn!(%error, "could not mark an interface bad");
        }
        tracing::error!(code = "interface-bad", %version, %reason, "an interface did not start; going back to the previous one");
        {
            let mut inner = self.lock();
            if inner
                .run
                .followed
                .as_ref()
                .is_some_and(|e| e.version == version)
            {
                inner.run.followed = None;
            }
            inner.run.pending.retain(|(v, _)| v != version);
        }
        self.apply(false);
        self.push();
    }

    /// A file of the interface served now, or for content-hashed files of
    /// any kept one or the built-in copy.
    async fn file(&self, path: &str, request: Request) -> Option<Response> {
        let (source, dir, others, version) = {
            let inner = self.lock();
            (
                inner.serving.source.clone(),
                inner.serving.dir.clone(),
                inner.serving.others.clone(),
                inner.serving.version.clone(),
            )
        };
        let hashed = path.starts_with("assets/");
        // A file whose name does not change with its content is checked
        // again by the version served, not its time: after a step back to
        // an older interface, its older files must not look unchanged.
        let freshness = if hashed {
            Freshness::Immutable
        } else {
            Freshness::Version(format!("\"{version}\""))
        };
        let mut places: Vec<Option<PathBuf>> = Vec::new();
        match source {
            Source::Kept(_) => places.push(dir),
            Source::BuiltIn => places.push(None),
            Source::Nothing => {}
        }
        if hashed {
            places.extend(others.into_iter().map(Some));
            places.push(None);
        }
        for place in places {
            let found = match place {
                Some(dir) => {
                    let file = dir.join(path);
                    file.is_file().then_some(built_in::Found::Disk(file))
                }
                None => self.files.find(path),
            };
            if let Some(found) = found {
                return Some(respond(found, request, &freshness).await);
            }
        }
        None
    }
}

fn fetch_error(error: &reqwest::Error) -> CheckError {
    if error.is_connect() || error.is_timeout() {
        CheckError::Offline
    } else {
        CheckError::Answer(error.to_string())
    }
}

/// Verifies and reads a manifest.
fn verify(bytes: &[u8], signature: &str, key: &str) -> Result<InterfaceManifest, CheckError> {
    crate::update::verify_signature(bytes, signature, key).map_err(|_| CheckError::Signature)?;
    let manifest: InterfaceManifest =
        serde_json::from_slice(bytes).map_err(|e| CheckError::Invalid(e.to_string()))?;
    if manifest.schema != INTERFACE_MANIFEST_SCHEMA {
        return Err(CheckError::Invalid(format!(
            "schema {} is not one this UXDuck reads",
            manifest.schema
        )));
    }
    Ok(manifest)
}

/// Downloads to `path`, refusing more than `size` or the limit.
async fn download(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    size: u64,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut response = client.get(url).send().await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            "no connection".to_owned()
        } else {
            e.to_string()
        }
    })?;
    if !response.status().is_success() {
        return Err(format!("the server answered {}", response.status()));
    }
    let limit = size.min(MAX_ARCHIVE_BYTES);
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err("it is larger than its manifest says".into());
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        received += chunk.len() as u64;
        if received > limit {
            return Err("it is larger than its manifest says".into());
        }
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.sync_all().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// How a file may be cached.
enum Freshness {
    /// Content-hashed: kept for good.
    Immutable,
    /// Checked again on every use, by this tag: the version served.
    Version(String),
}

async fn respond(found: built_in::Found, mut request: Request, freshness: &Freshness) -> Response {
    if let Freshness::Version(tag) = freshness {
        let same = request
            .headers()
            .get(header::IF_NONE_MATCH)
            .is_some_and(|sent| sent.as_bytes() == tag.as_bytes());
        if same {
            let mut unchanged = StatusCode::NOT_MODIFIED.into_response();
            set_freshness(&mut unchanged, freshness);
            return unchanged;
        }
        let headers = request.headers_mut();
        headers.remove(header::IF_MODIFIED_SINCE);
        headers.remove(header::IF_NONE_MATCH);
    }
    let mut response = match found {
        built_in::Found::Bytes { body, mime } => {
            ([(header::CONTENT_TYPE, mime)], Body::from(body)).into_response()
        }
        built_in::Found::Disk(file) => match ServeFile::new(file).try_call(request).await {
            Ok(response) => response.map(Body::new).into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
    };
    if matches!(freshness, Freshness::Version(_)) {
        response.headers_mut().remove(header::LAST_MODIFIED);
    }
    set_freshness(&mut response, freshness);
    response
}

fn set_freshness(response: &mut Response, freshness: &Freshness) {
    let headers = response.headers_mut();
    match freshness {
        Freshness::Immutable => {
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
        }
        Freshness::Version(tag) => {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            if let Ok(tag) = HeaderValue::from_str(tag) {
                headers.insert(header::ETAG, tag);
            }
        }
    }
}

/// The path inside the interface, or none when it could leave it.
fn inside(uri_path: &str) -> Option<String> {
    let path = percent_decode(uri_path.trim_start_matches('/'))?;
    let ok = !path.contains('\\')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| part != ".." && part != "." && !part.contains(':'));
    ok.then_some(path)
}

fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return Some(text.to_owned());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Serves the interface: `index.html` and every page path from the one
/// served now (`no-cache`), content-hashed files from it, then the other
/// kept ones, then the built-in copy (`immutable`), and other files from
/// the one served now (`no-cache`).
pub async fn serve(State(interfaces): State<Arc<Interfaces>>, request: Request) -> Response {
    let Some(path) = inside(request.uri().path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let page = path.is_empty() || path == "index.html" || !path.contains('.');
    if !page {
        return match interfaces.file(&path, request).await {
            Some(response) => response,
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let navigation = request
        .headers()
        .get("sec-fetch-dest")
        .is_some_and(|dest| dest == "document");
    let (source, version) = {
        let inner = interfaces.lock();
        (inner.serving.source.clone(), inner.serving.version.clone())
    };
    match interfaces.file("index.html", request).await {
        Some(response) => {
            if navigation && matches!(source, Source::Kept(_)) && response.status().is_success() {
                interfaces.loaded(&version);
            }
            response
        }
        None => no_ui(&interfaces),
    }
}

fn no_ui(interfaces: &Interfaces) -> Response {
    const NOT_BUNDLED: &str = "The web app (<code>apps/web</code>) is not bundled in development.";
    let (heading, explanation) = if let Some(origin) = &interfaces.config.dev {
        let origin = escape_html(origin.trim_end_matches('/'));
        (
            "Running in development",
            format!("{NOT_BUNDLED} Vite serves it at <a href=\"{origin}/\">{origin}</a>."),
        )
    } else if matches!(interfaces.files, built_in::Files::None) {
        ("Running in development", NOT_BUNDLED.to_owned())
    } else {
        (
            "UXDuck is running",
            "The app files are missing. Reinstall UXDuck, or check your \
             <code>--ui-dir</code> directory."
                .to_owned(),
        )
    };
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<!doctype html><title>UXDuck</title><h1>{heading}</h1><p>{explanation}</p>"
        )),
    )
        .into_response()
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What a page reports: its version, and for a failed start, the error.
#[derive(Debug, serde::Deserialize)]
pub struct PageReport {
    pub version: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /interface/ready`.
pub fn ready(
    State(interfaces): State<Arc<Interfaces>>,
    axum::Json(report): axum::Json<PageReport>,
) -> StatusCode {
    interfaces.ready(&report.version);
    StatusCode::NO_CONTENT
}

/// `POST /interface/boot-failed`.
pub fn boot_failed(
    State(interfaces): State<Arc<Interfaces>>,
    axum::Json(report): axum::Json<PageReport>,
) -> StatusCode {
    let error: String = report
        .error
        .as_deref()
        .unwrap_or("it did not start")
        .chars()
        .take(300)
        .collect();
    interfaces.failed(
        &report.version,
        &format!("a page could not start it: {error}"),
    );
    StatusCode::NO_CONTENT
}

/// `interface/check` from a page.
#[derive(Debug)]
struct Port(Arc<Interfaces>);

impl uxduck_engine::interface::InterfacePort for Port {
    fn check(&mut self) {
        self.0.wake.notify_one();
    }
}

/// `uxduck config set interface.updates on|off`, written without UXDuck
/// running; the next check reads it.
///
/// # Errors
/// A value other than on or off, or a store that cannot be written.
pub fn set_updates(data_dir: &Path, value: &str) -> Result<(), String> {
    if value != "on" && value != "off" {
        return Err("interface.updates is on or off".into());
    }
    let store = Store::open(data_dir.join("interface")).map_err(|e| e.to_string())?;
    store
        .set_settings(&store::Settings {
            updates: value.to_owned(),
        })
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;

//! The simulator catalogue (S18, ADR-0011 M12): prebuilt SITL builds for
//! this platform, listed in a manifest signed with the release key
//! (ADR-0008). A build downloads file by file into `.part` files, resuming
//! where a broken download stopped, and each file must match the signed
//! checksum before the build is installed into its own folder. The
//! catalogue's files are zstd-compressed and decompressed as they arrive;
//! the compressed bytes are kept in `.download` so a broken download
//! resumes, and they are what the checksum is of. Deleting a
//! build removes that folder. Nothing is fetched until the simulation page
//! asks.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::sim::{
    CATALOGUE_SCHEMA, CatalogueBuild, CatalogueEntry, CatalogueFile, SimCatalogue,
    SimCatalogueResult,
};

use super::{BUILD_FILE, Simulators};

/// Where the catalogue is published (R2, `docs/plan/review.md` S18).
pub const DEFAULT_URL: &str = "https://simulators.uxduck.com/catalogue.json";

/// This machine, as the catalogue names platforms: `aarch64-macos`.
#[must_use]
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

#[derive(Clone, Debug, Default)]
enum Fetch {
    #[default]
    NotYet,
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
struct Download {
    received: u64,
    failed: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    fetch: Fetch,
    builds: Vec<CatalogueBuild>,
    downloads: HashMap<String, Download>,
}

/// The newest manifest seen, so an older signed one cannot be replayed.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Seen {
    published_at: String,
}

#[derive(Debug)]
pub struct Catalogue {
    url: Option<String>,
    key: Option<String>,
    simulators: Arc<Simulators>,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
}

impl Catalogue {
    /// `UXDUCK_SIM_CATALOGUE_URL` points a development build elsewhere, and
    /// `none` turns the catalogue off. The key is the release key built into this binary; a
    /// development build takes `UXDUCK_SIM_CATALOGUE_KEY` instead, for
    /// tests.
    #[must_use]
    pub fn new(simulators: Arc<Simulators>, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let url = match crate::update::url_override("UXDUCK_SIM_CATALOGUE_URL") {
            Some(url) if url == "none" => None,
            Some(url) => Some(url),
            None => Some(DEFAULT_URL.to_owned()),
        };
        let key = std::env::var("UXDUCK_SIM_CATALOGUE_KEY")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .or_else(|| crate::update::PUBLIC_KEY.map(str::to_owned));
        Arc::new(Self {
            url,
            key,
            simulators,
            runtime,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What `sim/catalogue` answers; fetches the manifest the first time,
    /// and again when asked.
    pub fn result(self: &Arc<Self>, refresh: bool) -> SimCatalogueResult {
        let fetch_now = {
            let state = self.lock();
            refresh && !matches!(state.fetch, Fetch::Loading)
                || matches!(state.fetch, Fetch::NotYet)
        };
        if fetch_now {
            self.fetch();
        }
        let installed = self.simulators.catalogue_dir();
        let state = self.lock();
        let (state_name, detail) = match &state.fetch {
            Fetch::NotYet | Fetch::Loading => ("loading", None),
            Fetch::Ready => ("ready", None),
            Fetch::Failed(why) => ("failed", Some(why.clone())),
        };
        let builds = state
            .builds
            .iter()
            .map(|build| {
                let total_bytes = build.files.iter().map(|f| f.size).sum();
                let download = state.downloads.get(&build.id);
                let (entry_state, received_bytes, detail) = match download {
                    Some(Download {
                        failed: Some(why), ..
                    }) => ("failed", None, Some(why.clone())),
                    Some(Download { received, .. }) => ("downloading", Some(*received), None),
                    None if installed.join(&build.id).join(BUILD_FILE).is_file() => {
                        ("installed", None, None)
                    }
                    None => ("available", None, None),
                };
                CatalogueEntry {
                    build: build.clone(),
                    state: entry_state.into(),
                    received_bytes,
                    total_bytes,
                    detail,
                }
            })
            .collect();
        SimCatalogueResult {
            state: state_name.into(),
            detail,
            builds,
        }
    }

    /// Fetches and verifies the manifest in the background.
    fn fetch(self: &Arc<Self>) {
        self.lock().fetch = Fetch::Loading;
        let this = self.clone();
        self.runtime.spawn(async move {
            let fetched = this.fetch_verified().await;
            let mut state = this.lock();
            match fetched {
                Ok(builds) => {
                    state.builds = builds;
                    state.fetch = Fetch::Ready;
                }
                Err(why) => {
                    tracing::warn!(%why, "cannot read the simulator catalogue");
                    state.fetch = Fetch::Failed(why);
                }
            }
        });
    }

    async fn fetch_verified(&self) -> Result<Vec<CatalogueBuild>, String> {
        let url = self
            .url
            .as_deref()
            .ok_or("the simulator catalogue is off here")?;
        let key = self
            .key
            .as_deref()
            .ok_or("this build has no release key, so it cannot trust a catalogue")?;
        let client = client()?;
        let get = |url: String| {
            let client = client.clone();
            async move {
                let response = client
                    .get(&url)
                    .timeout(Duration::from_secs(30))
                    .send()
                    .await
                    .map_err(|e| format!("cannot fetch {url}: {e}"))?;
                if !response.status().is_success() {
                    return Err(format!("cannot fetch {url}: {}", response.status()));
                }
                response
                    .bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| format!("cannot fetch {url}: {e}"))
            }
        };
        let bytes = get(url.to_owned()).await?;
        let signature = get(format!("{url}.minisig")).await?;
        let catalogue = verify(&bytes, &String::from_utf8_lossy(&signature), key)?;
        // Refuse a manifest older than the newest seen: an old signed one
        // could list builds since withdrawn.
        let seen_path = self.simulators.catalogue_dir().join("seen.json");
        let seen: Seen = std::fs::read(&seen_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if catalogue.published_at < seen.published_at {
            return Err(format!(
                "the catalogue was published at {}, before one already seen ({}). It is ignored",
                catalogue.published_at, seen.published_at
            ));
        }
        let _ = std::fs::create_dir_all(self.simulators.catalogue_dir());
        let _ = std::fs::write(
            &seen_path,
            serde_json::to_vec(&Seen {
                published_at: catalogue.published_at.clone(),
            })
            .unwrap_or_default(),
        );
        let here = platform();
        Ok(catalogue
            .builds
            .into_iter()
            .filter(|b| b.platform == here && valid(b))
            .collect())
    }

    /// Starts downloading a build of this platform's catalogue.
    ///
    /// # Errors
    /// When the build is not in the catalogue, or is already downloading.
    pub fn download(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let build = {
            let mut state = self.lock();
            let build = state
                .builds
                .iter()
                .find(|b| b.id == id)
                .cloned()
                .ok_or_else(|| format!("no build {id} in the catalogue for this computer"))?;
            if state.downloads.get(id).is_some_and(|d| d.failed.is_none()) {
                return Err(format!("{id} is already downloading"));
            }
            state.downloads.insert(
                id.to_owned(),
                Download {
                    received: 0,
                    failed: None,
                },
            );
            build
        };
        let this = self.clone();
        self.runtime.spawn(async move {
            match this.install(&build).await {
                Ok(()) => {
                    tracing::info!(build = %build.id, "installed a simulator build");
                    let simulators = this.simulators.clone();
                    let _ = tokio::task::spawn_blocking(move || simulators.rescan()).await;
                    // Installed once the registry knows it.
                    this.lock().downloads.remove(&build.id);
                }
                Err(why) => {
                    tracing::warn!(build = %build.id, %why, "a simulator download failed");
                    if let Some(download) = this.lock().downloads.get_mut(&build.id) {
                        download.failed = Some(why);
                    }
                }
            }
        });
        Ok(())
    }

    async fn install(&self, build: &CatalogueBuild) -> Result<(), String> {
        let root = self.simulators.catalogue_dir();
        let parts = root.join("downloads").join(&build.id);
        std::fs::create_dir_all(&parts).map_err(|e| e.to_string())?;
        let client = client()?;
        let mut before = 0;
        for file in &build.files {
            let part = parts.join(format!("{}.part", file.name.replace('/', "__")));
            self.fetch_file(&client, file, &part, before).await?;
            before += file.size;
        }
        // All there and all checked: into the build's own folder.
        let folder = root.join(&build.id);
        let _ = std::fs::remove_dir_all(&folder);
        for (index, file) in build.files.iter().enumerate() {
            let target = folder.join(&file.name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let part = parts.join(format!("{}.part", file.name.replace('/', "__")));
            std::fs::rename(&part, &target).map_err(|e| e.to_string())?;
            if index == 0 {
                executable(&target)?;
            }
        }
        std::fs::write(
            folder.join(BUILD_FILE),
            serde_json::to_vec_pretty(build).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&parts);
        Ok(())
    }

    /// One file's download into `<part>.download`, resuming what is there,
    /// checked against the catalogue; a zstd one is decompressed into `part`
    /// as it arrives, and otherwise the download becomes `part`.
    async fn fetch_file(
        &self,
        client: &reqwest::Client,
        file: &CatalogueFile,
        part: &Path,
        before: u64,
    ) -> Result<(), String> {
        let download = part.with_extension("download");
        let zstd = file.encoding.as_deref() == Some("zstd");
        let mismatch = || {
            let _ = std::fs::remove_file(&download);
            let _ = std::fs::remove_file(part);
            format!(
                "{} does not match the catalogue's checksum, so it was deleted",
                file.name
            )
        };
        let mut have = std::fs::metadata(&download).map_or(0, |m| m.len());
        if have > file.size {
            have = 0;
            let _ = std::fs::remove_file(&download);
        }
        let mut sink = zstd.then(|| Decompress::new(part)).transpose()?;
        // What came before a break is decompressed again, so the
        // decompressed file always starts from the frame's beginning.
        if let Some(sink) = sink.as_mut().filter(|_| have > 0) {
            let mut kept = std::fs::File::open(&download).map_err(|e| e.to_string())?;
            if std::io::copy(&mut kept, sink).is_err() {
                return Err(mismatch());
            }
        }
        if have < file.size {
            let mut request = client.get(&file.url);
            if have > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
            }
            let mut response = request
                .send()
                .await
                .map_err(|e| format!("cannot download {}: {e}", file.name))?;
            let resumed = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
            if !response.status().is_success() {
                return Err(format!(
                    "cannot download {}: {}",
                    file.name,
                    response.status()
                ));
            }
            if !resumed && have > 0 {
                have = 0;
                sink = zstd.then(|| Decompress::new(part)).transpose()?;
            }
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(resumed)
                .write(true)
                .truncate(!resumed)
                .open(&download)
                .map_err(|e| e.to_string())?;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("the download of {} broke off: {e}", file.name))?
            {
                out.write_all(&chunk).map_err(|e| e.to_string())?;
                // Corrupt, or larger than any build: it cannot match.
                if let Some(sink) = sink.as_mut()
                    && sink.write_all(&chunk).is_err()
                {
                    return Err(mismatch());
                }
                have += chunk.len() as u64;
                if let Some(download) = self.lock().downloads.get_mut(&file_owner(part)) {
                    download.received = before + have;
                }
            }
            out.flush().map_err(|e| e.to_string())?;
        }
        let decompressed = sink.map(Decompress::finish).transpose();
        if decompressed.is_err() || hash(&download)? != file.sha256.to_ascii_lowercase() {
            return Err(mismatch());
        }
        if zstd {
            let _ = std::fs::remove_file(&download);
        } else {
            std::fs::rename(&download, part).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Deletes an installed build and its working copies' binaries.
    ///
    /// # Errors
    /// When it is not installed, or cannot be deleted.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        if id.contains(['/', '\\']) || id.starts_with('.') {
            return Err(format!("no build {id}"));
        }
        let folder = self.simulators.catalogue_dir().join(id);
        if !folder.join(BUILD_FILE).is_file() {
            return Err(format!("{id} is not installed"));
        }
        std::fs::remove_dir_all(&folder).map_err(|e| format!("cannot delete {id}: {e}"))?;
        self.lock().downloads.remove(id);
        self.simulators.rescan();
        tracing::info!(build = %id, "deleted a simulator build");
        Ok(())
    }
}

/// The most a catalogue file decompresses to: far more than any SITL
/// build, so a corrupt or hostile stream cannot fill the disk.
const MAX_DECOMPRESSED: u64 = 512 * 1024 * 1024;

/// Decompresses zstd into a file as it arrives.
struct Decompress(zstd::stream::write::Decoder<'static, Capped>);

/// A file that refuses more than [`MAX_DECOMPRESSED`].
struct Capped {
    file: std::fs::File,
    written: u64,
}

impl std::io::Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written + bytes.len() as u64 > MAX_DECOMPRESSED {
            return Err(std::io::Error::other("it decompresses to too much"));
        }
        let n = self.file.write(bytes)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Decompress {
    fn new(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        zstd::stream::write::Decoder::new(Capped { file, written: 0 })
            .map(Self)
            .map_err(|e| e.to_string())
    }

    fn finish(mut self) -> Result<(), String> {
        self.0.flush().map_err(|e| e.to_string())
    }
}

impl std::io::Write for Decompress {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The download folder is named after the build, which owns the progress.
fn file_owner(part: &Path) -> String {
    part.parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .user_agent(format!("uxduck/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())
}

fn hash(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(unix)]
fn executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps, reason = "the same signature as on unix")]
fn executable(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Verifies and reads a manifest.
///
/// # Errors
/// A bad key, signature or document.
pub fn verify(bytes: &[u8], signature: &str, public_key: &str) -> Result<SimCatalogue, String> {
    check_signature(bytes, signature, public_key)?;
    let catalogue: SimCatalogue =
        serde_json::from_slice(bytes).map_err(|e| format!("the catalogue is not valid: {e}"))?;
    if catalogue.schema != CATALOGUE_SCHEMA {
        return Err(format!(
            "catalogue schema {} is not one this build reads ({CATALOGUE_SCHEMA})",
            catalogue.schema
        ));
    }
    Ok(catalogue)
}

/// Whether `bytes` were signed with the release key, as the parameter
/// definitions catalogue is checked (S67).
///
/// # Errors
/// A bad key or signature.
pub fn check_signature(bytes: &[u8], signature: &str, public_key: &str) -> Result<(), String> {
    uxduck_engine::param_definitions::files::check_signature(bytes, signature, public_key)
}

/// A build this daemon can install safely: files that stay in its folder,
/// the program first, over HTTPS (or, in a development build, plain HTTP
/// to this machine, for tests).
fn valid(build: &CatalogueBuild) -> bool {
    let safe_id = !build.id.is_empty()
        && build
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        && !build.id.starts_with('.');
    let fetched = |url: &str| {
        url.starts_with("https://")
            || cfg!(debug_assertions)
                && (url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost"))
    };
    let safe_files = !build.files.is_empty()
        && build.files.iter().all(|f| {
            let local = Path::new(&f.name)
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
            let readable = f.encoding.as_deref().is_none_or(|e| e == "zstd");
            local && fetched(&f.url) && f.sha256.len() == 64 && readable
        });
    let program_first = build.files.first().is_some_and(|f| {
        f.name
            .to_ascii_lowercase()
            .starts_with(&build.program.to_ascii_lowercase())
    });
    safe_id && safe_files && program_first
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_contracts::dto::sim::CatalogueFile;

    fn build(name: &str, url: &str) -> CatalogueBuild {
        CatalogueBuild {
            id: "ardupilot-arducopter-4.7.1-x86_64-linux".into(),
            firmware: "ardupilot".into(),
            program: "arducopter".into(),
            version: "4.7.1".into(),
            platform: "x86_64-linux".into(),
            files: vec![CatalogueFile {
                name: name.into(),
                url: url.into(),
                size: 1,
                sha256: "0".repeat(64),
                encoding: None,
            }],
            source_url: "https://example.com/source.tar.gz".into(),
        }
    }

    #[test]
    fn only_builds_that_stay_in_their_folder_over_https_are_installed() {
        assert!(valid(&build(
            "arducopter",
            "https://simulators.uxduck.com/a"
        )));
        assert!(!valid(&build("../arducopter", "https://x/a")));
        assert!(!valid(&build("/usr/bin/arducopter", "https://x/a")));
        assert!(!valid(&build("arducopter", "http://example.com/a")));
        assert!(
            !valid(&build("cygwin1.dll", "https://x/a")),
            "the program comes first"
        );
        let mut sneaky = build("arducopter", "https://x/a");
        sneaky.id = "../../evil".into();
        assert!(!valid(&sneaky));
    }
}

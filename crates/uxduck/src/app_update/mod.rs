//! Background product downloads and the consented installer handoff. Electron
//! is never involved. The signed installer is checked both before and after
//! shutdown; a copied worker holds the daemon lock until installation ends.
//! A Mac downloads only the files of the app that changed when the release
//! lists them (S164, `files`), and falls back to the full package otherwise.
#[cfg(unix)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "only the Mac installer builds bundles; tested everywhere"
    )
)]
mod files;
mod installer;
#[cfg(target_os = "macos")]
mod sparkle;
#[cfg(test)]
mod tests;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uxduck_contracts::dto::release::ReleaseArtifact;
use uxduck_contracts::dto::update::{AppUpdateStatus, UpdatePhase};

use crate::daemon::engine_task::EngineHandle;

// Kept open until process exit, so the worker cannot install while the old
// daemon, Electron children or background tasks are still alive.
static HANDOFF_PIPE: std::sync::Mutex<Option<std::process::ChildStdin>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "macos")]
static SPARKLE_GUARD_PIPE: std::sync::Mutex<Option<std::process::ChildStdin>> =
    std::sync::Mutex::new(None);

const MAX_PACKAGE: u64 = 1 << 30;
const MAX_MANIFEST: u64 = 4 << 20;
/// Files downloaded at once for a per-file update.
#[cfg(unix)]
const PARALLEL_FILES: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Plan {
    version: String,
    kind: String,
    executable: PathBuf,
    #[serde(default)]
    installed_executable: Option<PathBuf>,
    data_dir: PathBuf,
    port: u16,
    reopen: bool,
}

fn key() -> Option<String> {
    std::env::var("UXDUCK_APP_UPDATE_KEY")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .or_else(|| crate::update::APP_UPDATE_PUBLIC_KEY.map(str::to_owned))
}

fn status(phase: UpdatePhase) -> AppUpdateStatus {
    AppUpdateStatus {
        phase,
        current: env!("CARGO_PKG_VERSION").into(),
        ..AppUpdateStatus::default()
    }
}

pub fn start(engine: EngineHandle, data_dir: PathBuf, port: u16, enabled: bool) {
    #[cfg(target_os = "macos")]
    if enabled && sparkle::installed_app().is_ok() {
        sparkle::start(engine, data_dir, port);
        return;
    }
    let Some(key) = key().filter(|_| enabled) else {
        return;
    };
    let url = crate::update::url_override("UXDUCK_UPDATE_URL")
        .unwrap_or_else(|| crate::update::DEFAULT_URL.into());
    if url == "none" {
        return;
    }
    tokio::spawn(async move {
        let mut next = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut ready: Option<PathBuf> = None;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(next) => {},
                () = engine.requested(|r| r.app_update) => {},
            }
            let Ok((check, queued)) = engine
                .run(|e| {
                    (
                        e.app_update_take_check(),
                        e.app_update_status().install_when_idle,
                    )
                })
                .await
            else {
                return;
            };
            if !queued && (check || tokio::time::Instant::now() >= next) {
                let _ = engine
                    .run(|e| e.app_update_publish(status(UpdatePhase::Checking)))
                    .await;
                let Ok(channel) = DownloadChannel::current(&engine).await else {
                    return;
                };
                match download_selected(channel, &data_dir, &url, &key).await {
                    Ok(path) => ready = path,
                    Err(why) => {
                        ready = None;
                        publish_channel_failure(&engine, channel.include_beta, why).await;
                    }
                }
                let spread = Duration::from_secs(u64::from(std::process::id() % 3600));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
                next = tokio::time::Instant::now() + crate::update::until_next_day(now, spread);
            }
            let Ok(true) = engine.run(|e| e.app_update_approved()).await else {
                // Only a consented update waiting for work to finish polls.
                // An ordinary idle ground station sleeps until its daily check.
                if queued {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                continue;
            };
            let Some(dir) = ready.as_ref() else {
                continue;
            };
            let reopen = engine
                .run(|e| e.app_update_visible())
                .await
                .unwrap_or(false);
            let (stage, data) = (dir.clone(), data_dir.clone());
            let prepared =
                tokio::task::spawn_blocking(move || prepare_worker(&stage, &data, port, reopen))
                    .await
                    .unwrap_or_else(|error| Err(format!("The installer helper failed: {error}")));
            match prepared {
                Ok((mut child, mut permission)) => {
                    // The engine's final gate runs after the helper is spawned.
                    // Without this byte the child cannot install, even on EOF.
                    if matches!(
                        engine.run(uxduck_engine::Engine::app_update_handoff).await,
                        Ok(true)
                    ) {
                        if let Err(error) = permission.write_all(b"install\n") {
                            tracing::error!(%error, "installer permission pipe failed");
                        }
                        if let Ok(mut pipe) = HANDOFF_PIPE.lock() {
                            *pipe = Some(permission);
                        }
                        return;
                    }
                    drop(permission);
                    let _ = child.wait();
                }
                Err(why) => {
                    publish_failure(&engine, why).await;
                    // A per-file update that failed its checks: fetch the
                    // full installer now, and ask again once it is ready.
                    if dir.join(FILES_FAILED).is_file()
                        && !dir.join(format!("package.{PACKAGE_KIND}")).is_file()
                    {
                        ready = None;
                        next = tokio::time::Instant::now();
                    }
                }
            }
        }
    });
}

/// The full installer kind on this platform.
const PACKAGE_KIND: &str = if cfg!(target_os = "macos") {
    "pkg"
} else if cfg!(windows) {
    "msi"
} else {
    "deb"
};

/// Marks a version whose per-file update failed, so it installs from its
/// full package instead. Kept in the version's folder.
const FILES_FAILED: &str = "files.failed";

async fn publish_failure(engine: &EngineHandle, why: String) {
    tracing::warn!(%why, "app update failed");
    let _ = engine
        .run(move |e| {
            let mut s = e.app_update_status();
            s.phase = UpdatePhase::Failed;
            s.detail = Some(why);
            e.app_update_publish(s);
        })
        .await;
}

async fn publish_channel_failure(engine: &EngineHandle, include_beta: bool, why: String) {
    tracing::warn!(%why, "app update failed");
    let _ = engine
        .run(move |e| {
            let mut s = e.app_update_status();
            s.phase = UpdatePhase::Failed;
            s.detail = Some(why);
            e.app_update_publish_for_channel(include_beta, s);
        })
        .await;
}

/// Every callback from one download belongs to its original eligibility.
#[derive(Clone, Copy)]
struct DownloadChannel<'a> {
    engine: &'a EngineHandle,
    include_beta: bool,
}

impl<'a> DownloadChannel<'a> {
    async fn current(engine: &'a EngineHandle) -> Result<Self, String> {
        let include_beta = engine
            .run(|e| e.state().settings.include_beta)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            engine,
            include_beta,
        })
    }

    async fn publish(self, status: AppUpdateStatus) -> Result<(), String> {
        let include_beta = self.include_beta;
        let accepted = self
            .engine
            .run(move |e| e.app_update_publish_for_channel(include_beta, status))
            .await
            .map_err(|e| e.to_string())?;
        if accepted {
            Ok(())
        } else {
            Err("Update preference changed. Checking the selected channel again.".into())
        }
    }
}

/// Use the platform's installer, never an archive of individual binaries.
fn installer_artifact(
    report: &crate::update::Report,
    target: &str,
) -> Result<ReleaseArtifact, String> {
    let kind = if target.ends_with("apple-darwin") {
        "pkg"
    } else if target.contains("windows") {
        "msi"
    } else if target.contains("linux") {
        "deb"
    } else {
        return Err("Automatic installation is not available on this platform.".into());
    };
    let artifact = crate::update::artifact_for(&report.manifest, target).filter(|a| a.kind == kind)
        .ok_or_else(|| format!("This release has no {kind} installer for this computer. Download and install UXDuck manually."))?;
    if artifact.size == 0
        || artifact.size > MAX_PACKAGE
        || artifact.sha256.len() != 64
        || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("The release has an invalid installer size or checksum.".into());
    }
    if !allowed_url(&artifact.url) {
        return Err("The installer download must use HTTPS.".into());
    }
    Ok(artifact)
}

/// Downloads use HTTPS; development and the local update test may use this
/// computer's own server.
fn allowed_url(url: &str) -> bool {
    url.starts_with("https://")
        || (cfg!(debug_assertions) || crate::update::IS_UPDATE_TEST)
            && url.starts_with("http://127.0.0.1:")
}

/// The per-file list of this target's app (S164), when the release has one.
#[cfg(unix)]
fn files_artifact(report: &crate::update::Report, target: &str) -> Option<ReleaseArtifact> {
    use uxduck_contracts::dto::release::BUNDLE_FILES_KIND;
    report
        .manifest
        .artifacts
        .iter()
        .find(|a| a.target == target && a.kind == BUNDLE_FILES_KIND)
        .filter(|a| {
            a.size > 0
                && a.size <= files::MAX_LIST
                && a.sha256.len() == 64
                && a.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                && allowed_url(&a.url)
        })
        .cloned()
}

async fn publish_current(
    engine: &EngineHandle,
    include_beta: bool,
    warning: Option<String>,
) -> Result<(), String> {
    let mut current = status(UpdatePhase::Current);
    if !include_beta && env!("CARGO_PKG_VERSION").contains('-') {
        current.detail = Some("You’ll receive stable updates when they catch up.".into());
    }
    if let Some(warning) = warning {
        current.detail = Some(warning);
    }
    engine
        .run(move |e| e.app_update_publish_for_channel(include_beta, current))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Cancel even a stalled network read promptly when eligibility changes.
async fn download_selected(
    channel: DownloadChannel<'_>,
    data_dir: &Path,
    url: &str,
    key: &str,
) -> Result<Option<PathBuf>, String> {
    let download = download(channel, data_dir, url, key);
    tokio::pin!(download);
    loop {
        tokio::select! {
            result = &mut download => return result,
            () = tokio::time::sleep(Duration::from_secs(1)) => {
                let include_beta = channel.include_beta;
                if !channel.engine.run(move |e| e.state().settings.include_beta == include_beta)
                    .await.unwrap_or(false) {
                    return Err("Update preference changed. Checking the selected channel again.".into());
                }
            }
        }
    }
}

async fn download(
    channel: DownloadChannel<'_>,
    data_dir: &Path,
    url: &str,
    key: &str,
) -> Result<Option<PathBuf>, String> {
    let DownloadChannel {
        engine,
        include_beta,
    } = channel;
    let report = crate::update::check_channels(url, include_beta, Some(key), data_dir)
        .await
        .map_err(|e| e.to_string())?;
    if !matches!(
        report.verdict,
        crate::update::Verdict::Available(_) | crate::update::Verdict::RolledBack(_)
    ) {
        publish_current(engine, include_beta, report.warning).await?;
        return Ok(None);
    }
    let artifact = installer_artifact(&report, crate::update::TARGET)?;
    let version = semver::Version::parse(&report.manifest.version)
        .map_err(|e| e.to_string())?
        .to_string();
    let dir = data_dir.join("app-updates").join(&version);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let package = dir.join(format!("package.{}", artifact.kind));
    let mut s = status(UpdatePhase::Downloading);
    s.version = Some(version.clone());
    s.detail = report.warning.clone();
    s.size = Some(artifact.size);
    s.notes_url = report
        .manifest
        .notes_url
        .clone()
        .filter(|url| url.starts_with("https://"));
    let (metadata_dir, signed, signature) = (
        dir.clone(),
        report.signed_bytes.clone(),
        report.signature.clone(),
    );
    tokio::task::spawn_blocking(move || {
        write(&metadata_dir.join("manifest.json"), &signed)?;
        write(&metadata_dir.join("manifest.minisig"), signature.as_bytes())
    })
    .await
    .map_err(|e| e.to_string())??;
    #[cfg(unix)]
    let staged = match files_artifact(&report, crate::update::TARGET) {
        Some(list) if cfg!(target_os = "macos") => {
            let installed = Path::new(installer::INSTALLED_APP);
            match download_files(channel, &dir, &list, &version, installed, &mut s).await {
                Ok(()) => true,
                Err(why) => {
                    channel.publish(s.clone()).await?;
                    // Never a half-built app: this version uses its full package.
                    tracing::warn!(%why, "per-file update unavailable; downloading the full installer");
                    let _ = write(&dir.join(FILES_FAILED), why.as_bytes());
                    let _ = std::fs::remove_file(dir.join(files::LIST));
                    s.size = Some(artifact.size);
                    s.received = None;
                    false
                }
            }
        }
        _ => false,
    };
    #[cfg(not(unix))]
    let staged = false;
    if !staged {
        let initial = s.clone();
        channel.publish(initial).await?;
        if verify_file_async(&package, &artifact).await.is_err() {
            let partial = dir.join("package.partial");
            let fetched = download_file(channel, &artifact, &partial, &s).await;
            if let Err(error) = fetched {
                let _ = std::fs::remove_file(&partial);
                return Err(error);
            }
            if let Err(error) = verify_file_async(&partial, &artifact).await {
                let _ = std::fs::remove_file(&partial);
                return Err(error);
            }
            if package.exists() {
                std::fs::remove_file(&package).map_err(|e| e.to_string())?;
            }
            std::fs::rename(&partial, &package).map_err(|e| e.to_string())?;
        }
        s.received = Some(artifact.size);
    }
    s.phase = UpdatePhase::Ready;
    // Surface a cancelled/failed installation when the background process returns.
    if let Ok(result) = std::fs::read_to_string(data_dir.join("app-update-result")) {
        s.detail = Some(
            s.detail
                .map_or_else(|| result.clone(), |warning| format!("{warning} {result}")),
        );
    }
    channel.publish(s).await?;
    Ok(Some(dir))
}

/// Shared download progress across files fetched at once.
#[cfg(unix)]
struct Progress {
    received: std::sync::atomic::AtomicU64,
    published: std::sync::Mutex<std::time::Instant>,
}

#[cfg(unix)]
impl Progress {
    /// Adds `bytes`, and returns the total when it is time to show it.
    fn add(&self, bytes: u64) -> Option<u64> {
        use std::sync::atomic::Ordering;
        let total = self.received.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let mut last = self.published.lock().ok()?;
        (last.elapsed() >= Duration::from_secs(1)).then(|| {
            *last = std::time::Instant::now();
            total
        })
    }
}

/// A per-file update (S164): the list the signed manifest names, then only
/// the files the installed app does not have, each checked, then a complete
/// candidate bundle built and checked like the installed update will be.
/// On success the list is kept in `dir` as the staged update.
#[cfg(unix)]
async fn download_files(
    channel: DownloadChannel<'_>,
    dir: &Path,
    artifact: &ReleaseArtifact,
    version: &str,
    installed: &Path,
    s: &mut AppUpdateStatus,
) -> Result<(), String> {
    use futures::{StreamExt, TryStreamExt};
    if let Ok(why) = std::fs::read_to_string(dir.join(FILES_FAILED)) {
        return Err(why);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_mins(15))
        .build()
        .map_err(|e| e.to_string())?;
    let bytes = fetch_bounded(&client, &artifact.url, artifact.size).await?;
    let list = files::read(
        &bytes,
        artifact,
        version,
        crate::update::TARGET,
        allowed_url,
    )?;
    let (stage, plan, from) = (dir.to_owned(), list.clone(), installed.to_owned());
    let missing = tokio::task::spawn_blocking(move || {
        let have = files::index(&from, &plan)?;
        Ok::<_, String>(files::missing(&plan, |sha, size| {
            have.contains_key(sha) || files::have_blob(&stage, sha, size)
        }))
    })
    .await
    .map_err(|e| e.to_string())??;
    let total: u64 = missing.iter().map(|m| m.packed).sum();
    if missing
        .iter()
        .map(|m| files::packed_limit(m.size))
        .sum::<u64>()
        > MAX_PACKAGE
    {
        return Err("The update's files are larger than UXDuck accepts.".into());
    }
    s.size = Some(total);
    s.received = Some(0);
    let initial = s.clone();
    channel.publish(initial).await?;
    let progress = Progress {
        received: 0.into(),
        published: std::sync::Mutex::new(std::time::Instant::now()),
    };
    let shown = s.clone();
    futures::stream::iter(missing)
        .map(|file| {
            let (client, list, progress, shown) = (&client, &list, &progress, &shown);
            async move {
                let compressed = dir.join("blobs").join(format!("{}.zst", file.sha256));
                std::fs::create_dir_all(dir.join("blobs")).map_err(|e| e.to_string())?;
                let url = format!("{}{}.zst", list.blobs, file.sha256);
                let fetched = fetch_file(
                    client,
                    &url,
                    &compressed,
                    files::packed_limit(file.size),
                    |bytes| {
                        if let Some(received) = progress.add(bytes) {
                            let mut s = shown.clone();
                            s.received = Some(received.min(total));
                            Some(s)
                        } else {
                            None
                        }
                    },
                    channel,
                )
                .await;
                let unpacked = match fetched {
                    Ok(()) => {
                        let (compressed, dir) = (compressed.clone(), dir.to_owned());
                        tokio::task::spawn_blocking(move || {
                            files::unpack(&compressed, &dir, &file.sha256, file.size)
                        })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|result| result)
                    }
                    Err(error) => Err(error),
                };
                let _ = std::fs::remove_file(&compressed);
                unpacked
            }
        })
        .buffer_unordered(PARALLEL_FILES)
        .try_collect::<Vec<()>>()
        .await?;
    s.received = Some(total);
    // The same build and checks the installation repeats after shutdown.
    let (stage, plan, approved) = (dir.to_owned(), list.clone(), version.to_owned());
    let installed = installed.to_owned();
    tokio::task::spawn_blocking(move || {
        installer::preflight_files(&stage, &plan, &installed, &approved)
    })
    .await
    .map_err(|e| e.to_string())??;
    write(&dir.join(files::LIST), &bytes)
}

/// Fetches `url` whole into memory, refusing more than `size` bytes.
#[cfg(unix)]
async fn fetch_bounded(client: &reqwest::Client, url: &str, size: u64) -> Result<Vec<u8>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if (bytes.len() + chunk.len()) as u64 > size {
            return Err("The update's file list is larger than the signed release says.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Streams `url` to `path`, refusing more than `limit` bytes, and reports
/// progress through `tick`, whose status is published when it returns one.
#[cfg(unix)]
async fn fetch_file(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    limit: u64,
    tick: impl Fn(u64) -> Option<AppUpdateStatus>,
    channel: DownloadChannel<'_>,
) -> Result<(), String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut out = tokio::fs::File::create(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut have = 0;
    while let Some(bytes) = response.chunk().await.map_err(|e| e.to_string())? {
        have += bytes.len() as u64;
        if have > limit {
            return Err("An update file is larger than the signed release allows.".into());
        }
        out.write_all(&bytes).await.map_err(|e| e.to_string())?;
        if let Some(s) = tick(bytes.len() as u64) {
            channel.publish(s).await?;
        }
    }
    out.sync_all().await.map_err(|e| e.to_string())
}

async fn download_file(
    channel: DownloadChannel<'_>,
    artifact: &ReleaseArtifact,
    partial: &Path,
    s: &AppUpdateStatus,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_mins(15))
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(&artifact.url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let mut out = tokio::fs::File::create(partial)
        .await
        .map_err(|e| e.to_string())?;
    let mut have = 0;
    let mut progress = std::time::Instant::now();
    while let Some(bytes) = response.chunk().await.map_err(|e| e.to_string())? {
        have += bytes.len() as u64;
        if have > artifact.size {
            return Err("The installer is larger than the signed release says.".into());
        }
        out.write_all(&bytes).await.map_err(|e| e.to_string())?;
        if progress.elapsed() >= Duration::from_secs(1) {
            let mut s = s.clone();
            s.received = Some(have);
            channel.publish(s).await?;
            progress = std::time::Instant::now();
        }
    }
    out.sync_all().await.map_err(|e| e.to_string())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| e.to_string())
}

async fn verify_file_async(path: &Path, artifact: &ReleaseArtifact) -> Result<(), String> {
    let (path, artifact) = (path.to_owned(), artifact.clone());
    tokio::task::spawn_blocking(move || verify_file(&path, &artifact))
        .await
        .map_err(|e| e.to_string())?
}

fn verify_file(path: &Path, artifact: &ReleaseArtifact) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() != artifact.size {
        return Err("The installer's size does not match the signed release.".into());
    }
    let mut hash = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    if format!("{:x}", hash.finalize()) != artifact.sha256.to_ascii_lowercase() {
        return Err("The installer does not match the signed release's checksum.".into());
    }
    Ok(())
}

/// The staged manifest, checked against the release key and its folder.
fn verified_report(dir: &Path, key: &str) -> Result<crate::update::Report, String> {
    let manifest = dir.join("manifest.json");
    if std::fs::metadata(&manifest)
        .map_err(|e| e.to_string())?
        .len()
        > MAX_MANIFEST
    {
        return Err("The release manifest is too large.".into());
    }
    let bytes = std::fs::read(manifest).map_err(|e| e.to_string())?;
    let signature =
        std::fs::read_to_string(dir.join("manifest.minisig")).map_err(|e| e.to_string())?;
    let manifest = crate::update::verify(&bytes, &signature, key).map_err(|e| e.to_string())?;
    if dir.file_name().and_then(|s| s.to_str()) != Some(&manifest.version) {
        return Err("The staged release version changed. Check for updates again.".into());
    }
    crate::update::compare(env!("CARGO_PKG_VERSION"), manifest, crate::update::TARGET)
        .map_err(|e| e.to_string())
}

fn verified_artifact(dir: &Path, key: &str) -> Result<ReleaseArtifact, String> {
    let report = verified_report(dir, key)?;
    let artifact = installer_artifact(&report, crate::update::TARGET)?;
    verify_file(&dir.join(format!("package.{}", artifact.kind)), &artifact)?;
    Ok(artifact)
}

/// What an approved update installs from.
#[derive(Debug)]
enum Staged {
    /// The platform's full installer, checked against the signed manifest.
    Package(ReleaseArtifact),
    /// The app's files (S164): the list, checked against the signed
    /// manifest. Each file is checked again as the new bundle is built.
    #[cfg(unix)]
    Files(uxduck_contracts::dto::release::BundleFiles),
}

impl Staged {
    fn kind(&self) -> &str {
        match self {
            Self::Package(artifact) => &artifact.kind,
            #[cfg(unix)]
            Self::Files(_) => uxduck_contracts::dto::release::BUNDLE_FILES_KIND,
        }
    }
}

/// The per-file update when one was staged and has not failed, otherwise
/// the full installer.
fn verified_staged(dir: &Path, key: &str) -> Result<Staged, String> {
    #[cfg(unix)]
    if dir.join(files::LIST).is_file() && !dir.join(FILES_FAILED).exists() {
        let report = verified_report(dir, key)?;
        let artifact = files_artifact(&report, crate::update::TARGET)
            .ok_or("The release no longer lists the update's files.")?;
        let path = dir.join(files::LIST);
        if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > files::MAX_LIST {
            return Err("The update's file list is too large.".into());
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        return files::read(
            &bytes,
            &artifact,
            &report.manifest.version,
            crate::update::TARGET,
            allowed_url,
        )
        .map(Staged::Files);
    }
    verified_artifact(dir, key).map(Staged::Package)
}

fn prepare_worker(
    dir: &Path,
    data_dir: &Path,
    port: u16,
    reopen: bool,
) -> Result<(Child, std::process::ChildStdin), String> {
    let staged = verified_staged(dir, &key().ok_or("This build has no release key")?)?;
    match &staged {
        Staged::Package(artifact) => {
            installer::preflight(&dir.join(format!("package.{}", artifact.kind)))?;
        }
        // The installed app may have changed since the download: build and
        // check the new bundle again before anything stops.
        #[cfg(unix)]
        Staged::Files(list) => {
            let version = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let installed = Path::new(installer::INSTALLED_APP);
            if let Err(why) = installer::preflight_files(dir, list, installed, version) {
                let _ = write(&dir.join(FILES_FAILED), why.as_bytes());
                return Err(format!(
                    "UXDuck could not prepare the smaller update ({why}). It is downloading the complete installer; install again when it is ready."
                ));
            }
        }
    }
    let executable = std::fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let plan = Plan {
        version: dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid update folder")?
            .into(),
        kind: staged.kind().to_owned(),
        executable: executable.clone(),
        installed_executable: Some(installer::installed_executable()?),
        data_dir: data_dir.into(),
        port,
        reopen,
    };
    spawn_worker(dir, &plan)
}

fn spawn_worker(dir: &Path, plan: &Plan) -> Result<(Child, std::process::ChildStdin), String> {
    write(
        &dir.join("plan.json"),
        &serde_json::to_vec(&plan).map_err(|e| e.to_string())?,
    )?;
    let copy = dir.join(if cfg!(windows) {
        "uxduck-updater.exe"
    } else {
        "uxduck-updater"
    });
    std::fs::copy(&plan.executable, &copy)
        .map_err(|e| format!("Couldn't prepare the installer helper: {e}"))?;
    let mut command = Command::new(copy);
    command
        .arg("__install-update")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or("The installer helper has no readiness pipe")?;
    let (ready, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line);
        let _ = ready.send(result.is_ok() && line == "ready\n");
    });
    if receive.recv_timeout(Duration::from_secs(10)) != Ok(true) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("The installer helper could not start. UXDuck has kept running.".into());
    }
    let pipe = child
        .stdin
        .take()
        .ok_or("The installer helper has no permission pipe")?;
    Ok((child, pipe))
}

/// Runs in a copied executable, surviving replacement of the entire app bundle.
/// EOF or a missing permission byte never installs anything.
///
/// # Errors
/// A missing/invalid staged installer, permission/installer failure, or failed restart.
#[allow(
    clippy::too_many_lines,
    reason = "one ordered installer lifecycle with unconditional restart after installation"
)]
pub fn worker(dir: &Path) -> Result<(), String> {
    let plan: Plan =
        serde_json::from_slice(&std::fs::read(dir.join("plan.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let installing = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(plan.data_dir.join("app-install.lock"))
        .map_err(|e| e.to_string())?;
    installing
        .try_lock()
        .map_err(|_| "Another UXDuck update is already installing")?;
    if plan.kind == "sparkle" {
        let _ = std::fs::remove_file(plan.data_dir.join("sparkle-installed"));
        let _ = std::fs::remove_file(plan.data_dir.join("sparkle-failed"));
    }
    println!("ready");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut permission = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut permission)
        .map_err(|e| e.to_string())?;
    if permission != "install\n" {
        return Ok(());
    }
    // Wait for process exit, not merely its checkpoint/lock release.
    std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink()).map_err(|e| e.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(plan.data_dir.join("daemon.lock"))
        .map_err(|e| e.to_string())?;
    let until = std::time::Instant::now() + Duration::from_secs(60);
    while lock.try_lock().is_err() {
        if std::time::Instant::now() > until {
            return Err("UXDuck did not shut down. The update was not installed.".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Holding this lock prevents startup (including installer postinstall hooks)
    // from acquiring vehicles or recordings until the installer has finished.
    let installed = plan
        .installed_executable
        .as_ref()
        .unwrap_or(&plan.executable);
    let result: Result<(), String> = (|| {
        if plan.kind == "sparkle" {
            wait_for_sparkle(&plan)?;
        } else {
            let staged = verified_staged(dir, &key().ok_or("This build has no release key")?)?;
            if plan.version != dir.file_name().and_then(|s| s.to_str()).unwrap_or("")
                || plan.kind != staged.kind()
            {
                return Err("The approved installer changed.".into());
            }
            match staged {
                Staged::Package(artifact) => {
                    let package = dir.join(format!("package.{}", artifact.kind));
                    installer::preflight(&package)?;
                    installer::install(&package, &artifact.kind, &plan.version)?;
                }
                // A failure leaves the previous app in place; the next attempt
                // downloads this version's full package.
                #[cfg(unix)]
                Staged::Files(list) => {
                    installer::install_files(dir, &list, &plan.version).inspect_err(|why| {
                        let _ = write(&dir.join(FILES_FAILED), why.as_bytes());
                    })?;
                }
            }
        }
        let version = Command::new(installed)
            .arg("--version")
            .output()
            .map_err(|e| e.to_string())?;
        if !version.status.success()
            || String::from_utf8_lossy(&version.stdout)
                .split_whitespace()
                .nth(1)
                != Some(plan.version.as_str())
        {
            return Err("Installation was cancelled or did not finish. UXDuck is still using its previous version.".into());
        }
        Ok(())
    })();
    let message = result.as_ref().map_or_else(
        |e| format!("Update not installed: {e}"),
        |()| format!("Updated UXDuck to {}.", plan.version),
    );
    if let Err(error) = write(&plan.data_dir.join("app-update-result"), message.as_bytes()) {
        eprintln!("Could not record the update result: {error}");
    }
    drop(lock);
    drop(installing);
    // Restart as the original user, never as the privileged installer account.
    let restart = if result.is_ok() {
        installed
    } else {
        &plan.executable
    };
    let restarted = Command::new(restart)
        .arg("--data-dir")
        .arg(&plan.data_dir)
        .arg("daemon")
        .arg("start")
        .env("UXDUCK_PORT", plan.port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Couldn't restart UXDuck: {e}"))?;
    if !restarted.success() {
        return Err(
            "The installer finished but UXDuck could not restart. Open UXDuck again.".into(),
        );
    }
    if plan.reopen {
        let _ = Command::new(restart)
            .arg("--data-dir")
            .arg(&plan.data_dir)
            .arg("open")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    result
}

/// A stale lock file is harmless. A live installer excludes fresh daemon starts.
///
/// # Errors
/// A live installer holds the lease, or the private directory is not writable.
pub fn check_startup(data_dir: &Path) -> std::io::Result<()> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("app-install.lock"))?;
    lock.try_lock().map_err(|_| std::io::Error::other("UXDuck is installing an update. Wait for installation to finish, then open UXDuck again."))
}

/// The standard Sparkle installer performs replacement and relaunches its helper.
/// The copied guardian keeps UXDuck's lease until that helper reports completion.
fn wait_for_sparkle(plan: &Plan) -> Result<(), String> {
    let marker = plan.data_dir.join("sparkle-installed");
    // Do not reopen while Sparkle may still be installing or awaiting native
    // authorization. Only its explicit success/failure releases the lease.
    loop {
        if let Ok(error) = std::fs::read_to_string(plan.data_dir.join("sparkle-failed")) {
            return Err(error);
        }
        if std::fs::read_to_string(&marker).is_ok_and(|version| version == plan.version) {
            let _ = std::fs::remove_file(marker);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

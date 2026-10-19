//! Background product downloads and the consented installer handoff. Electron
//! is never involved. The signed installer is checked both before and after
//! shutdown; a copied worker holds the daemon lock until installation ends.
mod installer;
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

const MAX_PACKAGE: u64 = 1 << 30;
const MAX_MANIFEST: u64 = 4 << 20;

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
                match download(&engine, &data_dir, &url, &key).await {
                    Ok(path) => ready = path,
                    Err(why) => {
                        ready = None;
                        publish_failure(&engine, why).await;
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
                Err(why) => publish_failure(&engine, why).await,
            }
        }
    });
}

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
    if !(artifact.url.starts_with("https://")
        || (cfg!(debug_assertions) || crate::update::IS_UPDATE_TEST)
            && artifact.url.starts_with("http://127.0.0.1:"))
    {
        return Err("The installer download must use HTTPS.".into());
    }
    Ok(artifact)
}

async fn download(
    engine: &EngineHandle,
    data_dir: &Path,
    url: &str,
    key: &str,
) -> Result<Option<PathBuf>, String> {
    let report = crate::update::check(url, Some(key), data_dir)
        .await
        .map_err(|e| e.to_string())?;
    if !matches!(
        report.verdict,
        crate::update::Verdict::Available(_) | crate::update::Verdict::RolledBack(_)
    ) {
        engine
            .run(|e| e.app_update_publish(status(UpdatePhase::Current)))
            .await
            .map_err(|e| e.to_string())?;
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
    s.version = Some(version);
    s.size = Some(artifact.size);
    s.notes_url = report
        .manifest
        .notes_url
        .filter(|url| url.starts_with("https://"));
    let initial = s.clone();
    engine
        .run(move |e| e.app_update_publish(initial))
        .await
        .map_err(|e| e.to_string())?;
    if verify_file_async(&package, &artifact).await.is_err() {
        let partial = dir.join("package.partial");
        let fetched = download_file(engine, &artifact, &partial, &s).await;
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
    let metadata_dir = dir.clone();
    tokio::task::spawn_blocking(move || {
        write(&metadata_dir.join("manifest.json"), &report.signed_bytes)?;
        write(
            &metadata_dir.join("manifest.minisig"),
            report.signature.as_bytes(),
        )
    })
    .await
    .map_err(|e| e.to_string())??;
    s.phase = UpdatePhase::Ready;
    s.received = Some(artifact.size);
    // Surface a cancelled/failed installation when the background process returns.
    if let Ok(result) = std::fs::read_to_string(data_dir.join("app-update-result")) {
        s.detail = Some(result);
    }
    engine
        .run(move |e| e.app_update_publish(s))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(dir))
}

async fn download_file(
    engine: &EngineHandle,
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
            engine
                .run(move |e| e.app_update_publish(s))
                .await
                .map_err(|e| e.to_string())?;
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

fn verified_artifact(dir: &Path, key: &str) -> Result<ReleaseArtifact, String> {
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
    let report = crate::update::compare(env!("CARGO_PKG_VERSION"), manifest, crate::update::TARGET)
        .map_err(|e| e.to_string())?;
    let artifact = installer_artifact(&report, crate::update::TARGET)?;
    verify_file(&dir.join(format!("package.{}", artifact.kind)), &artifact)?;
    Ok(artifact)
}

fn prepare_worker(
    dir: &Path,
    data_dir: &Path,
    port: u16,
    reopen: bool,
) -> Result<(Child, std::process::ChildStdin), String> {
    let artifact = verified_artifact(dir, &key().ok_or("This build has no release key")?)?;
    installer::preflight(&dir.join(format!("package.{}", artifact.kind)))?;
    let executable = std::fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let plan = Plan {
        version: dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("Invalid update folder")?
            .into(),
        kind: artifact.kind,
        executable: executable.clone(),
        installed_executable: Some(installer::installed_executable()?),
        data_dir: data_dir.into(),
        port,
        reopen,
    };
    write(
        &dir.join("plan.json"),
        &serde_json::to_vec(&plan).map_err(|e| e.to_string())?,
    )?;
    let copy = dir.join(if cfg!(windows) {
        "uxduck-updater.exe"
    } else {
        "uxduck-updater"
    });
    std::fs::copy(executable, &copy)
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
        let artifact = verified_artifact(dir, &key().ok_or("This build has no release key")?)?;
        if plan.version != dir.file_name().and_then(|s| s.to_str()).unwrap_or("")
            || plan.kind != artifact.kind
        {
            return Err("The approved installer changed.".into());
        }
        let package = dir.join(format!("package.{}", artifact.kind));
        installer::preflight(&package)?;
        installer::install(&package, &artifact.kind, &plan.version)?;
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

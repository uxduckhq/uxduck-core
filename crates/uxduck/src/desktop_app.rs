//! The desktop window: shipped with UXDuck, loaded only on an explicit
//! app/window action. Signed runtime updates may also be kept in the data
//! folder. The daemon and menu-bar helper run independently of Electron.

mod startup;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use sha2::{Digest, Sha256};
use uxduck_contracts::dto::app::{DesktopAppState, DesktopAppStatus};
use uxduck_contracts::dto::release::{DesktopAppArtifact, ReleaseManifest};

/// The largest archive accepted, whatever the manifest says.
const MAX_ARCHIVE: u64 = 1 << 30;
/// Written last into a version's folder: it is complete and checked.
const INSTALLED: &str = "installed.json";

/// The executable inside an unpacked desktop app.
#[must_use]
pub fn executable(version_dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        version_dir.join("UXDuck.app/Contents/MacOS/UXDuck")
    } else if cfg!(windows) {
        version_dir.join("UXDuck.exe")
    } else {
        version_dir.join("uxduck-desktop")
    }
}

#[derive(Clone, Debug, Default)]
enum Fetch {
    #[default]
    NotYet,
    Loading,
    /// The version and archive offered for this computer, if any.
    Ready(Option<(String, DesktopAppArtifact)>),
    Failed(String),
}

#[derive(Debug, Default)]
struct State {
    fetch: Fetch,
    /// Bytes received while downloading.
    downloading: Option<u64>,
    /// Why the last download or check failed.
    failed: Option<String>,
}

#[derive(Debug)]
struct DesktopChild {
    process: Child,
    starting: bool,
}

#[derive(Debug)]
pub struct DesktopApp {
    /// The release manifest, or none: downloads are off.
    url: Option<String>,
    key: Option<String>,
    target: String,
    dir: PathBuf,
    /// Shipped with the installer; never downloaded or started implicitly.
    bundled_dir: PathBuf,
    data_dir: PathBuf,
    runtime: tokio::runtime::Handle,
    state: Mutex<State>,
    /// Started desktop apps: their standard input closes when UXDuck
    /// stops, and they quit with it.
    children: Mutex<Vec<DesktopChild>>,
}

impl DesktopApp {
    /// `UXDUCK_UPDATE_URL` points the manifest elsewhere and `none` turns
    /// downloads off. The key is the release key built into this binary; a
    /// development build takes `UXDUCK_DESKTOP_APP_KEY` instead, for tests.
    #[must_use]
    pub fn new(data_dir: &Path, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let url = match std::env::var("UXDUCK_UPDATE_URL") {
            Ok(url) if url == "none" => None,
            Ok(url) => Some(url),
            Err(_) => Some(crate::update::DEFAULT_URL.to_owned()),
        };
        let key = std::env::var("UXDUCK_DESKTOP_APP_KEY")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .or_else(|| crate::update::APP_UPDATE_PUBLIC_KEY.map(str::to_owned));
        Arc::new(Self {
            url,
            key,
            target: crate::update::TARGET.to_owned(),
            dir: data_dir.join("desktop-app"),
            bundled_dir: bundled_directory(&std::env::current_exe().unwrap_or_default()),
            data_dir: data_dir.to_owned(),
            runtime,
            state: Mutex::new(State::default()),
            children: Mutex::new(Vec::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The newest complete version on this computer, and its folder.
    fn installed(&self) -> Option<(String, PathBuf)> {
        std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join(INSTALLED).is_file() && executable(path).is_file())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?.to_owned();
                Some((semver::Version::parse(&name).ok()?, name, path))
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, name, path)| (name, path))
    }

    fn available_installation(&self) -> Option<(String, PathBuf)> {
        let bundled = std::fs::read(self.bundled_dir.join(INSTALLED))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["version"].as_str().map(str::to_owned))
            .filter(|_| executable(&self.bundled_dir).is_file())
            .map(|version| (version, self.bundled_dir.clone()));
        [self.installed(), bundled]
            .into_iter()
            .flatten()
            .filter_map(|(version, folder)| {
                Some((semver::Version::parse(&version).ok()?, version, folder))
            })
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, version, folder)| (version, folder))
    }

    /// Where it stands; asks the release channel the first time, and again
    /// when `refresh`. The engine fills in `chosen`.
    pub fn status(self: &Arc<Self>, refresh: bool) -> DesktopAppStatus {
        let fetch_now = {
            let state = self.lock();
            self.url.is_some()
                && (matches!(state.fetch, Fetch::NotYet)
                    || refresh && !matches!(state.fetch, Fetch::Loading))
        };
        if fetch_now {
            self.fetch();
        }
        let installed = self.available_installation();
        let state = self.lock();
        let offered = match &state.fetch {
            Fetch::Ready(offered) => offered.clone(),
            _ => None,
        };
        let (kind, detail) = if let Some(received) = state.downloading {
            let _ = received;
            (DesktopAppState::Downloading, None)
        } else if installed.is_some() {
            (DesktopAppState::Installed, None)
        } else if let Some(why) = &state.failed {
            (DesktopAppState::Failed, Some(why.clone()))
        } else {
            match &state.fetch {
                _ if self.url.is_none() => (
                    DesktopAppState::Unavailable,
                    Some("Downloads are turned off for this UXDuck.".to_owned()),
                ),
                Fetch::NotYet | Fetch::Loading => (DesktopAppState::Checking, None),
                Fetch::Ready(Some(_)) => (DesktopAppState::Available, None),
                Fetch::Ready(None) => (
                    DesktopAppState::Unavailable,
                    Some("There is no desktop app for this computer yet.".to_owned()),
                ),
                Fetch::Failed(why) => (DesktopAppState::Failed, Some(why.clone())),
            }
        };
        DesktopAppStatus {
            state: kind,
            chosen: false,
            version: offered.as_ref().map(|(version, _)| version.clone()),
            size: offered.as_ref().map(|(_, artifact)| artifact.size),
            unpacked_size: offered.as_ref().map(|(_, artifact)| artifact.unpacked_size),
            received: state.downloading,
            installed_version: installed.map(|(version, _)| version),
            detail,
        }
    }

    fn fetch(self: &Arc<Self>) {
        let (Some(url), key) = (self.url.clone(), self.key.clone()) else {
            return;
        };
        self.lock().fetch = Fetch::Loading;
        let this = self.clone();
        self.runtime.spawn(async move {
            let fetched = manifest(&url, key.as_deref()).await;
            this.lock().fetch = match fetched {
                Ok(manifest) => Fetch::Ready(offered(&manifest, &this.target)),
                Err(why) => {
                    tracing::warn!(%why, "could not check for the desktop app");
                    Fetch::Failed(format!("Couldn't check for the desktop app: {why}"))
                }
            };
        });
    }

    /// Starts downloading the version offered for this computer.
    ///
    /// # Errors
    /// When none is offered, or one is downloading already.
    pub fn download(self: &Arc<Self>) -> Result<(), String> {
        let (version, artifact) = {
            let mut state = self.lock();
            if state.downloading.is_some() {
                return Err("The desktop app is downloading already.".into());
            }
            let Fetch::Ready(Some(offered)) = &state.fetch else {
                return Err("There is no desktop app to download for this computer.".into());
            };
            let offered = offered.clone();
            state.downloading = Some(0);
            state.failed = None;
            offered
        };
        let this = self.clone();
        self.runtime.spawn(async move {
            let result = this.fetch_archive(&version, &artifact).await;
            let result = match result {
                Ok(archive) => {
                    let this = this.clone();
                    tokio::task::spawn_blocking(move || {
                        let done = this.install(&archive, &version, &artifact);
                        let _ = std::fs::remove_file(&archive);
                        done
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                }
                Err(why) => Err(why),
            };
            let mut state = this.lock();
            state.downloading = None;
            match result {
                Ok(path) => tracing::info!(path = %path.display(), "installed the desktop app"),
                Err(why) => {
                    tracing::warn!(%why, "the desktop app was not installed");
                    state.failed = Some(why);
                }
            }
        });
        Ok(())
    }

    async fn fetch_archive(
        &self,
        version: &str,
        artifact: &DesktopAppArtifact,
    ) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let archive = self.dir.join(format!("{version}.download"));
        let client = crate::sim::catalogue::client()?;
        let mut response = client
            .get(&artifact.url)
            .send()
            .await
            .map_err(|e| format!("Couldn't download the desktop app: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Couldn't download the desktop app: {}",
                response.status()
            ));
        }
        let mut out = std::fs::File::create(&archive).map_err(|e| e.to_string())?;
        let mut have = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("The download broke off: {e}"))?
        {
            have += chunk.len() as u64;
            if have > artifact.size.min(MAX_ARCHIVE) {
                drop(out);
                let _ = std::fs::remove_file(&archive);
                return Err(
                    "The download is larger than the release says, so it was deleted.".into(),
                );
            }
            out.write_all(&chunk).map_err(|e| e.to_string())?;
            self.lock().downloading = Some(have);
        }
        out.flush().map_err(|e| e.to_string())?;
        Ok(archive)
    }

    /// Checks a downloaded archive against the manifest, unpacks it into
    /// its version's folder and checks what is inside; then removes older
    /// versions. Anything that fails is deleted.
    ///
    /// # Errors
    /// Why, in words for Settings.
    pub fn install(
        &self,
        archive: &Path,
        version: &str,
        artifact: &DesktopAppArtifact,
    ) -> Result<PathBuf, String> {
        if semver::Version::parse(version).is_err() {
            return Err(format!("{version} is not a desktop app version."));
        }
        let size = std::fs::metadata(archive).map_err(|e| e.to_string())?.len();
        if size != artifact.size {
            return Err("The download is not the size the release says, so it was deleted.".into());
        }
        if sha256(archive)? != artifact.sha256.to_ascii_lowercase() {
            return Err(
                "The download does not match the release's checksum, so it was deleted.".into(),
            );
        }
        let folder = self.dir.join(version);
        let partial = self.dir.join(format!("{version}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
        let checked = unpack(archive, &partial, &artifact.url).and_then(|()| {
            flatten(&partial)?;
            let exe = executable(&partial);
            if !exe.is_file() {
                return Err("The download has no desktop app in it, so it was deleted.".into());
            }
            check_signature(&exe)
        });
        if let Err(why) = checked {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(why);
        }
        std::fs::write(
            partial.join(INSTALLED),
            format!("{{\"version\":\"{version}\"}}\n"),
        )
        .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::rename(&partial, &folder).map_err(|e| e.to_string())?;
        // Older versions go; one still running is left for next time.
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path != folder && path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            }
        }
        Ok(executable(&folder))
    }

    /// The installed desktop app's executable, checked.
    ///
    /// # Errors
    /// When none is installed, or it fails its checks (it is then deleted).
    pub fn path(&self) -> Result<String, String> {
        let (_, folder) = self.available_installation().ok_or(
            "UXDuck's desktop window is missing. Reinstall UXDuck, or run `uxduck open --browser`. For a development window, package apps/desktop first."
        )?;
        let exe = executable(&folder);
        if let Err(why) = check_signature(&exe) {
            if folder != self.bundled_dir {
                let _ = std::fs::remove_dir_all(&folder);
            }
            self.lock().failed = Some(why.clone());
            return Err(why);
        }
        Ok(exe.display().to_string())
    }

    /// Starts the desktop app as UXDuck's child, after checking it again.
    /// It finds UXDuck through `UXDUCK_DATA_DIR`.
    ///
    /// # Errors
    /// When it is not UXDuck's desktop app, fails its checks, or does not
    /// start.
    #[allow(
        clippy::too_many_lines,
        reason = "ordered signature, child supervision and startup receipt lifecycle"
    )]
    pub fn open(self: &Arc<Self>, path: &str, args: &[String]) -> Result<(), String> {
        let path = Path::new(path);
        let inside = std::fs::canonicalize(path).is_ok_and(|exe| {
            std::fs::canonicalize(executable(&self.bundled_dir))
                .ok()
                .as_ref()
                == Some(&exe)
                || std::fs::canonicalize(&self.dir).is_ok_and(|dir| exe.starts_with(dir))
        });
        if !inside {
            return Err(
                "It is not the desktop window shipped with UXDuck or an installed update.".into(),
            );
        }
        if let Err(why) = check_signature(path) {
            if let Some((_, folder)) = self.installed() {
                let _ = std::fs::remove_dir_all(folder);
            }
            self.lock().failed = Some(why.clone());
            return Err(why);
        }
        // Keep Windows executable and profile paths in the same plain form
        // as a direct launch, including a caller's relative data directory.
        let executable = dunce::canonicalize(path)
            .map_err(|e| format!("Could not resolve the desktop app path: {e}"))?;
        let data_dir = dunce::canonicalize(&self.data_dir)
            .map_err(|e| format!("Could not resolve UXDuck's data folder: {e}"))?;
        let startup = tempfile::Builder::new()
            .prefix("desktop-startup-")
            .tempdir_in(&data_dir)
            .map_err(|e| format!("Could not create the desktop startup receipt: {e}"))?;
        let receipt = startup.path().join("result.json");
        let mut command = Command::new(&executable);
        // Start from the runtime's folder, as a direct Explorer launch does;
        // do not inherit a launcher's working directory or verbatim data path.
        command
            .current_dir(
                executable
                    .parent()
                    .ok_or("The desktop app has no parent folder.")?,
            )
            .arg("--quit-with-uxduck")
            .args(args)
            .env("UXDUCK_DATA_DIR", data_dir)
            .env("UXDUCK_DESKTOP_STARTUP_FILE", &receipt)
            .env("ELECTRON_ENABLE_LOGGING", "1");
        // A development UXDuck's window keeps Reload and the developer tools
        // even when `pnpm desktop:local` packaged it.
        if cfg!(debug_assertions) {
            command.env("UXDUCK_DEVELOPMENT", "1");
        }
        // Keep Chromium's own errors: discarding stderr left only the
        // generic ERR_FAILED dialog when the Windows interface did not load.
        let log_path = self.data_dir.join("desktop-app.log");
        if std::fs::metadata(&log_path).is_ok_and(|meta| meta.len() > 1024 * 1024) {
            let previous = self.data_dir.join("desktop-app.previous.log");
            if previous.exists() {
                std::fs::remove_file(&previous).map_err(|e| {
                    format!("Could not remove the previous desktop startup log: {e}")
                })?;
            }
            std::fs::rename(&log_path, previous)
                .map_err(|e| format!("Could not rotate the desktop startup log: {e}"))?;
        }
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| {
                format!(
                    "Could not open the desktop startup log {}: {e}",
                    log_path.display()
                )
            })?;
        writeln!(
            log,
            "\nDesktop startup: {} ({})",
            executable.display(),
            jiff::Timestamp::now()
        )
        .map_err(|e| format!("Could not write the desktop startup log: {e}"))?;
        let errors = log
            .try_clone()
            .map_err(|e| format!("Could not share the desktop startup log: {e}"))?;
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors))
            .spawn()
            .map_err(|e| {
                format!(
                    "The desktop app did not start: {e}. Startup log: {}",
                    log_path.display()
                )
            })?;
        let mut children = self.children.lock().unwrap_or_else(PoisonError::into_inner);
        children.retain_mut(|child| child.starting || matches!(child.process.try_wait(), Ok(None)));
        let pid = child.id();
        children.push(DesktopChild {
            process: child,
            starting: true,
        });
        drop(children);
        let this = Arc::clone(self);
        this.runtime.clone().spawn(async move {
            let result = startup::wait(
                &receipt,
                || {
                    let mut children = this.children.lock().unwrap_or_else(PoisonError::into_inner);
                    children.iter_mut().find(|child| child.process.id() == pid)
                        .map_or(Ok(None), |child| child.process.try_wait().map_err(|e| e.to_string()))
                },
                std::time::Duration::from_secs(30),
            ).await;
            if result.is_ok() {
                let mut children = this.children.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(child) = children.iter_mut().find(|child| child.process.id() == pid) {
                    child.starting = false;
                }
            }
            if let Err(why) = result {
                // Removing the runtime deliberately cancels pending launches.
                if !this.children.lock().unwrap_or_else(PoisonError::into_inner)
                    .iter().any(|child| child.process.id() == pid) {
                    return;
                }
                {
                    let mut children = this.children.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(child) = children.iter_mut().find(|child| child.process.id() == pid) {
                        let _ = child.process.kill();
                        let _ = child.process.wait();
                        child.starting = false;
                    }
                }
                let message = format!("{why}\n\nStartup log: {}\n\nYou can open UXDuck in your browser with uxduck open --browser.", log_path.display());
                this.lock().failed = Some(message.clone());
                tracing::error!(%message, "desktop startup failed");
                if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                    let _ = writeln!(log, "{message}");
                }
                if let Some(helper) = crate::tray::binary() {
                    let result = tokio::process::Command::new(helper)
                        .arg("--window-error").arg(&message)
                        .env("UXDUCK_DATA_DIR", &this.data_dir)
                        .stdin(Stdio::null()).output().await;
                    if !result.is_ok_and(|output| output.status.success()) {
                        tracing::error!("could not show the desktop startup error dialog");
                    }
                } else {
                    tracing::error!("the menu-bar helper is missing; could not show the desktop startup error dialog");
                }
            }
            drop(startup);
        });
        Ok(())
    }

    /// Deletes the desktop app; a running one is asked to quit first.
    ///
    /// # Errors
    /// When its folder cannot be deleted.
    pub fn remove(&self) -> Result<(), String> {
        let children: Vec<DesktopChild> =
            std::mem::take(&mut *self.children.lock().unwrap_or_else(PoisonError::into_inner));
        for child in children {
            let mut child = child.process;
            // Its standard input closing tells it to quit.
            drop(child.stdin.take());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = child.kill();
        }
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir)
                .map_err(|e| format!("Couldn't delete the desktop app: {e}"))?;
        }
        // Its own profile: its window sizes and the interface's storage there.
        let _ = std::fs::remove_dir_all(self.data_dir.join("desktop-app-profile"));
        self.lock().failed = None;
        Ok(())
    }
}

/// Installers and portable archives use the same runtime folder name. A
/// UXDuck started from a `\\?\` path on Windows still finds a plain one.
fn bundled_directory(daemon: &Path) -> PathBuf {
    let parent = dunce::simplified(daemon)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    if cfg!(target_os = "macos") && parent.ends_with("Contents/MacOS") {
        return parent.join("../Resources/desktop-app");
    }
    if cfg!(target_os = "linux") && parent == Path::new("/usr/bin") {
        return PathBuf::from("/usr/lib/uxduck/desktop-app");
    }
    parent.join("desktop-app")
}

/// The version and archive the manifest offers for this computer.
#[must_use]
pub fn offered(manifest: &ReleaseManifest, target: &str) -> Option<(String, DesktopAppArtifact)> {
    let release = manifest.desktop_app.as_ref()?;
    release
        .artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .or_else(|| {
            release.artifacts.iter().find(|artifact| {
                target.ends_with("apple-darwin") && artifact.target == "universal-apple-darwin"
            })
        })
        .map(|artifact| (release.version.clone(), artifact.clone()))
}

/// The release manifest, checked against the release key.
async fn manifest(url: &str, key: Option<&str>) -> Result<ReleaseManifest, String> {
    let key = key.ok_or("this build has no release key")?;
    let client = crate::sim::catalogue::client()?;
    let get = |url: String| {
        let client = client.clone();
        async move {
            let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(response.status().to_string());
            }
            response.bytes().await.map_err(|e| e.to_string())
        }
    };
    let bytes = get(url.to_owned()).await?;
    let signature = get(format!("{url}.minisig")).await?;
    crate::update::verify(&bytes, &String::from_utf8_lossy(&signature), key)
        .map_err(|e| e.to_string())
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

/// Unpacks with the system's own tools: `ditto` keeps a Mac app's links
/// and signatures intact; `tar` reads both zip and tar.gz elsewhere
/// (Windows 10 and later have it).
fn unpack(archive: &Path, into: &Path, url: &str) -> Result<(), String> {
    let zip = Path::new(url)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
    let mut command = if cfg!(target_os = "macos") && zip {
        let mut command = Command::new("ditto");
        command.args(["-x", "-k"]).arg(archive).arg(into);
        command
    } else {
        let mut command = Command::new("tar");
        command
            .arg(if zip { "-xf" } else { "-xzf" })
            .arg(archive)
            .arg("-C")
            .arg(into);
        command
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't unpack the desktop app: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Couldn't unpack the desktop app: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// An archive whose files sit in one folder of their own: moved up a level.
fn flatten(dir: &Path) -> Result<(), String> {
    if executable(dir).is_file() {
        return Ok(());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    if let [only] = entries.as_slice()
        && only.is_dir()
        && executable(only).is_file()
    {
        for entry in std::fs::read_dir(only)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            std::fs::rename(entry.path(), dir.join(entry.file_name()))
                .map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir(only);
    }
    Ok(())
}

/// The developer (Team ID) a Mac executable or app is signed by, if any.
#[cfg(target_os = "macos")]
fn team(path: &Path) -> Option<String> {
    let output = Command::new("codesign")
        .args(["-dv", "--verbose=2"])
        .arg(path)
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::to_owned)
        .filter(|team| team != "not set")
}

/// On macOS the desktop app must be validly signed by the developer who
/// signed this UXDuck. A UXDuck that is not signed (built from source)
/// checks nothing, since there is no developer to compare with.
#[cfg(target_os = "macos")]
fn check_signature(exe: &Path) -> Result<(), String> {
    let own = std::env::current_exe().ok().and_then(|exe| team(&exe));
    let Some(own) = own else {
        return Ok(());
    };
    let app = exe
        .ancestors()
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
        .unwrap_or(exe);
    let valid = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .is_ok_and(|output| output.status.success());
    if valid && team(app).as_deref() == Some(own.as_str()) {
        Ok(())
    } else {
        Err("The desktop app is not signed by UXDuck's developer. Reinstall UXDuck.".into())
    }
}

/// Windows builds are not signed yet (ADR-0025 D7), nor Linux ones.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
fn check_signature(_exe: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop app archive with an executable where this platform's is.
    fn archive(dir: &Path) -> (PathBuf, DesktopAppArtifact) {
        let src = dir.join("src");
        let exe = executable(&src.join("UXDuck Desktop"));
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        let archive = dir.join("app.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("UXDuck Desktop")
            .status()
            .unwrap();
        assert!(status.success());
        let (sha256, size) = (
            sha256(&archive).unwrap(),
            std::fs::metadata(&archive).unwrap().len(),
        );
        let artifact = DesktopAppArtifact {
            target: "test".into(),
            url: "https://example.test/uxduck-desktop-app-0.2.0-test.tar.gz".into(),
            sha256,
            size,
            unpacked_size: 10,
        };
        (archive, artifact)
    }

    fn app(data: &Path) -> Arc<DesktopApp> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let app = DesktopApp::new(data, runtime.handle().clone());
        std::mem::forget(runtime);
        app
    }

    #[test]
    fn bundled_runtime_is_available_without_a_download_and_newer_updates_win() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(&tmp.path().join("data"));
        Arc::get_mut(&mut app).unwrap().bundled_dir = tmp.path().join("bundled");
        let write_runtime = |dir: &Path, version: &str| {
            let exe = executable(dir);
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
            std::fs::write(
                dir.join(INSTALLED),
                format!("{{\"version\":\"{version}\"}}"),
            )
            .unwrap();
        };
        write_runtime(&app.bundled_dir, "0.2.0");
        assert_eq!(app.available_installation().unwrap().0, "0.2.0");
        assert_eq!(
            app.path().unwrap(),
            executable(&app.bundled_dir).display().to_string()
        );
        assert!(
            app.children.lock().unwrap().is_empty(),
            "finding a runtime never starts it"
        );
        write_runtime(&app.dir.join("0.1.0"), "0.1.0");
        assert_eq!(app.available_installation().unwrap().0, "0.2.0");
        write_runtime(&app.dir.join("0.3.0"), "0.3.0");
        assert_eq!(app.available_installation().unwrap().0, "0.3.0");
        app.remove().unwrap();
        assert_eq!(app.available_installation().unwrap().0, "0.2.0");
        assert!(
            executable(&app.bundled_dir).is_file(),
            "the installer owns the bundled runtime"
        );
    }

    #[test]
    fn a_missing_runtime_has_an_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(tmp.path());
        Arc::get_mut(&mut app).unwrap().bundled_dir = tmp.path().join("missing");
        let error = app.path().unwrap_err();
        assert!(error.contains("Reinstall UXDuck"));
        assert!(error.contains("open --browser"));
    }

    #[test]
    fn a_checked_archive_is_installed_and_older_versions_go() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(&tmp.path().join("data"));
        let (archive, artifact) = archive(tmp.path());
        std::fs::create_dir_all(app.dir.join("0.1.0")).unwrap();
        let exe = app.install(&archive, "0.2.0", &artifact).unwrap();
        assert_eq!(exe, executable(&app.dir.join("0.2.0")));
        assert!(exe.is_file());
        assert!(!app.dir.join("0.1.0").exists());
        assert_eq!(app.installed().unwrap().0, "0.2.0");
        assert_eq!(app.path().unwrap(), exe.display().to_string());
    }

    #[test]
    fn an_archive_of_the_wrong_size_or_checksum_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(&tmp.path().join("data"));
        let (archive, artifact) = archive(tmp.path());
        let bigger = DesktopAppArtifact {
            size: artifact.size + 1,
            ..artifact.clone()
        };
        assert!(
            app.install(&archive, "0.2.0", &bigger)
                .unwrap_err()
                .contains("size")
        );
        let other = DesktopAppArtifact {
            sha256: "0".repeat(64),
            ..artifact.clone()
        };
        assert!(
            app.install(&archive, "0.2.0", &other)
                .unwrap_err()
                .contains("checksum")
        );
        assert!(app.installed().is_none());
        assert!(!app.dir.join("0.2.0.partial").exists());
    }

    #[test]
    fn an_archive_without_the_app_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(&tmp.path().join("data"));
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(empty.join("nothing")).unwrap();
        let archive = tmp.path().join("empty.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&empty)
                .arg("nothing")
                .status()
                .unwrap()
                .success()
        );
        let artifact = DesktopAppArtifact {
            target: "test".into(),
            url: "https://example.test/x.tar.gz".into(),
            sha256: sha256(&archive).unwrap(),
            size: std::fs::metadata(&archive).unwrap().len(),
            unpacked_size: 1,
        };
        assert!(
            app.install(&archive, "0.2.0", &artifact)
                .unwrap_err()
                .contains("no desktop app")
        );
        assert!(app.installed().is_none());
    }

    #[test]
    fn the_same_version_is_kept_across_uxduck_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(&tmp.path().join("data"));
        let (archive, artifact) = archive(tmp.path());
        app.install(&archive, "0.2.0", &artifact).unwrap();
        // A newer UXDuck whose manifest names the same desktop app: nothing to download.
        let manifest = ReleaseManifest {
            schema: 1,
            channel: "stable".into(),
            version: "0.3.0".into(),
            published_at: "2026-10-12T00:00:00Z".into(),
            rollback: false,
            notes_url: None,
            artifacts: Vec::new(),
            desktop_app: Some(uxduck_contracts::dto::release::DesktopAppRelease {
                version: "0.2.0".into(),
                artifacts: vec![artifact.clone()],
                inputs: None,
            }),
        };
        let (version, _) = offered(&manifest, "test").unwrap();
        assert_eq!(Some(version), app.installed().map(|(v, _)| v));
        assert!(offered(&manifest, "other-target").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_uxduck_path_finds_a_plain_bundled_runtime() {
        assert_eq!(
            bundled_directory(Path::new(r"\\?\C:\Program Files\UXDuck\uxduck.exe")),
            PathBuf::from(r"C:\Program Files\UXDuck\desktop-app")
        );
    }

    #[test]
    fn only_the_downloaded_app_is_started() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(&tmp.path().join("data"));
        let elsewhere = tmp.path().join("other");
        std::fs::write(&elsewhere, b"").unwrap();
        assert!(
            app.open(&elsewhere.display().to_string(), &[])
                .unwrap_err()
                .contains("not the desktop window")
        );
    }
}

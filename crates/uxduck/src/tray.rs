//! The tray's supervisor (S64, ADR-0008 "The tray"). With `desktop-tray`,
//! a desktop session starts `uxduck-tray` beside the daemon executable and
//! keeps it running while "Show UXDuck in the menu bar" is on: restarted if it
//! crashes, stopped when the daemon stops. It is never orphaned: its stdin
//! is a pipe from the daemon, and it quits when that closes, even after
//! `kill -9`. With no display (a Pi, a server, an SSH session) or for a
//! headless forward there is no tray and no error.
//!
//! The daemon links no GUI toolkit; the tray is a client of the daemon's
//! socket like the CLI, and draws the menu the engine builds.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

use crate::daemon::engine_task::EngineHandle;

/// How often the supervisor looks.
const EVERY: Duration = Duration::from_secs(2);
/// A tray that ran less than this before it ended counts as a failure;
/// after this many in a row it is not started again until the setting is
/// switched off and on, or the daemon restarts.
const SHORT: Duration = Duration::from_secs(10);
const FAILURES: u32 = 5;

/// Whether this session can show a tray icon.
fn has_display() -> bool {
    if std::env::var_os("UXDUCK_NO_TRAY").is_some() {
        return false;
    }
    let ssh = std::env::var_os("SSH_CONNECTION").is_some();
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        !ssh
    }
}

/// The tray's executable: `UXDUCK_TRAY` (empty for none), else beside the
/// daemon's real file (on macOS, inside UXDuck.app, which
/// `/usr/local/bin/uxduck` links into), else in the installed UXDuck.app.
pub fn binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("UXDUCK_TRAY") {
        return Some(PathBuf::from(path)).filter(|p| !p.as_os_str().is_empty());
    }
    // macOS reports the path it was started by, which may be the link.
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "uxduck-tray.exe"
    } else {
        "uxduck-tray"
    };
    [
        dir.join(name),
        PathBuf::from("/Applications/UXDuck.app/Contents/MacOS/uxduck-tray"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Desktop supervision may begin disabled and need discovery when enabled.
#[derive(Debug)]
pub struct Tray {
    binary: Option<PathBuf>,
}

fn missing() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "UXDuck requires uxduck-tray for its menu bar. Reinstall UXDuck, or build it with cargo build -p uxduck-tray. For an intentional headless session, set UXDUCK_NO_TRAY=1.",
    )
}

/// Check required files and executability before the engine opens any links.
///
/// # Errors
/// Missing or unlaunchable required helpers, invalid artwork, or a timeout.
pub async fn preflight(headless: bool, enabled: bool) -> std::io::Result<Option<Tray>> {
    if !cfg!(feature = "desktop-tray")
        || headless
        || !has_display()
        || std::env::var_os("UXDUCK_TRAY").is_some_and(|path| path.is_empty())
    {
        return Ok(None);
    }
    let binary = binary();
    if enabled {
        let path = binary.as_ref().ok_or_else(missing)?;
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(path)
                .arg("--check")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "UXDuck's menu-bar helper preflight timed out",
            )
        })?
        .map_err(|error| std::io::Error::new(error.kind(), format!("Cannot launch required menu-bar helper at {}: {error}. Reinstall UXDuck, or run cargo build -p uxduck-tray.", path.display())))?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "UXDuck's menu-bar helper failed preflight ({}): {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    Ok(Some(Tray { binary }))
}

/// The name of the app a development tray runs from on macOS.
#[cfg(target_os = "macos")]
const DEV_APP: &str = "UXDuck Dev.app";

/// macOS asks its permission questions, location among them, only of an
/// app: `CoreLocationAgent` skips a bare executable ("client bundle is NULL"),
/// and the request waits for ever. The installed tray is UXDuck.app's own
/// executable; one built with cargo is bare, so it runs from a small app
/// beside it, `UXDuck Dev.app`, refreshed from the built file whenever that
/// changes and signed ad hoc (S137).
#[cfg(target_os = "macos")]
fn in_app(binary: &Path) -> std::io::Result<PathBuf> {
    if binary
        .components()
        .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
    {
        return Ok(binary.to_owned());
    }
    let dir = binary
        .parent()
        .ok_or_else(|| std::io::Error::other("the menu-bar helper has no folder"))?;
    let app = dir.join(DEV_APP);
    let exe = app.join("Contents/MacOS/uxduck-tray");
    let built = std::fs::metadata(binary)?;
    let fresh = std::fs::metadata(&exe).is_ok_and(|copy| {
        copy.len() == built.len()
            && matches!((copy.modified(), built.modified()), (Ok(copy), Ok(built)) if copy >= built)
    });
    if fresh {
        return Ok(exe);
    }
    std::fs::create_dir_all(exe.parent().unwrap_or(dir))?;
    std::fs::copy(binary, &exe)?;
    std::fs::write(app.join("Contents/Info.plist"), DEV_INFO)?;
    let signed = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !signed.success() {
        return Err(std::io::Error::other(format!(
            "Cannot sign the development menu-bar app {}: codesign failed ({signed}). Install Xcode's command line tools.",
            app.display()
        )));
    }
    tracing::info!(app = %app.display(), "the development menu-bar helper runs from an app");
    Ok(exe)
}

/// `UXDuck Dev.app`'s Info.plist: a menu-bar app with UXDuck's location
/// question, apart from the installed UXDuck.app.
#[cfg(target_os = "macos")]
const DEV_INFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.uxduck.app.dev</string>
  <key>CFBundleName</key><string>UXDuck Dev</string>
  <key>CFBundleDisplayName</key><string>UXDuck Dev</string>
  <key>CFBundleExecutable</key><string>uxduck-tray</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSUIElement</key><true/>
  <key>NSLocationUsageDescription</key><string>UXDuck shows where you are on its maps, and its AI can use it when you ask.</string>
  <key>NSLocationWhenInUseUsageDescription</key><string>UXDuck shows where you are on its maps, and its AI can use it when you ask.</string>
</dict>
</plist>
"#;

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps, reason = "macOS can fail here")]
fn in_app(binary: &Path) -> std::io::Result<PathBuf> {
    Ok(binary.to_owned())
}

fn spawn(binary: &Path, data_dir: &Path) -> std::io::Result<Child> {
    Command::new(in_app(binary)?)
        .arg("--data-dir")
        .arg(data_dir)
        // Its end of this pipe closes when the daemon goes, however it goes.
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// Starts supervising the tray, unless there is nothing to show it on.
pub fn start(data_dir: &Path, engine: EngineHandle, tray: Option<Tray>) {
    if let Some(tray) = tray {
        tokio::spawn(supervise(tray.binary, data_dir.to_owned(), engine));
    }
}

async fn supervise(mut path: Option<PathBuf>, data_dir: PathBuf, engine: EngineHandle) {
    // On a desktop: with the icon switched off, the last window quits.
    let _ = engine.run(|e| e.set_desktop(true)).await;
    let mut running: Option<(Child, Instant)> = None;
    let mut failures = 0;
    loop {
        let Ok(wanted) = engine.run(|e| e.state().settings.show_in_menu_bar).await else {
            // The engine has stopped: dropping the child stops the tray.
            return;
        };
        if !wanted {
            failures = 0;
            let _ = engine.run(uxduck_engine::Engine::tray_recovered).await;
            if let Some((mut child, _)) = running.take() {
                let _ = child.kill().await;
            }
        }
        if let Some((child, started)) = running.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            let short = started.elapsed() < SHORT;
            failures = if short { failures + 1 } else { 0 };
            tracing::info!(%status, failures, "the tray ended; starting it again");
            running = None;
        }
        if wanted && running.is_none() && failures < FAILURES {
            if path.is_none() {
                path = binary();
            }
            match path
                .as_ref()
                .map_or_else(|| Err(missing()), |path| spawn(path, &data_dir))
            {
                Ok(child) => running = Some((child, Instant::now())),
                Err(error) => {
                    failures += 1;
                    tracing::warn!(%error, "the tray did not start");
                }
            }
            if failures == FAILURES {
                tracing::error!(
                    "UXDuck's menu-bar helper failed repeatedly. Repair the installation and toggle Show UXDuck in the menu bar to retry"
                );
                let _ = engine.run(|e| e.tray_failed("The menu-bar helper could not stay running after five attempts. Repair the installation, then turn Show UXDuck in the menu bar off and on to retry.")).await;
            }
        } else if wanted && failures >= FAILURES {
            let _ = engine.run(|e| e.tray_failed("The menu-bar helper could not stay running after five attempts. Repair the installation, then turn Show UXDuck in the menu bar off and on to retry.")).await;
        }
        if running
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= SHORT)
        {
            let _ = engine.run(uxduck_engine::Engine::tray_recovered).await;
        }
        tokio::time::sleep(EVERY).await;
    }
}

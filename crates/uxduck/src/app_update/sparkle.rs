//! Sparkle's public user-driver protocol, supervised by the native daemon.
//! Only the engine admits shutdown; the helper requires consent AND parent EOF.
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use uxduck_contracts::dto::update::UpdatePhase;

use super::{EngineHandle, HANDOFF_PIPE, Plan, publish_channel_failure, status};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    phase: String,
    version: Option<String>,
    detail: Option<String>,
    notes_url: Option<String>,
    received: Option<u64>,
    size: Option<u64>,
}

pub(super) fn app_for(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_owned())
}

pub(super) fn installed_app() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    app_for(&exe).ok_or("Mac app updates require the complete UXDuck.app bundle.".into())
}

fn launch(
    data: &Path,
    port: u16,
    feed: &str,
) -> Result<
    (
        Child,
        ChildStdin,
        tokio::sync::mpsc::UnboundedReceiver<Event>,
    ),
    String,
> {
    let helper =
        installed_app()?.join("Contents/Helpers/UXDuck Updater.app/Contents/MacOS/uxduck-updater");
    if !helper.is_file() {
        return Err(
            "UXDuck's Sparkle updater is missing. Reinstall the complete UXDuck app.".into(),
        );
    }
    let mut child = Command::new(helper)
        .arg("watch")
        .arg(data)
        .arg(port.to_string())
        .arg(feed)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start the Mac updater: {e}"))?;
    let input = child
        .stdin
        .take()
        .ok_or("The Mac updater has no consent pipe")?;
    let output = child
        .stdout
        .take()
        .ok_or("The Mac updater has no status pipe")?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            let Ok(event) = serde_json::from_str(&line) else {
                continue;
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    Ok((child, input, rx))
}

pub(super) fn start(engine: EngineHandle, data: PathBuf, port: u16) {
    tokio::spawn(async move {
        let mut next = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(next) => {},
                () = engine.requested(|r| r.app_update) => {},
            }
            let _ = engine
                .run(uxduck_engine::Engine::app_update_take_check)
                .await;
            let _ = engine
                .run(|e| e.app_update_publish(status(UpdatePhase::Checking)))
                .await;
            let Ok(include_beta) = engine.run(|e| e.state().settings.include_beta).await else {
                return;
            };
            if let Err(why) = cycle(&engine, &data, port, include_beta).await {
                publish_channel_failure(&engine, include_beta, why).await;
            }
            if engine
                .run(|e| e.app_update_installing())
                .await
                .unwrap_or(true)
            {
                return;
            }
            let recheck = engine
                .run(|e| e.app_update_pending())
                .await
                .unwrap_or(false);
            next = tokio::time::Instant::now()
                + if recheck {
                    Duration::ZERO
                } else {
                    Duration::from_hours(24)
                };
        }
    });
}

struct ReleaseFeed {
    version: String,
    url: String,
    warning: Option<String>,
}

async fn release_feed(
    engine: &EngineHandle,
    data: &Path,
    include_beta: bool,
) -> Result<Option<ReleaseFeed>, String> {
    let report = crate::update::check_channels(
        crate::update::DEFAULT_URL,
        include_beta,
        crate::update::APP_UPDATE_PUBLIC_KEY,
        data,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !matches!(report.verdict, crate::update::Verdict::Available(_)) {
        super::publish_current(engine, include_beta, report.warning).await?;
        return Ok(None);
    }
    let feed = report
        .manifest
        .artifacts
        .iter()
        .find(|a| a.target == crate::update::TARGET && a.kind == "sparkle")
        .ok_or("This release has no signed Mac update feed. Install UXDuck manually.")?;
    if !super::allowed_url(&feed.url) {
        return Err("The Mac update feed must use HTTPS.".into());
    }
    Ok(Some(ReleaseFeed {
        version: report.manifest.version,
        url: feed.url.clone(),
        warning: report.warning,
    }))
}

#[allow(
    clippy::too_many_lines,
    reason = "one Sparkle event and consent lifecycle; handoff stays in the same loop"
)]
async fn cycle(
    engine: &EngineHandle,
    data: &Path,
    port: u16,
    include_beta: bool,
) -> Result<(), String> {
    let Some(ReleaseFeed {
        version: expected_version,
        url: feed,
        warning,
    }) = release_feed(engine, data, include_beta).await?
    else {
        return Ok(());
    };
    let (mut child, mut input, mut events) = launch(data, port, &feed)?;
    let mut ready = None;
    let mut failure = None;
    let mut refresh_at = tokio::time::Instant::now() + Duration::from_hours(24);
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    let exit = child.wait().map_err(|e| e.to_string())?;
                    return if exit.success() { Ok(()) } else { Err(failure.unwrap_or_else(|| "The Mac updater stopped. Check for updates again.".into())) };
                };
                if event.phase == "armed" { continue; }
                if event.version.as_ref().is_some_and(|v| v != &expected_version) {
                    let _ = child.kill(); let _ = child.wait();
                    return Err("The Mac feed offered a different release than the signed channel.".into());
                }
                let phase = match event.phase.as_str() {
                    "current" => UpdatePhase::Current,
                    "downloading" => UpdatePhase::Downloading,
                    "ready" => UpdatePhase::Ready,
                    "failed" => UpdatePhase::Failed,
                    _ => continue,
                };
                if phase == UpdatePhase::Ready {
                    ready.clone_from(&event.version);
                } else { ready = None; }
                if phase == UpdatePhase::Failed { failure.clone_from(&event.detail); }
                let mut s = status(phase);
                s.version = event.version;
                s.detail = match (&warning, event.detail) {
                    (Some(warning), Some(detail)) => Some(format!("{warning} {detail}")),
                    (Some(warning), None) => Some(warning.clone()),
                    (None, detail) => detail,
                };
                s.notes_url = event.notes_url;
                s.received = event.received;
                s.size = event.size;
                if !engine.run(move |e| e.app_update_publish_for_channel(include_beta, s)).await.unwrap_or(false) {
                    let _ = child.kill(); let _ = child.wait(); return Ok(());
                }
            },
            () = engine.requested(|r| r.app_update) => {
                let recheck = engine.run(move |e| {
                    e.state().settings.include_beta != include_beta
                        || (e.app_update_pending() && !e.app_update_status().install_when_idle)
                }).await.unwrap_or(true);
                if recheck {
                    // Keep the check flag for the outer loop. EOF without consent
                    // or killing an unapproved helper can never start installation.
                    let _ = child.kill(); let _ = child.wait();
                    return Ok(());
                }
            },
            () = tokio::time::sleep_until(refresh_at) => {
                refresh_at = tokio::time::Instant::now() + Duration::from_hours(24);
                let refresh = engine.run(|e| {
                    if e.app_update_status().install_when_idle { return false; }
                    e.app_update_request_check(); true
                }).await.unwrap_or(false);
                if refresh {
                    let _ = child.kill(); let _ = child.wait(); return Ok(());
                }
            },
            () = tokio::time::sleep(Duration::from_secs(1)), if ready.is_some() => {},
        }
        if ready.is_none()
            || !engine
                .run(|e| e.app_update_approved())
                .await
                .unwrap_or(false)
        {
            if ready.is_some() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            continue;
        }
        let version = ready.as_deref().ok_or("No prepared Mac update")?;
        semver::Version::parse(version).map_err(|_| "The Mac update has an invalid version")?;
        let reopen = engine
            .run(|e| e.app_update_visible())
            .await
            .unwrap_or(false);
        let dir = data.join("updates/sparkle").join(version);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let plan = Plan {
            version: version.into(),
            kind: "sparkle".into(),
            executable: executable.clone(),
            installed_executable: Some(executable),
            data_dir: data.into(),
            port,
            reopen,
        };
        let (mut guard, mut permission) = super::spawn_worker(&dir, &plan)?;
        if engine
            .run(uxduck_engine::Engine::app_update_handoff)
            .await
            .unwrap_or(false)
        {
            // The guardian holds the app-install lease. Its restart also restores
            // the previous app if Sparkle fails or authorization is cancelled.
            writeln!(input, "{{\"action\":\"install\",\"reopen\":{reopen}}}")
                .map_err(|e| e.to_string())?;
            permission
                .write_all(b"install\n")
                .map_err(|e| e.to_string())?;
            if let Ok(mut pipe) = HANDOFF_PIPE.lock() {
                *pipe = Some(input);
            }
            // Both pipes must stay open until final process exit.
            if let Ok(mut pipe) = super::SPARKLE_GUARD_PIPE.lock() {
                *pipe = Some(permission);
            }
            drop(child);
            drop(guard);
            return Ok(());
        }
        drop(permission);
        let _ = guard.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundle_location_follows_the_running_copy() {
        for path in [
            "/Users/ana/Applications/UXDuck.app",
            "/Applications/UXDuck.app",
            "/Volumes/UXDuck/UXDuck.app",
        ] {
            assert_eq!(
                app_for(&Path::new(path).join("Contents/MacOS/uxduck")),
                Some(PathBuf::from(path))
            );
        }
        assert_eq!(app_for(Path::new("/usr/local/bin/uxduck")), None);
        assert_eq!(app_for(Path::new("/tmp/UXDuck.app/Other/uxduck")), None);
    }
}

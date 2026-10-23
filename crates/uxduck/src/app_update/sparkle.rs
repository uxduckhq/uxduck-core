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

fn helper_for(app: &Path) -> Result<PathBuf, String> {
    let helper = app.join("Contents/Helpers/UXDuck Updater.app/Contents/MacOS/uxduck-updater");
    if !helper.is_file() {
        return Err(
            "UXDuck's Sparkle updater is missing. Reinstall the complete UXDuck app.".into(),
        );
    }
    Ok(helper)
}

fn preflight(app: &Path) -> Result<(), String> {
    let helper = helper_for(app)?;
    let result = Command::new(helper)
        .arg("preflight")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| {
            format!("Cannot start the Mac updater: {e}. Reinstall the complete UXDuck app.")
        })?;
    if !result.success() {
        return Err("The Mac updater could not start. Reinstall the complete UXDuck app.".into());
    }
    Ok(())
}

fn launch(
    data: &Path,
    port: u16,
    feed: &str,
    archive: &Path,
    archive_url: &str,
) -> Result<
    (
        Child,
        ChildStdin,
        tokio::sync::mpsc::UnboundedReceiver<Event>,
    ),
    String,
> {
    let helper = helper_for(&installed_app()?)?;
    let mut child = Command::new(helper)
        .arg("watch")
        .arg(data)
        .arg(port.to_string())
        .arg(feed)
        .arg(archive)
        .arg(archive_url)
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
    archive: uxduck_contracts::dto::release::ReleaseArtifact,
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
    let archive = report
        .manifest
        .artifacts
        .iter()
        .find(|a| a.target == crate::update::TARGET && a.kind == "dmg")
        .cloned()
        .ok_or("This release has no Mac update archive. Install UXDuck manually.")?;
    if archive.size == 0
        || archive.size > super::MAX_PACKAGE
        || archive.sha256.len() != 64
        || !archive.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || !super::allowed_url(&archive.url)
    {
        return Err("The Mac update archive has an invalid size, checksum or HTTPS URL.".into());
    }
    Ok(Some(ReleaseFeed {
        version: report.manifest.version,
        url: feed.url.clone(),
        warning: report.warning,
        archive,
    }))
}

/// Download while the engine and its clients remain available. Cancellation of
/// even a stalled request follows channel changes, as for other platforms.
async fn stage_archive(
    channel: super::DownloadChannel<'_>,
    data: &Path,
    version: &str,
    artifact: &uxduck_contracts::dto::release::ReleaseArtifact,
    warning: Option<String>,
) -> Result<PathBuf, String> {
    let version = semver::Version::parse(version).map_err(|e| e.to_string())?;
    let dir = data.join("app-updates").join(version.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join("package.dmg");
    let partial = dir.join("package.partial");
    let mut s = status(UpdatePhase::Downloading);
    s.version = Some(version.to_string());
    s.detail = warning;
    s.size = Some(artifact.size);
    s.received = Some(0);
    channel.publish(s.clone()).await?;
    let result = {
        let download = async {
            if super::verify_file_async(&archive, artifact).await.is_err() {
                super::download_file(channel, artifact, &partial, &s).await?;
                super::verify_file_async(&partial, artifact).await?;
                std::fs::rename(&partial, &archive).map_err(|e| e.to_string())?;
            }
            s.received = Some(artifact.size);
            s.detail = Some("Verifying the update…".into());
            channel.publish(s).await
        };
        tokio::pin!(download);
        loop {
            tokio::select! {
                result = &mut download => break result,
                () = tokio::time::sleep(Duration::from_secs(1)) => {
                    let changed = channel.engine.run(move |e| e.state().settings.include_beta != channel.include_beta)
                        .await.unwrap_or(true);
                    if changed { break Err("Update preference changed. Checking the selected channel again.".into()); }
                }
            }
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result?;
    Ok(archive)
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
        archive,
    }) = release_feed(engine, data, include_beta).await?
    else {
        return Ok(());
    };
    // Required native support must be present before doing any download work.
    let app = installed_app()?;
    tokio::task::spawn_blocking(move || preflight(&app))
        .await
        .map_err(|e| e.to_string())??;
    let channel = super::DownloadChannel {
        engine,
        include_beta,
    };
    let staged = stage_archive(channel, data, &expected_version, &archive, warning.clone()).await?;
    // Sparkle is not started until the complete signed archive is on disk. It
    // still cannot extract, authorize or replace anything before consent + EOF.
    let (mut child, mut input, mut events) = launch(data, port, &feed, &staged, &archive.url)?;
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
        let reopen_target = engine
            .run(|e| e.app_update_reopen_target())
            .await
            .unwrap_or(None);
        let reopen = reopen_target.is_some();
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
            reopen_target,
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
#[allow(clippy::unwrap_used, reason = "test fixtures")]
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

    #[test]
    fn missing_helper_fails_before_archive_staging() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            helper_for(root.path())
                .unwrap_err()
                .contains("Reinstall the complete UXDuck app")
        );
        let helper = root
            .path()
            .join("Contents/Helpers/UXDuck Updater.app/Contents/MacOS/uxduck-updater");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::create_dir(&helper).unwrap();
        assert!(
            helper_for(root.path()).is_err(),
            "a directory is not the required helper"
        );
        std::fs::remove_dir(&helper).unwrap();
        std::fs::write(&helper, b"fixture helper").unwrap();
        assert_eq!(helper_for(root.path()).unwrap(), helper);
    }

    #[test]
    fn preflight_checks_launch_and_its_exit_status_without_starting_an_update() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let helper = root
            .path()
            .join("Contents/Helpers/UXDuck Updater.app/Contents/MacOS/uxduck-updater");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(
            &helper,
            b"#!/bin/sh\n[ \"$#\" -eq 1 ] && [ \"$1\" = preflight ]\n",
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(preflight(root.path()).is_ok());
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(preflight(root.path()).unwrap_err().contains("Cannot start"));
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&helper, b"#!/bin/sh\nexit 1\n").unwrap();
        assert!(
            preflight(root.path())
                .unwrap_err()
                .contains("could not start")
        );
    }

    #[tokio::test]
    async fn stages_verified_archive_without_consent_and_reuses_it_offline() {
        use sha2::{Digest, Sha256};
        let engine = EngineHandle::spawn(super::super::tests::test_engine());
        let channel = super::super::DownloadChannel {
            engine: &engine,
            include_beta: false,
        };
        let bytes = b"signed update fixture";
        let (base, _) =
            super::super::tests::serve([("/update.dmg".into(), bytes.to_vec())].into()).await;
        let mut artifact = uxduck_contracts::dto::release::ReleaseArtifact {
            target: crate::update::TARGET.into(),
            kind: "dmg".into(),
            file: "update.dmg".into(),
            url: format!("{base}/update.dmg"),
            size: bytes.len() as u64,
            sha256: format!("{:x}", uxduck_core::encoding::Hex(&Sha256::digest(bytes))),
        };
        let data = tempfile::tempdir().unwrap();
        let staged = stage_archive(channel, data.path(), "0.5.1-beta.2", &artifact, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), bytes);
        assert!(!engine.run(|e| e.app_update_approved()).await.unwrap());
        assert_eq!(
            engine
                .run(|e| e.app_update_status().received)
                .await
                .unwrap(),
            Some(bytes.len() as u64)
        );
        artifact.url = "http://127.0.0.1:1/unavailable".into();
        assert_eq!(
            stage_archive(channel, data.path(), "0.5.1-beta.2", &artifact, None)
                .await
                .unwrap(),
            staged
        );
        std::fs::write(&staged, b"tampered").unwrap();
        assert!(
            stage_archive(channel, data.path(), "0.5.1-beta.2", &artifact, None)
                .await
                .is_err()
        );
        assert!(!engine.run(|e| e.app_update_approved()).await.unwrap());
    }

    #[tokio::test]
    async fn rejects_tampered_archive_before_sparkle_starts() {
        use sha2::{Digest, Sha256};
        let engine = EngineHandle::spawn(super::super::tests::test_engine());
        let channel = super::super::DownloadChannel {
            engine: &engine,
            include_beta: false,
        };
        let (base, _) =
            super::super::tests::serve([("/update.dmg".into(), b"other".to_vec())].into()).await;
        let artifact = uxduck_contracts::dto::release::ReleaseArtifact {
            target: crate::update::TARGET.into(),
            kind: "dmg".into(),
            file: "update.dmg".into(),
            url: format!("{base}/update.dmg"),
            size: 5,
            sha256: format!(
                "{:x}",
                uxduck_core::encoding::Hex(&Sha256::digest(b"hello"))
            ),
        };
        let data = tempfile::tempdir().unwrap();
        assert!(
            stage_archive(channel, data.path(), "0.5.1-beta.2", &artifact, None)
                .await
                .unwrap_err()
                .contains("checksum")
        );
        assert!(
            !data
                .path()
                .join("app-updates/0.5.1-beta.2/package.dmg")
                .exists()
        );
        assert!(
            !data
                .path()
                .join("app-updates/0.5.1-beta.2/package.partial")
                .exists()
        );
    }

    #[tokio::test]
    async fn changing_channel_cancels_a_stalled_mac_download() {
        use futures::channel::mpsc::unbounded;
        let mut e = super::super::tests::test_engine();
        let (sender, _receiver) = unbounded();
        let c = e.connect(
            uxduck_contracts::dto::trace::Transport::Socket,
            uxduck_core::policy::admission::Trust::LocalUser,
            sender,
        );
        e.receive(c, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","kind":"operator-ui","version":"0.1.1"},"capabilities":{"protocolVersion":1}}}).to_string());
        let engine = EngineHandle::spawn(e);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, request_started) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            let _ = accepted.send(());
            std::future::pending::<()>().await;
        });
        let artifact = uxduck_contracts::dto::release::ReleaseArtifact {
            target: crate::update::TARGET.into(),
            kind: "dmg".into(),
            file: "update.dmg".into(),
            url: format!("http://{address}/update.dmg"),
            size: 5,
            sha256: "0".repeat(64),
        };
        let data = tempfile::tempdir().unwrap();
        let handle = engine.clone();
        let dir = data.path().to_owned();
        let download = tokio::spawn(async move {
            let channel = super::super::DownloadChannel {
                engine: &handle,
                include_beta: false,
            };
            stage_archive(channel, &dir, "0.5.1-beta.2", &artifact, None).await
        });
        tokio::time::timeout(Duration::from_secs(3), request_started)
            .await
            .unwrap()
            .unwrap();
        engine.run(move |e| e.receive(c, &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"settings/update","params":{"expectedVersion":0,"includeBeta":true}}).to_string())).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), download)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("preference changed"));
        assert!(
            !data
                .path()
                .join("app-updates/0.5.1-beta.2/package.dmg")
                .exists()
        );
        assert_eq!(
            engine.run(|e| e.app_update_status().phase).await.unwrap(),
            UpdatePhase::Checking
        );
        server.abort();
    }
}

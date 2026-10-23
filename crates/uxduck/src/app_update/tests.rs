#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test fixtures")]
use super::*;
use uxduck_contracts::dto::release::ReleaseManifest;

fn report(target: &str, kind: &str) -> crate::update::Report {
    crate::update::compare(
        "0.1.0",
        ReleaseManifest {
            schema: 1,
            channel: "stable".into(),
            version: "0.2.0".into(),
            published_at: "2026-10-17T00:00:00Z".into(),
            rollback: false,
            notes_url: None,
            desktop_app: None,
            artifacts: vec![ReleaseArtifact {
                target: target.into(),
                kind: kind.into(),
                file: "unused".into(),
                url: "https://example.test/update".into(),
                size: 5,
                sha256: checksum(b"hello"),
            }],
        },
        target,
    )
    .unwrap()
}

#[test]
fn chooses_each_platform_installer_and_rejects_archives_and_wrong_architecture() {
    for (target, kind) in [
        ("aarch64-apple-darwin", "pkg"),
        ("x86_64-apple-darwin", "pkg"),
        ("x86_64-pc-windows-msvc", "msi"),
        ("x86_64-unknown-linux-gnu", "deb"),
        ("aarch64-unknown-linux-gnu", "deb"),
    ] {
        assert_eq!(
            installer_artifact(&report(target, kind), target)
                .unwrap()
                .kind,
            kind
        );
        assert!(installer_artifact(&report(target, "tar.gz"), target).is_err());
    }
    assert!(
        installer_artifact(
            &report("aarch64-apple-darwin", "pkg"),
            "x86_64-apple-darwin"
        )
        .is_err()
    );
    assert!(
        installer_artifact(
            &report("x86_64-pc-windows-msvc", "msi"),
            "x86_64-unknown-linux-gnu"
        )
        .is_err()
    );
}

#[test]
fn tampered_truncated_and_oversized_packages_are_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("package.pkg");
    let report = report("aarch64-apple-darwin", "pkg");
    let artifact = &report.manifest.artifacts[0];
    assert!(verify_file(&package, artifact).is_err());
    std::fs::write(&package, b"hello").unwrap();
    assert!(verify_file(&package, artifact).is_ok());
    for bytes in [b"other".as_slice(), b"hi", b"too long"] {
        std::fs::write(&package, bytes).unwrap();
        assert!(verify_file(&package, artifact).is_err());
    }
    let mut huge = report.clone();
    huge.manifest.artifacts[0].size = MAX_PACKAGE + 1;
    assert!(installer_artifact(&huge, "aarch64-apple-darwin").is_err());
    let mut bad = report;
    bad.manifest.artifacts[0].sha256 = "no digest".into();
    assert!(installer_artifact(&bad, "aarch64-apple-darwin").is_err());
}

#[test]
fn installer_lease_blocks_new_daemons_but_a_stale_file_does_not() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check_startup(dir.path()).is_ok());
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.path().join("app-install.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    assert!(
        check_startup(dir.path())
            .unwrap_err()
            .to_string()
            .contains("installing an update")
    );
    drop(lease);
    assert!(check_startup(dir.path()).is_ok());
}

#[test]
fn worker_checks_the_exact_signed_manifest_and_package_again() {
    let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let public = key.pk.to_box().unwrap().into_string();
    let public = public.lines().nth(1).unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("0.2.0");
    std::fs::create_dir(&dir).unwrap();
    let target = crate::update::TARGET;
    let kind = if cfg!(target_os = "macos") {
        "pkg"
    } else if cfg!(windows) {
        "msi"
    } else {
        "deb"
    };
    let report = report(target, kind);
    let bytes = serde_json::to_vec(&report.manifest).unwrap();
    let signature = minisign::sign(
        None,
        &key.sk,
        std::io::Cursor::new(&bytes),
        Some("test"),
        None,
    )
    .unwrap()
    .into_string();
    std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
    std::fs::write(dir.join("manifest.minisig"), signature).unwrap();
    std::fs::write(dir.join(format!("package.{kind}")), b"hello").unwrap();
    assert!(verified_artifact(&dir, public).is_ok());
    std::fs::write(dir.join(format!("package.{kind}")), b"other").unwrap();
    assert!(verified_artifact(&dir, public).is_err());
    std::fs::write(dir.join(format!("package.{kind}")), b"hello").unwrap();
    let wrong = minisign::KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_box()
        .unwrap()
        .into_string();
    assert!(verified_artifact(&dir, wrong.lines().nth(1).unwrap()).is_err());
    std::fs::write(dir.join("manifest.json"), b"{}").unwrap();
    assert!(verified_artifact(&dir, public).is_err());
}

pub(super) fn test_engine() -> uxduck_engine::Engine {
    use crate::adapters::system::{OsRandom, SystemClock};
    use uxduck_engine::runtime::{
        Forget, MemoryCredentials, NoAgent, NoLinks, NoPlugins, NoSystem, Ports,
    };
    use uxduck_engine::{Engine, EngineConfig, HostAbout, Loaded};
    Engine::new(
        EngineConfig {
            daemon_version: "0.1.1".into(),
            capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
            ui_url: "http://127.0.0.1:48091".into(),
            checkpoint_every: 1000,
            models: vec![],
            agent_provider: "fake".into(),
            about: HostAbout::default(),
        },
        Box::new(SystemClock),
        Box::new(OsRandom),
        Ports {
            persistence: Box::new(Forget),
            agent: Box::new(NoAgent),
            credentials: Box::new(MemoryCredentials::default()),
            links: Box::new(NoLinks),
            system: Box::new(NoSystem::default()),
            plugins: Box::new(NoPlugins),
        },
        Loaded::default(),
    )
}

#[tokio::test]
async fn installation_freeze_rejects_delayed_host_work_but_allows_the_final_checkpoint() {
    use futures::channel::mpsc::unbounded;
    let mut e = test_engine();
    let (sender, _receiver) = unbounded();
    let c = e.connect(
        uxduck_contracts::dto::trace::Transport::Socket,
        uxduck_core::policy::admission::Trust::LocalUser,
        sender,
    );
    e.receive(c, &serde_json::json!({ "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"clientInfo":{"name":"test","kind":"operator-ui","version":"0.1.1"},"capabilities":{"protocolVersion":1}} }).to_string());
    e.app_update_publish(AppUpdateStatus {
        phase: UpdatePhase::Ready,
        version: Some("0.2.0".into()),
        ..Default::default()
    });
    e.receive(c, &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"update/install","params":{"version":"0.2.0","whenIdle":false}}).to_string());
    assert!(e.app_update_handoff());
    let handle = EngineHandle::spawn(e);
    assert!(
        handle
            .run(|_| panic!("late work must not run during installation"))
            .await
            .is_err()
    );
    assert!(handle.checkpoint().await.is_ok());
}

/// A signed manifest for this computer with a package and a per-file list
/// (S164), staged in `<root>/0.2.0` as a download leaves it.
#[cfg(unix)]
fn staged_with_files(root: &Path) -> (PathBuf, String, Vec<u8>) {
    use uxduck_contracts::dto::release::{BUNDLE_FILES_KIND, BundleEntry, BundleFiles};
    let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let public = key.pk.to_box().unwrap().into_string();
    let public = public.lines().nth(1).unwrap().to_owned();
    let dir = root.join("0.2.0");
    std::fs::create_dir(&dir).unwrap();
    let target = crate::update::TARGET;
    let list = serde_json::to_vec(&BundleFiles {
        schema: 1,
        version: "0.2.0".into(),
        target: target.into(),
        blobs: "https://app.uxduck.com/releases/files/".into(),
        mode: 0o755,
        entries: vec![BundleEntry::Dir {
            path: "Contents".into(),
            mode: 0o755,
        }],
    })
    .unwrap();
    let mut report = report(target, PACKAGE_KIND);
    report.manifest.artifacts.push(ReleaseArtifact {
        target: target.into(),
        kind: BUNDLE_FILES_KIND.into(),
        file: "list".into(),
        url: "https://example.test/list".into(),
        size: list.len() as u64,
        sha256: checksum(&list),
    });
    let bytes = serde_json::to_vec(&report.manifest).unwrap();
    let signature = minisign::sign(None, &key.sk, std::io::Cursor::new(&bytes), None, None)
        .unwrap()
        .into_string();
    std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
    std::fs::write(dir.join("manifest.minisig"), signature).unwrap();
    (dir, public, list)
}

#[cfg(unix)]
#[test]
fn a_staged_file_list_installs_by_files_until_it_fails_then_by_package() {
    let root = tempfile::tempdir().unwrap();
    let (dir, key, list) = staged_with_files(root.path());
    // Nothing downloaded yet: neither a list nor a package is ready.
    assert!(verified_staged(&dir, &key).is_err());
    std::fs::write(dir.join(files::LIST), &list).unwrap();
    let staged = verified_staged(&dir, &key).unwrap();
    assert!(matches!(staged, Staged::Files(_)));
    assert_eq!(staged.kind(), "files");
    // A changed list is never used, and is not silently replaced either.
    let mut tampered = list.clone();
    tampered.push(b' ');
    std::fs::write(dir.join(files::LIST), &tampered).unwrap();
    assert!(verified_staged(&dir, &key).is_err());
    // After a failure the version installs from its checked package.
    std::fs::write(dir.join(FILES_FAILED), b"codesign refused it").unwrap();
    assert!(verified_staged(&dir, &key).is_err(), "no package yet");
    std::fs::write(dir.join(format!("package.{PACKAGE_KIND}")), b"hello").unwrap();
    let staged = verified_staged(&dir, &key).unwrap();
    assert!(matches!(staged, Staged::Package(_)));
    assert_eq!(staged.kind(), PACKAGE_KIND);
}

#[cfg(unix)]
#[test]
fn only_this_targets_checked_https_list_is_used() {
    use uxduck_contracts::dto::release::BUNDLE_FILES_KIND;
    let target = "aarch64-apple-darwin";
    let mut with_list = report(target, "pkg");
    with_list.manifest.artifacts.push(ReleaseArtifact {
        target: target.into(),
        kind: BUNDLE_FILES_KIND.into(),
        file: "list".into(),
        url: "https://example.test/list".into(),
        size: 10,
        sha256: "a".repeat(64),
    });
    assert!(files_artifact(&with_list, target).is_some());
    assert!(files_artifact(&with_list, "x86_64-apple-darwin").is_none());
    // The installer is still the package: older UXDucks ignore the list.
    assert_eq!(installer_artifact(&with_list, target).unwrap().kind, "pkg");
    for change in [
        |a: &mut ReleaseArtifact| a.url = "http://example.test/list".into(),
        |a: &mut ReleaseArtifact| a.size = files::MAX_LIST + 1,
        |a: &mut ReleaseArtifact| a.size = 0,
        |a: &mut ReleaseArtifact| a.sha256 = "short".into(),
    ] {
        let mut bad = with_list.clone();
        change(&mut bad.manifest.artifacts[1]);
        assert!(files_artifact(&bad, target).is_none());
    }
}

/// Serves `files` at their paths on this computer, as the release bucket does.
#[cfg(unix)]
pub(super) async fn serve(
    files: std::collections::HashMap<String, Vec<u8>>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::response::IntoResponse;
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = asked.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let (files, log) = (files.clone(), log.clone());
        async move {
            log.lock().unwrap().push(uri.path().to_owned());
            match files.get(uri.path()) {
                Some(body) => body.clone().into_response(),
                None => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), asked)
}

/// Serves `list` as the signed manifest names it, and returns that artifact.
#[cfg(unix)]
async fn publish_list(list: &uxduck_contracts::dto::release::BundleFiles) -> ReleaseArtifact {
    let bytes = serde_json::to_vec(list).unwrap();
    let (base, _) = serve([("/list".to_owned(), bytes.clone())].into()).await;
    ReleaseArtifact {
        target: crate::update::TARGET.into(),
        kind: uxduck_contracts::dto::release::BUNDLE_FILES_KIND.into(),
        file: "list".into(),
        url: format!("{base}/list"),
        size: bytes.len() as u64,
        sha256: checksum(&bytes),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_per_file_download_fetches_and_checks_only_the_files_this_app_lacks() {
    use uxduck_contracts::dto::release::{BundleEntry, BundleFiles};
    let root = tempfile::tempdir().unwrap();
    let installed = root.path().join("UXDuck.app");
    std::fs::create_dir_all(installed.join("Contents/MacOS")).unwrap();
    std::fs::write(installed.join("Contents/MacOS/uxduck-tray"), b"tray").unwrap();
    std::fs::write(installed.join("Contents/MacOS/uxduck"), b"old daemon").unwrap();
    let entry = |path: &str, bytes: &[u8], packed: u64| BundleEntry::File {
        path: path.into(),
        mode: 0o755,
        size: bytes.len() as u64,
        sha256: checksum(bytes),
        packed,
    };
    let new_daemon = b"new daemon".repeat(50);
    let sha = checksum(&new_daemon);
    let compressed = zstd::encode_all(&new_daemon[..], 3).unwrap();
    let (base, asked) = serve([(format!("/files/{sha}.zst"), compressed.clone())].into()).await;
    let list = BundleFiles {
        schema: 1,
        version: "0.2.0".into(),
        target: crate::update::TARGET.into(),
        blobs: format!("{base}/files/"),
        mode: 0o755,
        entries: vec![
            BundleEntry::Dir {
                path: "Contents".into(),
                mode: 0o755,
            },
            BundleEntry::Dir {
                path: "Contents/MacOS".into(),
                mode: 0o755,
            },
            entry(
                "Contents/MacOS/uxduck",
                &new_daemon,
                compressed.len() as u64,
            ),
            entry("Contents/MacOS/uxduck-tray", b"tray", 99),
        ],
    };
    let artifact = publish_list(&list).await;
    let engine = EngineHandle::spawn(test_engine());
    let channel = DownloadChannel::current(&engine).await.unwrap();
    let dir = root.path().join("0.2.0");
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = status(UpdatePhase::Downloading);
    let result = download_files(channel, &dir, &artifact, "0.2.0", &installed, &mut s).await;
    // Only the changed daemon was downloaded, and kept once it checked out.
    assert_eq!(*asked.lock().unwrap(), [format!("/files/{sha}.zst")]);
    assert_eq!(s.size, Some(compressed.len() as u64));
    assert_eq!(s.received, Some(compressed.len() as u64));
    assert!(files::have_blob(&dir, &sha, new_daemon.len() as u64));
    assert!(!dir.join("blobs").join(format!("{sha}.zst")).exists());
    if cfg!(target_os = "macos") {
        // This fixture is not a signed app, so the Mac checks refuse it.
        assert!(result.is_err());
    } else {
        assert_eq!(
            result.unwrap_err(),
            "Per-file updates are only for the Mac app."
        );
    }
    assert!(
        !dir.join(files::LIST).exists(),
        "never staged without its checks"
    );

    // A file whose contents differ from the list is refused and not kept.
    let (bad, _) = serve(
        [(
            format!("/files/{sha}.zst"),
            zstd::encode_all(&b"tampered!!".repeat(50)[..], 3).unwrap(),
        )]
        .into(),
    )
    .await;
    let mut tampered = list.clone();
    tampered.blobs = format!("{bad}/files/");
    let artifact = publish_list(&tampered).await;
    let other = root.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let error = download_files(channel, &other, &artifact, "0.2.0", &installed, &mut s)
        .await
        .unwrap_err();
    assert!(
        error.contains("does not match the signed release"),
        "{error}"
    );
    assert!(!files::have_blob(&other, &sha, new_daemon.len() as u64));
}

#[cfg(unix)]
#[tokio::test]
async fn obsolete_download_progress_and_failures_cannot_replace_the_new_channel() {
    use futures::channel::mpsc::unbounded;
    let mut e = test_engine();
    let (sender, _receiver) = unbounded();
    let c = e.connect(
        uxduck_contracts::dto::trace::Transport::Socket,
        uxduck_core::policy::admission::Trust::LocalUser,
        sender,
    );
    e.receive(c, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","kind":"operator-ui","version":"0.1.1"},"capabilities":{"protocolVersion":1}}}).to_string());
    e.app_update_publish(status(UpdatePhase::Downloading));
    e.receive(c, &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"settings/update","params":{"expectedVersion":0,"includeBeta":true}}).to_string());
    assert!(e.state().settings.include_beta);
    let engine = EngineHandle::spawn(e);
    let channel = DownloadChannel {
        engine: &engine,
        include_beta: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let (base, _) = serve([("/blob".to_owned(), b"obsolete".to_vec())].into()).await;
    let error = fetch_file(
        &reqwest::Client::new(),
        &format!("{base}/blob"),
        &dir.path().join("partial"),
        20,
        |_| Some(status(UpdatePhase::Downloading)),
        channel,
    )
    .await
    .unwrap_err();
    assert!(error.contains("preference changed"));
    publish_channel_failure(&engine, false, "obsolete error".into()).await;
    assert!(channel.publish(status(UpdatePhase::Ready)).await.is_err());
    let (s, pending) = engine
        .run(|e| (e.app_update_status(), e.app_update_pending()))
        .await
        .unwrap();
    assert_eq!(s.phase, UpdatePhase::Checking);
    assert!(s.detail.is_none());
    assert!(s.version.is_none());
    assert!(pending, "the new check must remain queued");
}

fn checksum(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", uxduck_core::encoding::Hex(&Sha256::digest(bytes)))
}

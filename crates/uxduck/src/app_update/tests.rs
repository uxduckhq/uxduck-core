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
                sha256: format!("{:x}", Sha256::digest(b"hello")),
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

#[tokio::test]
async fn installation_freeze_rejects_delayed_host_work_but_allows_the_final_checkpoint() {
    use crate::adapters::system::{OsRandom, SystemClock};
    use futures::channel::mpsc::unbounded;
    use uxduck_engine::runtime::{
        Forget, MemoryCredentials, NoAgent, NoLinks, NoPlugins, NoSystem, Ports,
    };
    use uxduck_engine::{Engine, EngineConfig, HostAbout, Loaded};
    let mut e = Engine::new(
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
    );
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

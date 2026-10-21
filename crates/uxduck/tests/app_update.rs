//! The installer worker must never install from EOF, and its lease excludes
//! startup without relying on an Electron process. Safe on all three OSes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration fixtures"
)]
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

#[test]
fn a_worker_without_consent_exits_and_releases_its_startup_lease() {
    for kind in ["pkg", "sparkle"] {
        let data = tempfile::tempdir().unwrap();
        let stage = data.path().join("app-updates/0.2.0");
        std::fs::create_dir_all(&stage).unwrap();
        let binary = env!("CARGO_BIN_EXE_uxduck");
        let plan = serde_json::json!({ "version": "0.2.0", "kind": kind, "executable": binary, "data_dir": data.path(), "port": 48091, "reopen": false });
        std::fs::write(stage.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
        let mut worker = Command::new(binary)
            .arg("__install-update")
            .arg(&stage)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(worker.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let lease = std::fs::OpenOptions::new()
            .write(true)
            .open(data.path().join("app-install.lock"))
            .unwrap();
        assert!(lease.try_lock().is_err());
        // Closing the permission pipe before consent must never touch an installer,
        // even though this fixture has neither a package nor a signed manifest.
        drop(worker.stdin.take());
        let result = worker.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!data.path().join("app-update-result").exists());
        assert!(lease.try_lock().is_ok());
    }
}

#[test]
fn worker_requires_parent_exit_even_after_consent_and_has_no_installer_on_bad_package() {
    let data = tempfile::tempdir().unwrap();
    let stage = data.path().join("app-updates/0.2.0");
    std::fs::create_dir_all(&stage).unwrap();
    let binary = env!("CARGO_BIN_EXE_uxduck");
    // Deliberately missing restart binary: a failed verification still writes
    // the reason, never invoking a platform installer with this missing package.
    let plan = serde_json::json!({ "version": "0.2.0", "kind": "pkg", "executable": data.path().join("missing-uxduck"), "data_dir": data.path(), "port": 48091, "reopen": false });
    std::fs::write(stage.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    std::fs::write(data.path().join("daemon.lock"), b"").unwrap();
    let mut worker = Command::new(binary)
        .arg("__install-update")
        .arg(&stage)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    worker
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"install\n")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        worker.try_wait().unwrap().is_none(),
        "worker must wait for parent pipe to close"
    );
    assert!(!data.path().join("app-update-result").exists());
    drop(worker.stdin.take());
    assert!(!worker.wait().unwrap().success());
    assert!(
        std::fs::read_to_string(data.path().join("app-update-result"))
            .unwrap()
            .starts_with("Update not installed:")
    );
}

#[cfg(unix)]
#[test]
fn sparkle_guard_waits_for_consent_parent_exit_and_the_matching_completion() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let stage = data.path().join("updates/sparkle/0.3.1");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(data.path().join("daemon.lock"), b"").unwrap();
    let installed = data.path().join("installed");
    let restarted = data.path().join("restarted");
    std::fs::write(&installed, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'uxduck 0.3.1'; else printf '%s\\n' \"$@\" > '{}'; fi\n", restarted.display())).unwrap();
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = serde_json::json!({"version":"0.3.1", "kind":"sparkle", "executable":installed, "installed_executable":installed, "data_dir":data.path(), "port":48091, "reopen":false});
    std::fs::write(stage.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("__install-update")
        .arg(&stage)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    worker
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"install\n")
        .unwrap();
    std::fs::write(data.path().join("sparkle-installed"), "0.3.1").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(worker.try_wait().unwrap().is_none());
    assert!(
        !restarted.exists(),
        "consent alone must not restart the app"
    );
    // A completion for another version must not release the lease either.
    std::fs::write(data.path().join("sparkle-installed"), "0.3.0").unwrap();
    drop(worker.stdin.take());
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(worker.try_wait().unwrap().is_none());
    assert!(!restarted.exists());
    std::fs::write(data.path().join("sparkle-installed"), "0.3.1").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while worker.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = worker.kill();
            panic!("Sparkle guardian did not release its lease after completion");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let result = worker.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        std::fs::read_to_string(restarted)
            .unwrap()
            .contains("daemon")
    );
    assert_eq!(
        std::fs::read_to_string(data.path().join("app-update-result")).unwrap(),
        "Updated UXDuck to 0.3.1."
    );
}

#[cfg(unix)]
#[test]
fn window_reopen_failure_is_recorded_after_a_successful_update() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let stage = data.path().join("updates/sparkle/0.3.1");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(data.path().join("daemon.lock"), b"").unwrap();
    let installed = data.path().join("installed");
    std::fs::write(&installed, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'uxduck 0.3.1'; elif [ \"$3\" = open ]; then echo 'window startup failed' >&2; exit 1; fi\n").unwrap();
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let plan = serde_json::json!({"version":"0.3.1","kind":"sparkle","executable":installed,"data_dir":data.path(),"port":48091,"reopen":true,"reopen_target":"window"});
    std::fs::write(stage.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("__install-update")
        .arg(&stage)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(worker.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    std::fs::write(data.path().join("sparkle-installed"), "0.3.1").unwrap();
    worker
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"install\n")
        .unwrap();
    drop(worker.stdin.take());
    let result = worker.wait_with_output().unwrap();
    assert!(
        !result.status.success(),
        "reopen failure was silently ignored"
    );
    let detail = std::fs::read_to_string(data.path().join("app-update-result")).unwrap();
    assert!(detail.contains("Updated UXDuck to 0.3.1."), "{detail}");
    assert!(detail.contains("window startup failed"), "{detail}");
}

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
    let data = tempfile::tempdir().unwrap();
    let stage = data.path().join("app-updates/0.2.0");
    std::fs::create_dir_all(&stage).unwrap();
    let binary = env!("CARGO_BIN_EXE_uxduck");
    let plan = serde_json::json!({ "version": "0.2.0", "kind": "pkg", "executable": binary, "data_dir": data.path(), "port": 48091, "reopen": false });
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

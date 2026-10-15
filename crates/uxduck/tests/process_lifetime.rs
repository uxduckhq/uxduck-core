//! Test-owned processes must exit even when their runner cannot run teardown.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_uxduck");

fn command() -> Command {
    let mut command = Command::new(BIN);
    command
        .env("UXDUCK_POSTHOG_KEY", "none")
        .env("UXDUCK_API_URL", "none")
        .env("UXDUCK_TRAY", "")
        .env("UXDUCK_VIDEO_PORT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps is required to check process cleanup");
        let state = String::from_utf8_lossy(&output.stdout);
        output.status.success() && !state.trim().is_empty() && !state.contains('Z')
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist is required to check process cleanup");
        assert!(output.status.success(), "tasklist failed");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\",\"{pid}\",\""))
    }
}

struct Cleanup {
    child: Child,
    descendants: Vec<u32>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in &self.descendants {
            if alive(*pid) {
                #[cfg(unix)]
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
                #[cfg(windows)]
                let _ = Command::new("taskkill")
                    .args(["/F", "/PID", &pid.to_string()])
                    .status();
            }
        }
    }
}

#[test]
fn killed_parent_leaves_no_daemon_or_fake_vehicle() {
    let directory = tempfile::tempdir().unwrap();
    let runner = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "spawned_test_parent", "--ignored"])
        .env("UXDUCK_TEST_PARENT_DIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        child: runner,
        descendants: Vec::new(),
    };
    let pids = directory.path().join("children.json");
    wait_for("the runner's children", || pids.exists());
    cleanup.descendants = serde_json::from_slice(&std::fs::read(pids).unwrap()).unwrap();
    let info = directory.path().join("daemon.json");
    wait_for("the test daemon", || info.exists());
    assert!(cleanup.descendants.iter().all(|pid| alive(*pid)));

    // SIGKILL on Unix, TerminateProcess on Windows: no teardown can run.
    cleanup.child.kill().unwrap();
    cleanup.child.wait().unwrap();
    wait_for("both children to exit after their runner", || {
        cleanup.descendants.iter().all(|pid| !alive(*pid))
    });
    assert!(!info.exists(), "the daemon should shut down cleanly");
    let db = rusqlite::Connection::open(directory.path().join("uxduck.db")).unwrap();
    let checkpoints: u32 = db
        .query_row("SELECT COUNT(*) FROM checkpoint", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        checkpoints, 1,
        "parent exit should write a final checkpoint"
    );
}

#[test]
#[ignore = "a subprocess fixture for killed_parent_leaves_no_daemon_or_fake_vehicle"]
#[allow(
    clippy::zombie_processes,
    reason = "this fixture is forcibly killed; the caller verifies and cleans up its children"
)]
fn spawned_test_parent() {
    let directory = PathBuf::from(std::env::var_os("UXDUCK_TEST_PARENT_DIR").unwrap());
    let daemon = command()
        .env("UXDUCK_EXIT_ON_STDIN_CLOSE", "true")
        .args(["daemon", "--port", "0", "--no-gamepads", "--data-dir"])
        .arg(&directory)
        .spawn()
        .unwrap();
    let vehicle = command()
        .env("UXDUCK_EXIT_ON_STDIN_CLOSE", "true")
        .args(["dev", "fake-vehicle", "--to", "127.0.0.1:9"])
        .spawn()
        .unwrap();
    std::fs::write(
        directory.join("children.tmp"),
        serde_json::to_vec(&[daemon.id(), vehicle.id()]).unwrap(),
    )
    .unwrap();
    std::fs::rename(
        directory.join("children.tmp"),
        directory.join("children.json"),
    )
    .unwrap();
    loop {
        std::thread::park();
        // Keep both parent pipe handles alive until this process is killed.
        std::hint::black_box((&daemon, &vehicle));
    }
}

#[test]
fn ordinary_daemon_ignores_stdin_closure() {
    let directory = tempfile::tempdir().unwrap();
    let child = command()
        .env("UXDUCK_EXIT_ON_STDIN_CLOSE", "false")
        .args(["daemon", "--port", "0", "--no-gamepads", "--data-dir"])
        .arg(directory.path())
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        child,
        descendants: Vec::new(),
    };
    wait_for("the ordinary daemon", || {
        directory.path().join("daemon.json").exists()
    });
    drop(cleanup.child.stdin.take());
    std::thread::sleep(Duration::from_millis(300));
    assert!(cleanup.child.try_wait().unwrap().is_none());
}

#[test]
fn owned_simulator_guard_reaps_its_child() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("simulator.pid");
    let child = command()
        .env("UXDUCK_EXIT_ON_STDIN_CLOSE", "true")
        .env("UXDUCK_TEST_SIMULATOR_PID", &pid_file)
        .args(["dev", "sim-guard", "--log"])
        .arg(directory.path().join("simulator.log"))
        .arg("--")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "spawned_simulator", "--ignored"])
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        child,
        descendants: Vec::new(),
    };
    wait_for("the simulator", || pid_file.exists());
    cleanup
        .descendants
        .push(std::fs::read_to_string(pid_file).unwrap().parse().unwrap());
    drop(cleanup.child.stdin.take());
    wait_for("the guard and its child to exit", || {
        !alive(cleanup.child.id()) && cleanup.descendants.iter().all(|pid| !alive(*pid))
    });
    assert!(cleanup.child.wait().unwrap().success());
}

#[test]
#[ignore = "a subprocess fixture for owned_simulator_guard_reaps_its_child"]
fn spawned_simulator() {
    let pid_file = PathBuf::from(std::env::var_os("UXDUCK_TEST_SIMULATOR_PID").unwrap());
    let temporary = pid_file.with_extension("tmp");
    std::fs::write(&temporary, std::process::id().to_string()).unwrap();
    std::fs::rename(temporary, pid_file).unwrap();
    loop {
        std::thread::park();
    }
}

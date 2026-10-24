//! Exercise CLI download ordering and failures over the real local transport.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test fixtures")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn request_reader(stream: UnixStream) -> BufReader<UnixStream> {
    // Accepted Unix sockets inherit nonblocking mode on macOS.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    BufReader::new(stream)
}

fn exercise(mode: &str, extra: &[&str]) -> (std::process::Output, Vec<String>) {
    let dir = tempfile::Builder::new()
        .prefix("uxsim")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = UnixListener::bind(dir.path().join("uxduck.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let mode = mode.to_owned();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut downloaded = false;
        while !stopped.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "CLI did not finish");
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let mut reader = request_reader(stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap();
                recorded.lock().unwrap().push(method.to_owned());
                let result = match method {
                    "initialize" => json!({}),
                    "sim/binaries" => {
                        if mode == "installed" {
                            json!({"binaries":[{"path":"/sim/arducopter","autopilot":"ardupilot","program":"arducopter","vehicles":["multicopter"],"frames":["X"],"defaultFrame":"X"}]})
                        } else {
                            json!({"binaries":[]})
                        }
                    }
                    "sim/catalogue" => {
                        let state = if downloaded {
                            if mode == "failed" {
                                "failed"
                            } else {
                                "installed"
                            }
                        } else if mode == "prefetch" {
                            downloaded = true;
                            "downloading"
                        } else {
                            "available"
                        };
                        let program = if mode == "unavailable" {
                            "arduplane"
                        } else {
                            "arducopter"
                        };
                        let builds: Vec<Value> = ["4.7.9", "4.7.10"].iter().map(|version| json!({
                            "build":{"id":format!("{program}-{version}"),"firmware":"ardupilot","program":program,"version":version,"platform":"test","files":[],"sourceUrl":"https://example.com/source"},
                            "state":state,"detail":"download refused","totalBytes":100
                        })).collect();
                        json!({"state":"ready","builds":builds})
                    }
                    "sim/download" => {
                        assert_eq!(request["params"]["id"], "arducopter-4.7.10");
                        downloaded = true;
                        json!({})
                    }
                    "sim/start" => {
                        assert!(mode == "installed" || mode == "custom" || downloaded);
                        json!({"id":"simulation"})
                    }
                    other => panic!("unexpected {other}"),
                };
                writeln!(
                    reader.get_mut(),
                    "{}",
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                )
                .unwrap();
            }
        }
    });
    let output = Command::new(env!("CARGO_BIN_EXE_uxduck"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["sim", "start", "--vehicle", "multicopter"])
        .args(extra)
        .output()
        .unwrap();
    done.store(true, Ordering::Relaxed);
    server.join().unwrap();
    let calls = calls.lock().unwrap().clone();
    (output, calls)
}

#[test]
fn a_missing_build_downloads_the_newest_version_before_starting() {
    let (output, calls) = exercise("missing", &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        calls.iter().position(|m| m == "sim/download").unwrap()
            < calls.iter().position(|m| m == "sim/start").unwrap()
    );
}

#[test]
fn installed_and_custom_builds_never_download_and_prefetch_is_joined() {
    for (mode, args) in [
        ("installed", vec![]),
        ("custom", vec!["--binary", "/my/arducopter"]),
        ("prefetch", vec![]),
    ] {
        let (output, calls) = exercise(mode, &args);
        assert!(output.status.success(), "{mode}: {output:?}");
        assert!(!calls.iter().any(|m| m == "sim/download"));
        assert!(calls.iter().any(|m| m == "sim/start"));
    }
}

#[test]
fn a_failed_or_unavailable_download_does_not_start_a_simulation() {
    for mode in ["failed", "unavailable"] {
        let (output, calls) = exercise(mode, &[]);
        assert!(!output.status.success());
        assert!(!calls.iter().any(|m| m == "sim/start"));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains(if mode == "failed" {
                "download refused"
            } else {
                "No arducopter"
            }),
            "{error}"
        );
    }
}

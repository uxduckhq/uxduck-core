//! The daemon stays responsive with a full, unread console pipe.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn call(reader: &mut BufReader<UnixStream>, id: u8, method: &str, params: &Value) -> Value {
    writeln!(
        reader.get_mut(),
        "{}",
        json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
    )
    .unwrap();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).expect("responsive daemon") > 0);
        let response: Value = serde_json::from_str(&line).unwrap();
        if response["id"] == id {
            assert!(response.get("error").is_none(), "{response}");
            return response["result"].clone();
        }
    }
}

fn flood_console(endpoint: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    // Each accepted connection produces a debug or disconnect-warning line;
    // this is substantially more output than a pipe can hold on either Unix.
    for _ in 0..4096 {
        loop {
            match UnixStream::connect(endpoint) {
                Ok(stream) => {
                    drop(stream);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    // A short accept-queue burst must not be mistaken for
                    // stalled logging. The old writer stays stuck here.
                    assert!(Instant::now() < deadline, "console blocked the listener");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("connecting to daemon: {error}"),
            }
        }
    }
}

#[test]
fn an_unread_console_pipe_cannot_stall_vehicle_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Running(
        Command::new(env!("CARGO_BIN_EXE_uxduck"))
            .args(["daemon", "--port", "0", "--data-dir"])
            .arg(dir.path())
            .env("RUST_LOG", "uxduck=debug")
            .env("UXDUCK_POSTHOG_KEY", "none")
            .env("UXDUCK_API_URL", "none")
            .env("UXDUCK_TRAY", "")
            .env("UXDUCK_VIDEO_PORT", "0")
            .stdout(Stdio::null())
            // No reader: fill the OS pipe and then the bounded log queue.
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let endpoint = loop {
        if let Some(endpoint) = std::fs::read_to_string(dir.path().join("daemon.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|info| info["socket"].as_str().map(str::to_owned))
        {
            break endpoint;
        }
        assert!(Instant::now() < deadline, "daemon startup timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    let stream = UnixStream::connect(&endpoint).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    call(
        &mut reader,
        1,
        "initialize",
        &json!({
            "clientInfo": {"name": "blocked-console-test", "version": "0.1.0", "kind": "operator-cli"},
            "capabilities": {"protocolVersion": uxduck_contracts::version::PROTOCOL_VERSION},
        }),
    );
    let (finished, waiting) = std::sync::mpsc::sync_channel(1);
    let flood = std::thread::spawn(move || {
        flood_console(&endpoint);
        finished.send(()).unwrap();
    });
    // On Linux a full Unix accept queue can block connect itself. The
    // test's main thread can still time out and kill its daemon guard.
    waiting
        .recv_timeout(Duration::from_secs(15))
        .expect("console cannot stall the listener");
    flood.join().unwrap();
    assert_eq!(
        call(&mut reader, 2, "vehicle/list", &json!({}))["vehicles"],
        json!([])
    );
    drop(daemon);
}

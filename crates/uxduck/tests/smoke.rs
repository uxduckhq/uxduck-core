//! A standalone Core client can start the daemon, query RPC and connect MCP.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_uxduck");

fn command(directory: &Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(["--data-dir"])
        .arg(directory)
        .env("UXDUCK_API_URL", "none")
        .env("UXDUCK_POSTHOG_KEY", "none")
        .env("UXDUCK_TERRAIN_URL", "none")
        .env("UXDUCK_VIDEO_PORT", "0")
        .env_remove("SSH_CONNECTION")
        .env_remove("UXDUCK_NO_TRAY")
        .kill_on_drop(true);
    command
}

async fn rpc(directory: &Path, method: &str) -> Value {
    let output = command(directory)
        .args(["rpc", "call", method])
        .output()
        .await
        .expect("run CLI");
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("RPC JSON")
}

async fn stop(mut child: Child) {
    drop(child.stdin.take());
    assert!(child.wait().await.expect("wait for clean exit").success());
}

async fn check_mcp(directory: &Path) {
    let mut mcp = command(directory)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start MCP stdio client");
    let mut input = mcp.stdin.take().expect("MCP stdin");
    let mut output = BufReader::new(mcp.stdout.take().expect("MCP stdout")).lines();
    for (id, method, params) in [
        (
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": { "name": "Core smoke test", "version": "1" }
            }),
        ),
        (2, "tools/list", json!({})),
        (
            3,
            "tools/call",
            json!({ "name": "vehicle_list", "arguments": {} }),
        ),
    ] {
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("write MCP request");
        let line = output
            .next_line()
            .await
            .expect("read MCP reply")
            .expect("MCP response");
        let reply: Value = serde_json::from_str(&line).expect("MCP JSON");
        assert_eq!(reply["id"], id);
        assert!(reply.get("error").is_none(), "{reply}");
        match id {
            1 => assert_eq!(reply["result"]["serverInfo"]["name"], "uxduck"),
            2 => assert!(
                reply["result"]["tools"]
                    .as_array()
                    .expect("tool list")
                    .iter()
                    .any(|tool| tool["name"] == "vehicle_list")
            ),
            3 => {
                assert_ne!(reply["result"]["isError"], true, "{reply}");
                let result: Value = serde_json::from_str(
                    reply["result"]["content"][0]["text"]
                        .as_str()
                        .expect("tool result"),
                )
                .expect("tool JSON");
                assert_eq!(result["vehicles"], json!([]));
            }
            _ => unreachable!(),
        }
    }
    drop(input);
    stop(mcp).await;
}

async fn smoke() {
    let directory = tempfile::tempdir().expect("isolated Core data");
    let stderr = directory.path().join("daemon.stderr");
    let mut daemon = command(directory.path())
        .args(["--exit-on-stdin-close", "daemon", "--port", "0"])
        .env("DISPLAY", ":core-smoke")
        // The product enables its tray explicitly. Its test session disables
        // that feature's UI; the default Core build must ignore a missing helper.
        .env(
            "UXDUCK_TRAY",
            if cfg!(feature = "desktop-tray") {
                String::new()
            } else {
                directory.path().join("missing-tray").display().to_string()
            },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("daemon diagnostics"))
        .spawn()
        .expect("start Core daemon");
    while !directory.path().join("daemon.json").exists() {
        assert!(
            daemon.try_wait().expect("daemon status").is_none(),
            "daemon exited: {}",
            std::fs::read_to_string(&stderr).expect("read daemon diagnostics")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(rpc(directory.path(), "daemon/about").await.is_object());
    assert_eq!(
        rpc(directory.path(), "vehicle/list").await["vehicles"],
        json!([])
    );

    check_mcp(directory.path()).await;
    stop(daemon).await;
}

#[tokio::test]
async fn daemon_rpc_and_mcp_work_in_a_standalone_core_build() {
    tokio::time::timeout(Duration::from_secs(30), smoke())
        .await
        .expect("Core startup, RPC, MCP and clean shutdown must finish within 30 seconds");
}

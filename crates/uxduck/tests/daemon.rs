//! The real `uxduck` binary: the daemon, the CLI against it over the local
//! socket, and a browser's view of the loopback listener (S1 demo).

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BIN: &str = env!("CARGO_BIN_EXE_uxduck");

#[path = "daemon/shared_forwarding.rs"]
mod shared_forwarding;

#[path = "daemon/vehicle_files.rs"]
mod vehicle_files;

#[cfg(unix)]
#[path = "daemon/simulation_profiles.rs"]
mod simulation_profiles;

#[cfg(unix)]
#[path = "daemon/simulator_stops.rs"]
mod simulator_stops;

#[cfg(unix)]
#[path = "daemon/practice_guide.rs"]
mod practice_guide;

#[cfg(unix)]
#[path = "daemon/terrain_sitl.rs"]
mod terrain_sitl;

#[cfg(unix)]
#[path = "../src/sim/cache.rs"]
mod simulator_cache;

#[cfg(unix)]
fn native_simulator(program: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("UXDUCK_SITL_DIR")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(simulator_cache::directories())
        .map(|path| path.join(program))
        .find(|path| path.is_file())
}

struct Daemon {
    child: Child,
    // Child::wait closes its stdin. Keep the lifetime pipe separately so
    // waiting for a requested panic or restart does not request a clean exit.
    _parent_pipe: ChildStdin,
    /// Taken by `kill_and_restart`, so the directory outlives the process.
    dir: Option<tempfile::TempDir>,
    port: u16,
}

impl Daemon {
    fn start(extra: &[&str]) -> Self {
        Self::start_in(tempfile::tempdir().expect("temp dir"), extra)
    }

    /// Starts a daemon on an existing data directory. A `daemon.json` left
    /// by a killed daemon is removed first, so the wait sees the new one.
    fn start_in(dir: tempfile::TempDir, extra: &[&str]) -> Self {
        Self::run_in(dir, &["daemon"], extra)
    }

    /// Runs `uxduck <command> --port 0 --data-dir <dir> <extra>` and waits
    /// for it to listen. Analytics are off unless `env` turns them on.
    fn run_in(dir: tempfile::TempDir, command: &[&str], extra: &[&str]) -> Self {
        Self::run_with_env(dir, command, extra, &[])
    }

    fn run_with_env(
        dir: tempfile::TempDir,
        command: &[&str],
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let _ = std::fs::remove_file(dir.path().join("daemon.json"));
        let mut child = Command::new(BIN)
            .env("UXDUCK_POSTHOG_KEY", "none")
            .env("UXDUCK_API_URL", "none")
            // Tests that need terrain supply their own signed fixture source.
            .env("UXDUCK_TERRAIN_URL", "none")
            // Any free port, so test daemons never fight over 5600.
            .env("UXDUCK_VIDEO_PORT", "0")
            // No tray icon from tests, unless one asks for a fake (S64).
            .env("UXDUCK_TRAY", "")
            .env_remove("SSH_CONNECTION")
            .env_remove("UXDUCK_NO_TRAY")
            .envs(env.iter().copied())
            .arg("--exit-on-stdin-close")
            .args(command)
            .args(["--port", "0", "--data-dir"])
            .arg(dir.path())
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.path().join("daemon.stderr")).unwrap())
            .spawn()
            .expect("spawn daemon");
        let parent_pipe = child.stdin.take().expect("parent lifetime pipe");
        let info = dir.path().join("daemon.json");
        let deadline = Instant::now() + Duration::from_secs(30);
        let port = loop {
            if let Some(port) = std::fs::read_to_string(&info)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|info| info["port"].as_u64())
            {
                break u16::try_from(port).expect("port");
            }
            let exited = child.try_wait().expect("check daemon status");
            assert!(
                exited.is_none() && Instant::now() < deadline,
                "the daemon did not start ({exited:?}): {}",
                std::fs::read_to_string(dir.path().join("daemon.stderr")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        Self {
            child,
            _parent_pipe: parent_pipe,
            dir: Some(dir),
            port,
        }
    }

    fn dir(&self) -> &Path {
        self.dir.as_ref().expect("data dir").path()
    }

    /// `kill -9` (`TerminateProcess` on Windows), then a new daemon on the
    /// same data directory.
    fn kill_and_restart(mut self, extra: &[&str]) -> Self {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let dir = self.dir.take().expect("data dir");
        drop(self);
        Self::start_in(dir, extra)
    }

    /// The CLI with `input` on standard input, as a person typing.
    fn cli_with_input(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(BIN)
            .args(args)
            .arg("--data-dir")
            .arg(self.dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the CLI");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("type");
        child.wait_with_output().expect("the CLI's output")
    }

    /// `kill_and_restart` with an environment.
    fn kill_and_restart_with_env(mut self, extra: &[&str], env: &[(&str, &str)]) -> Self {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let dir = self.dir.take().expect("data dir");
        drop(self);
        Self::run_with_env(dir, &["daemon"], extra, env)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .arg("--data-dir")
            .arg(self.dir())
            .output()
            .expect("run uxduck")
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn http(&self, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        (status, response)
    }

    fn get(&self, path: &str, headers: &str) -> (u16, String) {
        self.http(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{headers}\r\n",
            self.port
        ))
    }

    fn post(&self, path: &str, origin: &str, body: &str) -> (u16, String) {
        self.http(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.port,
            body.len()
        ))
    }

    async fn websocket(
        &self,
        cookie: Option<&str>,
    ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://127.0.0.1:{}/rpc", self.port)
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("Origin", self.origin().parse().expect("header"));
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("Cookie", cookie.parse().expect("header"));
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_call(socket: &mut Socket, id: i64, method: &str, params: Value) -> Value {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .expect("send");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("reply in time");
        if let Some(Ok(Message::Text(text))) = message {
            let value: Value = serde_json::from_str(&text).expect("JSON");
            if value["id"] == id {
                return value;
            }
        }
    }
}

fn operator_ui() -> Value {
    json!({ "clientInfo": { "name": "uxduck-web", "version": "0.1.0", "kind": "operator-ui" }, "capabilities": { "protocolVersion": 1 } })
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON on stdout")
}

fn session_cookie(response: &str) -> String {
    let line = response
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap_or_else(|| panic!("no Set-Cookie in {response}"));
    assert!(
        line.contains("HttpOnly") && line.contains("SameSite=Strict"),
        "{line}"
    );
    line.split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn body(response: &str) -> Value {
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or_default()).expect("JSON body")
}

#[test]
fn rpc_call_initialize_prints_the_handshake() {
    let daemon = Daemon::start(&[]);
    let result = stdout_json(&daemon.cli(&["rpc", "call", "initialize"]));
    assert_eq!(result["daemonVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(
        result["granted"],
        json!(["read", "control", "config", "approve"])
    );
}

/// `daemon/about` (S87): the version, the build `build.rs` stamped, where
/// the data lives and the port, which `uxduck --version` prints too.
#[test]
fn daemon_about_names_this_build_and_where_it_listens() {
    let daemon = Daemon::start(&[]);
    let about = stdout_json(&daemon.cli(&["rpc", "call", "daemon/about"]));
    assert_eq!(about["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(about["runsIn"], "installed");
    assert_eq!(about["port"], daemon.port);
    let data_dir = std::fs::canonicalize(about["dataDir"].as_str().expect("dataDir"));
    assert_eq!(data_dir.ok(), std::fs::canonicalize(daemon.dir()).ok());
    let version = Command::new(BIN)
        .arg("--version")
        .output()
        .expect("uxduck --version");
    let version = String::from_utf8_lossy(&version.stdout);
    let target = about["target"].as_str().expect("target");
    assert!(version.contains(&format!("target {target}")), "{version}");
    if let Some(commit) = about["commit"].as_str() {
        assert!(version.contains(&format!("commit {commit}")), "{version}");
    }
    let built = about["builtAt"].as_str().expect("builtAt");
    assert!(version.contains(&format!("built {built}")), "{version}");
}

/// Resource budgets (S24): an idle daemon's timers do not run. Only the
/// 1 Hz housekeeping and the engine's own work may wake it.
#[test]
fn an_idle_daemon_does_not_run_its_data_plane_or_poll_for_restarts() {
    let daemon = Daemon::start(&["--no-gamepads"]);
    std::thread::sleep(Duration::from_secs(2));
    let wakeups = stdout_json(&daemon.cli(&["rpc", "call", "daemon/wakeups"]));
    let tasks: Vec<(&str, u64)> = wakeups["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap_or_default(),
                t["count"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert!(
        tasks
            .iter()
            .all(|(name, _)| !name.starts_with("data plane")),
        "the data plane's timers wait for links: {tasks:?}"
    );
    let engine = tasks
        .iter()
        .find(|(name, _)| *name == "engine")
        .map_or(0, |(_, count)| *count);
    assert!(
        engine < 40,
        "the engine is not polled ten times a second: {tasks:?}"
    );
}

#[test]
fn an_unknown_method_is_an_error_with_a_stable_code() {
    let daemon = Daemon::start(&[]);
    let output = daemon.cli(&["rpc", "call", "vehicle/fly"]);
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr).expect("error JSON");
    assert_eq!(error["code"], -32601);
}

#[test]
fn the_cli_explains_when_no_daemon_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .args(["rpc", "call", "initialize", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no daemon is running"));
}

#[test]
fn a_second_daemon_on_the_same_directory_is_refused() {
    let daemon = Daemon::start(&[]);
    let output = Command::new(BIN)
        .args(["daemon", "--port", "0", "--data-dir"])
        .arg(daemon.dir())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("already running"));
}

#[test]
fn trace_shows_other_connections_frames() {
    let daemon = Daemon::start(&[]);
    let mut trace = Command::new(BIN)
        .args(["trace", "--data-dir"])
        .arg(daemon.dir())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn trace");
    // Wait until the tracer is subscribed: it is the only other connection.
    std::thread::sleep(Duration::from_millis(500));
    stdout_json(&daemon.cli(&["rpc", "call", "pairing/list"]));
    std::thread::sleep(Duration::from_millis(300));
    let _ = trace.kill();
    let _ = trace.wait();
    let mut out = String::new();
    trace
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let frames: Vec<Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).expect("NDJSON"))
        .collect();
    let methods: Vec<(&str, &str)> = frames
        .iter()
        .map(|f| {
            (
                f["direction"].as_str().unwrap_or(""),
                f["method"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert!(methods.contains(&("in", "initialize")), "{out}");
    assert!(methods.contains(&("in", "pairing/list")), "{out}");
    assert!(
        frames
            .iter()
            .any(|f| f["direction"] == "out" && f["latencyUs"].is_u64()),
        "{out}"
    );
}

#[test]
fn the_listener_refuses_foreign_hosts_and_origins() {
    let daemon = Daemon::start(&[]);
    let (status, _) = daemon
        .http("GET /api/health HTTP/1.1\r\nHost: evil.example:80\r\nConnection: close\r\n\r\n");
    assert_eq!(status, 403, "DNS rebinding");
    let (status, response) = daemon.get("/api/health", "");
    assert_eq!(status, 200);
    assert_eq!(body(&response)["daemonVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        daemon.post("/api/pairing", "https://evil.example", "").0,
        403
    );
    let (status, _) = daemon.http(&format!(
        "POST /api/pairing HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        daemon.port
    ));
    assert_eq!(status, 403, "a write without an Origin");
}

#[tokio::test]
async fn a_browser_without_a_session_cannot_open_the_websocket() {
    let daemon = Daemon::start(&[]);
    let (_, response) = daemon.get("/api/session", "");
    assert_eq!(body(&response)["paired"], false);
    match daemon.websocket(None).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
}

#[tokio::test]
async fn uxduck_open_pairs_the_browser_through_a_bootstrap_token() {
    let daemon = Daemon::start(&[]);
    let output = daemon.cli(&["open", "--print"]);
    assert!(output.status.success());
    let url = String::from_utf8(output.stdout).unwrap();
    let token = url
        .trim()
        .split("bootstrap=")
        .nth(1)
        .expect("token in URL")
        .to_owned();
    assert!(url.starts_with(&daemon.origin()));

    let (status, response) = daemon.post(
        "/api/session",
        &daemon.origin(),
        &json!({ "bootstrap": token }).to_string(),
    );
    assert_eq!(status, 200);
    let cookie = session_cookie(&response);
    assert_eq!(
        daemon
            .post(
                "/api/session",
                &daemon.origin(),
                &json!({ "bootstrap": token }).to_string()
            )
            .0,
        403,
        "once only"
    );
    let (_, response) = daemon.get("/api/session", &format!("Cookie: {cookie}\r\n"));
    assert_eq!(body(&response)["paired"], true);

    let mut socket = daemon.websocket(Some(&cookie)).await.expect("websocket");
    let reply = ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    assert_eq!(
        reply["result"]["granted"],
        json!(["read", "control", "config", "approve"])
    );
    let reply = ws_call(&mut socket, 2, "session/createBootstrap", json!({})).await;
    assert_eq!(
        reply["error"]["data"]["kind"], "capability-denied",
        "tokens only over the local socket"
    );
}

#[tokio::test]
async fn an_unknown_browser_pairs_when_an_operator_approves_its_code() {
    let daemon = Daemon::start(&[]);
    let (status, response) = daemon.post("/api/pairing", &daemon.origin(), "");
    assert_eq!(status, 200);
    let started = body(&response);
    let (id, code) = (
        started["id"].as_str().unwrap().to_owned(),
        started["code"].as_str().unwrap().to_owned(),
    );

    let (_, response) = daemon.get(&format!("/api/pairing/{id}"), "");
    assert_eq!(body(&response)["state"], "pending");

    let listed = stdout_json(&daemon.cli(&["rpc", "call", "pairing/list"]));
    assert_eq!(listed["requests"][0]["code"], code);
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "pairing/approve",
        "--json",
        &json!({ "code": code }).to_string(),
    ]));

    let (_, response) = daemon.get(&format!("/api/pairing/{id}"), "");
    assert_eq!(body(&response)["state"], "approved");
    let cookie = session_cookie(&response);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("websocket");
    assert_eq!(
        ws_call(&mut socket, 1, "initialize", operator_ui()).await["result"]["protocolVersion"],
        1
    );
}

#[tokio::test]
async fn uxduck_on_the_web_pairs_for_a_token_and_connects_with_it() {
    // ADR-0011 open decision 1: app.uxduck.com is another site, so no
    // cookie; it keeps the token and sends it as a subprotocol.
    let daemon = Daemon::start(&[]);
    let web = "https://app.uxduck.com";
    let (status, response) = daemon.http(&format!(
        "OPTIONS /api/pairing HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {web}\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Private-Network: true\r\nConnection: close\r\n\r\n",
        daemon.port
    ));
    assert_eq!(status, 204, "{response}");
    let lower = response.to_lowercase();
    assert!(
        lower.contains("access-control-allow-private-network: true"),
        "{response}"
    );
    assert!(
        lower.contains(&format!("access-control-allow-origin: {web}")),
        "{response}"
    );

    let (status, response) = daemon.post("/api/pairing", web, "");
    assert_eq!(status, 200);
    assert!(
        response
            .to_lowercase()
            .contains(&format!("access-control-allow-origin: {web}"))
    );
    let started = body(&response);
    let (id, code) = (
        started["id"].as_str().unwrap().to_owned(),
        started["code"].as_str().unwrap().to_owned(),
    );
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "pairing/approve",
        "--json",
        &json!({ "code": code }).to_string(),
    ]));
    let from_web = format!("Origin: {web}\r\n");
    let (_, response) = daemon.get(&format!("/api/pairing/{id}"), &from_web);
    assert_eq!(body(&response)["state"], "approved");
    assert!(
        !response.to_lowercase().contains("set-cookie"),
        "{response}"
    );
    let token = body(&response)["token"].as_str().expect("token").to_owned();

    let bearer = format!("{from_web}Authorization: Bearer {token}\r\n");
    assert_eq!(body(&daemon.get("/api/session", &bearer).1)["paired"], true);
    let staging =
        format!("Origin: https://app-staging.uxduck.com\r\nAuthorization: Bearer {token}\r\n");
    assert_eq!(
        body(&daemon.get("/api/session", &staging).1)["paired"],
        false,
        "only from the origin that paired"
    );

    let open = |origin: &str| {
        let mut request = format!("ws://127.0.0.1:{}/rpc", daemon.port)
            .into_client_request()
            .expect("request");
        let headers = request.headers_mut();
        headers.insert("Origin", origin.parse().expect("header"));
        headers.insert(
            "Sec-WebSocket-Protocol",
            format!("uxduck, uxduck-token.{token}")
                .parse()
                .expect("header"),
        );
        tokio_tungstenite::connect_async(request)
    };
    let (mut socket, response) = open(web).await.expect("websocket");
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .map(|v| v.to_str().unwrap()),
        Some("uxduck")
    );
    assert_eq!(
        ws_call(&mut socket, 1, "initialize", operator_ui()).await["result"]["protocolVersion"],
        1
    );
    match open("https://app-staging.uxduck.com").await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
}

#[test]
fn dev_mode_allows_the_dev_server_and_sends_the_browser_there() {
    let daemon = Daemon::start(&["--dev", "--dev-origin", "http://localhost:5173"]);
    let (status, _) = daemon.http(
        "POST /api/pairing HTTP/1.1\r\nHost: localhost:5173\r\nOrigin: http://localhost:5173\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(
        status, 200,
        "the Vite proxy forwards its own Host and Origin"
    );
    let url = String::from_utf8(daemon.cli(&["open", "--print"]).stdout).unwrap();
    assert!(
        url.starts_with("http://localhost:5173/?bootstrap="),
        "{url}"
    );
}

fn operators(daemon: &Daemon) -> Vec<Value> {
    stdout_json(&daemon.cli(&["rpc", "call", "operator/list"]))["operators"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn names(daemon: &Daemon) -> Vec<String> {
    let mut names: Vec<String> = operators(daemon)
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

fn create_operator(daemon: &Daemon, name: &str) -> Value {
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "operator/create",
        "--json",
        &json!({ "name": name }).to_string(),
    ]))
}

/// A session cookie from `uxduck open`.
fn paired_cookie(daemon: &Daemon) -> String {
    let url = String::from_utf8(daemon.cli(&["open", "--print"]).stdout).unwrap();
    let token = url
        .trim()
        .split("bootstrap=")
        .nth(1)
        .expect("token")
        .to_owned();
    let (_, response) = daemon.post(
        "/api/session",
        &daemon.origin(),
        &json!({ "bootstrap": token }).to_string(),
    );
    session_cookie(&response)
}

#[test]
fn operators_survive_kill_9() {
    for checkpoint_every in ["500", "2"] {
        let extra = ["--checkpoint-every", checkpoint_every];
        let daemon = Daemon::start(&extra);
        create_operator(&daemon, "Ana");
        let ben = create_operator(&daemon, "Ben");
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "operator/update",
            "--json",
            &json!({ "id": ben["id"], "expectedVersion": 1, "name": "Ben K" }).to_string(),
        ]));
        assert_eq!(names(&daemon), ["Ana", "Ben K"]);
        // Writes reach the OS at the end of each batch; give it one.
        std::thread::sleep(Duration::from_millis(300));

        let daemon = daemon.kill_and_restart(&extra);
        assert_eq!(
            names(&daemon),
            ["Ana", "Ben K"],
            "checkpoint every {checkpoint_every}"
        );
        let ben = operators(&daemon)
            .into_iter()
            .find(|o| o["name"] == "Ben K")
            .unwrap();
        assert_eq!(ben["version"], 2);
        // The sequence continues: a new change still applies.
        create_operator(&daemon, "Cy");
        assert_eq!(names(&daemon), ["Ana", "Ben K", "Cy"]);
    }
}

#[tokio::test]
async fn a_paired_browser_stays_paired_across_a_restart() {
    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    std::thread::sleep(Duration::from_millis(300));
    let daemon = daemon.kill_and_restart(&[]);
    let (_, response) = daemon.get("/api/session", &format!("Cookie: {cookie}\r\n"));
    assert_eq!(body(&response)["paired"], true);
    assert!(daemon.websocket(Some(&cookie)).await.is_ok());
}

#[tokio::test]
async fn subscribers_joining_under_load_see_every_seq_after_their_snapshot() {
    const UPDATES: u64 = 300;
    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);

    let mut writer = daemon.websocket(Some(&cookie)).await.unwrap();
    ws_call(&mut writer, 1, "initialize", operator_ui()).await;
    let created = ws_call(&mut writer, 2, "operator/create", json!({ "name": "Ana" })).await;
    let id = created["result"]["id"].clone();
    let last_seq = 1 + UPDATES;

    let writing = tokio::spawn(async move {
        for version in 1..=UPDATES {
            let reply = ws_call(
                &mut writer,
                i64::try_from(version).unwrap() + 2,
                "operator/update",
                json!({ "id": id, "expectedVersion": version, "name": format!("Ana {version}") }),
            )
            .await;
            assert!(reply.get("result").is_some(), "{reply}");
        }
    });

    let mut readers = Vec::new();
    for _ in 0..6 {
        let mut socket = daemon.websocket(Some(&cookie)).await.unwrap();
        readers.push(tokio::spawn(async move {
            ws_call(&mut socket, 1, "initialize", operator_ui()).await;
            let subscribed = ws_call(
                &mut socket,
                2,
                "state/subscribe",
                json!({ "entities": ["operator"] }),
            )
            .await;
            let snapshot = subscribed["result"]["seq"].as_u64().unwrap();
            let mut seen = Vec::new();
            while seen.last().copied().unwrap_or(snapshot) < last_seq {
                let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
                    .await
                    .expect("deltas in time");
                if let Some(Ok(Message::Text(text))) = message {
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    if frame["method"] == "state/delta" {
                        seen.push(frame["params"]["seq"].as_u64().unwrap());
                    }
                }
            }
            (snapshot, seen)
        }));
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    writing.await.unwrap();
    for reader in readers {
        let (snapshot, seen) = reader.await.unwrap();
        let expected: Vec<u64> = (snapshot + 1..=last_seq).collect();
        assert_eq!(seen, expected, "snapshot at {snapshot}");
    }
}

fn items_of(daemon: &Daemon, thread_id: &str) -> Vec<Value> {
    let snapshot = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["item"]}"#,
    ]));
    snapshot["snapshot"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["threadId"] == thread_id)
        .cloned()
        .collect()
}

/// Waits until the thread's last item is a completed agent message.
fn wait_for_answer(daemon: &Daemon, thread_id: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let items = items_of(daemon, thread_id);
        if items.len() >= count
            && items
                .last()
                .is_some_and(|i| i["kind"] == "agentMessage" && i["status"] == "completed")
        {
            return items;
        }
        assert!(Instant::now() < deadline, "no answer: {items:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn send(daemon: &Daemon, thread_id: &str, text: &str) {
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "thread/send",
        "--json",
        &json!({ "threadId": thread_id, "text": text }).to_string(),
    ]));
}

#[test]
fn a_thread_continues_after_kill_9() {
    let extra = ["--agent-provider", "fake"];
    let daemon = Daemon::start(&extra);
    let thread = stdout_json(&daemon.cli(&["rpc", "call", "thread/create"]));
    let id = thread["id"].as_str().unwrap().to_owned();
    send(&daemon, &id, "what vehicles do I have");
    let items = wait_for_answer(&daemon, &id, 3);
    assert_eq!(items[1]["toolCall"]["method"], "vehicle/list");
    assert_eq!(items[1]["toolCall"]["result"], json!({ "vehicles": [] }));
    std::thread::sleep(Duration::from_millis(300));

    let daemon = daemon.kill_and_restart(&extra);
    assert_eq!(items_of(&daemon, &id).len(), 3, "the thread is still there");
    send(&daemon, &id, "and now?");
    let items = wait_for_answer(&daemon, &id, 5);
    assert_eq!(items[3]["text"], "and now?");
    assert_eq!(items[4]["index"], 4);
}

/// A development build's `auto` provider (S29): signed out and with no key
/// of its own it is the hosted model, which asks to sign in; with a key it
/// uses the key.
#[test]
fn config_set_stores_the_key_and_the_agent_becomes_ready() {
    let daemon = Daemon::start(&[]);
    let config = stdout_json(&daemon.cli(&["rpc", "call", "agent/config"]));
    assert_eq!(config["provider"], "hosted");
    assert_eq!(config["ready"], false);
    let thread = stdout_json(&daemon.cli(&["rpc", "call", "thread/create"]));
    let id = thread["id"].as_str().unwrap().to_owned();
    send(&daemon, &id, "hello");
    let deadline = Instant::now() + Duration::from_secs(10);
    let error = loop {
        if let Some(error) = items_of(&daemon, &id)
            .into_iter()
            .find(|i| i["kind"] == "error")
        {
            break error;
        }
        assert!(Instant::now() < deadline, "no error item");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        error["text"]
            .as_str()
            .unwrap()
            .contains("Sign in to UXDuck in Settings > Account"),
        "{error}"
    );

    let output = daemon.cli(&["config", "set", "openai.key", "sk-test"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = stdout_json(&daemon.cli(&["rpc", "call", "agent/config"]));
    assert_eq!(config["provider"], "openai");
    assert_eq!(config["ready"], true);
    let stored = std::fs::read_to_string(daemon.dir().join("credentials.json")).unwrap();
    assert!(stored.contains("sk-test"));
}

/// A UDP port nobody holds right now.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn vehicles(daemon: &Daemon) -> Vec<Value> {
    stdout_json(&daemon.cli(&["rpc", "call", "vehicle/list"]))["vehicles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_udp_vehicle_is_discovered_identified_recorded_and_goes_offline() {
    use uxduck_mavlink::sim::{FakeVehicle, terrain_request_frame};

    let daemon = Daemon::start(&[
        "--presence-stale-ms",
        "600",
        "--presence-offline-ms",
        "1200",
    ]);
    let port = free_udp_port();
    let link =
        stdout_json(
            &daemon.cli(&[
                "rpc",
                "call",
                "link/add",
                "--json",
                &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } })
                    .to_string(),
            ]),
        );
    wait_for("the link to come up", || {
        let links = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["link"]}"#,
        ]));
        links["snapshot"]["links"][0]["status"] == "up"
    });

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let target = format!("127.0.0.1:{port}");
    let mut fake = FakeVehicle::new(4, 0xFEED);
    let mut sent = Vec::new();
    let mut buffer = [0u8; 2048];
    let mut requests = 0;
    for _ in 0..6 {
        let heartbeat = fake.heartbeat();
        socket.send_to(&heartbeat, &target).unwrap();
        sent.push(heartbeat);
        while let Ok((len, _)) = socket.recv_from(&mut buffer) {
            requests += 1;
            for reply in fake.answer(&buffer[..len]) {
                socket.send_to(&reply, &target).unwrap();
                sent.push(reply);
            }
        }
    }
    assert!(
        requests >= 1,
        "the daemon asked for AUTOPILOT_VERSION on our own port"
    );
    let vehicle = vehicles(&daemon).into_iter().next().expect("a vehicle");
    assert_eq!(
        vehicle["id"],
        uxduck_mavlink::identity::vid_from_uid(0xFEED, [0; 18]).unwrap()
    );
    assert_eq!(
        (vehicle["systemId"].as_u64(), vehicle["presence"].as_str()),
        (Some(4), Some("online"))
    );
    assert_eq!(vehicle["linkId"], link["id"]);
    assert_eq!(vehicle["firmware"], "4.5.1");

    // A terrain request is never answered (S23).
    let terrain = terrain_request_frame(4, 1, 99);
    socket.send_to(&terrain, &target).unwrap();
    sent.push(terrain);
    assert!(
        socket.recv_from(&mut buffer).is_err(),
        "no reply to TERRAIN_REQUEST"
    );

    wait_for("stale then offline", || {
        vehicles(&daemon)[0]["presence"] == "offline"
    });

    let tlogs: Vec<_> = std::fs::read_dir(daemon.dir().join("tlogs"))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(tlogs.len(), 1);
    let log = std::fs::read(tlogs[0].path()).unwrap();
    let recorded: Vec<Vec<u8>> = uxduck_mavlink::tlog::read(&log)
        .into_iter()
        .map(|(_, frame)| frame.to_vec())
        .collect();
    assert_eq!(recorded, sent, "every frame, byte for byte, in order");
}

#[test]
fn a_busy_port_is_refused_on_add_and_reported_on_start_with_its_owner() {
    let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let daemon = Daemon::start(&[]);
    let add = |daemon: &Daemon| {
        daemon.cli(&[
            "rpc",
            "call",
            "link/add",
            "--json",
            &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } })
                .to_string(),
        ])
    };
    // Adding checks the bind first and names who holds the port.
    let refused = add(&daemon);
    assert!(!refused.status.success());
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["data"]["kind"], "port-in-use");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("UDP port {port} is in use by")),
        "{message}"
    );
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert!(
            message.contains(&format!("(pid {})", std::process::id())),
            "{message}"
        );
    }

    // Taken while the daemon was down: start reports it and keeps running.
    drop(holder);
    assert!(add(&daemon).status.success());
    let holder = std::net::UdpSocket::bind(("127.0.0.1", port)).ok();
    let daemon = daemon.kill_and_restart(&[]);
    if holder.is_some() {
        wait_for("the port-in-use status", || {
            let links = stdout_json(&daemon.cli(&[
                "rpc",
                "call",
                "state/subscribe",
                "--json",
                r#"{"entities":["link"]}"#,
            ]));
            links["snapshot"]["links"][0]["status"] == "port-in-use"
                && links["snapshot"]["links"][0]["statusDetail"]
                    .as_str()
                    .is_some_and(|d| d.contains("is in use by"))
        });
    }
    assert!(
        stdout_json(&daemon.cli(&["rpc", "call", "agent/config"])).is_object(),
        "the daemon keeps running"
    );
}

#[test]
fn a_read_only_link_never_transmits() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port }, "policy": "read" })
            .to_string(),
    ]);
    wait_for("the link to come up", || {
        let links = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["link"]}"#,
        ]));
        links["snapshot"]["links"][0]["status"] == "up"
    });
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut fake = FakeVehicle::new(2, 0xBEEF);
    let mut buffer = [0u8; 2048];
    for _ in 0..4 {
        socket
            .send_to(&fake.heartbeat(), format!("127.0.0.1:{port}"))
            .unwrap();
        assert!(
            socket.recv_from(&mut buffer).is_err(),
            "a read-only link sent a frame"
        );
    }
    let vehicle = vehicles(&daemon).into_iter().next().expect("heard anyway");
    assert_eq!(vehicle["uid"], Value::Null, "no UID without asking");
}

#[test]
fn telemetry_from_a_udp_vehicle_reaches_telemetry_get() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]);
    wait_for("the link to come up", || {
        let links = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["link"]}"#,
        ]));
        links["snapshot"]["links"][0]["status"] == "up"
    });
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let target = format!("127.0.0.1:{port}");
    let mut fake = FakeVehicle::new(3, 0x00C0_FFEE);
    socket.send_to(&fake.heartbeat(), &target).unwrap();
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    // Like ArduCopter, it streams only what the daemon asks for.
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut buffer = [0u8; 2048];
    while let Ok((len, _)) = socket.recv_from(&mut buffer) {
        let _ = fake.answer(&buffer[..len]);
    }
    for frame in fake.telemetry(15.0) {
        socket.send_to(&frame, &target).unwrap();
    }
    let mut telemetry = Value::Null;
    wait_for("telemetry", || {
        telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": id }).to_string(),
        ]));
        telemetry["items"]
            .as_array()
            .is_some_and(|items| items.len() >= 10)
    });
    let fused = &telemetry["positions"][0];
    assert_eq!(fused["source"], "fused");
    assert_eq!(
        fused["altitudes"][1],
        json!({ "meters": 30.0, "frame": "above-home" })
    );
    let item = |key: &str| {
        telemetry["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(item("gps.fix")["text"], "3D");
    assert_eq!(item("throttle.output_pct")["value"], 42.0);
    // Keys say their units (S50, ADR-0006).
    for key in [
        "battery.voltage_v",
        "speed.ground_mps",
        "heading.true_deg",
        "attitude.roll_deg",
        "wind.speed_mps",
        "rangefinder.distance_m",
    ] {
        assert!(item(key)["value"].is_number(), "{key}");
    }
}

/// The MAVLink inspector: what a vehicle sent, decoded by MAVLink's own
/// field names, and what UXDuck sent it.
#[test]
fn mavlink_inspect_shows_a_udp_vehicles_messages_decoded() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]);
    wait_for("the link to come up", || {
        let links = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["link"]}"#,
        ]));
        links["snapshot"]["links"][0]["status"] == "up"
    });
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let target = format!("127.0.0.1:{port}");
    let mut fake = FakeVehicle::new(3, 0x00C0_FFEE);
    socket.send_to(&fake.heartbeat(), &target).unwrap();
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let mut inspected = Value::Null;
    wait_for("the heartbeat and what UXDuck sent", || {
        socket.send_to(&fake.heartbeat(), &target).unwrap();
        inspected = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "mavlink/inspect",
            "--json",
            &json!({ "vehicleId": id }).to_string(),
        ]));
        let messages = inspected["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        messages.iter().any(|m| m["name"] == "HEARTBEAT")
            && messages.iter().any(|m| m["direction"] == "sent")
    });
    assert_eq!(inspected["decoded"], true);
    let heartbeat = inspected["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["direction"] == "received" && m["name"] == "HEARTBEAT")
        .cloned()
        .expect("the vehicle's heartbeat");
    assert_eq!(heartbeat["systemId"], 3);
    assert_eq!(heartbeat["dialect"], "common");
    assert!(heartbeat["count"].as_u64().unwrap() >= 2, "{heartbeat}");
    assert!(heartbeat["fields"]["type"].is_number(), "{heartbeat}");
    assert!(heartbeat["fields"]["autopilot"].is_number(), "{heartbeat}");
    let sent = inspected["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["direction"] == "sent")
        .cloned()
        .unwrap();
    assert_eq!(sent["systemId"], uxduck_mavlink::GCS_SYSTEM_ID, "{sent}");
}

/// Stepping back in the inspector: a vehicle's heartbeats newest first from
/// its link's tlogs, this run's then an earlier run's, without another
/// vehicle's on the same link.
#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, told in order")]
fn mavlink_history_pages_a_vehicles_messages_from_its_tlogs() {
    use uxduck_mavlink::sim::{FakeVehicle, heartbeat_frame};

    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    let link =
        stdout_json(
            &daemon.cli(&[
                "rpc",
                "call",
                "link/add",
                "--json",
                &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } })
                    .to_string(),
            ]),
        );
    let link_id = link["id"].as_str().unwrap().to_owned();
    // An earlier run's tlog of the same link: three of its heartbeats and
    // one of another vehicle's.
    let mut earlier = Vec::new();
    for (n, system_id) in [(0u8, 3u8), (1, 4), (2, 3), (3, 3)] {
        let at_us = 1_000_000_000 + u64::from(n);
        uxduck_mavlink::tlog::record(&mut earlier, at_us, &heartbeat_frame(system_id, 1, n))
            .unwrap();
    }
    std::fs::create_dir_all(daemon.dir().join("tlogs")).unwrap();
    std::fs::write(
        daemon.dir().join(format!("tlogs/{link_id}-1000.tlog")),
        &earlier,
    )
    .unwrap();
    wait_for("the link to come up", || {
        let links = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["link"]}"#,
        ]));
        links["snapshot"]["links"][0]["status"] == "up"
    });
    let target = format!("127.0.0.1:{port}");
    let ours = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let theirs = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut fake = FakeVehicle::new(3, 0x00C0_FFEE);
    let mut other = FakeVehicle::new(4, 0x00BE_EF00);
    ours.send_to(&fake.heartbeat(), &target).unwrap();
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let id = vehicles(&daemon)
        .iter()
        .find(|v| v["systemId"] == 3)
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .unwrap();
    let history = |before: &Value, limit: u32| {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "mavlink/history",
            "--json",
            &json!({ "vehicleId": id, "direction": "received", "id": 0, "before": before, "limit": limit })
                .to_string(),
        ]))
    };
    let mut newest = Value::Null;
    wait_for("three of this run's heartbeats", || {
        ours.send_to(&fake.heartbeat(), &target).unwrap();
        theirs.send_to(&other.heartbeat(), &target).unwrap();
        newest = history(&Value::Null, 500);
        newest["messages"]
            .as_array()
            .is_some_and(|m| m.len() >= 3 + 3)
    });
    assert_eq!(newest["fromLog"], true, "{newest}");
    let all = newest["messages"].as_array().unwrap().clone();
    assert!(newest.get("older").is_none(), "{newest}");
    assert!(all.iter().all(|m| m["systemId"] == 3), "{newest}");
    assert!(
        all.iter().all(|m| m["fields"]["type"].is_number()),
        "{newest}"
    );
    let times: Vec<u64> = all.iter().map(|m| m["atUs"].as_u64().unwrap()).collect();
    assert!(
        times.windows(2).all(|w| w[0] >= w[1]),
        "newest first: {times:?}"
    );
    assert_eq!(
        times[times.len() - 3..],
        [1_000_000_003, 1_000_000_002, 1_000_000_000]
    );

    // Two at a time, the cursor carrying on into the earlier file.
    let mut paged = Vec::new();
    let mut before = Value::Null;
    loop {
        let page = history(&before, 2);
        let messages = page["messages"].as_array().unwrap();
        assert!(messages.len() <= 2, "{page}");
        paged.extend(messages.iter().map(|m| m["atUs"].as_u64().unwrap()));
        match page.get("older") {
            Some(older) => before = older.clone(),
            None => break,
        }
    }
    assert_eq!(paged[paged.len() - 3..], times[times.len() - 3..]);
    assert_eq!(paged[..times.len()], times[..], "{paged:?}");

    let unknown = daemon.cli(&[
        "rpc",
        "call",
        "mavlink/history",
        "--json",
        &json!({ "vehicleId": "nope", "direction": "received", "id": 0 }).to_string(),
    ]);
    assert!(!unknown.status.success());
    let error: Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(error["data"]["kind"], "not-found", "{error}");
    let bad = daemon.cli(&[
        "rpc",
        "call",
        "mavlink/history",
        "--json",
        &json!({ "vehicleId": id, "direction": "received", "id": 0, "before": "nonsense" })
            .to_string(),
    ]);
    let error: Value = serde_json::from_slice(&bad.stderr).unwrap();
    assert_eq!(error["data"]["kind"], "invalid-value", "{error}");
}

/// A local UDP socket standing in for another program, such as Mission Planner.
fn program() -> (std::net::UdpSocket, u16) {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

fn add_link(daemon: &Daemon, params: &Value) -> Value {
    let output = daemon.cli(&["rpc", "call", "link/add", "--json", &params.to_string()]);
    if output.status.success() {
        stdout_json(&output)
    } else {
        json!({ "error": serde_json::from_slice::<Value>(&output.stderr).unwrap_or_default() })
    }
}

fn link_status(daemon: &Daemon, id: &str) -> Value {
    let state = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["link"]}"#,
    ]));
    state["snapshot"]["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .cloned()
        .unwrap_or_default()
}

fn wait_up(daemon: &Daemon, id: &str) {
    wait_for("the link to come up", || {
        link_status(daemon, id)["status"] == "up"
    });
}

/// A few heartbeats from `fake` to the daemon's listener on `port`,
/// answering what the daemon asks (`AUTOPILOT_VERSION`).
fn heard_on(socket: &std::net::UdpSocket, fake: &mut uxduck_mavlink::sim::FakeVehicle, port: u16) {
    let target = format!("127.0.0.1:{port}");
    let mut buffer = [0u8; 2048];
    for _ in 0..3 {
        socket.send_to(&fake.heartbeat(), &target).unwrap();
        while let Ok((len, _)) = socket.recv_from(&mut buffer) {
            for reply in fake.answer(&buffer[..len]) {
                socket.send_to(&reply, &target).unwrap();
            }
        }
    }
}

#[test]
fn a_link_is_moved_and_turned_off_without_being_removed() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[
        "--presence-stale-ms",
        "600",
        "--presence-offline-ms",
        "1200",
    ]);
    let first = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": first } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    wait_up(&daemon, &id);
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut fake = FakeVehicle::new(4, 0xFEED);
    heard_on(&socket, &mut fake, first);
    let online = |daemon: &Daemon| {
        vehicles(daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    };
    wait_for("the vehicle to be online", || online(&daemon));
    let vehicle_id = vehicles(&daemon)[0]["id"].clone();

    // Moved to another port, the link keeps its id and its vehicle, which is
    // offline until heard there.
    let second = free_udp_port();
    let moved = rpc(
        &daemon,
        "link/update",
        &json!({ "id": id, "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": second } }),
    );
    assert_eq!(moved["name"], format!("UDP {second}"));
    assert_eq!(vehicles(&daemon)[0]["presence"], "offline");
    wait_up(&daemon, &id);
    heard_on(&socket, &mut fake, second);
    wait_for("the vehicle back on the new port", || online(&daemon));
    let heard = vehicles(&daemon);
    assert_eq!(heard.len(), 1, "the same vehicle, not a second: {heard:?}");
    assert_eq!(heard[0]["id"], vehicle_id);
    assert_eq!(heard[0]["linkId"], id);

    // Read only changes the link where it is: it never leaves "up".
    let read_only = rpc(
        &daemon,
        "link/update",
        &json!({ "id": id, "policy": "read" }),
    );
    assert_eq!(
        (read_only["policy"].as_str(), read_only["status"].as_str()),
        (Some("read"), Some("up"))
    );
    std::thread::sleep(Duration::from_millis(300));
    let settled = link_status(&daemon, &id);
    assert_eq!(settled["status"], "up");
    assert_eq!(settled["version"], read_only["version"], "not reopened");

    // Off: closed and kept; the vehicle is offline and stays so while
    // nothing listens.
    let off = rpc(&daemon, "link/disable", &json!({ "id": id }));
    assert_eq!(off["status"], "disabled");
    assert_eq!(vehicles(&daemon)[0]["presence"], "offline");
    heard_on(&socket, &mut fake, second);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(vehicles(&daemon)[0]["presence"], "offline");
    assert_eq!(link_status(&daemon, &id)["status"], "disabled");

    // On again: it listens where it was moved to.
    let on = rpc(&daemon, "link/enable", &json!({ "id": id }));
    assert_eq!(on["status"], "configured");
    assert_eq!(on["kind"]["port"], second);
    wait_up(&daemon, &id);
    heard_on(&socket, &mut fake, second);
    wait_for("the vehicle online once more", || online(&daemon));
    assert_eq!(vehicles(&daemon)[0]["id"], vehicle_id);
}

#[test]
fn a_forward_carries_its_vehicles_both_ways_and_refuses_our_own_listener() {
    use uxduck_mavlink::sim::{FakeVehicle, gcs_heartbeat_frame};

    let daemon = Daemon::start(&[]);
    let listen = free_udp_port();
    let radio = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": listen } }),
    );
    wait_up(&daemon, radio["id"].as_str().unwrap());

    // Forwarding into our own listener is refused.
    let own = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": listen } }),
    );
    assert!(
        own["error"]["message"]
            .as_str()
            .unwrap()
            .contains("own listeners"),
        "{own}"
    );

    // Two vehicles; the forward carries only the first.
    let (vehicle_a, _) = program();
    let (vehicle_b, _) = program();
    let target = format!("127.0.0.1:{listen}");
    let mut a = FakeVehicle::new(1, 0xA);
    let mut b = FakeVehicle::new(2, 0xB);
    vehicle_a.send_to(&a.heartbeat(), &target).unwrap();
    vehicle_b.send_to(&b.heartbeat(), &target).unwrap();
    wait_for("both vehicles", || vehicles(&daemon).len() == 2);
    let vid_a = vehicles(&daemon)
        .into_iter()
        .find(|v| v["systemId"] == 1)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (mission_planner, mp_port) = program();
    let forward = add_link(
        &daemon,
        &json!({
            "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": mp_port },
            "vehicleFilter": [vid_a]
        }),
    );
    wait_up(&daemon, forward["id"].as_str().unwrap());
    let mut buffer = [0u8; 2048];
    let heartbeat_a = a.heartbeat();
    vehicle_b.send_to(&b.heartbeat(), &target).unwrap();
    // A forward that has just come up can miss the first frame under load:
    // send until one arrives.
    let (len, from) = (0..20)
        .find_map(|_| {
            vehicle_a.send_to(&heartbeat_a, &target).unwrap();
            mission_planner.recv_from(&mut buffer).ok()
        })
        .expect("the heartbeat forwarded");
    assert_eq!(&buffer[..len], heartbeat_a.as_slice(), "byte for byte");
    // Anything more is a repeat of it: vehicle b is filtered out.
    while let Ok((n, _)) = mission_planner.recv_from(&mut buffer) {
        assert_eq!(
            &buffer[..n],
            heartbeat_a.as_slice(),
            "vehicle b is filtered out"
        );
    }

    // Mission Planner's heartbeat reaches vehicle a only, from our listener.
    let gcs = gcs_heartbeat_frame(255, 190, 7);
    mission_planner.send_to(&gcs, from).unwrap();
    let mut got = None;
    // Our stream-rate requests come first, one for each in the table.
    for _ in 0..10 + uxduck_mavlink::session::STREAM_RATES.len() * 2 {
        match vehicle_a.recv_from(&mut buffer) {
            Ok((len, _)) if buffer[..len] == gcs[..] => {
                got = Some(());
                break;
            }
            Ok(_) => {} // Our own requests to the vehicle.
            Err(_) => break,
        }
    }
    assert!(got.is_some(), "the forward's reply reached vehicle a");
    while let Ok((len, _)) = vehicle_b.recv_from(&mut buffer) {
        assert_ne!(&buffer[..len], gcs.as_slice(), "never to b");
    }
}

#[test]
fn a_loop_through_another_program_disables_the_forward() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let listen = free_udp_port();
    let radio = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": listen } }),
    );
    wait_up(&daemon, radio["id"].as_str().unwrap());
    let (relay, relay_port) = program();
    let forward = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": relay_port } }),
    );
    let forward_id = forward["id"].as_str().unwrap().to_owned();
    wait_up(&daemon, &forward_id);

    // A tool that relays everything it gets back into our listener.
    let target = format!("127.0.0.1:{listen}");
    let (vehicle, _) = program();
    let mut fake = FakeVehicle::new(1, 0xC);
    let mut buffer = [0u8; 2048];
    for _ in 0..8 {
        vehicle.send_to(&fake.heartbeat(), &target).unwrap();
        if let Ok((len, _)) = relay.recv_from(&mut buffer) {
            relay.send_to(&buffer[..len], &target).unwrap();
        }
    }
    wait_for("loop detection", || {
        link_status(&daemon, &forward_id)["status"] == "disabled-by-loop-detection"
    });
    let link = link_status(&daemon, &forward_id);
    assert!(
        link["statusDetail"].as_str().unwrap().contains("loop"),
        "{link}"
    );
    assert_eq!(
        vehicles(&daemon).len(),
        1,
        "looped copies are not a second vehicle"
    );

    let enabled = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "link/enable",
        "--json",
        &json!({ "id": forward_id }).to_string(),
    ]));
    assert_eq!(enabled["status"], "configured");
    wait_up(&daemon, &forward_id);
}

#[test]
fn a_tcp_link_reassembles_the_stream_and_reconnects() {
    use uxduck_mavlink::sim::FakeVehicle;

    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let daemon = Daemon::start(&[]);
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "tcp-client", "host": "127.0.0.1", "port": port } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    let (mut stream, _) = server.accept().unwrap();
    wait_up(&daemon, &id);
    // A heartbeat in three pieces, as a stream may deliver it.
    let mut fake = FakeVehicle::new(9, 0xD);
    let heartbeat = fake.heartbeat();
    for piece in heartbeat.chunks(5) {
        stream.write_all(piece).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for("the TCP vehicle", || {
        vehicles(&daemon).iter().any(|v| v["systemId"] == 9)
    });

    drop(stream);
    wait_for("the link to go down", || {
        link_status(&daemon, &id)["status"] == "down"
    });
    let (_again, _) = server.accept().unwrap();
    wait_up(&daemon, &id);
}

#[test]
fn a_udp_client_says_hello_and_hears_the_vehicle_behind_its_server() {
    use uxduck_mavlink::sim::FakeVehicle;

    // A server that sends only to whoever has spoken to it, as a telemetry
    // bridge does.
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let daemon = Daemon::start(&[]);
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-client", "host": "127.0.0.1", "port": port } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    assert_eq!(link["name"], format!("UDP 127.0.0.1:{port}"));
    let mut buffer = [0u8; 512];
    let (_, client) = server.recv_from(&mut buffer).expect("a hello");
    wait_up(&daemon, &id);

    let mut fake = FakeVehicle::new(7, 0x7);
    wait_for("the vehicle behind the server", || {
        server.send_to(&fake.heartbeat(), client).unwrap();
        vehicles(&daemon).iter().any(|v| v["systemId"] == 7)
    });
}

/// The ground station system id is a setting (ADR-0004): set to 42,
/// everything UXDuck sends goes from 42, a UDP client's hello included, and
/// a vehicle's answers to 42 are taken, so it is identified.
#[test]
fn a_ground_station_id_set_to_42_is_sent_as_and_answered_to() {
    use uxduck_mavlink::sim::FakeVehicle;

    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let daemon = Daemon::start(&[]);
    let settings = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    assert_eq!(settings["snapshot"]["settings"]["gcsSystemId"], 255);
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    let saved = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        &json!({ "expectedVersion": version, "gcsSystemId": 42 }).to_string(),
    ]));
    assert_eq!(saved["gcsSystemId"], 42, "{saved}");

    add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-client", "host": "127.0.0.1", "port": port } }),
    );
    let mut buffer = [0u8; 2048];
    let (len, client) = loop {
        if let Ok(got) = server.recv_from(&mut buffer) {
            break got;
        }
    };
    let hello = uxduck_mavlink::framing::header(&buffer[..len]).expect("a MAVLink hello");
    assert_eq!(hello.system_id, 42, "the hello comes from 42");

    // A vehicle behind the server, answering whatever UXDuck asks.
    let mut fake = FakeVehicle::new(7, 0x0042_0042);
    let mut senders = std::collections::BTreeSet::new();
    wait_for("the vehicle identified by its answers to 42", || {
        server.send_to(&fake.heartbeat(), client).unwrap();
        while let Ok((len, _)) = server.recv_from(&mut buffer) {
            for frame in uxduck_mavlink::framing::split(&buffer[..len]).frames {
                if let Some(header) = uxduck_mavlink::framing::header(frame) {
                    senders.insert(header.system_id);
                }
            }
            for reply in fake.answer(&buffer[..len]) {
                server.send_to(&reply, client).unwrap();
            }
        }
        vehicles(&daemon)
            .iter()
            .any(|v| v["systemId"] == 7 && v["uid"].is_string())
    });
    assert_eq!(senders, [42].into(), "nothing goes from 255");
}

#[test]
fn a_tcp_listener_keeps_each_connection_as_its_own_vehicle() {
    use uxduck_mavlink::sim::FakeVehicle;

    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let daemon = Daemon::start(&[]);
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "tcp-server", "host": "127.0.0.1", "port": port } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    assert_eq!(link["name"], format!("TCP {port}"));
    wait_up(&daemon, &id);

    // Two vehicles with the same system id, their heartbeats cut into
    // pieces and interleaved: each connection is its own stream.
    let mut first = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut second = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut one = FakeVehicle::new(1, 0xA1);
    let mut two = FakeVehicle::new(1, 0xB2);
    wait_for("two vehicles on one listener", || {
        let (a, b) = (one.heartbeat(), two.heartbeat());
        for (x, y) in a.chunks(5).zip(b.chunks(5)) {
            first.write_all(x).unwrap();
            second.write_all(y).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        vehicles(&daemon)
            .iter()
            .filter(|v| v["systemId"] == 1)
            .count()
            == 2
    });

    // A second listener on the same port is refused.
    let refused = daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "tcp-server", "host": "0.0.0.0", "port": port } }).to_string(),
    ]);
    assert!(!refused.status.success());
}

#[cfg(unix)]
#[test]
fn a_serial_link_over_a_pseudo_terminal_hears_a_vehicle_and_notices_it_go() {
    use serialport::{SerialPort, TTYPort};
    use uxduck_mavlink::sim::FakeVehicle;

    // The pair is made after the daemon starts, so the daemon does not
    // inherit the vehicle's end and the pair can hang up.
    let daemon = Daemon::start(&[]);
    let (mut vehicle_end, daemon_end) = TTYPort::pair().expect("a pseudo-terminal pair");
    let device = daemon_end.name().expect("a device path");
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "serial", "device": device, "baud": 115_200 } }),
    );
    let id = link["id"].as_str().unwrap().to_owned();
    wait_up(&daemon, &id);
    let mut fake = FakeVehicle::new(4, 0xE);
    wait_for("the serial vehicle", || {
        vehicle_end.write_all(&fake.heartbeat()).unwrap();
        vehicle_end.flush().unwrap();
        vehicles(&daemon).iter().any(|v| v["systemId"] == 4)
    });
    // The other end goes away, as an unplugged cable does.
    drop(vehicle_end);
    drop(daemon_end);
    wait_for("the link to go down", || {
        link_status(&daemon, &id)["status"] == "down"
    });
}

#[test]
fn uxduck_forward_runs_headless_from_its_command_line() {
    use uxduck_mavlink::sim::FakeVehicle;

    let listen = free_udp_port();
    let (ground, ground_port) = program();
    let forward = Daemon::run_in(
        tempfile::tempdir().unwrap(),
        &[
            "forward",
            "--in",
            &format!("udp:127.0.0.1:{listen}"),
            "--out",
            &format!("udp:127.0.0.1:{ground_port}"),
        ],
        &[],
    );
    let (vehicle, _) = program();
    let mut fake = FakeVehicle::new(1, 0xF);
    let mut buffer = [0u8; 2048];
    let mut forwarded = false;
    for _ in 0..30 {
        let heartbeat = fake.heartbeat();
        vehicle
            .send_to(&heartbeat, format!("127.0.0.1:{listen}"))
            .unwrap();
        if let Ok((len, _)) = ground.recv_from(&mut buffer)
            && buffer[..len] == heartbeat[..]
        {
            forwarded = true;
            break;
        }
    }
    assert!(
        forwarded,
        "the vehicle's heartbeat reached the ground station"
    );
    let links = stdout_json(&forward.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["link"]}"#,
    ]));
    assert_eq!(links["snapshot"]["links"].as_array().unwrap().len(), 2);
    let ports = forward.cli(&["ports"]);
    let text = String::from_utf8_lossy(&ports.stdout);
    assert!(text.contains(&listen.to_string()), "{text}");
}

/// Serves files from a map over HTTP until the test ends.
fn file_server(
    files: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 2048];
            let len = stream.read(&mut request).unwrap_or(0);
            let text = String::from_utf8_lossy(&request[..len]);
            let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = files.lock().unwrap().get(&path).cloned();
            let response = match body {
                Some(body) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    port
}

fn signed_manifest(
    key: &minisign::KeyPair,
    version: &str,
    published_at: &str,
    rollback: bool,
) -> (Vec<u8>, Vec<u8>) {
    let manifest = json!({
        "schema": 1,
        "channel": "stable",
        "version": version,
        "publishedAt": published_at,
        "rollback": rollback,
        "artifacts": [{
            "target": "x86_64-unknown-linux-gnu",
            "kind": "deb",
            "file": format!("uxduck-{version}-x86_64-unknown-linux-gnu.deb"),
            "url": format!("https://example.test/uxduck-{version}-x86_64-unknown-linux-gnu.deb"),
            "sha256": "ab",
            "size": 1
        }]
    });
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let signature = minisign::sign(
        None,
        &key.sk,
        std::io::Cursor::new(&bytes),
        Some("test"),
        None,
    )
    .unwrap()
    .into_string()
    .into_bytes();
    (bytes, signature)
}

#[test]
fn update_check_verifies_the_manifest_and_sees_a_rollback() {
    let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let public = key.pk.to_box().unwrap().into_string();
    let public_b64 = public.lines().nth(1).unwrap().to_owned();
    let files = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let port = file_server(files.clone());
    let url = format!("http://127.0.0.1:{port}/releases/stable.json");
    let publish = |manifest: (Vec<u8>, Vec<u8>)| {
        let mut files = files.lock().unwrap();
        files.insert("/releases/stable.json".into(), manifest.0);
        files.insert("/releases/stable.json.minisig".into(), manifest.1);
    };
    let dir = tempfile::tempdir().unwrap();
    let check = |key: &str| {
        Command::new(BIN)
            .args([
                "update",
                "check",
                "--url",
                &url,
                "--public-key",
                key,
                "--data-dir",
            ])
            .arg(dir.path())
            .output()
            .unwrap()
    };
    let current = env!("CARGO_PKG_VERSION");
    let next = {
        let mut v = semver::Version::parse(current).unwrap();
        v.minor += 1;
        v.to_string()
    };

    // A newer release is reported.
    publish(signed_manifest(&key, &next, "2026-10-01T00:00:00Z", false));
    let out = check(&public_b64);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!("UXDuck {next} is available")),
        "{text}"
    );

    // The channel is re-pointed at this version as a rollback: seen.
    publish(signed_manifest(&key, current, "2026-10-02T00:00:00Z", true));
    let text = String::from_utf8_lossy(&check(&public_b64).stdout).into_owned();
    assert!(text.contains("is up to date"), "{text}");

    // An older manifest replayed after that is refused.
    publish(signed_manifest(&key, &next, "2026-10-01T00:00:00Z", false));
    let out = check(&public_b64);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("replayed"));

    // A tampered manifest, or another key, is refused.
    let (mut bytes, signature) = signed_manifest(&key, &next, "2026-10-03T00:00:00Z", false);
    bytes.extend_from_slice(b" ");
    publish((bytes, signature));
    let out = check(&public_b64);
    assert!(String::from_utf8_lossy(&out.stderr).contains("signature does not verify"));
    let other = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    publish(signed_manifest(
        &other,
        &next,
        "2026-10-04T00:00:00Z",
        false,
    ));
    let out = check(&public_b64);
    assert!(String::from_utf8_lossy(&out.stderr).contains("signature does not verify"));

    // A development build has no key and says so.
    let out = check("");
    assert!(!out.status.success());
}

#[test]
fn a_rollback_to_an_older_version_is_reported_to_newer_clients() {
    // The version on the channel is older than this build and marked as a
    // rollback, as the rollback workflow publishes it.
    let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let public_b64 = key
        .pk
        .to_box()
        .unwrap()
        .into_string()
        .lines()
        .nth(1)
        .unwrap()
        .to_owned();
    let files = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let port = file_server(files.clone());
    let (bytes, signature) = signed_manifest(&key, "0.0.1", "2026-10-05T00:00:00Z", true);
    files.lock().unwrap().insert("/stable.json".into(), bytes);
    files
        .lock()
        .unwrap()
        .insert("/stable.json.minisig".into(), signature);
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["update", "check", "--public-key", &public_b64, "--data-dir"])
        .arg(dir.path())
        .env(
            "UXDUCK_UPDATE_URL",
            format!("http://127.0.0.1:{port}/stable.json"),
        )
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("was rolled back to 0.0.1"),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A fake vehicle on its own thread: heartbeats and telemetry at 5 Hz, and
/// answers what it is sent, until the flag is lowered.
fn fly(
    port: u16,
    mut fake: uxduck_mavlink::sim::FakeVehicle,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering;
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = running.clone();
    let thread = std::thread::spawn(move || {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let target = format!("127.0.0.1:{port}");
        let mut buffer = [0u8; 2048];
        let started = Instant::now();
        while flag.load(Ordering::Relaxed) {
            socket.send_to(&fake.heartbeat(), &target).unwrap();
            for frame in fake.telemetry(started.elapsed().as_secs_f64()) {
                socket.send_to(&frame, &target).unwrap();
            }
            if let Ok((len, from)) = socket.recv_from(&mut buffer) {
                for reply in fake.answer(&buffer[..len]) {
                    socket.send_to(&reply, from).unwrap();
                }
            }
        }
    });
    (running, thread)
}

#[test]
fn a_command_goes_to_the_vehicle_and_its_receipt_comes_back() {
    let daemon = Daemon::start(&[
        "--presence-stale-ms",
        "600",
        "--presence-offline-ms",
        "1200",
    ]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    });
    let id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let send = |command: Value| {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "command/send",
            "--json",
            &json!({ "vehicleId": id, "command": command }).to_string(),
        ]))
    };

    // The CLI call returns once the vehicle has acknowledged.
    let armed = daemon.cli(&["command", "arm"]);
    assert!(
        armed.status.success(),
        "{}{}",
        String::from_utf8_lossy(&armed.stdout),
        String::from_utf8_lossy(&armed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&armed.stdout).trim(), "accepted");
    wait_for("armed in the heartbeat", || {
        vehicles(&daemon)[0]["armed"] == true
    });
    let receipt = send(json!({ "type": "set-mode", "mode": "LOITER" }));
    assert_eq!(receipt["status"], "accepted");
    wait_for("the mode", || vehicles(&daemon)[0]["mode"] == "LOITER");

    // Silence: arm is blocked, RTL is sent without a recent heartbeat.
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
    wait_for("offline", || vehicles(&daemon)[0]["presence"] == "offline");
    let blocked = send(json!({ "type": "arm" }));
    assert_eq!(blocked["status"], "not-sent");
    let rtl = send(json!({ "type": "rtl" }));
    assert_eq!(
        (
            rtl["status"].as_str(),
            rtl["sentWithoutRecentHeartbeat"].as_bool(),
            rtl["attempts"].as_u64()
        ),
        (Some("timeout"), Some(true), Some(3))
    );
    let state = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["notification"]}"#,
    ]));
    let titles: Vec<String> = state["snapshot"]["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        titles.iter().any(|t| t.ends_with("is offline")),
        "{titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|t| t.ends_with("didn't answer Return to launch")),
        "{titles:?}"
    );
}

/// `uxduck mcp` on pipes: one JSON-RPC line out, one line back.
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn start(daemon: &Daemon) -> Self {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .arg("--data-dir")
            .arg(daemon.dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn uxduck mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, method: &str, params: &Value) {
        self.next_id += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.stdin, "{line}").expect("write");
    }

    fn answer(&mut self) -> Value {
        use std::io::BufRead;
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.send(method, params);
        self.answer()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pending_approvals(daemon: &Daemon) -> Vec<Value> {
    stdout_json(&daemon.cli(&["rpc", "call", "approval/list"]))["approvals"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_coding_agent_over_mcp_reads_freely_and_waits_for_an_operator_to_command() {
    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    let mut mcp = Mcp::start(&daemon);
    let hello = mcp.call(
        "initialize",
        &json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": { "name": "Claude Code", "version": "2.0.0" } }),
    );
    assert_eq!(hello["result"]["serverInfo"]["name"], "uxduck", "{hello}");
    let tools = mcp.call("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"vehicle_list") && names.contains(&"command_send"),
        "{names:?}"
    );

    // Reading needs no one.
    let list = mcp.call(
        "tools/call",
        &json!({ "name": "vehicle_list", "arguments": {} }),
    );
    assert_eq!(list["result"]["isError"], false, "{list}");
    assert_eq!(
        list["result"]["structuredContent"]["vehicles"][0]["id"],
        vehicle.as_str()
    );

    // Arming waits for an operator; the CLI shows and answers it.
    let arm = json!({ "name": "command_send",
                      "arguments": { "vehicleId": vehicle, "command": { "type": "arm" } } });
    mcp.send("tools/call", &arm);
    wait_for("the approval", || pending_approvals(&daemon).len() == 1);
    let approval = pending_approvals(&daemon)[0].clone();
    assert_eq!(approval["clientName"], "Claude Code");
    let listed = String::from_utf8_lossy(&daemon.cli(&["approvals"]).stdout).into_owned();
    assert!(listed.contains("Claude Code wants to arm"), "{listed}");
    let allow = daemon.cli(&[
        "approvals",
        "allow",
        approval["id"].as_str().unwrap(),
        "--remember",
        "always",
    ]);
    assert!(
        allow.status.success(),
        "{}",
        String::from_utf8_lossy(&allow.stderr)
    );
    let armed = mcp.answer();
    assert_eq!(armed["result"]["isError"], false, "{armed}");
    assert_eq!(armed["result"]["structuredContent"]["status"], "accepted");

    // Always: the next command goes straight through.
    let again = mcp.call("tools/call", &arm);
    assert_eq!(again["result"]["isError"], false, "{again}");
    assert!(pending_approvals(&daemon).is_empty());

    // Back to read-only: held again, and a denial is a tool error.
    let clients = stdout_json(&daemon.cli(&["rpc", "call", "client/list"]));
    let record = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["client"]["name"] == "Claude Code")
        .unwrap()["client"]
        .clone();
    assert_eq!(record["capabilities"], json!(["read", "control"]));
    let set = daemon.cli(&[
        "rpc",
        "call",
        "client/setCapabilities",
        "--json",
        &json!({ "id": record["id"], "expectedVersion": record["version"], "capabilities": ["read"] })
            .to_string(),
    ]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    mcp.send("tools/call", &arm);
    wait_for("the second approval", || {
        pending_approvals(&daemon).len() == 1
    });
    let id = pending_approvals(&daemon)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(daemon.cli(&["approvals", "deny", &id]).status.success());
    let denied = mcp.answer();
    assert_eq!(denied["result"]["isError"], true, "{denied}");

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

fn transfers_state(daemon: &Daemon) -> Value {
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["parameter-set","parameter","mission","notification"]}"#,
    ]))["snapshot"]
        .clone()
}

fn rpc(daemon: &Daemon, method: &str, params: &Value) -> Value {
    let out = daemon.cli(&["rpc", "call", method, "--json", &params.to_string()]);
    assert!(
        out.status.success(),
        "{method}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout_json(&out)
}

#[test]
fn parameters_and_the_mission_download_are_written_and_refresh_after_a_reboot() {
    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    fake.drops_every = Some(7);
    // Streaming on through the restart, as a vehicle with its own `SRn_`
    // rates does: a copter, quiet until asked again, restarted this soon
    // after it was first heard looks paused.
    fake.streams_on_request = false;
    let (running, thread) = fly(port, fake);

    // First sight: both download on their own, gaps and all.
    wait_for("the parameters", || {
        transfers_state(&daemon)["parameterSets"][0]["status"] == "synced"
    });
    let state = transfers_state(&daemon);
    let vehicle = state["parameterSets"][0]["vehicleId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        state["parameterSets"][0]["count"],
        uxduck_mavlink::sim::FAKE_PARAMETERS.len()
    );
    wait_for("the mission", || {
        transfers_state(&daemon)["missions"][0]["status"] == "synced"
    });
    let first_boot = state["parameterSets"][0]["bootInstantUs"].as_u64().unwrap();

    // A write, confirmed; a read-only one, refused by the vehicle.
    let set = rpc(
        &daemon,
        "param/set",
        &json!({ "vehicleId": vehicle, "name": "RTL_ALT", "value": 3000, "expectedValue": 1500 }),
    );
    assert_eq!(set["status"], "accepted", "{set}");
    let kept = rpc(
        &daemon,
        "param/set",
        &json!({ "vehicleId": vehicle, "name": "FORMAT_VERSION", "value": 1 }),
    );
    assert_eq!(kept["status"], "rejected");

    // A mission up, then down again.
    let item = |command: u16, altitude: f64| {
        json!({ "command": command, "frame": 3, "param1": 0, "param2": 0, "param3": 0,
                "param4": 0, "latitudeDeg": -35.363_261_1, "longitudeDeg": 149.165_230_2,
                "altitudeM": altitude, "autocontinue": true })
    };
    let version = transfers_state(&daemon)["missions"][0]["version"]
        .as_u64()
        .unwrap();
    let uploaded = rpc(
        &daemon,
        "mission/upload",
        &json!({ "vehicleId": vehicle, "expectedVersion": version,
                 "items": [item(16, 0.0), item(22, 20.0), item(16, 30.0)] }),
    );
    assert_eq!(uploaded["status"], "accepted", "{uploaded}");
    rpc(&daemon, "mission/refresh", &json!({ "id": vehicle }));
    wait_for("the mission again", || {
        let mission = &transfers_state(&daemon)["missions"][0];
        mission["status"] == "synced" && mission["items"].as_array().is_some_and(|i| i.len() == 3)
    });
    assert_eq!(
        transfers_state(&daemon)["missions"][0]["version"],
        version + 1,
        "the same items keep the upload's version"
    );

    // A reboot: a new boot, both downloaded again, the new value kept. Up
    // for two seconds first, as an autopilot is: a restart sooner than a
    // second in cannot be told from reordering, and FTP is that quick (S74).
    std::thread::sleep(Duration::from_secs(2));
    let reboot = rpc(
        &daemon,
        "command/send",
        &json!({ "vehicleId": vehicle, "command": { "type": "reboot" } }),
    );
    assert_eq!(reboot["status"], "accepted", "{reboot}");
    wait_for("the new boot", || {
        let set = &transfers_state(&daemon)["parameterSets"][0];
        set["status"] == "synced"
            && set["bootInstantUs"]
                .as_u64()
                .is_some_and(|b| b > first_boot)
    });
    let rtl = transfers_state(&daemon)["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "RTL_ALT")
        .unwrap()["value"]
        .clone();
    assert_eq!(rtl, 3000.0);
    let journal = std::fs::read_to_string(daemon.dir().join("events.ndjson")).unwrap();
    assert!(journal.contains("Alpha restarted"));

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
fn a_gamepads_raw_input_is_read_back_for_testing() {
    let pad_port = free_udp_port();
    let daemon = Daemon::start(&["--no-gamepads", "--fake-controller", &pad_port.to_string()]);
    let pad = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut inputs = Value::Null;
    // Datagrams can be missed while the daemon is busy starting: say it again.
    wait_for("the pad's input", || {
        let message =
            json!({ "id": "fake", "axes": { "left-z": -1.0 }, "buttons": { "mode": true } });
        pad.send_to(message.to_string().as_bytes(), ("127.0.0.1", pad_port))
            .unwrap();
        inputs = stdout_json(&daemon.cli(&["rpc", "call", "controller/inputs", "--json", "{}"]));
        inputs["controllers"][0]["buttons"]
            == json!([{ "name": "mode", "button": "mode", "pressed": true, "seen": true }])
    });
    assert_eq!(
        inputs["controllers"][0]["axes"],
        json!([{ "name": "left-z", "axis": "left-z", "value": -1.0 }])
    );
    assert_eq!(inputs["controllers"][0]["layoutKnown"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claimed_controller_flies_the_vehicle_and_unplugging_it_leaves_the_failsafe() {
    let pad_port = free_udp_port();
    let daemon = Daemon::start(&["--no-gamepads", "--fake-controller", &pad_port.to_string()]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    });
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    let pad = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let move_pad = |message: Value| {
        pad.send_to(message.to_string().as_bytes(), ("127.0.0.1", pad_port))
            .unwrap();
    };
    move_pad(json!({ "id": "fake", "name": "Test pad", "axes": { "right-stick-y": 0.8 } }));
    let controllers = || {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["controller"]}"#,
        ]))["snapshot"]["controllers"]
            .clone()
    };
    // Datagrams can be missed while the daemon is busy starting: say it again.
    wait_for("the controller", || {
        move_pad(json!({ "id": "fake", "name": "Test pad", "axes": { "right-stick-y": 0.8 } }));
        controllers()[0]["connected"] == true
    });

    let cookie = paired_cookie(&daemon);
    let mut ui = daemon.websocket(Some(&cookie)).await.expect("websocket");
    assert!(
        ws_call(&mut ui, 1, "initialize", operator_ui())
            .await
            .get("result")
            .is_some()
    );
    let claim = json!({ "id": "fake", "mode": "pinned", "vehicleId": vehicle });
    let off_centre = ws_call(&mut ui, 2, "controller/claim", claim.clone()).await;
    assert_eq!(
        off_centre["error"]["data"]["kind"], "invalid-value",
        "{off_centre}"
    );
    move_pad(json!({ "id": "fake", "axes": { "right-stick-y": 0.0 } }));
    let mut claimed = Value::Null;
    for attempt in 3..50 {
        claimed = ws_call(&mut ui, attempt, "controller/claim", claim.clone()).await;
        if claimed.get("result").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(claimed["result"]["target"], vehicle.as_str(), "{claimed}");

    // Sticks flow: the vehicle's failsafe does not trip.
    move_pad(json!({ "id": "fake", "axes": { "right-stick-y": 0.3 } }));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(vehicles(&daemon)[0]["mode"], "STABILIZE");

    // Unplugged: released, nothing more is sent, and the vehicle returns home.
    move_pad(json!({ "id": "fake", "connected": false }));
    wait_for("the release", || controllers()[0]["claim"].is_null());
    wait_for("the failsafe", || vehicles(&daemon)[0]["mode"] == "RTL");

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
fn a_restart_keeps_the_world_and_asks_first_while_armed() {
    let daemon = Daemon::start(&["--no-gamepads"]);
    create_operator(&daemon, "Ana");
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online" && v["uid"].is_string())
    });
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let pid = daemon.child.id();

    // Armed: the restart asks first.
    let armed = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "command/send",
        "--json",
        &json!({ "vehicleId": vehicle, "command": { "type": "arm" } }).to_string(),
    ]));
    assert_eq!(armed["status"], "accepted", "{armed}");
    wait_for("armed", || vehicles(&daemon)[0]["armed"] == true);
    let refused = daemon.cli(&["daemon", "restart"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("Alpha is armed"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let restarted = daemon.cli(&["daemon", "restart", "--yes"]);
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert!(String::from_utf8_lossy(&restarted.stdout).starts_with("Restarted"));
    #[cfg(unix)]
    assert_eq!(daemon.child.id(), pid, "the same process, a new program");
    let _ = pid;

    // The world is as it was: the operator, the vehicle under its id, back online.
    assert_eq!(names(&daemon), ["Ana"]);
    wait_for("the vehicle again", || {
        vehicles(&daemon)
            .iter()
            .any(|v| v["id"] == vehicle.as_str() && v["presence"] == "online")
    });
    // Heard again before its UID, it has a provisional record for a moment.
    wait_for("one vehicle", || vehicles(&daemon).len() == 1);

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

/// Builds the first-party plugins for WebAssembly (in `plugins/`, its own
/// workspace and target) and lays one out in a directory to install from.
fn built_plugin(into: &Path, id: &str, module: &str) -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let built = Command::new(cargo)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(&root)
        .status()
        .expect("cargo");
    assert!(built.success(), "the plugins build");
    let dir = into.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(root.join(id).join("plugin.json"), dir.join("plugin.json")).unwrap();
    std::fs::copy(
        root.join("target/wasm32-unknown-unknown/release")
            .join(module),
        dir.join(module),
    )
    .unwrap();
    dir
}

/// Whether `plugin list` shows this plugin running: the Clock, which comes
/// with UXDuck, runs beside it.
fn plugin_running(daemon: &Daemon, id: &str) -> bool {
    String::from_utf8_lossy(&daemon.cli(&["plugin", "list"]).stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some(id) && line.ends_with("running"))
}

/// A TypeScript plugin from source, in a folder to install from.
fn script_plugin(into: &Path, id: &str, script: &str) -> std::path::PathBuf {
    let dir = into.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        json!({
            "abi": 2, "id": id, "name": id, "version": "0.1.0", "script": "index.ts",
            "capabilities": ["read"],
            "subscribe": { "items": { "keys": ["battery.*"], "cadenceMs": 200 } }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("index.ts"), script).unwrap();
    dir
}

fn camera_plugin(into: &Path) -> std::path::PathBuf {
    built_plugin(into, "camera-manager", "camera_manager.wasm")
}

/// Runs `uxduck plugin test`, one JSON value per line.
fn harness(plugin: &Path, tlog: &Path, extra: &[&str]) -> Vec<Value> {
    let out = Command::new(BIN)
        .args(["plugin", "test"])
        .arg(plugin)
        .arg("--tlog")
        .arg(tlog)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every effect of one kind from every call to `function`.
fn effects_of(lines: &[Value], function: &str, kind: &str) -> Vec<Value> {
    lines
        .iter()
        .filter(|l| l["call"] == function)
        .flat_map(|l| l["effects"][kind].as_array().cloned().unwrap_or_default())
        .collect()
}

#[test]
fn the_camera_plugin_passes_its_harness_run_against_a_recorded_tlog() {
    use uxduck_mavlink::sim::{command_ack_frame, heartbeat_frame};
    let work = tempfile::tempdir().unwrap();
    let plugin = camera_plugin(work.path());
    let tlog = work.path().join("flight.tlog");
    let mut log = Vec::new();
    for (at, frame) in [
        heartbeat_frame(1, 1, 0),
        command_ack_frame(1, 1, (255, 190), 1, 2000, 0),
        command_ack_frame(1, 1, (255, 190), 2, 2000, 0),
        command_ack_frame(1, 1, (255, 190), 3, 2000, 4),
        command_ack_frame(1, 1, (255, 190), 4, 2500, 0),
    ]
    .iter()
    .enumerate()
    {
        uxduck_mavlink::tlog::record(&mut log, at as u64 * 1000, frame).unwrap();
    }
    std::fs::write(&tlog, log).unwrap();
    let lines = harness(&plugin, &tlog, &["--command", "photo"]);
    assert_eq!(lines[0]["describe"]["panels"][0]["title"], "Camera");
    // It reads decoded acknowledgements, and the harness prints them.
    let first = lines.iter().find(|l| l["call"] == "on_messages").unwrap();
    assert_eq!(first["input"]["messages"][0]["name"], "COMMAND_ACK");
    assert_eq!(first["input"]["messages"][0]["fields"]["command"], 2000);
    let items = effects_of(&lines, "on_messages", "items");
    let last = |key: &str| {
        items
            .iter()
            .rev()
            .find(|i| i["key"] == key)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        last("camera.photos")["value"],
        2.0,
        "a failed capture does not count"
    );
    assert_eq!(last("camera.video")["text"], "Recording");
    assert_eq!(
        effects_of(&lines, "on_command", "commands")[0]["command"],
        json!({ "type": "long", "command": 2000, "params": [0.0, 0.0, 1.0, 0.0] })
    );
}

#[test]
fn the_example_plugin_reads_items_and_messages_and_takes_a_command_with_parameters() {
    let work = tempfile::tempdir().unwrap();
    let plugin = built_plugin(work.path(), "home-distance", "home_distance.wasm");
    // Sixteen and a half minutes of the fake vehicle's circle, every 5 s:
    // its battery drains below 20 %. Nobody asks for its streams: it sends
    // them all.
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    fake.streams_on_request = false;
    let mut log = Vec::new();
    for step in 0..200u32 {
        let t_s = f64::from(step) * 5.0;
        let at_us = u64::from(step) * 5_000_000;
        uxduck_mavlink::tlog::record(&mut log, at_us, &fake.heartbeat()).unwrap();
        for frame in fake.telemetry(t_s) {
            uxduck_mavlink::tlog::record(&mut log, at_us, &frame).unwrap();
        }
    }
    let tlog = work.path().join("flight.tlog");
    std::fs::write(&tlog, log).unwrap();
    let lines = harness(
        &plugin,
        &tlog,
        &["--command", "mark", "--params", r#"{"label":"Waypoint A"}"#],
    );
    // Decoded positions in, the distance from home out, in metres.
    let positions = lines.iter().find(|l| l["call"] == "on_messages").unwrap();
    assert_eq!(
        positions["input"]["messages"][0]["name"],
        "GLOBAL_POSITION_INT"
    );
    let distances = effects_of(&lines, "on_messages", "items");
    assert!(
        distances
            .iter()
            .all(|i| i["key"] == "home.distance_m" && i["unit"] == "meter")
    );
    let farthest = distances
        .iter()
        .filter_map(|i| i["value"].as_f64())
        .fold(0.0_f64, f64::max);
    assert!(
        (100.0..=170.0).contains(&farthest),
        "{farthest} m across an 80 m circle"
    );
    // Home's 100 m circle and its centre, drawn once when home is found.
    let features = effects_of(&lines, "on_messages", "features");
    let ids: Vec<&str> = features.iter().map(|f| f["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["home-circle", "home"]);
    assert_eq!(features[0]["geometry"]["type"], "Polygon");
    // Battery items in, one notice below 20 %.
    let items = lines.iter().find(|l| l["call"] == "on_items").unwrap();
    assert!(
        items["input"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["key"].as_str().unwrap().starts_with("battery."))
    );
    let notices = effects_of(&lines, "on_items", "notifications");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["title"], "Battery at 19 %");
    // The declared command, with its parameter.
    assert_eq!(
        effects_of(&lines, "on_command", "notifications")[0]["title"],
        "Marked: Waypoint A"
    );
}

#[test]
fn the_typescript_example_runs_in_the_harness_as_the_rust_one_does() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/home-distance-ts");
    let work = tempfile::tempdir().unwrap();
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    fake.streams_on_request = false;
    let mut log = Vec::new();
    for step in 0..200u32 {
        let at_us = u64::from(step) * 5_000_000;
        uxduck_mavlink::tlog::record(&mut log, at_us, &fake.heartbeat()).unwrap();
        for frame in fake.telemetry(f64::from(step) * 5.0) {
            uxduck_mavlink::tlog::record(&mut log, at_us, &frame).unwrap();
        }
    }
    let tlog = work.path().join("flight.tlog");
    std::fs::write(&tlog, log).unwrap();
    let lines = harness(
        &root,
        &tlog,
        &["--command", "mark", "--params", r#"{"label":"Waypoint A"}"#],
    );
    assert_eq!(lines[0]["describe"]["panels"][0]["title"], "Home");
    let distances = effects_of(&lines, "on_messages", "items");
    assert!(distances.iter().all(|i| i["key"] == "home.distance_m"));
    let farthest = distances
        .iter()
        .filter_map(|i| i["value"].as_f64())
        .fold(0.0_f64, f64::max);
    assert!((100.0..=170.0).contains(&farthest), "{farthest}");
    assert_eq!(
        effects_of(&lines, "on_messages", "features")[0]["id"],
        "home-circle"
    );
    let notices = effects_of(&lines, "on_items", "notifications");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["title"], "Battery at 19 %");
    assert_eq!(
        effects_of(&lines, "on_command", "notifications")[0]["title"],
        "Marked: Waypoint A"
    );
}

#[test]
fn the_clock_plugin_ticks_with_no_vehicle_and_its_buttons_carry_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/clock-ts");
    let work = tempfile::tempdir().unwrap();
    // Ten seconds of a flight it never reads: ticks come whatever arrives.
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    let mut log = Vec::new();
    for step in 0..10u32 {
        let at_us = u64::from(step) * 1_000_000;
        uxduck_mavlink::tlog::record(&mut log, at_us, &fake.heartbeat()).unwrap();
    }
    let tlog = work.path().join("flight.tlog");
    std::fs::write(&tlog, log).unwrap();
    let lines = harness(
        &root,
        &tlog,
        &["--command", "timer-start", "--params", r#"{"minutes":5}"#],
    );
    let panels = lines[0]["describe"]["panels"].as_array().unwrap();
    assert_eq!(
        panels
            .iter()
            .map(|p| p["title"].clone())
            .collect::<Vec<_>>(),
        ["Clock", "Stopwatch", "Timer", "Alarm"]
    );
    assert!(panels.iter().all(|p| p["tab"] == true));
    assert_eq!(panels[0]["widgets"][0]["type"], "display");
    assert_eq!(panels[2]["widgets"][2]["params"], json!({ "minutes": 1 }));
    // Started with nothing kept, it says what it shows, keeps its state and
    // asks to be woken in a minute to set the clock again: no ticks (S104).
    let started = lines.iter().find(|l| l["call"] == "on_start").unwrap();
    assert!(
        started["effects"]["store"]["timer"].is_object(),
        "{started}"
    );
    assert_eq!(started["effects"]["wakeInMs"], 60_000);
    assert!(
        lines.iter().all(|l| l["call"] != "on_tick"),
        "nothing to do for ten seconds"
    );
    let shown = effects_of(&lines, "on_start", "items");
    assert!(
        shown.iter().all(|i| i.get("vehicleId").is_none()),
        "the plugin's own"
    );
    let clock = shown.iter().find(|i| i["key"] == "clock.now_at").unwrap();
    assert_eq!(clock["unit"], "time");
    assert_eq!(clock["ratePerS"], 1.0);
    let elapsed = shown
        .iter()
        .find(|i| i["key"] == "stopwatch.elapsed_s")
        .unwrap();
    assert_eq!(elapsed["unit"], "second");
    assert_eq!(elapsed["value"], 0.0);
    assert!(elapsed.get("ratePerS").is_none(), "stopped: not counting");
    // A timer button's parameters reach the plugin; it counts down, kept.
    let started = effects_of(&lines, "on_command", "items");
    let left = started.iter().find(|i| i["key"] == "timer.left_s").unwrap();
    assert!(
        (299.0..=300.0).contains(&left["value"].as_f64().unwrap()),
        "{left}"
    );
    assert_eq!(left["ratePerS"], -1.0);
    let state = started.iter().find(|i| i["key"] == "timer.state").unwrap();
    assert_eq!(state["text"], "Running");
    let command = lines.iter().find(|l| l["call"] == "on_command").unwrap();
    assert_eq!(command["effects"]["store"]["timer"]["setMs"], 300_000.0);
    // Woken in a minute; sooner only for a timer that ends sooner.
    assert_eq!(command["effects"]["wakeInMs"], 60_000);
}

/// The alarm takes a moment the app worked out from a time of day, and
/// counts down to it; the plugin needs no time zone.
#[test]
fn the_clock_plugin_alarm_counts_down_to_the_moment_it_is_given() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/clock-ts");
    let work = tempfile::tempdir().unwrap();
    let mut fake = uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB);
    let mut log = Vec::new();
    uxduck_mavlink::tlog::record(&mut log, 0, &fake.heartbeat()).unwrap();
    let tlog = work.path().join("flight.tlog");
    std::fs::write(&tlog, log).unwrap();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        + 600.0;
    let params = json!({ "at": at }).to_string();
    let lines = harness(
        &root,
        &tlog,
        &["--command", "alarm-set", "--params", &params],
    );
    let panels = lines[0]["describe"]["panels"].as_array().unwrap();
    let alarm = panels.iter().find(|p| p["id"] == "alarm").unwrap();
    assert!(
        alarm["widgets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["type"] == "time-input" && w["command"] == "alarm-set"),
        "{alarm}"
    );
    let shown = effects_of(&lines, "on_command", "items");
    let left = shown.iter().find(|i| i["key"] == "alarm.left_s").unwrap();
    assert!(
        (300.0..=600.0).contains(&left["value"].as_f64().unwrap()),
        "{left}"
    );
    assert_eq!(left["ratePerS"], -1.0);
    let rings = shown.iter().find(|i| i["key"] == "alarm.at_at").unwrap();
    assert_eq!(rings["unit"], "time");
    assert!(
        (rings["value"].as_f64().unwrap() - at).abs() < 0.01,
        "{rings}"
    );
    let state = shown.iter().find(|i| i["key"] == "alarm.state").unwrap();
    assert_eq!(state["text"], "Counting down");
    let command = lines.iter().find(|l| l["call"] == "on_command").unwrap();
    assert!(
        command["effects"]["store"]["alarm"]["at"].is_number(),
        "kept"
    );
    // Ten minutes off: woken in a minute to set the clock, then again.
    assert_eq!(command["effects"]["wakeInMs"], 60_000);
}

/// S104: the clock comes with UXDuck, on, and what it keeps carries a
/// running stopwatch across a restart; the Plugins page's figures and its
/// switch work against a real daemon.
#[test]
fn the_clock_comes_with_uxduck_and_its_stopwatch_runs_on_after_a_restart() {
    let call = |daemon: &Daemon, method: &str, params: Value| {
        stdout_json(&daemon.cli(&["rpc", "call", method, "--json", &params.to_string()]))
    };
    let item = |daemon: &Daemon, key: &str| -> Option<Value> {
        let items = call(daemon, "plugin/items", json!({}));
        items["plugins"]
            .as_array()?
            .iter()
            .find(|p| p["pluginId"] == "clock")?["items"]
            .as_array()?
            .iter()
            .find(|i| i["key"] == key)
            .cloned()
    };
    let daemon = Daemon::start(&[]);
    wait_for("the clock to show", || {
        item(&daemon, "clock.now_at").is_some()
    });
    let listed = String::from_utf8_lossy(&daemon.cli(&["plugin", "list"]).stdout).into_owned();
    assert!(listed.contains("clock"), "{listed}");
    // Not removable: it comes with UXDuck.
    let removed = daemon.cli(&[
        "rpc",
        "call",
        "plugin/remove",
        "--json",
        r#"{"id":"clock"}"#,
    ]);
    assert!(!removed.status.success());

    call(
        &daemon,
        "plugin/command",
        json!({ "pluginId": "clock", "command": "stopwatch-start" }),
    );
    wait_for("the stopwatch to run", || {
        item(&daemon, "stopwatch.state").is_some_and(|i| i["text"] == "Running")
    });
    std::thread::sleep(Duration::from_millis(1500));
    // The host says what it used every few seconds.
    let deadline = Instant::now() + Duration::from_secs(15);
    let usage = loop {
        let usage = call(&daemon, "plugin/usage", json!({}));
        if usage["plugins"].as_array().is_some_and(|p| !p.is_empty()) {
            break usage;
        }
        assert!(Instant::now() < deadline, "no usage came");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(usage["plugins"][0]["pluginId"], "clock");
    assert!(
        usage["plugins"][0]["callsPerMin"].as_u64().unwrap() > 0,
        "{usage}"
    );
    assert!(
        usage["plugins"][0]["storageBytes"].as_u64().unwrap() > 0,
        "{usage}"
    );

    let daemon = daemon.kill_and_restart(&[]);
    wait_for("the stopwatch to run on", || {
        item(&daemon, "stopwatch.state").is_some_and(|i| i["text"] == "Running")
            && item(&daemon, "stopwatch.elapsed_s")
                .and_then(|i| i["value"].as_f64())
                .is_some_and(|s| s >= 1.5)
    });

    // Off: stopped, its items gone, and it stays off across a restart.
    let off = call(
        &daemon,
        "plugin/setEnabled",
        json!({ "id": "clock", "enabled": false }),
    );
    assert_eq!(off["status"], "off");
    wait_for("its items to go", || {
        item(&daemon, "clock.now_at").is_none()
    });
    let daemon = daemon.kill_and_restart(&[]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(item(&daemon, "clock.now_at").is_none(), "still off");
    call(
        &daemon,
        "plugin/setEnabled",
        json!({ "id": "clock", "enabled": true }),
    );
    wait_for("the clock to show again", || {
        item(&daemon, "clock.now_at").is_some()
    });
}

#[test]
fn a_new_typescript_plugin_passes_its_own_test() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("flight-time");
    let made = Command::new(BIN)
        .args(["plugin", "new", "flight-time", "--typescript", "--dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let tested = Command::new(BIN)
        .args(["plugin", "test"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        tested.status.success(),
        "{}",
        String::from_utf8_lossy(&tested.stderr)
    );
    assert!(String::from_utf8_lossy(&tested.stderr).contains("test.json: passed"));
    // A test it fails says so.
    std::fs::write(
        dir.join("test.json"),
        r#"{"expect":{"items":["flight_time.left_min"]}}"#,
    )
    .unwrap();
    let failed = Command::new(BIN)
        .args(["plugin", "test"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("never produced flight_time.left_min")
    );
}

#[test]
fn a_script_that_never_returns_or_runs_out_of_memory_is_stopped_with_a_notice() {
    let work = tempfile::tempdir().unwrap();
    let forever = script_plugin(
        work.path(),
        "forever",
        "export function onItems(batch: unknown) { while (true) {} }",
    );
    let hungry = script_plugin(
        work.path(),
        "hungry",
        "export function describe() { let s = 'x'; while (true) s = s + s; }",
    );
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    for plugin in [&forever, &hungry] {
        let installed = daemon.cli(&["plugin", "install", plugin.to_str().unwrap()]);
        assert!(
            installed.status.success(),
            "{}",
            String::from_utf8_lossy(&installed.stderr)
        );
    }
    let state = || {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["plugin","notification"]}"#,
        ]))["snapshot"]
            .clone()
    };
    // Each runs out of time or memory on its first call: the forever one
    // when the vehicle's battery items first reach it.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let plugins = state()["plugins"].clone();
        let stopped = ["forever", "hungry"].iter().all(|id| {
            plugins.as_array().is_some_and(|plugins| {
                plugins
                    .iter()
                    .any(|plugin| plugin["id"] == *id && plugin["status"] == "failed")
            })
        });
        if stopped {
            let detail = |id: &str| {
                plugins
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["id"] == id)
                    .unwrap()["detail"]
                    .clone()
            };
            assert_eq!(
                detail("forever"),
                "on_items ran for more than 1 s and was stopped"
            );
            assert_eq!(detail("hungry"), "describe ran out of its 16 MiB of memory");
            break;
        }
        assert!(Instant::now() < deadline, "not both stopped: {plugins}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let snapshot = state();
    let notices: Vec<String> = snapshot["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        notices.contains(&"The forever plugin stopped".to_owned()),
        "{notices:?}"
    );
    assert!(
        notices.contains(&"The hungry plugin stopped".to_owned()),
        "{notices:?}"
    );
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
fn claude_code_over_mcp_finds_a_plugins_command_and_runs_it_once_allowed() {
    let work = tempfile::tempdir().unwrap();
    let plugin = camera_plugin(work.path());
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    assert!(
        daemon
            .cli(&["plugin", "install", plugin.to_str().unwrap()])
            .status
            .success()
    );
    wait_for("the plugin running", || {
        plugin_running(&daemon, "camera-manager")
    });

    let mut mcp = Mcp::start(&daemon);
    mcp.call(
        "initialize",
        &json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": { "name": "Claude Code", "version": "2.0.0" } }),
    );
    let tools = mcp.call("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"tool_search") && names.contains(&"plugin_call"),
        "{names:?}"
    );

    let found = mcp.call(
        "tools/call",
        &json!({ "name": "tool_search", "arguments": { "query": "photo" } }),
    );
    let tool = found["result"]["structuredContent"]["tools"][0].clone();
    assert_eq!(tool["command"], "photo", "{found}");
    assert_eq!(tool["pluginId"], "camera-manager");

    // Parameters the command does not take are a tool error at once.
    let wrong = mcp.call(
        "tools/call",
        &json!({ "name": "plugin_call", "arguments": { "pluginId": "camera-manager",
                 "command": "photo", "vehicleId": vehicle, "params": { "count": 3 } } }),
    );
    assert_eq!(wrong["result"]["isError"], true, "{wrong}");

    // Running it waits for an operator, then the camera answers.
    mcp.send(
        "tools/call",
        &json!({ "name": "plugin_call", "arguments": { "pluginId": "camera-manager",
                 "command": "photo", "vehicleId": vehicle } }),
    );
    wait_for("the approval", || pending_approvals(&daemon).len() == 1);
    let listed = String::from_utf8_lossy(&daemon.cli(&["approvals"]).stdout).into_owned();
    assert!(
        listed.contains("Claude Code wants to run the Camera plugin's “Take a photo” on"),
        "{listed}"
    );
    let approval = pending_approvals(&daemon)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        daemon
            .cli(&["approvals", "allow", &approval])
            .status
            .success()
    );
    let ran = mcp.answer();
    assert_eq!(ran["result"]["isError"], false, "{ran}");
    // The journal is written behind the call.
    wait_for("the call journaled", || {
        std::fs::read_to_string(daemon.dir().join("events.ndjson"))
            .is_ok_and(|journal| journal.contains(r#""type":"pluginCalled""#))
    });
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, end to end")]
fn an_agent_drafts_a_plugin_tests_it_on_the_last_minutes_and_installs_it_once_allowed() {
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/home-distance-ts/index.ts"),
    )
    .unwrap();
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    // A few seconds of flight in the link's tlog.
    std::thread::sleep(Duration::from_secs(3));

    let mut mcp = Mcp::start(&daemon);
    mcp.call(
        "initialize",
        &json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": { "name": "Claude Code", "version": "2.0.0" } }),
    );
    let tool = |mcp: &mut Mcp, name: &str, arguments: Value| {
        mcp.call(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        )["result"]
            .clone()
    };
    let drafted = tool(
        &mut mcp,
        "plugin_draft",
        json!({ "id": "distance-from-home", "name": "Distance from home", "script": script,
                "subscribe": { "items": { "keys": ["battery.*"] },
                               "messages": { "names": ["GLOBAL_POSITION_INT"] } } }),
    );
    assert_eq!(drafted["isError"], false, "{drafted}");
    let path = drafted["structuredContent"]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        path.ends_with("plugins/drafts/distance-from-home"),
        "{path}"
    );
    assert_eq!(
        drafted["structuredContent"]["writtenBy"]["agent"],
        "Claude Code"
    );

    // A script that does not parse is said so at once.
    let broken = tool(
        &mut mcp,
        "plugin_draft",
        json!({ "id": "broken", "name": "Broken", "script": "export function (" }),
    );
    assert_eq!(broken["isError"], true, "{broken}");

    let tested = tool(
        &mut mcp,
        "plugin_test",
        json!({ "id": "distance-from-home", "vehicleId": vehicle }),
    );
    assert_eq!(tested["isError"], false, "{tested}");
    let result = &tested["structuredContent"];
    assert!(result["flightS"].as_f64().unwrap() > 1.0, "{result}");
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["key"] == "home.distance_m" && i["unit"] == "meter"),
        "{result}"
    );

    // Installing waits for an operator, who sees what it may do.
    mcp.send(
        "tools/call",
        &json!({ "name": "plugin_install", "arguments": { "path": path } }),
    );
    wait_for("the approval", || pending_approvals(&daemon).len() == 1);
    let listed = String::from_utf8_lossy(&daemon.cli(&["approvals"]).stdout).into_owned();
    assert!(
        listed.contains(
            "Claude Code wants to install the plugin “Distance from home” 0.1.0, which may read"
        ),
        "{listed}"
    );
    let approval = pending_approvals(&daemon)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        daemon
            .cli(&["approvals", "allow", &approval])
            .status
            .success()
    );
    let installed = mcp.answer();
    assert_eq!(installed["result"]["isError"], false, "{installed}");
    wait_for("the distance in the vehicle's telemetry", || {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": vehicle }).to_string(),
        ]))["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["key"] == "home.distance_m"))
    });
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[test]
fn an_installed_plugin_commands_the_vehicle_as_itself() {
    let work = tempfile::tempdir().unwrap();
    let plugin = camera_plugin(work.path());
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xAB));
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online" && v["uid"].is_string())
    });
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    let installed = daemon.cli(&["plugin", "install", plugin.to_str().unwrap()]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&installed.stdout)
            .contains("Installed Camera 0.2.1, which may: read, control")
    );
    wait_for("the plugin running", || {
        plugin_running(&daemon, "camera-manager")
    });

    // A panel button, as the UI presses it.
    stdout_json(
        &daemon.cli(&[
            "rpc",
            "call",
            "plugin/command",
            "--json",
            &json!({ "pluginId": "camera-manager", "command": "photo", "vehicleId": vehicle })
                .to_string(),
        ]),
    );
    let receipts = || {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["receipt"]}"#,
        ]))["snapshot"]["receipts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    wait_for("the camera's receipt", || {
        receipts()
            .iter()
            .any(|r| r["status"] == "accepted" && r["clientId"] == "plugin:camera-manager")
    });
    // Its acknowledgement reached the plugin, which counts the photo.
    wait_for("the photo counted", || {
        let telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": vehicle }).to_string(),
        ]));
        telemetry["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|i| i["key"] == "camera.photos" && i["value"] == 1.0)
        })
    });
    let journal = std::fs::read_to_string(daemon.dir().join("events.ndjson")).unwrap();
    assert!(journal.contains(r#""clientId":"plugin:camera-manager""#));

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

#[cfg(unix)]
#[test]
fn uxduck_open_starts_the_registered_desktop_shell() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start(&["--no-gamepads"]);
    // S110 restricts app launches to the daemon's downloaded-app directory.
    let work = daemon.dir().join("desktop-app/test-version");
    std::fs::create_dir_all(&work).unwrap();
    let marker = work.join("opened");
    let shell = work.join("UXDuck");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\necho \"$UXDUCK_DATA_DIR\" > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    let registered = daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        &json!({ "expectedVersion": 0, "shellPath": shell.display().to_string() }).to_string(),
    ]);
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(daemon.cli(&["open"]).status.success());
    wait_for("the shell", || marker.exists());
    let seen = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(seen.trim(), daemon.dir().display().to_string());
    // `--print` still prints the browser address.
    let printed = String::from_utf8_lossy(&daemon.cli(&["open", "--print"]).stdout).into_owned();
    assert!(printed.contains("bootstrap="));
}

/// A stand-in for `PostHog`'s batch API: records each request body, and
/// answers 503 while `failing` is set.
struct FakePostHog {
    port: u16,
    bodies: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    failing: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakePostHog {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let failing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, fail) = (bodies.clone(), failing.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream);
                let mut length = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let failing = fail.load(std::sync::atomic::Ordering::SeqCst);
                if !failing && let Ok(json) = serde_json::from_slice::<Value>(&body) {
                    seen.lock().unwrap().push(json);
                }
                let status = if failing {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                let _ = reader.get_mut().write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-length: 12\r\nconnection: close\r\n\r\n{{\"status\":1}}").as_bytes(),
                );
            }
        });
        Self {
            port,
            bodies,
            failing,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("UXDUCK_POSTHOG_KEY", "phc_test".to_owned()),
            (
                "UXDUCK_POSTHOG_HOST",
                format!("http://127.0.0.1:{}", self.port),
            ),
        ]
    }

    /// Every event received so far, in order.
    fn events(&self) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| {
                assert_eq!(body["api_key"], "phc_test");
                body["batch"].as_array().cloned().unwrap_or_default()
            })
            .collect()
    }

    fn wait_for(&self, name: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = self.events().into_iter().find(|e| e["event"] == name) {
                return event;
            }
            assert!(Instant::now() < deadline, "{name} never arrived");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn start_counted(dir: tempfile::TempDir, posthog: &FakePostHog, extra: &[&str]) -> Daemon {
    let env = posthog.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut args = vec!["--analytics-gather-ms", "100"];
    args.extend_from_slice(extra);
    Daemon::run_with_env(dir, &["daemon"], &args, &env)
}

/// S28: a new install is counted under a fresh install id, the app's events
/// go out with it, switching analytics off sends nothing more and forgets
/// the id, and events queued while `PostHog` cannot be reached survive a
/// hard kill and arrive later, in order.
#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, told in order")]
fn analytics_go_out_from_the_outbox_and_stop_when_switched_off() {
    let posthog = FakePostHog::start();
    let daemon = start_counted(tempfile::tempdir().unwrap(), &posthog, &[]);

    let installed = posthog.wait_for("install_completed");
    let install_id = installed["properties"]["distinct_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(installed["properties"]["package"], "dev");
    assert_eq!(installed["properties"]["$lib"], "uxduck-daemon");
    assert!(installed["uuid"].as_str().is_some_and(|u| u.len() == 36));

    rpc(
        &daemon,
        "analytics/track",
        &json!({"event": "feature_used", "feature": "map"}),
    );
    let used = posthog.wait_for("feature_used");
    assert_eq!(used["properties"]["feature"], "map");
    assert_eq!(used["properties"]["distinct_id"], install_id.as_str());

    // Free text is refused, and an alias joins a consenting browser.
    let refused = daemon.cli(&[
        "rpc",
        "call",
        "analytics/track",
        "--json",
        &json!({"event": "error_shown", "code": "Where is my drone?"}).to_string(),
    ]);
    assert!(!refused.status.success());
    rpc(
        &daemon,
        "analytics/alias",
        &json!({"browserId": "0192-browser"}),
    );
    let alias = posthog.wait_for("$create_alias");
    assert_eq!(alias["properties"]["alias"], "0192-browser");

    // Off: nothing more, and no install id kept.
    let settings = rpc(
        &daemon,
        "state/subscribe",
        &json!({"entities": ["settings"]}),
    );
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    assert_eq!(settings["snapshot"]["settings"]["usageAnalytics"], true);
    rpc(
        &daemon,
        "settings/update",
        &json!({"expectedVersion": version, "usageAnalytics": false}),
    );
    let before = posthog.events().len();
    rpc(
        &daemon,
        "analytics/track",
        &json!({"event": "feature_used", "feature": "mission"}),
    );
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(posthog.events().len(), before, "nothing is sent while off");

    // On again: a new id, not joined to the old one.
    let settings = rpc(
        &daemon,
        "state/subscribe",
        &json!({"entities": ["settings"]}),
    );
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    rpc(
        &daemon,
        "settings/update",
        &json!({"expectedVersion": version, "usageAnalytics": true}),
    );
    posthog
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for feature in ["parameters", "controllers"] {
        rpc(
            &daemon,
            "analytics/track",
            &json!({"event": "feature_used", "feature": feature}),
        );
    }
    std::thread::sleep(Duration::from_millis(400));

    // A hard kill while PostHog is down; the queue is on disk.
    let mut daemon = daemon;
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    let dir = daemon.dir.take().unwrap();
    drop(daemon);
    posthog
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let _daemon = start_counted(dir, &posthog, &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let late = loop {
        let late: Vec<Value> = posthog
            .events()
            .into_iter()
            .filter(|e| {
                e["event"] == "feature_used"
                    && ["parameters", "controllers"]
                        .contains(&e["properties"]["feature"].as_str().unwrap_or(""))
            })
            .collect();
        if late.len() == 2 {
            break late;
        }
        assert!(Instant::now() < deadline, "the queued events never arrived");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(late[0]["properties"]["feature"], "parameters", "in order");
    assert_ne!(late[0]["properties"]["distinct_id"], install_id.as_str());
    assert_eq!(
        posthog
            .events()
            .iter()
            .filter(|e| e["event"] == "install_completed")
            .count(),
        1,
        "a restart is not a new install"
    );
}

/// S28: `--no-analytics` on the first start sends nothing at all and keeps
/// no install id.
#[test]
fn an_install_started_with_analytics_off_sends_nothing() {
    let posthog = FakePostHog::start();
    let daemon = start_counted(tempfile::tempdir().unwrap(), &posthog, &["--no-analytics"]);
    rpc(
        &daemon,
        "analytics/track",
        &json!({"event": "feature_used", "feature": "map"}),
    );
    std::thread::sleep(Duration::from_millis(800));
    assert!(posthog.events().is_empty());
    let settings = rpc(
        &daemon,
        "state/subscribe",
        &json!({"entities": ["settings"]}),
    );
    assert_eq!(settings["snapshot"]["settings"]["usageAnalytics"], false);
}

/// A stand-in for the UXDuck API's `/v1/feedback`: keeps each multipart
/// body, and answers 503 while `failing` is set, or 503 "not configured"
/// while `not_configured` is.
/// Each request's content type and body.
type Bodies = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

struct FakeApi {
    port: u16,
    bodies: Bodies,
    failing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    not_configured: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakeApi {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let failing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let not_configured = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, fail, unset) = (bodies.clone(), failing.clone(), not_configured.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream);
                let (mut length, mut content_type) = (0usize, String::new());
                let mut line = String::new();
                loop {
                    line.clear();
                    if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("content-type:") {
                        line["content-type:".len()..]
                            .trim()
                            .clone_into(&mut content_type);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let failing = fail.load(std::sync::atomic::Ordering::SeqCst);
                let answer = if unset.load(std::sync::atomic::Ordering::SeqCst) {
                    let json = r#"{"error":"Feedback isn't set up on UXDuck's servers yet (missing LINEAR_API_KEY). Your report wasn't sent.","code":"not-configured"}"#;
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                        json.len()
                    )
                } else if failing {
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_owned()
                } else {
                    seen.lock().unwrap().push((content_type, body));
                    let json = r#"{"status":"filed","issue":"FB-1"}"#;
                    format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                        json.len()
                    )
                };
                let _ = reader.get_mut().write_all(answer.as_bytes());
            }
        });
        Self {
            port,
            bodies,
            failing,
            not_configured,
        }
    }

    /// The parts of the `n`th body, by name.
    fn parts(&self, n: usize) -> std::collections::BTreeMap<String, Vec<u8>> {
        let bodies = self.bodies.lock().unwrap();
        let (content_type, body) = &bodies[n];
        let boundary = content_type.split("boundary=").nth(1).unwrap().to_owned();
        let marker = format!("--{boundary}");
        let mut parts = std::collections::BTreeMap::new();
        let text = body.clone();
        let mut at = 0;
        while let Some(start) = find(&text[at..], marker.as_bytes()).map(|i| at + i + marker.len())
        {
            let Some(end) = find(&text[start..], marker.as_bytes()).map(|i| start + i) else {
                break;
            };
            let part = &text[start..end];
            if let Some(split) = find(part, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&part[..split]).to_string();
                let name = head
                    .split("name=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap();
                let content = &part[split + 4..part.len() - 2];
                parts.insert(name.to_owned(), content.to_vec());
            }
            at = end;
        }
        parts
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// S31: an API without feedback set up is no network outage: the report is
/// refused at once with the API's words, not kept as if offline.
#[test]
fn feedback_says_when_the_api_is_not_set_up() {
    let api = FakeApi::start();
    api.not_configured
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let url = format!("http://127.0.0.1:{}", api.port);
    let env = [("UXDUCK_API_URL", url.as_str())];
    let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
    let report_id = "0192f3a4-5b6c-4d7e-8f90-a1b2c3d4e5f7";
    let params = json!({
        "reportId": report_id,
        "kind": "problem",
        "text": "The map froze",
        "includeDetails": false,
        "surface": "desktop",
        "ui": {
            "route": "/", "panes": ["map"], "theme": "dark", "density": "comfortable",
            "mapEngine": "maplibre", "actions": [], "appVersion": "0.1.0", "locale": "en"
        },
        "errors": []
    });
    let out = daemon.cli(&[
        "rpc",
        "call",
        "feedback/send",
        "--json",
        &params.to_string(),
    ]);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("isn't set up on UXDuck's servers"), "{said}");
    assert!(!daemon.dir().join("feedback").join(report_id).exists());
}

/// S31: a report sent while the API cannot be reached is kept, survives a
/// hard kill, and reaches the API after the restart, with the platform
/// first, UXDuck's scrubbed bundle, and the screenshot as the page sent it.
#[test]
fn feedback_is_kept_while_offline_and_sent_after_a_restart() {
    let api = FakeApi::start();
    api.failing.store(true, std::sync::atomic::Ordering::SeqCst);
    let url = format!("http://127.0.0.1:{}", api.port);
    let env = [
        ("UXDUCK_API_URL", url.as_str()),
        ("UXDUCK_FEEDBACK_RETRY_MS", "200"),
    ];
    let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
    rpc(
        &daemon,
        "link/add",
        &json!({"kind": {"type": "udp-listen", "host": "127.0.0.1", "port": 47961}}),
    );
    let report_id = "0192f3a4-5b6c-4d7e-8f90-a1b2c3d4e5f6";
    let result = rpc(
        &daemon,
        "feedback/send",
        &json!({
            "reportId": report_id,
            "kind": "problem",
            "text": "The altitude jumped when I switched to Guided",
            "screenshot": "aGVsbG8=",
            "includeDetails": true,
            "surface": "desktop",
            "replyEmail": "ana@example.com",
            "ui": {
                "route": "/", "panes": ["map"], "theme": "dark", "density": "comfortable",
                "mapEngine": "maplibre", "actions": ["open:map"], "appVersion": "0.1.0", "locale": "en"
            },
            "errors": [{ "code": "map-unavailable", "atUs": 1 }]
        }),
    );
    assert_eq!(result["status"], "queued");
    assert!(
        daemon
            .dir()
            .join("feedback")
            .join(report_id)
            .join("report.json")
            .exists()
    );

    let mut daemon = daemon;
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    let dir = daemon.dir.take().unwrap();
    drop(daemon);
    api.failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &env);

    let deadline = Instant::now() + Duration::from_secs(10);
    while api.bodies.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the kept report never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
    let parts = api.parts(0);
    let report: Value = serde_json::from_slice(&parts["report"]).unwrap();
    assert_eq!(report["id"], report_id);
    assert_eq!(report["via"], "daemon");
    assert_eq!(report["email"], "ana@example.com");
    let context = report["context"].as_str().unwrap();
    assert!(
        context.contains("UXDuck 0.1.0 · desktop · map open · no vehicle"),
        "{context}"
    );
    assert_eq!(parts["screenshot"], b"hello");

    let bundle_text = String::from_utf8(parts["bundle"].clone()).unwrap();
    let bundle: Value = serde_json::from_str(&bundle_text).unwrap();
    assert_eq!(bundle["schema"], "uxduck.feedback/1");
    assert_eq!(bundle["daemon"]["links"][0]["kind"], "udp-listen");
    assert!(bundle["report"]["platform"].is_string());
    assert!(bundle["log"].as_array().is_some_and(|log| !log.is_empty()));
    assert!(
        bundle["trace"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["method"] == "initialize"),
        "this client's recent frames (each CLI call is its own client)"
    );
    assert!(!bundle_text.contains("127.0.0.1"), "no addresses");
    assert!(
        !bundle_text.contains("ana@example.com"),
        "no e-mail addresses"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.dir().join("feedback").join(report_id).exists() {
        assert!(Instant::now() < deadline, "the sent report was not removed");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// S32: `uxduck debug panic` makes a development build panic; the panic is
/// written to the outbox as it happens and reaches `PostHog` after the next
/// start, with frames that keep nothing of the machine it ran on. With
/// analytics off, nothing is recorded.
#[test]
fn a_panic_reaches_posthog_after_the_restart_and_not_when_switched_off() {
    let posthog = FakePostHog::start();
    let mut daemon = start_counted(tempfile::tempdir().unwrap(), &posthog, &[]);
    posthog.wait_for("install_completed");
    let answer = daemon.cli(&["debug", "panic"]);
    assert!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    let status = daemon.child.wait().unwrap();
    assert!(!status.success(), "the daemon panicked and exited");

    let dir = daemon.dir.take().unwrap();
    drop(daemon);
    let _daemon = start_counted(dir, &posthog, &[]);
    let exception = posthog.wait_for("$exception");
    let first = &exception["properties"]["$exception_list"][0];
    assert_eq!(first["type"], "panic");
    assert_eq!(first["value"], "panic", "the code, never the message");
    let frames = first["stacktrace"]["frames"].as_array().unwrap();
    assert!(
        frames
            .iter()
            .any(|f| f["filename"] == "crates/uxduck/src/daemon/mod.rs"),
        "{frames:?}"
    );
    let text = exception.to_string();
    assert!(
        !text.contains("/Users/") && !text.contains("/home/"),
        "{text}"
    );
    assert!(!text.contains("a test of error reporting"), "no message");

    // Off: the panic is not recorded.
    let quiet = FakePostHog::start();
    let mut daemon = start_counted(tempfile::tempdir().unwrap(), &quiet, &["--no-analytics"]);
    assert!(daemon.cli(&["debug", "panic"]).status.success());
    let _ = daemon.child.wait();
    let dir = daemon.dir.take().unwrap();
    drop(daemon);
    let _daemon = start_counted(dir, &quiet, &["--no-analytics"]);
    std::thread::sleep(Duration::from_millis(800));
    assert!(quiet.events().is_empty());
}

/// Whether a process whose command line holds `text` is running.
#[cfg(unix)]
fn running(text: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", "--", text])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
}

/// A development daemon finds reusable binaries even after target is removed.
#[cfg(unix)]
#[test]
fn a_development_daemon_discovers_native_simulators_outside_target() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir().unwrap();
    let cache = workspace.path().join(".cache/simulators");
    let native = cache.join("native");
    std::fs::create_dir_all(&native).unwrap();
    let program = native.join("arducopter");
    std::fs::write(
        &program,
        "#!/bin/sh\necho '{\"ArduCopter\":{\"default_frame\":\"quad\",\"frames\":{\"quad\":{\"waf_target\":\"bin/arducopter\"}}}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let target = workspace.path().join("target");
    std::fs::create_dir(&target).unwrap();
    for _ in 0..2 {
        let daemon = Daemon::run_with_env(
            tempfile::tempdir().unwrap(),
            &["daemon"],
            &[],
            &[("UXDUCK_SIM_CACHE_DIR", cache.to_str().unwrap())],
        );
        let builds = stdout_json(&daemon.cli(&["sim", "builds"]));
        assert!(
            builds["binaries"].as_array().unwrap().iter().any(|build| {
                build["path"] == program.to_str().unwrap() && build["defaultFrame"] == "quad"
            }),
            "{builds}"
        );
        if target.exists() {
            std::fs::remove_dir_all(&target).unwrap();
        }
    }
}

/// The simulation manager (S17): a simulated multicopter starts from a
/// SITL build, comes back after `kill -9` and a restart, stops, and a
/// killed daemon leaves no build behind. Needs `scripts/sitl.sh`'s build in
/// `.cache/simulators/native/`; skips without the build.
#[cfg(unix)]
#[test]
fn a_simulated_vehicle_returns_after_kill_9_and_leaves_nothing_behind() {
    if native_simulator("arducopter").is_none() {
        eprintln!("skipped: no cached SITL build (scripts/sitl.sh)");
        return;
    }
    let online = |daemon: &Daemon| vehicles(daemon).iter().any(|v| v["presence"] == "online");
    let slowly = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // A place no other SITL on this machine starts at, to find its process.
    // It ends in 1, so the number prints as written.
    let place = format!("-35.3{}1,149.165,584,0", std::process::id());

    let daemon = Daemon::start(&[]);
    // Recreating simulations is opt-in (S34); this flow tests that choice.
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        r#"{"expectedVersion":0,"recreateSimulations":true}"#,
    ]));
    let started = stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "multicopter",
        "--frame",
        "X",
        "--at",
        &place,
    ]));
    // The lowest instance whose ports are free here: another simulator on
    // this computer may hold 5760 (S71).
    assert!(started["kind"]["instance"].is_u64(), "{started}");
    let id = started["id"].as_str().unwrap().to_owned();
    slowly("the simulated vehicle", &|| online(&daemon));
    assert!(running(&place));

    let daemon = daemon.kill_and_restart(&[]);
    slowly("the vehicle after a restart", &|| online(&daemon));
    assert!(daemon.cli(&["sim", "stop", &id]).status.success());
    slowly("the build to stop", &|| !running(&place));

    daemon.cli(&["sim", "start", "--at", &place]);
    slowly("a second vehicle", &|| running(&place));
    drop(daemon); // kill -9
    slowly("nothing left of the killed daemon", &|| !running(&place));
}

/// PX4 with SIH (S17): a quad started in the air through `uxduck sim
/// start --autopilot px4` is PX4 by its modes, arms, takes off to the height
/// above home and holds there by itself, and a killed daemon leaves no PX4 behind. Needs
/// `scripts/px4-sitl.sh`'s build in `.cache/simulators/native/px4/`; skips without it.
#[cfg(unix)]
#[test]
fn a_simulated_px4_quad_takes_off_and_holds() {
    if native_simulator("px4/bin/px4").is_none() {
        eprintln!("skipped: no cached PX4 build (scripts/px4-sitl.sh)");
        return;
    }
    let slowly = |what: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    let daemon = Daemon::start(&[]);
    let started = stdout_json(&daemon.cli(&[
        "sim",
        "start",
        "--vehicle",
        "multicopter",
        "--autopilot",
        "px4",
        "--in-air",
        "20",
        "--speedup",
        "2",
    ]));
    assert!(started["kind"]["instance"].is_u64(), "{started}");
    let vehicle = || vehicles(&daemon).into_iter().next();
    slowly("PX4 holding", &|| {
        vehicle()
            .is_some_and(|v| v["autopilot"] == "Px4" && v["armed"] == true && v["mode"] == "Hold")
    });
    let id = vehicle().unwrap()["id"].as_str().unwrap().to_owned();
    let height = || {
        let telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": id }).to_string(),
        ]));
        telemetry["positions"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p["altitudes"].as_array().cloned().unwrap_or_default())
            .find(|a| a["frame"] == "above-home")
            .and_then(|a| a["meters"].as_f64())
    };
    slowly("20 m above home", &|| height().is_some_and(|h| h > 17.0));
    assert!(height().is_some_and(|h| h < 25.0), "{:?}", height());

    let place = daemon.dir().join("simulators").display().to_string();
    assert!(running(&place));
    drop(daemon); // kill -9
    slowly("nothing left of the killed daemon", &|| !running(&place));
}

#[cfg(unix)]
type Files = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>;

#[cfg(unix)]
/// Like `file_server`, with byte ranges (`206 Partial Content`), and a log
/// of each request's path and range, for the simulator catalogue.
fn range_server(files: Files) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 2048];
            let len = stream.read(&mut request).unwrap_or(0);
            let text = String::from_utf8_lossy(&request[..len]).to_string();
            let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let from = text.lines().find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("range: bytes=")
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
            });
            seen.lock().unwrap().push(format!("{path} {from:?}"));
            let body = files.lock().unwrap().get(&path).cloned();
            let response = match (body, from) {
                (Some(body), Some(from)) if from < body.len() => {
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {from}-{}/{}\r\nconnection: close\r\n\r\n",
                        body.len() - from,
                        body.len() - 1,
                        body.len()
                    )
                    .into_bytes();
                    r.extend(&body[from..]);
                    r
                }
                (Some(body), _) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend(body);
                    r
                }
                (None, _) => {
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_vec()
                }
            };
            let _ = stream.write_all(&response);
        }
    });
    (port, log)
}

#[cfg(unix)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(unix)]
/// A catalogue build of `arducopter` served at `url`, claiming `sha256`,
/// zstd-compressed when `zstd`.
fn catalogue_build(
    version: &str,
    platform: &str,
    url: &str,
    size: usize,
    sha256: &str,
    zstd: bool,
) -> Value {
    let mut file = json!({ "name": "arducopter", "url": url, "size": size, "sha256": sha256 });
    if zstd {
        file["encoding"] = json!("zstd");
    }
    json!({
        "id": format!("ardupilot-arducopter-{version}-{platform}"),
        "firmware": "ardupilot",
        "program": "arducopter",
        "version": version,
        "platform": platform,
        "files": [file],
        "sourceUrl": "https://example.com/source.tar.gz"
    })
}

/// The simulator catalogue (S18): a signed catalogue lists builds; this
/// computer's download compressed, resuming a broken download, and are found
/// as builds with their version; a file listed without an encoding
/// downloads as it is; a build that does not match its checksum is refused;
/// deleting one gives the space back.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines, reason = "the demo in order")]
fn a_catalogue_build_downloads_resumes_is_checked_and_deletes() {
    let platform = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    // A stand-in for SITL that answers the probe with a frame table.
    let program = b"#!/bin/sh\necho '{\"ArduCopter\":{\"default_frame\":\"quad\",\"frames\":{\"quad\":{\"waf_target\":\"bin/arducopter\"},\"X\":{\"waf_target\":\"bin/arducopter\"}}}}'\n".to_vec();
    let files: Files = std::sync::Arc::default();
    let (port, requests) = range_server(files.clone());
    let base = format!("http://127.0.0.1:{port}");
    let hash = sha256_hex(&program);
    let zst = zstd::encode_all(&program[..], 19).unwrap();
    let zst_hash = sha256_hex(&zst);
    let mut changed = zst.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    let manifest = json!({
        "schema": 1,
        "publishedAt": "2026-09-30T00:00:00Z",
        "builds": [
            catalogue_build("4.7.1", &platform, &format!("{base}/arducopter.zst"), zst.len(), &zst_hash, true),
            // Served changed: it will not match the checksum signed for it.
            catalogue_build("4.6.0", &platform, &format!("{base}/corrupted.zst"), zst.len(), &zst_hash, true),
            // Not compressed, as a catalogue of local test builds may list it.
            catalogue_build("4.5.0", &platform, &format!("{base}/plain"), program.len(), &hash, false),
            catalogue_build("4.7.1", "sparc-plan9", &format!("{base}/arducopter.zst"), 1, &hash, true),
        ]
    })
    .to_string();
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(None, &pair.sk, manifest.as_bytes(), Some("catalogue"), None)
        .unwrap()
        .into_string();
    {
        let mut files = files.lock().unwrap();
        files.insert("/catalogue.json".into(), manifest.into_bytes());
        files.insert("/catalogue.json.minisig".into(), signature.into_bytes());
        files.insert("/arducopter.zst".into(), zst.clone());
        files.insert("/corrupted.zst".into(), changed);
        files.insert("/plain".into(), program.clone());
    }

    let url = format!("{base}/catalogue.json");
    let key = pair.pk.to_base64();
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &["daemon"],
        &[],
        &[
            ("UXDUCK_SIM_CATALOGUE_URL", url.as_str()),
            ("UXDUCK_SIM_CATALOGUE_KEY", key.as_str()),
        ],
    );
    let catalogue = |daemon: &Daemon| stdout_json(&daemon.cli(&["rpc", "call", "sim/catalogue"]));
    let entry = |daemon: &Daemon, version: &str| {
        catalogue(daemon)["builds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["build"]["version"] == version)
            .cloned()
            .unwrap_or(Value::Null)
    };
    wait_for("the catalogue", || catalogue(&daemon)["state"] == "ready");
    assert_eq!(
        catalogue(&daemon)["builds"].as_array().unwrap().len(),
        3,
        "only this computer's builds"
    );
    assert_eq!(entry(&daemon, "4.7.1")["state"], "available");
    assert_eq!(
        entry(&daemon, "4.7.1")["totalBytes"],
        zst.len(),
        "the download size is the compressed size"
    );
    assert_eq!(entry(&daemon, "4.5.0")["totalBytes"], program.len());

    // Half a compressed file from a broken download: it resumes from there.
    let id = format!("ardupilot-arducopter-4.7.1-{platform}");
    let parts = daemon
        .dir()
        .join("simulators/catalogue/downloads")
        .join(&id);
    std::fs::create_dir_all(&parts).unwrap();
    std::fs::write(parts.join("arducopter.download"), &zst[..10]).unwrap();
    assert!(daemon.cli(&["sim", "download", &id]).status.success());
    wait_for("the install", || {
        entry(&daemon, "4.7.1")["state"] == "installed"
    });
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "/arducopter.zst Some(10)"),
        "{:?}",
        requests.lock().unwrap()
    );
    let program_path = daemon
        .dir()
        .join("simulators/catalogue")
        .join(&id)
        .join("arducopter");
    assert_eq!(std::fs::read(&program_path).unwrap(), program);
    assert!(!parts.exists(), "the downloads are cleared once installed");
    let builds = stdout_json(&daemon.cli(&["sim", "builds"]));
    let installed = builds["binaries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["catalogueId"] == id.as_str())
        .cloned()
        .expect("the installed build is found");
    assert_eq!(installed["version"], "4.7.1");
    assert_eq!(installed["frames"], json!(["X", "quad"]));

    // A file that does not match its checksum is refused and deleted.
    let bad = format!("ardupilot-arducopter-4.6.0-{platform}");
    daemon.cli(&["sim", "download", &bad]);
    wait_for("the refusal", || {
        entry(&daemon, "4.6.0")["state"] == "failed"
    });
    assert!(
        entry(&daemon, "4.6.0")["detail"]
            .as_str()
            .unwrap()
            .contains("checksum")
    );
    assert!(
        !daemon
            .dir()
            .join("simulators/catalogue")
            .join(&bad)
            .exists()
    );

    // A file listed without an encoding downloads as it is.
    let plain = format!("ardupilot-arducopter-4.5.0-{platform}");
    assert!(daemon.cli(&["sim", "download", &plain]).status.success());
    wait_for("the plain install", || {
        entry(&daemon, "4.5.0")["state"] == "installed"
    });

    // Deleting gives the space back.
    let simulators = |daemon: &Daemon| {
        stdout_json(&daemon.cli(&["rpc", "call", "storage/usage"]))["categories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["category"] == "simulators")
            .map_or(0, |c| c["bytes"].as_u64().unwrap_or(0))
    };
    let before = simulators(&daemon);
    assert!(before >= program.len() as u64, "{before}");
    assert!(daemon.cli(&["sim", "delete", &id]).status.success());
    assert!(simulators(&daemon) < before);
    assert_eq!(entry(&daemon, "4.7.1")["state"], "available");
}

/// A fake of the API's accounts (S27): the code is always 123456, the token
/// `tok-1`; `down` makes it unreachable in all but name.
#[derive(Clone, Default)]
struct FakeAccounts {
    updates: std::sync::Arc<std::sync::atomic::AtomicBool>,
    signed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
    down: std::sync::Arc<std::sync::atomic::AtomicBool>,
    codes_sent: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Signed in before: the next sign-in is not a new account (S114).
    known: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The name the daemon gave its session (S114).
    session_name: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    browser_signed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deletion_asked: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[allow(clippy::too_many_lines, reason = "one fake API, route by route")]
fn fake_accounts_api(fake: FakeAccounts) -> u16 {
    use std::sync::atomic::Ordering::SeqCst;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 8192];
            let len = stream.read(&mut request).unwrap_or(0);
            let text = String::from_utf8_lossy(&request[..len]).to_string();
            if fake.down.load(SeqCst) {
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                continue;
            }
            let mut words = text.split_whitespace();
            let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let body: Value = text
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|b| serde_json::from_str(b).ok())
                .unwrap_or(Value::Null);
            let bearer = text
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-1")
                && !fake.signed_out.load(SeqCst);
            let (status, extra, reply) = match (method, path) {
                ("POST", "/v1/auth/email-otp/send-verification-otp") => {
                    fake.codes_sent
                        .lock()
                        .unwrap()
                        .push(body["email"].as_str().unwrap_or("").to_owned());
                    (200, "", json!({ "success": true }))
                }
                ("POST", "/v1/auth/sign-in/email-otp") if body["otp"] == "123456" => {
                    fake.signed_out.store(false, SeqCst);
                    let new = !fake.known.swap(true, SeqCst);
                    (
                        200,
                        "set-auth-token: tok-1\r\n",
                        json!({ "token": "tok-1", "isNew": new }),
                    )
                }
                ("POST", "/v1/auth/sign-in/email-otp") => {
                    (400, "", json!({ "message": "Invalid OTP" }))
                }
                ("GET", "/v1/me") if bearer => (
                    200,
                    "",
                    json!({
                        "user": { "id": "u1", "email": "ana@example.com", "name": "", "emailVerified": true },
                        "productUpdates": fake.updates.load(SeqCst),
                        "signedSession": { "token": "s", "expiresAt": "2026-09-30T00:05:00Z" }
                    }),
                ),
                ("PUT", "/v1/updates/me") if bearer => {
                    fake.updates.store(body["subscribed"] == true, SeqCst);
                    (200, "", json!({ "subscribed": body["subscribed"] }))
                }
                ("PUT", "/v1/me/sessions/current") if bearer => {
                    *fake.session_name.lock().unwrap() = body["name"].as_str().map(str::to_owned);
                    (200, "", json!({ "name": body["name"] }))
                }
                ("GET", "/v1/me/sessions") if bearer => {
                    let mut sessions = vec![json!({
                        "id": "s1", "current": true,
                        "computer": *fake.session_name.lock().unwrap(),
                        "userAgent": "uxduck/0.4.0",
                        "createdAt": "2026-10-11T09:00:00.000Z",
                        "lastActiveAt": "2026-10-11T09:00:00.000Z"
                    })];
                    if !fake.browser_signed_out.load(SeqCst) {
                        sessions.push(json!({
                            "id": "s2", "current": false, "computer": null,
                            "userAgent": "Mozilla/5.0 (Macintosh) Chrome/141.0.0.0 Safari/537.36",
                            "createdAt": "2026-10-10T18:30:00.000Z",
                            "lastActiveAt": "2026-10-11T08:12:00.000Z"
                        }));
                    }
                    (200, "", json!({ "sessions": sessions }))
                }
                ("DELETE", "/v1/me/sessions/s2") | ("POST", "/v1/me/sessions/sign-out-others")
                    if bearer =>
                {
                    fake.browser_signed_out.store(true, SeqCst);
                    (200, "", json!({ "signedOut": 1 }))
                }
                ("DELETE", "/v1/me/sessions/s1") if bearer => {
                    fake.signed_out.store(true, SeqCst);
                    (200, "", json!({ "signedOut": 1 }))
                }
                ("POST", "/v1/me/delete") if bearer => {
                    fake.deletion_asked.store(true, SeqCst);
                    (202, "", json!({ "status": "check-your-email" }))
                }
                ("POST", "/v1/auth/sign-out") => {
                    fake.signed_out.store(true, SeqCst);
                    (200, "", json!({ "success": true }))
                }
                _ => (401, "", json!({ "error": "not signed in" })),
            };
            let reply = reply.to_string();
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

/// Accounts (S27): `uxduck login` with an emailed code and no browser, the
/// token kept by the daemon across `kill -9`, the updates choice, what was
/// known last while offline, a wrong code refused, and `uxduck logout`.
/// Answers every request with a redirect to `location`, as Cloudflare
/// Access or a network's sign-in page does.
fn redirecting_api(location: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
        }
    });
    port
}

/// The API never redirects, so a redirect is something in the way. Sign-in
/// says what, instead of following it to a page that answers 200 and
/// claiming a code was sent.
#[test]
fn a_redirected_sign_in_says_what_is_in_the_way() {
    for (location, says) in [
        (
            "https://orth.cloudflareaccess.com/cdn-cgi/access/login/api-staging.uxduck.com",
            "Cloudflare Access",
        ),
        (
            "http://hotel-wifi.example/login",
            "The network wants you to sign in to it first",
        ),
    ] {
        let api = format!("http://127.0.0.1:{}", redirecting_api(location));
        let env = [("UXDUCK_API_URL", api.as_str())];
        let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
        let login = daemon.cli_with_input(&["login", "--email", "ana@example.com"], "123456\n");
        assert!(!login.status.success(), "no code was sent");
        let said = String::from_utf8_lossy(&login.stderr);
        assert!(said.contains(says), "{said}");
    }
}

#[test]
fn the_daemon_signs_in_with_an_emailed_code_keeps_the_token_and_signs_out() {
    use std::sync::atomic::Ordering::SeqCst;
    let fake = FakeAccounts::default();
    let api = format!("http://127.0.0.1:{}", fake_accounts_api(fake.clone()));
    let env = [("UXDUCK_API_URL", api.as_str())];
    let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
    let account = |daemon: &Daemon| stdout_json(&daemon.cli(&["rpc", "call", "account/get"]));
    assert_eq!(account(&daemon)["signedIn"], false);

    let wrong = daemon.cli_with_input(&["login", "--email", "ana@example.com"], "000000\n");
    assert!(!wrong.status.success());
    let said = String::from_utf8_lossy(&wrong.stderr);
    assert!(said.contains("Invalid OTP"), "{said}");

    let login = daemon.cli_with_input(&["login", "--email", "Ana@Example.com"], "123456\n");
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );
    assert!(String::from_utf8_lossy(&login.stderr).contains("Signed in as ana@example.com"));
    assert_eq!(
        fake.codes_sent.lock().unwrap().last().unwrap(),
        "ana@example.com"
    );
    let credentials = std::fs::read_to_string(daemon.dir().join("credentials.json")).unwrap();
    assert!(credentials.contains("tok-1"));

    let daemon = daemon.kill_and_restart_with_env(&[], &env);
    let state = account(&daemon);
    assert_eq!(state["signedIn"], true, "{state}");
    assert_eq!(state["email"], "ana@example.com");
    assert_eq!(state["productUpdates"], false);
    let on = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "account/setUpdates",
        "--json",
        r#"{"subscribed":true}"#,
    ]));
    assert_eq!(on["productUpdates"], true);

    fake.down.store(true, SeqCst);
    let offline = account(&daemon);
    assert_eq!(
        (offline["signedIn"].clone(), offline["offline"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(offline["email"], "ana@example.com");
    fake.down.store(false, SeqCst);

    assert!(daemon.cli(&["logout"]).status.success());
    assert!(fake.signed_out.load(SeqCst));
    assert_eq!(account(&daemon)["signedIn"], false);
    let credentials = std::fs::read_to_string(daemon.dir().join("credentials.json")).unwrap();
    assert!(!credentials.contains("tok-1"));
}

#[test]
#[cfg(debug_assertions)]
fn development_server_choices_survive_restart_and_restore_only_their_own_sign_in() {
    let fake = FakeAccounts::default();
    let api = format!("http://127.0.0.1:{}", fake_accounts_api(fake));
    let env = [
        ("UXDUCK_API_URL", api.as_str()),
        ("UXDUCK_LOCAL_API_URL", api.as_str()),
    ];
    let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
    let login = daemon.cli_with_input(&["login", "--email", "ana@example.com"], "123456\n");
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );
    let selected = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "account/setEnvironment",
        "--json",
        r#"{"environment":"production"}"#,
    ]));
    assert_eq!(selected["apiUrl"], api);
    assert_eq!(selected["restartRequired"], true);
    let daemon = daemon.kill_and_restart_with_env(&[], &env);
    let active = stdout_json(&daemon.cli(&["rpc", "call", "account/environment"]));
    assert_eq!(active["apiUrl"], "https://api.uxduck.com");
    assert_eq!(active["restartRequired"], false);
    assert_eq!(
        stdout_json(&daemon.cli(&["rpc", "call", "account/get"]))["signedIn"],
        false
    );
    let selected = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "account/setEnvironment",
        "--json",
        r#"{"environment":"local"}"#,
    ]));
    assert_eq!(selected["restartRequired"], true);
    let daemon = daemon.kill_and_restart_with_env(&[], &env);
    let account = stdout_json(&daemon.cli(&["rpc", "call", "account/get"]));
    assert_eq!(account["signedIn"], true);
    assert_eq!(account["email"], "ana@example.com");
    let active = stdout_json(&daemon.cli(&["rpc", "call", "account/environment"]));
    assert_eq!(active["environment"], "local");
    assert_eq!(active["apiUrl"], api);
    let default = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "account/setEnvironment",
        "--json",
        r#"{"environment":null}"#,
    ]));
    assert_eq!(default["restartRequired"], false);
    assert!(default["selected"].is_null());
}

/// Where the account is signed in (S114): the first sign-in says the
/// account was made, the session is named after the computer, the browser
/// is signed out from here, the deletion link is asked for, and signing out
/// this computer's own session forgets the token.
#[test]
fn the_account_lists_where_it_is_signed_in_signs_out_and_asks_to_delete() {
    use std::sync::atomic::Ordering::SeqCst;
    let fake = FakeAccounts::default();
    let api = format!("http://127.0.0.1:{}", fake_accounts_api(fake.clone()));
    let env = [("UXDUCK_API_URL", api.as_str())];
    let daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);

    let login = daemon.cli_with_input(&["login", "--email", "ana@example.com"], "123456\n");
    let said = String::from_utf8_lossy(&login.stderr).to_string();
    assert!(
        said.contains("Account created. Signed in as ana@example.com"),
        "{said}"
    );
    let name = fake.session_name.lock().unwrap().clone();
    assert!(name.as_deref().is_some_and(|n| !n.is_empty()), "{name:?}");

    let listed = stdout_json(&daemon.cli(&["account", "sessions", "--json"]));
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{listed}");
    assert_eq!(sessions[0]["current"], true);
    assert_eq!(sessions[0]["computer"], json!(name));
    let plain = daemon.cli(&["account", "sessions"]);
    let plain = String::from_utf8_lossy(&plain.stdout).to_string();
    assert!(
        plain.contains("(this computer)") && plain.contains("UXDuck Web"),
        "{plain}"
    );

    let out = daemon.cli(&["account", "sign-out", "s2"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("Signed out."));
    assert!(fake.browser_signed_out.load(SeqCst));
    assert_eq!(
        stdout_json(&daemon.cli(&["rpc", "call", "account/get"]))["signedIn"],
        true
    );

    let delete = daemon.cli(&["account", "delete"]);
    assert!(
        delete.status.success(),
        "{}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert!(
        String::from_utf8_lossy(&delete.stderr).contains("A link is on its way to ana@example.com")
    );
    assert!(fake.deletion_asked.load(SeqCst));

    let mine = daemon.cli(&["account", "sign-out", "s1"]);
    assert!(String::from_utf8_lossy(&mine.stderr).contains("this computer too"));
    assert_eq!(
        stdout_json(&daemon.cli(&["rpc", "call", "account/get"]))["signedIn"],
        false
    );
    let credentials = std::fs::read_to_string(daemon.dir().join("credentials.json")).unwrap();
    assert!(!credentials.contains("tok-1"));
}

/// Parameter definitions (S67): a vehicle on Copter 4.5.7 gets that
/// release's set from the signed catalogue, kept on disk, and its parameters
/// are described by name; a set a person adds from the CLI wins for it.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines, reason = "the demo in order")]
fn a_vehicles_parameters_are_described_from_the_signed_catalogue() {
    use std::io::Write as _;
    use uxduck_contracts::pdef;
    use uxduck_core::domain::param_definitions::{DefinitionSet, DefinitionSource, Firmware};
    use uxduck_mavlink::sim::FakeVehicle;

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../uxduck-contracts/fixtures/pdef");
    let read = |name: &str| std::fs::read_to_string(fixtures.join(name)).unwrap();
    let (_, xml) = pdef::from_xml(&read("copter.pdef.xml")).unwrap();
    let (_, json_defs) = pdef::from_json(&read("copter.pdef.json")).unwrap();
    let set = DefinitionSet {
        source: DefinitionSource::Ardupilot,
        firmware: Firmware::ArduCopter,
        version: Some("4.5.7".into()),
        commit: Some("2a3dc4b7bf2507120f7378a7b2fde73185e0c325".into()),
        parameters: pdef::merge(xml, &json_defs),
    };
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&serde_json::to_vec(&set).unwrap()).unwrap();
    let gz = gz.finish().unwrap();

    let files: Files = std::sync::Arc::default();
    let (port, requests) = range_server(files.clone());
    let base = format!("http://127.0.0.1:{port}");
    let manifest = json!({
        "schema": 1,
        "publishedAt": "2026-10-01T00:00:00Z",
        "sets": [{
            "id": "arducopter-4.5.7",
            "firmware": "ArduCopter",
            "version": "4.5.7",
            "commit": "2a3dc4b7bf2507120f7378a7b2fde73185e0c325",
            "file": {
                "name": "arducopter-4.5.7-2a3dc4b7bf.json.gz",
                "url": format!("{base}/sets/arducopter-4.5.7-2a3dc4b7bf.json.gz"),
                "size": gz.len(),
                "sha256": sha256_hex(&gz)
            },
            "sourceUrl": "https://github.com/ArduPilot/ardupilot/tree/2a3dc4b7bf"
        }]
    })
    .to_string();
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(None, &pair.sk, manifest.as_bytes(), Some("defs"), None)
        .unwrap()
        .into_string();
    {
        let mut files = files.lock().unwrap();
        files.insert("/parameters/catalogue.json".into(), manifest.into_bytes());
        files.insert(
            "/parameters/catalogue.json.minisig".into(),
            signature.into_bytes(),
        );
        files.insert(
            "/sets/arducopter-4.5.7-2a3dc4b7bf.json.gz".into(),
            gz.clone(),
        );
    }
    let url = format!("{base}/parameters/catalogue.json");
    let key = pair.pk.to_base64();
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &["daemon"],
        &[],
        &[
            ("UXDUCK_PARAM_CATALOGUE_URL", url.as_str()),
            ("UXDUCK_PARAM_CATALOGUE_KEY", key.as_str()),
            ("UXDUCK_SIM_CATALOGUE_URL", "none"),
        ],
    );

    let udp = free_udp_port();
    daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": udp } }).to_string(),
    ]);
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let target = format!("127.0.0.1:{udp}");
    let mut fake = FakeVehicle::new(1, 0xD0C5);
    fake.flight_sw_version = 0x0405_07ff;
    let mut buffer = [0u8; 2048];
    let definitions = |daemon: &Daemon| {
        let Some(vehicle) = vehicles(daemon).into_iter().next() else {
            return Value::Null;
        };
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "param/definitions",
            "--json",
            &json!({ "vehicleId": vehicle["id"] }).to_string(),
        ]))
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(20);
    let described = loop {
        socket.send_to(&fake.heartbeat(), &target).unwrap();
        // Telemetry carries the boot time, which starts the parameter download.
        for frame in fake.telemetry(started.elapsed().as_secs_f64()) {
            socket.send_to(&frame, &target).unwrap();
        }
        while let Ok((len, _)) = socket.recv_from(&mut buffer) {
            for reply in fake.answer(&buffer[..len]) {
                socket.send_to(&reply, &target).unwrap();
            }
        }
        let now = definitions(&daemon);
        if now["status"]["status"] == "ready" && !now["definitions"].as_array().unwrap().is_empty()
        {
            break now;
        }
        assert!(Instant::now() < deadline, "timed out: {now}");
    };
    assert_eq!(described["status"]["matched"], "exact");
    assert_eq!(described["status"]["title"], "Copter 4.5.7");
    let names: Vec<&str> = described["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "BATT_CAPACITY",
            "BATT_FS_LOW_ACT",
            "FS_THR_ENABLE",
            "RTL_ALT"
        ],
        "the fake's parameters the set describes"
    );
    assert!(
        daemon
            .dir()
            .join("parameters/sets/arducopter-4.5.7-2a3dc4b7bf.json.gz")
            .is_file(),
        "kept"
    );
    let fetched = |path: &str| {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with(path))
            .count()
    };
    assert_eq!(fetched("/sets/"), 1, "downloaded once");

    // A fork's file, added from the CLI for this vehicle, describes a
    // parameter ArduPilot's set does not.
    let vehicle_id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let fork = daemon.dir().join("fork.pdef.json");
    std::fs::write(
        &fork,
        read("copter.pdef.json").replacen(
            "\"Copter\": {",
            "\"Copter\": {\n    \"LAND_SPEED\": { \"DisplayName\": \"Our landing speed\", \"Description\": \"Ours.\", \"Units\": \"cm/s\" },",
            1,
        ),
    )
    .unwrap();
    let added = daemon.cli(&[
        "params",
        "definitions",
        "add",
        fork.to_str().unwrap(),
        "--label",
        "Our fork",
        "--vehicle-id",
        &vehicle_id,
    ]);
    assert!(added.status.success(), "{added:?}");
    wait_for("the fork's set", || {
        definitions(&daemon)["status"]["matched"] == "own"
    });
    let described = definitions(&daemon);
    assert_eq!(described["status"]["title"], "Our fork");
    assert!(
        described["definitions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == "LAND_SPEED" && d["units"] == "cm/s"),
        "{described}"
    );
    let listed = stdout_json(&daemon.cli(&["params", "definitions", "list"]));
    assert_eq!(listed["own"][0]["label"], "Our fork");
    assert!(
        std::fs::read_dir(daemon.dir().join("parameters/own"))
            .unwrap()
            .count()
            == 1,
        "kept on disk"
    );
}

/// Parameters for coding agents (S68): through `uxduck mcp`, Claude Code
/// finds a parameter by what it means, reads its definition and the set it
/// is from, and a change waits for an operator, who sees the value it
/// replaces.
#[test]
fn a_coding_agent_over_mcp_searches_parameters_and_asks_to_change_one() {
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &["daemon"],
        &[],
        &[("UXDUCK_PARAM_CATALOGUE_URL", "none")],
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../uxduck-contracts/fixtures/pdef/copter.pdef.xml");
    let added = daemon.cli(&[
        "params",
        "definitions",
        "add",
        fixture.to_str().unwrap(),
        "--label",
        "Copter 4.5.7 (test)",
        "--version",
        "4.5.7",
    ]);
    assert!(added.status.success(), "{added:?}");
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xC0DE));
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    let mut mcp = Mcp::start(&daemon);
    mcp.call(
        "initialize",
        &json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": { "name": "Claude Code", "version": "2.0.0" } }),
    );
    let tools = mcp.call("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tool in [
        "param_search",
        "param_get",
        "param_list",
        "param_diff",
        "param_set",
    ] {
        assert!(names.contains(&tool), "{tool} in {names:?}");
    }

    let search = json!({ "name": "param_search",
                         "arguments": { "query": "battery failsafe" } });
    wait_for("the parameters and their descriptions", || {
        mcp.call("tools/call", &search)["result"]["structuredContent"]["total"] == 1
    });
    let found = mcp.call("tools/call", &search);
    let content = &found["result"]["structuredContent"];
    assert_eq!(content["hits"][0]["name"], "BATT_FS_LOW_ACT");
    assert_eq!(content["hits"][0]["valueName"], "RTL");
    assert_eq!(
        content["describedBy"]["definitions"]["title"],
        "Copter 4.5.7 (test)"
    );

    // Changing one waits for an operator, who sees what it replaces.
    mcp.send(
        "tools/call",
        &json!({ "name": "param_set",
                 "arguments": { "vehicleId": vehicle, "name": "RTL_ALT", "value": 3000 } }),
    );
    wait_for("the approval", || pending_approvals(&daemon).len() == 1);
    assert_eq!(
        pending_approvals(&daemon)[0]["summary"],
        "set RTL_ALT from 1500 to 3000 cm on Alpha"
    );
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

/// The in-app agent (S68), with the scripted provider: asked why the
/// vehicle climbs before returning, it searches the parameters, reads the
/// best match and answers from its definition, naming the set.
#[test]
fn the_agent_answers_about_a_parameter_from_its_definition() {
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &["daemon"],
        &["--agent-provider", "fake"],
        &[("UXDUCK_PARAM_CATALOGUE_URL", "none")],
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../uxduck-contracts/fixtures/pdef/copter.pdef.xml");
    let added = daemon.cli(&[
        "params",
        "definitions",
        "add",
        fixture.to_str().unwrap(),
        "--label",
        "Copter 4.5.7 (test)",
    ]);
    assert!(added.status.success(), "{added:?}");
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xFACE));
    wait_for("the parameters", || {
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "param/search",
            "--json",
            r#"{"query":"RTL_ALT"}"#,
        ]))["hits"][0]["displayName"]
            == "RTL Altitude"
    });

    let chat = stdout_json(&daemon.cli(&["rpc", "call", "thread/create"]));
    let chat = chat["id"].as_str().unwrap();
    send(
        &daemon,
        chat,
        "Why does my copter climb so high before returning?",
    );
    let items = wait_for_answer(&daemon, chat, 4);
    let tools: Vec<&str> = items
        .iter()
        .filter_map(|i| i["toolCall"]["method"].as_str())
        .collect();
    assert_eq!(tools, ["param/search", "param/get"], "{items:?}");
    let answer = items.last().unwrap()["text"].as_str().unwrap();
    assert!(
        answer.starts_with("RTL_ALT is 1500 cm: RTL Altitude.")
            && answer.ends_with("(From Copter 4.5.7 (test).)"),
        "{answer}"
    );
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

/// A vehicle's history (S69): a note written yesterday, read by the in-app
/// agent with the scripted provider, which cites it; and offered to coding
/// agents over `uxduck mcp`.
#[test]
fn the_agent_reads_a_vehicles_history_and_cites_a_note() {
    let daemon = Daemon::start(&["--agent-provider", "fake"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());
    let (running, thread) = fly(port, uxduck_mavlink::sim::FakeVehicle::new(1, 0xBEAD));
    wait_for("the vehicle", || !vehicles(&daemon).is_empty());
    let yesterday = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros(),
    )
    .unwrap()
        - 86_400_000_000;
    // From the CLI, by the vehicle's name, as at a bench.
    let noted = daemon.cli(&[
        "vehicle",
        "note",
        "alpha",
        "Replaced motor 3",
        "--when",
        "yesterday",
        "--category",
        "repair",
    ]);
    assert!(noted.status.success(), "{noted:?}");
    let history = String::from_utf8_lossy(&daemon.cli(&["vehicle", "history"]).stdout).into_owned();
    assert!(history.contains("History of Alpha:"), "{history}");
    assert!(
        history.contains("Note (repair): Replaced motor 3"),
        "{history}"
    );
    assert!(history.contains("First seen, as a Quadrotor"), "{history}");

    let chat = stdout_json(&daemon.cli(&["rpc", "call", "thread/create"]));
    let chat = chat["id"].as_str().unwrap();
    send(
        &daemon,
        chat,
        "Has anything been done to this copter recently?",
    );
    let items = wait_for_answer(&daemon, chat, 3);
    let tools: Vec<&str> = items
        .iter()
        .filter_map(|i| i["toolCall"]["method"].as_str())
        .collect();
    assert_eq!(tools, ["vehicle/history"], "{items:?}");
    let answer = items.last().unwrap()["text"].as_str().unwrap();
    assert_eq!(
        answer,
        format!(
            "Yes: “Replaced motor 3”, noted for day {} since 1970.",
            yesterday / 86_400_000_000
        )
    );

    let mut mcp = Mcp::start(&daemon);
    mcp.call(
        "initialize",
        &json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                 "clientInfo": { "name": "Claude Code", "version": "2.0.0" } }),
    );
    let tools = mcp.call("tools/list", &json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"vehicle_history") && names.contains(&"vehicle_add_note"),
        "{names:?}"
    );
    let read = mcp.call(
        "tools/call",
        &json!({ "name": "vehicle_history", "arguments": { "kind": "note" } }),
    );
    assert_eq!(
        read["result"]["structuredContent"]["entries"][0]["kind"]["text"],
        "Replaced motor 3"
    );
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    thread.join().unwrap();
}

/// Rovers, boats and submarines (S37): each starts from a SITL build of
/// `ArduRover` or `ArduSub`, ready where it is, and says what it is. Needs
/// `scripts/sitl.sh rover sub`'s builds; skips without them.
#[cfg(unix)]
#[test]
fn a_rover_a_boat_and_a_submarine_start_ready_where_they_are() {
    if native_simulator("ardurover").is_none() || native_simulator("ardusub").is_none() {
        eprintln!("skipped: no cached ardurover or ardusub (scripts/sitl.sh rover sub)");
        return;
    }
    let daemon = Daemon::start(&[]);
    let builds = stdout_json(&daemon.cli(&["sim", "builds"]));
    let programs: Vec<&str> = builds["binaries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["program"].as_str())
        .collect();
    assert!(
        programs.contains(&"ardurover") && programs.contains(&"ardusub"),
        "{programs:?}"
    );
    let refused = daemon.cli(&["sim", "start", "--vehicle", "rover", "--in-air", "30"]);
    assert!(!refused.status.success(), "a rover never starts in the air");

    let mut ids = Vec::new();
    for vehicle in ["rover", "boat", "submarine"] {
        let started = stdout_json(&daemon.cli(&["sim", "start", "--vehicle", vehicle]));
        assert_eq!(started["kind"]["vehicle"], vehicle, "{started}");
        ids.push(started["id"].as_str().unwrap().to_owned());
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let heard = vehicles(&daemon);
        let types: Vec<&str> = heard
            .iter()
            .filter(|v| v["presence"] == "online")
            .filter_map(|v| v["vehicleType"].as_str())
            .collect();
        // A rover and a boat start ready, in HOLD.
        let held = heard
            .iter()
            .filter(|v| {
                matches!(
                    v["vehicleType"].as_str(),
                    Some("GroundRover" | "SurfaceBoat")
                )
            })
            .all(|v| v["mode"] == "HOLD");
        if held
            && ["GroundRover", "SurfaceBoat", "Submarine"]
                .iter()
                .all(|t| types.contains(t))
        {
            let sub = heard
                .iter()
                .find(|v| v["vehicleType"] == "Submarine")
                .unwrap();
            assert_eq!(sub["armed"], false, "ready, not launched");
            // A snapshot of it, and a new submarine from that.
            let sub_id = sub["id"].as_str().unwrap().to_owned();
            // Once it has a position to save.
            let mut snapshot = Value::Null;
            while Instant::now() < deadline {
                let output = daemon.cli(&["sim", "snapshot", &sub_id]);
                if output.status.success() {
                    snapshot = stdout_json(&output);
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            assert_eq!(snapshot["vehicle"], "submarine", "{snapshot}");
            let again = stdout_json(&daemon.cli(&[
                "sim",
                "from-snapshot",
                snapshot["id"].as_str().unwrap(),
            ]));
            assert_eq!(again["kind"]["vehicle"], "submarine", "{again}");
            ids.push(again["id"].as_str().unwrap().to_owned());
            break;
        }
        assert!(Instant::now() < deadline, "timed out: {types:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    for id in ids {
        assert!(daemon.cli(&["sim", "stop", &id]).status.success());
    }
}

#[path = "../../uxduck-media/tests/fixtures/mod.rs"]
mod media;

/// An H.265 keyframe as one PES in MPEG-TS packets, from continuity `cc`.
fn keyframe_packets(pts: u64, cc: u8) -> Vec<Vec<u8>> {
    let idr = media::hex("2601af26f9e2ef2c707d22af80710254");
    let es = media::annex_b(&[
        &media::sample_vps(),
        &media::sample_sps(),
        &media::sample_pps(),
        &idr,
    ]);
    media::packetize(0x21, &media::pes(0xe0, pts, &es, 8), cc)
}

/// RTP version 2 carrying MPEG-TS (payload type 33).
fn rtp(sequence: u16, packets: &[Vec<u8>]) -> Vec<u8> {
    let [hi, lo] = sequence.to_be_bytes();
    [
        vec![0x80, 33, hi, lo, 0, 0, 0, 0, 0, 0, 0, 1],
        packets.concat(),
    ]
    .concat()
}

#[tokio::test]
async fn a_camera_over_udp_appears_as_a_source_and_plays_on_the_video_socket() {
    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let listed = ws_call(&mut socket, 2, "video/list", json!({})).await;
    let port = u16::try_from(listed["result"]["port"].as_u64().expect("listening")).unwrap();

    let camera = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut sequence = 0u16;
    let mut cc = 0u8;
    let mut send = |packets: Vec<Vec<u8>>| {
        for datagram in packets.chunks(7) {
            camera
                .send_to(&rtp(sequence, datagram), ("127.0.0.1", port))
                .unwrap();
            sequence = sequence.wrapping_add(1);
        }
    };
    let mut pts = 90_000;
    let mut picture = |cc: &mut u8| {
        let packets = keyframe_packets(pts, *cc);
        *cc = cc.wrapping_add(u8::try_from(packets.len()).unwrap());
        pts += 1_800;
        packets
    };
    send(vec![media::pat(0x20), media::pmt(0x20)]);
    send(picture(&mut cc));
    send(picture(&mut cc));

    let source_id = format!("udp-{port}-127.0.0.1");
    let mut id = 3;
    let deadline = Instant::now() + Duration::from_secs(10);
    let source = loop {
        let listed = ws_call(&mut socket, id, "video/list", json!({})).await;
        id += 1;
        if let Some(source) = listed["result"]["sources"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["id"] == source_id.as_str()))
            .filter(|s| s["codec"].is_string())
        {
            break source.clone();
        }
        assert!(Instant::now() < deadline, "no source: {listed}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(source["codec"], "hvc1.1.2.L120");
    assert_eq!(source["container"], "rtp-ts");
    assert_eq!(source["state"], "receiving");
    assert_eq!(source["counters"]["pesLengthMaxExcess"], 8);

    // A token that was never handed out opens nothing.
    let refused = format!("ws://127.0.0.1:{}/video?token=nope", daemon.port)
        .into_client_request()
        .unwrap();
    assert!(tokio_tungstenite::connect_async(refused).await.is_err());

    let watch = ws_call(
        &mut socket,
        id,
        "video/watch",
        json!({ "sourceId": source_id }),
    )
    .await;
    let path = watch["result"]["path"].as_str().expect("path").to_owned();
    let request = format!("ws://127.0.0.1:{}{path}", daemon.port)
        .into_client_request()
        .unwrap();
    let (mut video, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("video socket");
    // Let the socket register before the pictures come.
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(picture(&mut cc));
    send(picture(&mut cc));
    send(picture(&mut cc));

    let mut messages = Vec::new();
    while messages.len() < 2 {
        let next = tokio::time::timeout(Duration::from_secs(5), video.next())
            .await
            .expect("a picture in time");
        if let Some(Ok(Message::Binary(bytes))) = next {
            messages.push(bytes.to_vec());
        }
    }
    let first = uxduck_media::wire::decode(&messages[0]).expect("a message");
    assert_eq!(first.kind, uxduck_media::wire::Kind::AccessUnit);
    assert!(first.key);
    assert_eq!(first.sequence, 0);
    assert_eq!(first.codec, Some("hvc1.1.2.L120"));
    // Parameter sets, then the picture, each with a four-byte start code.
    assert_eq!(&first.payload[..5], &[0, 0, 0, 1, 0x40]);
    let second = uxduck_media::wire::decode(&messages[1]).expect("a message");
    assert_eq!(second.sequence, 1);
    assert_eq!(second.codec, None, "sent only when it changes");
    assert_eq!(second.timestamp_us - first.timestamp_us, 20_000);

    // The open socket, for the inspector.
    let listed = ws_call(&mut socket, id + 1, "video/list", json!({})).await;
    let sockets = listed["result"]["sockets"].as_array().expect("sockets");
    assert_eq!(sockets.len(), 1);
    assert_eq!(sockets[0]["sourceId"], source_id.as_str());
    assert!(sockets[0]["sent"].as_u64().unwrap() >= 2);
}

/// Waits until `video/list` has a source with this id and a codec.
async fn video_source(socket: &mut Socket, id: &mut i64, source_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        *id += 1;
        let listed = ws_call(socket, *id, "video/list", json!({})).await;
        if let Some(source) = listed["result"]["sources"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["id"] == source_id))
            .filter(|s| s["codec"].is_string())
        {
            return source.clone();
        }
        assert!(Instant::now() < deadline, "no source: {listed}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn two_streams_from_one_computer_to_one_port_are_two_sources() {
    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let listed = ws_call(&mut socket, 2, "video/list", json!({})).await;
    let port = u16::try_from(listed["result"]["port"].as_u64().expect("listening")).unwrap();
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    // Two pipelines on one computer: RTP H.264 and MPEG-TS, interleaved.
    let rtp = uxduck_media::replay::rtp_video_datagrams(&fixture, 7);
    let ts =
        uxduck_media::replay::datagrams(&fixture, uxduck_media::replay::first_pat(&fixture), None);
    let (a, b) = (
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
    );
    let b_port = b.local_addr().unwrap().port();
    for i in 0..rtp.len().max(ts.len()) {
        if let Some(d) = rtp.get(i) {
            a.send_to(&d.bytes, ("127.0.0.1", port)).unwrap();
        }
        if let Some(d) = ts.get(i) {
            b.send_to(&d.bytes, ("127.0.0.1", port)).unwrap();
        }
        std::thread::sleep(Duration::from_micros(300));
    }
    let mut id = 2;
    let first = video_source(&mut socket, &mut id, &format!("udp-{port}-127.0.0.1")).await;
    let second_id = format!("udp-{port}-127.0.0.1-from-{b_port}");
    let second = video_source(&mut socket, &mut id, &second_id).await;
    assert_eq!(first["container"], "rtp-h264");
    assert_eq!(first["name"], format!("udp://127.0.0.1:{port}"));
    assert_eq!(second["container"], "ts");
    assert_eq!(
        second["name"],
        format!("udp://127.0.0.1:{port} (from :{b_port})")
    );
    assert!(
        first["codec"]
            .as_str()
            .is_some_and(|c| c.starts_with("avc1"))
    );
    assert!(
        second["codec"]
            .as_str()
            .is_some_and(|c| c.starts_with("avc1"))
    );
}

#[tokio::test]
async fn rtp_h264_with_a_dynamic_payload_type_plays_on_the_video_socket() {
    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let listed = ws_call(&mut socket, 2, "video/list", json!({})).await;
    let port = u16::try_from(listed["result"]["port"].as_u64().expect("listening")).unwrap();
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let datagrams = uxduck_media::replay::rtp_video_datagrams(&fixture, 7);
    let camera = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let send = |from: usize, to: usize| {
        for d in &datagrams[from..to] {
            camera.send_to(&d.bytes, ("127.0.0.1", port)).unwrap();
            std::thread::sleep(Duration::from_micros(300));
        }
    };
    let half = datagrams.len() / 2;
    send(0, half);
    let source_id = format!("udp-{port}-127.0.0.1");
    let mut id = 2;
    let source = video_source(&mut socket, &mut id, &source_id).await;
    assert_eq!(source["container"], "rtp-h264");
    assert_eq!(
        (source["width"].as_u64(), source["height"].as_u64()),
        (Some(320), Some(180))
    );

    id += 1;
    let watch = ws_call(
        &mut socket,
        id,
        "video/watch",
        json!({ "sourceId": source_id }),
    )
    .await;
    let path = watch["result"]["path"].as_str().expect("path").to_owned();
    let request = format!("ws://127.0.0.1:{}{path}", daemon.port)
        .into_client_request()
        .unwrap();
    let (mut video, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("video");
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(half, datagrams.len());
    let first = loop {
        let next = tokio::time::timeout(Duration::from_secs(5), video.next())
            .await
            .expect("a picture in time");
        if let Some(Ok(Message::Binary(bytes))) = next {
            break bytes.to_vec();
        }
    };
    let message = uxduck_media::wire::decode(&first).expect("a message");
    assert!(message.key);
    assert!(message.codec.is_some_and(|c| c.starts_with("avc1.42")));
}

/// A camera speaking just enough RTSP: Digest on DESCRIBE, which answers
/// `sdp`, SETUP, PLAY, then the pattern as interleaved RTP at its own pace;
/// it hangs up at the end, `connections` times.
fn rtsp_camera(
    listener: std::net::TcpListener,
    sdp: &'static str,
    datagrams: Vec<uxduck_media::replay::Datagram>,
    connections: usize,
) {
    rtsp_camera_tracks(listener, sdp, vec![("", datagrams)], connections);
}

/// [`rtsp_camera`] with a pattern per track: PLAY sends the one whose
/// control the SETUP's address ends with (S90).
fn rtsp_camera_tracks(
    listener: std::net::TcpListener,
    sdp: &'static str,
    tracks: Vec<(&'static str, Vec<uxduck_media::replay::Datagram>)>,
    connections: usize,
) {
    rtsp_camera_with_klv(listener, sdp, tracks, None, connections);
}

/// A KLV track for [`rtsp_camera_with_klv`] (S91): its control, its RTP
/// packets, and whether the camera refuses to set it up.
struct KlvTrack {
    control: &'static str,
    datagrams: Vec<uxduck_media::replay::Datagram>,
    refuse: bool,
}

/// [`rtsp_camera_tracks`] with a KLV track, sent on channel 2 beside the
/// video once set up, or refused with 461.
#[allow(clippy::too_many_lines, reason = "a camera's whole conversation")]
fn rtsp_camera_with_klv(
    listener: std::net::TcpListener,
    sdp: &'static str,
    tracks: Vec<(&'static str, Vec<uxduck_media::replay::Datagram>)>,
    klv: Option<KlvTrack>,
    connections: usize,
) {
    use std::io::{BufRead, BufReader, Write};
    std::thread::spawn(move || {
        'connections: for stream in listener.incoming().take(connections) {
            let Ok(mut stream) = stream else { return };
            let port = stream.local_addr().unwrap().port();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut datagrams = &tracks[0].1;
            let mut klv_on = false;
            loop {
                let mut head = Vec::new();
                loop {
                    let mut line = String::new();
                    // UXDuck hung up, as it does to play another track.
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue 'connections;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    head.push(line.trim_end().to_owned());
                }
                let method = head[0].split(' ').next().unwrap_or("").to_owned();
                let cseq = head
                    .iter()
                    .find_map(|h| h.strip_prefix("CSeq: "))
                    .unwrap_or("0")
                    .to_owned();
                let authorised = head
                    .iter()
                    .any(|h| h.starts_with("Authorization: Digest username=\"admin\""));
                let reply = |status: &str, extra: &str, body: &str| {
                    format!(
                        "RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let answer = match method.as_str() {
                    "DESCRIBE" if !authorised => reply(
                        "401 Unauthorized",
                        "WWW-Authenticate: Digest realm=\"cam\", nonce=\"abc\"\r\n",
                        "",
                    ),
                    "DESCRIBE" => reply(
                        "200 OK",
                        &format!(
                            "Content-Base: rtsp://127.0.0.1:{port}/live/\r\nContent-Type: application/sdp\r\n"
                        ),
                        sdp,
                    ),
                    "SETUP"
                        if klv.as_ref().is_some_and(|k| {
                            head[0].split(' ').nth(1).unwrap_or("").ends_with(k.control)
                        }) =>
                    {
                        if klv.as_ref().is_some_and(|k| k.refuse) {
                            reply("461 Unsupported Transport", "", "")
                        } else {
                            klv_on = true;
                            reply(
                                "200 OK",
                                "Session: 1234;timeout=60\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n",
                                "",
                            )
                        }
                    }
                    "SETUP" => {
                        let uri = head[0].split(' ').nth(1).unwrap_or("");
                        if let Some((_, chosen)) =
                            tracks.iter().find(|(control, _)| uri.ends_with(control))
                        {
                            datagrams = chosen;
                        }
                        reply(
                            "200 OK",
                            "Session: 1234;timeout=60\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
                            "",
                        )
                    }
                    // PLAY, and keepalives.
                    _ => reply("200 OK", "", ""),
                };
                stream.write_all(answer.as_bytes()).unwrap();
                if method == "PLAY" {
                    let started = Instant::now();
                    let mut all: Vec<(u8, &uxduck_media::replay::Datagram)> =
                        datagrams.iter().map(|d| (0, d)).collect();
                    if let Some(k) = klv.as_ref().filter(|_| klv_on) {
                        all.extend(k.datagrams.iter().map(|d| (2, d)));
                        all.sort_by_key(|(_, d)| d.at_us);
                    }
                    for (channel, d) in all {
                        let due = Duration::from_micros(d.at_us);
                        if let Some(wait) = due.checked_sub(started.elapsed()) {
                            std::thread::sleep(wait);
                        }
                        let len = u16::try_from(d.bytes.len()).unwrap().to_be_bytes();
                        let mut frame = vec![b'$', channel, len[0], len[1]];
                        frame.extend_from_slice(&d.bytes);
                        if stream.write_all(&frame).is_err() {
                            continue 'connections;
                        }
                    }
                    break;
                }
            }
        }
    });
}

#[tokio::test]
async fn an_rtsp_camera_added_by_hand_plays_and_comes_back_after_hanging_up() {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    rtsp_camera(
        listener,
        "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=1\r\n",
        uxduck_media::replay::rtp_video_datagrams(&fixture, 7),
        2,
    );

    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let added = ws_call(
        &mut socket,
        2,
        "video/add",
        json!({ "url": format!("rtsp://admin:pw@127.0.0.1:{port}/live") }),
    )
    .await;
    let source_id = added["result"]["source"]["id"]
        .as_str()
        .expect("a source")
        .to_owned();
    assert_eq!(
        added["result"]["source"]["url"],
        format!("rtsp://admin@127.0.0.1:{port}/live")
    );

    let mut id = 2;
    let source = video_source(&mut socket, &mut id, &source_id).await;
    assert_eq!(source["container"], "rtp-h264");
    assert_eq!(source["state"], "receiving");

    // It hangs up after three seconds, and UXDuck connects again.
    let mut saw_stopped = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        id += 1;
        let listed = ws_call(&mut socket, id, "video/list", json!({})).await;
        let state = listed["result"]["sources"][0]["state"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if state == "stopped" {
            saw_stopped = true;
            assert!(
                listed["result"]["sources"][0]["stateDetail"]
                    .as_str()
                    .is_some_and(|d| d.starts_with("Can't reach rtsp://admin@"))
            );
        } else if saw_stopped && state == "receiving" {
            break;
        }
        assert!(Instant::now() < deadline, "did not come back: {listed}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    id += 1;
    ws_call(
        &mut socket,
        id,
        "video/remove",
        json!({ "sourceId": source_id }),
    )
    .await;
    id += 1;
    let listed = ws_call(&mut socket, id, "video/list", json!({})).await;
    assert_eq!(listed["result"]["sources"], json!([]));
}

#[tokio::test]
async fn an_rtsp_camera_serving_mpeg_ts_plays() {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let start = uxduck_media::replay::first_pat(&fixture);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Payload type 33 with no rtpmap, as many cameras with KLV describe it.
    rtsp_camera(
        listener,
        "v=0\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=1\r\n",
        uxduck_media::replay::datagrams(&fixture, start, Some((7, 0))),
        1,
    );

    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let added = ws_call(
        &mut socket,
        2,
        "video/add",
        json!({ "url": format!("rtsp://admin:pw@127.0.0.1:{port}/live") }),
    )
    .await;
    let source_id = added["result"]["source"]["id"]
        .as_str()
        .expect("a source")
        .to_owned();

    let mut id = 2;
    let source = video_source(&mut socket, &mut id, &source_id).await;
    assert_eq!(source["container"], "rtp-ts");
    assert_eq!(source["state"], "receiving");
}

#[tokio::test]
async fn an_rtsp_camera_offering_two_tracks_plays_the_first_then_the_one_chosen() {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let start = uxduck_media::replay::first_pat(&fixture);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // H.264 first, then MPEG-TS: the same pictures two ways, told apart by
    // the container each arrives in.
    rtsp_camera_tracks(
        listener,
        "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=framesize:96 320-240\r\n\
         a=framerate:30\r\na=control:trackID=1\r\n\
         m=video 0 RTP/AVP 33\r\na=control:trackID=2\r\n",
        vec![
            (
                "trackID=1",
                uxduck_media::replay::rtp_video_datagrams(&fixture, 7),
            ),
            (
                "trackID=2",
                uxduck_media::replay::datagrams(&fixture, start, Some((7, 0))),
            ),
        ],
        4,
    );

    let daemon = Daemon::start(&[]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let added = ws_call(
        &mut socket,
        2,
        "video/add",
        json!({ "url": format!("rtsp://admin:pw@127.0.0.1:{port}/live") }),
    )
    .await;
    let source_id = added["result"]["source"]["id"]
        .as_str()
        .expect("a source")
        .to_owned();

    let mut id = 2;
    let source = video_source(&mut socket, &mut id, &source_id).await;
    assert_eq!(source["container"], "rtp-h264");
    assert_eq!(
        source["tracks"],
        json!([
            { "number": 1, "codec": "H.264", "width": 320, "height": 240, "frameRate": 30.0, "playing": true },
            { "number": 2, "codec": "MPEG-TS" },
        ])
    );

    id += 1;
    let chosen = ws_call(
        &mut socket,
        id,
        "video/track",
        json!({ "sourceId": source_id, "track": 2 }),
    )
    .await;
    assert_eq!(
        chosen["result"]["url"],
        format!("rtsp://admin@127.0.0.1:{port}/live#track=2")
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        id += 1;
        let listed = ws_call(&mut socket, id, "video/list", json!({})).await;
        let source = &listed["result"]["sources"][0];
        if source["container"] == "rtp-ts" && source["state"] == "receiving" {
            assert_eq!(source["tracks"][1]["playing"], true);
            assert!(source["tracks"][0].get("playing").is_none());
            break;
        }
        assert!(Instant::now() < deadline, "track 2 never played: {listed}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A track the camera does not offer is refused.
    id += 1;
    let refused = ws_call(
        &mut socket,
        id,
        "video/track",
        json!({ "sourceId": source_id, "track": 3 }),
    )
    .await;
    assert_eq!(
        refused["error"]["data"]["kind"], "invalid-value",
        "{refused}"
    );
}

#[test]
fn a_vehicles_camera_names_its_stream_and_the_video_belongs_to_that_vehicle() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let video_port = listed["port"].as_u64().expect("listening");
    let port = free_udp_port();
    add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let target = format!("127.0.0.1:{port}");
    let mut fake = FakeVehicle::new(5, 0xCA4E);
    fake.camera = Some(format!("udp://127.0.0.1:{video_port}"));
    let mut buffer = [0u8; 2048];
    let mut named = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !named {
        assert!(Instant::now() < deadline, "the camera was never asked");
        socket.send_to(&fake.heartbeat(), &target).unwrap();
        socket
            .send_to(&fake.camera_heartbeat().unwrap(), &target)
            .unwrap();
        while let Ok((len, _)) = socket.recv_from(&mut buffer) {
            for reply in fake.answer(&buffer[..len]) {
                named |= matches!(
                    uxduck_mavlink::decode::decode(&reply).map(|d| d.message),
                    Some(uxduck_mavlink::decode::Message::VideoStream { .. })
                );
                socket.send_to(&reply, &target).unwrap();
            }
        }
    }
    let vehicle_id = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();

    // Video from loopback: no address could say whose it is; the camera did.
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let camera = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    for chunk in fixture.chunks(7 * 188) {
        camera
            .send_to(chunk, ("127.0.0.1", u16::try_from(video_port).unwrap()))
            .unwrap();
    }
    wait_for("the source to belong to the vehicle", || {
        let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
        listed["sources"]
            .as_array()
            .is_some_and(|s| s.iter().any(|s| s["vehicleId"] == vehicle_id.as_str()))
    });
}

/// The test pattern to the daemon's video port as a camera sends it, at
/// real pace for `seconds`, looping.
fn send_pattern(video_port: u16, seconds: f64) {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let camera = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let started = Instant::now();
    let start = uxduck_media::replay::first_pat(&fixture);
    let mut from = 0;
    while started.elapsed().as_secs_f64() < seconds {
        let base = started.elapsed();
        for d in uxduck_media::replay::datagrams(&fixture, from, None) {
            if started.elapsed().as_secs_f64() >= seconds {
                return;
            }
            if let Some(wait) =
                (base + Duration::from_micros(d.at_us)).checked_sub(started.elapsed())
            {
                std::thread::sleep(wait);
            }
            camera.send_to(&d.bytes, ("127.0.0.1", video_port)).unwrap();
        }
        from = start;
    }
}

fn recordings(daemon: &Daemon) -> Vec<Value> {
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["video-recording"]}"#,
    ]))["snapshot"]["videoRecordings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_camera_is_recorded_played_back_and_recovered_after_kill_9() {
    let daemon = Daemon::start(&[]);
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        r#"{"expectedVersion":0,"videoRecording":"always"}"#,
    ]));
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let video_port = u16::try_from(listed["port"].as_u64().expect("listening")).unwrap();

    // Three seconds of the pattern, then silence: one whole recording.
    send_pattern(video_port, 3.0);
    wait_for("the recording to end", || {
        recordings(&daemon)
            .first()
            .is_some_and(|r| r["state"] == "done")
    });
    let recording = recordings(&daemon).remove(0);
    let id = recording["id"].as_str().unwrap().to_owned();
    assert!(
        recording["durationUs"].as_u64().unwrap() > 1_000_000,
        "{recording}"
    );
    let file = std::fs::read(daemon.dir().join("video").join(format!("{id}.mp4"))).unwrap();
    assert_eq!(uxduck_media::mp4::complete_length(&file), file.len());
    assert_eq!(recording["bytes"].as_u64(), Some(file.len() as u64));

    // Played back with a token, a range at a time, as a <video> seeks.
    let playback = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "video/playback",
        "--json",
        &json!({ "recordingId": id }).to_string(),
    ]));
    let path = playback["path"].as_str().unwrap().to_owned();
    let (status, response) = daemon.get(&path, "Range: bytes=4-7\r\n");
    assert_eq!(status, 206);
    assert!(response.ends_with("ftyp"), "{response}");
    let (refused, _) = daemon.get(&format!("/recordings/{id}.mp4?token=nope"), "");
    assert_eq!(refused, 401);

    // Killed while recording: the next start ends it at its last fragment.
    let sending = std::thread::spawn(move || send_pattern(video_port, 4.0));
    wait_for("a second recording", || recordings(&daemon).len() == 2);
    std::thread::sleep(Duration::from_millis(2500));
    let daemon = daemon.kill_and_restart(&[]);
    let _ = sending.join();
    let second = recordings(&daemon)
        .into_iter()
        .find(|r| r["id"] != id.as_str())
        .unwrap();
    assert_eq!(second["state"], "recovered", "{second}");
    let file = std::fs::read(
        daemon
            .dir()
            .join("video")
            .join(format!("{}.mp4", second["id"].as_str().unwrap())),
    )
    .unwrap();
    assert_eq!(uxduck_media::mp4::complete_length(&file), file.len());
    assert!(second["durationUs"].as_u64().unwrap() > 0);
}

#[test]
fn a_cameras_klv_names_its_vehicle_and_becomes_its_telemetry() {
    use uxduck_mavlink::sim::FakeVehicle;
    use uxduck_media::klv::{encode, to_signed};

    let daemon = Daemon::start(&[]);
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let video_port = u16::try_from(listed["port"].as_u64().expect("listening")).unwrap();
    let port = free_udp_port();
    add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    let link = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut fake = FakeVehicle::new(6, 0xD00D);
    wait_for("the vehicle", || {
        link.send_to(&fake.heartbeat(), ("127.0.0.1", port))
            .unwrap();
        !vehicles(&daemon).is_empty()
    });
    let vehicle = vehicles(&daemon).remove(0);
    let (vehicle_id, name) = (
        vehicle["id"].as_str().unwrap().to_owned(),
        vehicle["name"].as_str().unwrap().to_owned(),
    );

    // KLV only, from loopback, naming the vehicle as its platform.
    let mut offsets = Vec::new();
    for (i, (lat, lon)) in [
        (0.001, -0.002),
        (0.001, 0.002),
        (-0.001, 0.002),
        (-0.001, -0.002),
    ]
    .into_iter()
    .enumerate()
    {
        let tag = u8::try_from(26 + 2 * i).unwrap();
        offsets.push((tag, to_signed(lat, 0.075, 2)));
        offsets.push((tag + 1, to_signed(lon, 0.075, 2)));
    }
    let klv = encode(
        &[
            vec![
                (10, name.clone().into_bytes()),
                (23, to_signed(-35.36, 90.0, 4)),
                (24, to_signed(149.17, 180.0, 4)),
            ],
            offsets,
        ]
        .concat(),
    );
    // Ten packets a second for three seconds, as a camera sends them.
    let camera = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut cc = 0u8;
    for n in 0..30u64 {
        let mut packets = if n % 10 == 0 {
            vec![media::pat(0x20), media::pmt(0x20)]
        } else {
            Vec::new()
        };
        let pes = media::packetize(0x28, &media::pes(0xfc, 90_000 + n * 9_000, &klv, 0), cc);
        cc = cc.wrapping_add(u8::try_from(pes.len()).unwrap());
        packets.extend(pes);
        camera
            .send_to(&packets.concat(), ("127.0.0.1", video_port))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_for("the camera's footprint in the vehicle's telemetry", || {
        let telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": vehicle_id }).to_string(),
        ]));
        telemetry["items"].as_array().is_some_and(|items| {
            items.iter().any(|i| {
                i["key"] == "camera.corner1_latitude_deg"
                    && i["value"]
                        .as_f64()
                        .is_some_and(|v| (v + 35.359).abs() < 1e-4)
            })
        })
    });
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    assert_eq!(listed["sources"][0]["vehicleId"], vehicle_id.as_str());
    assert!(listed["sources"][0]["counters"]["klv"].as_u64().unwrap() >= 1);
}

/// An RTSP camera's KLV track naming `platform`, as RFC 6597 packets ten
/// times a second for `seconds`, each local set split over two packets.
fn rtsp_klv_datagrams(platform: &str, seconds: u64) -> Vec<uxduck_media::replay::Datagram> {
    use uxduck_media::klv::{encode, to_signed};
    let mut offsets = Vec::new();
    for (i, (lat, lon)) in [
        (0.001, -0.002),
        (0.001, 0.002),
        (-0.001, 0.002),
        (-0.001, -0.002),
    ]
    .into_iter()
    .enumerate()
    {
        let tag = u8::try_from(26 + 2 * i).unwrap();
        offsets.push((tag, to_signed(lat, 0.075, 2)));
        offsets.push((tag + 1, to_signed(lon, 0.075, 2)));
    }
    let klv = encode(
        &[
            vec![
                (10, platform.as_bytes().to_vec()),
                (23, to_signed(-35.36, 90.0, 4)),
                (24, to_signed(149.17, 180.0, 4)),
            ],
            offsets,
        ]
        .concat(),
    );
    let mut sequence = 100;
    (0..seconds * 10)
        .flat_map(|n| {
            let timestamp = 5_000 + u32::try_from(n * 100).unwrap();
            let half = klv.len() / 2 + 1;
            uxduck_media::rtp_klv::packetize(&klv, 97, timestamp, 9, &mut sequence, half)
                .into_iter()
                .map(move |bytes| uxduck_media::replay::Datagram {
                    at_us: n * 100_000,
                    bytes,
                })
        })
        .collect()
}

const RTSP_WITH_KLV: &str = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=1\r\n\
     m=application 0 RTP/AVP 97\r\na=rtpmap:97 smpte336m/1000\r\na=control:trackID=2\r\n";

#[test]
fn an_rtsp_cameras_klv_track_draws_its_footprint_under_its_vehicle() {
    use uxduck_mavlink::sim::FakeVehicle;

    let daemon = Daemon::start(&[]);
    let port = free_udp_port();
    add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    let link = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut fake = FakeVehicle::new(7, 0xBEEF);
    wait_for("the vehicle", || {
        link.send_to(&fake.heartbeat(), ("127.0.0.1", port))
            .unwrap();
        !vehicles(&daemon).is_empty()
    });
    let vehicle = vehicles(&daemon).remove(0);
    let (vehicle_id, name) = (
        vehicle["id"].as_str().unwrap().to_owned(),
        vehicle["name"].as_str().unwrap().to_owned(),
    );

    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let camera = listener.local_addr().unwrap().port();
    rtsp_camera_with_klv(
        listener,
        RTSP_WITH_KLV,
        vec![("", uxduck_media::replay::rtp_video_datagrams(&fixture, 7))],
        Some(KlvTrack {
            control: "trackID=2",
            datagrams: rtsp_klv_datagrams(&name, 6),
            refuse: false,
        }),
        1,
    );
    let url = format!("rtsp://admin:pw@127.0.0.1:{camera}/live");
    daemon.cli(&[
        "rpc",
        "call",
        "video/add",
        "--json",
        &json!({ "url": url }).to_string(),
    ]);
    wait_for("the camera's footprint in the vehicle's telemetry", || {
        let telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": vehicle_id }).to_string(),
        ]));
        telemetry["items"].as_array().is_some_and(|items| {
            items.iter().any(|i| {
                i["key"] == "camera.corner1_latitude_deg"
                    && i["value"]
                        .as_f64()
                        .is_some_and(|v| (v + 35.359).abs() < 1e-4)
            })
        })
    });
    // The source says so with its next report, about a second later.
    wait_for("the source to carry KLV", || {
        stdout_json(&daemon.cli(&["rpc", "call", "video/list"]))["sources"][0]["klv"] == true
    });
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let source = &listed["sources"][0];
    assert_eq!(source["vehicleId"], vehicle_id.as_str());
    assert_eq!(source["container"], "rtp-h264");
    assert!(source["counters"]["klv"].as_u64().unwrap() >= 1);
    assert!(source.get("stateDetail").is_none(), "{source}");
}

#[test]
fn an_rtsp_camera_that_refuses_its_klv_track_still_plays_and_says_so() {
    let daemon = Daemon::start(&[]);
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../uxduck-media/fixtures/testsrc-h264.mpegts"
    ))
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let camera = listener.local_addr().unwrap().port();
    rtsp_camera_with_klv(
        listener,
        RTSP_WITH_KLV,
        vec![("", uxduck_media::replay::rtp_video_datagrams(&fixture, 7))],
        Some(KlvTrack {
            control: "trackID=2",
            datagrams: Vec::new(),
            refuse: true,
        }),
        // It plays seven seconds a connection; UXDuck comes back for more.
        5,
    );
    let url = format!("rtsp://admin:pw@127.0.0.1:{camera}/live");
    daemon.cli(&[
        "rpc",
        "call",
        "video/add",
        "--json",
        &json!({ "url": url }).to_string(),
    ]);
    wait_for("the video, with a note about the KLV track", || {
        let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
        let source = &listed["sources"][0];
        source["state"] == "receiving"
            && source["stateDetail"]
                .as_str()
                .is_some_and(|d| d.contains("refused its KLV track"))
    });
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn vehicles_on_this_computer_are_found_connected_and_a_busy_port_named() {
    use uxduck_mavlink::sim::FakeVehicle;

    let (udp_port, tcp_port) = (free_udp_port(), free_tcp_port());
    // A second UDP port, as 15010 beside 14550: another UXDuck forwarding.
    let forward_port = free_udp_port();
    let daemon = Daemon::start(&[
        "--find-udp-port",
        &format!("{udp_port},{forward_port}"),
        "--find-tcp-ports",
        &tcp_port.to_string(),
        "--find-no-gateway",
    ]);
    // A simulator on TCP, a vehicle sending to UDP, and one forwarded to
    // the second UDP port.
    let listener = std::net::TcpListener::bind(("127.0.0.1", tcp_port)).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut fake = FakeVehicle::new(2, 0x2);
                while stream.write_all(&fake.heartbeat()).is_ok() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let sending = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut fake = FakeVehicle::new(3, 0x3);
            let mut forwarded = FakeVehicle::new(4, 0x4);
            while !stop.load(Ordering::Relaxed) {
                let _ = socket.send_to(&fake.heartbeat(), ("127.0.0.1", udp_port));
                let _ = socket.send_to(&forwarded.heartbeat(), ("127.0.0.1", forward_port));
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let found = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "find/vehicles",
        "--json",
        r#"{"scope":"nearby"}"#,
    ]));
    let finds = found["finds"].as_array().unwrap();
    let tcp = finds
        .iter()
        .find(|f| f["id"] == format!("tcp:127.0.0.1:{tcp_port}"))
        .unwrap_or_else(|| panic!("no TCP find: {found}"));
    assert_eq!(tcp["vehicle"]["systemId"], 2);
    assert_eq!(tcp["place"], "this-computer");
    let udp = finds
        .iter()
        .find(|f| f["id"] == format!("udp:{udp_port}"))
        .unwrap_or_else(|| panic!("no UDP find: {found}"));
    assert_eq!(udp["vehicle"]["systemId"], 3);
    let forwarded = finds
        .iter()
        .find(|f| f["id"] == format!("udp:{forward_port}"))
        .unwrap_or_else(|| panic!("no find on the second UDP port: {found}"));
    assert_eq!(forwarded["vehicle"]["systemId"], 4);

    // Connect makes an ordinary link with what worked, and the vehicle comes.
    add_link(&daemon, &json!({ "kind": tcp["link"] }));
    wait_for("the found vehicle", || {
        vehicles(&daemon).iter().any(|v| v["systemId"] == 2)
    });

    // Another program on the UDP port: a busy find, not a gap.
    stop.store(true, Ordering::Relaxed);
    let _ = sending.join();
    let _holder = std::net::UdpSocket::bind(("0.0.0.0", udp_port)).unwrap();
    let found = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "find/vehicles",
        "--json",
        r#"{"scope":"nearby"}"#,
    ]));
    let busy = found["finds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == format!("udp:{udp_port}"))
        .unwrap_or_else(|| panic!("no busy find: {found}"))
        .clone();
    assert_eq!(busy["reason"], "busy");
    assert!(busy["detail"].as_str().unwrap().contains("is in use"));
    // The TCP vehicle is now a link's: not offered again.
    assert!(
        !found["finds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == format!("tcp:127.0.0.1:{tcp_port}"))
    );
}

/// A plugin that brings its vendor's dialect (S60): it reads the gimbal's
/// status into an item and points it on a command, both by name.
fn gimbal_plugin(into: &Path) -> std::path::PathBuf {
    let dir = into.join("acme-gimbal");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("acme.xml"),
        include_str!("../../uxduck-mavlink/fixtures/vendor_test.xml"),
    )
    .unwrap();
    std::fs::write(
        dir.join("plugin.json"),
        json!({
            "abi": 2, "id": "acme-gimbal", "name": "Acme gimbal", "version": "0.1.0",
            "script": "index.ts", "dialect": "acme.xml",
            "capabilities": ["read", "control"],
            "subscribe": { "messages": { "names": ["ACME_GIMBAL_STATUS"], "cadenceMs": 100 } },
            "sends": ["ACME_GIMBAL_POINT"],
            "commands": [{ "name": "point", "title": "Point the gimbal", "risk": "none" }]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("index.ts"),
        r#"import { value } from "@uxduck/sdk";
export function onMessages(batch: any) {
  const items = [];
  for (const m of batch.messages) {
    if (m.name === "ACME_GIMBAL_STATUS") {
      items.push(value(m.vehicleId, "gimbal.pan_deg", "Gimbal pan", "degree", m.fields.pan));
    }
  }
  return { items };
}
export function onCommand(call: any) {
  return {
    messages: [{
      vehicleId: call.vehicleId,
      name: "ACME_GIMBAL_POINT",
      fields: { target_system: 1, target_component: 154, pan: 10, tilt: -5 },
    }],
  };
}
"#,
    )
    .unwrap();
    dir
}

/// The gimbal's status as the vehicle sends it, in the test's own copy of
/// the dialect.
fn gimbal_status(sequence: u8, pan: f64) -> Vec<u8> {
    use uxduck_mavlink::named;
    let messages = named::parse_dialect(include_str!(
        "../../uxduck-mavlink/fixtures/vendor_test.xml"
    ))
    .unwrap();
    named::register("test", messages).unwrap();
    let mut fields = serde_json::Map::new();
    fields.insert("pan".into(), json!(pan));
    fields.insert("tilt".into(), json!(-20.0));
    fields.insert("serial".into(), json!("AC-0042"));
    let encoded = named::encode("ACME_GIMBAL_STATUS", &fields).unwrap();
    named::frame(&encoded, 1, 154, sequence)
}

#[test]
fn a_vendor_dialect_is_decoded_for_its_plugin_in_the_harness() {
    use uxduck_mavlink::sim::heartbeat_frame;
    let work = tempfile::tempdir().unwrap();
    let plugin = gimbal_plugin(work.path());
    let tlog = work.path().join("flight.tlog");
    let mut log = Vec::new();
    for (at, frame) in [
        heartbeat_frame(1, 1, 0),
        gimbal_status(1, 12.5),
        gimbal_status(2, 15.0),
    ]
    .iter()
    .enumerate()
    {
        uxduck_mavlink::tlog::record(&mut log, at as u64 * 200_000, frame).unwrap();
    }
    std::fs::write(&tlog, log).unwrap();
    let lines = harness(&plugin, &tlog, &["--command", "point"]);
    let read = lines.iter().find(|l| l["call"] == "on_messages").unwrap();
    let message = &read["input"]["messages"][0];
    assert_eq!(message["name"], "ACME_GIMBAL_STATUS");
    assert_eq!(message["fields"]["pan"], 12.5);
    assert_eq!(message["fields"]["serial"], "AC-0042");
    let items = effects_of(&lines, "on_messages", "items");
    assert_eq!(items.last().unwrap()["value"], 15.0);
    let sent = effects_of(&lines, "on_command", "messages");
    assert_eq!(sent[0]["name"], "ACME_GIMBAL_POINT");

    // The tlog read by name, with the plugin's dialect and without.
    let decode = |extra: &[&std::ffi::OsStr]| -> Vec<Value> {
        let out = Command::new(BIN)
            .args(["dev", "decode"])
            .arg(&tlog)
            .args(extra)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let with = decode(&["--plugin".as_ref(), plugin.as_os_str()]);
    assert_eq!(with[0]["name"], "HEARTBEAT");
    assert_eq!(with[1]["name"], "ACME_GIMBAL_STATUS");
    assert_eq!(with[1]["fields"]["pan"], 12.5);
    let without = decode(&[]);
    assert_eq!(without[1]["id"], 46_000, "unknown without its dialect");

    // A dialect that takes a built-in message's id is refused at install.
    let clash = work.path().join("clash");
    std::fs::create_dir_all(&clash).unwrap();
    std::fs::copy(plugin.join("index.ts"), clash.join("index.ts")).unwrap();
    std::fs::write(
        clash.join("acme.xml"),
        r#"<mavlink><messages><message id="0" name="ACME_BEAT"><field type="uint8_t" name="a"/></message></messages></mavlink>"#,
    )
    .unwrap();
    std::fs::write(
        clash.join("plugin.json"),
        json!({ "abi": 2, "id": "clash", "name": "Clash", "version": "0.1.0",
                "script": "index.ts", "dialect": "acme.xml", "capabilities": ["read"] })
        .to_string(),
    )
    .unwrap();
    let refused = Command::new(BIN)
        .args(["plugin", "test"])
        .arg(&clash)
        .arg("--tlog")
        .arg(&tlog)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("ACME_BEAT has id 0, which is HEARTBEAT in the ArduPilot dialect"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn a_running_plugin_reads_and_sends_its_vendors_messages_by_name() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let work = tempfile::tempdir().unwrap();
    let plugin = gimbal_plugin(work.path());
    let daemon = Daemon::start(&["--no-gamepads"]);
    let port = free_udp_port();
    let link = add_link(
        &daemon,
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    wait_up(&daemon, link["id"].as_str().unwrap());

    // A vehicle with the gimbal: heartbeats and its status, and whatever
    // comes back with the gimbal's message id kept.
    let running = Arc::new(AtomicBool::new(true));
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let thread = {
        let (running, received) = (running.clone(), received.clone());
        std::thread::spawn(move || {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let target = format!("127.0.0.1:{port}");
            let mut buffer = [0u8; 2048];
            let mut sequence = 0u8;
            while running.load(Ordering::Relaxed) {
                sequence = sequence.wrapping_add(1);
                socket
                    .send_to(
                        &uxduck_mavlink::sim::heartbeat_frame(1, 1, sequence),
                        &target,
                    )
                    .unwrap();
                socket
                    .send_to(&gimbal_status(sequence, 21.5), &target)
                    .unwrap();
                if let Ok((len, _)) = socket.recv_from(&mut buffer) {
                    let split = uxduck_mavlink::framing::split(&buffer[..len]);
                    for frame in split.frames {
                        if uxduck_mavlink::framing::header(frame)
                            .is_some_and(|h| h.message_id == 46_001)
                        {
                            received.lock().unwrap().push(frame.to_vec());
                        }
                    }
                }
            }
        })
    };
    wait_for("the vehicle", || {
        vehicles(&daemon)
            .first()
            .is_some_and(|v| v["presence"] == "online")
    });
    let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
    let installed = daemon.cli(&["plugin", "install", plugin.to_str().unwrap()]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    wait_for("the plugin running", || {
        plugin_running(&daemon, "acme-gimbal")
    });

    // Its status, decoded for the plugin, becomes an item.
    wait_for("the gimbal's pan", || {
        let telemetry = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "telemetry/get",
            "--json",
            &json!({ "id": vehicle }).to_string(),
        ]));
        telemetry["items"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|i| i["key"] == "gimbal.pan_deg" && i["value"] == 21.5)
        })
    });

    // Its command goes out as the vendor's message, encoded by name.
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "plugin/command",
        "--json",
        &json!({ "pluginId": "acme-gimbal", "command": "point", "vehicleId": vehicle }).to_string(),
    ]));
    wait_for("the gimbal pointed", || {
        !received.lock().unwrap().is_empty()
    });
    let frame = received.lock().unwrap()[0].clone();
    let payload = uxduck_mavlink::framing::payload(&frame).unwrap();
    let decoded = uxduck_mavlink::named::decode(46_001, payload).unwrap();
    assert_eq!(decoded.name, "ACME_GIMBAL_POINT");
    assert_eq!(decoded.fields["pan"], 10.0);
    assert_eq!(decoded.fields["target_component"], 154);

    running.store(false, Ordering::Relaxed);
    thread.join().unwrap();
}

/// A stand-in for `uxduck-tray` (S64): it notes its pid and lives until its
/// stdin closes, as the real one does.
#[cfg(unix)]
fn fake_tray(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("uxduck-tray");
    let pids = dir.join("tray.pids");
    std::fs::write(
        &script,
        r#"#!/bin/sh
case "$1" in
  --check) exit 0 ;;
  --login-item) echo '{"state":"unavailable","detail":"test helper"}'; exit 0 ;;
esac
echo $$ >> "$UXDUCK_TEST_TRAY_PIDS"
exec cat > /dev/null
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, pids)
}

#[cfg(unix)]
fn tray_pids(pids: &Path) -> Vec<String> {
    std::fs::read_to_string(pids)
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(unix)]
fn alive(pid: &str) -> bool {
    // A reaped process is gone; `ps` does not list zombies as running here.
    Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|out| {
            let stat = String::from_utf8_lossy(&out.stdout);
            out.status.success() && !stat.trim().is_empty() && !stat.contains('Z')
        })
}

#[cfg(unix)]
#[test]
fn the_daemon_runs_the_tray_starts_it_again_and_takes_it_along() {
    let work = tempfile::tempdir().unwrap();
    let (script, pids) = fake_tray(work.path());
    let env = [
        ("UXDUCK_TRAY", script.to_str().unwrap()),
        ("UXDUCK_TEST_TRAY_PIDS", pids.to_str().unwrap()),
        // Only the fake helper runs; Linux still needs a desktop session.
        ("DISPLAY", ":uxduck-test"),
    ];
    let mut daemon = Daemon::run_with_env(tempfile::tempdir().unwrap(), &["daemon"], &[], &env);
    wait_for("the tray", || tray_pids(&pids).len() == 1);
    let first = tray_pids(&pids)[0].clone();
    assert!(alive(&first));

    // Killed, it comes back within seconds.
    Command::new("kill").args(["-9", &first]).status().unwrap();
    wait_for("the tray again", || tray_pids(&pids).len() == 2);
    let second = tray_pids(&pids)[1].clone();

    // "Show UXDuck in the menu bar" off: it goes; on: it comes back.
    let update = |on: bool| {
        let settings = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["settings"]}"#,
        ]));
        let version = settings["snapshot"]["settings"]["version"].clone();
        stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "settings/update",
            "--json",
            &json!({ "expectedVersion": version, "showInMenuBar": on }).to_string(),
        ]))
    };
    assert_eq!(update(false)["showInMenuBar"], false);
    wait_for("the tray gone", || !alive(&second));
    update(true);
    wait_for("the tray back", || tray_pids(&pids).len() == 3);
    let third = tray_pids(&pids)[2].clone();

    // The daemon killed outright: the tray does not outlive it.
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    wait_for("the tray gone with the daemon", || !alive(&third));
}

#[cfg(unix)]
#[test]
fn a_headless_forward_starts_no_tray() {
    let work = tempfile::tempdir().unwrap();
    let (script, pids) = fake_tray(work.path());
    let listen = free_udp_port();
    let out = free_udp_port();
    let _forward = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &[
            "forward",
            "--in",
            &format!("udp:127.0.0.1:{listen}"),
            "--out",
            &format!("udp:127.0.0.1:{out}"),
        ],
        &[],
        &[
            ("UXDUCK_TRAY", script.to_str().unwrap()),
            ("UXDUCK_TEST_TRAY_PIDS", pids.to_str().unwrap()),
        ],
    );
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        tray_pids(&pids).is_empty(),
        "no tray for a headless forward"
    );
}

/// A stand-in for the relay (S78): a WebSocket at `/v1/computers/edge` that
/// answers `edge/hello` and `edge/ping`, and `GET /v1/computers` as HTTP;
/// and the media coordinator's `POST /v1/media/publish` (S84), logged as
/// "media <body>".
struct FakeRelay {
    port: u16,
    /// "connect <path> <authorization>", "hello", "ping" and "closed", in order.
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Drops the current connection, as a Worker deploy does.
    drop_now: std::sync::Arc<tokio::sync::Notify>,
    /// Frames to send the computer, such as `edge/demand` (S79).
    outbox: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// What the computer published.
    published: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    /// The computer's answers to the relay's requests (S80).
    answers: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    /// The API's grant key (S80), whose public half the fake serves.
    key: std::sync::Arc<aws_lc_rs::signature::Ed25519KeyPair>,
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

impl FakeRelay {
    #[allow(clippy::too_many_lines, reason = "the whole fake, read top to bottom")]
    async fn start() -> Self {
        use futures::{SinkExt, StreamExt};
        use tokio::io::AsyncWriteExt;
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let drop_now = std::sync::Arc::new(tokio::sync::Notify::new());
        let outbox = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let published = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let answers = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = std::sync::Arc::new(
            aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
        );
        let keys_json = {
            use aws_lc_rs::signature::KeyPair;
            json!({ "keys": [{ "kid": "k1", "x": base64url(key.public_key().as_ref()) }] })
                .to_string()
        };
        let replies = answers.clone();
        let (log, dropper) = (seen.clone(), drop_now.clone());
        let (sending, relayed) = (outbox.clone(), published.clone());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = [0u8; 2048];
                let n = stream.peek(&mut head).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&head[..n]).to_ascii_lowercase();
                if !text.contains("upgrade: websocket") {
                    let grant;
                    let json = if text.starts_with("get /v1/computers/keys") {
                        keys_json.as_str()
                    } else if text.starts_with("post /v1/media/publish") {
                        let raw = String::from_utf8_lossy(&head[..n]).into_owned();
                        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
                        log.lock().unwrap().push(format!("media {body}"));
                        let expires = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis()
                            + 3_600_000;
                        grant = json!({
                            "streamId": "s1", "protocol": "whip", "grant": "the.publish.grant",
                            "expiresAtMs": u64::try_from(expires).unwrap(),
                            "nodes": [{ "id": "lon1", "region": "eu-west-2",
                                "whipUrl": format!("http://127.0.0.1:{port}/live/s1/whip") }],
                        })
                        .to_string();
                        grant.as_str()
                    } else {
                        r#"{"computers":[{"id":"dv0000000000000001","name":"Laptop","online":false,"lastSeenAtMs":1}]}"#
                    };
                    let mut buffer = vec![0u8; n];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await;
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                                json.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                    continue;
                }
                let (log, dropper) = (log.clone(), dropper.clone());
                let (sending, relayed) = (sending.clone(), relayed.clone());
                let replies = replies.clone();
                tokio::spawn(async move {
                    let header_log = log.clone();
                    #[allow(clippy::result_large_err, reason = "tungstenite's callback type")]
                    let callback = move |request: &Request, response: Response| {
                        let auth = request
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        header_log
                            .lock()
                            .unwrap()
                            .push(format!("connect {} {auth}", request.uri()));
                        Ok(response)
                    };
                    let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await
                    else {
                        return;
                    };
                    let (mut sink, mut frames) = socket.split();
                    loop {
                        tokio::select! {
                            () = dropper.notified() => break,
                            () = tokio::time::sleep(Duration::from_millis(50)) => {
                                let queued: Vec<String> = sending.lock().unwrap().drain(..).collect();
                                for text in queued {
                                    if sink.send(Message::text(text)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            frame = frames.next() => {
                                let Some(Ok(Message::Text(text))) = frame else { break };
                                let message: Value = serde_json::from_str(&text).unwrap();
                                // An answer to the relay's own request (S80).
                                if message.get("method").is_none() {
                                    replies.lock().unwrap().push(message);
                                    continue;
                                }
                                let method = message["method"].as_str().unwrap_or_default();
                                log.lock().unwrap().push(method.trim_start_matches("edge/").to_owned());
                                if method == "edge/publish" {
                                    relayed.lock().unwrap().push(message["params"].clone());
                                    continue;
                                }
                                let result = if method == "edge/ping" {
                                    json!({ "sentAtMs": message["params"]["sentAtMs"], "relayAtMs": 1_700_000_000_000_u64 })
                                } else {
                                    json!({ "computer": { "id": "dv", "name": "Laptop", "online": true } })
                                };
                                let answer = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
                                if sink.send(Message::text(answer.to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    log.lock().unwrap().push("closed".into());
                });
            }
        });
        Self {
            port,
            seen,
            drop_now,
            outbox,
            published,
            answers,
            key,
        }
    }

    /// A grant for someone away, signed with the API's key.
    fn grant(&self, user: &str, device: &str, exp_ms: u64) -> String {
        let payload = base64url(
            json!({ "kid": "k1", "sub": user, "name": format!("{user}@example.com"), "dev": device, "exp": exp_ms })
                .to_string()
                .as_bytes(),
        );
        let signature = self.key.sign(payload.as_bytes());
        format!("{payload}.{}", base64url(signature.as_ref()))
    }

    async fn answer(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(found) = self.answers.lock().unwrap().iter().find(|a| a["id"] == id) {
                return found.clone();
            }
            assert!(Instant::now() < deadline, "no answer to {id}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn send(&self, frame: &Value) {
        self.outbox.lock().unwrap().push(frame.to_string());
    }

    fn count(&self, what: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with(what))
            .count()
    }

    async fn until(&self, what: &str, at_least: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.count(what) < at_least {
            assert!(
                Instant::now() < deadline,
                "waited for {at_least} {what}: {:?}",
                self.seen.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_in_computer_connects_to_its_account_and_comes_back_after_a_drop() {
    let relay = FakeRelay::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{}", relay.port);
    let daemon = Daemon::run_with_env(
        dir,
        &["daemon"],
        &[],
        &[
            ("UXDUCK_API_URL", api.as_str()),
            ("UXDUCK_DEVICE_NAME", "Test laptop"),
        ],
    );

    // It dials out with its session and its device id, says hello, pings.
    relay.until("ping", 1).await;
    let connect = relay.seen.lock().unwrap()[0].clone();
    assert!(
        connect.starts_with("connect /v1/computers/edge?device=dv"),
        "{connect}"
    );
    assert!(connect.contains("&name=Test%20laptop "), "{connect}");
    assert!(connect.ends_with(" Bearer t0k3n"), "{connect}");
    assert_eq!(relay.seen.lock().unwrap()[1], "hello");

    let computers = stdout_json(&daemon.cli(&["account", "computers", "--json"]));
    assert_eq!(computers["edge"]["state"], "connected", "{computers}");
    assert!(computers["edge"]["roundTripMs"].is_u64(), "{computers}");
    assert!(
        computers["thisComputer"]
            .as_str()
            .is_some_and(|id| id.starts_with("dv") && connect.contains(id)),
        "{computers}"
    );
    assert_eq!(computers["computers"][0]["name"], "Laptop");

    // A deploy drops it: it comes back on its own, after a short wait.
    relay.drop_now.notify_waiters();
    relay.until("closed", 1).await;
    relay.until("connect", 2).await;
    relay.until("hello", 2).await;

    // Switched off, it goes and stays gone.
    let output = daemon.cli(&["account", "connect", "off"]);
    assert!(output.status.success(), "{output:?}");
    relay.until("closed", 2).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        relay.count("connect"),
        2,
        "{:?}",
        relay.seen.lock().unwrap()
    );
    let computers = stdout_json(&daemon.cli(&["account", "computers", "--json"]));
    assert_eq!(computers["edge"]["state"], "off");

    // What left the computer is on the record.
    let journal = std::fs::read_to_string(daemon.dir().join("events.ndjson")).unwrap();
    assert!(
        journal.matches("edgeConnection").count() >= 4,
        "two connects and two disconnects"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "one demo, read top to bottom")]
async fn a_watched_vehicle_is_published_only_while_someone_watches() {
    let relay = FakeRelay::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{}", relay.port);
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &[("UXDUCK_API_URL", api.as_str())]);
    let port = free_udp_port();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]));
    let mut fake = Command::new(BIN)
        .args(["dev", "fake-vehicle", "--to", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    relay.until("ping", 2).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        relay.published.lock().unwrap().is_empty(),
        "nobody watching: nothing but pings"
    );

    // Someone opens the list, then the vehicle.
    relay
        .send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": { "summary": true } }));
    relay.until("publish", 1).await;
    let first = relay.published.lock().unwrap()[0].clone();
    let vehicle = &first["vehicles"][0];
    assert_eq!(vehicle["name"], "Alpha", "{first}");
    assert!(
        vehicle["peer"].is_null() && vehicle["uid"].is_null(),
        "only what a viewer shows"
    );
    assert!(
        vehicle["telemetry"]["items"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "the list has the position only"
    );
    let id = vehicle["id"].as_str().unwrap().to_owned();
    relay.send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": { "summary": true, "detail": [id] } }));
    let deadline = Instant::now() + Duration::from_secs(10);
    let detailed = loop {
        let found = relay.published.lock().unwrap().iter().find_map(|p| {
            p["vehicles"]
                .as_array()?
                .iter()
                .find(|v| {
                    v["telemetry"]["items"]
                        .as_array()
                        .is_some_and(|i| !i.is_empty())
                })
                .cloned()
        });
        if let Some(found) = found {
            break found;
        }
        assert!(Instant::now() < deadline, "no detail published");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let keys: Vec<&str> = detailed["telemetry"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"attitude.roll_deg") && keys.contains(&"battery.remaining_pct"),
        "{keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k.starts_with("vibration")),
        "only what a viewer shows: {keys:?}"
    );
    assert!(
        detailed["atMs"].as_u64().unwrap() > 1_700_000_000_000,
        "dated on the relay's clock"
    );

    // Everyone gone: it stops.
    relay.send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": {} }));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stopped = relay.published.lock().unwrap().len();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(relay.published.lock().unwrap().len(), stopped);

    // Switched off at the computer, nothing is sent even when asked.
    let settings = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        &json!({ "expectedVersion": version, "watchFromAccount": false }).to_string(),
    ]));
    relay
        .send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": { "summary": true } }));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(relay.published.lock().unwrap().len(), stopped);
    let _ = fake.kill();
    let _ = fake.wait();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "one demo, read top to bottom")]
async fn a_command_from_away_takes_the_computers_own_path() {
    let relay = FakeRelay::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{}", relay.port);
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &[("UXDUCK_API_URL", api.as_str())]);
    let port = free_udp_port();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]));
    let mut fake = Command::new(BIN)
        .args(["dev", "fake-vehicle", "--to", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    relay.until("ping", 1).await;
    let computers = stdout_json(&daemon.cli(&["account", "computers", "--json"]));
    let device = computers["thisComputer"].as_str().unwrap().to_owned();
    let vehicle = loop {
        let state = stdout_json(&daemon.cli(&[
            "rpc",
            "call",
            "state/subscribe",
            "--json",
            r#"{"entities":["vehicle"]}"#,
        ]));
        if let Some(id) = state["snapshot"]["vehicles"][0]["id"].as_str() {
            break id.to_owned();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let now_ms = || {
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let grant = relay.grant("u-ana", &device, now_ms() + 60_000);
    let command = |id: &str, command_id: &str, grant: &str, command: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "edge/command", "params": {
            "grant": grant, "commandId": command_id, "vehicleId": vehicle, "command": command, "sentAtMs": now_ms(),
        } })
    };
    let rtl = json!({ "type": "rtl" });

    // Off at the computer: refused.
    relay.send(&command("r1", "c1", &grant, rtl.clone()));
    let off = relay.answer("r1").await;
    assert_eq!(off["error"]["data"]["kind"], "capability-denied", "{off}");

    // Allowed for safety and mission at the computer.
    let settings = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        &json!({ "expectedVersion": version, "remoteControl": ["safety", "mission"] }).to_string(),
    ]));
    relay.send(&command("r2", "c2", &grant, rtl.clone()));
    let sent = relay.answer("r2").await;
    let receipt = &sent["result"];
    assert!(receipt["status"].is_string(), "{sent}");
    assert_eq!(receipt["remote"]["name"], "u-ana@example.com");
    assert_eq!(receipt["remote"]["commandId"], "c2");

    // The same command again: its receipt, not a second command.
    relay.send(&command("r3", "c2", &grant, rtl.clone()));
    let again = relay.answer("r3").await;
    assert_eq!(again["result"]["id"], receipt["id"], "{again}");

    // A grant from someone else's key, or for another computer: refused.
    // One character changed mid-signature (the last one's low bits are padding).
    let at = grant.len() - 20;
    let swapped = if &grant[at..=at] == "A" { "B" } else { "A" };
    let forged = format!("{}{swapped}{}", &grant[..at], &grant[at + 1..]);
    relay.send(&command("r4", "c4", &forged, rtl.clone()));
    assert_eq!(
        relay.answer("r4").await["error"]["data"]["kind"],
        "capability-denied"
    );
    let elsewhere = relay.grant("u-ana", "dv0000000000000009", now_ms() + 60_000);
    relay.send(&command("r5", "c5", &elsewhere, rtl.clone()));
    assert!(
        relay.answer("r5").await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("another computer")
    );

    // A real vehicle and nobody at the computer: no lease, so no mission commands.
    relay.send(
        &json!({ "jsonrpc": "2.0", "id": "r6", "method": "edge/lease", "params": {
        "grant": grant, "vehicleId": vehicle, "take": true,
    } }),
    );
    let lease = relay.answer("r6").await;
    assert!(
        lease["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Nobody is at the computer"),
        "{lease}"
    );
    relay.send(&command(
        "r7",
        "c7",
        &grant,
        json!({ "type": "set-mode", "mode": "AUTO" }),
    ));
    assert_eq!(relay.answer("r7").await["error"]["data"]["kind"], "busy");

    // Its receipt, by the id the phone gave it, after a dropped answer.
    relay.send(
        &json!({ "jsonrpc": "2.0", "id": "r8", "method": "edge/receipt", "params": {
        "grant": grant, "commandId": "c2",
    } }),
    );
    assert_eq!(relay.answer("r8").await["result"]["id"], receipt["id"]);
    let _ = fake.kill();
    let _ = fake.wait();
}

/// Whether a datagram holds UXDuck's ground station heartbeat: system 255,
/// component 190, message 0, in MAVLink 1 or 2.
fn ground_heartbeat_in(bytes: &[u8]) -> bool {
    match bytes.first() {
        Some(0xFD) if bytes.len() > 9 => {
            bytes[5] == 255 && bytes[6] == 190 && bytes[7] == 0 && bytes[8] == 0 && bytes[9] == 0
        }
        Some(0xFE) if bytes.len() > 5 => bytes[3] == 255 && bytes[4] == 190 && bytes[5] == 0,
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ground_heartbeat_stops_when_nobody_supervises_and_comes_back() {
    let daemon = Daemon::start(&[]);
    let settings = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    let version = settings["snapshot"]["settings"]["version"]
        .as_u64()
        .unwrap();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        &json!({ "expectedVersion": version, "supervisionGraceS": 5 }).to_string(),
    ]));
    // Someone at the computer, from the start.
    let cookie = paired_cookie(&daemon);
    let mut window = daemon.websocket(Some(&cookie)).await.expect("websocket");
    ws_call(&mut window, 1, "initialize", operator_ui()).await;

    let port = free_udp_port();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]));
    // A vehicle: heartbeats out, and every ground heartbeat back counted.
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let heard = Arc::new(std::sync::Mutex::new(Vec::<Instant>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let vehicle = {
        let (socket, heard, stop) = (socket.try_clone().unwrap(), heard.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut sequence = 0u8;
            let mut last = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            let mut buffer = [0u8; 2048];
            while !stop.load(Ordering::SeqCst) {
                if last.elapsed() >= Duration::from_millis(200) {
                    let frame = uxduck_mavlink::sim::heartbeat_frame(1, 1, sequence);
                    let _ = socket.send_to(&frame, ("127.0.0.1", port));
                    sequence = sequence.wrapping_add(1);
                    last = Instant::now();
                }
                if let Ok(n) = socket.recv(&mut buffer)
                    && ground_heartbeat_in(&buffer[..n])
                {
                    heard.lock().unwrap().push(Instant::now());
                }
            }
        })
    };
    let heard_within = |window: Duration| {
        let since = Instant::now()
            .checked_sub(window)
            .unwrap_or_else(Instant::now);
        heard
            .lock()
            .unwrap()
            .iter()
            .filter(|at| **at >= since)
            .count()
    };
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        heard_within(Duration::from_secs(3)) >= 2,
        "about once a second while supervised"
    );

    // The window closes: after the 5 s grace, no more, and none before it.
    drop(window);
    let closed = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        heard_within(Duration::from_secs(3)) >= 1,
        "still sent within the grace period"
    );
    let deadline = closed + Duration::from_secs(30);
    while heard_within(Duration::from_secs(3)) > 0 {
        assert!(Instant::now() < deadline, "never withheld");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        closed.elapsed() >= Duration::from_secs(5),
        "not before the grace period"
    );

    // Someone back: it resumes.
    let mut back = daemon.websocket(Some(&cookie)).await.expect("websocket");
    ws_call(&mut back, 1, "initialize", operator_ui()).await;
    let back_at = Instant::now();
    while heard_within(Duration::from_secs(3)) < 2 {
        assert!(back_at.elapsed() < Duration::from_secs(20), "never resumed");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    stop.store(true, Ordering::SeqCst);
    vehicle.join().unwrap();
}

/// A stand-in for the API's flight routes and R2 (S82): grants that point
/// back here, puts kept by name, and completion that checks them.
struct FakeStore {
    port: u16,
    objects: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
    puts: Arc<std::sync::atomic::AtomicUsize>,
    failing: Arc<AtomicBool>,
}

impl FakeStore {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let objects = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        let puts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failing = Arc::new(AtomicBool::new(false));
        let (kept, counted, fail) = (objects.clone(), puts.clone(), failing.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream);
                let mut first = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut first).unwrap_or(0) == 0 {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0
                        || line == "\r\n"
                    {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let json = |status: &str, value: &Value| {
                    let text = value.to_string();
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    )
                };
                let answer = match (method, path) {
                    ("POST", "/v1/ingest/grants") => {
                        let request: Value = serde_json::from_slice(&body).unwrap_or_default();
                        let grants: Vec<Value> = request["chunks"].as_array().into_iter().flatten().map(|c| {
                            json!({ "name": c["name"], "url": format!("http://127.0.0.1:{port}/r2/{}", c["name"].as_str().unwrap()) })
                        }).collect();
                        json("200 OK", &json!({ "grants": grants, "expiresAtMs": 0 }))
                    }
                    ("PUT", path) if path.starts_with("/r2/") => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        kept.lock()
                            .unwrap()
                            .insert(path["/r2/".len()..].to_owned(), body);
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_owned()
                    }
                    ("POST", "/v1/ingest/complete") if fail.load(Ordering::SeqCst) => {
                        json("503 Service Unavailable", &json!({ "error": "down" }))
                    }
                    ("POST", "/v1/ingest/complete") => {
                        let request: Value = serde_json::from_slice(&body).unwrap_or_default();
                        let stored = kept.lock().unwrap();
                        let (mut acknowledged, mut missing) = (Vec::new(), Vec::new());
                        for chunk in request["chunks"].as_array().into_iter().flatten() {
                            let name = chunk["name"].as_str().unwrap().to_owned();
                            if stored
                                .get(&name)
                                .is_some_and(|b| b.len() as u64 == chunk["size"].as_u64().unwrap())
                            {
                                acknowledged.push(name);
                            } else {
                                missing.push(name);
                            }
                        }
                        json(
                            "200 OK",
                            &json!({ "acknowledged": acknowledged, "missing": missing }),
                        )
                    }
                    _ => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_owned(),
                };
                let _ = reader.get_mut().write_all(answer.as_bytes());
            }
        });
        Self {
            port,
            objects,
            puts,
            failing,
        }
    }
}

#[test]
#[allow(clippy::too_many_lines, reason = "one demo, read top to bottom")]
fn flights_reach_the_account_once_through_an_outage_and_a_restart() {
    let store = FakeStore::start();
    store.failing.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{}", store.port);
    let env = [
        ("UXDUCK_API_URL", api.as_str()),
        ("UXDUCK_FLIGHT_CUT_MS", "500"),
    ];
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &env);
    let port = free_udp_port();
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "link/add",
        "--json",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }).to_string(),
    ]));
    let mut fake = Command::new(BIN)
        .args(["dev", "fake-vehicle", "--to", &format!("127.0.0.1:{port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Chunks are put while the API cannot complete them: they wait, said so.
    wait_for("a chunk put", || store.puts.load(Ordering::SeqCst) >= 2);
    let usage = stdout_json(&daemon.cli(&["rpc", "call", "storage/usage"]));
    assert!(usage["upload"]["chunks"].as_u64().unwrap() >= 1, "{usage}");
    assert!(
        usage["upload"]["problem"]
            .as_str()
            .unwrap()
            .contains("not set up"),
        "{usage}"
    );
    let _ = fake.kill();
    let _ = fake.wait();
    std::thread::sleep(Duration::from_secs(2));

    // Restarted mid-upload, then the API is back: everything is kept, each once.
    let daemon = daemon.kill_and_restart_with_env(&[], &env);
    store.failing.store(false, Ordering::SeqCst);
    wait_for("the backlog stored", || {
        let usage = stdout_json(&daemon.cli(&["rpc", "call", "storage/usage"]));
        usage["upload"]["chunks"] == 0
    });
    let objects = store.objects.lock().unwrap().clone();
    assert_eq!(
        store.puts.load(Ordering::SeqCst),
        objects.len(),
        "each chunk put once"
    );
    // Joined in order, the chunks are the tlog the computer recorded.
    let tlogs: Vec<_> = std::fs::read_dir(daemon.dir().join("tlogs"))
        .unwrap()
        .flatten()
        .collect();
    let tlog = tlogs
        .iter()
        .find(|t| t.path().extension().is_some_and(|e| e == "tlog"))
        .unwrap();
    let recording = tlog
        .path()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut joined = Vec::new();
    for (name, bytes) in &objects {
        if name.starts_with(&format!("{recording}/")) {
            joined.extend(zstd::bulk::decompress(bytes, 1 << 26).unwrap());
        }
    }
    assert_eq!(joined, std::fs::read(tlog.path()).unwrap());
    let journal = std::fs::read_to_string(daemon.dir().join("events.ndjson")).unwrap();
    assert!(
        journal.contains("flightStored"),
        "what left is on the record"
    );
}

/// A daemon signed in to the stand-in account, recording every camera.
fn recording_to_store(store: &FakeStore, extra_env: &[(&str, &str)]) -> (Daemon, u16) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{}", store.port);
    let mut env = vec![
        ("UXDUCK_API_URL", api.as_str()),
        ("UXDUCK_FLIGHT_CUT_MS", "500"),
    ];
    env.extend_from_slice(extra_env);
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &env);
    stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "settings/update",
        "--json",
        r#"{"expectedVersion":0,"videoRecording":"always"}"#,
    ]));
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let video_port = u16::try_from(listed["port"].as_u64().expect("listening")).unwrap();
    (daemon, video_port)
}

#[test]
fn a_recording_reaches_the_account_in_segments_once_it_ends() {
    let store = FakeStore::start();
    let (daemon, video_port) = recording_to_store(&store, &[]);
    send_pattern(video_port, 3.0);
    wait_for("the recording to end", || {
        recordings(&daemon)
            .first()
            .is_some_and(|r| r["state"] == "done")
    });
    let id = recordings(&daemon)[0]["id"].as_str().unwrap().to_owned();
    wait_for("the recording stored", || {
        let usage = stdout_json(&daemon.cli(&["rpc", "call", "storage/usage"]));
        usage["upload"]["chunks"] == 0
            && store
                .objects
                .lock()
                .unwrap()
                .keys()
                .any(|k| k.starts_with(&format!("{id}/")))
    });
    // Its init segment first, then segments; together, its own bytes.
    let objects = store.objects.lock().unwrap().clone();
    let names: Vec<_> = objects
        .keys()
        .filter(|k| k.starts_with(&format!("{id}/")))
        .cloned()
        .collect();
    assert!(names[0].contains(".mp4"), "{names:?}");
    assert!(names[1..].iter().all(|n| n.contains(".m4s")), "{names:?}");
    let joined: Vec<u8> = names.iter().flat_map(|n| objects[n].clone()).collect();
    let file = std::fs::read(daemon.dir().join("video").join(format!("{id}.mp4"))).unwrap();
    assert_eq!(joined, file);
    // Kept here too: the original is this computer's until the disk needs room.
    assert!(
        daemon
            .dir()
            .join("video")
            .join(format!("{id}.mp4"))
            .exists()
    );
}

#[test]
fn a_recording_evicted_before_it_was_stored_is_journaled_and_told_with_its_range() {
    let store = FakeStore::start();
    // The account cannot keep anything, and recordings may hold one byte.
    store.failing.store(true, Ordering::SeqCst);
    let (daemon, video_port) = recording_to_store(&store, &[("UXDUCK_VIDEO_BUDGET_BYTES", "1")]);
    send_pattern(video_port, 3.0);
    wait_for("the recording to end", || {
        recordings(&daemon)
            .first()
            .is_some_and(|r| r["state"] == "done")
    });
    let id = recordings(&daemon)[0]["id"].as_str().unwrap().to_owned();
    wait_for("the recording evicted", || {
        !daemon
            .dir()
            .join("video")
            .join(format!("{id}.mp4"))
            .exists()
    });
    let state = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["notification"]}"#,
    ]));
    let titles: Vec<String> = state["snapshot"]["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        titles
            .iter()
            .any(|t| t.starts_with("Part of a recording") && t.ends_with("wasn't uploaded")),
        "{titles:?}"
    );
    let journal = std::fs::read_to_string(daemon.dir().join("events.ndjson")).unwrap();
    assert!(
        journal.contains("videoLost"),
        "the lost range is on the record"
    );
    // Nothing waits for bytes that are gone.
    wait_for("the backlog emptied", || {
        let usage = stdout_json(&daemon.cli(&["rpc", "call", "storage/usage"]));
        usage["upload"]["chunks"] == 0
    });
}

/// A stand-in for the relay's viewer side (S86): `/v1/computers` lists the
/// laptop, and `/v1/computers/<id>/live` sends a snapshot, answers each
/// `edge/watch` with one, and then deltas with a gap after the second.
#[allow(clippy::too_many_lines, reason = "the whole fake, read top to bottom")]
async fn fake_live_relay() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let vehicle = |at_ms: u64, lat: f64| {
        json!({
            "id": "v1", "name": "Copter 1", "vehicleType": "Quadrotor", "autopilot": "ArduPilotMega",
            "presence": "online", "armed": true, "mode": "LOITER", "atMs": at_ms,
            "telemetry": { "vehicleId": "v1", "positions": [{
                "source": "fused", "componentId": 1, "latitudeDeg": lat, "longitudeDeg": 149.165,
                "altitudes": [{ "meters": 30.0, "frame": "above-home" }], "timeUs": 1
            }], "items": [{
                "key": "battery.remaining_pct", "label": "Battery", "unit": "percent", "value": 44.0,
                "source": "power", "componentId": 1, "timeUs": 1
            }] }
        })
    };
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut head = [0u8; 2048];
            let n = stream.peek(&mut head).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&head[..n]).to_ascii_lowercase();
            if !text.contains("upgrade: websocket") {
                let json = r#"{"computers":[{"id":"dv0000000000000002","name":"Laptop","online":true,"lastSeenAtMs":1}]}"#;
                let mut buffer = vec![0u8; n];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await;
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}",
                            json.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                continue;
            }
            let log = log.clone();
            tokio::spawn(async move {
                let path = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
                let at = path.clone();
                #[allow(clippy::result_large_err, reason = "tungstenite's callback type")]
                let callback = move |request: &Request, response: Response| {
                    request.uri().path().clone_into(&mut at.lock().unwrap());
                    Ok(response)
                };
                let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    return;
                };
                let path = path.lock().unwrap().clone();
                if !path.ends_with("/live") {
                    // The computer's own edge connection: held, silent.
                    let (_sink, mut frames) = socket.split();
                    while frames.next().await.is_some() {}
                    return;
                }
                log.lock().unwrap().push(format!("connect {path}"));
                let (mut sink, mut frames) = socket.split();
                let now = || {
                    u64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis(),
                    )
                    .unwrap()
                };
                let snapshot = |seq: u64| {
                    json!({
                        "seq": seq,
                        "computer": { "id": "dv0000000000000002", "name": "Laptop", "online": true, "lastSeenAtMs": 1 },
                        "vehicles": [vehicle(now() - 300, -35.0)],
                        "trails": [{ "vehicleId": "v1", "points": [{ "atMs": 1, "latitudeDeg": -35.0, "longitudeDeg": 149.165 }] }],
                        "relayAtMs": now(),
                    })
                };
                let _ = sink
                    .send(Message::text(
                        json!({ "jsonrpc": "2.0", "method": "edge/snapshot", "params": snapshot(1) })
                            .to_string(),
                    ))
                    .await;
                let mut watches = 0;
                while let Some(Ok(Message::Text(text))) = frames.next().await {
                    let request: Value = serde_json::from_str(&text).unwrap_or_default();
                    if request["method"] != "edge/watch" {
                        continue;
                    }
                    watches += 1;
                    log.lock().unwrap().push(format!(
                        "watch {}",
                        request["params"]["vehicleId"].as_str().unwrap_or("-")
                    ));
                    let _ = sink
                        .send(Message::text(
                            json!({ "jsonrpc": "2.0", "id": request["id"], "result": snapshot(10) })
                                .to_string(),
                        ))
                        .await;
                    if watches > 1 {
                        continue;
                    }
                    // Two in order, then one after a gap.
                    for (seq, lat) in [(11, -35.001), (12, -35.002), (14, -35.004)] {
                        let delta = json!({ "seq": seq, "sourceSeq": seq, "vehicles": [vehicle(now(), lat)], "relayAtMs": now() });
                        let _ = sink
                            .send(Message::text(
                                json!({ "jsonrpc": "2.0", "method": "edge/delta", "params": delta })
                                    .to_string(),
                            ))
                            .await;
                    }
                }
                log.lock().unwrap().push("closed".into());
            });
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_window_watches_another_computer_through_the_relay_as_this_computers_account() {
    let (relay_port, seen) = fake_live_relay().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let api = format!("http://127.0.0.1:{relay_port}");
    let daemon = Daemon::run_with_env(dir, &["daemon"], &[], &[("UXDUCK_API_URL", api.as_str())]);
    let cookie = paired_cookie(&daemon);
    let mut socket = daemon.websocket(Some(&cookie)).await.expect("socket");
    ws_call(&mut socket, 1, "initialize", operator_ui()).await;
    let watched = ws_call(
        &mut socket,
        2,
        "remote/watch",
        json!({ "computerId": "dv0000000000000002", "vehicleId": "v1" }),
    )
    .await;
    assert!(watched["result"].is_object(), "{watched}");

    // Its vehicles arrive as remote/changed, each with its age on this clock,
    // and a gap in the deltas asks the relay for a snapshot again.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut views = Vec::new();
    while seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("watch"))
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "{:?} {views:?}",
            seen.lock().unwrap()
        );
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(200), socket.next()).await
        {
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["method"] == "remote/changed" {
                views.push(frame["params"].clone());
            }
        }
    }
    // What was sent before the gap is still on its way.
    while let Ok(Some(Ok(Message::Text(text)))) =
        tokio::time::timeout(Duration::from_millis(500), socket.next()).await
    {
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["method"] == "remote/changed" {
            views.push(frame["params"].clone());
        }
    }
    let log = seen.lock().unwrap().clone();
    assert_eq!(log[0], "connect /v1/computers/dv0000000000000002/live");
    assert_eq!(log[1], "watch v1");
    assert_eq!(log[2], "watch v1", "the gap asked again");
    let last = views.last().expect("a view");
    assert_eq!(last["computerName"], "Laptop");
    assert_eq!(last["online"], true);
    assert_eq!(last["vehicles"][0]["name"], "Copter 1");
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let age = now.saturating_sub(last["vehicles"][0]["atMs"].as_u64().unwrap());
    assert!(age < 5_000, "{age}");
    // The deltas in order grew the trail.
    assert!(
        views.iter().any(|v| v["trails"][0]["points"]
            .as_array()
            .is_some_and(|p| p.len() >= 3)),
        "{views:?}"
    );

    // Watching nothing closes the relay connection.
    ws_call(&mut socket, 3, "remote/watch", json!({})).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.lock().unwrap().iter().any(|s| s == "closed") {
        assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// S84: a camera watched from away goes to the media node the coordinator
/// names, through the publisher, only while someone watches it. The
/// publisher here is a stand-in for `ffmpeg` that keeps its arguments and
/// what it was given.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_camera_watched_from_away_is_published_only_while_watched() {
    use std::os::unix::fs::PermissionsExt;

    let relay = FakeRelay::start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("credentials.json"),
        r#"{"account.token":"t0k3n"}"#,
    )
    .unwrap();
    let tools = tempfile::tempdir().unwrap();
    let ffmpeg = tools.path().join("ffmpeg");
    std::fs::write(
        &ffmpeg,
        "#!/bin/sh\ncase \"$*\" in\n  *-muxers*) echo ' E  whip            WHIP muxer'; exit 0;;\n  *-protocols*) exit 0;;\nesac\necho \"$*\" > \"$(dirname \"$0\")/args\"\nexec cat > \"$(dirname \"$0\")/stdin.h264\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let api = format!("http://127.0.0.1:{}", relay.port);
    let daemon = Daemon::run_with_env(
        dir,
        &["daemon"],
        &[],
        &[
            ("UXDUCK_API_URL", api.as_str()),
            ("UXDUCK_LIVE_FFMPEG", ffmpeg.to_str().unwrap()),
            ("UXDUCK_LIVE_LINGER_MS", "1000"),
        ],
    );
    let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
    let video_port = u16::try_from(listed["port"].as_u64().expect("listening")).unwrap();
    let camera = std::thread::spawn(move || send_pattern(video_port, 12.0));
    let mut source = String::new();
    wait_for("the camera", || {
        let listed = stdout_json(&daemon.cli(&["rpc", "call", "video/list"]));
        listed["sources"][0]["id"].as_str().is_some_and(|id| {
            source = id.to_owned();
            true
        })
    });
    relay.until("ping", 1).await;
    let stdin = tools.path().join("stdin.h264");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!stdin.exists(), "nobody watching: nothing is sent");

    // Someone away plays it.
    relay.send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": { "summary": true, "video": [source] } }));
    relay.until("media", 1).await;
    let asked: Value = {
        let seen = relay.seen.lock().unwrap();
        let line = seen.iter().find(|s| s.starts_with("media ")).unwrap();
        serde_json::from_str(&line["media ".len()..]).unwrap()
    };
    assert_eq!(asked["cameraId"], source.as_str());
    assert_eq!(asked["store"], true, "storing is on by default (C17)");
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::fs::metadata(&stdin).map_or(0, |m| m.len()) < 2_000 {
        assert!(
            Instant::now() < deadline,
            "no pictures reached the publisher"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let args = std::fs::read_to_string(tools.path().join("args")).unwrap();
    assert!(args.contains("-f h264 -i pipe:0"), "{args}");
    assert!(args.contains("-bf 0"), "{args}");
    assert!(
        args.contains(&format!(
            "-f whip -authorization the.publish.grant http://127.0.0.1:{}/live/s1/whip",
            relay.port
        )),
        "{args}"
    );
    let bytes = std::fs::read(&stdin).unwrap();
    assert_eq!(&bytes[..4], &[0, 0, 0, 1], "Annex B, from a keyframe");

    // Nobody plays it any more: after the linger, the publisher stops.
    relay
        .send(&json!({ "jsonrpc": "2.0", "method": "edge/demand", "params": { "summary": true } }));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let stopped = std::fs::metadata(&stdin).unwrap().len();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(std::fs::metadata(&stdin).unwrap().len(), stopped);
    let _ = camera.join();
}

/// Real ArduPilot Blimp SITL, built by the CI SITL action.
#[cfg(unix)]
#[test]
fn a_blimp_starts_as_an_airship_with_its_own_modes() {
    if native_simulator("blimp").is_none() {
        eprintln!("skipped: no cached blimp (scripts/sitl.sh blimp)");
        return;
    }
    let daemon = Daemon::start(&[]);
    let builds = stdout_json(&daemon.cli(&["sim", "builds"]));
    let blimp = builds["binaries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|build| build["program"] == "blimp")
        .unwrap();
    assert_eq!(blimp["vehicles"], json!(["airship"]));
    assert_eq!(blimp["defaultFrame"], "blimp");
    assert!(
        blimp["frames"]
            .as_array()
            .unwrap()
            .contains(&json!("blimp-motor"))
    );
    for frame in ["blimp", "blimp-motor"] {
        let started =
            stdout_json(&daemon.cli(&["sim", "start", "--vehicle", "airship", "--frame", frame]));
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let heard = vehicles(&daemon);
            if heard.iter().any(|vehicle| {
                vehicle["linkId"] == started["id"]
                    && vehicle["presence"] == "online"
                    && vehicle["vehicleType"] == "Airship"
                    && vehicle["mode"] == "MANUAL"
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "blimp not online in MANUAL: {heard:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(
            daemon
                .cli(&["sim", "stop", started["id"].as_str().unwrap()])
                .status
                .success()
        );
    }
    for options in [["--in-air", "50"], ["--autopilot", "px4"]] {
        assert!(
            !daemon
                .cli(&[
                    "sim",
                    "start",
                    "--vehicle",
                    "airship",
                    options[0],
                    options[1]
                ])
                .status
                .success()
        );
    }
}

/// A reported Blimp regression: a claimed Mode 2 pad must actually climb,
/// not merely arm and show the correct mode.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_blimp_climbs_with_a_claimed_controllers_left_stick() {
    if native_simulator("blimp").is_none() {
        eprintln!("skipped: no cached blimp (scripts/sitl.sh blimp)");
        return;
    }
    for frame in ["blimp", "blimp-motor"] {
        let pad_port = free_udp_port();
        let daemon = Daemon::start(&["--no-gamepads", "--fake-controller", &pad_port.to_string()]);
        let started =
            stdout_json(&daemon.cli(&["sim", "start", "--vehicle", "airship", "--frame", frame]));
        // Real firmware starts more slowly while the full integration suite builds plugins.
        let deadline = Instant::now() + Duration::from_secs(90);
        while !vehicles(&daemon).iter().any(|v| v["presence"] == "online") {
            assert!(Instant::now() < deadline, "timed out waiting for {frame}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let vehicle = vehicles(&daemon)[0]["id"].as_str().unwrap().to_owned();
        let pad = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let move_pad =
            |up: f64| {
                pad.send_to(
                json!({ "id": "blimp-pad", "name": "Blimp pad", "axes": { "left-stick-y": up } })
                    .to_string().as_bytes(),
                ("127.0.0.1", pad_port),
            ).unwrap();
            };
        wait_for("the centred pad", || {
            move_pad(0.0);
            rpc(&daemon, "controller/inputs", &json!({}))["controllers"]
                .as_array()
                .is_some_and(|c| !c.is_empty())
        });
        let cookie = paired_cookie(&daemon);
        let mut ui = daemon.websocket(Some(&cookie)).await.unwrap();
        ws_call(&mut ui, 1, "initialize", operator_ui()).await;
        let claimed = ws_call(
            &mut ui,
            2,
            "controller/claim",
            json!({
                "id": "blimp-pad", "mode": "pinned", "vehicleId": vehicle,
            }),
        )
        .await;
        assert!(claimed.get("result").is_some(), "{claimed}");
        let height = || {
            let telemetry = rpc(&daemon, "telemetry/get", &json!({ "id": vehicle }));
            telemetry["positions"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| p["altitudes"].as_array().cloned().unwrap_or_default())
                .find(|a| a["frame"] == "above-home")
                .and_then(|a| a["meters"].as_f64())
                .unwrap_or(0.0)
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        while vehicles(&daemon)[0]["armed"] != true {
            move_pad(0.0);
            let _ = rpc(
                &daemon,
                "command/send",
                &json!({
                    "vehicleId": vehicle, "command": { "type": "arm" },
                }),
            );
            assert!(Instant::now() < deadline, "Blimp failed to arm");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        for mode in ["MANUAL", "LOITER"] {
            move_pad(0.0);
            let receipt = rpc(
                &daemon,
                "command/send",
                &json!({
                    "vehicleId": vehicle, "command": { "type": "set-mode", "mode": mode },
                }),
            );
            assert_eq!(receipt["status"], "accepted", "{receipt}");
            let before = height();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                move_pad(1.0);
                let current = height();
                if current > before + 0.4 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{frame} {mode}: left stick did not climb: {before} -> {current}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        move_pad(0.0);
        assert!(
            daemon
                .cli(&["sim", "stop", started["id"].as_str().unwrap()])
                .status
                .success()
        );
    }
}

#[test]
fn missing_tray_fails_before_the_daemon_is_published() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("missing-tray");
    let output = Command::new(BIN)
        .args(["daemon", "--port", "0", "--data-dir"])
        .arg(dir.path())
        .env("UXDUCK_TRAY", &absent)
        .env("DISPLAY", ":0")
        .env_remove("SSH_CONNECTION")
        .env_remove("UXDUCK_NO_TRAY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("menu bar"),
        "{output:?}"
    );
    assert!(!dir.path().join("daemon.json").exists());
    assert!(!dir.path().join("uxduck.sock").exists());
}

#[cfg(unix)]
#[test]
fn an_unlaunchable_tray_fails_before_the_daemon_is_published() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("tray");
    std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = Command::new(BIN)
        .args(["daemon", "--port", "0", "--data-dir"])
        .arg(dir.path())
        .env("UXDUCK_TRAY", &helper)
        .env("DISPLAY", ":0")
        .env_remove("SSH_CONNECTION")
        .env_remove("UXDUCK_NO_TRAY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("menu bar"));
    assert!(!dir.path().join("daemon.json").exists());
}

#[test]
fn a_disabled_tray_does_not_require_the_helper_until_enabled() {
    let daemon = Daemon::start(&[]);
    let snapshot = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    stdout_json(&daemon.cli(&["rpc", "call", "settings/update", "--json", &json!({"expectedVersion": snapshot["snapshot"]["settings"]["version"], "showInMenuBar": false}).to_string()]));
    let dir = daemon.dir.as_ref().unwrap().path().to_path_buf();
    let mut daemon = daemon;
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    let _ = std::fs::remove_file(dir.join("daemon.json"));
    let restarted = Daemon::run_with_env(
        daemon.dir.take().unwrap(),
        &["daemon"],
        &[],
        &[("UXDUCK_TRAY", dir.join("missing-tray").to_str().unwrap())],
    );
    assert!(restarted.dir().join("daemon.json").exists());
}

#[cfg(unix)]
#[test]
fn repeated_tray_failures_are_visible_and_can_be_retried() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    let helper = work.path().join("tray");
    std::fs::write(
        &helper,
        "#!/bin/sh\nif [ \"$1\" = \"--check\" ]; then exit 0; fi\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = Daemon::run_with_env(
        tempfile::tempdir().unwrap(),
        &["daemon"],
        &[],
        &[("UXDUCK_TRAY", helper.to_str().unwrap()), ("DISPLAY", ":0")],
    );
    wait_for("a visible tray failure", || {
        let menu = stdout_json(&daemon.cli(&["rpc", "call", "tray/menu"]));
        menu["status"] == "UXDuck's menu bar needs attention" && menu["tone"] == "broken"
    });
    let snapshot = stdout_json(&daemon.cli(&[
        "rpc",
        "call",
        "state/subscribe",
        "--json",
        r#"{"entities":["settings"]}"#,
    ]));
    stdout_json(&daemon.cli(&["rpc", "call", "settings/update", "--json", &json!({"expectedVersion": snapshot["snapshot"]["settings"]["version"], "showInMenuBar": false}).to_string()]));
    wait_for("the failure cleared when intentionally off", || {
        let menu = stdout_json(&daemon.cli(&["rpc", "call", "tray/menu"]));
        menu["tone"] == "plain"
    });
}

#[cfg(unix)]
#[test]
fn invalid_tray_artwork_fails_preflight_before_the_engine_starts() {
    use std::os::unix::fs::PermissionsExt;
    let work = tempfile::tempdir().unwrap();
    let helper = work.path().join("tray");
    std::fs::write(
        &helper,
        "#!/bin/sh\necho 'invalid required artwork' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = tempfile::tempdir().unwrap();
    let output = Command::new(BIN)
        .args(["daemon", "--port", "0", "--data-dir"])
        .arg(data.path())
        .env("UXDUCK_TRAY", &helper)
        .env("DISPLAY", ":0")
        .env_remove("SSH_CONNECTION")
        .env_remove("UXDUCK_NO_TRAY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid required artwork"));
    assert!(!data.path().join("daemon.json").exists());
    assert!(!data.path().join("uxduck.sock").exists());
}

#[test]
fn repeated_packets_from_the_original_vehicle_do_not_disable_forwarding() {
    use uxduck_mavlink::sim::heartbeat_frame;

    let daemon = Daemon::start(&[]);
    let listen = free_udp_port();
    let radio = add_link(
        &daemon,
        &json!({
            "name": "Repeated telemetry", "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": listen }
        }),
    );
    wait_up(&daemon, radio["id"].as_str().unwrap());
    let (receiver, destination) = program();
    let forward = add_link(
        &daemon,
        &json!({
            "kind": { "type": "udp-forward", "host": "127.0.0.1", "port": destination }
        }),
    );
    let forward_id = forward["id"].as_str().unwrap();
    wait_up(&daemon, forward_id);
    let (vehicle, _) = program();
    // The same bytes and sequence can recur when SITL wraps its sequence
    // counter. Each repeat must still arrive, without an echo from the receiver.
    let frame = heartbeat_frame(1, 1, 144);
    let mut buffer = [0u8; 2048];
    for _ in 0..12 {
        vehicle.send_to(&frame, ("127.0.0.1", listen)).unwrap();
        let (len, _) = receiver
            .recv_from(&mut buffer)
            .expect("valid source repetition must reach the destination");
        assert_eq!(&buffer[..len], frame.as_slice());
        assert_eq!(link_status(&daemon, forward_id)["status"], "up");
    }
}

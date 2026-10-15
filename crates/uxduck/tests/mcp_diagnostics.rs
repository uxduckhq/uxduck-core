//! Read-only diagnostic tools through the actual MCP stdio adapter.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::io::{BufRead, BufReader, Write};
use std::net::UdpSocket;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_uxduck");

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Mcp {
    _process: Process,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(dir: &Path) -> Self {
        let mut process = Command::new(BIN)
            .args(["mcp", "--data-dir"])
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = process.stdin.take().unwrap();
        let output = BufReader::new(process.stdout.take().unwrap());
        Self {
            _process: Process(process),
            input,
            output,
            next_id: 0,
        }
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let request =
            &json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params });
        writeln!(self.input, "{request}").unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], self.next_id);
        assert!(response.get("error").is_none(), "MCP {method}: {response}");
        response["result"].clone()
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.call(
            "tools/call",
            &json!({ "name": name, "arguments": arguments }),
        )
    }
}

fn rpc(dir: &Path, method: &str, params: &Value) -> Value {
    let output = Command::new(BIN)
        .args([
            "rpc",
            "call",
            method,
            "--json",
            &params.to_string(),
            "--data-dir",
        ])
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn until<T>(label: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(result) = check() {
            return result;
        }
        assert!(Instant::now() < deadline, "timed out: {label}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[allow(clippy::too_many_lines, reason = "one diagnostic scenario, end to end")]
fn mcp_reads_recorded_messages_and_preserves_api_failures() {
    let dir = tempfile::tempdir().unwrap();
    let _daemon = Process(
        Command::new(BIN)
            .args(["daemon", "--port", "0", "--data-dir"])
            .arg(dir.path())
            .env("UXDUCK_POSTHOG_KEY", "none")
            .env("UXDUCK_API_URL", "none")
            .env("UXDUCK_VIDEO_PORT", "0")
            .env("UXDUCK_TRAY", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("daemon started", || {
        dir.path().join("daemon.json").exists().then_some(())
    });
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    rpc(
        dir.path(),
        "link/add",
        &json!({ "kind": { "type": "udp-listen", "host": "127.0.0.1", "port": port } }),
    );
    let _fake = Process(
        Command::new(BIN)
            .args(["dev", "fake-vehicle", "--to", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let vehicle = until("vehicle heard", || {
        rpc(dir.path(), "vehicle/list", &json!({}))["vehicles"][0]["id"]
            .as_str()
            .map(str::to_owned)
    });

    let mut mcp = Mcp::start(dir.path());
    let hello = mcp.call(
        "initialize",
        &json!({ "clientInfo": { "name": "diagnostic regression test" } }),
    );
    assert_eq!(hello["serverInfo"]["name"], "uxduck");
    let catalogue = mcp.call("tools/list", &json!({}));
    for name in [
        "daemon_about",
        "mavlink_inspect",
        "mavlink_history",
        "link_list",
        "telemetry_window",
    ] {
        assert!(
            catalogue["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
    let about = mcp.tool("daemon_about", &json!({}));
    assert_eq!(about["isError"], false, "{about}");
    let links = mcp.tool("link_list", &json!({}));
    assert_eq!(links["isError"], false, "{links}");
    assert_eq!(
        links["structuredContent"]["links"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let inspected = mcp.tool("mavlink_inspect", &json!({ "vehicleId": vehicle }));
    assert_eq!(inspected["isError"], false, "{inspected}");
    assert!(
        inspected["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["name"] == "HEARTBEAT")
    );
    let history = until("recorded heartbeat", || {
        let history = mcp.tool(
            "mavlink_history",
            &json!({ "vehicleId": vehicle, "direction": "received", "id": 0, "limit": 1 }),
        );
        assert_eq!(history["isError"], false, "{history}");
        (!history["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .is_empty())
        .then_some(history)
    });
    assert_eq!(
        history["structuredContent"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(history["structuredContent"]["fromLog"], true);
    assert!(
        history["structuredContent"]["messages"][0]["atUs"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(history["structuredContent"]["messages"][0]["fields"].is_object());

    let key = until("numeric telemetry", || {
        let live = mcp.tool("telemetry_get", &json!({ "id": vehicle }));
        assert_eq!(live["isError"], false, "{live}");
        live["structuredContent"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["value"].is_number())
            .and_then(|item| item["key"].as_str())
            .map(str::to_owned)
    });
    let window = until("telemetry captured before polling", || {
        let window = mcp.tool(
            "telemetry_window",
            &json!({
                "vehicleId": vehicle, "keys": [key], "durationMs": 120_000, "maxSamples": 1
            }),
        );
        assert_eq!(window["isError"], false, "{window}");
        (window["structuredContent"]["items"][0]["coverage"]["count"]
            .as_u64()
            .unwrap_or(0)
            > 1)
        .then_some(window)
    });
    let series = &window["structuredContent"]["items"][0];
    assert_eq!(series["samples"].as_array().unwrap().len(), 1);
    assert_eq!(series["coverage"]["downsampled"], true);
    assert!(series["numeric"]["min"].is_number());
    assert!(series["numeric"]["max"].is_number());

    let first_read = mcp.tool("vehicle_get", &json!({ "id": vehicle }));
    assert_eq!(first_read["isError"], false, "{first_read}");
    let first_heartbeat = first_read["structuredContent"]["lastHeartbeatUs"]
        .as_u64()
        .unwrap();
    let refreshed = until("identical heartbeats refresh direct reads", || {
        let reading = mcp.tool("vehicle_get", &json!({ "id": vehicle }));
        assert_eq!(reading["isError"], false, "{reading}");
        (reading["structuredContent"]["lastHeartbeatUs"]
            .as_u64()
            .unwrap()
            >= first_heartbeat + 2_000_000)
            .then_some(reading)
    });
    assert!(
        refreshed["structuredContent"]["heartbeatAgeMs"]
            .as_u64()
            .unwrap()
            < 2000,
        "{refreshed}"
    );
    for field in ["armed", "mode", "presence"] {
        assert_eq!(
            refreshed["structuredContent"][field],
            first_read["structuredContent"][field]
        );
    }

    let missing = mcp.tool("mavlink_inspect", &json!({ "vehicleId": "missing" }));
    assert_eq!(missing["isError"], true, "{missing}");
    let error = &missing["structuredContent"]["error"];
    assert!(error["code"].is_number(), "{missing}");
    assert!(error["message"].is_string(), "{missing}");
    assert!(error["data"].is_object(), "{missing}");
    assert_eq!(
        serde_json::from_str::<Value>(missing["content"][0]["text"].as_str().unwrap()).unwrap(),
        *error
    );

    let subscription = mcp.tool(
        "vehicle_messages",
        &json!({ "vehicleId": vehicle, "subscribe": true }),
    );
    assert_eq!(subscription["isError"], true, "{subscription}");
    assert_eq!(
        subscription["structuredContent"]["error"]["data"]["field"],
        "subscribe"
    );
    let snapshot = mcp.tool("vehicle_messages", &json!({ "vehicleId": vehicle }));
    assert_eq!(snapshot["isError"], false, "{snapshot}");
    assert!(
        rpc(dir.path(), "approval/list", &json!({}))["approvals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

fn diagnostic_daemon(dir: &Path) -> Process {
    let _ = std::fs::remove_file(dir.join("daemon.json"));
    let daemon = Process(
        Command::new(BIN)
            .args(["daemon", "--port", "0", "--data-dir"])
            .arg(dir)
            .env("UXDUCK_POSTHOG_KEY", "none")
            .env("UXDUCK_API_URL", "none")
            .env("UXDUCK_VIDEO_PORT", "0")
            .env("UXDUCK_TRAY", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until("diagnostic daemon started", || {
        dir.join("daemon.json").exists().then_some(())
    });
    daemon
}

fn wait_link(dir: &Path, id: &str, status: &str) {
    until("diagnostic connection status", || {
        rpc(dir, "link/list", &json!({}))["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["id"] == id && link["status"] == status)
            .then_some(())
    });
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the diagnostic incident through MCP, including restart"
)]
fn mcp_explains_packet_provenance_and_keeps_loop_evidence_after_restart() {
    use uxduck_mavlink::sim::heartbeat_frame;
    let dir = tempfile::tempdir().unwrap();
    let daemon = diagnostic_daemon(dir.path());
    let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let destination = receiver.local_addr().unwrap().port();
    let reserve = UdpSocket::bind("127.0.0.1:0").unwrap();
    let listen = reserve.local_addr().unwrap().port();
    drop(reserve);
    let source = rpc(
        dir.path(),
        "link/add",
        &json!({ "name":"Telemetry wire", "kind": { "type":"udp-listen", "host":"127.0.0.1", "port":listen } }),
    );
    let source_id = source["id"].as_str().unwrap();
    wait_link(dir.path(), source_id, "up");
    let forward = rpc(
        dir.path(),
        "link/add",
        &json!({ "name":"Other ground station", "kind": { "type":"udp-forward", "host":"127.0.0.1", "port":destination } }),
    );
    let forward_id = forward["id"].as_str().unwrap();
    wait_link(dir.path(), forward_id, "up");
    let vehicle = UdpSocket::bind("127.0.0.1:0").unwrap();
    let frame = heartbeat_frame(1, 1, 144);
    let raw_hex = frame.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
        out
    });
    let mut buffer = [0u8; 2048];
    for _ in 0..8 {
        vehicle.send_to(&frame, ("127.0.0.1", listen)).unwrap();
        let (len, _) = receiver.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], frame.as_slice());
    }
    let mut mcp = Mcp::start(dir.path());
    mcp.call(
        "initialize",
        &json!({"clientInfo":{"name":"packet diagnostic regression"}}),
    );
    let catalogue = mcp.call("tools/list", &json!({}));
    for name in [
        "router_events",
        "link_diagnose",
        "socket_inspect",
        "daemon_logs",
    ] {
        assert!(
            catalogue["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name)
        );
    }
    let history = mcp.tool(
        "mavlink_history",
        &json!({"linkId":source_id,"id":0,"limit":1,"includeRaw":true}),
    );
    assert_eq!(history["isError"], false, "{history}");
    let packet = &history["structuredContent"]["messages"][0];
    assert_eq!(packet["sequence"], 144);
    assert_eq!(packet["rawHex"], raw_hex);
    assert_eq!(packet["peer"], vehicle.local_addr().unwrap().to_string());
    assert_eq!(
        packet["fingerprint"],
        uxduck_mavlink::router::fingerprint(&frame)
    );
    assert!(history["structuredContent"]["coverage"].is_object());
    let cursor = history["structuredContent"]["older"].as_str().unwrap();
    let next = mcp.tool(
        "mavlink_history",
        &json!({"linkId":source_id,"id":0,"limit":1,"includeRaw":true,"before":cursor}),
    );
    assert_eq!(next["isError"], false, "{next}");
    assert_eq!(
        history["structuredContent"]["coverage"]["requestedToUs"],
        next["structuredContent"]["coverage"]["requestedToUs"]
    );
    let wrong = mcp.tool(
        "mavlink_history",
        &json!({"linkId":forward_id,"id":0,"before":cursor,"includeRaw":true}),
    );
    assert_eq!(wrong["isError"], true);
    let outbound = mcp.tool(
        "mavlink_history",
        &json!({"linkId":forward_id,"direction":"sent","id":0,"includeRaw":true}),
    );
    assert_eq!(outbound["isError"], false, "{outbound}");
    assert_eq!(
        outbound["structuredContent"]["messages"][0]["rawHex"],
        raw_hex
    );
    assert_eq!(
        outbound["structuredContent"]["messages"][0]["peer"],
        format!("127.0.0.1:{destination}")
    );
    let sockets = mcp.tool(
        "socket_inspect",
        &json!({"protocol":"udp","port":destination}),
    );
    assert_eq!(sockets["isError"], false, "{sockets}");
    assert!(
        sockets["structuredContent"]["owners"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["pid"] == std::process::id())
    );
    #[cfg(target_os = "macos")]
    assert!(
        sockets["structuredContent"]["owners"]
            .as_array()
            .unwrap()
            .iter()
            .all(|owner| {
                owner["localAddress"]
                    .as_str()
                    .is_some_and(|address| address.ends_with(&format!(":{destination}")))
            }),
        "programs sending to a port are not owners of its local socket"
    );
    let diag = mcp.tool("link_diagnose", &json!({"query":"Telemetry wire"}));
    assert_eq!(diag["isError"], false, "{diag}");
    assert_eq!(diag["structuredContent"]["link"]["id"], source_id);
    assert_eq!(diag["structuredContent"]["forwards"][0]["id"], forward_id);
    let events = mcp.tool("router_events", &json!({"linkId":forward_id,"limit":1}));
    assert_eq!(events["isError"], false, "{events}");
    assert_eq!(
        events["structuredContent"]["events"][0]["decision"],
        "forwarded"
    );
    assert!(
        events["structuredContent"]["events"][0]["packet"]
            .get("rawHex")
            .is_none()
    );
    // A different peer relaying the packets is a real loop.
    let relay = UdpSocket::bind("127.0.0.1:0").unwrap();
    for seq in 0..5 {
        let fresh = heartbeat_frame(1, 1, seq);
        vehicle.send_to(&fresh, ("127.0.0.1", listen)).unwrap();
        let (len, _) = receiver.recv_from(&mut buffer).unwrap();
        relay
            .send_to(&buffer[..len], ("127.0.0.1", listen))
            .unwrap();
    }
    wait_link(dir.path(), forward_id, "disabled-by-loop-detection");
    let incident = mcp.tool(
        "router_events",
        &json!({"linkId":forward_id,"decision":"dropped-loop","includeRaw":true}),
    );
    assert_eq!(incident["isError"], false, "{incident}");
    let failed = incident["structuredContent"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["disabled"] == true)
        .unwrap()
        .clone();
    assert_eq!(failed["threshold"], 5);
    assert_eq!(failed["windowUs"], 2_000_000);
    assert_eq!(
        failed["packet"]["endpoint"]["peer"],
        relay.local_addr().unwrap().to_string()
    );
    assert_eq!(
        failed["matchedPacket"]["endpoint"]["peer"],
        vehicle.local_addr().unwrap().to_string()
    );
    assert_eq!(
        failed["packet"]["fingerprint"],
        failed["matchedPacket"]["fingerprint"]
    );
    assert_eq!(
        failed["packet"]["rawHex"],
        failed["matchedPacket"]["rawHex"]
    );
    assert!(failed["packet"]["rawHex"].is_string());
    assert!(failed["matchedPacket"]["rawHex"].is_string());
    let logs = mcp.tool(
        "daemon_logs",
        &json!({"level":"warn","query":"forward disabled"}),
    );
    assert_eq!(logs["isError"], false, "{logs}");
    assert!(
        !logs["structuredContent"]["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let bad = mcp.tool("router_events", &json!({"limit":0}));
    assert_eq!(bad["isError"], true);
    drop(mcp);
    drop(daemon);
    let _restarted = diagnostic_daemon(dir.path());
    let mut mcp = Mcp::start(dir.path());
    mcp.call(
        "initialize",
        &json!({"clientInfo":{"name":"restored diagnostic regression"}}),
    );
    let diag = mcp.tool("link_diagnose", &json!({"query":forward_id}));
    assert_eq!(diag["isError"], false, "{diag}");
    assert_eq!(
        diag["structuredContent"]["lastIncident"]["id"],
        failed["id"]
    );
    assert_eq!(
        diag["structuredContent"]["lastIncident"]["matchedPacket"]["endpoint"]["peer"],
        vehicle.local_addr().unwrap().to_string()
    );
    let historical = mcp.tool("mavlink_history",&json!({"linkId":source_id,"fromUs":packet["atUs"].as_u64().unwrap()-1,"toUs":packet["atUs"].as_u64().unwrap(),"id":0,"includeRaw":true}));
    assert_eq!(historical["isError"], false, "{historical}");
    assert!(
        historical["structuredContent"]["messages"][0]
            .get("peer")
            .is_none()
    );
    assert_eq!(
        historical["structuredContent"]["messages"][0]["rawHex"],
        raw_hex
    );
}

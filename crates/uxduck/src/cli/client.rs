//! A JSON-RPC client over the local socket, for the CLI and the tests.

use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("no daemon is running for {0}; start one with `uxduck daemon`")]
    NoDaemon(String),
    #[error("{0}")]
    Usage(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection lost: {0}")]
    Io(#[from] std::io::Error),
    /// The daemon answered with a JSON-RPC error object.
    #[error("{0}")]
    Api(Value),
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

pub struct LocalClient {
    lines: Lines<BufReader<tokio::io::ReadHalf<Stream>>>,
    writer: tokio::io::WriteHalf<Stream>,
    next_id: i64,
    pending_methods: std::collections::HashMap<i64, (String, bool)>,
    mission_reads: uxduck_contracts::dto::mission_read::MissionReadSession,
    /// Notifications that arrived while waiting for a response.
    queued: std::collections::VecDeque<Value>,
}

impl std::fmt::Debug for LocalClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl LocalClient {
    /// # Errors
    /// When no daemon listens for `data_dir`.
    pub async fn connect(data_dir: &Path) -> Result<Self, CliError> {
        #[cfg(unix)]
        let stream = {
            let path = crate::paths::socket_path(data_dir);
            tokio::net::UnixStream::connect(&path)
                .await
                .map_err(|_| CliError::NoDaemon(data_dir.display().to_string()))?
        };
        #[cfg(windows)]
        let stream = open_named_pipe(
            &crate::paths::pipe_name(data_dir),
            std::time::Duration::from_secs(5),
        )
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                CliError::Io(error)
            } else {
                CliError::NoDaemon(data_dir.display().to_string())
            }
        })?;
        let (reader, writer) = tokio::io::split(stream);
        Ok(Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 0,
            pending_methods: std::collections::HashMap::new(),
            mission_reads: uxduck_contracts::dto::mission_read::MissionReadSession::default(),
            queued: std::collections::VecDeque::new(),
        })
    }

    /// `initialize` as the CLI: an operator.
    ///
    /// # Errors
    /// When the daemon refuses the handshake.
    pub async fn initialize(&mut self) -> Result<Value, CliError> {
        self.call(
            "initialize",
            json!({
                "clientInfo": { "name": "uxduck-cli", "version": env!("CARGO_PKG_VERSION"), "kind": "operator-cli" },
                "capabilities": { "protocolVersion": uxduck_contracts::version::PROTOCOL_VERSION, "missionReadFloatBits": true, "replayStepMarks": true }
            }),
        )
        .await
    }

    /// `initialize` as another kind of client, such as `agent` for
    /// `uxduck mcp`, under the name it gives.
    ///
    /// # Errors
    /// When the daemon refuses the handshake.
    pub async fn initialize_as(
        &mut self,
        name: &str,
        kind: &str,
        approvals: bool,
    ) -> Result<Value, CliError> {
        self.call(
            "initialize",
            json!({
                "clientInfo": { "name": name, "version": env!("CARGO_PKG_VERSION"), "kind": kind },
                "capabilities": {
                    "protocolVersion": uxduck_contracts::version::PROTOCOL_VERSION,
                    "missionReadFloatBits": true,
                    "approvals": approvals,
                    "replayStepMarks": true
                }
            }),
        )
        .await
    }

    /// Sends a request and waits for its response, queueing notifications.
    ///
    /// # Errors
    /// The daemon's error object as `CliError::Api`, or a transport failure.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, CliError> {
        let id = self.send_request(method, params).await?;
        loop {
            let Some(message) = self.read().await? else {
                return Err(CliError::Protocol(
                    "the daemon closed the connection".into(),
                ));
            };
            if message.get("id") == Some(&json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(CliError::Api(error.clone()));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if message.get("method").is_some() {
                self.queued.push_back(message);
            }
        }
    }

    pub(crate) async fn send_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<i64, CliError> {
        self.mission_reads
            .before_request(method, &params)
            .map_err(|error| CliError::Api(json!(error)))?;
        let mission_bearing =
            uxduck_contracts::dto::mission_read::mission_read_request(method, &params);
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        self.pending_methods
            .insert(id, (method.to_owned(), mission_bearing));
        Ok(id)
    }

    pub(crate) async fn cancel_request(&mut self, id: i64) -> Result<(), CliError> {
        self.pending_methods.remove(&id);
        self.write(json!({ "jsonrpc": "2.0", "method": "cancel", "params": { "id": id } }))
            .await
    }

    async fn write(&mut self, frame: Value) -> Result<(), CliError> {
        let mut line = frame.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(())
    }

    /// The next notification, or `None` when the daemon closes the connection.
    ///
    /// # Errors
    /// On a transport failure or a frame that is not JSON.
    pub async fn next_notification(&mut self) -> Result<Option<Value>, CliError> {
        if let Some(message) = self.queued.pop_front() {
            self.check_notification(&message)?;
            return Ok(Some(message));
        }
        self.read().await
    }

    fn check_notification(&mut self, frame: &Value) -> Result<(), CliError> {
        if frame["method"] == "state/delta" {
            let mission_bearing = uxduck_contracts::dto::mission_read::mission_read_request(
                "state/delta",
                &frame["params"],
            );
            self.mission_reads
                .accept_result("state/delta", mission_bearing, &frame["params"])
                .map_err(|error| CliError::Api(json!(error)))?;
        }
        Ok(())
    }

    pub(crate) async fn read(&mut self) -> Result<Option<Value>, CliError> {
        let Some(line) = self.lines.next_line().await? else {
            return Ok(None);
        };
        let mut frame: Value =
            serde_json::from_str(&line).map_err(|e| CliError::Protocol(e.to_string()))?;
        self.check_notification(&frame)?;
        if let Some(id) = frame.get("id").and_then(Value::as_i64)
            && let Some((method, mission_bearing)) = self.pending_methods.remove(&id)
            && frame.get("error").is_none()
            && let Some(result) = frame.get("result")
            && let Err(error) = self
                .mission_reads
                .accept_result(&method, mission_bearing, result)
            && let Some(object) = frame.as_object_mut()
        {
            object.remove("result");
            object.insert("error".into(), json!(error));
        }
        Ok(Some(frame))
    }
}

/// A pipe can exist with every listening instance temporarily occupied.
/// Retrying `ERROR_PIPE_BUSY` must not turn a live daemon into a second launch.
#[cfg(windows)]
async fn open_named_pipe(name: &str, timeout: std::time::Duration) -> std::io::Result<Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::{Instant, sleep};
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    let deadline = Instant::now() + timeout;
    loop {
        match ClientOptions::new().open(name) {
            Ok(stream) => return Ok(stream),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed()) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "UXDuck's local pipe remained busy; retry the command",
                    ));
                }
                sleep(remaining.min(std::time::Duration::from_millis(50))).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, reason = "paired in-memory protocol fixture")]
impl LocalClient {
    pub(crate) fn test_pair() -> (Self, Stream) {
        let (stream, peer) = Stream::pair().unwrap();
        let (reader, writer) = tokio::io::split(stream);
        (
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
                next_id: 0,
                pending_methods: std::collections::HashMap::default(),
                mission_reads: uxduck_contracts::dto::mission_read::MissionReadSession::default(),
                queued: std::collections::VecDeque::default(),
            },
            peer,
        )
    }
}

#[cfg(all(test, unix))]
mod mission_read_tests {
    #![allow(clippy::unwrap_used, reason = "paired in-memory protocol fixtures")]
    use super::*;

    fn golden(method: &str, id: i64) -> Value {
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../uxduck-contracts/tests/goldens/mission-read.json"
        )))
        .unwrap();
        let mut frame: Value =
            serde_json::from_str(fixture["frames"][method].as_str().unwrap()).unwrap();
        frame["id"] = json!(id);
        frame
    }

    async fn response(
        client: &mut LocalClient,
        peer: &mut BufReader<Stream>,
        frame: Value,
    ) -> Value {
        let line = format!("{frame}\n");
        let (written, received) =
            tokio::join!(peer.get_mut().write_all(line.as_bytes()), client.read());
        written.unwrap();
        received.unwrap().unwrap()
    }

    #[tokio::test]
    async fn mission_read_handshakes_advertise_exact_support_for_cli_and_mcp() {
        let (mut client, peer) = LocalClient::test_pair();
        let mut peer = BufReader::new(peer);
        for agent in [false, true] {
            let server = async {
                let mut line = String::new();
                peer.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "initialize");
                assert_eq!(
                    request["params"]["capabilities"]["missionReadFloatBits"],
                    true
                );
                assert_eq!(
                    request["params"]["clientInfo"]["kind"],
                    if agent { "agent" } else { "operator-cli" }
                );
                peer.get_mut()
                    .write_all(
                        format!(
                            "{}\n",
                            json!({ "jsonrpc": "2.0", "id": request["id"], "result": {} })
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            };
            let initialize = async {
                if agent {
                    client.initialize_as("MCP test", "agent", false).await
                } else {
                    client.initialize().await
                }
            };
            let (result, ()) = tokio::join!(initialize, server);
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn mission_read_queued_notifications_are_rechecked_after_quarantine() {
        let (mut client, peer) = LocalClient::test_pair();
        let mut peer = BufReader::new(peer);
        let mut mission_delta = golden("state/delta", 0);
        mission_delta.as_object_mut().unwrap().remove("id");
        let nonmission = json!({ "jsonrpc": "2.0", "method": "state/delta", "params": {
            "seq": 43, "changes": [{ "entity": "vehicle", "op": "remove", "id": "gone" }]
        } });
        let server = async {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "about");
            for frame in [
                mission_delta.clone(),
                nonmission.clone(),
                mission_delta.clone(),
                json!({ "jsonrpc": "2.0", "id": request["id"], "result": {} }),
            ] {
                peer.get_mut()
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .unwrap();
            }
        };
        let (about, ()) = tokio::join!(client.call("about", json!({})), server);
        about.unwrap();
        assert_eq!(client.queued.len(), 3);
        let id = client
            .send_request("mission/refresh", json!({ "id": "v" }))
            .await
            .unwrap();
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let mut invalid = golden("mission/refresh", id);
        invalid["result"]["items"][0]["param4"] = Value::Null;
        assert_eq!(
            response(&mut client, &mut peer, invalid).await["error"]["data"]["kind"],
            "update-required"
        );
        let before = client.next_id;
        assert!(
            matches!(client.next_notification().await, Err(CliError::Api(error)) if error["data"]["kind"] == "update-required")
        );
        assert_eq!(client.next_notification().await.unwrap(), Some(nonmission));
        assert!(
            matches!(client.next_notification().await, Err(CliError::Api(error)) if error["data"]["kind"] == "update-required")
        );
        assert!(client.queued.is_empty());
        assert_eq!(client.next_id, before);
        assert_eq!(
            peer.get_ref().try_read(&mut [0_u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn mission_read_api_errors_keep_their_kinds_without_quarantining_the_session() {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        let (mut client, peer) = LocalClient::test_pair();
        let mut peer = BufReader::new(peer);
        for (method, kind) in [
            ("mission/history", ErrorKind::InvalidParams),
            ("mission/refresh", ErrorKind::CapabilityDenied),
            ("vehicle/list", ErrorKind::InvalidValue),
        ] {
            let id = client
                .send_request(method, json!({ "id": "v" }))
                .await
                .unwrap();
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            let expected = serde_json::to_value(ApiError::new(kind, "fixture API error")).unwrap();
            let received = response(
                &mut client,
                &mut peer,
                json!({ "jsonrpc": "2.0", "id": id, "error": expected }),
            )
            .await;
            assert_eq!(received["error"], expected);
            assert!(
                client
                    .mission_reads
                    .before_request("mission/history", &json!({}))
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines, reason = "one paired-stream protocol scenario")]
    async fn mission_read_pair_preserves_bits_quarantines_pending_results_and_keeps_nonmission_calls()
     {
        let (mut client, peer) = LocalClient::test_pair();
        let mut peer = BufReader::new(peer);
        let id = client
            .send_request("mission/refresh", json!({ "id": "v" }))
            .await
            .unwrap();
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        let good = response(&mut client, &mut peer, golden("mission/refresh", id)).await;
        for part in ["items", "fence", "rally"] {
            for field in [
                "param1",
                "param2",
                "param3",
                "param4",
                "latitudeDeg",
                "longitudeDeg",
                "altitudeM",
            ] {
                assert_eq!(
                    good["result"][part][1][field].as_f64().unwrap().to_bits(),
                    (-0.0_f64).to_bits()
                );
                assert_eq!(
                    good["result"][part][3][field]["nonFiniteF64"],
                    "7ff8000000000001"
                );
                assert_eq!(
                    good["result"][part][4][field]["nonFiniteF64"],
                    "fff8000000000042"
                );
            }
        }
        let mut pending = Vec::new();
        for method in [
            "mission/history",
            "mission/upload",
            "mission/restore",
            "plan/upload",
            "state/subscribe",
        ] {
            let params = if method == "state/subscribe" {
                json!({ "entities": ["mission"] })
            } else {
                json!({})
            };
            let id = client.send_request(method, params).await.unwrap();
            line.clear();
            peer.read_line(&mut line).await.unwrap();
            pending.push((method, id));
        }
        let id = client
            .send_request("mission/refresh", json!({ "id": "v" }))
            .await
            .unwrap();
        line.clear();
        peer.read_line(&mut line).await.unwrap();
        let mut broken = golden("mission/refresh", id);
        broken["result"]["items"][0]["param4"] = Value::Null;
        let refused = response(&mut client, &mut peer, broken).await;
        assert_eq!(refused["error"]["data"]["kind"], "update-required");
        assert!(refused.get("result").is_none());
        for (method, id) in pending {
            let refused = response(&mut client, &mut peer, golden(method, id)).await;
            assert_eq!(
                refused["error"]["data"]["kind"], "update-required",
                "{method}: {refused}"
            );
            assert!(refused.get("result").is_none());
        }
        let before = client.next_id;
        for method in [
            "mission/refresh",
            "mission/history",
            "mission/upload",
            "mission/restore",
            "plan/upload",
            "state/subscribe",
        ] {
            assert!(matches!(
                client
                    .send_request(method, json!({ "entities": ["mission"] }))
                    .await,
                Err(CliError::Api(_))
            ));
        }
        assert_eq!(client.next_id, before);
        assert!(peer.buffer().is_empty());
        assert_eq!(
            peer.get_ref().try_read(&mut [0_u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let id = client
            .send_request("vehicle/list", json!({}))
            .await
            .unwrap();
        line.clear();
        peer.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "vehicle/list");
        let next = response(
            &mut client,
            &mut peer,
            json!({ "jsonrpc": "2.0", "id": id, "result": { "vehicles": [] } }),
        )
        .await;
        assert_eq!(next["result"], json!({ "vehicles": [] }));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    use super::{CliError, LocalClient, open_named_pipe};

    #[tokio::test]
    async fn a_busy_pipe_connects_when_the_server_replaces_its_listener() {
        let directory = tempfile::tempdir().unwrap();
        let name = crate::paths::pipe_name(directory.path());
        let occupied = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let first = ClientOptions::new().open(&name).unwrap();
        let connecting = open_named_pipe(&name, Duration::from_secs(2));
        tokio::pin!(connecting);
        assert!(
            tokio::time::timeout(Duration::from_millis(75), &mut connecting)
                .await
                .is_err()
        );
        let next = ServerOptions::new().create(&name).unwrap();
        let connected = tokio::time::timeout(Duration::from_secs(2), connecting)
            .await
            .unwrap()
            .unwrap();
        next.connect().await.unwrap();
        drop((connected, first, occupied, next));
    }

    #[tokio::test]
    async fn a_persistently_busy_pipe_has_a_bounded_explicit_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let name = crate::paths::pipe_name(directory.path());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        let error = open_named_pipe(&name, Duration::from_millis(75))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("remained busy"));
        drop((client, server));
    }

    #[tokio::test]
    async fn a_missing_pipe_still_reports_no_daemon() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            LocalClient::connect(directory.path()).await,
            Err(CliError::NoDaemon(_))
        ));
    }
}

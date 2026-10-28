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
                "capabilities": { "protocolVersion": uxduck_contracts::version::PROTOCOL_VERSION }
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
                    "approvals": approvals
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
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        Ok(id)
    }

    pub(crate) async fn cancel_request(&mut self, id: i64) -> Result<(), CliError> {
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
            return Ok(Some(message));
        }
        self.read().await
    }

    pub(crate) async fn read(&mut self) -> Result<Option<Value>, CliError> {
        let Some(line) = self.lines.next_line().await? else {
            return Ok(None);
        };
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| CliError::Protocol(e.to_string()))
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

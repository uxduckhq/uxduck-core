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
    #[cfg_attr(
        windows,
        allow(
            clippy::unused_async,
            clippy::unused_async_trait_impl,
            reason = "opening a named pipe does not wait"
        )
    )]
    pub async fn connect(data_dir: &Path) -> Result<Self, CliError> {
        #[cfg(unix)]
        let stream = {
            let path = crate::paths::socket_path(data_dir);
            tokio::net::UnixStream::connect(&path)
                .await
                .map_err(|_| CliError::NoDaemon(data_dir.display().to_string()))?
        };
        #[cfg(windows)]
        let stream = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(crate::paths::pipe_name(data_dir))
            .map_err(|_| CliError::NoDaemon(data_dir.display().to_string()))?;
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
        self.next_id += 1;
        let id = self.next_id;
        let mut frame =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        frame.push('\n');
        self.writer.write_all(frame.as_bytes()).await?;
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

    async fn read(&mut self) -> Result<Option<Value>, CliError> {
        let Some(line) = self.lines.next_line().await? else {
            return Ok(None);
        };
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| CliError::Protocol(e.to_string()))
    }
}

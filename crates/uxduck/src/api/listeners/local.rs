//! The local socket: a Unix domain socket (mode 0600, peer uid checked) or a
//! Windows named pipe, carrying newline-delimited JSON (ADR-0001 D4). Every
//! same-user process is `Trust::LocalUser`; its kind comes from `initialize`.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;

use crate::daemon::engine_task::EngineHandle;

/// Moves NDJSON frames between one accepted stream and the engine until
/// either side closes.
pub async fn serve<S>(stream: S, engine: EngineHandle, executable: Option<String>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let (outbox, mut outgoing) = futures::channel::mpsc::unbounded::<String>();
    let Ok(connection) = engine
        .connect(Transport::Socket, Trust::LocalUser, outbox)
        .await
    else {
        return;
    };
    let _ = engine
        .run(move |e| e.set_executable(connection, executable))
        .await;

    let writing = tokio::spawn(async move {
        while let Some(mut text) = outgoing.next().await {
            text.push('\n');
            if writer.write_all(text.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            engine.receive(connection, line);
        }
    }
    engine.disconnect(connection);
    writing.abort();
}

/// The daemon is already running on this data directory.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("a daemon is already running on {0}")]
    AlreadyRunning(String),
    #[error("cannot listen on {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// The pause after a failed accept, so a persistent failure does not spin.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(unix)]
pub use unix::bind;

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::{ACCEPT_BACKOFF, BindError, EngineHandle, Path, PathBuf, serve};

    /// Binds the socket, refusing if a live daemon answers on it and removing
    /// a stale one left by a crash. Returns the accept loop to spawn.
    ///
    /// # Errors
    /// When another daemon owns the socket or it cannot be created.
    pub async fn bind(
        data_dir: &Path,
        engine: EngineHandle,
    ) -> Result<(PathBuf, impl Future<Output = ()> + Send + use<>), BindError> {
        let path = crate::paths::socket_path(data_dir);
        let io = |source| BindError::Io {
            path: path.display().to_string(),
            source,
        };
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return Err(BindError::AlreadyRunning(path.display().to_string()));
        }
        if path.exists() {
            std::fs::remove_file(&path).map_err(io)?;
        }
        let listener = tokio::net::UnixListener::bind(&path).map_err(io)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        // The data directory is created by the user running the daemon.
        let owner = std::fs::metadata(data_dir).map_err(io)?.uid();

        let accept = async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    // Out of file descriptors fails at once, every time:
                    // back off rather than spin a core (S24).
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                };
                match stream.peer_cred() {
                    Ok(peer) if peer.uid() == owner => {
                        tracing::debug!(pid = ?peer.pid(), "local client connected");
                        let executable = peer
                            .pid()
                            .and_then(|pid| u32::try_from(pid).ok())
                            .and_then(crate::ports::executable_of);
                        tokio::spawn(serve(stream, engine.clone(), executable));
                    }
                    Ok(peer) => tracing::warn!(
                        uid = peer.uid(),
                        "refused a local client owned by another user"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, "refused a local client without peer credentials");
                    }
                }
            }
        };
        Ok((path, accept))
    }
}

#[cfg(windows)]
pub use windows::bind;

#[cfg(windows)]
mod windows {
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::{ACCEPT_BACKOFF, BindError, EngineHandle, Path, PathBuf, serve};

    /// Creates the named pipe. `first_pipe_instance` refuses a second daemon;
    /// remote clients are rejected. The default DACL lets only the creating
    /// user, administrators and SYSTEM open it for writing.
    ///
    /// # Errors
    /// When another daemon owns the pipe or it cannot be created.
    #[allow(
        clippy::unused_async,
        reason = "the same signature as the Unix listener"
    )]
    pub async fn bind(
        data_dir: &Path,
        engine: EngineHandle,
    ) -> Result<(PathBuf, impl Future<Output = ()> + Send + use<>), BindError> {
        let name = crate::paths::pipe_name(data_dir);
        let mut server = create(&name, true).map_err(|source| {
            if source.kind() == std::io::ErrorKind::PermissionDenied {
                BindError::AlreadyRunning(name.clone())
            } else {
                BindError::Io {
                    path: name.clone(),
                    source,
                }
            }
        })?;
        let path = PathBuf::from(&name);
        let accept = async move {
            loop {
                if server.connect().await.is_err() {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
                let Ok(next) = create(&name, false) else {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                };
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(serve(connected, engine.clone(), None));
            }
        };
        Ok((path, accept))
    }

    fn create(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create(name)
    }
}

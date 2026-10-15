//! An opt-in lifetime pipe for processes owned by a test runner.
//! Closing the parent's end also works after SIGKILL or `TerminateProcess`.

use std::future::{Future, pending};
use std::io;

use tokio::sync::oneshot;

/// Observe EOF without polling or leaving a Tokio blocking task that would
/// keep runtime shutdown waiting for stdin. Ordinary daemons never read it.
pub fn stdin_closed(enabled: bool) -> io::Result<impl Future<Output = ()>> {
    let receiver = if enabled {
        let (closed, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("parent-pipe".into())
            .spawn(move || {
                if let Err(error) = io::copy(&mut io::stdin().lock(), &mut io::sink()) {
                    tracing::warn!(%error, "cannot read the parent pipe; stopping");
                }
                let _ = closed.send(());
            })?;
        Some(receiver)
    } else {
        None
    };
    Ok(async move {
        if let Some(receiver) = receiver {
            let _ = receiver.await;
            tracing::info!("the parent pipe closed; stopping");
        } else {
            pending::<()>().await;
        }
    })
}

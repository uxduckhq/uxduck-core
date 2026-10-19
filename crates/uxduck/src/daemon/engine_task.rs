//! The Tokio task that owns the engine, making it the single writer
//! (ADR-0001 D15). Listeners talk to it through an [`EngineHandle`].

use tokio::sync::{mpsc, oneshot, watch};
use uxduck_contracts::dto::trace::Transport;
use uxduck_core::policy::admission::Trust;
use uxduck_engine::{ConnectionId, Engine, Outbox, PairingState, Session};

enum Command {
    Connect {
        transport: Transport,
        trust: Trust,
        outbox: Outbox,
        reply: oneshot::Sender<ConnectionId>,
    },
    Receive {
        connection: ConnectionId,
        text: String,
    },
    Disconnect {
        connection: ConnectionId,
    },
    CreateBootstrap {
        reply: oneshot::Sender<String>,
    },
    ExchangeBootstrap {
        token: String,
        reply: oneshot::Sender<Option<String>>,
    },
    RequestPairing {
        origin: String,
        user_agent: Option<String>,
        reply: oneshot::Sender<(String, String)>,
    },
    PollPairing {
        id: String,
        reply: oneshot::Sender<PairingState>,
    },
    Session {
        token: String,
        reply: oneshot::Sender<Option<Session>>,
    },
    Checkpoint {
        reply: oneshot::Sender<()>,
    },
    /// Any work on the engine, in order with everything else.
    Run(Box<dyn FnOnce(&mut Engine) + Send>),
}

/// What a client asked of the daemon itself, watched by the composition
/// root instead of polled (S24).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent requests watched by the composition root"
)]
pub struct Requested {
    pub restart: bool,
    pub stop: bool,
    pub panic: bool,
    pub app_update: bool,
}

/// A cheap, cloneable way to reach the engine task.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<Command>,
    requested: watch::Receiver<Requested>,
}

/// The engine task has stopped; the daemon is shutting down.
#[derive(Debug, thiserror::Error)]
#[error("the engine has stopped")]
pub struct EngineStopped;

impl EngineHandle {
    /// Starts the engine task.
    #[must_use]
    pub fn spawn(mut engine: Engine) -> Self {
        let (commands, mut inbox) = mpsc::unbounded_channel();
        let (requests, requested) = watch::channel(Requested::default());
        tokio::spawn(async move {
            while let Some(command) = inbox.recv().await {
                crate::wakes::count("engine");
                // A dropped reply means the caller went away; nothing to do.
                match command {
                    Command::Connect {
                        transport,
                        trust,
                        outbox,
                        reply,
                    } => {
                        let _ = reply.send(engine.connect(transport, trust, outbox));
                    }
                    Command::Receive { connection, text } => engine.receive(connection, &text),
                    Command::Disconnect { connection } => engine.disconnect(connection),
                    Command::CreateBootstrap { reply } => {
                        let _ = reply.send(engine.create_bootstrap());
                    }
                    Command::ExchangeBootstrap { token, reply } => {
                        let _ = reply.send(engine.exchange_bootstrap(&token));
                    }
                    Command::RequestPairing {
                        origin,
                        user_agent,
                        reply,
                    } => {
                        let _ = reply.send(engine.request_pairing(&origin, user_agent));
                    }
                    Command::PollPairing { id, reply } => {
                        let _ = reply.send(engine.poll_pairing(&id));
                    }
                    Command::Session { token, reply } => {
                        let _ = reply.send(engine.session(&token));
                    }
                    Command::Checkpoint { reply } => {
                        engine.checkpoint();
                        let _ = reply.send(());
                    }
                    Command::Run(work) if !engine.app_update_installing() => work(&mut engine),
                    // After the final update gate, delayed host callbacks must
                    // not start work either. Checkpoint remains available above.
                    Command::Run(_) => {}
                }
                let now = Requested {
                    restart: engine.restart_requested(),
                    stop: engine.stop_requested(),
                    panic: engine.panic_requested(),
                    app_update: engine.app_update_pending(),
                };
                requests.send_if_modified(|was| {
                    let changed = *was != now;
                    *was = now;
                    changed
                });
            }
        });
        Self {
            commands,
            requested,
        }
    }

    /// Resolves once a client has asked for what `wanted` picks out.
    pub async fn requested(&self, wanted: impl Fn(&Requested) -> bool) {
        let mut requested = self.requested.clone();
        if requested.wait_for(wanted).await.is_err() {
            // The engine stopped; shutdown happens elsewhere.
            std::future::pending::<()>().await;
        }
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, EngineStopped> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| EngineStopped)?;
        answer.await.map_err(|_| EngineStopped)
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn connect(
        &self,
        transport: Transport,
        trust: Trust,
        outbox: Outbox,
    ) -> Result<ConnectionId, EngineStopped> {
        self.ask(|reply| Command::Connect {
            transport,
            trust,
            outbox,
            reply,
        })
        .await
    }

    pub fn receive(&self, connection: ConnectionId, text: String) {
        let _ = self.commands.send(Command::Receive { connection, text });
    }

    pub fn disconnect(&self, connection: ConnectionId) {
        let _ = self.commands.send(Command::Disconnect { connection });
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn create_bootstrap(&self) -> Result<String, EngineStopped> {
        self.ask(|reply| Command::CreateBootstrap { reply }).await
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn exchange_bootstrap(&self, token: String) -> Result<Option<String>, EngineStopped> {
        self.ask(|reply| Command::ExchangeBootstrap { token, reply })
            .await
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn request_pairing(
        &self,
        origin: String,
        user_agent: Option<String>,
    ) -> Result<(String, String), EngineStopped> {
        self.ask(|reply| Command::RequestPairing {
            origin,
            user_agent,
            reply,
        })
        .await
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn poll_pairing(&self, id: String) -> Result<PairingState, EngineStopped> {
        self.ask(|reply| Command::PollPairing { id, reply }).await
    }

    /// # Errors
    /// When the engine has stopped.
    pub async fn session(&self, token: String) -> Result<Option<Session>, EngineStopped> {
        self.ask(|reply| Command::Session { token, reply }).await
    }

    /// Writes a checkpoint of the current state.
    ///
    /// # Errors
    /// When the engine has stopped.
    pub async fn checkpoint(&self) -> Result<(), EngineStopped> {
        self.ask(|reply| Command::Checkpoint { reply }).await
    }

    /// Runs `work` on the engine without waiting, from any thread, such as
    /// the video listener's.
    pub fn post(&self, work: impl FnOnce(&mut Engine) + Send + 'static) {
        let _ = self.commands.send(Command::Run(Box::new(work)));
    }

    /// Runs `work` on the engine, in order with every other command, and
    /// returns its result.
    ///
    /// # Errors
    /// When the engine has stopped.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Engine) -> T + Send + 'static,
    ) -> Result<T, EngineStopped> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Run(Box::new(move |engine| {
                let _ = reply.send(work(engine));
            })))
            .map_err(|_| EngineStopped)?;
        answer.await.map_err(|_| EngineStopped)
    }
}

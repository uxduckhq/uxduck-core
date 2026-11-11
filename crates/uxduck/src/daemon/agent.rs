//! The local agent host (ADR-0009): runs `uxduck-agent` turns in the daemon.
//! The agent is a client of kind `agent` on its own in-process connection,
//! so every tool call is an ordinary request: traced, attributed and checked
//! against its capabilities. Everything else it reports through the engine.

mod claude;
mod codex;
mod connections;
mod runtime;
#[cfg(test)]
mod runtime_tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{AbortHandle, Abortable, BoxFuture};
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use uxduck_agent::conversation::{self, Entry};
use uxduck_agent::provider::Sleep;
use uxduck_agent::provider::anthropic::AnthropicProvider;
use uxduck_agent::provider::fake::FakeProvider;
use uxduck_agent::provider::hosted;
use uxduck_agent::provider::openai::OpenAiProvider;
use uxduck_agent::provider::resilient::Resilient;
use uxduck_agent::{Host, Provider, Turn, Wire, models, run_turn};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::version::PROTOCOL_VERSION;
use uxduck_core::domain::thread::{Item, ItemKind, ItemStatus};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::agent::ToolOutcome;
use uxduck_engine::runtime::{AgentPort, TurnRequest, TurnStats};
use uxduck_engine::{ANTHROPIC_KEY, ConnectionId, OPENAI_KEY};

use super::engine_task::EngineHandle;
use super::transcripts::{Saved, Transcripts};
use crate::account::Sessions;
use crate::adapters::credentials::FileCredentials;

#[derive(Clone, Debug)]
pub enum ProviderChoice {
    /// UXDuck's hosted model, for a signed-in UXDuck (S29).
    Hosted,
    /// A developer's own key; development builds only (ADR-0013 P5).
    OpenAi {
        api_base: Option<String>,
        anthropic_base: Option<String>,
    },
    /// The own key when one is set, else the hosted model; development builds.
    Auto {
        api_base: Option<String>,
        anthropic_base: Option<String>,
    },
    /// Scripted, for development and tests; streams a word every `delay`.
    Fake { delay: Duration },
}

enum Command {
    Connection(uxduck_contracts::dto::thread::AgentConnectionParams),
    Start(TurnRequest),
    Cancel(String),
    Steer(String, u64, uxduck_agent::input::Input),
}

/// The engine's end: never blocks.
#[derive(Debug)]
pub struct AgentChannel(mpsc::UnboundedSender<Command>);

impl AgentPort for AgentChannel {
    fn connection(
        &mut self,
        params: uxduck_contracts::dto::thread::AgentConnectionParams,
    ) -> Result<(), String> {
        if !matches!(params.provider.as_str(), "claude-code" | "codex") {
            return Err("Unknown AI connection.".into());
        }
        self.0
            .send(Command::Connection(params))
            .map_err(|_| "The AI connection service has stopped.".into())
    }
    fn steer_turn(&mut self, thread: &str, turn: u64, item: &str, text: &str, share: bool) {
        let _ = self.0.send(Command::Steer(
            thread.into(),
            turn,
            uxduck_agent::input::Input {
                share,
                id: item.into(),
                text: text.into(),
            },
        ));
    }
    fn start_turn(&mut self, turn: TurnRequest) {
        let _ = self.0.send(Command::Start(turn));
    }
    fn cancel_turn(&mut self, thread_id: &str) {
        let _ = self.0.send(Command::Cancel(thread_id.to_owned()));
    }
}

/// The host's end, before it is started.
#[derive(Debug)]
pub struct AgentInbox(mpsc::UnboundedReceiver<Command>);

/// The two ends: give the channel to the engine, start the inbox after.
#[must_use]
pub fn channel() -> (AgentChannel, AgentInbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    (AgentChannel(tx), AgentInbox(rx))
}

/// JSON-RPC over an in-process engine connection.
struct Rpc {
    engine: EngineHandle,
    connection: ConnectionId,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    mission_reads: Mutex<uxduck_contracts::dto::mission_read::MissionReadSession>,
}

impl Rpc {
    async fn open(engine: EngineHandle) -> Option<Self> {
        let (outbox, mut frames) = futures::channel::mpsc::unbounded::<String>();
        let connection = engine
            .connect(Transport::Internal, Trust::LocalUser, outbox)
            .await
            .ok()?;
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::default();
        let routes = pending.clone();
        tokio::spawn(async move {
            while let Some(text) = frames.next().await {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let waiter = frame["id"]
                    .as_i64()
                    .and_then(|id| routes.lock().ok()?.remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(frame);
                }
            }
        });
        let rpc = Self {
            engine,
            connection,
            next_id: AtomicI64::new(0),
            pending,
            mission_reads: Mutex::new(
                uxduck_contracts::dto::mission_read::MissionReadSession::default(),
            ),
        };
        let init = rpc
            .call(
                "initialize",
                json!({
                    "clientInfo": { "name": "uxduck-agent", "version": env!("CARGO_PKG_VERSION"), "kind": "agent" },
                    "capabilities": { "protocolVersion": PROTOCOL_VERSION, "missionReadFloatBits": true, "replayStepMarks": true }
                }),
            )
            .await;
        init.is_ok().then_some(rpc)
    }

    /// `Err` holds the `ApiError` object.
    async fn call(&self, method: &str, params: Value) -> Result<Value, Value> {
        let stopped = || json!({ "code": -32603, "message": "the daemon is stopping", "data": { "kind": "internal" } });
        self.mission_reads
            .lock()
            .map_err(|_| stopped())?
            .before_request(method, &params)
            .map_err(|error| json!(error))?;
        let mission_bearing =
            uxduck_contracts::dto::mission_read::mission_read_request(method, &params);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (reply, answer) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, reply);
        }
        let _request = PendingRequest { rpc: self, id };
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.engine.receive(self.connection, frame.to_string());
        let frame = answer.await.map_err(|_| stopped())?;
        if let Some(error) = frame.get("error") {
            Err(error.clone())
        } else {
            let result = frame.get("result").cloned().unwrap_or(Value::Null);
            self.mission_reads
                .lock()
                .map_err(|_| stopped())?
                .accept_result(method, mission_bearing, &result)
                .map_err(|error| json!(error))?;
            Ok(result)
        }
    }
}

struct PendingRequest<'a> {
    rpc: &'a Rpc,
    id: i64,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if self
            .rpc
            .pending
            .lock()
            .is_ok_and(|mut pending| pending.remove(&self.id).is_some())
        {
            self.rpc.engine.receive(
                self.rpc.connection,
                json!({ "jsonrpc": "2.0", "method": "cancel", "params": { "id": self.id } })
                    .to_string(),
            );
        }
    }
}

/// One turn's view of the engine.
struct LocalHost {
    rpc: Arc<Rpc>,
    thread_id: String,
    clock: std::time::Instant,
    inputs: uxduck_agent::input::Inputs,
    turn: u64,
}

impl LocalHost {
    fn engine(&self) -> &EngineHandle {
        &self.rpc.engine
    }
}

impl Host for LocalHost {
    fn inputs(&self) -> uxduck_agent::input::Inputs {
        self.inputs.clone()
    }
    fn input_applied(&self, id: String) -> BoxFuture<'_, ()> {
        let (thread, turn) = (self.thread_id.clone(), self.turn);
        async move {
            let _ = self
                .engine()
                .run(move |e| e.agent_input_applied(&thread, turn, &id))
                .await;
        }
        .boxed()
    }
    fn message_start(&self) -> BoxFuture<'_, String> {
        let (connection, thread) = (self.rpc.connection, self.thread_id.clone());
        async move {
            self.engine()
                .run(move |e| e.agent_message_start(Some(connection), &thread))
                .await
                .unwrap_or_default()
        }
        .boxed()
    }

    fn text_delta(&self, item: String, delta: String) -> BoxFuture<'_, ()> {
        async move {
            let _ = self
                .engine()
                .run(move |e| e.agent_text_delta(&item, &delta))
                .await;
        }
        .boxed()
    }

    fn message_finish(&self, item: String, response: Option<String>) -> BoxFuture<'_, ()> {
        let connection = self.rpc.connection;
        async move {
            let _ = self
                .engine()
                .run(move |e| {
                    e.agent_message_finish(
                        Some(connection),
                        &item,
                        ItemStatus::Completed,
                        response,
                    );
                })
                .await;
        }
        .boxed()
    }

    fn tool_start(&self, method: String, params: Value) -> BoxFuture<'_, String> {
        let (connection, thread) = (self.rpc.connection, self.thread_id.clone());
        async move {
            self.engine()
                .run(move |e| e.agent_tool_start(connection, &thread, &method, params.to_string()))
                .await
                .unwrap_or_default()
        }
        .boxed()
    }

    fn call(&self, method: String, mut params: Value) -> BoxFuture<'_, Result<Value, Value>> {
        // A draft is written in this chat (X12).
        if method == "plugin/draft"
            && let Some(fields) = params.as_object_mut()
        {
            fields
                .entry("threadId")
                .or_insert_with(|| Value::String(self.thread_id.clone()));
        }
        // A tab opens for the person in this chat (S43), whatever was asked.
        if method == "layout/open"
            && let Some(fields) = params.as_object_mut()
        {
            fields.insert("threadId".into(), Value::String(self.thread_id.clone()));
        }
        // A report is filed from this chat, and added to from it (S294).
        if matches!(method.as_str(), "feedback/file" | "feedback/add")
            && let Some(fields) = params.as_object_mut()
        {
            fields.insert("threadId".into(), Value::String(self.thread_id.clone()));
            fields.remove("attachments");
        }
        async move { self.rpc.call(&method, params).await }.boxed()
    }

    fn tool_finish(&self, item: String, outcome: Result<Value, Value>) -> BoxFuture<'_, ()> {
        let connection = self.rpc.connection;
        let outcome = match outcome {
            Ok(result) => ToolOutcome::Result(result.to_string()),
            Err(error) => ToolOutcome::Error(error.to_string()),
        };
        async move {
            let _ = self
                .engine()
                .run(move |e| e.agent_tool_finish(connection, &item, outcome))
                .await;
        }
        .boxed()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Starts the host. Turns run concurrently, one per thread; a cancel aborts
/// the thread's turn (the engine has already marked its items cancelled).
pub fn start(
    inbox: AgentInbox,
    engine: EngineHandle,
    provider: ProviderChoice,
    credentials: FileCredentials,
    data_dir: &std::path::Path,
) {
    let AgentInbox(mut commands) = inbox;
    let connections = connections::Connections::new(&credentials, data_dir);
    let sessions = Arc::new(Sessions::new(credentials.clone()));
    let transcripts = Arc::new(Transcripts::new(data_dir));
    let services = Arc::new(Services {
        choice: provider,
        credentials,
        sessions,
        transcripts: transcripts.clone(),
        connections: connections.clone(),
    });
    tokio::spawn(async move {
        let Some(rpc) = Rpc::open(engine.clone()).await.map(Arc::new) else {
            tracing::error!(
                code = "agent-connect",
                "the agent could not connect to the engine"
            );
            return;
        };
        // Chats deleted since the last start keep no transcript.
        let live: std::collections::HashSet<String> = engine
            .run(|e| {
                e.state()
                    .threads
                    .values()
                    .filter(|thread| thread.deleted_at_us.is_none())
                    .map(|thread| thread.id.clone())
                    .collect()
            })
            .await
            .unwrap_or_default();
        transcripts.prune(|thread| live.contains(thread));
        connections.publish(&engine).await;
        let mut connection_jobs: HashMap<String, AbortHandle> = HashMap::new();
        for provider in ["claude-code", "codex"] {
            start_connection_job(
                &mut connection_jobs,
                connections.clone(),
                engine.clone(),
                uxduck_contracts::dto::thread::AgentConnectionParams {
                    provider: provider.into(),
                    action: uxduck_contracts::dto::thread::ConnectionAction::Refresh,
                },
            );
        }
        let mut running: HashMap<String, (u64, AbortHandle, uxduck_agent::input::Inputs)> =
            HashMap::new();
        while let Some(command) = commands.recv().await {
            match command {
                Command::Connection(params) => {
                    if params.action == uxduck_contracts::dto::thread::ConnectionAction::Disconnect
                    {
                        let provider = params.provider.clone();
                        let _ = engine
                            .run(move |e| e.agent_provider_disconnected(&provider))
                            .await;
                    }
                    start_connection_job(
                        &mut connection_jobs,
                        connections.clone(),
                        engine.clone(),
                        params,
                    );
                }
                Command::Cancel(thread_id) => {
                    if let Some((_, handle, _)) = running.remove(&thread_id) {
                        handle.abort();
                    }
                }
                Command::Steer(thread, turn, input) => {
                    if let Some((active, _, inputs)) = running.get(&thread)
                        && *active == turn
                    {
                        inputs.push(input);
                    }
                }
                Command::Start(request) => {
                    running.retain(|_, (_, handle, _)| !handle.is_aborted());
                    let (handle, registration) = AbortHandle::new_pair();
                    let inputs = uxduck_agent::input::Inputs::default();
                    running.insert(
                        request.thread_id.clone(),
                        (request.turn, handle, inputs.clone()),
                    );
                    let turn = run(rpc.clone(), services.clone(), request, inputs);
                    tokio::spawn(Abortable::new(turn, registration));
                }
            }
        }
    });
}

/// Signed in, but `/v1/me` has not said which models there are yet.
const CHECKING_MODELS: &str = "UXDuck is still checking which AI models your account can use. Send your message again in a moment.";

/// The hosted model for one turn: a fresh session, what may be kept, and
/// the chat's id in the account.
async fn hosted_provider(
    sessions: &Sessions,
    request: &TurnRequest,
    wire: Wire,
) -> Result<Box<dyn Provider>, String> {
    let session = sessions.current().await?;
    // A chat sent before the server said which models there are.
    if request.model.is_empty() {
        return Err(CHECKING_MODELS.to_owned());
    }
    let api = sessions
        .api()
        .ok_or("This UXDuck has no account server.")?
        .to_owned();
    let chat = crate::account::chat_id(sessions.credentials(), &request.thread_id);
    Ok(hosted::provider(&api, &session, request.share, &chat, wire))
}

/// The provider for this turn's model: a developer's own key for its
/// company where development builds allow one, else the hosted model.
async fn provider_for_turn(
    choice: ProviderChoice,
    credentials: &FileCredentials,
    sessions: &Sessions,
    request: &TurnRequest,
) -> Result<Box<dyn Provider>, String> {
    let wire = Wire::for_model(&request.model);
    let own = |name: &str| credentials.get(name).filter(|key| !key.is_empty());
    let direct =
        |api_base: Option<&str>, anthropic_base: Option<&str>| -> Option<Box<dyn Provider>> {
            match wire {
                Wire::OpenAi => own(OPENAI_KEY)
                    .map(|key| Box::new(OpenAiProvider::new(&key, api_base)) as Box<dyn Provider>),
                Wire::Anthropic => own(ANTHROPIC_KEY).map(|key| {
                    Box::new(AnthropicProvider::new(&key, anthropic_base)) as Box<dyn Provider>
                }),
            }
        };
    match choice {
        ProviderChoice::Fake { delay } => {
            let sleep = Arc::new(|every: Duration| tokio::time::sleep(every).boxed());
            Ok(Box::new(FakeProvider::new().with_delay(sleep, delay)))
        }
        ProviderChoice::OpenAi {
            api_base,
            anthropic_base,
        } => direct(api_base.as_deref(), anthropic_base.as_deref()).ok_or_else(|| {
            match wire {
                Wire::OpenAi => NO_KEY,
                Wire::Anthropic => NO_ANTHROPIC_KEY,
            }
            .to_owned()
        }),
        ProviderChoice::Auto {
            api_base,
            anthropic_base,
        } => match direct(api_base.as_deref(), anthropic_base.as_deref()) {
            Some(provider) => Ok(provider),
            None => hosted_provider(sessions, request, wire).await,
        },
        ProviderChoice::Hosted => hosted_provider(sessions, request, wire).await,
    }
}

/// The host's timer, for retries and timeouts.
fn sleep() -> Sleep {
    Arc::new(|every: Duration| tokio::time::sleep(every).boxed())
}

/// What the chat is about, told to the model once per change.
fn context_of(request: &TurnRequest) -> Option<String> {
    request.vehicle.as_ref().map(|(id, name)| {
        format!("This thread is about the vehicle \"{name}\" (id {id}). Use vehicle_get with that id for its details and telemetry_get for where it is and its battery, attitude and GPS.")
    })
}

fn engine_stats(stats: &uxduck_agent::TurnStats) -> TurnStats {
    TurnStats {
        rounds: stats.rounds,
        tool_errors: stats.tool_errors,
        input_tokens: stats.usage.input_tokens,
        output_tokens: stats.usage.output_tokens,
        cache_read_tokens: stats.usage.cache_read_tokens,
        cache_write_tokens: stats.usage.cache_write_tokens,
        first_token_ms: stats.first_token_ms,
        retries: stats.retries,
        fell_back: stats.fell_back,
        compacted: stats.compacted,
        thinking_dropped: stats.usage.thinking_dropped,
    }
}

/// A chat's items, as the journal has them now.
async fn items_now(rpc: &Rpc, thread_id: &str) -> Vec<Item> {
    let thread_id = thread_id.to_owned();
    rpc.engine
        .run(move |e| {
            e.state()
                .items_of(&thread_id)
                .into_iter()
                .cloned()
                .collect()
        })
        .await
        .unwrap_or_default()
}

#[derive(Clone)]
struct Services {
    choice: ProviderChoice,
    credentials: FileCredentials,
    sessions: Arc<Sessions>,
    transcripts: Arc<Transcripts>,
    connections: Arc<connections::Connections>,
}
struct Disconnect {
    engine: EngineHandle,
    connection: ConnectionId,
}
impl Drop for Disconnect {
    fn drop(&mut self) {
        self.engine.disconnect(self.connection);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the existing hosted turn preparation and transcript lifecycle stay together"
)]
async fn run(
    rpc: Arc<Rpc>,
    services: Arc<Services>,
    request: TurnRequest,
    inputs: uxduck_agent::input::Inputs,
) {
    let engine = rpc.engine.clone();
    let (thread_id, turn_id) = (request.thread_id.clone(), request.turn);
    if request.model.starts_with("claude-code:") || request.model.starts_with("codex:") {
        run_local(rpc, &services.connections, request, inputs).await;
        return;
    }
    let provider = match provider_for_turn(
        services.choice.clone(),
        &services.credentials,
        &services.sessions,
        &request,
    )
    .await
    {
        Ok(provider) => provider,
        Err(why) => {
            let thread = thread_id.clone();
            let _ = engine
                .run(move |e| {
                    e.agent_error(None, &thread, &why);
                    e.agent_turn_finish(&thread, turn_id, None);
                })
                .await;
            return;
        }
    };
    let provider =
        Resilient::new(provider, sleep()).with_fallback(models::settings(&request.model).fallback);
    let wire = provider.wire();

    // The chat as the journal has it, before this turn's message.
    let items = items_now(&rpc, &thread_id).await;
    let before = items
        .iter()
        .position(|item| item.id == request.item_id)
        .unwrap_or(items.len());
    let covers: Vec<String> = items[..before]
        .iter()
        .filter(|i| i.in_history())
        .map(|item| item.id.clone())
        .collect();
    let context = context_of(&request);
    let kept = services.transcripts.matching(&thread_id, wire, &covers);
    let previous = kept
        .as_ref()
        .and_then(|kept| kept.last_response.clone())
        .or_else(|| request.previous_response_id.clone());
    let (conversation, context_sent) = match kept {
        Some(kept) => (
            kept.conversation,
            kept.context_sent && kept.context == context,
        ),
        None => (
            conversation::rebuild(wire, &entries(&items[..before])),
            false,
        ),
    };

    let host = LocalHost {
        rpc: rpc.clone(),
        thread_id: thread_id.clone(),
        clock: std::time::Instant::now(),
        inputs,
        turn: turn_id,
    };
    let cache_key = Some(crate::account::chat_id(&services.credentials, &thread_id));
    let turn = Turn {
        model: request.model.clone(),
        effort: request.effort.clone(),
        text: request.text.clone(),
        conversation,
        context: context.clone(),
        context_sent,
        previous,
        cache_key: cache_key.clone(),
    };
    let began = std::time::Instant::now();
    let outcome = run_turn(&provider, &host, turn).await;
    let elapsed_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &outcome.result {
        Err(error) => tracing::warn!(
            code = "agent-turn-failed",
            model = %request.model,
            rounds = outcome.stats.rounds,
            retries = outcome.stats.retries,
            fell_back = outcome.stats.fell_back,
            elapsed_ms,
            %error,
            "an AI turn failed"
        ),
        Ok(()) if outcome.stats.retries > 0 || outcome.stats.fell_back => tracing::info!(
            code = "agent-turn-recovered",
            model = %request.model,
            rounds = outcome.stats.rounds,
            retries = outcome.stats.retries,
            fell_back = outcome.stats.fell_back,
            elapsed_ms,
            "an AI turn needed another try"
        ),
        Ok(()) => {}
    }

    if outcome.result.is_ok() {
        let now = items_now(&rpc, &thread_id)
            .await
            .into_iter()
            .filter(Item::in_history)
            .map(|item| item.id)
            .collect();
        services.transcripts.keep(
            &thread_id,
            &Saved {
                wire,
                model: request.model.clone(),
                covers: now,
                context,
                context_sent: outcome.context_sent,
                last_response: outcome.response_id.clone(),
                conversation: outcome.conversation,
            },
        );
    }
    let success = outcome.result.is_ok();
    let stats = engine_stats(&outcome.stats);
    let (result, response_id) = (outcome.result, outcome.response_id);
    let finished_thread = thread_id.clone();
    let _ = engine
        .run(move |e| {
            e.agent_turn_stats(turn_id, stats);
            match result {
                Ok(()) => e.agent_turn_finish(&finished_thread, turn_id, response_id),
                Err(error) => {
                    e.agent_error(None, &finished_thread, &error.to_string());
                    e.agent_turn_finish(&finished_thread, turn_id, None);
                }
            }
        })
        .await;
    if success
        && let Some(text) = request.title.as_deref()
        && title_requested(&engine, &request).await
    {
        let began = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            uxduck_agent::turn::generate_title(&provider, &request.model, text, cache_key),
        )
        .await
        .unwrap_or_default();
        finish_title(&engine, &request, outcome, began.elapsed()).await;
    }
}

async fn title_requested(engine: &EngineHandle, request: &TurnRequest) -> bool {
    let (thread, item) = (request.thread_id.clone(), request.item_id.clone());
    engine
        .run(move |e| e.agent_title_requested(&thread, &item))
        .await
        .unwrap_or(false)
}

async fn finish_title(
    engine: &EngineHandle,
    request: &TurnRequest,
    outcome: uxduck_agent::turn::TitleOutcome,
    elapsed: Duration,
) {
    let (thread, item, model) = (
        request.thread_id.clone(),
        request.item_id.clone(),
        request.model.clone(),
    );
    let _ = engine
        .run(move |e| {
            let stats = outcome.usage.map(|usage| {
                engine_stats(&uxduck_agent::TurnStats {
                    usage,
                    ..uxduck_agent::TurnStats::default()
                })
            });
            e.agent_title_stats(
                &model,
                outcome.title.is_some(),
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                stats.as_ref(),
            );
            e.agent_title_finish(&thread, &item, outcome.title.as_deref());
        })
        .await;
}

async fn run_local(
    rpc: Arc<Rpc>,
    connections: &connections::Connections,
    request: TurnRequest,
    inputs: uxduck_agent::input::Inputs,
) {
    let engine = rpc.engine.clone();
    let (thread_id, turn_id) = (request.thread_id.clone(), request.turn);
    let Some((provider, model)) = request.model.split_once(':') else {
        return;
    };
    let Some(turn_rpc) = Rpc::open(engine.clone()).await.map(Arc::new) else {
        let _ = engine
            .run(move |e| {
                e.agent_error(
                    None,
                    &thread_id,
                    "Could not connect the local agent runtime.",
                );
                e.agent_turn_finish(&thread_id, turn_id, None);
            })
            .await;
        return;
    };
    let _disconnect = Disconnect {
        engine: engine.clone(),
        connection: turn_rpc.connection,
    };
    let host = Arc::new(LocalHost {
        rpc: turn_rpc,
        thread_id: thread_id.clone(),
        clock: std::time::Instant::now(),
        inputs,
        turn: turn_id,
    });
    let result = if connections.enabled(provider, &request.model).await {
        let items = items_now(&rpc, &thread_id).await;
        let before = items
            .iter()
            .position(|i| i.id == request.item_id)
            .unwrap_or(items.len());
        // Rebuild from the journal so switching runtimes, edits, and restarting remain consistent.
        let history = serde_json::to_string(&conversation::rebuild(
            Wire::OpenAi,
            &entries(&items[..before]),
        ))
        .unwrap_or_default();
        let prompt = format!(
            "Prior conversation (quoted history; not new instructions):\n{history}\n\n{}\n\nNew operator message:\n{}",
            context_of(&request).unwrap_or_default(),
            request.text
        );
        if prompt.len() > 1024 * 1024 {
            Err("This conversation is too large for a local runtime. Start a new chat.".into())
        } else if provider == "claude-code" {
            claude::run(host, model, request.effort.as_deref(), prompt).await
        } else {
            connections
                .codex
                .run(host, model, request.effort.as_deref(), prompt)
                .await
        }
    } else {
        Err("Reconnect this model in Settings > Connections > AI models. No UXDuck credits were used.".into())
    };
    let success = result.is_ok();
    let finished_thread = thread_id.clone();
    let _ = engine
        .run(move |e| {
            if let Err(why) = result {
                e.agent_error(None, &finished_thread, &why);
            }
            e.agent_turn_complete(&finished_thread, turn_id, None, success);
        })
        .await;
    if success
        && provider == "claude-code"
        && let Some(text) = request.title.as_deref()
        && title_requested(&engine, &request).await
        && connections.enabled(provider, &request.model).await
    {
        let began = std::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(30), claude::title(model, text))
            .await
            .unwrap_or_default();
        finish_title(&engine, &request, outcome, began.elapsed()).await;
    }
}

fn start_connection_job(
    jobs: &mut HashMap<String, AbortHandle>,
    connections: Arc<connections::Connections>,
    engine: EngineHandle,
    params: uxduck_contracts::dto::thread::AgentConnectionParams,
) {
    if let Some(old) = jobs.remove(&params.provider) {
        old.abort();
    }
    let (handle, registration) = AbortHandle::new_pair();
    jobs.insert(params.provider.clone(), handle);
    tokio::spawn(Abortable::new(
        async move {
            connections.handle(&engine, params).await;
        },
        registration,
    ));
}

/// Shown in the thread when no key is set.
const NO_KEY: &str = "No OpenAI key is set. Run `uxduck config set openai.key` on this computer.";
const NO_ANTHROPIC_KEY: &str =
    "No Anthropic key is set. Run `uxduck config set anthropic.key` on this computer.";

/// The journal's messages and tool calls, as a provider-neutral history.
fn entries(items: &[Item]) -> Vec<Entry> {
    items
        .iter()
        .filter(|item| item.in_history())
        .filter_map(|item| match item.kind {
            ItemKind::UserMessage => Some(Entry::User(uxduck_core::domain::thread::for_agent(
                &item.text,
                &item.attachments,
            ))),
            ItemKind::AgentMessage => Some(Entry::Agent(item.text.clone())),
            ItemKind::ToolCall => item.tool_call.as_ref().map(|call| Entry::Tool {
                id: item.id.clone(),
                name: uxduck_agent::tools::tool_name(&call.method),
                arguments: serde_json::from_str(&call.params).unwrap_or_else(|_| json!({})),
                output: call
                    .result
                    .clone()
                    .or_else(|| call.error.clone())
                    .unwrap_or_else(|| r#"{"error":"cancelled"}"#.into()),
                failed: call.result.is_none(),
            }),
            ItemKind::Error => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_core::domain::thread::{ItemStatus, ToolCall};

    fn item(id: &str, kind: ItemKind, text: &str) -> Item {
        Item {
            id: id.into(),
            thread_id: "thread".into(),
            index: 0,
            kind,
            status: ItemStatus::Completed,
            text: text.into(),
            tool_call: None,
            created_at_us: 0,
            version: 1,
            response_id: None,
            feedback: None,
            attachments: Vec::new(),
            spoken: None,
        }
    }

    #[test]
    fn the_journal_becomes_a_history_with_cancelled_tools_failed() {
        let mut tool = item("tool_1", ItemKind::ToolCall, "");
        tool.status = ItemStatus::Cancelled;
        tool.tool_call = Some(ToolCall {
            method: "vehicle/list".into(),
            params: "not-json".into(),
            client_id: "agent".into(),
            result: None,
            error: None,
        });
        let mut error = item("error_1", ItemKind::Error, "provider failed");
        error.index = 3;
        let mut user = item("user_1", ItemKind::UserMessage, "old question");
        user.index = 1;
        let mut agent = item("agent_1", ItemKind::AgentMessage, "old answer");
        agent.index = 2;
        let items = [user, agent, tool, error];

        assert_eq!(
            entries(&items),
            [
                Entry::User("old question".into()),
                Entry::Agent("old answer".into()),
                Entry::Tool {
                    id: "tool_1".into(),
                    name: "vehicle_list".into(),
                    arguments: json!({}),
                    output: r#"{"error":"cancelled"}"#.into(),
                    failed: true,
                }
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::unwrap_used,
        clippy::too_many_lines,
        reason = "in-process RPC protocol fixture"
    )]
    async fn mission_read_rpc_validates_pending_replies_and_refuses_before_send() {
        let rpc = Rpc::open(super::runtime_tests::engine()).await.unwrap();
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../uxduck-contracts/tests/goldens/mission-read.json"
        )))
        .unwrap();
        let golden = |method: &str| -> Value {
            let frame: Value =
                serde_json::from_str(fixture["frames"][method].as_str().unwrap()).unwrap();
            frame["result"].clone()
        };
        let mut good = Box::pin(rpc.call("mission/refresh", json!({ "id": "v" })));
        assert!(futures::poll!(good.as_mut()).is_pending());
        let id = rpc.next_id.load(Ordering::SeqCst);
        rpc.pending
            .lock()
            .unwrap()
            .remove(&id)
            .unwrap()
            .send(json!({ "id": id, "result": golden("mission/refresh") }))
            .unwrap();
        let result = good.await.unwrap();
        assert_eq!(
            result["items"][3]["param4"]["nonFiniteF64"],
            "7ff8000000000001"
        );
        let mut late = Box::pin(rpc.call("mission/history", json!({ "id": "v" })));
        assert!(futures::poll!(late.as_mut()).is_pending());
        let late_id = rpc.next_id.load(Ordering::SeqCst);
        let mut broken = golden("mission/refresh");
        broken["items"][0]["param4"] = Value::Null;
        let mut bad = Box::pin(rpc.call("mission/refresh", json!({ "id": "v" })));
        assert!(futures::poll!(bad.as_mut()).is_pending());
        let id = rpc.next_id.load(Ordering::SeqCst);
        rpc.pending
            .lock()
            .unwrap()
            .remove(&id)
            .unwrap()
            .send(json!({ "id": id, "result": broken }))
            .unwrap();
        assert_eq!(bad.await.unwrap_err()["data"]["kind"], "update-required");
        rpc.pending
            .lock()
            .unwrap()
            .remove(&late_id)
            .unwrap()
            .send(json!({ "id": late_id, "result": golden("mission/history") }))
            .unwrap();
        assert_eq!(late.await.unwrap_err()["data"]["kind"], "update-required");
        let before = rpc.next_id.load(Ordering::SeqCst);
        for method in [
            "mission/refresh",
            "mission/history",
            "mission/upload",
            "mission/restore",
            "plan/upload",
            "state/subscribe",
        ] {
            assert_eq!(
                rpc.call(method, json!({ "entities": ["mission"] }))
                    .await
                    .unwrap_err()["data"]["kind"],
                "update-required"
            );
        }
        assert_eq!(rpc.next_id.load(Ordering::SeqCst), before);
        assert!(rpc.pending.lock().unwrap().is_empty());
        assert_eq!(
            rpc.call("vehicle/list", json!({})).await.unwrap()["vehicles"],
            json!([])
        );
    }
}

//! Browser host for the shared agent loop. All tools use an ordinary agent
//! connection; JavaScript performs only the authenticated HTTP transport.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use futures::channel::{mpsc, oneshot};
use futures::future::{AbortHandle, Abortable, BoxFuture};
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uxduck_agent::conversation::{self, Entry};
use uxduck_agent::provider::{anthropic, openai};
use uxduck_agent::{
    Host, Provider, ProviderError, ProviderEvent, ProviderRequest, Turn, TurnOutcome, Wire,
    run_turn,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::version::PROTOCOL_VERSION;
use uxduck_core::domain::thread::{Item, ItemKind, ItemStatus};
use uxduck_core::policy::admission::Trust;
use uxduck_engine::agent::ToolOutcome;
use uxduck_engine::runtime::{TurnRequest, TurnStats};
use uxduck_engine::{ConnectionId, Engine};

/// An account hint from one page: the browser's one account, whichever
/// operator the page acts as (ADR-0033, 2026-10-31). The HTTP transport
/// checks its account id against the current server session before every
/// inference request. `operator_id` is the page's operator: a change ends
/// that page's saved conversations, and it no longer decides who may use AI.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Account {
    pub operator_id: Option<String>,
    pub account_id: Option<String>,
    pub revision: Option<String>,
    /// Server-offered models from the page's account read; inference still authorizes every request.
    #[serde(default)]
    pub hosted_models: Option<Vec<String>>,
}
impl Account {
    /// Models offered by the server, none until it has said; the API
    /// authorizes every request again.
    fn supports(&self, model: &str) -> bool {
        self.hosted_models
            .as_ref()
            .is_some_and(|models| models.iter().any(|offered| offered == model))
    }

    #[must_use]
    pub fn valid(&self) -> bool {
        [&self.operator_id, &self.account_id, &self.revision]
            .into_iter()
            .all(|v| v.as_ref().is_none_or(|v| !v.is_empty() && v.len() <= 128))
            && self.account_id.is_some()
            && self.revision.is_some()
            && self.hosted_models.as_ref().is_none_or(|models| {
                models.len() <= 128
                    && models
                        .iter()
                        .all(|model| !model.is_empty() && model.len() <= 128)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Output {
    Request {
        request: u64,
        #[serde(rename = "accountId")]
        account_id: String,
        chat: String,
        share: bool,
        continues: Option<String>,
        /// The hosted model's API for this model: `responses` or `messages`.
        path: &'static str,
        body: Value,
        websocket: bool,
    },
    Send {
        request: u64,
        event: Value,
    },
    Abort {
        request: u64,
    },
}

struct Scope {
    thread: String,
    turn: u64,
    connection: ConnectionId,
    origin: ConnectionId,
    live: AtomicBool,
    account: Account,
    share: bool,
    now: Arc<AtomicU64>,
    began: u64,
}
type Reply<T> = oneshot::Sender<T>;
enum Action {
    MessageStart(Reply<String>),
    Delta(String, String),
    MessageFinish(String, Option<String>),
    ToolStart(String, Value, Reply<String>),
    Rpc(String, Value, Reply<Result<Value, Value>>),
    ToolFinish(String, Result<Value, Value>),
    InputApplied(String),
    SocketSend(u64, Value),
    Model(
        u64,
        ProviderRequest,
        mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
        uxduck_agent::input::Inputs,
        bool,
    ),
    Finished(TurnOutcome, String),
    TitleFinished(u64, uxduck_agent::turn::TitleOutcome),
}
type Queue = Arc<Mutex<Vec<(Arc<Scope>, Action)>>>;
fn push(queue: &Queue, scope: &Arc<Scope>, action: Action) {
    if scope.live.load(Ordering::SeqCst) {
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((scope.clone(), action));
    }
}
struct BrowserHost {
    inputs: uxduck_agent::input::Inputs,
    scope: Arc<Scope>,
    queue: Queue,
    mission_reads: Mutex<uxduck_contracts::dto::mission_read::MissionReadSession>,
}
impl Host for BrowserHost {
    fn can_file_feedback(&self) -> bool {
        false
    }
    fn inputs(&self) -> uxduck_agent::input::Inputs {
        self.inputs.clone()
    }
    fn input_applied(&self, id: String) -> BoxFuture<'_, ()> {
        push(&self.queue, &self.scope, Action::InputApplied(id));
        futures::future::ready(()).boxed()
    }
    fn message_start(&self) -> BoxFuture<'_, String> {
        let (tx, rx) = oneshot::channel();
        push(&self.queue, &self.scope, Action::MessageStart(tx));
        async move { rx.await.unwrap_or_default() }.boxed()
    }
    fn text_delta(&self, item: String, delta: String) -> BoxFuture<'_, ()> {
        push(&self.queue, &self.scope, Action::Delta(item, delta));
        futures::future::ready(()).boxed()
    }
    fn message_finish(&self, item: String, response: Option<String>) -> BoxFuture<'_, ()> {
        push(
            &self.queue,
            &self.scope,
            Action::MessageFinish(item, response),
        );
        futures::future::ready(()).boxed()
    }
    fn tool_start(&self, method: String, params: Value) -> BoxFuture<'_, String> {
        let (tx, rx) = oneshot::channel();
        push(
            &self.queue,
            &self.scope,
            Action::ToolStart(method, params, tx),
        );
        async move { rx.await.unwrap_or_default() }.boxed()
    }
    fn call(&self, method: String, mut params: Value) -> BoxFuture<'_, Result<Value, Value>> {
        if let Err(error) = self
            .mission_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .before_request(&method, &params)
        {
            return futures::future::ready(Err(json!(error))).boxed();
        }
        let mission_bearing =
            uxduck_contracts::dto::mission_read::mission_read_request(&method, &params);
        if matches!(
            method.as_str(),
            "layout/open" | "plugin/draft" | "feedback/file" | "feedback/add"
        ) && let Some(fields) = params.as_object_mut()
        {
            fields.insert("threadId".into(), Value::String(self.scope.thread.clone()));
        }
        let (tx, rx) = oneshot::channel();
        push(
            &self.queue,
            &self.scope,
            Action::Rpc(method.clone(), params, tx),
        );
        async move {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(json!({"code":-32603,"message":"The agent stopped."})))?;
            self.mission_reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .accept_result(&method, mission_bearing, &result)
                .map_err(|error| json!(error))?;
            Ok(result)
        }
        .boxed()
    }
    fn tool_finish(&self, item: String, result: Result<Value, Value>) -> BoxFuture<'_, ()> {
        push(&self.queue, &self.scope, Action::ToolFinish(item, result));
        futures::future::ready(()).boxed()
    }
    fn now_ms(&self) -> u64 {
        self.scope
            .now
            .load(Ordering::SeqCst)
            .saturating_sub(self.scope.began)
    }
}
struct BrowserProvider {
    wire: Wire,
    scope: Arc<Scope>,
    queue: Queue,
    next: Arc<AtomicU64>,
}
impl Provider for BrowserProvider {
    fn respond_with_input(
        &self,
        request: ProviderRequest,
        inputs: uxduck_agent::input::Inputs,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let (tx, rx) = mpsc::unbounded();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        push(
            &self.queue,
            &self.scope,
            Action::Model(id, request, tx, inputs, false),
        );
        rx.boxed()
    }
    fn wire(&self) -> Wire {
        self.wire
    }
    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let (tx, rx) = mpsc::unbounded();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        push(
            &self.queue,
            &self.scope,
            Action::Model(
                id,
                request,
                tx,
                uxduck_agent::input::Inputs::default(),
                false,
            ),
        );
        rx.boxed()
    }
    fn respond_once_private(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let (tx, rx) = mpsc::unbounded();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        push(
            &self.queue,
            &self.scope,
            Action::Model(
                id,
                request,
                tx,
                uxduck_agent::input::Inputs::default(),
                true,
            ),
        );
        rx.boxed()
    }
}
struct Connection {
    inbox: mpsc::UnboundedReceiver<String>,
    pending: HashMap<u64, Reply<Result<Value, Value>>>,
    next: u64,
}
struct Running {
    inputs: uxduck_agent::input::Inputs,
    scope: Arc<Scope>,
    abort: AbortHandle,
    title: Option<(String, String)>,
}
struct TitleJob {
    scope: Arc<Scope>,
    abort: AbortHandle,
    item: String,
    model: String,
}
struct Stream {
    socket: Option<uxduck_agent::provider::socket::SocketRound>,
    inputs: uxduck_agent::input::Inputs,
    scope: Arc<Scope>,
    tx: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    /// Claude's answer so far; Responses events stand alone.
    answer: Option<anthropic::Answer>,
}
struct Saved {
    account: Account,
    model: String,
    covers: Vec<String>,
    context: Option<String>,
    conversation: Vec<Value>,
    context_sent: bool,
    response: Option<String>,
}
#[derive(Default)]
pub struct Runtime {
    queue: Queue,
    tasks: FuturesUnordered<BoxFuture<'static, ()>>,
    running: HashMap<String, Running>,
    titles: HashMap<u64, TitleJob>,
    connections: HashMap<ConnectionId, Connection>,
    streams: HashMap<u64, Stream>,
    saved: HashMap<String, Saved>,
    next: Arc<AtomicU64>,
    now: Arc<AtomicU64>,
}
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("running", &self.running.len())
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}
impl Runtime {
    #[allow(
        clippy::too_many_lines,
        reason = "a turn from request to first call, in order"
    )]
    pub fn start(&mut self, engine: &mut Engine, request: TurnRequest, account: Account, now: u64) {
        let fail = |engine: &mut Engine, why: &str| {
            engine.agent_error(None, &request.thread_id, why);
            engine.agent_turn_finish(&request.thread_id, request.turn, None);
        };
        if !account.valid() {
            fail(engine, crate::root::NO_ACCOUNT);
            return;
        }
        if request.model.is_empty() {
            fail(
                engine,
                "UXDuck is still checking which AI models your account can use. Send your message again in a moment.",
            );
            return;
        }
        if !account.supports(&request.model) {
            fail(engine, "Choose a supported hosted model for this chat.");
            return;
        }
        let (outbox, inbox) = mpsc::unbounded();
        let connection = engine.connect(Transport::Internal, Trust::LocalUser, outbox);
        self.connections.insert(
            connection,
            Connection {
                inbox,
                pending: HashMap::new(),
                next: 0,
            },
        );
        let scope = Arc::new(Scope {
            thread: request.thread_id.clone(),
            turn: request.turn,
            connection,
            origin: request.connection,
            live: AtomicBool::new(true),
            account,
            share: request.share,
            now: self.now.clone(),
            began: now / 1000,
        });
        let items: Vec<_> = engine
            .state()
            .items_of(&request.thread_id)
            .into_iter()
            .cloned()
            .collect();
        let before = items
            .iter()
            .position(|i| i.id == request.item_id)
            .unwrap_or(items.len());
        let covers: Vec<_> = items[..before]
            .iter()
            .filter(|i| i.in_history())
            .map(|i| i.id.clone())
            .collect();
        let context = request.vehicle.as_ref().map(|(id, name)| format!("This thread is about the vehicle \"{name}\" (id {id}). Use vehicle_get with that id for its details and telemetry_get for where it is and its battery, attitude and GPS."));
        let wire = Wire::for_model(&request.model);
        let kept = self.saved.remove(&request.thread_id).filter(|s| {
            s.account == scope.account && s.model == request.model && s.covers == covers
        });
        let (conversation, context_sent, previous) = match kept {
            Some(s) => (
                s.conversation,
                s.context_sent && s.context == context,
                s.response,
            ),
            None => (
                conversation::rebuild(wire, &entries(&items[..before])),
                false,
                None,
            ),
        };
        let turn = Turn {
            model: request.model.clone(),
            effort: request.effort.clone(),
            text: request.text,
            conversation,
            context,
            context_sent,
            previous,
            cache_key: Some(format!("web-{}", request.thread_id)),
        };
        let inputs = uxduck_agent::input::Inputs::default();
        let host = BrowserHost {
            inputs: inputs.clone(),
            scope: scope.clone(),
            queue: self.queue.clone(),
            mission_reads: Mutex::new(
                uxduck_contracts::dto::mission_read::MissionReadSession::default(),
            ),
        };
        let provider = BrowserProvider {
            wire,
            scope: scope.clone(),
            queue: self.queue.clone(),
            next: self.next.clone(),
        };
        let queue = self.queue.clone();
        let task_scope = scope.clone();
        let (abort, registration) = AbortHandle::new_pair();
        self.running.insert(
            request.thread_id,
            Running {
                inputs,
                scope,
                abort,
                title: request.title.map(|text| (request.item_id, text)),
            },
        );
        self.tasks.push(async move {
            let work = async move {
                let init = host.call("initialize".into(), json!({"clientInfo":{"name":"uxduck-browser-agent","version":env!("CARGO_PKG_VERSION"),"kind":"agent"},"capabilities":{"protocolVersion":PROTOCOL_VERSION,"missionReadFloatBits":true,"replayStepMarks":true}})).await;
                let outcome = if init.is_ok() { run_turn(&provider, &host, turn).await } else {
                    TurnOutcome { result: Err(uxduck_agent::TurnError::Provider(ProviderError::Account("The browser agent could not initialize.".into()))),
                        response_id: None, conversation: Vec::new(), context_sent: false, stats: uxduck_agent::TurnStats::default() }
                };
                push(&queue, &task_scope, Action::Finished(outcome, request.model));
            };
            let _ = Abortable::new(work, registration).await;
        }.boxed());
    }
    pub fn steer(&mut self, thread: &str, turn: u64, id: String, text: String, share: bool) {
        if let Some(run) = self.running.get(thread)
            && run.scope.turn == turn
        {
            run.inputs
                .push(uxduck_agent::input::Input { id, text, share });
        }
    }
    pub fn cancel(&mut self, engine: &mut Engine, thread: &str, outputs: &mut Vec<Output>) {
        let jobs: Vec<_> = self
            .titles
            .iter()
            .filter(|(_, job)| job.scope.thread == thread)
            .map(|(id, _)| *id)
            .collect();
        for id in jobs {
            self.finish_title(
                engine,
                id,
                uxduck_agent::turn::TitleOutcome::default(),
                outputs,
            );
        }
        let Some(run) = self.running.remove(thread) else {
            return;
        };
        run.scope.live.store(false, Ordering::SeqCst);
        run.abort.abort();
        let ids: Vec<_> = self
            .streams
            .iter()
            .filter(|(_, s)| Arc::ptr_eq(&s.scope, &run.scope))
            .map(|(id, _)| *id)
            .collect();
        for request in ids {
            self.streams.remove(&request);
            outputs.push(Output::Abort { request });
        }
        self.connections.remove(&run.scope.connection);
        engine.disconnect(run.scope.connection);
        engine.agent_turn_finish(thread, run.scope.turn, None);
    }
    pub fn cancel_origin(
        &mut self,
        engine: &mut Engine,
        origin: ConnectionId,
        outputs: &mut Vec<Output>,
    ) {
        let jobs: Vec<_> = self
            .titles
            .iter()
            .filter(|(_, job)| job.scope.origin == origin)
            .map(|(id, _)| *id)
            .collect();
        for id in jobs {
            self.finish_title(
                engine,
                id,
                uxduck_agent::turn::TitleOutcome::default(),
                outputs,
            );
        }
        let threads: Vec<_> = self
            .running
            .iter()
            .filter(|(_, r)| r.scope.origin == origin)
            .map(|(thread, _)| thread.clone())
            .collect();
        for thread in threads {
            self.cancel(engine, &thread, outputs);
        }
    }
    pub fn forget_account(&mut self, account: &Account) {
        self.saved.retain(|_, saved| &saved.account != account);
    }
    pub fn event(&mut self, request: u64, data: Option<&str>, error: Option<&str>) {
        let Some(stream) = self.streams.get_mut(&request) else {
            return;
        };
        if !stream.scope.live.load(Ordering::SeqCst) {
            self.streams.remove(&request);
            return;
        }
        if let (Some(socket), Some(data)) = (&mut stream.socket, data) {
            let value = serde_json::from_str::<Value>(data);
            let result = value
                .map_err(|_| ProviderError::Transport("An unreadable model event arrived.".into()))
                .and_then(|value| {
                    if value["type"] == "uxduck.response.done" {
                        socket.settled(&stream.inputs);
                        return if socket.ready {
                            Ok((socket.finish(), Vec::new(), true))
                        } else {
                            Err(ProviderError::Transport(
                                "The response ended before completion.".into(),
                            ))
                        };
                    }
                    socket
                        .event(&value, &stream.inputs)
                        .map(|(events, send)| (events, send, false))
                });
            let done = match result {
                Ok((events, send, done)) => {
                    for event in events {
                        let _ = stream.tx.unbounded_send(Ok(event));
                    }
                    for event in send {
                        push(
                            &self.queue,
                            &stream.scope,
                            Action::SocketSend(request, event),
                        );
                    }
                    done
                }
                Err(error) => {
                    let _ = stream.tx.unbounded_send(Err(error));
                    true
                }
            };
            if done {
                self.streams.remove(&request);
            }
            return;
        }
        let results = if let Some(error) = error {
            vec![Err(ProviderError::Account(
                error.chars().take(1000).collect(),
            ))]
        } else if let Some(data) = data {
            match serde_json::from_str::<Value>(data) {
                Ok(value) => match stream.answer.as_mut() {
                    Some(answer) => anthropic::event(&value, answer).map_or_else(
                        |error| vec![Err(error)],
                        |events| events.into_iter().map(Ok).collect(),
                    ),
                    None => openai::event(&value).transpose().into_iter().collect(),
                },
                Err(_) => vec![Err(ProviderError::Transport(
                    "The server sent an unreadable response.".into(),
                ))],
            }
        } else {
            vec![Err(ProviderError::Transport(
                "The response ended before completion. Try again.".into(),
            ))]
        };
        for result in results {
            let done = matches!(result, Ok(ProviderEvent::Completed { .. }) | Err(_));
            let _ = stream.tx.unbounded_send(result);
            if done {
                self.streams.remove(&request);
                return;
            }
        }
    }
    /// Poll without blocking; the root calls again after every event or engine tick.
    pub fn drive(&mut self, engine: &mut Engine, now: u64, outputs: &mut Vec<Output>) -> bool {
        self.now.store(now / 1000, Ordering::SeqCst);
        let mut progress = false;
        let expired: Vec<_> = self
            .titles
            .iter()
            .filter(|(_, job)| {
                now / 1000 >= job.scope.began.saturating_add(30_000)
                    || !engine.agent_title_requested(&job.scope.thread, &job.item)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.finish_title(
                engine,
                id,
                uxduck_agent::turn::TitleOutcome::default(),
                outputs,
            );
            progress = true;
        }
        for (request, stream) in &mut self.streams {
            if let Some(socket) = &mut stream.socket
                && socket.accepts_input()
            {
                let mut pending = stream.inputs.drain().into_iter();
                if let Some(input) = pending.next() {
                    if !stream.inputs.sharing() {
                        outputs.push(Output::Send {
                            request: *request,
                            event: json!({"type":"uxduck.share","share":false}),
                        });
                    }
                    outputs.push(Output::Send {
                        request: *request,
                        event: socket.steer(input),
                    });
                    progress = true;
                }
                for input in pending {
                    stream.inputs.push(input);
                }
            }
        }
        for connection in self.connections.values_mut() {
            while let Ok(text) = connection.inbox.try_recv() {
                if let Ok(frame) = serde_json::from_str::<Value>(&text)
                    && let Some(reply) = frame["id"]
                        .as_u64()
                        .and_then(|id| connection.pending.remove(&id))
                {
                    let result = frame.get("error").cloned().map_or_else(
                        || Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                        Err,
                    );
                    let _ = reply.send(result);
                    progress = true;
                }
            }
        }
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        while matches!(self.tasks.poll_next_unpin(&mut cx), Poll::Ready(Some(()))) {
            progress = true;
        }
        let actions: Vec<_> = self
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        if !actions.is_empty() {
            progress = true;
        }
        for (scope, action) in actions {
            if !scope.live.load(Ordering::SeqCst) {
                continue;
            }
            self.handle_action(engine, &scope, action, outputs);
        }
        progress
    }
    fn handle_action(
        &mut self,
        engine: &mut Engine,
        scope: &Arc<Scope>,
        action: Action,
        outputs: &mut Vec<Output>,
    ) {
        match action {
            Action::SocketSend(request, event) => outputs.push(Output::Send { request, event }),
            Action::InputApplied(id) => engine.agent_input_applied(&scope.thread, scope.turn, &id),
            Action::MessageStart(reply) => {
                let _ =
                    reply.send(engine.agent_message_start(Some(scope.connection), &scope.thread));
            }
            Action::Delta(item, delta) => engine.agent_text_delta(&item, &delta),
            Action::MessageFinish(item, response) => engine.agent_message_finish(
                Some(scope.connection),
                &item,
                ItemStatus::Completed,
                response,
            ),
            Action::ToolStart(method, params, reply) => {
                let _ = reply.send(engine.agent_tool_start(
                    scope.connection,
                    &scope.thread,
                    &method,
                    params.to_string(),
                ));
            }
            Action::ToolFinish(item, outcome) => engine.agent_tool_finish(
                scope.connection,
                &item,
                match outcome {
                    Ok(result) => ToolOutcome::Result(result.to_string()),
                    Err(error) => ToolOutcome::Error(error.to_string()),
                },
            ),
            Action::Rpc(method, params, reply) => {
                if let Some(conn) = self.connections.get_mut(&scope.connection) {
                    conn.next += 1;
                    let id = conn.next;
                    conn.pending.insert(id, reply);
                    engine.receive(
                        scope.connection,
                        &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                            .to_string(),
                    );
                }
            }
            Action::Model(request, args, tx, inputs, private) => {
                let websocket = !private && args.model.starts_with("gpt-6");
                let share = !private && scope.share && inputs.sharing();
                let (path, mut body, answer) = match Wire::for_model(&args.model) {
                    Wire::OpenAi => ("responses", openai::body(&args), None),
                    Wire::Anthropic => (
                        "messages",
                        anthropic::body(&args),
                        Some(anthropic::Answer::default()),
                    ),
                };
                if private && let Some(body) = body.as_object_mut() {
                    body.remove("fallbacks");
                }
                self.streams.insert(
                    request,
                    Stream {
                        socket: websocket
                            .then(|| uxduck_agent::provider::socket::SocketRound::new(&args)),
                        inputs,
                        scope: scope.clone(),
                        tx,
                        answer,
                    },
                );
                outputs.push(Output::Request {
                    request,
                    account_id: scope.account.account_id.clone().unwrap_or_default(),
                    chat: format!("web-{}", scope.thread),
                    share,
                    continues: args.continues.clone(),
                    path,
                    body,
                    websocket,
                });
            }
            Action::Finished(outcome, model) => self.finish(engine, scope, outcome, model),
            Action::TitleFinished(id, outcome) => self.finish_title(engine, id, outcome, outputs),
        }
    }
    fn finish(
        &mut self,
        engine: &mut Engine,
        scope: &Arc<Scope>,
        outcome: TurnOutcome,
        model: String,
    ) {
        let title = self
            .running
            .get(&scope.thread)
            .filter(|run| Arc::ptr_eq(&run.scope, scope))
            .and_then(|run| run.title.clone());
        let title = title.filter(|_| outcome.result.is_ok());
        if outcome.result.is_ok() {
            let covers = engine
                .state()
                .items_of(&scope.thread)
                .iter()
                .filter(|i| i.in_history())
                .map(|i| i.id.clone())
                .collect();
            let context = engine.state().threads.get(&scope.thread).and_then(|t| t.vehicle_id.as_ref()).and_then(|id| engine.vehicle(id)).map(|v| format!("This thread is about the vehicle \"{}\" (id {}). Use vehicle_get with that id for its details and telemetry_get for where it is and its battery, attitude and GPS.",v.name,v.id));
            self.saved.insert(
                scope.thread.clone(),
                Saved {
                    account: scope.account.clone(),
                    model: model.clone(),
                    covers,
                    context,
                    conversation: outcome.conversation,
                    context_sent: outcome.context_sent,
                    response: outcome.response_id.clone(),
                },
            );
        }
        engine.agent_turn_stats(scope.turn, stats(&outcome.stats));
        if let Err(error) = &outcome.result {
            engine.agent_error(None, &scope.thread, &error.to_string());
        }
        let response = if outcome.result.is_ok() {
            outcome.response_id
        } else {
            None
        };
        engine.agent_turn_finish(&scope.thread, scope.turn, response);
        self.running.remove(&scope.thread);
        scope.live.store(false, Ordering::SeqCst);
        self.connections.remove(&scope.connection);
        engine.disconnect(scope.connection);
        if let Some((item, text)) = title
            && engine.agent_title_requested(&scope.thread, &item)
        {
            self.start_title(scope, item, text, model);
        }
    }
    fn start_title(&mut self, parent: &Scope, item: String, text: String, model: String) {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let scope = Arc::new(Scope {
            thread: parent.thread.clone(),
            turn: parent.turn,
            connection: parent.connection,
            origin: parent.origin,
            live: AtomicBool::new(true),
            account: parent.account.clone(),
            share: false,
            now: self.now.clone(),
            began: self.now.load(Ordering::SeqCst),
        });
        let provider = BrowserProvider {
            wire: Wire::for_model(&model),
            scope: scope.clone(),
            queue: self.queue.clone(),
            next: self.next.clone(),
        };
        let (abort, registration) = AbortHandle::new_pair();
        self.titles.insert(
            id,
            TitleJob {
                scope: scope.clone(),
                abort,
                item,
                model: model.clone(),
            },
        );
        let queue = self.queue.clone();
        self.tasks.push(
            async move {
                let work = async move {
                    let outcome = uxduck_agent::turn::generate_title(
                        &provider,
                        &model,
                        &text,
                        Some(format!("web-{}", scope.thread)),
                    )
                    .await;
                    push(&queue, &scope, Action::TitleFinished(id, outcome));
                };
                let _ = Abortable::new(work, registration).await;
            }
            .boxed(),
        );
    }
    fn finish_title(
        &mut self,
        engine: &mut Engine,
        id: u64,
        outcome: uxduck_agent::turn::TitleOutcome,
        outputs: &mut Vec<Output>,
    ) {
        let Some(job) = self.titles.remove(&id) else {
            return;
        };
        job.scope.live.store(false, Ordering::SeqCst);
        job.abort.abort();
        let requests: Vec<_> = self
            .streams
            .iter()
            .filter(|(_, stream)| Arc::ptr_eq(&stream.scope, &job.scope))
            .map(|(id, _)| *id)
            .collect();
        for request in requests {
            self.streams.remove(&request);
            outputs.push(Output::Abort { request });
        }
        engine.agent_title_finish(&job.scope.thread, &job.item, outcome.title.as_deref());
        engine.agent_title_stats(
            &job.model,
            outcome.title.is_some(),
            self.now
                .load(Ordering::SeqCst)
                .saturating_sub(job.scope.began),
            outcome
                .usage
                .map(|usage| {
                    stats(&uxduck_agent::TurnStats {
                        rounds: 1,
                        usage,
                        ..Default::default()
                    })
                })
                .as_ref(),
        );
    }
}

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
                    .unwrap_or_else(|| "{\"error\":\"cancelled\"}".into()),
                failed: call.result.is_none(),
            }),
            ItemKind::Error => None,
        })
        .collect()
}
fn stats(s: &uxduck_agent::TurnStats) -> TurnStats {
    TurnStats {
        rounds: s.rounds,
        tool_errors: s.tool_errors,
        input_tokens: s.usage.input_tokens,
        output_tokens: s.usage.output_tokens,
        cache_read_tokens: s.usage.cache_read_tokens,
        cache_write_tokens: s.usage.cache_write_tokens,
        first_token_ms: s.first_token_ms,
        retries: s.retries,
        fell_back: s.fell_back,
        compacted: s.compacted,
        thinking_dropped: s.usage.thinking_dropped,
    }
}

#[cfg(test)]
mod mission_read_tests {
    #![allow(clippy::unwrap_used, reason = "in-memory browser host fixtures")]
    use super::*;
    use uxduck_engine::runtime::{Clock, Forget, Ports, Random};

    struct Tick;
    impl Clock for Tick {
        fn now_us(&self) -> u64 {
            1_000
        }
    }
    struct Counter(u8);
    impl Random for Counter {
        fn fill(&mut self, bytes: &mut [u8]) {
            for byte in bytes {
                self.0 = self.0.wrapping_add(37);
                *byte = self.0;
            }
        }
    }

    fn host() -> BrowserHost {
        let mut engine = Engine::new(
            uxduck_engine::EngineConfig {
                daemon_version: "0.13.0".into(),
                capabilities: uxduck_contracts::dto::initialize::DaemonCapabilities::default(),
                ui_url: "http://127.0.0.1:45000".into(),
                checkpoint_every: 100,
                models: vec![],
                agent_provider: "fake".into(),
                about: uxduck_engine::HostAbout::default(),
            },
            Box::new(Tick),
            Box::new(Counter(0)),
            Ports::with_persistence(Box::new(Forget)),
            uxduck_engine::Loaded::default(),
        );
        let (outbox, _inbox) = mpsc::unbounded();
        let connection = engine.connect(Transport::Internal, Trust::LocalUser, outbox);
        BrowserHost {
            inputs: uxduck_agent::input::Inputs::default(),
            queue: Arc::default(),
            mission_reads: Mutex::new(
                uxduck_contracts::dto::mission_read::MissionReadSession::default(),
            ),
            scope: Arc::new(Scope {
                thread: "test-thread".into(),
                turn: 1,
                connection,
                origin: connection,
                live: AtomicBool::new(true),
                account: Account::default(),
                share: false,
                now: Arc::new(AtomicU64::new(0)),
                began: 0,
            }),
        }
    }

    fn answer(host: &BrowserHost, method: &str, result: Value) {
        let mut queue = host.queue.lock().unwrap();
        let index = queue
            .iter()
            .position(|(_, action)| matches!(action, Action::Rpc(name, _, _) if name == method))
            .unwrap();
        let (_, Action::Rpc(_, _, reply)) = queue.remove(index) else {
            panic!("expected RPC action")
        };
        reply.send(Ok(result)).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines, reason = "one browser host protocol scenario")]
    fn mission_read_host_preserves_bits_and_latches_pending_and_presend_quarantine() {
        let host = host();
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
        let good = host.call("mission/refresh".into(), json!({ "id": "v" }));
        answer(&host, "mission/refresh", golden("mission/refresh"));
        let result = good.now_or_never().unwrap().unwrap();
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
                    result[part][1][field].as_f64().unwrap().to_bits(),
                    (-0.0_f64).to_bits()
                );
                assert_eq!(result[part][3][field]["nonFiniteF64"], "7ff8000000000001");
            }
        }
        let late = host.call("mission/history".into(), json!({ "id": "v" }));
        let bad = host.call("mission/refresh".into(), json!({ "id": "v" }));
        let mut broken = golden("mission/refresh");
        broken["items"][0]["param4"] = Value::Null;
        answer(&host, "mission/refresh", broken);
        assert_eq!(
            bad.now_or_never().unwrap().unwrap_err()["data"]["kind"],
            "update-required"
        );
        answer(&host, "mission/history", golden("mission/history"));
        assert_eq!(
            late.now_or_never().unwrap().unwrap_err()["data"]["kind"],
            "update-required"
        );
        for method in [
            "mission/refresh",
            "mission/history",
            "mission/upload",
            "mission/restore",
            "plan/upload",
            "state/subscribe",
        ] {
            let refused = host
                .call(method.into(), json!({ "entities": ["mission"] }))
                .now_or_never()
                .unwrap()
                .unwrap_err();
            assert_eq!(refused["data"]["kind"], "update-required");
            assert!(host.queue.lock().unwrap().is_empty());
        }
        let other = host.call("vehicle/list".into(), json!({}));
        answer(&host, "vehicle/list", json!({ "vehicles": [] }));
        assert_eq!(
            other.now_or_never().unwrap().unwrap(),
            json!({ "vehicles": [] })
        );
        assert!(host.scope.live.load(Ordering::SeqCst));
        assert!(host.queue.lock().unwrap().is_empty());
    }
}

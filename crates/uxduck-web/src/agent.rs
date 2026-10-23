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
use uxduck_agent::provider::openai;
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

/// An account hint from one page. The HTTP transport checks its account id
/// against the current server session before every inference request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Account {
    pub operator_id: Option<String>,
    pub account_id: Option<String>,
    pub revision: Option<String>,
}
impl Account {
    #[must_use]
    pub fn valid(&self) -> bool {
        [&self.operator_id, &self.account_id, &self.revision]
            .into_iter()
            .all(|v| v.as_ref().is_none_or(|v| !v.is_empty() && v.len() <= 128))
            && self.operator_id.is_some()
            && self.account_id.is_some()
            && self.revision.is_some()
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
        body: Value,
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
    Model(
        u64,
        ProviderRequest,
        mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
    ),
    Finished(TurnOutcome, String),
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
    scope: Arc<Scope>,
    queue: Queue,
}
impl Host for BrowserHost {
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
        if matches!(method.as_str(), "layout/open" | "plugin/draft")
            && let Some(fields) = params.as_object_mut()
        {
            fields.insert("threadId".into(), Value::String(self.scope.thread.clone()));
        }
        let (tx, rx) = oneshot::channel();
        push(&self.queue, &self.scope, Action::Rpc(method, params, tx));
        async move {
            rx.await
                .unwrap_or_else(|_| Err(json!({"code":-32603,"message":"The agent stopped."})))
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
    scope: Arc<Scope>,
    queue: Queue,
    next: Arc<AtomicU64>,
}
impl Provider for BrowserProvider {
    fn wire(&self) -> Wire {
        Wire::OpenAi
    }
    fn respond(
        &self,
        request: ProviderRequest,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let (tx, rx) = mpsc::unbounded();
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        push(&self.queue, &self.scope, Action::Model(id, request, tx));
        rx.boxed()
    }
}
struct Connection {
    inbox: mpsc::UnboundedReceiver<String>,
    pending: HashMap<u64, Reply<Result<Value, Value>>>,
    next: u64,
}
struct Running {
    scope: Arc<Scope>,
    abort: AbortHandle,
}
struct Stream {
    scope: Arc<Scope>,
    tx: mpsc::UnboundedSender<Result<ProviderEvent, ProviderError>>,
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
    pub fn start(&mut self, engine: &mut Engine, request: TurnRequest, account: Account, now: u64) {
        let fail = |engine: &mut Engine, why: &str| {
            engine.agent_error(None, &request.thread_id, why);
            engine.agent_turn_finish(&request.thread_id, request.turn, None);
        };
        if !account.valid() || account.operator_id != request.operator_id {
            fail(
                engine,
                "Sign in with this operator’s account to use AI. Request access from your account page if needed.",
            );
            return;
        }
        if !["gpt-6-luna", "gpt-6-sol"].contains(&request.model.as_str()) {
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
            .rposition(|i| i.kind == ItemKind::UserMessage)
            .unwrap_or(items.len());
        let covers: Vec<_> = items[..before].iter().map(|i| i.id.clone()).collect();
        let context = request.vehicle.as_ref().map(|(id, name)| format!("This thread is about the vehicle \"{name}\" (id {id}). Use vehicle_get with that id for its details and telemetry_get for where it is and its battery, attitude and GPS."));
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
                conversation::rebuild(Wire::OpenAi, &entries(&items[..before])),
                false,
                None,
            ),
        };
        let turn = Turn {
            model: request.model.clone(),
            text: request.text,
            conversation,
            context,
            context_sent,
            previous,
            cache_key: Some(format!("web-{}", request.thread_id)),
        };
        let host = BrowserHost {
            scope: scope.clone(),
            queue: self.queue.clone(),
        };
        let provider = BrowserProvider {
            scope: scope.clone(),
            queue: self.queue.clone(),
            next: self.next.clone(),
        };
        let queue = self.queue.clone();
        let task_scope = scope.clone();
        let (abort, registration) = AbortHandle::new_pair();
        self.running
            .insert(request.thread_id, Running { scope, abort });
        self.tasks.push(async move {
            let work = async move {
                let init = host.call("initialize".into(), json!({"clientInfo":{"name":"uxduck-browser-agent","version":env!("CARGO_PKG_VERSION"),"kind":"agent"},"capabilities":{"protocolVersion":PROTOCOL_VERSION}})).await;
                let outcome = if init.is_ok() { run_turn(&provider, &host, turn).await } else {
                    TurnOutcome { result: Err(uxduck_agent::TurnError::Provider(ProviderError::Account("The browser agent could not initialize.".into()))),
                        response_id: None, conversation: Vec::new(), context_sent: false, stats: uxduck_agent::TurnStats::default() }
                };
                push(&queue, &task_scope, Action::Finished(outcome, request.model));
            };
            let _ = Abortable::new(work, registration).await;
        }.boxed());
    }
    pub fn cancel(&mut self, engine: &mut Engine, thread: &str, outputs: &mut Vec<Output>) {
        let Some(run) = self.running.remove(thread) else {
            return;
        };
        run.scope.live.store(false, Ordering::SeqCst);
        run.abort.abort();
        let ids: Vec<_> = self
            .streams
            .iter()
            .filter(|(_, s)| s.scope.thread == thread)
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
        let Some(stream) = self.streams.get(&request) else {
            return;
        };
        if !stream.scope.live.load(Ordering::SeqCst) {
            self.streams.remove(&request);
            return;
        }
        let result = if let Some(error) = error {
            Some(Err(ProviderError::Account(
                error.chars().take(1000).collect(),
            )))
        } else if let Some(data) = data {
            match serde_json::from_str::<Value>(data) {
                Ok(value) => openai::event(&value).transpose(),
                Err(_) => Some(Err(ProviderError::Transport(
                    "The server sent an unreadable response.".into(),
                ))),
            }
        } else {
            Some(Err(ProviderError::Transport(
                "The response ended before completion. Try again.".into(),
            )))
        };
        if let Some(result) = result {
            let done = matches!(result, Ok(ProviderEvent::Completed { .. }) | Err(_));
            let _ = stream.tx.unbounded_send(result);
            if done {
                self.streams.remove(&request);
            }
        }
    }
    /// Poll without blocking; the root calls again after every event or engine tick.
    pub fn drive(&mut self, engine: &mut Engine, now: u64, outputs: &mut Vec<Output>) -> bool {
        self.now.store(now / 1000, Ordering::SeqCst);
        let mut progress = false;
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
            Action::Model(request, args, tx) => {
                self.streams.insert(
                    request,
                    Stream {
                        scope: scope.clone(),
                        tx,
                    },
                );
                outputs.push(Output::Request {
                    request,
                    account_id: scope.account.account_id.clone().unwrap_or_default(),
                    chat: format!("web-{}", scope.thread),
                    share: scope.share,
                    continues: args.continues.clone(),
                    body: openai::body(&args),
                });
            }
            Action::Finished(outcome, model) => self.finish(engine, scope, outcome, model),
        }
    }
    fn finish(&mut self, engine: &mut Engine, scope: &Scope, outcome: TurnOutcome, model: String) {
        if outcome.result.is_ok() {
            let covers = engine
                .state()
                .items_of(&scope.thread)
                .iter()
                .map(|i| i.id.clone())
                .collect();
            let context = engine.state().threads.get(&scope.thread).and_then(|t| t.vehicle_id.as_ref()).and_then(|id| engine.vehicle(id)).map(|v| format!("This thread is about the vehicle \"{}\" (id {}). Use vehicle_get with that id for its details and telemetry_get for where it is and its battery, attitude and GPS.",v.name,v.id));
            self.saved.insert(
                scope.thread.clone(),
                Saved {
                    account: scope.account.clone(),
                    model,
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
    }
}
fn entries(items: &[Item]) -> Vec<Entry> {
    items
        .iter()
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

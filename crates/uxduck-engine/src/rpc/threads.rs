//! Threads and the agent: create, send, update, cancel.

use serde_json::{Value, json};
use uxduck_contracts::dto::thread::{
    AgentConfig, BulkAction, DeleteParams, Item as ItemDto, Thread as ThreadDto, ThreadBulkParams,
    ThreadBulkResult, ThreadCreateParams, ThreadSendParams, ThreadUpdateParams,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::thread::{self, ItemKind, ItemStatus, Thread};
use uxduck_core::events::Change;

use super::handlers::to_value;
use crate::runtime::TurnRequest;
use crate::{ACCOUNT_TOKEN, ANTHROPIC_KEY, ConnectionId, Engine, OPENAI_KEY};

/// The thread, agent, vehicle and credential methods.
pub fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::AgentConfig(_) => {
            require(Capability::Read)?;
            to_value(&agent_config(engine))
        }
        ClientRequest::ThreadCreate(params) => {
            require(Capability::Control)?;
            create(engine, connection, params)
        }
        ClientRequest::ThreadSend(params) => {
            require(Capability::Control)?;
            send(engine, connection, &params)
        }
        ClientRequest::ThreadUpdate(params) => {
            require(Capability::Control)?;
            update(engine, connection, params)
        }
        ClientRequest::ThreadCancel(params) => {
            require(Capability::Control)?;
            own_thread(engine, connection, &params.thread_id)?;
            cancel(engine, &params.thread_id)
        }
        ClientRequest::ThreadDelete(params) => {
            require(Capability::Control)?;
            delete(engine, connection, &params)
        }
        ClientRequest::ThreadBulk(params) => {
            require(Capability::Control)?;
            bulk(engine, connection, params)
        }
        ClientRequest::FolderCreate(_)
        | ClientRequest::FolderUpdate(_)
        | ClientRequest::FolderDelete(_) => {
            require(Capability::Control)?;
            super::folders::handle(engine, connection, request)
        }
        ClientRequest::CredentialsSet(params) => {
            let local = engine
                .connections
                .get(&connection)
                .is_some_and(|c| c.transport == Transport::Socket);
            if !operator || !local {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "secrets are only set by an operator over the local socket",
                ));
            }
            engine
                .credentials
                .set(&params.name, &params.value)
                .map_err(|message| ApiError::new(ErrorKind::Internal, message))?;
            if matches!(
                params.name.as_str(),
                OPENAI_KEY | ANTHROPIC_KEY | ACCOUNT_TOKEN
            ) {
                config_changed(engine);
            }
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a thread method")),
    }
}

fn config_changed(engine: &mut Engine) {
    let frame = super::router::notification(&ServerNotification::AgentConfigChanged(
        uxduck_contracts::dto::Empty {},
    ));
    let readers: Vec<_> = engine
        .connections
        .iter()
        .filter(|(_, connection)| {
            connection
                .admitted
                .as_ref()
                .is_some_and(|admitted| admitted.granted.contains(&Capability::Read))
        })
        .map(|(id, _)| *id)
        .collect();
    for connection in readers {
        engine.send(connection, &frame);
    }
}

/// Whether a connection may see a chat or folder started by `owner`
/// (maintainer, 2026-10-23): each operator's chats are their own, like a
/// browser profile's; one started with no operator is everyone's. Other
/// connections are told nothing about the rest, not even that they exist.
pub(crate) fn sees(engine: &Engine, connection: ConnectionId, owner: Option<&str>) -> bool {
    owner.is_none_or(|owner| {
        engine
            .connections
            .get(&connection)
            .and_then(|c| c.operator_id.as_deref())
            == Some(owner)
    })
}

/// Whether a connection may see the chat a thread id names.
pub(crate) fn sees_thread(engine: &Engine, connection: ConnectionId, thread_id: &str) -> bool {
    engine
        .state
        .threads
        .get(thread_id)
        .is_some_and(|t| sees(engine, connection, t.operator_id.as_deref()))
}

/// The threads a connection may see, most recently updated first.
pub fn threads(engine: &Engine, connection: ConnectionId) -> Vec<ThreadDto> {
    let mut threads: Vec<ThreadDto> = engine
        .state
        .threads
        .values()
        .filter(|t| sees(engine, connection, t.operator_id.as_deref()))
        .map(ThreadDto::from)
        .collect();
    threads.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at_us));
    threads
}

/// The items of the threads a connection may see, by thread and position,
/// with streamed text merged in.
pub fn items(engine: &Engine, connection: ConnectionId) -> Vec<ItemDto> {
    let mut items: Vec<ItemDto> = engine
        .state
        .items
        .values()
        .filter(|item| sees_thread(engine, connection, &item.thread_id))
        .map(|item| {
            let mut dto = ItemDto::from(item);
            if let Some(text) = engine.streaming.get(&item.id) {
                dto.text.clone_from(text);
            }
            dto
        })
        .collect();
    items.sort_by(|a, b| (&a.thread_id, a.index).cmp(&(&b.thread_id, b.index)));
    items
}

/// Which provider answers, and whether it can. `auto` (development builds)
/// is the developer's own key when one is set, else the hosted model; the
/// hosted model needs a signed-in UXDuck (S29). Keys are per company, so
/// in `auto` a model with no key of its own goes to the hosted model, as
/// the daemon sends it.
pub fn agent_config(engine: &Engine) -> AgentConfig {
    let has = |name: &str| engine.credentials.has(name);
    let own_key = has(OPENAI_KEY) || has(ANTHROPIC_KEY);
    let signed_in = has(ACCOUNT_TOKEN);
    let configured = engine.config.agent_provider.as_str();
    let provider = match configured {
        "auto" if own_key => "openai",
        "auto" => "hosted",
        other => other,
    }
    .to_owned();
    let key_for = |model: &str| {
        if model.starts_with("claude-") {
            has(ANTHROPIC_KEY)
        } else {
            has(OPENAI_KEY)
        }
    };
    let ready_models: Vec<String> = engine
        .config
        .models
        .iter()
        .filter(|model| match configured {
            "fake" => true,
            "openai" => key_for(model),
            "auto" => key_for(model) || signed_in,
            _ => signed_in,
        })
        .cloned()
        .collect();
    let default_model = default_model(engine);
    AgentConfig {
        models: engine.config.models.clone(),
        ready: configured == "fake" || ready_models.contains(&default_model),
        default_model,
        ready_models,
        provider,
    }
}

fn default_model(engine: &Engine) -> String {
    engine.config.models.first().cloned().unwrap_or_default()
}

/// A vehicle scope: the vehicle's current id (resolving an alias), or none.
fn scope(engine: &Engine, id: Option<&str>) -> Result<Option<String>, ApiError> {
    id.map(|id| {
        engine
            .vehicle(id)
            .map(|v| v.id.clone())
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))
    })
    .transpose()
}

/// A live thread this connection may see; another operator's is as good as gone.
fn own_thread(engine: &Engine, connection: ConnectionId, id: &str) -> Result<Thread, ApiError> {
    thread(engine, id).and_then(|found| {
        if sees(engine, connection, found.operator_id.as_deref()) {
            Ok(found)
        } else {
            Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no thread {id}"),
            ))
        }
    })
}

/// A live thread; a tombstone is as good as gone.
fn thread(engine: &Engine, id: &str) -> Result<Thread, ApiError> {
    engine
        .state
        .threads
        .get(id)
        .filter(|thread| thread.deleted_at_us.is_none())
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no thread {id}")))
}

/// Live threads in a folder (or unfiled), by position, as `(id, key)`.
pub(super) fn thread_siblings(
    engine: &Engine,
    folder: Option<&str>,
    except: &str,
) -> Vec<(String, String)> {
    let mut siblings: Vec<(String, String)> = engine
        .state
        .threads
        .values()
        .filter(|t| t.deleted_at_us.is_none() && t.folder_id.as_deref() == folder && t.id != except)
        .map(|t| (t.id.clone(), t.sort_key.clone()))
        .collect();
    siblings.sort_by(|a, b| a.1.cmp(&b.1));
    siblings
}

fn model(engine: &Engine, requested: Option<String>) -> Result<String, ApiError> {
    match requested {
        None => Ok(default_model(engine)),
        Some(model) if engine.config.models.contains(&model) => Ok(model),
        Some(model) => Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("unknown model {model}"),
        )),
    }
}

pub fn create(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadCreateParams,
) -> Result<Value, ApiError> {
    let now = engine.clock.now_us();
    let actor = engine.actor(connection);
    let created = Thread {
        id: format!("th{}", &engine.random.token()[..16]),
        title: params
            .title
            .as_deref()
            .map_or_else(|| "New chat".to_owned(), thread::title_from),
        operator_id: actor.operator_id.clone(),
        vehicle_id: scope(engine, params.vehicle_id.as_deref())?,
        model: model(engine, params.model)?,
        auto_approve: false,
        last_response_id: None,
        created_at_us: now,
        updated_at_us: now,
        version: 1,
        folder_id: None,
        sort_key: super::folders::place(&thread_siblings(engine, None, ""), None)?,
        pinned: false,
        archived: false,
        exclude_from_learning: false,
        deleted_at_us: None,
    };
    let result = ThreadDto::from(&created);
    engine.commit(connection, Change::ThreadUpserted { thread: created });
    to_value(&result)
}

/// Adds the user's message and hands the turn to the agent host.
pub fn send(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &ThreadSendParams,
) -> Result<Value, ApiError> {
    let found = own_thread(engine, connection, &params.thread_id)?;
    let text = params.text.trim().to_owned();
    if text.is_empty() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "the message is empty",
        ));
    }
    let attachments: Vec<thread::Attachment> = params.attachments.iter().map(Into::into).collect();
    if attachments.len() > thread::MAX_ATTACHMENTS
        || !attachments.iter().all(thread::Attachment::fits)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "at most {} attachments, each with a title and short fields",
                thread::MAX_ATTACHMENTS
            ),
        ));
    }
    if engine.turns.contains_key(&found.id) {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "the agent is still answering in this thread",
        ));
    }
    // A thread named by its first message.
    if found.title == "New chat" && engine.state.items_of(&found.id).is_empty() {
        let mut renamed = found.clone();
        renamed.title = thread::title_from(&text);
        renamed.version += 1;
        engine.commit(connection, Change::ThreadUpserted { thread: renamed });
    }
    let actor = engine.actor(connection);
    let for_agent = thread::for_agent(&text, &attachments);
    let item_id = engine.add_message(
        actor,
        &found.id,
        ItemKind::UserMessage,
        ItemStatus::Completed,
        text,
        None,
        attachments,
    );
    let item = engine.state.items.get(&item_id).map(ItemDto::from);

    engine.next_turn += 1;
    let turn = engine.next_turn;
    engine.turns.insert(found.id.clone(), turn);
    engine.turn_began(turn);
    let current = thread(engine, &found.id)?;
    let vehicle = current
        .vehicle_id
        .as_deref()
        .and_then(|id| engine.vehicle(id))
        .map(|v| (v.id.clone(), v.name.clone()));
    engine.agent.start_turn(TurnRequest {
        connection,
        operator_id: engine.actor(connection).operator_id,
        turn,
        vehicle,
        thread_id: current.id,
        text: for_agent,
        share: engine.state.settings.share_conversations && !current.exclude_from_learning,
        model: current.model,
        previous_response_id: current.last_response_id,
    });
    to_value(&item)
}

pub fn update(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadUpdateParams,
) -> Result<Value, ApiError> {
    let mut found = own_thread(engine, connection, &params.id)?;
    if found.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!(
                "thread {} is at version {}, not {}",
                found.id, found.version, params.expected_version
            ),
        );
        error.data.current_version = Some(found.version);
        return Err(error);
    }
    if let Some(title) = &params.title {
        let title = title.trim();
        if title.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "a title cannot be empty",
            ));
        }
        found.title = thread::title_from(title);
    }
    if params.model.is_some() {
        found.model = model(engine, params.model)?;
    }
    if let Some(vehicle) = &params.vehicle_id {
        found.vehicle_id = scope(engine, vehicle.as_deref())?;
    }
    let moved = match &params.folder_id {
        Some(folder) => {
            super::folders::live_folder(engine, connection, folder.as_deref())?;
            *folder != found.folder_id
        }
        None => false,
    };
    if let Some(folder) = params.folder_id {
        found.folder_id = folder;
    }
    if moved || params.before_id.is_some() {
        let siblings = thread_siblings(engine, found.folder_id.as_deref(), &found.id);
        found.sort_key = super::folders::place(&siblings, params.before_id.as_deref())?;
    }
    if let Some(pinned) = params.pinned {
        found.pinned = pinned;
    }
    if let Some(exclude) = params.exclude_from_learning {
        found.exclude_from_learning = exclude;
    }
    if let Some(archived) = params.archived {
        found.archived = archived;
    }
    found.version += 1;
    let result = ThreadDto::from(&found);
    engine.commit(connection, Change::ThreadUpserted { thread: found });
    to_value(&result)
}

/// A tombstone: the thread leaves every list, its items and receipts stay,
/// and a turn in progress stops.
pub fn delete(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &DeleteParams,
) -> Result<Value, ApiError> {
    let mut found = own_thread(engine, connection, &params.id)?;
    if found.version != params.expected_version {
        let mut error = ApiError::new(
            ErrorKind::VersionConflict,
            format!("thread {} changed", found.id),
        );
        error.data.current_version = Some(found.version);
        return Err(error);
    }
    cancel(engine, &found.id)?;
    found.deleted_at_us = Some(engine.clock.now_us());
    found.version += 1;
    engine.commit(connection, Change::ThreadUpserted { thread: found });
    Ok(json!({}))
}

/// One action on many threads, each at its current version.
pub fn bulk(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadBulkParams,
) -> Result<Value, ApiError> {
    if let BulkAction::Move { folder_id } = &params.action {
        super::folders::live_folder(engine, connection, folder_id.as_deref())?;
    }
    let mut result = ThreadBulkResult::default();
    for id in params.thread_ids {
        let Ok(mut found) = own_thread(engine, connection, &id) else {
            result.skipped.push(id);
            continue;
        };
        match &params.action {
            BulkAction::Move { folder_id } => {
                if found.folder_id == *folder_id {
                    result.skipped.push(id);
                    continue;
                }
                found.folder_id.clone_from(folder_id);
                let siblings = thread_siblings(engine, folder_id.as_deref(), &found.id);
                found.sort_key = super::folders::place(&siblings, None)?;
            }
            BulkAction::Archive => found.archived = true,
            BulkAction::Unarchive => found.archived = false,
            BulkAction::Pin => found.pinned = true,
            BulkAction::Unpin => found.pinned = false,
            BulkAction::Delete => {
                cancel(engine, &found.id)?;
                found.deleted_at_us = Some(engine.clock.now_us());
            }
        }
        found.version += 1;
        engine.commit(connection, Change::ThreadUpserted { thread: found });
        result.changed.push(id);
    }
    to_value(&result)
}

/// Stops the turn: the host is told, and anything still in progress ends as
/// cancelled at once, so every client sees the stop even if the host is slow.
pub fn cancel(engine: &mut Engine, thread_id: &str) -> Result<Value, ApiError> {
    thread(engine, thread_id)?;
    if let Some(turn) = engine.turns.remove(thread_id) {
        engine.agent.cancel_turn(thread_id);
        engine.end_open_items(thread_id, ItemStatus::Cancelled);
        engine.turn_ended(thread_id, turn, true);
    }
    Ok(json!({}))
}

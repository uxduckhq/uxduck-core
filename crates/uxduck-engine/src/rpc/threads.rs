//! Threads and the agent: create, send, update, cancel.

use serde_json::{Value, json};
use uxduck_contracts::dto::thread::{
    AgentConfig, BulkAction, DeleteParams, FollowUpBehavior, Item as ItemDto, ModelEfforts,
    Thread as ThreadDto, ThreadBulkParams, ThreadBulkResult, ThreadCancelResult,
    ThreadCreateParams, ThreadEditParams, ThreadSendParams, ThreadUpdateParams,
};
use uxduck_contracts::dto::trace::Transport;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::thread::{self, Item, ItemKind, ItemStatus, Thread};
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
        ClientRequest::AgentConnection(params) => {
            manage_connection(engine, connection, operator, params)
        }
        ClientRequest::AgentConfig(_) => {
            require(Capability::Read)?;
            let mut config = agent_config(engine);
            if !operator {
                for c in &mut config.connections {
                    c.login = None;
                }
            }
            to_value(&config)
        }
        ClientRequest::ThreadCreate(params) => {
            require(Capability::Control)?;
            create(engine, connection, params)
        }
        ClientRequest::ThreadSend(params) => {
            require(Capability::Control)?;
            send(engine, connection, &params, None)
        }
        ClientRequest::ThreadEdit(params) => {
            require(Capability::Control)?;
            edit(engine, connection, params)
        }
        ClientRequest::ThreadQueue(params) => {
            require(Capability::Control)?;
            own_thread(engine, connection, &params.thread_id)?;
            super::thread_queue::handle(engine, connection, params)
        }
        ClientRequest::ThreadUpdate(params) => {
            require(Capability::Control)?;
            update(engine, connection, params)
        }
        ClientRequest::ThreadCancel(params) => {
            require(Capability::Control)?;
            own_thread(engine, connection, &params.thread_id)?;
            match (params.item_id.as_deref(), params.take_back) {
                (Some(item_id), take_back) => {
                    cancel_item(engine, connection, &params.thread_id, item_id, take_back)
                }
                (None, true) => stop_and_take_back(engine, connection, &params.thread_id),
                (None, false) => cancel(engine, &params.thread_id),
            }
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

fn manage_connection(
    engine: &mut Engine,
    connection: ConnectionId,
    operator: bool,
    params: uxduck_contracts::dto::thread::AgentConnectionParams,
) -> Result<Value, ApiError> {
    let local = engine.connections.get(&connection).is_some_and(|c| {
        matches!(
            c.transport,
            Transport::Socket | Transport::WebSocket | Transport::Internal
        ) && matches!(
            c.trust,
            uxduck_core::policy::admission::Trust::LocalUser
                | uxduck_core::policy::admission::Trust::PairedBrowser { .. }
        )
    });
    if !operator || !local {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "AI connections can only be managed by a local operator.",
        ));
    }
    engine
        .agent
        .connection(params)
        .map_err(|why| ApiError::new(ErrorKind::InvalidValue, why))?;
    Ok(json!({}))
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
    let hosted_offers = |model: &str| {
        engine
            .hosted_models
            .as_ref()
            .is_some_and(|hosted| hosted.iter().any(|m| m == model))
    };
    let mut models = offered(engine);
    let mut ready_models: Vec<String> = models
        .iter()
        .filter(|model| match configured {
            "fake" => true,
            "openai" => key_for(model),
            "auto" => key_for(model) || (signed_in && hosted_offers(model)),
            _ => signed_in,
        })
        .cloned()
        .collect();
    for connection in &engine.agent_connections {
        for model in &connection.models {
            models.push(model.id.clone());
            if connection.status == uxduck_contracts::dto::thread::ConnectionStatus::Connected {
                ready_models.push(model.id.clone());
            }
        }
    }
    let default_model = default_model(engine);
    let mut efforts: Vec<ModelEfforts> = models
        .iter()
        .filter_map(|model| {
            let (levels, default) = thread::efforts(model)?;
            Some(ModelEfforts {
                model: model.clone(),
                levels: levels.iter().map(|&l| l.to_owned()).collect(),
                default: Some(default.to_owned()),
            })
        })
        .collect();
    for model in engine.agent_connections.iter().flat_map(|c| &c.models) {
        if !model.efforts.is_empty() {
            efforts.push(ModelEfforts {
                model: model.id.clone(),
                levels: model.efforts.clone(),
                default: model
                    .default_effort
                    .clone()
                    .filter(|default| model.efforts.contains(default)),
            });
        }
    }
    AgentConfig {
        spoken_turns: true,
        talk_model: talk_choice(engine).map(|(model, _)| model.to_owned()),
        efforts,
        models,
        connections: engine.agent_connections.clone(),
        ready: configured == "fake" || ready_models.contains(&default_model),
        default_model,
        ready_models,
        provider,
    }
}

/// Whether the hosted model answers every model: then only the server
/// says which models there are (S29).
fn hosted(engine: &Engine) -> bool {
    let own_key = engine.credentials.has(OPENAI_KEY) || engine.credentials.has(ANTHROPIC_KEY);
    match engine.config.agent_provider.as_str() {
        "hosted" => true,
        "auto" => !own_key,
        _ => false,
    }
}

/// The models the selector offers: for the hosted model, only what the
/// server says it offers, and none until it has said, so a model it would
/// refuse is never offered; for a developer's own keys or the fake, the
/// configured ones.
fn offered(engine: &Engine) -> Vec<String> {
    if hosted(engine) {
        engine.hosted_models.clone().unwrap_or_default()
    } else {
        engine.config.models.clone()
    }
}

/// The first model on offer, in the server's order for the hosted model;
/// none while the server has not said.
fn default_model(engine: &Engine) -> String {
    offered(engine).first().cloned().unwrap_or_default()
}

/// The model a message goes to: the chat's, or the default for a chat
/// begun before the server said which models there are, or on a model the
/// server no longer offers (maintainer, 2026-10-31).
fn model_for_turn(engine: &Engine, chat: &str) -> String {
    // Never replace a selected subscription with hosted billing, including after disconnect.
    if chat.starts_with("codex:") || chat.starts_with("claude-code:") {
        return chat.to_owned();
    }
    let known = !hosted(engine) || engine.hosted_models.is_some();
    if chat.is_empty() || (known && !offered(engine).iter().any(|model| model == chat)) {
        default_model(engine)
    } else {
        chat.to_owned()
    }
}

/// The fast models spoken turns run on, the first one offered, with the
/// effort each takes for no reasoning (ADR-0012 V5a). The chat keeps its own
/// model, which typed messages go back to; a subscription chat keeps its
/// own for spoken turns too, and is never moved to hosted billing.
const TALK_MODELS: [(&str, Option<&str>); 2] = [
    ("gpt-6-luna", Some("none")),
    ("deepseek/deepseek-v4.1-flash", None),
];

fn talk_choice(engine: &Engine) -> Option<(&'static str, Option<&'static str>)> {
    let offered = offered(engine);
    TALK_MODELS
        .into_iter()
        .find(|(model, _)| offered.iter().any(|offer| offer == model))
}

fn talk_model(engine: &Engine, chat: &str, effort: Option<&str>) -> (String, Option<String>) {
    match talk_choice(engine) {
        Some((model, effort))
            if !chat.starts_with("codex:") && !chat.starts_with("claude-code:") =>
        {
            (model.to_owned(), effort.map(str::to_owned))
        }
        _ => (chat.to_owned(), effort.map(str::to_owned)),
    }
}

impl Engine {
    /// What the hosted model offers this account, as `/v1/me` said; open
    /// pages are told when it changes.
    pub fn set_hosted_models(&mut self, models: Option<Vec<String>>) {
        if self.hosted_models != models {
            self.hosted_models = models;
            config_changed(self);
        }
    }
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
pub(super) fn own_thread(
    engine: &Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Thread, ApiError> {
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

/// The effort a chat on `model` asks for: one the model takes, or none.
fn effort(
    engine: &Engine,
    model: &str,
    requested: Option<String>,
) -> Result<Option<String>, ApiError> {
    let Some(effort) = requested else {
        return Ok(None);
    };
    let takes = agent_config(engine)
        .efforts
        .iter()
        .any(|m| m.model == model && m.levels.contains(&effort));
    if takes {
        Ok(Some(effort))
    } else {
        Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("{model} does not take reasoning effort {effort}"),
        ))
    }
}

/// Moves a chat to `model`, keeping its effort only if the model takes it.
fn retarget(engine: &Engine, chat: &mut Thread, model: String) {
    chat.effort = effort(engine, &model, chat.effort.take()).unwrap_or(None);
    chat.model = model;
}

fn model(engine: &Engine, requested: Option<String>) -> Result<String, ApiError> {
    match requested {
        None => Ok(default_model(engine)),
        Some(model) if agent_config(engine).models.contains(&model) => Ok(model),
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
    let model = model(engine, params.model)?;
    let created = Thread {
        id: format!("th{}", &engine.random.token()[..16]),
        title_state: if params.title.is_some() {
            thread::TitleState::Fixed
        } else {
            thread::TitleState::Automatic
        },
        effort: effort(engine, &model, params.effort)?,
        title: params
            .title
            .as_deref()
            .map_or_else(|| "New chat".to_owned(), thread::title_from),
        operator_id: actor.operator_id.clone(),
        vehicle_id: scope(engine, params.vehicle_id.as_deref())?,
        model,
        auto_approve: false,
        last_response_id: None,
        created_at_us: now,
        updated_at_us: now,
        version: 1,
        folder_id: None,
        sort_key: super::folders::place(&thread_siblings(engine, None, ""), None)?,
        pinned: false,
        archived: false,
        deleted_at_us: None,
    };
    let result = ThreadDto::from(&created);
    engine.commit(connection, Change::ThreadUpserted { thread: created });
    to_value(&result)
}

/// Sends new words in place of one of the operator's messages in the chat.
fn edit(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadEditParams,
) -> Result<Value, ApiError> {
    let replaced = engine
        .state
        .items
        .get(&params.item_id)
        .filter(|i| {
            i.thread_id == params.thread_id && i.kind == ItemKind::UserMessage && i.in_history()
        })
        .cloned()
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                "That message is no longer in the chat.",
            )
        })?;
    let message = ThreadSendParams {
        thread_id: params.thread_id,
        behavior: FollowUpBehavior::Queue,
        text: params.text,
        attachments: params.attachments,
        spoken: params.spoken,
    };
    send(engine, connection, &message, Some(&replaced))
}

/// Adds the user's message and hands the turn to the agent host; in place
/// of an earlier message of theirs when one is `replaced`.
#[allow(
    clippy::too_many_lines,
    reason = "validation and first-message dispatch stay together"
)]
pub fn send(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &ThreadSendParams,
    replaced: Option<&Item>,
) -> Result<Value, ApiError> {
    let found = own_thread(engine, connection, &params.thread_id)?;
    if params.spoken.is_some()
        && params.behavior == FollowUpBehavior::Steer
        && engine.turns.contains_key(&found.id)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Queue spoken messages so each starts its own turn with command approval.",
        ));
    }
    let text = params.text.trim().to_owned();
    if text.is_empty() || text.len() > 64 * 1024 {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A message must contain text and fit within 64 KB.",
        ));
    }
    let mut attachments: Vec<thread::Attachment> =
        params.attachments.iter().map(Into::into).collect();
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
    if replaced.is_none()
        && engine
            .state
            .items_of(&found.id)
            .iter()
            .filter(|i| matches!(i.status, ItemStatus::Queued | ItemStatus::Steering))
            .count()
            >= 16
    {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "At most 16 follow-ups can wait in a chat.",
        ));
    }
    // Files uploaded whole by this window (S246), named as UXDuck has them.
    engine.take_attached_files(
        connection,
        &found.id,
        replaced.map(|r| r.id.as_str()),
        &mut attachments,
    )?;
    if let Some(replaced) = replaced {
        cancel(engine, &found.id)?;
        take_back(engine, connection, replaced);
    }
    let found = thread(engine, &found.id)?;
    let active = engine.turns.contains_key(&found.id);
    let waiting = active
        || engine
            .state
            .items_of(&found.id)
            .iter()
            .any(|i| i.status == ItemStatus::Queued);
    let status = if replaced.is_some() {
        ItemStatus::InProgress
    } else if active && params.behavior == FollowUpBehavior::Steer {
        ItemStatus::Steering
    } else if waiting {
        ItemStatus::Queued
    } else {
        ItemStatus::InProgress
    };
    let model = model_for_turn(engine, &found.model);
    // A thread named by its first message, and given the default model if
    // it has none or the server dropped its own.
    let named = found.title_state == thread::TitleState::Automatic
        && engine.state.items_of(&found.id).is_empty();
    if named || model != found.model {
        let mut changed = found.clone();
        if named {
            changed.title = thread::title_from(&text);
        }
        retarget(engine, &mut changed, model.clone());
        changed.version += 1;
        engine.commit(connection, Change::ThreadUpserted { thread: changed });
    }
    let actor = engine.actor(connection);
    let for_agent = thread::for_agent(&text, &attachments);
    let item_id = engine.add_message(
        actor,
        &found.id,
        ItemKind::UserMessage,
        status,
        text,
        None,
        attachments,
        params.spoken.map(|spoken| thread::Spoken {
            units: spoken.units.into(),
        }),
    );
    let item = engine.state.items.get(&item_id).map(ItemDto::from);

    if status == ItemStatus::Queued {
        engine.pending_origins.insert(item_id, connection);
    } else if status == ItemStatus::Steering {
        engine.pending_origins.insert(item_id.clone(), connection);
        engine.agent.steer_turn(
            &found.id,
            engine.turns[&found.id],
            &item_id,
            &for_agent,
            engine.state.settings.share_conversations,
        );
    } else {
        start_item(engine, connection, &found.id, &item_id, for_agent)?;
    }
    to_value(&item)
}

pub(super) fn start_item(
    engine: &mut Engine,
    connection: ConnectionId,
    thread_id: &str,
    item_id: &str,
    text: String,
) -> Result<(), ApiError> {
    engine.next_turn += 1;
    let turn = engine.next_turn;
    engine.turns.insert(thread_id.to_owned(), turn);
    engine.turn_began(turn);
    let current = thread(engine, thread_id)?;
    let title = if current.title_state == thread::TitleState::Automatic {
        let first = engine
            .state
            .items_of(thread_id)
            .into_iter()
            .find(|item| item.kind == ItemKind::UserMessage && item.in_history());
        if let Some(first) = first.filter(|item| item.id == item_id) {
            let text = first.text.clone();
            let can_generate = !current.model.starts_with("codex:") && !text.is_empty();
            let mut changed = current.clone();
            changed.title_state = if can_generate {
                thread::TitleState::Requested {
                    item_id: item_id.to_owned(),
                }
            } else {
                thread::TitleState::Fixed
            };
            changed.version += 1;
            engine.commit(connection, Change::ThreadUpserted { thread: changed });
            can_generate.then_some(text)
        } else {
            None
        }
    } else {
        None
    };
    let vehicle = current
        .vehicle_id
        .as_deref()
        .and_then(|id| engine.vehicle(id))
        .map(|v| (v.id.clone(), v.name.clone()));
    let spoken = engine.state.items.get(item_id).and_then(|item| item.spoken);
    let (text, model, effort) = match spoken {
        Some(spoken) => {
            let (model, effort) = talk_model(engine, &current.model, current.effort.as_deref());
            let text = engine.spoken_text(current.vehicle_id.as_deref(), spoken, &text);
            engine.spoken_turns.insert(turn, model.clone());
            (text, model, effort)
        }
        None => (text, current.model, current.effort),
    };
    engine.agent.start_turn(TurnRequest {
        connection,
        operator_id: engine.actor(connection).operator_id,
        turn,
        vehicle,
        thread_id: current.id,
        item_id: item_id.to_owned(),
        text,
        share: engine.state.settings.share_conversations,
        model,
        title,
        effort,
        previous_response_id: current.last_response_id,
    });
    Ok(())
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
        found.title_state = thread::TitleState::Fixed;
    }
    if params.model.is_some() {
        let chosen = model(engine, params.model)?;
        retarget(engine, &mut found, chosen);
    }
    if let Some(chosen) = params.effort {
        found.effort = effort(engine, &found.model, chosen)?;
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

/// How long after the agent began answering a stop still takes the message back.
const QUICK_STOP_US: u64 = 5_000_000;

/// Stops the turn and, when nothing has come of it yet, takes back the
/// message it answers for the operator to change and send again.
fn stop_and_take_back(
    engine: &mut Engine,
    connection: ConnectionId,
    thread_id: &str,
) -> Result<Value, ApiError> {
    let asked = engine
        .state
        .items_of(thread_id)
        .into_iter()
        .rfind(|i| i.kind == ItemKind::UserMessage && i.status == ItemStatus::InProgress)
        .cloned();
    cancel(engine, thread_id)?;
    let now = engine.clock.now_us();
    let quiet = |asked: &Item| {
        engine
            .state
            .items_of(thread_id)
            .into_iter()
            .filter(|i| i.index > asked.index && i.in_history())
            .all(|i| {
                i.kind == ItemKind::AgentMessage
                    && (i.text.trim().is_empty()
                        || now.saturating_sub(i.created_at_us) < QUICK_STOP_US)
            })
    };
    let Some(asked) = asked.filter(|asked| quiet(asked)) else {
        return to_value(&ThreadCancelResult::default());
    };
    take_back(engine, connection, &asked);
    to_value(&ThreadCancelResult {
        taken_back: engine.state.items.get(&asked.id).map(ItemDto::from),
    })
}

/// Takes back an operator's message: it and everything after it leave the
/// chat and the agent's history, kept as a record. Follow-ups still
/// waiting stay, and the provider conversation the thread continued ends.
fn take_back(engine: &mut Engine, connection: ConnectionId, message: &Item) {
    let after: Vec<Item> = engine
        .state
        .items_of(&message.thread_id)
        .into_iter()
        .filter(|i| {
            i.index >= message.index
                && !matches!(
                    i.status,
                    ItemStatus::Queued | ItemStatus::Steering | ItemStatus::Superseded
                )
        })
        .cloned()
        .collect();
    for mut item in after {
        item.status = ItemStatus::Superseded;
        item.version += 1;
        engine.commit(connection, Change::ItemUpserted { item });
    }
    if let Some(mut thread) = engine.state.threads.get(&message.thread_id).cloned() {
        thread.last_response_id = None;
        thread.updated_at_us = engine.clock.now_us();
        thread.version += 1;
        engine.commit(connection, Change::ThreadUpserted { thread });
    }
}

/// Stops the turn: the host is told, and anything still in progress ends as
/// cancelled at once, so every client sees the stop even if the host is slow.
pub fn cancel(engine: &mut Engine, thread_id: &str) -> Result<Value, ApiError> {
    thread(engine, thread_id)?;
    super::thread_queue::hold_steering(engine, thread_id);
    if let Some(turn) = engine.turns.remove(thread_id) {
        engine.agent.cancel_turn(thread_id);
        engine.end_open_items(thread_id, ItemStatus::Cancelled);
        engine.turn_ended(thread_id, turn, true);
    }
    Ok(json!({}))
}

fn cancel_item(
    engine: &mut Engine,
    connection: ConnectionId,
    thread_id: &str,
    item_id: &str,
    take_back: bool,
) -> Result<Value, ApiError> {
    let Some(mut item) = engine
        .state
        .items
        .get(item_id)
        .filter(|item| item.thread_id == thread_id && item.kind == ItemKind::UserMessage)
        .cloned()
    else {
        return Ok(json!({}));
    };
    match item.status {
        ItemStatus::Queued => {
            item.status = ItemStatus::Cancelled;
            item.version += 1;
            engine.pending_origins.remove(item_id);
            engine.commit(connection, Change::ItemUpserted { item });
        }
        ItemStatus::Steering => {
            return Err(ApiError::new(
                ErrorKind::Busy,
                "This input has already reached the agent. Stop the turn to interrupt it.",
            ));
        }
        ItemStatus::InProgress
            if engine.turns.contains_key(thread_id)
                && engine
                    .state
                    .items_of(thread_id)
                    .into_iter()
                    .rfind(|item| {
                        item.kind == ItemKind::UserMessage && item.status == ItemStatus::InProgress
                    })
                    .is_some_and(|item| item.id == item_id) =>
        {
            return if take_back {
                stop_and_take_back(engine, connection, thread_id)
            } else {
                cancel(engine, thread_id)
            };
        }
        _ => {}
    }
    Ok(json!({}))
}

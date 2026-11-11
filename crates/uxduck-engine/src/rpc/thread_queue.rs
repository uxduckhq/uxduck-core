//! Follow-ups are journaled user items. They remain separate from the model's history until used.
use crate::{ConnectionId, Engine};
use serde_json::{Value, json};
use uxduck_contracts::{
    dto::thread::{QueueAction, ThreadQueueParams},
    errors::{ApiError, ErrorKind},
};
use uxduck_core::{
    domain::thread::{self, Item, ItemStatus},
    events::Change,
};

fn queued(engine: &Engine, thread: &str) -> Vec<Item> {
    engine
        .state
        .items_of(thread)
        .into_iter()
        .filter(|i| i.status == ItemStatus::Queued)
        .cloned()
        .collect()
}
fn put(engine: &mut Engine, connection: ConnectionId, mut item: Item) {
    item.version += 1;
    engine.commit(connection, Change::ItemUpserted { item });
}
pub(crate) fn hold_steering(engine: &mut Engine, thread: &str) {
    let items: Vec<_> = engine
        .state
        .items_of(thread)
        .into_iter()
        .filter(|i| i.status == ItemStatus::Steering)
        .cloned()
        .collect();
    for mut item in items {
        item.status = ItemStatus::Queued;
        item.version += 1;
        engine.commit_as(engine.agent_actor(None), Change::ItemUpserted { item });
    }
}
/// Automatic advancement needs the original live operator. A restart or disconnect leaves the queue paused.
pub(crate) fn start_next(engine: &mut Engine, thread: &str, resume: Option<ConnectionId>) {
    if engine.turn_in_progress(thread) {
        return;
    }
    let Some(mut item) = queued(engine, thread).into_iter().next() else {
        return;
    };
    let Some(connection) = resume.or_else(|| engine.pending_origins.get(&item.id).copied()) else {
        return;
    };
    let allowed = engine.connections.get(&connection).is_some_and(|c| {
        c.admitted.as_ref().is_some_and(|a| {
            a.granted
                .contains(&uxduck_core::domain::client::Capability::Control)
        })
    }) && super::threads::own_thread(engine, connection, thread).is_ok();
    if !allowed {
        return;
    }
    if let Some(connection) = resume {
        for pending in queued(engine, thread) {
            engine.pending_origins.insert(pending.id, connection);
        }
    }
    item.status = ItemStatus::InProgress;
    item.index = engine
        .state
        .items_of(thread)
        .iter()
        .map(|i| i.index)
        .max()
        .unwrap_or(0)
        + 1;
    let text = thread::for_agent(&item.text, &item.attachments);
    engine.pending_origins.remove(&item.id);
    put(engine, connection, item.clone());
    if let Err(error) = super::threads::start_item(engine, connection, thread, &item.id, text) {
        engine.agent_error(None, thread, &error.message);
    }
}

pub(super) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadQueueParams,
) -> Result<Value, ApiError> {
    if params.action == QueueAction::Resume {
        if engine.turn_in_progress(&params.thread_id) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                "The agent is already working.",
            ));
        }
        start_next(engine, &params.thread_id, Some(connection));
        return Ok(json!({}));
    }
    let (id, version) = match &params.action {
        QueueAction::Edit {
            item_id,
            expected_version,
            ..
        }
        | QueueAction::Delete {
            item_id,
            expected_version,
        }
        | QueueAction::Up {
            item_id,
            expected_version,
        }
        | QueueAction::Down {
            item_id,
            expected_version,
        }
        | QueueAction::Steer {
            item_id,
            expected_version,
        } => (item_id.clone(), *expected_version),
        QueueAction::Resume => unreachable!(),
    };
    let Some(mut item) = engine
        .state
        .items
        .get(&id)
        .filter(|i| i.thread_id == params.thread_id && i.status == ItemStatus::Queued)
        .cloned()
    else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            "This follow-up has already been sent or removed.",
        ));
    };
    if item.version != version {
        return Err(ApiError::new(
            ErrorKind::VersionConflict,
            "This follow-up changed. Try again.",
        ));
    }
    match params.action {
        QueueAction::Edit {
            text, attachments, ..
        } => edit_item(&mut item, &text, &attachments)?,
        QueueAction::Delete { .. } => {
            item.status = ItemStatus::Cancelled;
            engine.pending_origins.remove(&id);
        }
        QueueAction::Up { .. } | QueueAction::Down { .. } => {
            move_item(
                engine,
                connection,
                &mut item,
                matches!(params.action, QueueAction::Up { .. }),
            )?;
        }
        QueueAction::Steer { .. } => {
            if item.spoken.is_some() {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "Queue spoken messages so each starts its own turn with command approval.",
                ));
            }
            let Some(turn) = engine.turns.get(&params.thread_id).copied() else {
                return Err(ApiError::new(
                    ErrorKind::Busy,
                    "Resume the queue to send this message.",
                ));
            };
            item.status = ItemStatus::Steering;
            engine.pending_origins.insert(item.id.clone(), connection);
            engine.agent.steer_turn(
                &params.thread_id,
                turn,
                &item.id,
                &thread::for_agent(&item.text, &item.attachments),
                engine.state.settings.share_conversations,
            );
        }
        QueueAction::Resume => unreachable!(),
    }
    put(engine, connection, item);
    Ok(json!({}))
}

fn move_item(
    engine: &mut Engine,
    connection: ConnectionId,
    item: &mut Item,
    up: bool,
) -> Result<(), ApiError> {
    let all = queued(engine, &item.thread_id);
    let at = all.iter().position(|i| i.id == item.id).ok_or_else(|| {
        ApiError::new(ErrorKind::NotFound, "The queued message is no longer here.")
    })?;
    let next = if up { at.checked_sub(1) } else { Some(at + 1) };
    if let Some(mut other) = next.and_then(|n| all.get(n)).cloned() {
        std::mem::swap(&mut item.index, &mut other.index);
        put(engine, connection, other);
    }
    Ok(())
}

fn edit_item(
    item: &mut Item,
    text: &str,
    attachments: &[uxduck_contracts::dto::thread::Attachment],
) -> Result<(), ApiError> {
    item.attachments = attachments.iter().map(Into::into).collect();
    if text.trim().is_empty()
        || text.len() > 64 * 1024
        || item.attachments.len() > thread::MAX_ATTACHMENTS
        || !item.attachments.iter().all(thread::Attachment::fits)
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "A nonempty message and at most eight valid attachments are required.",
        ));
    }
    text.trim().clone_into(&mut item.text);
    Ok(())
}

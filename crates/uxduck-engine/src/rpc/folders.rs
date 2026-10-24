//! Folders: nestable, cheap, meaningless beyond the user's organisation
//! (ADR-0009). Deleting one is a tombstone; its threads and folders move up
//! to its parent, so nothing is lost.

use serde_json::{Value, json};
use uxduck_contracts::dto::thread::{
    DeleteParams, Folder as FolderDto, FolderCreateParams, FolderUpdateParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::thread::Folder;
use uxduck_core::events::Change;
use uxduck_core::order;

use super::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The longest folder name, in characters.
const MAX_NAME_CHARS: usize = 80;

pub fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::FolderCreate(params) => create(engine, connection, &params),
        ClientRequest::FolderUpdate(params) => update(engine, connection, params),
        ClientRequest::FolderDelete(params) => delete(engine, connection, &params),
        _ => Err(ApiError::new(ErrorKind::Internal, "not a folder method")),
    }
}

/// The folders a connection may see, tombstones included, by parent and position.
pub fn folders(engine: &Engine, connection: ConnectionId) -> Vec<FolderDto> {
    let mut folders: Vec<FolderDto> = engine
        .state
        .folders
        .values()
        .filter(|f| super::threads::sees(engine, connection, f.operator_id.as_deref()))
        .map(FolderDto::from)
        .collect();
    folders.sort_by(|a, b| (&a.parent_id, &a.sort_key).cmp(&(&b.parent_id, &b.sort_key)));
    folders
}

/// A sort key: directly before `before_id` among `siblings`, or last.
pub(super) fn place(
    siblings: &[(String, String)],
    before_id: Option<&str>,
) -> Result<String, ApiError> {
    let Some(before_id) = before_id else {
        return Ok(order::key_between(
            siblings.last().map(|(_, key)| key.as_str()),
            None,
        ));
    };
    let index = siblings
        .iter()
        .position(|(id, _)| id == before_id)
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                format!("{before_id} is not in the same folder"),
            )
        })?;
    let previous = index.checked_sub(1).map(|i| siblings[i].1.as_str());
    Ok(order::key_between(previous, Some(&siblings[index].1)))
}

/// Checks a folder id refers to a live folder the connection may see;
/// `None` (the top level) always does.
pub(super) fn live_folder(
    engine: &Engine,
    connection: ConnectionId,
    id: Option<&str>,
) -> Result<(), ApiError> {
    match id {
        None => Ok(()),
        Some(id)
            if engine.state.folders.get(id).is_some_and(|f| {
                f.deleted_at_us.is_none()
                    && super::threads::sees(engine, connection, f.operator_id.as_deref())
            }) =>
        {
            Ok(())
        }
        Some(id) => Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no folder {id}"),
        )),
    }
}

fn folder(engine: &Engine, connection: ConnectionId, id: &str) -> Result<Folder, ApiError> {
    engine
        .state
        .folders
        .get(id)
        .filter(|f| {
            f.deleted_at_us.is_none()
                && super::threads::sees(engine, connection, f.operator_id.as_deref())
        })
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no folder {id}")))
}

fn siblings(engine: &Engine, parent: Option<&str>, except: &str) -> Vec<(String, String)> {
    let mut siblings: Vec<(String, String)> = engine
        .state
        .folders
        .values()
        .filter(|f| f.deleted_at_us.is_none() && f.parent_id.as_deref() == parent && f.id != except)
        .map(|f| (f.id.clone(), f.sort_key.clone()))
        .collect();
    siblings.sort_by(|a, b| a.1.cmp(&b.1));
    siblings
}

fn clean_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("a folder name has 1 to {MAX_NAME_CHARS} characters"),
        ));
    }
    Ok(name.to_owned())
}

fn stale(folder: &Folder, expected: u64) -> Result<(), ApiError> {
    if folder.version == expected {
        return Ok(());
    }
    let mut error = ApiError::new(
        ErrorKind::VersionConflict,
        format!("folder {} changed", folder.id),
    );
    error.data.current_version = Some(folder.version);
    Err(error)
}

fn create(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &FolderCreateParams,
) -> Result<Value, ApiError> {
    live_folder(engine, connection, params.parent_id.as_deref())?;
    let created = Folder {
        id: format!("fo{}", &engine.random.token()[..16]),
        name: clean_name(&params.name)?,
        parent_id: params.parent_id.clone(),
        operator_id: engine.actor(connection).operator_id,
        sort_key: place(&siblings(engine, params.parent_id.as_deref(), ""), None)?,
        created_at_us: engine.clock.now_us(),
        version: 1,
        deleted_at_us: None,
    };
    let result = FolderDto::from(&created);
    engine.commit(connection, Change::FolderUpserted { folder: created });
    to_value(&result)
}

/// True if `candidate` is `folder` or inside it: moving `folder` there would
/// make a cycle.
fn within(engine: &Engine, folder: &str, candidate: Option<&str>) -> bool {
    let mut current = candidate.map(str::to_owned);
    let mut steps = 0;
    while let Some(id) = current {
        if id == folder || steps > engine.state.folders.len() {
            return true;
        }
        current = engine
            .state
            .folders
            .get(&id)
            .and_then(|f| f.parent_id.clone());
        steps += 1;
    }
    false
}

fn update(
    engine: &mut Engine,
    connection: ConnectionId,
    params: FolderUpdateParams,
) -> Result<Value, ApiError> {
    let mut found = folder(engine, connection, &params.id)?;
    stale(&found, params.expected_version)?;
    if let Some(name) = &params.name {
        found.name = clean_name(name)?;
    }
    let moved = match &params.parent_id {
        Some(parent) => {
            live_folder(engine, connection, parent.as_deref())?;
            if within(engine, &found.id, parent.as_deref()) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "a folder cannot go inside itself",
                ));
            }
            *parent != found.parent_id
        }
        None => false,
    };
    if let Some(parent) = params.parent_id {
        found.parent_id = parent;
    }
    if moved || params.before_id.is_some() {
        found.sort_key = place(
            &siblings(engine, found.parent_id.as_deref(), &found.id),
            params.before_id.as_deref(),
        )?;
    }
    found.version += 1;
    let result = FolderDto::from(&found);
    engine.commit(connection, Change::FolderUpserted { folder: found });
    to_value(&result)
}

fn delete(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &DeleteParams,
) -> Result<Value, ApiError> {
    let mut found = folder(engine, connection, &params.id)?;
    stale(&found, params.expected_version)?;
    let parent = found.parent_id.clone();

    let child_folders: Vec<Folder> = engine
        .state
        .folders
        .values()
        .filter(|f| f.deleted_at_us.is_none() && f.parent_id.as_deref() == Some(found.id.as_str()))
        .cloned()
        .collect();
    for mut child in child_folders {
        child.parent_id.clone_from(&parent);
        child.sort_key = place(&siblings(engine, parent.as_deref(), &child.id), None)?;
        child.version += 1;
        engine.commit(connection, Change::FolderUpserted { folder: child });
    }
    let threads: Vec<_> = engine
        .state
        .threads
        .values()
        .filter(|t| t.deleted_at_us.is_none() && t.folder_id.as_deref() == Some(found.id.as_str()))
        .cloned()
        .collect();
    for mut thread in threads {
        thread.folder_id.clone_from(&parent);
        thread.sort_key = place(
            &super::threads::thread_siblings(engine, parent.as_deref(), &thread.id),
            None,
        )?;
        thread.version += 1;
        engine.commit(connection, Change::ThreadUpserted { thread });
    }
    found.deleted_at_us = Some(engine.clock.now_us());
    found.version += 1;
    engine.commit(connection, Change::FolderUpserted { folder: found });
    Ok(json!({}))
}

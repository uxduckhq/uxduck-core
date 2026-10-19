//! The SQLite file: control state and projections (ADR-0007). Migrations
//! are forward-only, named by timestamp, and preceded by a backup of the
//! file so a failed upgrade can go back to the previous binary and data.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use uxduck_core::events::{Change, Event};
use uxduck_engine::pairing::Session;
use uxduck_engine::state::State;

/// Every migration, in order. Append only; never edit one that has shipped.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "20260929T1200_initial",
        include_str!("migrations/20260929T1200_initial.sql"),
    ),
    (
        "20260929T1500_threads",
        include_str!("migrations/20260929T1500_threads.sql"),
    ),
    (
        "20260929T1800_folders",
        include_str!("migrations/20260929T1800_folders.sql"),
    ),
    (
        "20260929T2100_vehicles",
        include_str!("migrations/20260929T2100_vehicles.sql"),
    ),
    (
        "20260929T2300_layouts",
        include_str!("migrations/20260929T2300_layouts.sql"),
    ),
    (
        "20260930T0900_receipts",
        include_str!("migrations/20260930T0900_receipts.sql"),
    ),
    (
        "20260930T1200_clients",
        include_str!("migrations/20260930T1200_clients.sql"),
    ),
    (
        "20261001T0900_parameters_missions",
        include_str!("migrations/20261001T0900_parameters_missions.sql"),
    ),
    (
        "20261001T1200_controllers",
        include_str!("migrations/20261001T1200_controllers.sql"),
    ),
    (
        "20261001T1500_plugins",
        include_str!("migrations/20261001T1500_plugins.sql"),
    ),
];

#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("backing up before migrating: {0}")]
    Backup(std::io::Error),
    #[error("the checkpoint does not parse: {0}")]
    Checkpoint(serde_json::Error),
    #[error("this database was written by a newer uxduck (migration {0} is unknown)")]
    Newer(String),
}

/// Opens (or creates) the database and brings it up to date.
///
/// # Errors
/// When the file cannot be opened, backed up or migrated.
pub fn open(path: &Path) -> Result<Connection, SqliteError> {
    open_with(path, MIGRATIONS)
}

/// Where the backup before a migration goes: beside the database, in
/// `backups/before-<migration>/`, holding the database (and so the
/// checkpoint) and the event log as they were. Rolling back is stopping
/// the daemon, putting both back, and starting the previous version.
#[must_use]
pub fn backup_dir(data_dir: &Path, migration: &str) -> std::path::PathBuf {
    data_dir.join("backups").join(format!("before-{migration}"))
}

/// `open`, with a given list of migrations: an older build's, in tests.
fn open_with(path: &Path, migrations: &[(&str, &str)]) -> Result<Connection, SqliteError> {
    let existed = path.exists();
    let mut db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    db.pragma_update(None, "foreign_keys", "ON")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_at_us INTEGER NOT NULL) STRICT;")?;

    let applied: Vec<String> = db
        .prepare("SELECT name FROM schema_migrations ORDER BY name")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if let Some(unknown) = applied
        .iter()
        .find(|name| !migrations.iter().any(|(known, _)| known == name))
    {
        return Err(SqliteError::Newer(unknown.clone()));
    }
    let pending: Vec<&(&str, &str)> = migrations
        .iter()
        .filter(|(name, _)| !applied.iter().any(|a| a == name))
        .collect();
    if pending.is_empty() {
        return Ok(db);
    }
    if existed && !applied.is_empty() {
        db.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        let data_dir = path.parent().unwrap_or(Path::new("."));
        let backup = backup_dir(data_dir, pending[0].0);
        std::fs::create_dir_all(&backup).map_err(SqliteError::Backup)?;
        std::fs::copy(path, backup.join("uxduck.db")).map_err(SqliteError::Backup)?;
        // The log too: an older build cannot read events a newer one wrote.
        let log = data_dir.join("events.ndjson");
        if log.exists() {
            std::fs::copy(&log, backup.join("events.ndjson")).map_err(SqliteError::Backup)?;
        }
        tracing::info!(backup = %backup.display(), "backed up the database and event log before migrating");
    }
    for (name, sql) in pending {
        let tx = db.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (name, applied_at_us) VALUES (?1, ?2)",
            params![name, now_us()],
        )?;
        tx.commit()?;
        tracing::info!(migration = name, "migrated the database");
    }
    Ok(db)
}

/// The last checkpoint, if one was written.
///
/// # Errors
/// When the row cannot be read or parsed.
pub fn load_checkpoint(db: &Connection) -> Result<Option<State>, SqliteError> {
    let text: Option<String> = db
        .query_row("SELECT state FROM checkpoint WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(text) = text else {
        return Ok(None);
    };
    let (state, _, repaired) =
        super::finite::read::<State>(&text).map_err(SqliteError::Checkpoint)?;
    if repaired > 0 {
        tracing::warn!(
            floats = repaired,
            code = "store-non-finite",
            "read NaN or infinite numbers in the checkpoint as 0"
        );
    }
    Ok(Some(state))
}

/// # Errors
/// When the row cannot be written.
pub fn save_checkpoint(db: &Connection, state: &State) -> Result<(), SqliteError> {
    let json = super::finite::write(state, "the checkpoint").map_err(SqliteError::Checkpoint)?;
    db.execute(
        "INSERT INTO checkpoint (id, seq, state, written_at_us) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT (id) DO UPDATE SET seq = excluded.seq, state = excluded.state, written_at_us = excluded.written_at_us",
        params![to_i64(state.seq), json, now_us()],
    )?;
    Ok(())
}

/// # Errors
/// When the rows cannot be read.
pub fn load_sessions(db: &Connection) -> Result<Vec<(String, Session)>, SqliteError> {
    let mut statement = db.prepare("SELECT token_hash, origin FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Session {
                origin: row.get(1)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// # Errors
/// When the row cannot be written.
pub fn save_session(
    db: &Connection,
    token_hash: &str,
    session: &Session,
) -> Result<(), SqliteError> {
    db.execute(
        "INSERT OR REPLACE INTO sessions (token_hash, origin, created_at_us) VALUES (?1, ?2, ?3)",
        params![token_hash, session.origin, now_us()],
    )?;
    Ok(())
}

/// # Errors
/// When the row cannot be deleted.
pub fn delete_session(db: &Connection, token_hash: &str) -> Result<(), SqliteError> {
    db.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        params![token_hash],
    )?;
    Ok(())
}

/// How far the projections have applied the log.
///
/// # Errors
/// When the row cannot be read.
pub fn projected_seq(db: &Connection) -> Result<u64, SqliteError> {
    let seq: i64 = db.query_row("SELECT seq FROM projection WHERE id = 1", [], |row| {
        row.get(0)
    })?;
    Ok(u64::try_from(seq).unwrap_or(0))
}

/// Applies events to the projection tables in one transaction. Events at or
/// below the projected `seq` are skipped, so this is safe to repeat.
///
/// # Errors
/// When a row cannot be written.
#[allow(clippy::too_many_lines, reason = "one arm for each change")]
pub fn project(db: &mut Connection, events: &[Event]) -> Result<(), SqliteError> {
    let done = projected_seq(db)?;
    let tx = db.transaction()?;
    let mut last = done;
    for event in events.iter().filter(|event| event.seq > done) {
        match &event.change {
            Change::OperatorUpserted { operator } => project_operator(&tx, operator, event.seq)?,
            Change::OperatorRemoved { id } => {
                tx.execute("DELETE FROM operators WHERE id = ?1", params![id])?;
            }
            Change::ThreadUpserted { thread } => {
                tx.execute(
                    "INSERT OR REPLACE INTO threads (id, title, operator_id, model, auto_approve, created_at_us,
                       updated_at_us, version, updated_seq, folder_id, sort_key, pinned, archived, deleted_at_us)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        thread.id,
                        thread.title,
                        thread.operator_id,
                        thread.model,
                        thread.auto_approve,
                        to_i64(thread.created_at_us),
                        to_i64(thread.updated_at_us),
                        to_i64(thread.version),
                        to_i64(event.seq),
                        thread.folder_id,
                        thread.sort_key,
                        thread.pinned,
                        thread.archived,
                        thread.deleted_at_us.map(to_i64)
                    ],
                )?;
            }
            Change::FolderUpserted { folder } => {
                tx.execute(
                    "INSERT OR REPLACE INTO folders (id, name, parent_id, operator_id, sort_key, created_at_us,
                       version, deleted_at_us, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        folder.id,
                        folder.name,
                        folder.parent_id,
                        folder.operator_id,
                        folder.sort_key,
                        to_i64(folder.created_at_us),
                        to_i64(folder.version),
                        folder.deleted_at_us.map(to_i64),
                        to_i64(event.seq)
                    ],
                )?;
            }
            change @ (Change::ReceiptUpserted { .. }
            | Change::NotificationUpserted { .. }
            | Change::ClientUpserted { .. }
            | Change::ClientRemoved { .. }
            | Change::ApprovalUpserted { .. }) => {
                project_commands(&tx, change, event.seq)?;
            }
            change @ (Change::SettingsChanged { .. }
            | Change::PlaceUpserted { .. }
            | Change::PlaceRemoved { .. }
            | Change::SnapshotUpserted { .. }
            | Change::SnapshotRemoved { .. }
            | Change::ControllerUpserted { .. }
            | Change::ControllerRemoved { .. }
            | Change::InputMappingUpserted { .. }
            | Change::PluginUpserted { .. }
            | Change::PluginRemoved { .. }) => {
                project_controllers(&tx, change, event.seq)?;
            }
            change @ (Change::ParameterSetUpserted { .. }
            | Change::ParametersReplaced { .. }
            | Change::ParameterChanged { .. }
            | Change::MissionUpserted { .. }) => project_transfers(&tx, change, event.seq)?,
            change @ (Change::LinkUpserted { .. }
            | Change::LinkRemoved { .. }
            | Change::VehicleUpserted { .. }
            | Change::VehicleRemoved { .. }) => project_vehicles(&tx, change, event.seq)?,
            Change::ItemUpserted { item } => project_item(&tx, item, event.seq)?,
            // The checkpoint keeps them; nothing queries them in SQL (S16,
            // S67).
            Change::SyncChanged { .. }
            | Change::EdgeConnection { .. }
            | Change::PracticeAttemptUpserted { .. }
            | Change::FileTransferUpserted { .. }
            | Change::FileTransferRemoved { .. }
            | Change::WatchLinkCreated { .. }
            | Change::WatchLinkRevoked { .. }
            | Change::FlightStored { .. }
            | Change::VideoLost { .. }
            | Change::LibraryUpserted { .. }
            | Change::ParamDefinitionsChosen { .. }
            | Change::OwnSetUpserted { .. }
            | Change::OwnSetRemoved { .. }
            | Change::HistoryUpserted { .. }
            | Change::HistoryCleared { .. }
            | Change::VideoSourceUpserted { .. }
            | Change::VideoSourceRemoved { .. }
            | Change::VideoRecordingUpserted { .. }
            | Change::VideoPhotoUpserted { .. }
            | Change::VideoRecordingRemoved { .. }
            | Change::VideoPhotoRemoved { .. }
            | Change::PluginCalled { .. }
            | Change::PluginPublished { .. }
            | Change::PluginDeclined { .. }
            | Change::PlanSaved { .. }
            | Change::PlanRemoved { .. }
            | Change::MissionRecorded { .. } => {}
        }
        last = event.seq;
    }
    tx.execute(
        "UPDATE projection SET seq = ?1 WHERE id = 1",
        params![to_i64(last)],
    )?;
    tx.commit()?;
    Ok(())
}

fn project_item(
    tx: &rusqlite::Transaction<'_>,
    item: &uxduck_core::domain::thread::Item,
    seq: u64,
) -> Result<(), SqliteError> {
    let tool_call = item
        .tool_call
        .as_ref()
        .and_then(|call| serde_json::to_string(call).ok());
    tx.execute(
            "INSERT OR REPLACE INTO items (id, thread_id, position, kind, status, text, tool_call,
               created_at_us, version, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                item.id,
                item.thread_id,
                to_i64(item.index),
                enum_text(&item.kind),
                enum_text(&item.status),
                item.text,
                tool_call,
                to_i64(item.created_at_us),
                to_i64(item.version),
                to_i64(seq)
            ],
        )?;
    Ok(())
}

fn project_operator(
    tx: &rusqlite::Transaction<'_>,
    operator: &uxduck_core::domain::operator::Operator,
    seq: u64,
) -> Result<(), SqliteError> {
    tx.execute(
        "INSERT INTO operators (id, name, units, coordinate_format, version, updated_seq, layout)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, units = excluded.units,
           coordinate_format = excluded.coordinate_format, version = excluded.version,
           updated_seq = excluded.updated_seq, layout = excluded.layout",
        params![
            operator.id,
            operator.name,
            enum_text(&operator.units),
            enum_text(&operator.coordinate_format),
            to_i64(operator.version),
            to_i64(seq),
            serde_json::to_string(&operator.layout).unwrap_or_default()
        ],
    )?;
    Ok(())
}

/// Receipts and notifications (S9).
fn project_commands(
    tx: &rusqlite::Transaction<'_>,
    change: &Change,
    seq: u64,
) -> Result<(), SqliteError> {
    match change {
        Change::ReceiptUpserted { receipt: r } => {
            tx.execute(
                "INSERT OR REPLACE INTO receipts (id, vehicle_id, command, status, detail,
                   sent_without_recent_heartbeat, forced, attempts, client_id, operator_id,
                   created_at_us, completed_at_us, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    r.id,
                    r.vehicle_id,
                    serde_json::to_string(&r.command).unwrap_or_default(),
                    enum_text(&r.status),
                    r.detail,
                    r.sent_without_recent_heartbeat,
                    r.forced,
                    r.attempts,
                    r.client_id,
                    r.operator_id,
                    to_i64(r.created_at_us),
                    r.completed_at_us.map(to_i64),
                    to_i64(r.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::NotificationUpserted { notification: n } => {
            tx.execute(
                "INSERT OR REPLACE INTO notifications (id, severity, kind, state, subject, title,
                   body, condition, created_at_us, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    n.id,
                    enum_text(&n.severity),
                    enum_text(&n.kind),
                    enum_text(&n.state),
                    serde_json::to_string(&n.subject).unwrap_or_default(),
                    n.title,
                    n.body,
                    n.condition,
                    to_i64(n.created_at_us),
                    to_i64(n.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::ClientUpserted { client: c } => {
            tx.execute(
                "INSERT OR REPLACE INTO clients (id, name, kind, executable, capabilities,
                   created_at_us, version, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    c.id,
                    c.name,
                    enum_text(&c.kind),
                    c.executable,
                    serde_json::to_string(&c.capabilities).unwrap_or_default(),
                    to_i64(c.created_at_us),
                    to_i64(c.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::ClientRemoved { id } => {
            tx.execute("DELETE FROM clients WHERE id = ?1", params![id])?;
        }
        Change::ApprovalUpserted { approval: a } => {
            tx.execute(
                "INSERT OR REPLACE INTO approvals (id, client_id, client_name, capability, method,
                   summary, state, remember, deadline_us, created_at_us, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    a.id,
                    a.client_id,
                    a.client_name,
                    enum_text(&a.capability),
                    a.method,
                    a.summary,
                    enum_text(&a.state),
                    a.remember.as_ref().map(enum_text),
                    to_i64(a.deadline_us),
                    to_i64(a.created_at_us),
                    to_i64(a.version),
                    to_i64(seq)
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Controllers and input mappings (S12), and plugins (S14). Settings and
/// saved places live in the checkpoint and the log; they have no table.
fn project_controllers(
    tx: &rusqlite::Transaction<'_>,
    change: &Change,
    seq: u64,
) -> Result<(), SqliteError> {
    match change {
        Change::PluginUpserted { plugin: p } => {
            tx.execute(
                "INSERT OR REPLACE INTO plugins (id, name, version, path, capabilities, status,
                   detail, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    p.id,
                    p.name,
                    p.version,
                    p.path,
                    serde_json::to_string(&p.capabilities).unwrap_or_default(),
                    enum_text(&p.status),
                    p.detail,
                    to_i64(seq)
                ],
            )?;
        }
        Change::PluginRemoved { id } => {
            tx.execute("DELETE FROM plugins WHERE id = ?1", params![id])?;
        }
        Change::ControllerRemoved { id } => {
            tx.execute("DELETE FROM controllers WHERE id = ?1", params![id])?;
        }
        Change::ControllerUpserted { controller: c } => {
            tx.execute(
                "INSERT OR REPLACE INTO controllers (id, name, connected, claim, mode, target,
                   pending_target, mapping_id, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    c.id,
                    c.name,
                    c.connected,
                    c.claim
                        .as_ref()
                        .and_then(|claim| serde_json::to_string(claim).ok()),
                    enum_text(&c.mode),
                    c.target,
                    c.pending_target,
                    c.mapping_id,
                    to_i64(c.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::InputMappingUpserted { mapping: m } => {
            tx.execute(
                "INSERT OR REPLACE INTO input_mappings (id, name, mapping, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    m.id,
                    m.name,
                    serde_json::to_string(m).unwrap_or_default(),
                    to_i64(m.version),
                    to_i64(seq)
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Parameter sets, parameters and missions (S11).
fn project_transfers(
    tx: &rusqlite::Transaction<'_>,
    change: &Change,
    seq: u64,
) -> Result<(), SqliteError> {
    let set_row = |set: &uxduck_core::domain::parameter::ParameterSet| {
        tx.execute(
            "INSERT OR REPLACE INTO parameter_sets (vehicle_id, status, detail, count,
               boot_instant_us, synced_at_us, updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                set.vehicle_id,
                enum_text(&set.status),
                set.detail,
                set.count,
                set.boot_instant_us.map(to_i64),
                set.synced_at_us.map(to_i64),
                to_i64(seq)
            ],
        )
    };
    let parameter_row = |p: &uxduck_core::domain::parameter::Parameter| {
        tx.execute(
            "INSERT OR REPLACE INTO parameters (vehicle_id, name, value, kind, updated_seq)
               VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                p.vehicle_id,
                p.name,
                p.value,
                enum_text(&p.kind),
                to_i64(seq)
            ],
        )
    };
    match change {
        Change::ParameterSetUpserted { set } => {
            set_row(set)?;
        }
        Change::ParametersReplaced {
            set,
            parameters,
            removed,
        } => {
            set_row(set)?;
            for name in removed {
                tx.execute(
                    "DELETE FROM parameters WHERE vehicle_id = ?1 AND name = ?2",
                    params![set.vehicle_id, name],
                )?;
            }
            for parameter in parameters {
                parameter_row(parameter)?;
            }
        }
        Change::ParameterChanged { parameter } => {
            parameter_row(parameter)?;
        }
        Change::MissionUpserted { mission: m } => {
            tx.execute(
                "INSERT OR REPLACE INTO missions (vehicle_id, items, status, detail,
                   boot_instant_us, synced_at_us, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    m.vehicle_id,
                    serde_json::to_string(&m.items).unwrap_or_default(),
                    enum_text(&m.status),
                    m.detail,
                    m.boot_instant_us.map(to_i64),
                    m.synced_at_us.map(to_i64),
                    to_i64(m.version),
                    to_i64(seq)
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Links and vehicles (S5).
fn project_vehicles(
    tx: &rusqlite::Transaction<'_>,
    change: &Change,
    seq: u64,
) -> Result<(), SqliteError> {
    match change {
        Change::LinkUpserted { link } => {
            tx.execute(
                "INSERT OR REPLACE INTO links (id, name, kind, policy, topology, status, created_at_us, version,
                   updated_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    link.id,
                    link.name,
                    serde_json::to_string(&link.kind).unwrap_or_default(),
                    enum_text(&link.policy),
                    enum_text(&link.topology),
                    enum_text(&link.status),
                    to_i64(link.created_at_us),
                    to_i64(link.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::LinkRemoved { id } => {
            tx.execute("DELETE FROM links WHERE id = ?1", params![id])?;
        }
        Change::VehicleUpserted { vehicle } => {
            tx.execute(
                "INSERT OR REPLACE INTO vehicles (id, name, aliases, link_id, peer, system_id, autopilot,
                   vehicle_type, uid, firmware, presence, saved, dismissed, version, updated_seq)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    vehicle.id,
                    vehicle.name,
                    serde_json::to_string(&vehicle.aliases).unwrap_or_default(),
                    vehicle.link_id,
                    vehicle.peer,
                    vehicle.system_id,
                    vehicle.autopilot,
                    vehicle.vehicle_type,
                    vehicle.uid,
                    vehicle.firmware,
                    enum_text(&vehicle.presence),
                    vehicle.saved,
                    vehicle.dismissed,
                    to_i64(vehicle.version),
                    to_i64(seq)
                ],
            )?;
        }
        Change::VehicleRemoved { id } => {
            tx.execute("DELETE FROM vehicles WHERE id = ?1", params![id])?;
        }
        _ => {}
    }
    Ok(())
}

/// Operators as the projection holds them, for tests and later queries.
///
/// # Errors
/// When the rows cannot be read.
#[cfg_attr(not(test), allow(dead_code, reason = "read by queries from S4"))]
pub fn operator_names(db: &Connection) -> Result<Vec<String>, SqliteError> {
    let mut statement = db.prepare("SELECT name FROM operators ORDER BY name")?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn now_us() -> i64 {
    use uxduck_engine::runtime::Clock;
    to_i64(crate::adapters::system::SystemClock.now_us())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_database_is_backed_up_then_migrated_forward() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uxduck.db");
        {
            // A database as S3 left it: the first two migrations only.
            let db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE schema_migrations (name TEXT PRIMARY KEY, applied_at_us INTEGER NOT NULL) STRICT;")
                .unwrap();
            for (name, sql) in &MIGRATIONS[..2] {
                db.execute_batch(sql).unwrap();
                db.execute(
                    "INSERT INTO schema_migrations VALUES (?1, 0)",
                    params![name],
                )
                .unwrap();
            }
            db.execute(
                "INSERT INTO threads VALUES ('t1', 'old', NULL, 'm', 0, 1, 1, 1, 1)",
                [],
            )
            .unwrap();
        }
        let db = open(&path).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let mut statement = db
            .prepare("SELECT name || ':' || applied_at_us FROM schema_migrations")
            .unwrap();
        names.extend(
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(Result::unwrap),
        );
        drop(statement);
        assert!(
            backup_dir(dir.path(), "20260929T1800_folders")
                .join("uxduck.db")
                .exists(),
            "{names:?}"
        );
        let (pinned, sort_key): (bool, String) = db
            .query_row(
                "SELECT pinned, sort_key FROM threads WHERE id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((pinned, sort_key.as_str()), (false, ""));
        let folders: i64 = db
            .query_row("SELECT count(*) FROM folders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(folders, 0);

        // Opening again changes nothing and backs up nothing more.
        drop(db);
        open(&path).unwrap();
        let backups = std::fs::read_dir(dir.path().join("backups")).unwrap();
        assert_eq!(backups.count(), 1);
    }

    #[test]
    fn a_failed_upgrade_rolls_back_to_the_backup_and_the_previous_build() {
        let old: &[(&str, &str)] = &MIGRATIONS[..MIGRATIONS.len() - 1];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uxduck.db");
        let log = "{\"seq\":1}\n";
        {
            let db = open_with(&path, old).unwrap();
            db.execute(
                "INSERT INTO operators (id, name, units, coordinate_format, version, updated_seq)
                 VALUES ('op1', 'Ana', 'metric', 'decimal', 1, 1)",
                [],
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("events.ndjson"), log).unwrap();

        // The new build migrates, keeping the old files.
        drop(open(&path).unwrap());
        let newest = MIGRATIONS[MIGRATIONS.len() - 1].0;
        let backup = backup_dir(dir.path(), newest);
        // The previous build cannot open what the new one wrote...
        assert!(matches!(
            open_with(&path, old),
            Err(SqliteError::Newer(name)) if name == newest
        ));

        // ...but it can open the backup, with its data and its log.
        let rolled = tempfile::tempdir().unwrap();
        std::fs::copy(backup.join("uxduck.db"), rolled.path().join("uxduck.db")).unwrap();
        std::fs::copy(
            backup.join("events.ndjson"),
            rolled.path().join("events.ndjson"),
        )
        .unwrap();
        let db = open_with(&rolled.path().join("uxduck.db"), old).unwrap();
        let name: String = db
            .query_row("SELECT name FROM operators WHERE id = 'op1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "Ana");
        assert_eq!(
            std::fs::read_to_string(rolled.path().join("events.ndjson")).unwrap(),
            log
        );
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uxduck.db");
        drop(open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO schema_migrations VALUES ('29990101T0000_future', 0)",
                [],
            )
            .unwrap();
        assert!(
            matches!(open(&path), Err(SqliteError::Newer(name)) if name == "29990101T0000_future")
        );
    }

    #[test]
    fn threads_and_folders_are_projected_with_their_organisation() {
        use uxduck_core::domain::thread::{Folder, Thread};
        use uxduck_core::events::Actor;

        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir.path().join("uxduck.db")).unwrap();
        let folder = Folder {
            id: "f1".into(),
            name: "Ops".into(),
            parent_id: None,
            operator_id: None,
            sort_key: "V".into(),
            created_at_us: 1,
            version: 1,
            deleted_at_us: None,
        };
        let thread = Thread {
            id: "t1".into(),
            title: "hello".into(),
            operator_id: None,
            vehicle_id: None,
            model: "m".into(),
            auto_approve: false,
            last_response_id: None,
            created_at_us: 1,
            updated_at_us: 2,
            version: 3,
            folder_id: Some("f1".into()),
            sort_key: "W".into(),
            pinned: true,
            archived: false,
            exclude_from_learning: false,
            deleted_at_us: Some(9),
        };
        let events = [
            Event {
                seq: 1,
                ts_us: 1,
                actor: Actor::default(),
                change: Change::FolderUpserted { folder },
            },
            Event {
                seq: 2,
                ts_us: 2,
                actor: Actor::default(),
                change: Change::ThreadUpserted { thread },
            },
        ];
        project(&mut db, &events).unwrap();
        let row: (String, String, bool, Option<i64>) = db
            .query_row(
                "SELECT folder_id, sort_key, pinned, deleted_at_us FROM threads WHERE id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, ("f1".into(), "W".into(), true, Some(9)));
        let name: String = db
            .query_row("SELECT name FROM folders WHERE id = 'f1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "Ops");
        assert_eq!(projected_seq(&db).unwrap(), 2);
    }

    #[test]
    fn a_forgotten_controller_leaves_the_projection() {
        use uxduck_core::domain::controller::{Controller, TargetMode};
        use uxduck_core::events::Actor;

        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir.path().join("uxduck.db")).unwrap();
        let controller = Controller {
            id: "g1".into(),
            name: "Xbox".into(),
            connected: false,
            claim: None,
            mode: TargetMode::FollowsSelection,
            target: None,
            input_pause: None,
            pending_target: None,
            mapping_id: "mode-2".into(),
            version: 1,
        };
        let count = |db: &Connection| -> i64 {
            db.query_row("SELECT count(*) FROM controllers", [], |row| row.get(0))
                .unwrap()
        };
        project(
            &mut db,
            &[Event {
                seq: 1,
                ts_us: 1,
                actor: Actor::default(),
                change: Change::ControllerUpserted { controller },
            }],
        )
        .unwrap();
        assert_eq!(count(&db), 1);
        project(
            &mut db,
            &[Event {
                seq: 2,
                ts_us: 2,
                actor: Actor::default(),
                change: Change::ControllerRemoved { id: "g1".into() },
            }],
        )
        .unwrap();
        assert_eq!(count(&db), 0);
    }
}

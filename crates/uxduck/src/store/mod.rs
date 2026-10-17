//! Persistence for the native daemon: the event log file and SQLite, owned
//! by one writer thread (ADR-0010) behind the engine's `Persistence` port.

pub mod event_log;
pub mod finite;
pub mod sqlite;

use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use uxduck_core::events::Event;
use uxduck_engine::Loaded;
use uxduck_engine::pairing::Session;
use uxduck_engine::runtime::Persistence;
use uxduck_engine::state::{self, State};

use event_log::EventLog;

/// How often buffered log writes are fsynced (ADR-0007: "a few hundred
/// milliseconds").
const FSYNC_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the event log {path}: {source}")]
    Log {
        path: String,
        source: std::io::Error,
    },
    #[error("the database: {0}")]
    Sqlite(#[from] sqlite::SqliteError),
}

enum Command {
    Append(Box<Event>),
    Checkpoint(Box<State>),
    Session(String, Session),
    /// A session forgotten, by its token's hash (S97).
    ForgetSession(String),
    /// Sync everything and end the thread.
    Stop,
}

/// The engine's end: sends work to the store thread and never blocks.
#[derive(Clone, Debug)]
pub struct StoreHandle {
    commands: mpsc::Sender<Command>,
}

impl Persistence for StoreHandle {
    fn append(&mut self, event: &Event) {
        let _ = self.commands.send(Command::Append(Box::new(event.clone())));
    }
    fn checkpoint(&mut self, state: &State) {
        let _ = self
            .commands
            .send(Command::Checkpoint(Box::new(state.clone())));
    }
    fn save_session(&mut self, token_hash: &str, session: &Session) {
        let _ = self
            .commands
            .send(Command::Session(token_hash.to_owned(), session.clone()));
    }
    fn delete_session(&mut self, token_hash: &str) {
        let _ = self
            .commands
            .send(Command::ForgetSession(token_hash.to_owned()));
    }
}

/// The running store: its handle and its thread.
#[derive(Debug)]
pub struct Store {
    pub handle: StoreHandle,
    thread: JoinHandle<()>,
}

impl Store {
    /// Syncs and stops the thread.
    /// Clones of the handle may outlive this (the engine holds one), so the
    /// thread is told to stop rather than waiting for them to drop.
    pub fn close(self) {
        let _ = self.handle.commands.send(Command::Stop);
        let _ = self.thread.join();
    }
}

/// Opens the store in `data_dir`, restores the state (checkpoint plus
/// replay) and starts the writer thread.
///
/// # Errors
/// When the log or the database cannot be opened.
pub fn open(data_dir: &Path) -> Result<(Store, Loaded), StoreError> {
    let log_path = data_dir.join("events.ndjson");
    let log_error = |source| StoreError::Log {
        path: log_path.display().to_string(),
        source,
    };
    let mut db = sqlite::open(&data_dir.join("uxduck.db"))?;
    let (mut log, read) = EventLog::open(&log_path).map_err(log_error)?;
    if read.repaired_floats > 0 {
        tracing::warn!(
            floats = read.repaired_floats,
            code = "store-non-finite",
            "read NaN or infinite numbers in the event log as 0"
        );
    }
    if read.truncated_bytes > 0 {
        tracing::warn!(
            bytes = read.truncated_bytes,
            "removed a torn last line from the event log"
        );
    }

    let checkpoint = sqlite::load_checkpoint(&db)?;
    let checkpoint_seq = checkpoint.as_ref().map_or(0, |c| c.seq);
    let restored = state::restore(checkpoint, read.events.iter().cloned());
    if let Some(gap) = restored.gap {
        // Nothing after a hole can be trusted, and new events must not land
        // after it. Keep the damaged log for inspection and start a new one.
        let damaged = log_path.with_extension(format!("ndjson.damaged-{}", gap.expected));
        drop(log);
        std::fs::rename(&log_path, &damaged).map_err(log_error)?;
        tracing::error!(%gap, damaged = %damaged.display(), code = "event-log-hole", "the event log has a hole; restored up to it");
        log = EventLog::open(&log_path).map_err(log_error)?.0;
    }
    sqlite::project(
        &mut db,
        &read
            .events
            .iter()
            .filter(|e| e.seq <= restored.state.seq)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    tracing::info!(
        seq = restored.state.seq,
        checkpoint = checkpoint_seq,
        replayed = restored.replayed,
        operators = restored.state.operators.len(),
        "restored"
    );
    let loaded = Loaded {
        state: restored.state,
        sessions: sqlite::load_sessions(&db)?,
    };

    let (commands, inbox) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("uxduck-store".into())
        .spawn(move || run(&inbox, &mut log, &mut db))
        .map_err(log_error)?;
    Ok((
        Store {
            handle: StoreHandle { commands },
            thread,
        },
        loaded,
    ))
}

/// The writer loop: take a batch, append it to the log, hand it to the OS,
/// project it, and fsync on the interval. A checkpoint is written only after
/// the log up to it is synced.
fn run(inbox: &mpsc::Receiver<Command>, log: &mut EventLog, db: &mut rusqlite::Connection) {
    let mut last_sync = Instant::now();
    loop {
        // With nothing waiting to be synced there is no deadline, so an
        // idle daemon's writer sleeps until the next event (S24).
        let received = if log.unsynced() {
            inbox.recv_timeout(FSYNC_INTERVAL)
        } else {
            inbox
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        let first = match received {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                crate::wakes::count("store: fsync (5 Hz while writing)");
                report(log.sync(), "syncing the event log");
                last_sync = Instant::now();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut batch = vec![first];
        batch.extend(inbox.try_iter().take(1024));

        let mut events = Vec::new();
        let mut stop = false;
        for command in batch {
            match command {
                Command::Append(event) => {
                    report(log.append(&event), "appending to the event log");
                    events.push(*event);
                }
                Command::Checkpoint(state) => {
                    report(log.sync(), "syncing the event log");
                    report_sqlite(sqlite::save_checkpoint(db, &state), "writing a checkpoint");
                }
                Command::Session(hash, session) => report_sqlite(
                    sqlite::save_session(db, &hash, &session),
                    "saving a session",
                ),
                Command::ForgetSession(hash) => {
                    report_sqlite(sqlite::delete_session(db, &hash), "forgetting a session");
                }
                Command::Stop => stop = true,
            }
        }
        report(log.flush(), "writing the event log");
        if !events.is_empty() {
            report_sqlite(sqlite::project(db, &events), "updating projections");
        }
        if stop || last_sync.elapsed() >= FSYNC_INTERVAL {
            report(log.sync(), "syncing the event log");
            last_sync = Instant::now();
        }
        if stop {
            break;
        }
    }
    report(log.sync(), "syncing the event log");
}

fn report(result: std::io::Result<()>, what: &str) {
    if let Err(error) = result {
        tracing::error!(%error, code = "store-io", "{what}");
    }
}

fn report_sqlite(result: Result<(), sqlite::SqliteError>, what: &str) {
    if let Err(error) = result {
        tracing::error!(%error, code = "store-sqlite", "{what}");
    }
}

#[cfg(test)]
mod tests {
    use uxduck_core::domain::operator::{CoordinateFormat, Operator, UnitSystem};
    use uxduck_core::events::{Actor, Change};

    use super::*;

    fn upsert(seq: u64, name: &str) -> Event {
        Event {
            seq,
            ts_us: seq,
            actor: Actor::default(),
            change: Change::OperatorUpserted {
                operator: Operator {
                    id: format!("op{seq}"),
                    name: name.into(),
                    units: UnitSystem::Metric,
                    coordinate_format: CoordinateFormat::Decimal,
                    altitude_frame: uxduck_core::domain::telemetry::AltitudeFrame::default(),
                    layout: uxduck_core::domain::operator::Layout::initial(),
                    place_search: true,
                    basemap: uxduck_core::domain::operator::Basemap::Imagery,
                    shortcuts: std::collections::BTreeMap::new(),
                    skipped_confirmations: Vec::new(),
                    telemetry_layouts: Vec::new(),
                    vehicle_telemetry: std::collections::BTreeMap::new(),
                    parameter_groups: Vec::new(),
                    inspector_pinned: Vec::new(),
                    control_actions: Vec::new(),
                    version: 1,
                },
            },
        }
    }

    #[test]
    fn restores_from_checkpoint_plus_log_and_keeps_projections_in_step() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (store, loaded) = open(dir.path()).unwrap();
            assert_eq!(loaded.state.seq, 0);
            let mut handle = store.handle.clone();
            let mut state = State::default();
            for (seq, name) in [(1, "Ana"), (2, "Ben"), (3, "Cy")] {
                let event = upsert(seq, name);
                state.apply(&event).unwrap();
                handle.append(&event);
                if seq == 2 {
                    handle.checkpoint(&state);
                }
            }
            handle.save_session(
                "hash",
                &Session {
                    origin: "http://localhost:5173".into(),
                },
            );
            store.close();
        }
        let (store, loaded) = open(dir.path()).unwrap();
        assert_eq!(loaded.state.seq, 3);
        assert_eq!(loaded.state.operators.len(), 3);
        assert_eq!(loaded.sessions.len(), 1);
        store.close();
        let db = sqlite::open(&dir.path().join("uxduck.db")).unwrap();
        assert_eq!(sqlite::operator_names(&db).unwrap(), ["Ana", "Ben", "Cy"]);
        assert_eq!(sqlite::projected_seq(&db).unwrap(), 3);
    }

    fn nan_parameter(seq: u64) -> Event {
        Event {
            seq,
            ts_us: seq,
            actor: Actor::default(),
            change: Change::ParameterChanged {
                parameter: uxduck_core::domain::parameter::Parameter {
                    vehicle_id: "v1".into(),
                    name: "CAL_ACC0_PRIO".into(),
                    value: f64::NAN,
                    kind: uxduck_core::domain::parameter::ParamKind::Int32,
                    default: None,
                },
            },
        }
    }

    fn parameter_value(state: &State) -> f64 {
        state
            .parameters
            .values()
            .next()
            .map_or(f64::NAN, |p| p.value)
    }

    /// A NaN, which JSON writes as `null`, is written as 0, so the event
    /// log keeps everything after it and the checkpoint still loads.
    #[test]
    fn a_nan_is_written_as_0_and_the_store_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (store, _) = open(dir.path()).unwrap();
            let mut handle = store.handle.clone();
            let mut state = State::default();
            for event in [upsert(1, "Ana"), nan_parameter(2), upsert(3, "Ben")] {
                state.apply(&event).unwrap();
                handle.append(&event);
                if event.seq == 2 {
                    handle.checkpoint(&state);
                }
            }
            store.close();
        }
        let log = std::fs::read_to_string(dir.path().join("events.ndjson")).unwrap();
        assert!(log.contains(r#""value":0"#), "{log}");
        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 3);
        assert_eq!(loaded.state.operators.len(), 2);
        assert!(parameter_value(&loaded.state).abs() < f64::EPSILON);
    }

    /// Files from before the guard, with `null` for the NaN in the log and
    /// in the checkpoint: read with 0 there, nothing after it dropped.
    #[test]
    fn an_older_log_and_checkpoint_with_null_for_a_float_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let events = [upsert(1, "Ana"), nan_parameter(2), upsert(3, "Ben")];
        let mut lines = String::new();
        for event in &events {
            lines.push_str(&serde_json::to_string(event).unwrap());
            lines.push('\n');
        }
        assert!(lines.contains(r#""value":null"#));
        std::fs::write(dir.path().join("events.ndjson"), lines).unwrap();
        let mut state = State::default();
        for event in &events[..2] {
            state.apply(event).unwrap();
        }
        let db = sqlite::open(&dir.path().join("uxduck.db")).unwrap();
        db.execute(
            "INSERT INTO checkpoint (id, seq, state, written_at_us) VALUES (1, 2, ?1, 0)",
            [serde_json::to_string(&state).unwrap()],
        )
        .unwrap();
        drop(db);

        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 3);
        assert_eq!(loaded.state.operators.len(), 2);
        assert!(parameter_value(&loaded.state).abs() < f64::EPSILON);
        assert!(!dir.path().join("events.ndjson.damaged-3").exists());
        let log = std::fs::read_to_string(dir.path().join("events.ndjson")).unwrap();
        assert_eq!(log.lines().count(), 3, "nothing truncated");
    }

    #[test]
    fn a_hole_in_the_log_is_set_aside_and_the_sequence_continues_from_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = String::new();
        for event in [upsert(1, "Ana"), upsert(2, "Ben"), upsert(4, "Dee")] {
            lines.push_str(&serde_json::to_string(&event).unwrap());
            lines.push('\n');
        }
        std::fs::write(dir.path().join("events.ndjson"), lines).unwrap();
        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 2);
        assert!(dir.path().join("events.ndjson.damaged-3").exists());
    }
}

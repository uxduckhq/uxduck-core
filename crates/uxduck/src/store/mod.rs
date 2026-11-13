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

/// How long forgetting a session waits for the writer to delete it.
const FORGET_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the event log {path}: {source}")]
    Log {
        path: String,
        source: std::io::Error,
    },
    #[error("the database: {0}")]
    Sqlite(#[from] sqlite::SqliteError),
    #[error(
        "the checkpoint at event {seq} does not parse ({error}), and the event log cannot rebuild it ({log}). Nothing was changed: the previous version of UXDuck still reads it"
    )]
    Checkpoint {
        seq: u64,
        error: serde_json::Error,
        log: String,
    },
    #[error("setting aside the checkpoint that does not parse: {0}")]
    SetAside(std::io::Error),
}

enum Command {
    Append(Box<Event>),
    Checkpoint(Box<State>),
    Session(String, Session),
    /// A session forgotten, by its token's hash (S97), answered once it is
    /// deleted.
    ForgetSession(String, mpsc::Sender<()>),
    /// Sync everything and end the thread.
    Stop,
}

/// The engine's end: sends work to the store thread and never blocks, apart
/// from forgetting a session, which waits for the delete.
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
    /// Waits, up to [`FORGET_WAIT`], until the session is deleted, so a
    /// daemon stopped straight after cannot bring a forgotten phone back.
    fn delete_session(&mut self, token_hash: &str) {
        let (deleted, written) = mpsc::channel();
        if self
            .commands
            .send(Command::ForgetSession(token_hash.to_owned(), deleted))
            .is_ok()
        {
            let _ = written.recv_timeout(FORGET_WAIT);
        }
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

    if let Some(first) = read.unreadable.first() {
        tracing::warn!(
            lines = read.unreadable.len(),
            line = first.line,
            error = %first.error,
            code = "event-log-unreadable",
            "kept lines of the event log this build cannot read"
        );
    }

    let checkpoint = match sqlite::checkpoint_row(&db)? {
        None => None,
        Some((seq, text)) => match sqlite::parse_checkpoint(&text) {
            Ok(state) => Some(state),
            Err(error) => {
                set_aside_checkpoint(data_dir, seq, &text, error, &read)?;
                None
            }
        },
    };
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

/// A checkpoint this build cannot read is rebuilt from the event log when
/// the log holds every event up to it, from the first. Its JSON is kept in
/// `backups/` for a build that can read it. Otherwise nothing is changed
/// and the daemon does not start, rather than start without the operator's
/// world.
fn set_aside_checkpoint(
    data_dir: &Path,
    seq: u64,
    text: &str,
    error: serde_json::Error,
    read: &event_log::Read,
) -> Result<(), StoreError> {
    let complete = read
        .events
        .iter()
        .zip(1..)
        .all(|(event, expected)| event.seq == expected);
    let last = read.events.last().map_or(0, |e| e.seq);
    if !complete || last < seq || !read.unreadable.is_empty() {
        let first = read.events.first().map_or(0, |e| e.seq);
        return Err(StoreError::Checkpoint {
            seq,
            error,
            log: format!(
                "it holds events {first} to {last}{}, {} unreadable lines",
                if complete { "" } else { " with holes" },
                read.unreadable.len()
            ),
        });
    }
    let backups = data_dir.join("backups");
    let kept = backups.join(format!("unreadable-checkpoint-{seq}.json"));
    std::fs::create_dir_all(&backups)
        .and_then(|()| std::fs::write(&kept, text))
        .map_err(StoreError::SetAside)?;
    tracing::error!(
        seq,
        %error,
        kept = %kept.display(),
        code = "store-checkpoint-unreadable",
        "the checkpoint does not parse; rebuilding it from the event log"
    );
    Ok(())
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
                Command::ForgetSession(hash, deleted) => {
                    report_sqlite(sqlite::delete_session(db, &hash), "forgetting a session");
                    let _ = deleted.send(());
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
                    quantity_units: None,
                    coordinate_format: CoordinateFormat::Decimal,
                    altitude_frame: uxduck_core::domain::telemetry::AltitudeFrame::default(),
                    layout: uxduck_core::domain::operator::Layout::initial(),
                    place_search: true,
                    basemap: uxduck_core::domain::operator::Basemap::Imagery,
                    shortcuts: std::collections::BTreeMap::new(),
                    skipped_confirmations: Vec::new(),
                    telemetry_layouts: Vec::new(),
                    vehicle_telemetry: std::collections::BTreeMap::new(),
                    calculated_items: Vec::new(),
                    parameter_groups: Vec::new(),
                    inspector_pinned: Vec::new(),
                    control_actions: Vec::new(),
                    me_marker_id: None,
                    display: None,
                    account_id: None,
                    controller_mappings: Vec::new(),
                    terms: None,
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

    #[test]
    fn a_forgotten_flight_stays_removed_after_native_store_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let flight = serde_json::from_value(serde_json::json!({
            "id": "fl1", "vehicleId": "v1", "vehicleName": "Alpha",
            "vehicleType": "Quadrotor", "armedAtUs": 10, "endedAtUs": 50,
            "end": "disarmed", "recordedHere": true, "keep": true, "version": 1
        }))
        .unwrap();
        let changes = [
            Change::FlightUpserted { flight },
            Change::FlightRemoved { id: "fl1".into() },
            Change::FlightForgotten { id: "fl1".into() },
        ];
        let (store, _) = open(dir.path()).unwrap();
        let mut handle = store.handle.clone();
        let mut state = State::default();
        for (index, change) in changes.into_iter().enumerate() {
            let event = Event {
                seq: index as u64 + 1,
                ts_us: 100,
                actor: Actor::default(),
                change,
            };
            state.apply(&event).unwrap();
            handle.append(&event);
            if index == 0 {
                handle.checkpoint(&state);
            }
        }
        store.close();
        let (store, loaded) = open(dir.path()).unwrap();
        assert!(loaded.state.flights.is_empty());
        assert!(loaded.state.forgotten_flights.contains("fl1"));
        assert_eq!(loaded.state.seq, 3);
        let mut handle = store.handle.clone();
        handle.checkpoint(&loaded.state);
        store.close();
        let (store, loaded) = open(dir.path()).unwrap();
        assert!(loaded.state.flights.is_empty());
        assert!(loaded.state.forgotten_flights.contains("fl1"));
        store.close();
        let db = sqlite::open(&dir.path().join("uxduck.db")).unwrap();
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

    /// A camera as 0.8.0 wrote it, before its counters had `recoveryPoints`.
    const CAMERA_0_8_0: &str = r#"{"id":"udp-5600-127.0.0.1:5000","name":"udp://127.0.0.1:5600","port":5600,"sender":"127.0.0.1:5000","container":"ts","state":"stopped","counters":{"bitrateBps":0,"datagrams":40,"pictures":20,"keyframes":2,"lost":0,"damaged":0,"pesLengthMismatches":0,"pesLengthMaxExcess":0,"nalsDropped":0},"firstSeenUs":1,"changedAtUs":1,"version":1}"#;

    fn write_checkpoint(dir: &Path, seq: u64, state: &str) {
        let db = sqlite::open(&dir.join("uxduck.db")).unwrap();
        db.execute(
            "INSERT INTO checkpoint (id, seq, state, written_at_us) VALUES (1, ?1, ?2, 0)",
            rusqlite::params![seq.cast_signed(), state],
        )
        .unwrap();
    }

    fn log_of(events: &[Event]) -> String {
        let mut log = String::new();
        for event in events {
            log.push_str(&serde_json::to_string(event).unwrap());
            log.push('\n');
        }
        log
    }

    /// FB-58 follow-up: 0.8.1-beta.2 could read neither a 0.8.0 checkpoint
    /// with a camera nor the camera's event, so it cut the log off there
    /// and would not start.
    #[test]
    fn a_camera_checkpoint_and_event_from_0_8_0_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let camera_event = format!(
            r#"{{"seq":1,"tsUs":1,"actor":{{"clientId":"daemon"}},"change":{{"type":"videoSourceUpserted","source":{CAMERA_0_8_0}}}}}"#
        );
        let log = format!("{camera_event}\n{}", log_of(&[upsert(2, "Ana")]));
        std::fs::write(dir.path().join("events.ndjson"), &log).unwrap();
        write_checkpoint(
            dir.path(),
            1,
            &format!(
                r#"{{"seq":1,"operators":{{}},"videoSources":{{"udp-5600-127.0.0.1:5000":{CAMERA_0_8_0}}}}}"#
            ),
        );

        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 2);
        assert_eq!(loaded.state.operators.len(), 1);
        let camera = &loaded.state.video_sources["udp-5600-127.0.0.1:5000"];
        assert_eq!(camera.counters.pictures, 20);
        assert_eq!(camera.counters.recovery_points, 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("events.ndjson")).unwrap(),
            log
        );
    }

    /// Every release's sample (`uxduck-engine/tests/stored`) opens as the
    /// daemon opens its data folder, with nothing cut off or set aside.
    #[test]
    fn every_release_sample_opens() {
        let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../uxduck-engine/tests/stored");
        let mut opened = 0;
        for sample in std::fs::read_dir(&samples).unwrap() {
            let sample = sample.unwrap().path();
            let dir = tempfile::tempdir().unwrap();
            let log = std::fs::read_to_string(sample.join("events.ndjson")).unwrap();
            std::fs::write(dir.path().join("events.ndjson"), &log).unwrap();
            let checkpoint = std::fs::read_to_string(sample.join("checkpoint.json")).unwrap();
            let seq: u64 = serde_json::from_str::<serde_json::Value>(&checkpoint).unwrap()["seq"]
                .as_u64()
                .unwrap();
            write_checkpoint(dir.path(), seq, &checkpoint);

            let (store, loaded) =
                open(dir.path()).unwrap_or_else(|e| panic!("{}: {e}", sample.display()));
            store.close();
            assert_eq!(loaded.state.seq, seq, "{}", sample.display());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("events.ndjson")).unwrap(),
                log,
                "{}",
                sample.display()
            );
            assert!(!dir.path().join("backups").exists(), "{}", sample.display());
            opened += 1;
        }
        assert!(opened > 0, "no samples in {}", samples.display());
    }

    /// The log holds every event from the first, so a checkpoint this
    /// build cannot read is rebuilt from it and kept aside.
    #[test]
    fn a_checkpoint_that_does_not_parse_is_rebuilt_from_a_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let events = [upsert(1, "Ana"), upsert(2, "Ben"), upsert(3, "Cy")];
        std::fs::write(dir.path().join("events.ndjson"), log_of(&events)).unwrap();
        let unreadable = r#"{"seq":2,"operators":7}"#;
        write_checkpoint(dir.path(), 2, unreadable);

        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 3);
        assert_eq!(loaded.state.operators.len(), 3);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backups/unreadable-checkpoint-2.json"))
                .unwrap(),
            unreadable
        );
    }

    /// Without the events before it, the checkpoint cannot be rebuilt: the
    /// daemon does not start, and nothing is changed for a build that can
    /// read it.
    #[test]
    fn a_checkpoint_that_does_not_parse_is_left_alone_without_the_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = log_of(&[upsert(1, "Ana"), upsert(2, "Ben")]);
        std::fs::write(dir.path().join("events.ndjson"), &log).unwrap();
        let unreadable = r#"{"seq":5,"operators":7}"#;
        write_checkpoint(dir.path(), 5, unreadable);

        let error = open(dir.path()).unwrap_err();
        assert!(
            matches!(error, StoreError::Checkpoint { seq: 5, .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("events.ndjson")).unwrap(),
            log
        );
        let db = sqlite::open(&dir.path().join("uxduck.db")).unwrap();
        assert_eq!(
            sqlite::checkpoint_row(&db).unwrap(),
            Some((5, unreadable.to_owned()))
        );
        assert!(!dir.path().join("backups").exists());
    }

    /// An unreadable event after the checkpoint is a hole: the state stops
    /// before it and the whole log is kept aside, as for any hole.
    #[test]
    fn an_unreadable_event_after_the_checkpoint_keeps_the_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = format!(
            "{}{}\n{}",
            log_of(&[upsert(1, "Ana")]),
            r#"{"seq":2,"tsUs":2,"change":{"type":"fromANewerBuild"}}"#,
            log_of(&[upsert(3, "Cy")])
        );
        std::fs::write(dir.path().join("events.ndjson"), &log).unwrap();

        let (store, loaded) = open(dir.path()).unwrap();
        store.close();
        assert_eq!(loaded.state.seq, 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("events.ndjson.damaged-2")).unwrap(),
            log
        );
    }

    /// Forgetting answers only once the delete is written, even with the
    /// writer busy, so stopping the daemon at once keeps it forgotten.
    #[test]
    fn a_forgotten_session_is_written_before_forgetting_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = open(dir.path()).unwrap();
        let mut handle = store.handle.clone();
        let session = Session {
            origin: "http://localhost:5173".into(),
        };
        handle.save_session("phone", &session);
        handle.save_session("laptop", &session);
        for seq in 1..=1500 {
            handle.append(&upsert(seq, "Ana"));
        }
        handle.delete_session("phone");
        let db = rusqlite::Connection::open(dir.path().join("uxduck.db")).unwrap();
        let hashes: Vec<String> = sqlite::load_sessions(&db)
            .unwrap()
            .into_iter()
            .map(|(hash, _)| hash)
            .collect();
        assert_eq!(hashes, ["laptop"]);
        drop(db);
        store.close();
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

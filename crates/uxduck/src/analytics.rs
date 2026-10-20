//! The analytics outbox (ADR-0013 "Analytics", ADR-0016 O6 and O16). Events
//! wait in `analytics.db` and go to `PostHog`'s batch API through
//! `bill.uxduck.com`, in order, whenever the daemon is online. The outbox
//! holds at most 10,000 events or 7 days, oldest dropped first, and never
//! delays anything on the flight path. No timer runs while it is empty
//! (S24).
//!
//! The install id lives here too. It exists only while usage analytics are
//! on: switching them off forgets it and everything queued (ADR-0016 O15).
//! The file is not part of the record, so it has no migrations and can be
//! deleted at any time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use uxduck_contracts::dto::analytics::{AnalyticsEvent, DaemonEvent, PackageKind};
use uxduck_engine::runtime::AnalyticsPort;

/// `PostHog`'s managed reverse proxy for UXDuck (ADR-0016 O16).
pub const HOST: &str = "https://bill.uxduck.com";
/// Project "UXDuck": public, as every `PostHog` project key is.
pub const PRODUCTION_KEY: &str = "phc_kvx9a8btY9vsHd44YZag5e5bDmhUHvUA7PYkJoRPQYs4";
/// Project "UXDuck staging".
pub const STAGING_KEY: &str = "phc_uoMs8TPPqyPtpHDp5vys37TwkPssqcTbPNkTW4qYtex3";

const MAX_EVENTS: i64 = 10_000;
const MAX_AGE_US: u64 = 7 * 24 * 3600 * 1_000_000;
/// Events per request.
const BATCH: usize = 100;
/// After a failed send: 1 minute, then 5, then every 15.
const RETRY: [Duration; 3] = [
    Duration::from_mins(1),
    Duration::from_mins(5),
    Duration::from_mins(15),
];

/// Where events go, and how long they gather before a send.
#[derive(Clone, Debug)]
pub struct Config {
    /// None: nothing is kept or sent.
    pub key: Option<String>,
    pub host: String,
    /// How long events gather before a send: a minute, shorter in tests.
    pub gather: Duration,
}

impl Config {
    /// `UXDUCK_POSTHOG_KEY` wins (`none` turns analytics off). Otherwise
    /// release builds use the production project, and development builds
    /// the staging one, but only when run with `--dev`, so tests and
    /// `cargo run` without it send nothing. `UXDUCK_POSTHOG_HOST` points
    /// elsewhere, such as a test's fake.
    #[must_use]
    pub fn from_env(dev: bool, gather: Duration) -> Self {
        let key = match std::env::var("UXDUCK_POSTHOG_KEY") {
            Ok(value) if value == "none" || value.is_empty() => None,
            Ok(value) => Some(value),
            Err(_) if cfg!(debug_assertions) => dev.then(|| STAGING_KEY.to_owned()),
            Err(_) => Some(PRODUCTION_KEY.to_owned()),
        };
        Self {
            key,
            host: std::env::var("UXDUCK_POSTHOG_HOST").unwrap_or_else(|_| HOST.into()),
            gather,
        }
    }
}

/// The platform's name as `std` gives it: `macos`, `windows` or `linux`.
#[must_use]
pub fn platform() -> &'static str {
    std::env::consts::OS
}

#[must_use]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// How this binary was installed, from where it runs.
#[must_use]
pub fn package_kind(exe: &Path) -> PackageKind {
    if cfg!(debug_assertions) {
        return PackageKind::Dev;
    }
    package_kind_at(&exe.to_string_lossy(), platform())
}

fn package_kind_at(exe: &str, os: &str) -> PackageKind {
    if exe.contains(".app/Contents/Resources") || exe.contains("resources/bin") {
        PackageKind::Desktop
    } else if os == "macos"
        && (exe == "/usr/local/bin/uxduck" || exe.ends_with("UXDuck.app/Contents/MacOS/uxduck"))
    {
        PackageKind::Pkg
    } else if os == "linux" && exe == "/usr/bin/uxduck" {
        PackageKind::Deb
    } else if os == "windows" && exe.contains("Program Files") {
        PackageKind::Msi
    } else {
        PackageKind::Archive
    }
}

/// The events and the install id, in `analytics.db`.
struct Outbox {
    db: Connection,
}

impl Outbox {
    fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, created_us INTEGER NOT NULL, body TEXT NOT NULL) STRICT;
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;",
        )?;
        Ok(Self { db })
    }

    fn install_id(&self) -> Option<String> {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = 'install_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    fn set_install_id(&self, id: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('install_id', ?1)",
            params![id],
        )?;
        Ok(())
    }

    /// Forgets the install id and everything queued.
    fn forget(&self) -> rusqlite::Result<()> {
        self.db
            .execute_batch("DELETE FROM events; DELETE FROM meta WHERE key = 'install_id';")
    }

    fn push(&self, now_us: u64, body: &str) -> rusqlite::Result<()> {
        let now = i64::try_from(now_us).unwrap_or(i64::MAX);
        let oldest = now.saturating_sub(i64::try_from(MAX_AGE_US).unwrap_or(i64::MAX));
        self.db.execute(
            "INSERT INTO events (created_us, body) VALUES (?1, ?2)",
            params![now, body],
        )?;
        self.db
            .execute("DELETE FROM events WHERE created_us < ?1", params![oldest])?;
        self.db.execute(
            "DELETE FROM events WHERE id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET ?1)",
            params![MAX_EVENTS],
        )?;
        Ok(())
    }

    /// The oldest events, up to `limit`, with the last one's id.
    fn batch(&self, limit: usize) -> rusqlite::Result<(Vec<Value>, Option<i64>)> {
        let mut statement = self
            .db
            .prepare("SELECT id, body FROM events ORDER BY id LIMIT ?1")?;
        let rows: Vec<(i64, String)> = statement
            .query_map(params![i64::try_from(limit).unwrap_or(100)], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let last = rows.last().map(|(id, _)| *id);
        let events = rows
            .into_iter()
            .filter_map(|(_, body)| serde_json::from_str(&body).ok())
            .collect();
        Ok((events, last))
    }

    fn remove_through(&self, id: i64) -> rusqlite::Result<()> {
        self.db
            .execute("DELETE FROM events WHERE id <= ?1", params![id])?;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.db
            .query_row("SELECT COUNT(*) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(true, |n| n == 0)
    }
}

enum Message {
    Track(Box<AnalyticsEvent>, String),
    Alias(String),
    Enabled(bool),
    /// Something was written to the outbox directly (a panic): look again.
    Wake,
}

/// What the panic hook needs to record a `$exception` synchronously, before
/// the process ends (S32): the outbox file, whether analytics are on, and a
/// way to wake the sender if the process lives on.
struct Sink {
    db: PathBuf,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: mpsc::UnboundedSender<Message>,
}

static SINK: std::sync::OnceLock<Sink> = std::sync::OnceLock::new();

/// Queues one of the daemon's events from anywhere, such as a tracing
/// layer; dropped when analytics are off or there is no outbox.
pub fn queue_event(event: DaemonEvent) {
    if let Some(sink) = SINK.get() {
        let _ = sink.tx.send(Message::Track(
            Box::new(AnalyticsEvent::Daemon(event)),
            "daemon".into(),
        ));
    }
}

/// Writes one of the daemon's events straight to the outbox, now, for a
/// panic: the process may end before the sender task runs again. Sent at
/// the next start, or at once if the process lives on.
pub fn record_now(event: DaemonEvent) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if !sink.enabled.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Ok(outbox) = Outbox::open(&sink.db) else {
        return;
    };
    let Some(install_id) = outbox.install_id() else {
        return;
    };
    let (name, properties) = AnalyticsEvent::Daemon(event).split();
    let now = now_us();
    let _ = outbox.push(
        now,
        &event_body(&name, properties, install_id, "daemon", now),
    );
    let _ = sink.tx.send(Message::Wake);
}

/// The engine's end: every call is a message to the outbox task.
#[derive(Clone, Debug)]
pub struct AnalyticsChannel {
    tx: mpsc::UnboundedSender<Message>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("analytics message")
    }
}

impl AnalyticsPort for AnalyticsChannel {
    fn track(&mut self, event: AnalyticsEvent, surface: &str) {
        let _ = self
            .tx
            .send(Message::Track(Box::new(event), surface.to_owned()));
    }
    fn alias(&mut self, browser_id: &str) {
        let _ = self.tx.send(Message::Alias(browser_id.to_owned()));
    }
    fn set_enabled(&mut self, on: bool) {
        let _ = self.tx.send(Message::Enabled(on));
    }
}

/// Starts the outbox task. With no key it keeps and sends nothing.
#[must_use]
pub fn start(data_dir: &Path, config: Config) -> AnalyticsChannel {
    let (tx, rx) = mpsc::unbounded_channel();
    let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let db = data_dir.join("analytics.db");
    if config.key.is_some() {
        let _ = SINK.set(Sink {
            db: db.clone(),
            enabled: enabled.clone(),
            tx: tx.clone(),
        });
    }
    tokio::spawn(run(db, config, rx, enabled));
    AnalyticsChannel { tx }
}

/// The first start of a new data directory: counts the install, when
/// analytics are on (ADR-0016 O6).
#[must_use]
pub fn install_completed() -> DaemonEvent {
    DaemonEvent::InstallCompleted {
        platform: platform().into(),
        arch: arch().into(),
        version: env!("CARGO_PKG_VERSION").into(),
        package: std::env::current_exe().map_or(PackageKind::Archive, |exe| package_kind(&exe)),
    }
}

struct Task {
    outbox: Option<Outbox>,
    config: Config,
    enabled: bool,
    /// The same, for the panic hook.
    shared_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    client: Option<reqwest::Client>,
}

async fn run(
    path: PathBuf,
    config: Config,
    mut rx: mpsc::UnboundedReceiver<Message>,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let outbox = if config.key.is_some() {
        match Outbox::open(&path) {
            Ok(outbox) => Some(outbox),
            Err(error) => {
                tracing::warn!(%error, "analytics are off: the outbox could not open");
                None
            }
        }
    } else {
        None
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("uxduck/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok();
    let mut task = Task {
        outbox,
        config,
        enabled: false,
        shared_enabled: enabled,
        client,
    };
    let mut failures = 0usize;
    loop {
        // Idle: wait for a message, with no timer, while nothing is queued.
        let pending = task.enabled && task.outbox.as_ref().is_some_and(|o| !o.is_empty());
        if !pending {
            let Some(message) = rx.recv().await else {
                return;
            };
            task.handle(message);
            continue;
        }
        // Something to send: let more gather first, still taking messages.
        let wait = if failures == 0 {
            task.config.gather
        } else {
            RETRY[(failures - 1).min(RETRY.len() - 1)]
        };
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                message = rx.recv() => match message {
                    Some(message) => task.handle(message),
                    None => return,
                },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        crate::wakes::count("analytics: send");
        match task.send_all().await {
            Ok(()) => failures = 0,
            Err(error) => {
                failures += 1;
                tracing::debug!(%error, failures, "analytics: will try again");
            }
        }
    }
}

impl Task {
    fn handle(&mut self, message: Message) {
        let Some(outbox) = &self.outbox else {
            return;
        };
        match message {
            Message::Enabled(on) => {
                self.enabled = on;
                self.shared_enabled
                    .store(on, std::sync::atomic::Ordering::SeqCst);
                let result = if on {
                    match outbox.install_id() {
                        Some(_) => Ok(()),
                        None => outbox.set_install_id(&uuid_v4()),
                    }
                } else {
                    outbox.forget()
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "analytics: the outbox could not be updated");
                }
            }
            Message::Track(event, surface) if self.enabled => {
                let (name, properties) = event.split();
                self.queue(&name, properties, &surface);
            }
            Message::Alias(browser_id) if self.enabled => {
                let mut properties = Map::new();
                properties.insert("alias".into(), Value::String(browser_id));
                self.queue("$create_alias", properties, "daemon");
            }
            Message::Track(..) | Message::Alias(_) | Message::Wake => {}
        }
    }

    /// Adds the install id, the time, a unique id (so a retried batch is
    /// counted once) and the daemon's own properties.
    fn queue(&self, name: &str, properties: Map<String, Value>, surface: &str) {
        let Some(outbox) = &self.outbox else {
            return;
        };
        let Some(install_id) = outbox.install_id() else {
            return;
        };
        let now_us = now_us();
        let body = event_body(name, properties, install_id, surface, now_us);
        if let Err(error) = outbox.push(now_us, &body) {
            tracing::warn!(%error, "analytics: an event could not be queued");
        }
    }

    async fn send_all(&mut self) -> Result<(), String> {
        let (Some(key), Some(client)) = (self.config.key.clone(), self.client.clone()) else {
            return Ok(());
        };
        let url = format!("{}/batch/", self.config.host.trim_end_matches('/'));
        loop {
            let Some(outbox) = &self.outbox else {
                return Ok(());
            };
            if !self.enabled {
                return Ok(());
            }
            let (events, last) = outbox.batch(BATCH).map_err(|e| e.to_string())?;
            let Some(last) = last else {
                return Ok(());
            };
            if !events.is_empty() {
                let body = json!({
                    "api_key": key,
                    "batch": events,
                    "sent_at": rfc3339(now_us()),
                });
                let response = client
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let status = response.status();
                // A 4xx other than rate limiting will not get better: drop
                // the batch rather than retry it for a week.
                if status.is_server_error() || status.as_u16() == 429 {
                    return Err(format!("PostHog answered {status}"));
                }
                if status.is_success() {
                    tracing::debug!(events = events.len(), "analytics: sent");
                } else {
                    tracing::warn!(%status, "analytics: PostHog refused a batch; it is dropped");
                }
            }
            if let Some(outbox) = &self.outbox {
                outbox.remove_through(last).map_err(|e| e.to_string())?;
            }
        }
    }
}

/// One event as the batch API takes it: the install id, the time, a unique
/// id (so a retried batch is counted once) and the daemon's own properties.
fn event_body(
    name: &str,
    mut properties: Map<String, Value>,
    install_id: String,
    surface: &str,
    now_us: u64,
) -> String {
    properties.insert("distinct_id".into(), Value::String(install_id));
    properties.insert("$lib".into(), json!("uxduck-daemon"));
    properties.insert("$lib_version".into(), json!(env!("CARGO_PKG_VERSION")));
    // The page says where it runs in `app_opened`; that wins.
    properties
        .entry("surface")
        .or_insert_with(|| json!(surface));
    properties.insert("platform".into(), json!(platform()));
    properties.insert("arch".into(), json!(arch()));
    properties.insert("identity_scope".into(), json!("installation"));
    properties.insert("engagement_schema".into(), json!(1));
    json!({
        "event": name,
        "properties": properties,
        "timestamp": rfc3339(now_us),
        "uuid": uuid_v4(),
    })
    .to_string()
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// A random (version 4) UUID.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Never expected; a clock-based value still differs per event.
        bytes[..8].copy_from_slice(&now_us().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        });
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// UTC, to the millisecond: `2026-09-30T07:45:05.962Z`.
#[must_use]
pub fn rfc3339(us: u64) -> String {
    let secs = us / 1_000_000;
    let millis = (us / 1000) % 1000;
    let (year, month, day) = civil(secs / 86_400);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// The UTC day of `us`, as `2026-09-30`.
#[must_use]
pub fn utc_day(us: u64) -> String {
    rfc3339(us)[..10].to_owned()
}

/// Days since 1970-01-01 as a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: u64) -> (i64, u32, u32) {
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_790_753_905_962_742), "2026-09-30T07:38:25.962Z");
        assert_eq!(utc_day(951_782_400_000_000), "2000-02-29");
    }

    #[test]
    fn uuids_are_version_4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, uuid_v4());
    }

    #[test]
    fn the_package_is_told_by_where_the_binary_is() {
        assert_eq!(
            package_kind_at("/usr/local/bin/uxduck", "macos"),
            PackageKind::Pkg
        );
        assert_eq!(
            package_kind_at("/Applications/UXDuck.app/Contents/MacOS/uxduck", "macos"),
            PackageKind::Pkg
        );
        assert_eq!(
            package_kind_at("/usr/bin/uxduck", "linux"),
            PackageKind::Deb
        );
        assert_eq!(
            package_kind_at(r"C:\Program Files\UXDuck\uxduck.exe", "windows"),
            PackageKind::Msi
        );
        assert_eq!(
            package_kind_at(
                "/Applications/UXDuck.app/Contents/Resources/bin/uxduck",
                "macos"
            ),
            PackageKind::Desktop
        );
        assert_eq!(
            package_kind_at("/home/ana/uxduck/uxduck", "linux"),
            PackageKind::Archive
        );
    }

    #[test]
    fn the_outbox_keeps_order_drops_the_oldest_and_forgets_on_switch_off() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::open(&dir.path().join("analytics.db")).unwrap();
        assert_eq!(outbox.install_id(), None);
        outbox.set_install_id("i1").unwrap();
        let day = 86_400_000_000;
        outbox.push(10 * day, r#"{"n":1}"#).unwrap();
        outbox.push(10 * day + 1, r#"{"n":2}"#).unwrap();
        let (events, last) = outbox.batch(10).unwrap();
        assert_eq!(events, vec![json!({"n": 1}), json!({"n": 2})]);
        outbox.remove_through(last.unwrap()).unwrap();
        assert!(outbox.is_empty());

        // Older than a week goes when something new arrives.
        outbox.push(day, r#"{"n":"old"}"#).unwrap();
        outbox.push(10 * day, r#"{"n":"new"}"#).unwrap();
        assert_eq!(outbox.batch(10).unwrap().0, vec![json!({"n": "new"})]);

        outbox.forget().unwrap();
        assert!(outbox.is_empty());
        assert_eq!(outbox.install_id(), None);

        // Reopened, it still holds what it held.
        outbox.set_install_id("i2").unwrap();
        outbox.push(10 * day, r#"{"n":3}"#).unwrap();
        drop(outbox);
        let again = Outbox::open(&dir.path().join("analytics.db")).unwrap();
        assert_eq!(again.install_id().as_deref(), Some("i2"));
        assert_eq!(again.batch(10).unwrap().0, vec![json!({"n": 3})]);
    }

    #[test]
    fn a_full_outbox_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::open(&dir.path().join("analytics.db")).unwrap();
        let now = 10 * 86_400_000_000;
        for n in 0..(MAX_EVENTS + 5) {
            outbox.push(now, &format!(r#"{{"n":{n}}}"#)).unwrap();
        }
        let count: i64 = outbox
            .db
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MAX_EVENTS);
        assert_eq!(outbox.batch(1).unwrap().0, vec![json!({"n": 5})]);
    }
}

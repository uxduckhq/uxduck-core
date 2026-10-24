//! Cloud sync's task (S16, ADR-0001 D9, ADR-0002). The exporter reads the
//! journal after the engine's export cursor, keeps the latest change of each
//! entity the account is to have, sends them, and tells the engine how far
//! the account has got, which journals the upload. Then it fetches what
//! other computers sent and hands it to the engine, which takes what wins.
//!
//! Idle until sync is on and UXDuck is signed in (S27's token). It runs when
//! the engine says there is something new, and on an interval for other
//! computers' changes: there is no push from the account yet.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;
use uxduck_core::events::Event;
use uxduck_engine::sync::{Outgoing, Pulled, SyncChange, SyncPort, outgoing};

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// Changes per push, and the most bytes of them.
const BATCH: usize = 200;
const BATCH_BYTES: usize = 2 * 1024 * 1024;
/// How long after a change the task sends it, so a burst goes together.
const SETTLE: Duration = Duration::from_millis(300);

/// The engine's end: a nudge that something is waiting to be sent.
#[derive(Debug, Clone)]
pub struct SyncChannel {
    wake: Arc<Notify>,
}

impl SyncPort for SyncChannel {
    fn wake(&mut self) {
        self.wake.notify_one();
    }
}

/// Starts the task.
#[must_use]
pub fn start(data_dir: &Path, credentials: FileCredentials, engine: EngineHandle) -> SyncChannel {
    let wake = Arc::new(Notify::new());
    let task = Task {
        api: crate::feedback::api_url(),
        client: crate::online::client(Duration::from_secs(60)),
        credentials,
        engine,
        reader: LogReader::new(data_dir.join("events.ndjson")),
        name: computer_name(),
        synced_dir: data_dir.join("plugins").join("synced"),
        failed: HashSet::new(),
    };
    tokio::spawn(task.run(wake.clone(), interval()));
    SyncChannel { wake }
}

/// How often other computers' changes are fetched.
fn interval() -> Duration {
    std::env::var("UXDUCK_SYNC_INTERVAL_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(Duration::from_secs(30), Duration::from_millis)
}

/// What this computer is called in the account.
pub(crate) fn computer_name() -> String {
    let named = std::env::var("UXDUCK_DEVICE_NAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| {
            let run = |program: &str, args: &[&str]| {
                std::process::Command::new(program)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|out| out.status.success())
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            };
            if cfg!(target_os = "macos") {
                run("scutil", &["--get", "ComputerName"])
            } else {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|name| name.trim().to_owned())
                    .or_else(|| run("hostname", &[]))
            }
        })
        .filter(|name| !name.is_empty());
    named
        .unwrap_or_else(|| "This computer".into())
        .chars()
        .take(64)
        .collect()
}

struct Task {
    api: Option<String>,
    client: Option<reqwest::Client>,
    credentials: FileCredentials,
    engine: EngineHandle,
    reader: LogReader,
    name: String,
    /// Where plugins from the account are unpacked, one folder each (S59).
    synced_dir: PathBuf,
    /// Bundles that did not install here, not tried again until a restart.
    failed: HashSet<String>,
}

/// Why a round stopped.
enum Stop {
    /// Said to the person in Settings.
    Problem(String),
    /// The engine has stopped.
    Gone,
}

#[derive(Deserialize)]
struct PullPage {
    changes: Vec<Pulled>,
    cursor: u64,
    more: bool,
}

impl Task {
    async fn run(mut self, wake: Arc<Notify>, every: Duration) {
        loop {
            let woken = tokio::select! {
                () = wake.notified() => true,
                () = tokio::time::sleep(every) => false,
            };
            if woken {
                tokio::time::sleep(SETTLE).await;
            }
            let problem = match self.round().await {
                Ok(()) => continue,
                Err(Stop::Gone) => return,
                Err(Stop::Problem(problem)) => problem,
            };
            tracing::debug!(%problem, "sync paused");
            if self
                .engine
                .run(move |e| e.sync_problem(Some(problem)))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// One round: send what is new, then take what others sent.
    async fn round(&mut self) -> Result<(), Stop> {
        let state = self
            .engine
            .run(|e| e.sync_state().clone())
            .await
            .map_err(|_| Stop::Gone)?;
        if !state.enabled {
            return Ok(());
        }
        let (Some(api), Some(client)) = (self.api.clone(), self.client.clone()) else {
            return Err(Stop::Problem("This UXDuck has no account server.".into()));
        };
        let token = self
            .credentials
            .get(crate::account::TOKEN)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Stop::Problem("Sign in to sync.".into()))?;

        // Plugins' files go first, so a computer that takes the change
        // finds them.
        self.publish_plugins(&api, &client, &token).await?;

        // Send.
        let through = self
            .reader
            .read_after(state.exported_through)
            .map_err(|e| Stop::Problem(format!("UXDuck could not read its journal: {e}")))?;
        // What the person left out stays here, unmarked, so it goes if
        // they bring it back (2026-10-24).
        self.reader.forget(|change| !state.syncs(&change.entity));
        let waiting = self.reader.waiting();
        let mut sent = 0u64;
        for batch in batches(&waiting) {
            let answer = client
                .post(format!("{api}/v1/sync/push"))
                .bearer_auth(&token)
                .json(&json!({
                    "deviceId": state.device_id,
                    "deviceName": self.name,
                    "changes": batch,
                }))
                .send()
                .await
                .map_err(|_| unreachable())?;
            refused(&answer)?;
            sent += batch.len() as u64;
        }
        self.reader.sent();
        if through > state.exported_through && sent > 0 {
            self.engine
                .run(move |e| e.sync_exported(through, sent))
                .await
                .map_err(|_| Stop::Gone)?;
        }

        // Take.
        let mut after = state.pulled_through;
        loop {
            let answer = client
                .get(format!("{api}/v1/sync/pull"))
                .query(&[
                    ("device", state.device_id.as_str()),
                    ("after", &after.to_string()),
                ])
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|_| unreachable())?;
            refused(&answer)?;
            let page: PullPage = answer.json().await.map_err(|_| {
                Stop::Problem("The account server's answer was not understood.".into())
            })?;
            if self.credentials.get(crate::account::TOKEN).as_deref() != Some(token.as_str()) {
                return Err(Stop::Problem("The account changed. Sync is paused.".into()));
            }
            let (changes, cursor, more) = (page.changes, page.cursor, page.more);
            let credentials = self.credentials.clone();
            let expected = token.clone();
            let applied = self
                .engine
                .run(move |e| {
                    if credentials.get(crate::account::TOKEN).as_deref() != Some(expected.as_str())
                    {
                        return false;
                    }
                    e.sync_pulled(changes, cursor);
                    true
                })
                .await
                .map_err(|_| Stop::Gone)?;
            if !applied {
                return Err(Stop::Problem("The account changed. Sync is paused.".into()));
            }
            if !more || cursor <= after {
                break;
            }
            after = cursor;
        }
        self.fetch_plugins(&api, &client, &token).await?;
        self.engine
            .run(|e| e.sync_problem(None))
            .await
            .map_err(|_| Stop::Gone)?;
        Ok(())
    }
}

impl Task {
    /// Uploads the bundle of each plugin installed from a folder here that
    /// the account does not have at its current files.
    async fn publish_plugins(
        &mut self,
        api: &str,
        client: &reqwest::Client,
        token: &str,
    ) -> Result<(), Stop> {
        let plugins = self
            .engine
            .run(|e| e.plugins_to_publish())
            .await
            .map_err(|_| Stop::Gone)?;
        for plugin in plugins {
            let bytes = match crate::plugin_bundle::pack(Path::new(&plugin.path), &plugin.module) {
                Ok(bytes) => bytes,
                Err(why) => {
                    tracing::debug!(id = %plugin.id, %why, "plugin not bundled for sync");
                    continue;
                }
            };
            let digest = crate::plugin_bundle::digest(&bytes);
            if plugin.published.as_deref() == Some(digest.as_str()) || self.failed.contains(&digest)
            {
                continue;
            }
            let answer = client
                .put(format!("{api}/v1/plugins/bundles/{digest}"))
                .bearer_auth(token)
                .header("Content-Type", "application/octet-stream")
                .body(bytes)
                .send()
                .await
                .map_err(|_| unreachable())?;
            if answer.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
                tracing::warn!(id = %plugin.id, "plugin too large to sync");
                self.failed.insert(digest);
                continue;
            }
            refused(&answer)?;
            let id = plugin.id;
            self.engine
                .run(move |e| e.plugin_published(&id, &digest))
                .await
                .map_err(|_| Stop::Gone)?;
        }
        Ok(())
    }

    /// Fetches and installs the account's plugins this computer lacks.
    async fn fetch_plugins(
        &mut self,
        api: &str,
        client: &reqwest::Client,
        token: &str,
    ) -> Result<(), Stop> {
        let wanted = self
            .engine
            .run(|e| e.plugins_to_fetch())
            .await
            .map_err(|_| Stop::Gone)?;
        for plugin in wanted {
            if self.failed.contains(&plugin.digest) {
                continue;
            }
            let answer = client
                .get(format!("{api}/v1/plugins/bundles/{}", plugin.digest))
                .bearer_auth(token)
                .send()
                .await
                .map_err(|_| unreachable())?;
            if answer.status() == reqwest::StatusCode::NOT_FOUND {
                continue;
            }
            refused(&answer)?;
            let bytes = answer.bytes().await.map_err(|_| unreachable())?;
            let dir = self.synced_dir.join(&plugin.id);
            if crate::plugin_bundle::digest(&bytes) != plugin.digest
                || crate::plugin_bundle::unpack(&bytes, &dir).is_err()
            {
                tracing::warn!(id = %plugin.id, "a plugin from the account did not unpack");
                self.failed.insert(plugin.digest);
                continue;
            }
            let (id, digest) = (plugin.id, plugin.digest);
            let path = dir.display().to_string();
            let check = digest.clone();
            let installed = self
                .engine
                .run(move |e| e.plugin_fetched(&id, &check, &path))
                .await
                .map_err(|_| Stop::Gone)?;
            if !installed {
                self.failed.insert(digest);
            }
        }
        Ok(())
    }
}

fn unreachable() -> Stop {
    Stop::Problem("UXDuck cannot reach its server. It will try again.".into())
}

fn refused(answer: &reqwest::Response) -> Result<(), Stop> {
    if let Some(why) = crate::online::redirected(answer) {
        return Err(Stop::Problem(why));
    }
    match answer.status() {
        status if status.is_success() => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED => Err(Stop::Problem("Sign in again to sync.".into())),
        reqwest::StatusCode::SERVICE_UNAVAILABLE => Err(Stop::Problem(
            "Sync is not set up on the account server yet.".into(),
        )),
        status => Err(Stop::Problem(format!("The account server said {status}."))),
    }
}

/// Changes in pushes of at most [`BATCH`] changes and about [`BATCH_BYTES`].
fn batches(changes: &[SyncChange]) -> Vec<&[SyncChange]> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, change) in changes.iter().enumerate() {
        let size = serde_json::to_vec(change).map_or(0, |v| v.len());
        if i > start && (i - start >= BATCH || bytes + size > BATCH_BYTES) {
            out.push(&changes[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += size;
    }
    if start < changes.len() {
        out.push(&changes[start..]);
    }
    out
}

/// Follows the journal file: where it has read to, and the latest change
/// of each entity waiting to be sent.
struct LogReader {
    path: PathBuf,
    offset: u64,
    scanned_through: u64,
    waiting: BTreeMap<(String, String), SyncChange>,
    /// The digest last sent for each entity, so a vehicle heard again with
    /// the same profile is not sent again.
    sent: HashMap<(String, String), String>,
}

impl LogReader {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            scanned_through: 0,
            waiting: BTreeMap::new(),
            sent: HashMap::new(),
        }
    }

    /// Reads events after `cursor` that have not been read yet, and returns
    /// the last seq read.
    fn read_after(&mut self, cursor: u64) -> std::io::Result<u64> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Ok(self.scanned_through.max(cursor));
        };
        let len = file.metadata()?.len();
        // A new log (after a hole or a restore): read it from the start.
        if len < self.offset {
            self.offset = 0;
            self.scanned_through = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            // A line still being written is read next time.
            if n == 0 || !line.ends_with('\n') {
                break;
            }
            self.offset += n as u64;
            let Ok(event) = serde_json::from_str::<Event>(line.trim_end()) else {
                continue;
            };
            self.scanned_through = self.scanned_through.max(event.seq);
            if event.seq <= cursor {
                continue;
            }
            match outgoing(&event) {
                Outgoing::Send(change) => {
                    let key = (change.entity.clone(), change.id.clone());
                    if self.sent.get(&key) == Some(&change.digest) {
                        self.waiting.remove(&key);
                    } else {
                        self.waiting.insert(key, *change);
                    }
                }
                Outgoing::Withdraw { entity, id } => {
                    self.waiting.remove(&(entity, id));
                }
                Outgoing::Ignore => {}
            }
        }
        Ok(self.scanned_through.max(cursor))
    }

    /// Drops waiting changes without marking them sent.
    fn forget(&mut self, left_out: impl Fn(&SyncChange) -> bool) {
        self.waiting.retain(|_, change| !left_out(change));
    }

    fn waiting(&self) -> Vec<SyncChange> {
        self.waiting.values().cloned().collect()
    }

    /// Everything waiting reached the account.
    fn sent(&mut self) {
        for (key, change) in std::mem::take(&mut self.waiting) {
            self.sent.insert(key, change.digest);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use uxduck_core::domain::thread::Folder;
    use uxduck_core::events::{Actor, Change};

    use super::*;

    fn event(seq: u64, name: &str, version: u64, from_account: bool) -> String {
        let event = Event {
            seq,
            ts_us: seq,
            actor: Actor {
                client_id: if from_account {
                    "sync:hangar".into()
                } else {
                    "c1".into()
                },
                operator_id: None,
                identity: None,
            },
            change: Change::FolderUpserted {
                folder: Folder {
                    id: format!("f-{name}"),
                    name: name.into(),
                    parent_id: None,
                    operator_id: None,
                    sort_key: String::new(),
                    created_at_us: 0,
                    version,
                    deleted_at_us: None,
                },
            },
        };
        format!("{}\n", serde_json::to_string(&event).unwrap())
    }

    #[test]
    fn reads_after_the_cursor_keeps_the_latest_and_waits_for_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.ndjson");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(event(1, "old", 1, false).as_bytes())
            .unwrap();
        file.write_all(event(2, "a", 1, false).as_bytes()).unwrap();
        file.write_all(event(3, "a", 2, false).as_bytes()).unwrap();
        file.write_all(event(4, "b", 1, true).as_bytes()).unwrap();
        let torn = event(5, "c", 1, false);
        file.write_all(&torn.as_bytes()[..10]).unwrap();
        file.flush().unwrap();

        let mut reader = LogReader::new(path.clone());
        assert_eq!(reader.read_after(1).unwrap(), 4);
        let waiting = reader.waiting();
        assert_eq!(
            waiting.len(),
            1,
            "one folder, its latest version; not the account's own"
        );
        assert_eq!(waiting[0].version, 2);
        reader.sent();

        file.write_all(&torn.as_bytes()[10..]).unwrap();
        file.flush().unwrap();
        assert_eq!(reader.read_after(4).unwrap(), 5);
        assert_eq!(reader.waiting()[0].id, "f-c");
    }

    #[test]
    fn batches_split_by_count() {
        let change = |i: u64| SyncChange {
            entity: "folder".into(),
            id: format!("f{i}"),
            version: 1,
            digest: String::new(),
            ts_us: i,
            change: Change::PlaceRemoved {
                id: format!("p{i}"),
            },
        };
        let changes: Vec<SyncChange> = (0..450).map(change).collect();
        let sizes: Vec<usize> = batches(&changes).iter().map(|b| b.len()).collect();
        assert_eq!(sizes, [200, 200, 50]);
        assert!(batches(&[]).is_empty());
    }
}

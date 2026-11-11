//! Flights to the account (S82, ADR-0003 C2 to C4, C17, ADR-0046): every
//! few seconds each flight synced with its log has it cut from its connection's
//! tlogs, and while UXDuck is signed in the chunks go straight to storage
//! with create-only URLs the API grants, then the API is told, checks them
//! and catalogues them. A chunk's file here goes only once the
//! account acknowledged it. A network or a cloud that is down only makes
//! the backlog wait; it is reported in Settings › Storage.
//!
//! Uploads run in the background, one chunk at a time, after everything
//! else (ADR-0003 "Uplink priority").
//!
//! Recordings (S83) take the same way as segments of their own bytes, once
//! they end unless "Upload video while flying" is on: every one with
//! automatic sync and "What to sync" at video, else those of flights synced
//! with their video, on a plan that takes video (B4, B8, B11).
//!
//! On a metered connection (B12), with "Wait for unmetered networks" on,
//! nothing is cut or sent until it is not; summaries sync as they do.
//!
//! The account has a quota and a plan (B3 to B5): `/v1/storage` says what
//! it allows, and a grant it refuses stops the uploads, everything kept
//! here, until the plan or what it keeps changes or someone signs in again. When the disk runs
//! low, ended recordings make room, those already in the account first;
//! whatever goes before it was stored is journaled and told with its range.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use uxduck_contracts::dto::analytics::DaemonEvent;
use uxduck_contracts::dto::settings::{StoragePlan, SyncRefusal, UploadStatus};

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;
use crate::spool::{Spool, State};

/// How often the spool cuts and the uploader looks.
const EVERY: Duration = Duration::from_secs(10);
/// Chunks per round.
const BATCH: usize = 20;
/// Below this much free disk, ended recordings make room (S83).
const MIN_FREE_BYTES: u64 = 2 << 30;
/// How often the account's storage used is asked for.
const USAGE_EVERY: Duration = Duration::from_secs(300);
/// Why nothing goes while the connection is metered (ADR-0046 B12).
const METERED: &str = "Waiting for a network that isn't metered";
/// After rounds that failed in a row, the next waits up to this many
/// rounds: a server with no storage yet is asked about every 5 minutes.
const MAX_REST_ROUNDS: u32 = 32;

/// Whether this round may cut and upload.
enum Gate {
    /// Nothing goes: signed out, or no server.
    Off(String),
    /// Not now (a vehicle is armed, or the connection is metered): nothing
    /// is cut yet.
    Wait(String),
    /// Signed in with automatic sync off: only the flights synced by hand
    /// go, each log by itself, and their videos (2026-10-24, ADR-0046 B1).
    Asked(String),
    /// Automatic sync on: flights' logs, and recordings when allowed.
    Ready,
}

/// What `/v1/storage` says of the account (ADR-0046 B3); each part unknown
/// to a server that does not say it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Usage {
    bytes: Option<u64>,
    quota_bytes: Option<u64>,
    plan: Option<StoragePlan>,
    video: Option<bool>,
    simulated: Option<bool>,
}

/// A refusal (B5): why, the server's words, and what the account was then;
/// uploads wait until that changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Refused {
    reason: SyncRefusal,
    said: Option<String>,
    then: Option<Usage>,
}

impl Refused {
    /// Whether the account changed enough to try again: anything for a
    /// full account (a delete, a bigger quota), the plan for video.
    fn lifted(&self, now: Option<&Usage>) -> bool {
        let plan = |u: Option<&Usage>| u.map(|u| (u.plan, u.video, u.quota_bytes));
        match self.reason {
            SyncRefusal::StorageFull => self.then.as_ref() != now,
            SyncRefusal::Plan => plan(self.then.as_ref()) != plan(now),
        }
    }
}

/// A round that did not finish: a problem to rest after, or the account's
/// refusal.
enum Failure {
    Problem(String),
    Refused(SyncRefusal, Option<String>),
}

impl From<String> for Failure {
    fn from(why: String) -> Self {
        Self::Problem(why)
    }
}

pub fn start(data_dir: &Path, credentials: FileCredentials, engine: EngineHandle) {
    let spool = match Spool::open(data_dir, data_dir.join("tlogs")) {
        Ok(spool) => spool,
        Err(error) => {
            tracing::warn!(%error, "the flight spool cannot open; flights stay on this computer");
            return;
        }
    };
    let api = crate::feedback::api_url();
    let video = crate::paths::media_dir(data_dir);
    tokio::spawn(run(
        Arc::new(Mutex::new(spool)),
        video,
        api,
        credentials,
        engine,
    ));
}

fn now_us() -> u64 {
    crate::edge::now_ms() * 1000
}

/// The spool, locked only on a blocking thread: SQLite's connection is not `Sync`.
type Shared = Arc<Mutex<Spool>>;

/// The spool's work, on a blocking thread. Its cuts read tlogs and its
/// queries scan the chunks; on a runtime worker they held up the daemon's
/// other tasks, and every vehicle's telemetry with them, for seconds each
/// round.
async fn on_spool<T: Send + 'static>(
    spool: &Shared,
    work: impl FnOnce(&mut Spool) -> T + Send + 'static,
) -> T {
    let spool = Arc::clone(spool);
    let done = tokio::task::spawn_blocking(move || {
        work(
            &mut spool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    })
    .await;
    match done {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // Cancelled only as the runtime shuts down, with this task.
            Err(_) => std::future::pending().await,
        },
    }
}

#[allow(clippy::too_many_lines, reason = "one round, read top to bottom")]
async fn run(
    spool: Shared,
    video: std::path::PathBuf,
    api: Option<String>,
    credentials: FileCredentials,
    engine: EngineHandle,
) {
    let client = crate::online::client(Duration::from_secs(120));
    // Tests cut and look every so many milliseconds instead.
    let fast = std::env::var("UXDUCK_FLIGHT_CUT_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok());
    let mut every = tokio::time::interval(fast.map_or(EVERY, Duration::from_millis));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut usage_asked: Option<std::time::Instant> = None;
    let mut usage_now: Option<Usage> = None;
    let mut refused: Option<Refused> = None;
    let mut signed_in_as: Option<String> = None;
    let mut problem: Option<String> = None;
    // Rounds that failed in a row, and when the next may try.
    let mut failed: u32 = 0;
    let mut rest_until: Option<tokio::time::Instant> = None;
    loop {
        every.tick().await;
        crate::wakes::count("flights: spool and upload");
        let token = credentials
            .get(crate::account::TOKEN)
            .filter(|t| !t.is_empty());
        // Signed in again, or as someone else: the account is asked afresh (B5).
        if token != signed_in_as {
            signed_in_as.clone_from(&token);
            (usage_now, usage_asked, refused) = (None, None, None);
        }
        let resting = rest_until.is_some_and(|at| tokio::time::Instant::now() < at);
        // What the account allows, every few minutes, and every round while
        // it refuses, to try again once that changes.
        if let (Some(api), Some(client), Some(token)) = (&api, &client, &token)
            && !resting
            && (refused.is_some()
                || usage_asked.is_none_or(|at: std::time::Instant| at.elapsed() >= USAGE_EVERY))
        {
            usage_asked = Some(std::time::Instant::now());
            usage_now = usage(api, client, token).await.or(usage_now);
            if refused
                .as_ref()
                .is_some_and(|r| r.lifted(usage_now.as_ref()))
            {
                refused = None;
            }
        }
        // Reported first, so this round's cuts already follow the plan.
        let video_ok = usage_now.as_ref().and_then(|u| u.video) != Some(false)
            && refused
                .as_ref()
                .is_none_or(|r| r.reason != SyncRefusal::Plan);
        let metered = tokio::task::spawn_blocking(crate::metered::metered)
            .await
            .ok()
            .flatten();
        let (chunks, bytes) = on_spool(&spool, |s| s.backlog()).await.unwrap_or((0, 0));
        let early = upload_status(
            (chunks, bytes),
            problem.clone(),
            usage_now.as_ref(),
            refused.as_ref(),
            metered,
        );
        if engine.run(move |e| e.upload_reported(early)).await.is_err() {
            return;
        }
        let gate = gate(
            api.as_ref(),
            client.as_ref(),
            token.as_ref(),
            &engine,
            metered == Some(true),
        );
        let Some(gate) = gate.await else {
            return;
        };
        // Whole connections' tlogs are no longer sent as they are written
        // (B1): what an earlier UXDuck cut of them goes.
        if let Err(error) = on_spool(&spool, Spool::release).await {
            tracing::warn!(%error, "the flight spool could not let go of its chunks");
        }
        let Ok(held) = flight_logs_waiting(&spool, &engine).await else {
            return;
        };
        let full = refused
            .as_ref()
            .is_some_and(|r| r.reason == SyncRefusal::StorageFull);
        let going = matches!(gate, Gate::Asked(_) | Gate::Ready) && !resting && !full;
        // Cut only what a round can send: a backlog the account is not
        // taking grows no further, and no round reads much.
        let (waiting, _) = on_spool(&spool, |s| s.backlog()).await.unwrap_or((0, 0));
        let room = if going && video_ok {
            BATCH.saturating_sub(usize::try_from(waiting).unwrap_or(usize::MAX))
        } else {
            0
        };
        if recordings(&spool, &video, &engine, room > 0).await.is_err() {
            return;
        }
        if going {
            cut_asked(&spool, &engine).await;
        }
        problem = match (gate, &api, &client, &token) {
            _ if full => refused
                .as_ref()
                .map(|r| refusal_problem(r, usage_now.as_ref())),
            (gate @ (Gate::Asked(_) | Gate::Ready), Some(api), Some(client), Some(token))
                if !resting =>
            {
                // Recordings go only while allowed; which go, the engine said.
                let only_asked = !video_ok;
                let outcome = round(&spool, api, client, token, &engine, only_asked, &held).await;
                let outcome = match outcome {
                    Err(Failure::Refused(reason, said)) => {
                        // Asked afresh at once, so a stale plan is not what waits.
                        usage_now = usage(api, client, token).await.or(usage_now);
                        usage_asked = Some(std::time::Instant::now());
                        let fresh = refused.as_ref().is_none_or(|r| r.reason != reason);
                        refused = Some(Refused {
                            reason,
                            said,
                            then: usage_now.clone(),
                        });
                        if fresh {
                            let _ = engine
                                .run(move |e| e.track(DaemonEvent::SyncRefused { reason }))
                                .await;
                        }
                        Ok(())
                    }
                    Err(Failure::Problem(why)) => Err(why),
                    Ok(()) => Ok(()),
                };
                let (problem, rest) = settle(outcome, &mut failed, every.period());
                rest_until = rest;
                problem
                    .or_else(|| {
                        refused
                            .as_ref()
                            .map(|r| refusal_problem(r, usage_now.as_ref()))
                    })
                    // Off but for what was asked: still said so.
                    .or(match gate {
                        Gate::Asked(why) => Some(why),
                        _ => None,
                    })
            }
            (Gate::Ready, ..) => problem,
            (Gate::Off(why) | Gate::Wait(why) | Gate::Asked(why), ..) => Some(why),
        };
        let (chunks, bytes) = on_spool(&spool, |s| s.backlog()).await.unwrap_or((0, 0));
        let report = upload_status(
            (chunks, bytes),
            problem.clone(),
            usage_now.as_ref(),
            refused.as_ref(),
            metered,
        );
        if engine
            .run(move |e| e.upload_reported(report))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// What the uploader reports: what waits, why it is not going, and what the
/// account allows (ADR-0046 B3 to B5).
fn upload_status(
    (chunks, bytes): (u64, u64),
    problem: Option<String>,
    usage: Option<&Usage>,
    refused: Option<&Refused>,
    metered: Option<bool>,
) -> UploadStatus {
    let video_refused = refused.is_some_and(|r| r.reason == SyncRefusal::Plan);
    UploadStatus {
        chunks,
        bytes,
        problem: problem.filter(|_| chunks > 0),
        stored_bytes: usage.and_then(|u| u.bytes),
        quota_bytes: usage.and_then(|u| u.quota_bytes),
        plan: usage.and_then(|u| u.plan),
        may_upload_video: if video_refused {
            Some(false)
        } else {
            usage.and_then(|u| u.video)
        },
        may_sync_simulated: usage.and_then(|u| u.simulated),
        refused: refused.map(|r| r.reason),
        metered,
    }
}

/// The flight logs waiting here, sorted by the engine (ADR-0046 B2, B6):
/// those no longer wanted go, and those to hold are returned. `Err` once
/// the engine stopped.
async fn flight_logs_waiting(spool: &Shared, engine: &EngineHandle) -> Result<Vec<String>, ()> {
    let waiting = on_spool(spool, |s| s.asked_recordings())
        .await
        .unwrap_or_default();
    if waiting.is_empty() {
        return Ok(Vec::new());
    }
    let (held, unwanted) = engine
        .run(move |e| e.flight_logs_waiting(waiting))
        .await
        .map_err(|_| ())?;
    for recording in unwanted {
        if let Err(error) = on_spool(spool, move |s| s.drop_recording(&recording)).await {
            tracing::warn!(%error, "a flight's log no longer wanted could not be let go");
        }
    }
    Ok(held)
}

/// Why nothing goes, in plain words (ADR-0046 B5).
fn refusal_problem(refused: &Refused, usage: Option<&Usage>) -> String {
    match refused.reason {
        SyncRefusal::Plan => "Video syncs on a paid plan.".into(),
        SyncRefusal::StorageFull => match usage.and_then(|u| u.bytes.zip(u.quota_bytes)) {
            Some((used, quota)) => format!(
                "Your account is full: {} of {} used.",
                size_words(used.min(quota)),
                size_words(quota)
            ),
            None => refused
                .said
                .clone()
                .unwrap_or_else(|| "Your account is full.".into()),
        },
    }
}

/// Bytes as people read them, in the account's decimal units: `312 MB`,
/// `1 GB`, `1.5 GB`.
fn size_words(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes < GB {
        return format!("{} MB", bytes.div_ceil(MB));
    }
    let tenths = (bytes + GB / 20) / (GB / 10);
    match tenths % 10 {
        0 => format!("{} GB", tenths / 10),
        t => format!("{}.{t} GB", tenths / 10),
    }
}

/// What this round may do; `None` once the engine stopped. A metered
/// connection holds everything while the person asked it to (B12).
async fn gate(
    api: Option<&String>,
    client: Option<&reqwest::Client>,
    token: Option<&String>,
    engine: &EngineHandle,
    metered: bool,
) -> Option<Gate> {
    Some(match (api, client, token) {
        (None, _, _) | (_, None, _) => Gate::Off("This UXDuck has no account server".into()),
        (_, _, None) => Gate::Off("Sign in to store flights in your account".into()),
        _ => match engine
            .run(|e| {
                (
                    e.stores_flights(),
                    e.upload_wanted(),
                    e.waits_for_unmetered(),
                )
            })
            .await
            .ok()?
        {
            (_, _, true) if metered => Gate::Wait(METERED.into()),
            (false, ..) => Gate::Asked("Automatic sync is off".into()),
            (true, Err(why), _) => Gate::Wait(why),
            (true, Ok(()), _) => Gate::Ready,
        },
    })
}

/// Recordings into the spool, and what went before it was stored: deleted
/// by hand or by Settings › Storage, or to make room. `Err` once the engine
/// stopped.
/// Their segments are cut only when `cut`, as the tlogs' chunks are.
async fn recordings(
    spool: &Shared,
    video: &Path,
    engine: &EngineHandle,
    cut: bool,
) -> Result<(), ()> {
    let wanted = engine.run(|e| e.video_for_spool()).await.map_err(|_| ())?;
    let cache = crate::video::playback::cache_dir(video);
    let kept_as_ts = |v: &uxduck_engine::SpoolVideo| {
        Path::new(&v.file_name)
            .extension()
            .is_some_and(|e| e == "ts")
    };
    // A recording kept as MPEG-TS goes to the account as its playback MP4,
    // so the account's format is unchanged (S151).
    let as_spooled = |v: &uxduck_engine::SpoolVideo| {
        let file = video.join(&v.file_name);
        let ts = kept_as_ts(v);
        crate::spool::Recording {
            id: v.id.clone(),
            path: if ts {
                crate::video::playback::cached(&cache, &v.id)
            } else {
                file.clone()
            },
            original: ts.then_some(file),
            started_at_us: v.started_at_us,
            ended: v.ended_at_us.is_some(),
        }
    };
    // One kept as MPEG-TS is cut only once it has ended and has its copy.
    let to_cut: Vec<_> = wanted
        .iter()
        .filter(|v| cut && v.cut && (!kept_as_ts(v) || v.ended_at_us.is_some()))
        .map(as_spooled)
        .collect();
    for r in &to_cut {
        if let Some(original) = r.original.clone()
            && !r.path.exists()
        {
            let to = r.path.clone();
            let made = tokio::task::spawn_blocking(move || {
                crate::video::playback::convert(&original, &to)
            })
            .await;
            if !matches!(made, Ok(Ok(()))) {
                tracing::warn!(recording = %r.id, "a recording could not be prepared for the account");
            }
        }
    }
    if !to_cut.is_empty()
        && let Err(error) = on_spool(spool, move |s| s.cut_video(&to_cut, now_us())).await
    {
        tracing::warn!(%error, "the spool could not cut a recording's segment");
    }
    let lost = on_spool(spool, Spool::find_lost).await.unwrap_or_default();
    let need = room_needed(video);
    if need > 0 {
        let ended: Vec<_> = wanted
            .iter()
            .filter_map(|v| v.ended_at_us.map(|at| (as_spooled(v), at)))
            .collect();
        if let Err(error) = on_spool(spool, move |s| s.evict(&ended, need)).await {
            tracing::warn!(%error, "recordings could not make room");
        }
    }
    for l in lost {
        engine
            .run(move |e| e.video_lost(l.recording, l.from_us, l.to_us))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

/// How many bytes recordings must free: what free space is short of 2 GB,
/// or, for tests, what they hold over `UXDUCK_VIDEO_BUDGET_BYTES`.
fn room_needed(video: &Path) -> u64 {
    if let Some(budget) = std::env::var("UXDUCK_VIDEO_BUDGET_BYTES")
        .ok()
        .and_then(|b| b.parse::<u64>().ok())
    {
        let held: u64 = std::fs::read_dir(video)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        return held.saturating_sub(budget);
    }
    fs4::available_space(video).map_or(0, |free| MIN_FREE_BYTES.saturating_sub(free))
}

/// What the account keeps and allows (ADR-0046 B3).
async fn usage(api: &str, client: &reqwest::Client, token: &str) -> Option<Usage> {
    let answer = client
        .get(format!("{api}/v1/storage"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !answer.status().is_success() {
        return None;
    }
    Some(read_usage(&answer.json().await.ok()?))
}

fn read_usage(usage: &Value) -> Usage {
    Usage {
        bytes: usage["bytes"].as_u64(),
        quota_bytes: usage["quotaBytes"].as_u64(),
        plan: serde_json::from_value(usage["plan"].clone()).ok(),
        video: usage["video"].as_bool(),
        simulated: usage["simulated"].as_bool(),
    }
}

/// One round: grants for what is waiting, the puts, and the completion.
/// A round's outcome: the problem to show, and how long to rest after it,
/// longer after each failure in a row.
fn settle(
    outcome: Result<(), String>,
    failed: &mut u32,
    period: Duration,
) -> (Option<String>, Option<tokio::time::Instant>) {
    match outcome {
        Ok(()) => {
            *failed = 0;
            (None, None)
        }
        Err(why) => {
            *failed = failed.saturating_add(1);
            let rounds = 2u32.saturating_pow(*failed).min(MAX_REST_ROUNDS);
            (
                Some(why),
                Some(tokio::time::Instant::now() + period * rounds),
            )
        }
    }
}

/// The flight logs asked for (2026-10-24), cut from their connections'
/// logs here once each; one with nothing here is told back.
async fn cut_asked(spool: &Shared, engine: &EngineHandle) {
    let Ok(asked) = engine.run(|e| e.flight_logs_asked()).await else {
        return;
    };
    for (flight, link, from_us, to_us) in asked {
        let recording = flight.clone();
        match on_spool(spool, move |s| {
            s.cut_flight(&recording, &link, from_us, to_us)
        })
        .await
        {
            Ok(0) => {
                let _ = engine.run(move |e| e.flight_log_missing(&flight)).await;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "a flight's log could not be cut"),
        }
    }
}

/// The chunks a round may send, oldest first: flights' logs only, or
/// recordings too; none of a log held back (B6).
async fn sendable(
    spool: &Shared,
    only_asked: bool,
    held: &[String],
) -> Result<Vec<(crate::spool::Chunk, State)>, String> {
    let held = held.to_vec();
    on_spool(spool, move |s| {
        let waiting = if only_asked {
            s.waiting_asked(usize::MAX)
        } else {
            s.waiting(usize::MAX)
        }?;
        Ok(waiting
            .into_iter()
            .filter(|(chunk, _)| !held.contains(&chunk.recording))
            .take(BATCH)
            .collect())
    })
    .await
    .map_err(|e: rusqlite::Error| e.to_string())
}

async fn round(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    engine: &EngineHandle,
    only_asked: bool,
    held: &[String],
) -> Result<(), Failure> {
    let waiting = sendable(spool, only_asked, held).await?;
    if waiting.is_empty() {
        return Ok(());
    }
    let device = engine
        .run(uxduck_engine::Engine::edge_device_id)
        .await
        .map_err(|_| "UXDuck is stopping".to_owned())?;
    put_new(spool, api, client, token, &device, &waiting).await?;
    complete(spool, api, client, token, &device, engine, only_asked, held).await?;
    Ok(())
}

fn unreachable(_: reqwest::Error) -> String {
    "UXDuck cannot reach its server. Flights remain on this computer.".to_owned()
}

/// Grants for the chunks not put yet, and the puts, straight to storage.
async fn put_new(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    device: &str,
    waiting: &[(crate::spool::Chunk, State)],
) -> Result<(), Failure> {
    // Grants for those not put yet.
    let to_put: Vec<_> = waiting
        .iter()
        .filter(|(_, state)| *state == State::Written)
        .map(|(chunk, _)| chunk.clone())
        .collect();
    if !to_put.is_empty() {
        let answer = client
            .post(format!("{api}/v1/ingest/grants"))
            .bearer_auth(token)
            .json(&json!({
                "deviceId": device,
                "chunks": to_put.iter().map(|c| json!({ "name": c.name, "size": c.size, "sha256": c.sha256 })).collect::<Vec<_>>(),
            }))
            .send()
            .await
            .map_err(unreachable)?;
        if answer.status() == reqwest::StatusCode::FORBIDDEN {
            let body: Value = answer.json().await.unwrap_or_default();
            return Err(grants_refused(&body));
        }
        if !answer.status().is_success() {
            return Err(refused(&answer).into());
        }
        let grants: Value = answer.json().await.map_err(|e| e.to_string())?;
        for chunk in &to_put {
            let Some(url) = grants["grants"]
                .as_array()
                .and_then(|g| g.iter().find(|g| g["name"] == chunk.name.as_str()))
                .and_then(|g| g["url"].as_str())
            else {
                continue;
            };
            let wanted = chunk.clone();
            let bytes = on_spool(spool, move |s| s.bytes(&wanted))
                .await
                .map_err(|e| e.to_string())?;
            let put = client
                .put(url)
                .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                .body(bytes)
                .send()
                .await
                .map_err(unreachable)?;
            if !put.status().is_success() {
                return Err(format!("storage refused a chunk ({})", put.status()).into());
            }
            let name = chunk.name.clone();
            on_spool(spool, move |s| s.mark(&name, State::Uploaded))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Tells the account what was put, and lets go of what it keeps.
#[allow(
    clippy::too_many_arguments,
    reason = "the round's context, passed down"
)]
async fn complete(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    device: &str,
    engine: &EngineHandle,
    only_asked: bool,
    held: &[String],
) -> Result<(), String> {
    // Everything put, here or in an earlier round: does the account keep it?
    let put: Vec<_> = sendable(spool, only_asked, held)
        .await?
        .into_iter()
        .filter(|(_, state)| *state == State::Uploaded)
        .map(|(chunk, _)| chunk)
        .collect();
    if put.is_empty() {
        return Ok(());
    }
    let answer = client
        .post(format!("{api}/v1/ingest/complete"))
        .bearer_auth(token)
        .json(&json!({
            "deviceId": device,
            "chunks": put.iter().map(|c| json!({
                "name": c.name, "size": c.size, "sha256": c.sha256, "firstUs": c.first_us, "lastUs": c.last_us,
            })).collect::<Vec<_>>(),
        }))
        .send()
        .await
        .map_err(unreachable)?;
    if !answer.status().is_success() {
        return Err(refused(&answer));
    }
    let completed: Value = answer.json().await.map_err(|e| e.to_string())?;
    let mut stored: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    for name in completed["acknowledged"].as_array().into_iter().flatten() {
        let Some(chunk) = put.iter().find(|c| *name == c.name.as_str()) else {
            continue;
        };
        let name = chunk.name.clone();
        on_spool(spool, move |s| s.mark(&name, State::Acknowledged))
            .await
            .map_err(|e| e.to_string())?;
        let entry = stored.entry(chunk.recording.clone()).or_default();
        entry.0 += 1;
        entry.1 += chunk.size;
    }
    // A chunk the account did not find is put again.
    for name in completed["missing"].as_array().into_iter().flatten() {
        if let Some(name) = name.as_str() {
            let name = name.to_owned();
            on_spool(spool, move |s| s.mark(&name, State::Written))
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    for (recording, (chunks, bytes)) in stored {
        // A flight's log asked for by itself: in the account once all of it is.
        let name = recording.clone();
        let whole = on_spool(spool, move |s| s.unacknowledged(&name))
            .await
            .is_ok_and(|(cut, left)| cut > 0 && left == 0);
        let _ = engine
            .run(move |e| {
                if whole {
                    e.flight_log_stored(&recording);
                }
                e.flight_stored(recording, chunks, bytes);
            })
            .await;
    }
    Ok(())
}

/// A grant refused with 403 (ADR-0046 B5): `{ error, code }`, the code
/// `storage-full` or `plan`; any other is a problem like the rest.
fn grants_refused(body: &Value) -> Failure {
    let said = body["error"].as_str().map(str::to_owned);
    match serde_json::from_value::<SyncRefusal>(body["code"].clone()) {
        Ok(reason) => Failure::Refused(reason, said),
        Err(_) => Failure::Problem(
            said.unwrap_or_else(|| "UXDuck's server refused the upload (403 Forbidden)".to_owned()),
        ),
    }
}

fn refused(answer: &reqwest::Response) -> String {
    let status = answer.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return "Sign in again to store flights".into();
    }
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return "Flight storage is not set up on UXDuck's server yet".into();
    }
    format!("UXDuck's server refused the upload ({status})")
}

/// The account's token and server, for the commands that read flights.
fn signed_in(data_dir: &Path) -> Result<(String, String, reqwest::Client), String> {
    let api = crate::feedback::api_url().ok_or("this UXDuck has no account server")?;
    let token = FileCredentials::new(data_dir)
        .get(crate::account::TOKEN)
        .filter(|t| !t.is_empty())
        .ok_or("You're not signed in. Run `uxduck login` first.")?;
    let client = crate::online::client(Duration::from_secs(120)).ok_or("no web client here")?;
    Ok((api, token, client))
}

/// What the account keeps: flights' tlogs (S82) or cameras' recordings (S83).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stored {
    Flights,
    Recordings,
}

impl Stored {
    const fn path(self) -> &'static str {
        match self {
            Self::Flights => "flights",
            Self::Recordings => "recordings",
        }
    }

    const fn one(self) -> &'static str {
        match self {
            Self::Flights => "flight",
            Self::Recordings => "recording",
        }
    }
}

/// `uxduck account logbook` and `recordings`: what the account keeps,
/// newest first.
///
/// # Errors
/// Not signed in, or the server cannot be reached.
pub async fn list(data_dir: &Path, kind: Stored, as_json: bool) -> Result<(), String> {
    let (api, token, client) = signed_in(data_dir)?;
    let answer = client
        .get(format!("{api}/v1/{}", kind.path()))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(unreachable)?;
    if !answer.status().is_success() {
        return Err(refused(&answer));
    }
    let listed: Value = answer.json().await.map_err(|e| e.to_string())?;
    if as_json {
        println!("{listed}");
        return Ok(());
    }
    let flights = listed[kind.path()].as_array().cloned().unwrap_or_default();
    if flights.is_empty() {
        println!("No {} in your account yet.", kind.path());
    }
    for flight in flights {
        let minutes = flight["lastUs"]
            .as_u64()
            .zip(flight["firstUs"].as_u64())
            .map(|(last, first)| last.saturating_sub(first) / 60_000_000);
        println!(
            "{}  {}  {}{}  {} KB",
            flight["computer"]
                .as_str()
                .or(flight["deviceId"].as_str())
                .unwrap_or("?"),
            flight["id"].as_str().unwrap_or("?"),
            minutes.map(|m| format!("{m} min")).unwrap_or_default(),
            if minutes.is_some() { "," } else { "" },
            flight["bytes"].as_u64().unwrap_or(0) / 1024,
        );
    }
    Ok(())
}

/// `uxduck account flight` and `recording`: one from the account, its
/// chunks checked and joined back into the tlog or MP4 its computer
/// recorded, for `uxduck dev replay` or any player.
///
/// # Errors
/// Not signed in, no such flight, or a chunk that does not check out.
pub async fn download(
    data_dir: &Path,
    kind: Stored,
    computer: &str,
    id: &str,
    out: &Path,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let (api, token, client) = signed_in(data_dir)?;
    // A computer's name means its device id.
    let listed: Value = client
        .get(format!("{api}/v1/{}", kind.path()))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(unreachable)?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let device = listed[kind.path()]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| {
            f["id"] == id
                && (f["deviceId"] == computer
                    || f["computer"]
                        .as_str()
                        .is_some_and(|c| c.eq_ignore_ascii_case(computer)))
        })
        .and_then(|f| f["deviceId"].as_str())
        .ok_or_else(|| format!("no {} {id} from {computer} in your account", kind.one()))?
        .to_owned();
    let answer = client
        .get(format!("{api}/v1/{}/{device}/{id}", kind.path()))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(unreachable)?;
    if !answer.status().is_success() {
        return Err(refused(&answer));
    }
    let flight: Value = answer.json().await.map_err(|e| e.to_string())?;
    let parts = match kind {
        Stored::Flights => &flight["chunks"],
        Stored::Recordings => &flight["segments"],
    };
    let mut tlog = Vec::new();
    for chunk in parts.as_array().into_iter().flatten() {
        let url = chunk["url"].as_str().ok_or("a chunk without a URL")?;
        let bytes = client
            .get(url)
            .send()
            .await
            .map_err(unreachable)?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        let hex = crate::spool::hex(&Sha256::digest(&bytes));
        if Some(hex.as_str()) != chunk["sha256"].as_str() {
            return Err(format!("chunk {} does not check out", chunk["seq"]));
        }
        match kind {
            Stored::Flights => {
                tlog.extend(zstd::bulk::decompress(&bytes, 64 << 20).map_err(|e| e.to_string())?);
            }
            Stored::Recordings => tlog.extend_from_slice(&bytes),
        }
    }
    std::fs::write(out, &tlog).map_err(|e| e.to_string())?;
    match kind {
        Stored::Flights => eprintln!(
            "{} ({} KB): replay it with `uxduck dev replay {}`",
            out.display(),
            tlog.len() / 1024,
            out.display()
        ),
        Stored::Recordings => {
            eprintln!("{} ({} KB)", out.display(), tlog.len() / 1024);
            for gap in flight["gaps"].as_array().into_iter().flatten() {
                eprintln!(
                    "Segments {} to {} are not in your account. Playback skips those segments.",
                    gap["fromSeq"], gap["toSeq"]
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_and_full() -> Usage {
        read_usage(&json!({
            "bytes": 1_000_000_000u64,
            "quotaBytes": 1_000_000_000u64,
            "plan": "free",
            "video": false,
            "simulated": false,
        }))
    }

    #[test]
    fn the_accounts_plan_is_read_and_an_older_server_says_nothing_of_it() {
        assert_eq!(
            free_and_full(),
            Usage {
                bytes: Some(1_000_000_000),
                quota_bytes: Some(1_000_000_000),
                plan: Some(StoragePlan::Free),
                video: Some(false),
                simulated: Some(false),
            }
        );
        assert_eq!(
            read_usage(&json!({ "bytes": 5 })),
            Usage {
                bytes: Some(5),
                ..Usage::default()
            }
        );
    }

    /// ADR-0046 B5: a 403 from the grants says why, in plain words.
    #[test]
    fn a_refused_grant_says_why_and_waits_until_the_account_changes() {
        let Failure::Refused(reason, said) = grants_refused(&json!({
            "error": "Your account is full.",
            "code": "storage-full",
        })) else {
            panic!("a refusal");
        };
        assert_eq!(reason, SyncRefusal::StorageFull);
        let refused = Refused {
            reason,
            said,
            then: Some(free_and_full()),
        };
        assert_eq!(
            refusal_problem(&refused, refused.then.as_ref()),
            "Your account is full: 1 GB of 1 GB used."
        );
        assert_eq!(
            refusal_problem(&refused, None),
            "Your account is full.",
            "the server's words when its numbers are unknown"
        );
        // Unchanged, it waits; a delete or a paid plan lets it try again.
        assert!(!refused.lifted(Some(&free_and_full())));
        let deleted = Usage {
            bytes: Some(400_000_000),
            ..free_and_full()
        };
        assert!(refused.lifted(Some(&deleted)));

        let Failure::Refused(reason, said) = grants_refused(&json!({
            "error": "Video syncs on a paid plan.",
            "code": "plan",
        })) else {
            panic!("a refusal");
        };
        let video = Refused {
            reason,
            said,
            then: Some(free_and_full()),
        };
        assert_eq!(refusal_problem(&video, None), "Video syncs on a paid plan.");
        // Logs uploaded since do not lift it; a paid plan does.
        assert!(!video.lifted(Some(&deleted)));
        let paid = Usage {
            plan: Some(StoragePlan::Paid),
            video: Some(true),
            quota_bytes: Some(100_000_000_000),
            ..free_and_full()
        };
        assert!(video.lifted(Some(&paid)));

        // Any other 403 is a problem like the rest.
        assert!(matches!(
            grants_refused(&json!({ "error": "no" })),
            Failure::Problem(why) if why == "no"
        ));
    }

    #[test]
    fn sizes_read_as_the_account_counts_them() {
        assert_eq!(size_words(312_000_000), "312 MB");
        assert_eq!(size_words(1), "1 MB");
        assert_eq!(size_words(1_000_000_000), "1 GB");
        assert_eq!(size_words(1_500_000_000), "1.5 GB");
        assert_eq!(size_words(100_000_000_000), "100 GB");
    }
}

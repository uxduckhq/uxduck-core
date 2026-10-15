//! Flights to the account (S82, ADR-0003 C2 to C4, C17): every few seconds
//! the spool cuts what is due, and while UXDuck is signed in with "Store
//! flights and recordings in my account" on, the chunks go straight to
//! storage with create-only URLs the API grants, then the API is told,
//! checks them and catalogues them. A chunk's file here goes only once the
//! account acknowledged it. A network or a cloud that is down only makes
//! the backlog wait; it is reported in Settings › Storage.
//!
//! Uploads run in the background, one chunk at a time, after everything
//! else (ADR-0003 "Uplink priority").
//!
//! Recordings (S83) take the same way as segments of their own bytes, once
//! they end unless "Upload video while flying" is on. When the disk runs
//! low, ended recordings make room, those already in the account first;
//! whatever goes before it was stored is journaled and told with its range.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use uxduck_contracts::dto::settings::UploadStatus;

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;
use crate::spool::{Spool, State};

/// How often the spool cuts and the uploader looks.
const EVERY: Duration = Duration::from_secs(10);
/// A tlog that has not grown for this long is cut whatever its size.
const IDLE_US: u64 = 60_000_000;
/// Chunks per round.
const BATCH: usize = 20;
/// Below this much free disk, ended recordings make room (S83).
const MIN_FREE_BYTES: u64 = 2 << 30;
/// How often the account's storage used is asked for.
const USAGE_EVERY: Duration = Duration::from_secs(300);

pub fn start(data_dir: &Path, credentials: FileCredentials, engine: EngineHandle) {
    let spool = match Spool::open(data_dir, data_dir.join("tlogs")) {
        Ok(spool) => spool,
        Err(error) => {
            tracing::warn!(%error, "the flight spool cannot open; flights stay on this computer");
            return;
        }
    };
    let api = crate::feedback::api_url();
    let video = crate::paths::recordings_dir(data_dir);
    tokio::spawn(run(
        std::sync::Mutex::new(spool),
        video,
        api,
        credentials,
        engine,
    ));
}

fn now_us() -> u64 {
    crate::edge::now_ms() * 1000
}

/// The spool, locked only between awaits: SQLite's connection is not `Sync`.
type Shared = std::sync::Mutex<Spool>;

fn lock(spool: &Shared) -> std::sync::MutexGuard<'_, Spool> {
    spool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    let mut stored_bytes: Option<u64> = None;
    loop {
        every.tick().await;
        crate::wakes::count("flights: spool and upload");
        let cut = lock(&spool).cut_every(
            now_us(),
            IDLE_US,
            fast.map_or(crate::spool::CUT_EVERY_US, |ms| ms * 1000),
        );
        if let Err(error) = cut {
            tracing::warn!(%error, "the flight spool could not cut a chunk");
        }
        if recordings(&spool, &video, &engine).await.is_err() {
            return;
        }
        let token = credentials
            .get(crate::account::TOKEN)
            .filter(|t| !t.is_empty());
        let problem = match (&api, &client, token) {
            (None, _, _) | (_, None, _) => Some("This UXDuck has no account server".to_owned()),
            (_, _, None) => Some("Sign in to store flights in your account".to_owned()),
            (Some(api), Some(client), Some(token)) => {
                match engine.run(|e| e.upload_wanted()).await {
                    Err(_) => return,
                    Ok(Err(why)) => Some(why),
                    Ok(Ok(())) => round(&spool, api, client, &token, &engine).await.err(),
                }
            }
        };
        if problem.is_none()
            && usage_asked.is_none_or(|at: std::time::Instant| at.elapsed() >= USAGE_EVERY)
        {
            usage_asked = Some(std::time::Instant::now());
            if let (Some(api), Some(client), Some(token)) = (
                &api,
                &client,
                credentials
                    .get(crate::account::TOKEN)
                    .filter(|t| !t.is_empty()),
            ) {
                stored_bytes = usage(api, client, &token).await.or(stored_bytes);
            }
        }
        let (chunks, bytes) = lock(&spool).backlog().unwrap_or((0, 0));
        let status = UploadStatus {
            chunks,
            bytes,
            problem: problem.filter(|_| chunks > 0),
            stored_bytes,
        };
        if engine
            .run(move |e| e.upload_reported(status))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Recordings into the spool, and what went before it was stored: deleted
/// by hand or by Settings › Storage, or to make room. `Err` once the engine
/// stopped.
async fn recordings(spool: &Shared, video: &Path, engine: &EngineHandle) -> Result<(), ()> {
    let wanted = engine.run(|e| e.video_for_spool()).await.map_err(|_| ())?;
    let as_spooled = |v: &uxduck_engine::SpoolVideo| crate::spool::Recording {
        id: v.id.clone(),
        path: crate::video::recorder::mp4_path(video, &v.id),
        started_at_us: v.started_at_us,
        ended: v.ended_at_us.is_some(),
    };
    let to_cut: Vec<_> = wanted.iter().filter(|v| v.cut).map(as_spooled).collect();
    if let Err(error) = lock(spool).cut_video(&to_cut, now_us()) {
        tracing::warn!(%error, "the spool could not cut a recording's segment");
    }
    let mut lost: Vec<(crate::spool::Lost, bool)> = lock(spool)
        .find_lost()
        .unwrap_or_default()
        .into_iter()
        .map(|l| (l, false))
        .collect();
    let need = room_needed(video);
    if need > 0 {
        let ended: Vec<_> = wanted
            .iter()
            .filter_map(|v| v.ended_at_us.map(|at| (as_spooled(v), at)))
            .collect();
        match lock(spool).evict(&ended, need) {
            Ok(evicted) => lost.extend(evicted.into_iter().map(|l| (l, true))),
            Err(error) => tracing::warn!(%error, "recordings could not make room"),
        }
    }
    for (l, evicted) in lost {
        engine
            .run(move |e| e.video_lost(l.recording, l.from_us, l.to_us, evicted))
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

/// What the account keeps, in bytes.
async fn usage(api: &str, client: &reqwest::Client, token: &str) -> Option<u64> {
    let answer = client
        .get(format!("{api}/v1/storage"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !answer.status().is_success() {
        return None;
    }
    let usage: Value = answer.json().await.ok()?;
    usage["bytes"].as_u64()
}

/// One round: grants for what is waiting, the puts, and the completion.
async fn round(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    engine: &EngineHandle,
) -> Result<(), String> {
    let waiting = lock(spool).waiting(BATCH).map_err(|e| e.to_string())?;
    if waiting.is_empty() {
        return Ok(());
    }
    let device = engine
        .run(uxduck_engine::Engine::edge_device_id)
        .await
        .map_err(|_| "UXDuck is stopping".to_owned())?;
    put_new(spool, api, client, token, &device, &waiting).await?;
    complete(spool, api, client, token, &device, engine).await
}

fn unreachable(_: reqwest::Error) -> String {
    "UXDuck cannot reach its server; the flights wait here".to_owned()
}

/// Grants for the chunks not put yet, and the puts, straight to storage.
async fn put_new(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    device: &str,
    waiting: &[(crate::spool::Chunk, State)],
) -> Result<(), String> {
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
        if !answer.status().is_success() {
            return Err(refused(&answer));
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
            let bytes = lock(spool).bytes(chunk).map_err(|e| e.to_string())?;
            let put = client
                .put(url)
                .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                .body(bytes)
                .send()
                .await
                .map_err(unreachable)?;
            if !put.status().is_success() {
                return Err(format!("storage refused a chunk ({})", put.status()));
            }
            lock(spool)
                .mark(&chunk.name, State::Uploaded)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Tells the account what was put, and lets go of what it keeps.
async fn complete(
    spool: &Shared,
    api: &str,
    client: &reqwest::Client,
    token: &str,
    device: &str,
    engine: &EngineHandle,
) -> Result<(), String> {
    // Everything put, here or in an earlier round: does the account keep it?
    let put: Vec<_> = lock(spool)
        .waiting(BATCH)
        .map_err(|e| e.to_string())?
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
        lock(spool)
            .mark(&chunk.name, State::Acknowledged)
            .map_err(|e| e.to_string())?;
        let entry = stored.entry(chunk.recording.clone()).or_default();
        entry.0 += 1;
        entry.1 += chunk.size;
    }
    // A chunk the account did not find is put again.
    for name in completed["missing"].as_array().into_iter().flatten() {
        if let Some(name) = name.as_str() {
            lock(spool)
                .mark(name, State::Written)
                .map_err(|e| e.to_string())?;
        }
    }
    for (recording, (chunks, bytes)) in stored {
        let _ = engine
            .run(move |e| e.flight_stored(recording, chunks, bytes))
            .await;
    }
    Ok(())
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

/// `uxduck account flights` and `recordings`: what the account keeps,
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
                    "Segments {} to {} are not in your account; the video jumps there.",
                    gap["fromSeq"], gap["toSeq"]
                );
            }
        }
    }
    Ok(())
}

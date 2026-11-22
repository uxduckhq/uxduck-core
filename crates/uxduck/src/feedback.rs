//! Sending feedback (ADR-0016 O9, O11): the engine hands over a report with
//! its scrubbed bundle; this adds the platform, the log tail and, when
//! ticked, the vehicle's last five minutes of tlog, keeps it all with the
//! files the person attached in
//! `feedback/<report id>/` in the data directory, and posts it to the UXDuck
//! API. People in the field are often offline: a report that cannot be sent
//! stays there and goes out later, including after a restart, for up to a
//! week. No timer runs while nothing waits (S24).
//!
//! The API takes so much of a bundle and of flight data and refuses the
//! whole report over more. What is over is left out here, always the
//! oldest, and said: in the bundle's `leftOut` for whoever reads the
//! report, and in the answer for the person who sent it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uxduck_contracts::dto::feedback::{
    FeedbackKind, FeedbackLeftOut, FeedbackSendResult, FeedbackStatus, LeftOut,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_engine::feedback::{FeedbackAddition, FeedbackOrder, FeedbackPort, FlightRef};

use crate::adapters::credentials::FileCredentials;
use crate::daemon::engine_task::EngineHandle;

/// Where reports go: the selected API, Local by default in development.
/// `UXDUCK_API_URL` points elsewhere; `none` turns feedback off.
#[must_use]
pub fn api_url() -> Option<String> {
    crate::dev_api::api_url()
}

/// How far back the flight data goes: the form says "the last 5 minutes".
const FLIGHT_US: u64 = 5 * 60 * 1_000_000;
/// The most UXDuck's servers take of flight data (`LIMITS.flight` in
/// `apps/api/src/features/feedback/report.ts`): they refuse the whole
/// report for more, the person's words with it. Five minutes of a busy link
/// can be more, so the newest of it goes and the answer says what did not.
pub(crate) const MAX_FLIGHT: usize = 20 * 1024 * 1024;
const KEEP_FOR: Duration = Duration::from_hours(24 * 7);
const RETRY: Duration = Duration::from_mins(1);
/// The longest a 429's `Retry-After` holds kept reports back.
const LONGEST_PAUSE: Duration = Duration::from_hours(24);
/// A report goes out in pieces this large, and the page waiting on it is
/// told how far it has got at most this often.
const UPLOAD_CHUNK: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// An upload is given up, and the report kept, when none of the rest of it
/// goes out for this long, or when it takes longer in all than a minute for
/// the answer and its body at `SLOWEST_UPLOAD` bytes a second.
pub(crate) const STALLED: Duration = Duration::from_mins(1);
const SLOWEST_UPLOAD: usize = 32 * 1024;
/// What a report's upload is counted as among the daemon's wakes.
const UPLOAD_WAKE: &str = "feedback: upload progress";

/// How many follow-up keys of reports the agent filed are kept (S294): a
/// file on this computer that would otherwise grow with every report an
/// agent files. An addition to an older report is refused in words ("this
/// computer no longer has that report's key").
const KEYS_KEPT: usize = 200;

/// What the sender task is asked to do.
#[derive(Debug)]
enum Job {
    Report(Box<FeedbackOrder>),
    Addition(FeedbackAddition),
}

/// The engine's end: each report is a message to the sender task.
#[derive(Debug)]
pub struct FeedbackChannel {
    tx: mpsc::UnboundedSender<Job>,
}

impl FeedbackPort for FeedbackChannel {
    fn send(&mut self, order: FeedbackOrder) {
        let _ = self.tx.send(Job::Report(Box::new(order)));
    }
    fn add(&mut self, addition: FeedbackAddition) {
        let _ = self.tx.send(Job::Addition(addition));
    }
}

/// Starts the sender; reports kept from an earlier run are tried again.
#[must_use]
pub fn start(
    data_dir: &Path,
    credentials: FileCredentials,
    engine: EngineHandle,
) -> FeedbackChannel {
    let (tx, rx) = mpsc::unbounded_channel();
    let sender = Sender {
        // A minute; `UXDUCK_FEEDBACK_RETRY_MS` shortens it for tests.
        retry: std::env::var("UXDUCK_FEEDBACK_RETRY_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(RETRY, Duration::from_millis),
        outbox: data_dir.join("feedback"),
        tlogs: data_dir.join("tlogs"),
        api: api_url(),
        client: crate::online::client(Duration::from_secs(60)),
        sent: data_dir.join("suggestions.json"),
        keys: data_dir.join("agent-feedback-keys.json"),
        credentials,
        engine,
    };
    // Suggestions sent from here that this version ships (S73).
    let shipped = crate::shipped::due(&sender.sent, env!("CARGO_PKG_VERSION"));
    if !shipped.is_empty() {
        let engine = sender.engine.clone();
        tokio::spawn(async move {
            let _ = engine
                .run(move |e| {
                    for (issue, title, version) in shipped {
                        e.suggestion_shipped(&issue, &title, &version);
                    }
                })
                .await;
        });
    }
    tokio::spawn(run(sender, rx));
    FeedbackChannel { tx }
}

struct Sender {
    retry: Duration,
    outbox: PathBuf,
    tlogs: PathBuf,
    api: Option<String>,
    client: Option<reqwest::Client>,
    /// The suggestions sent from here to be told about (S73).
    sent: PathBuf,
    /// The follow-up keys of reports the agent filed, by report id (S294).
    keys: PathBuf,
    credentials: FileCredentials,
    engine: EngineHandle,
}

/// How a send went.
enum Sent {
    Done(Box<FeedbackSendResult>),
    /// Offline, or the API cannot take it now: kept for later.
    Later,
    /// The API takes this report not now (429): kept, and tried again after
    /// `wait`. When the API limits this whole address, nothing else is tried
    /// before then either. Otherwise the others still go (UXD-338).
    Limited {
        wait: Duration,
        address: bool,
    },
    /// The API refused it for good, saying why.
    Refused(String),
}

async fn run(sender: Sender, mut rx: mpsc::UnboundedReceiver<Job>) {
    let engine = sender.engine.clone();
    // While the API is limiting this address, kept reports wait until then.
    // A report the API limited on its own waits in its folder (`defer`).
    let mut paused_until: Option<Instant> = None;
    loop {
        let paused = paused_until.filter(|until| *until > Instant::now());
        let wake = if sender.waiting().is_empty() {
            next_due(&sender.outbox).map(|at| {
                let due = Instant::now() + at.duration_since(SystemTime::now()).unwrap_or_default();
                paused.map_or(due, |until| until.max(due))
            })
        } else {
            Some(paused.unwrap_or_else(|| Instant::now() + sender.retry))
        };
        let job = match wake {
            None => rx.recv().await,
            Some(wake) => tokio::select! {
                job = rx.recv() => job,
                () = tokio::time::sleep_until(wake) => {
                    crate::wakes::count("feedback: retry");
                    for dir in sender.waiting() {
                        if let Some(wait) = sender.retry(&dir).await {
                            paused_until = Some(Instant::now() + wait);
                            break;
                        }
                    }
                    continue;
                }
            },
        };
        let order = match job {
            None => return,
            Some(Job::Report(order)) => *order,
            Some(Job::Addition(addition)) => {
                let id = addition.id.clone();
                let answer = sender.add(addition).await;
                let _ = engine.run(move |e| e.feedback_done(&id, answer)).await;
                continue;
            }
        };
        let report_id = order.report_id.clone();
        let answer = match sender.keep(order).await {
            Err(error) => {
                // Nothing of it stays to go out later without the file that
                // could not be kept, as a report the person was told had
                // failed.
                let _ = std::fs::remove_dir_all(sender.outbox.join(&report_id));
                Err(ApiError::new(
                    ErrorKind::Internal,
                    format!("the report could not be kept: {error}"),
                ))
            }
            Ok((dir, left_out)) => match sender.send(&dir).await {
                Sent::Done(result) => {
                    sender.remember(&dir, &result);
                    let _ = std::fs::remove_dir_all(&dir);
                    Ok(FeedbackSendResult {
                        left_out,
                        ..*result
                    })
                }
                Sent::Later => Ok(FeedbackSendResult {
                    status: FeedbackStatus::Queued,
                    issue: None,
                    transcript: None,
                    limited: false,
                    left_out,
                }),
                Sent::Limited { wait, address } => {
                    if address {
                        paused_until = Some(Instant::now() + wait);
                    } else {
                        defer(&dir, wait);
                    }
                    Ok(FeedbackSendResult {
                        status: FeedbackStatus::Queued,
                        issue: None,
                        transcript: None,
                        limited: true,
                        left_out,
                    })
                }
                Sent::Refused(why) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    Err(ApiError::new(ErrorKind::InvalidValue, why))
                }
            },
        };
        let _ = engine
            .run(move |e| e.feedback_done(&report_id, answer))
            .await;
    }
}

impl Sender {
    /// Reports waiting to go, oldest first; ones older than a week are dropped.
    fn waiting(&self) -> Vec<PathBuf> {
        waiting(&self.outbox)
    }

    /// Tries a kept report again; how long to wait when the API is limiting
    /// this address. A report limited on its own waits in its folder.
    async fn retry(&self, dir: &Path) -> Option<Duration> {
        match self.send(dir).await {
            Sent::Done(result) => {
                self.remember(dir, &result);
                tracing::info!(issue = ?result.issue, "a kept feedback report was sent");
                let _ = std::fs::remove_dir_all(dir);
            }
            Sent::Refused(why) => {
                tracing::warn!(%why, "a kept feedback report was refused; it is dropped");
                let _ = std::fs::remove_dir_all(dir);
            }
            Sent::Later => {}
            Sent::Limited {
                wait,
                address: true,
            } => return Some(wait),
            Sent::Limited { wait, .. } => defer(dir, wait),
        }
        None
    }

    /// A suggestion sent with "tell me when it ships" (S73): kept, so this
    /// install can say so when it updates to the release that ships it. One
    /// credited to an account is told by email instead.
    fn remember(&self, dir: &Path, result: &FeedbackSendResult) {
        let Some(issue) = &result.issue else {
            return;
        };
        let Some(report) = std::fs::read(dir.join("report.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return;
        };
        if report["kind"] != "suggestion" || report["credit"] != true {
            return;
        }
        let title = report["text"]
            .as_str()
            .and_then(|t| t.lines().next())
            .unwrap_or_default();
        let by_email = self.signed_in();
        crate::shipped::remember(&self.sent, issue, title, by_email);
    }

    /// The follow-up keys of reports the agent filed, newest last.
    fn read_keys(&self) -> Vec<(String, String)> {
        std::fs::read(&self.keys)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Keeps a report's follow-up key so the agent can add to it later,
    /// after a restart too; only the newest are kept.
    fn keep_key(&self, report_id: &str, key: &str) -> std::io::Result<()> {
        let mut keys = self.read_keys();
        keys.retain(|(id, _)| id != report_id);
        keys.push((report_id.to_owned(), key.to_owned()));
        let skip = keys.len().saturating_sub(KEYS_KEPT);
        let kept: Vec<_> = keys.into_iter().skip(skip).collect();
        std::fs::write(&self.keys, json!(kept).to_string())
    }

    /// Adds to a report the agent filed (S294): `POST /v1/feedback/<id>/more`
    /// with its kept key. Not kept while offline: the agent is told.
    async fn add(&self, addition: FeedbackAddition) -> Result<FeedbackSendResult, ApiError> {
        let (Some(api), Some(client)) = (&self.api, &self.client) else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "feedback is turned off in this build",
            ));
        };
        let Some((_, key)) = self
            .read_keys()
            .into_iter()
            .find(|(id, _)| *id == addition.report_id)
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "this computer no longer has that report's key",
            ));
        };
        let mut form = Multipart::new();
        form.part(
            "addition",
            None,
            "application/json",
            json!({ "id": addition.id, "key": key, "text": addition.text })
                .to_string()
                .as_bytes(),
        );
        for file in &addition.attachments {
            let Some(bytes) = base64_decode(&file.data) else {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!("{} is not base64", file.name),
                ));
            };
            form.part(
                "attachment",
                Some(&quoted(&file.name)),
                "application/octet-stream",
                &bytes,
            );
        }
        let (content_type, body) = form.finish();
        let unreachable = || {
            ApiError::new(
                ErrorKind::Busy,
                "couldn't reach UXDuck's servers. Nothing was added",
            )
        };
        let total = body.len();
        let handed = Arc::new(AtomicUsize::new(0));
        let request = client
            .post(format!("{api}/v1/feedback/{}/more", addition.report_id))
            .timeout(upload_timeout(total))
            .header("Content-Type", content_type)
            .header(reqwest::header::CONTENT_LENGTH, total)
            .body(streamed(body, Arc::clone(&handed)));
        let response = tokio::select! {
            response = request.send() => response.map_err(|_| unreachable())?,
            () = watch(&handed, total, STALLED, UPLOAD_WAKE, |_| {}) => return Err(unreachable()),
        };
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        match status.as_u16() {
            200..=299 => Ok(FeedbackSendResult {
                status: match answer["status"].as_str() {
                    Some("received") => FeedbackStatus::Received,
                    Some("duplicate") => FeedbackStatus::Duplicate,
                    _ => FeedbackStatus::Filed,
                },
                issue: answer["issue"].as_str().map(str::to_owned),
                transcript: None,
                limited: false,
                left_out: None,
            }),
            404 => Err(ApiError::new(
                ErrorKind::NotFound,
                "the report hasn't reached UXDuck's servers yet. Try again once it's sent",
            )),
            429 => Err(ApiError::new(
                ErrorKind::Busy,
                "UXDuck's servers are taking no more from this network for now. Nothing was added. Try again later",
            )),
            500..=599 => Err(unreachable()),
            _ => Err(ApiError::new(
                ErrorKind::InvalidValue,
                answer["error"]
                    .as_str()
                    .unwrap_or("the addition was refused")
                    .to_owned(),
            )),
        }
    }

    fn signed_in(&self) -> bool {
        self.credentials
            .get(crate::account::TOKEN)
            .is_some_and(|t| !t.is_empty())
    }

    /// Writes the report and its files to the outbox before any sending.
    /// With the report's folder, what was left out of it for what the
    /// servers take, if anything was.
    async fn keep(
        &self,
        mut order: FeedbackOrder,
    ) -> std::io::Result<(PathBuf, Option<FeedbackLeftOut>)> {
        // The flight data first: what of it does not go is said in the
        // bundle.
        let (mut log, mut left_out) = (Vec::new(), FeedbackLeftOut::default());
        if let Some(flight) = order.flight.take() {
            let tlogs = self.tlogs.clone();
            let now = now_us();
            let read =
                tokio::task::spawn_blocking(move || last_minutes(&tlogs, &flight, now, MAX_FLIGHT))
                    .await
                    .map_err(std::io::Error::other)
                    .and_then(|read| read);
            (log, left_out) = asked_flight(read);
        }
        self.write(order, &log, left_out)
    }

    fn write(
        &self,
        order: FeedbackOrder,
        flight: &[u8],
        mut left_out: FeedbackLeftOut,
    ) -> std::io::Result<(PathBuf, Option<FeedbackLeftOut>)> {
        let dir = self.outbox.join(&order.report_id);
        std::fs::create_dir_all(&dir)?;
        let mut report = json!({
            "id": order.report_id,
            "kind": match order.kind {
                FeedbackKind::Problem => "problem",
                FeedbackKind::Idea => "idea",
                FeedbackKind::Other => "other",
                FeedbackKind::Suggestion => "suggestion",
            },
            "credit": order.credit,
            "fromAgent": order.from_agent,
            "agentFiled": order.agent_filed,
            "overMcp": order.over_mcp,
            "text": order.text,
            "email": order.reply_email,
            "name": order.reporter_name,
            "surface": order.surface,
            "via": "daemon",
            "context": context(&order.context, &left_out),
            "followUpKey": order.follow_up_key,
        });
        // One entry per `voice-<n>` file, in order: what the app wrote down.
        if !order.voice_notes.is_empty() {
            report["voiceNotes"] = order
                .voice_notes
                .iter()
                .map(|note| match &note.transcript {
                    Some(words) => json!({ "transcript": words }),
                    None => json!({}),
                })
                .collect();
        }
        std::fs::write(dir.join("report.json"), report.to_string())?;
        if order.agent_filed
            && let Some(key) = &order.follow_up_key
        {
            self.keep_key(&order.report_id, key)?;
        }
        if let Some(mut bundle) = order.bundle {
            if let Some(report) = bundle.get_mut("report").and_then(Value::as_object_mut) {
                report.insert("platform".into(), json!(crate::analytics::platform()));
                report.insert("arch".into(), json!(crate::analytics::arch()));
            }
            // The log is added here. Its lines left out the endpoints of the
            // links configured as each was kept.
            bundle["log"] = json!(crate::logs::tail());
            left_out = uxduck_engine::feedback::fit(&mut bundle, left_out);
            std::fs::write(dir.join("feedback-bundle.json"), bundle.to_string())?;
        }
        if let Some(image) = order.screenshot.as_deref().and_then(base64_decode) {
            std::fs::write(dir.join("screenshot"), image)?;
        }
        for (n, voice) in order.voice_notes.iter().enumerate() {
            let bytes = base64_decode(&voice.data).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("voice note {} is not base64", n + 1),
                )
            })?;
            std::fs::write(dir.join(format!("voice-{}", n + 1)), bytes)?;
        }
        // The person's files as `attachments/<n>`, their names beside them.
        if !order.attachments.is_empty() {
            let files = dir.join("attachments");
            std::fs::create_dir_all(&files)?;
            let mut names = Vec::new();
            for (n, file) in order.attachments.iter().enumerate() {
                let bytes = base64_decode(&file.data).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} is not base64", file.name),
                    )
                })?;
                std::fs::write(files.join(n.to_string()), bytes)?;
                names.push(file.name.clone());
            }
            std::fs::write(files.join("names.json"), json!(names).to_string())?;
        }
        if !flight.is_empty() {
            std::fs::write(dir.join("flight.tlog"), flight)?;
        }
        let left_out = (left_out != FeedbackLeftOut::default()).then_some(left_out);
        Ok((dir, left_out))
    }

    /// Posts one kept report as multipart, as the API takes it.
    async fn send(&self, dir: &Path) -> Sent {
        let (Some(api), Some(client)) = (&self.api, &self.client) else {
            return Sent::Refused("feedback is turned off in this build".into());
        };
        let Ok(report) = std::fs::read(dir.join("report.json")) else {
            return Sent::Refused("the kept report is unreadable".into());
        };
        let (report, voices) = match voice_notes(dir, report) {
            Ok(kept) => kept,
            Err(why) => return Sent::Refused(why),
        };
        let mut form = Multipart::new();
        form.part("report", None, "application/json", &report);
        for (name, file, kind) in [
            ("screenshot", "screenshot", "application/octet-stream"),
            ("bundle", "feedback-bundle.json", "application/json"),
            ("flight", "flight.tlog", "application/octet-stream"),
        ] {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                form.part(name, Some(file), kind, &bytes);
            }
        }
        for (file, bytes) in &voices {
            form.part("voice", Some(file), "application/octet-stream", bytes);
        }
        let names: Vec<String> = std::fs::read(dir.join("attachments/names.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        for (n, name) in names.iter().enumerate() {
            let Ok(bytes) = std::fs::read(dir.join("attachments").join(n.to_string())) else {
                return Sent::Refused(format!("the attached {name} is missing from the outbox"));
            };
            form.part(
                "attachment",
                Some(&quoted(name)),
                "application/octet-stream",
                &bytes,
            );
        }
        let (content_type, body) = form.finish();
        // The outbox keeps each report under its id.
        let report_id = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let total = body.len();
        let handed = Arc::new(AtomicUsize::new(0));
        let mut request = client
            .post(format!("{api}/v1/feedback"))
            .timeout(upload_timeout(total))
            .header("Content-Type", content_type)
            .header(reqwest::header::CONTENT_LENGTH, total)
            .body(streamed(body, Arc::clone(&handed)));
        // Signed in: the report is the account's, to see on any computer,
        // and credited when asked (S73). The API reads it from the token.
        // One the agent filed by itself goes without it (S294).
        let anonymous =
            serde_json::from_slice::<Value>(&report).is_ok_and(|kept| kept["agentFiled"] == true);
        if let Some(token) = self
            .credentials
            .get(crate::account::TOKEN)
            .filter(|t| !t.is_empty() && !anonymous)
        {
            request = request.bearer_auth(token);
        }
        let tell = |sent: usize| {
            let report_id = report_id.clone();
            self.engine
                .post(move |e| e.feedback_progress(&report_id, sent as u64, total as u64));
        };
        let response = tokio::select! {
            response = request.send() => response,
            () = watch(&handed, total, STALLED, UPLOAD_WAKE, tell) => {
                tracing::debug!("feedback: none of the report went out for a minute; kept");
                return Sent::Later;
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, "feedback: UXDuck's servers could not be reached; kept");
                return Sent::Later;
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| Duration::from_secs(seconds.min(LONGEST_PAUSE.as_secs())));
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        outcome(status.as_u16(), retry_after, &answer, self.retry)
    }
}

/// The file in a report's folder saying when the API will take it again,
/// as seconds since 1970, when the API limited that report on its own.
const NOT_BEFORE: &str = "not-before";

/// Keeps a report back for `wait` while the others still go.
fn defer(dir: &Path, wait: Duration) {
    let until = (SystemTime::now() + wait)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let _ = std::fs::write(dir.join(NOT_BEFORE), until.to_string());
}

/// When a deferred report may go again.
fn not_before(dir: &Path) -> Option<SystemTime> {
    let seconds = std::fs::read_to_string(dir.join(NOT_BEFORE)).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds.trim().parse().ok()?))
}

/// The kept reports that may go now, oldest first. Ones older than a week
/// are dropped.
fn waiting(outbox: &Path) -> Vec<PathBuf> {
    let now = SystemTime::now();
    kept(outbox)
        .into_iter()
        .filter(|dir| not_before(dir).is_none_or(|at| at <= now))
        .collect()
}

/// When the first deferred report may go again.
fn next_due(outbox: &Path) -> Option<SystemTime> {
    kept(outbox).iter().filter_map(|dir| not_before(dir)).min()
}

/// Every kept report, oldest first. Ones older than a week are dropped.
fn kept(outbox: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(outbox)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("report.json").exists())
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    dirs.retain(|(modified, dir)| {
        let stale = modified.elapsed().is_ok_and(|age| age > KEEP_FOR);
        if stale {
            let _ = std::fs::remove_dir_all(dir);
        }
        !stale
    });
    dirs.sort();
    dirs.into_iter().map(|(_, dir)| dir).collect()
}

/// What the API's answer to a report means for it. A 429 waits as long as
/// the API asks, at least `retry`. A 413, or any other 4xx, is refused for
/// good, because sending the same report again can never be taken.
fn outcome(status: u16, retry_after: Option<Duration>, answer: &Value, retry: Duration) -> Sent {
    match status {
        // Not set up on the server: no retry will help, so say so now.
        503 if answer["code"] == "not-configured" => Sent::Refused(
            answer["error"]
                .as_str()
                .unwrap_or("feedback isn't set up on UXDuck's servers")
                .to_owned(),
        ),
        200..=299 => {
            let status = match answer["status"].as_str() {
                Some("received") => FeedbackStatus::Received,
                Some("duplicate") => FeedbackStatus::Duplicate,
                Some("held") => FeedbackStatus::Held,
                _ => FeedbackStatus::Filed,
            };
            Sent::Done(Box::new(FeedbackSendResult {
                status,
                issue: answer["issue"].as_str().map(str::to_owned),
                transcript: answer["transcript"].as_str().map(str::to_owned),
                limited: false,
                left_out: None,
            }))
        }
        429 => {
            let wait = retry_after.unwrap_or(retry).max(retry);
            tracing::warn!(
                seconds = wait.as_secs(),
                "feedback: the API takes no more reports from this address for now; kept"
            );
            Sent::Limited {
                wait,
                address: answer["code"] == "address-limited",
            }
        }
        // A redirect is a network that wants a sign-in first, or
        // something else in the way: keep the report for later.
        300..=399 | 500..=599 => {
            tracing::warn!(
                status,
                "feedback: the API could not take the report now; kept"
            );
            Sent::Later
        }
        _ => Sent::Refused(
            answer["error"]
                .as_str()
                .unwrap_or("the report was refused")
                .to_owned(),
        ),
    }
}

/// A body sent in pieces, counting in `handed` how much of it has been
/// handed to the network. A sync push goes out this way too.
pub(crate) fn streamed(body: Vec<u8>, handed: Arc<AtomicUsize>) -> reqwest::Body {
    let total = body.len();
    let pieces = (0..total).step_by(UPLOAD_CHUNK).map(move |start| {
        let end = (start + UPLOAD_CHUNK).min(total);
        handed.store(end, Ordering::Relaxed);
        Ok::<_, std::io::Error>(body[start..end].to_vec())
    });
    reqwest::Body::wrap_stream(futures::stream::iter(pieces))
}

/// Watches an upload: `tell` hears the latest count every `PROGRESS_EVERY`,
/// as the network takes the body in bursts. Returns once none of the rest
/// has gone out for `stalled`. After the last piece it never returns: the
/// network may still hold megabytes of the body, and the request's own
/// timeout waits for the answer. `wake` names the upload where wakes are
/// counted.
pub(crate) async fn watch(
    handed: &AtomicUsize,
    total: usize,
    stalled: Duration,
    wake: &'static str,
    mut tell: impl FnMut(usize),
) {
    let (mut told, mut moved) = (0, Instant::now());
    loop {
        tokio::time::sleep(PROGRESS_EVERY).await;
        crate::wakes::count(wake);
        let sent = handed.load(Ordering::Relaxed);
        if sent != told {
            (told, moved) = (sent, Instant::now());
            tell(sent);
        }
        if sent == total {
            return std::future::pending().await;
        }
        if moved.elapsed() >= stalled {
            return;
        }
    }
}

/// The longest a request may take in all, as `STALLED` says.
pub(crate) fn upload_timeout(bytes: usize) -> Duration {
    STALLED + Duration::from_secs((bytes / SLOWEST_UPLOAD) as u64)
}

/// A kept report as sent, and its voice notes as (file name, bytes).
type KeptVoices = (Vec<u8>, Vec<(String, Vec<u8>)>);

/// A kept report's voice notes, in order: `voice-1`, `voice-2`, … as its
/// `voiceNotes` lists them. A report kept by an earlier version has one
/// `voice` file and no list; it goes as one note, listed for the API.
fn voice_notes(dir: &Path, report: Vec<u8>) -> Result<KeptVoices, String> {
    let mut value: Value =
        serde_json::from_slice(&report).map_err(|_| "the kept report is unreadable".to_owned())?;
    let listed = value["voiceNotes"].as_array().map_or(0, Vec::len);
    if listed == 0 {
        let Ok(bytes) = std::fs::read(dir.join("voice")) else {
            return Ok((report, Vec::new()));
        };
        value["voiceNotes"] = json!([{}]);
        return Ok((
            value.to_string().into_bytes(),
            vec![("voice".into(), bytes)],
        ));
    }
    let mut voices = Vec::with_capacity(listed);
    for n in 1..=listed {
        let file = format!("voice-{n}");
        let bytes = std::fs::read(dir.join(&file))
            .map_err(|_| format!("voice note {n} is missing from the outbox"))?;
        voices.push((file, bytes));
    }
    Ok((report, voices))
}

/// "macOS arm64", for the issue's first line.
fn platform_name() -> String {
    let os = match crate::analytics::platform() {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch = match crate::analytics::arch() {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os} {arch}")
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// The report's one line for people: the platform first, then what of the
/// flight data did not go, so whoever reads the issue knows before opening
/// anything, then the engine's line.
fn context(line: &str, left_out: &FeedbackLeftOut) -> String {
    let flight = match (&left_out.flight, &left_out.no_flight) {
        (Some(left), _) => format!("telemetry without its oldest {} · ", megabytes(left.bytes)),
        (None, Some(_)) => "telemetry asked for and not sent · ".to_owned(),
        (None, None) => String::new(),
    };
    format!("{} · {flight}{line}", platform_name())
}

/// "11.2 MB", as the window writes a size.
#[allow(clippy::cast_precision_loss, reason = "a size for people")]
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// A vehicle's last minutes as a tlog of their own, and what of them did
/// not go in it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Flight {
    pub log: Vec<u8>,
    pub left_out: Option<LeftOut>,
}

/// The vehicle's frames of the last five minutes from its link's tlogs, as
/// a tlog of their own of `most` bytes at most: the newest of them, which
/// are the ones nearest what a report is about, with what was left out,
/// how much and from when to when.
///
/// The logs are read backwards from the newest's end, a part at a time, and
/// on into the log before it where the link reopened inside the five
/// minutes, so none is held whole. Until 2026-11-15 the link's newest log
/// was read whole into memory, the first 20 MB of the five minutes were
/// kept, the newest left out, and nothing said so.
///
/// # Errors
/// When a log cannot be read.
pub(crate) fn last_minutes(
    tlogs: &Path,
    flight: &FlightRef,
    now_us: u64,
    most: usize,
) -> std::io::Result<Flight> {
    let since = now_us.saturating_sub(FLIGHT_US);
    // The newest part first, each part's frames in the order recorded.
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut kept = 0;
    let mut left_out: Option<LeftOut> = None;
    'logs: for path in crate::tlog_index::logs(tlogs, &flight.link_id) {
        let mut log = match crate::tlog_index::Backward::open(&path, None) {
            Ok(log) => log,
            // Removed since the folder was listed.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(named(&path, &error)),
        };
        while let Some((_, bytes)) = log.part().map_err(|error| named(&path, &error))? {
            let records: Vec<(u64, &[u8])> = uxduck_mavlink::tlog::records(bytes).collect();
            let mut part: Vec<(u64, &[u8])> = Vec::new();
            let mut before = false;
            for &(at, frame) in records.iter().rev() {
                if at < since {
                    before = true;
                    break;
                }
                if system_id(frame) != Some(flight.system_id) {
                    continue;
                }
                let len = 8 + frame.len();
                // Once one does not fit, none older goes in either: what is
                // kept has no gaps.
                match &mut left_out {
                    None if kept + len <= most => {
                        kept += len;
                        part.push((at, frame));
                    }
                    None => {
                        left_out = Some(LeftOut {
                            count: 1,
                            bytes: len as u64,
                            from_us: Some(at),
                            to_us: Some(at),
                            limit_bytes: most as u64,
                        });
                    }
                    Some(left) => {
                        left.count += 1;
                        left.bytes += len as u64;
                        left.from_us = Some(at);
                    }
                }
            }
            let mut out = Vec::with_capacity(part.iter().map(|(_, f)| 8 + f.len()).sum());
            for (at, frame) in part.into_iter().rev() {
                let _ = uxduck_mavlink::tlog::record(&mut out, at, frame);
            }
            parts.push(out);
            if before {
                break 'logs;
            }
        }
    }
    Ok(Flight {
        log: parts.into_iter().rev().flatten().collect(),
        left_out,
    })
}

/// The flight data a person asked for as it goes, and what is said of what
/// does not go: its oldest when it is more than the servers take, or why
/// none of it goes. The report goes either way, because the person's words
/// matter more than its files.
fn asked_flight(read: std::io::Result<Flight>) -> (Vec<u8>, FeedbackLeftOut) {
    let mut left_out = FeedbackLeftOut::default();
    let log = match read {
        Err(error) => {
            left_out.no_flight = Some(format!("UXDuck could not read its log {error}."));
            Vec::new()
        }
        Ok(flight) if flight.log.is_empty() => {
            left_out.no_flight = Some(
                "UXDuck's logs hold no telemetry from this vehicle in the last 5 minutes."
                    .to_owned(),
            );
            Vec::new()
        }
        Ok(flight) => {
            left_out.flight = flight.left_out;
            flight.log
        }
    };
    (log, left_out)
}

/// A log's reading error with the log's name, for the person.
fn named(path: &Path, error: &std::io::Error) -> std::io::Error {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    std::io::Error::new(error.kind(), format!("{name}: {error}"))
}

/// The sending system of a MAVLink v1 or v2 frame.
fn system_id(frame: &[u8]) -> Option<u8> {
    match frame.first()? {
        0xFE => frame.get(3).copied(),
        0xFD => frame.get(5).copied(),
        _ => None,
    }
}

/// Standard base64, with or without padding; a `data:` prefix is skipped.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.split_once(',').map_or(text, |(head, rest)| {
        if head.starts_with("data:") {
            rest
        } else {
            text
        }
    });
    let value = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        }))
    };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        buffer = (buffer << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// A file name fit for a `filename="…"` parameter: no quotes, backslashes or
/// line breaks. The API cleans names again on arrival.
fn quoted(name: &str) -> String {
    name.chars()
        .map(|c| {
            if matches!(c, '"' | '\\' | '\r' | '\n') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// A `multipart/form-data` body, built by hand: the API takes a few parts.
struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    fn new() -> Self {
        let mut bytes = [0u8; 12];
        let _ = getrandom::fill(&mut bytes);
        let boundary = bytes.iter().fold(String::from("uxduck-"), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        });
        Self {
            boundary,
            body: Vec::new(),
        }
    }

    fn part(&mut self, name: &str, file: Option<&str>, content_type: &str, bytes: &[u8]) {
        let disposition = file.map_or_else(
            || format!("form-data; name=\"{name}\""),
            |file| format!("form-data; name=\"{name}\"; filename=\"{file}\""),
        );
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: {disposition}\r\nContent-Type: {content_type}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
    }

    fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_too_large_ever_is_refused_and_a_limit_waits_as_asked() {
        let retry = Duration::from_secs(60);
        let too_large = json!({ "error": "These files come to 48.0 MB, more than UXDuck's servers take from everyone in a day, 10.0 MB, so they can never be sent." });
        assert!(
            matches!(outcome(413, None, &too_large, retry), Sent::Refused(why) if why.starts_with("These files come to 48.0 MB"))
        );
        let tomorrow = Duration::from_hours(9);
        assert!(matches!(
            outcome(429, Some(tomorrow), &json!({}), retry),
            Sent::Limited { wait, address: false } if wait == tomorrow
        ));
        assert!(matches!(
            outcome(429, None, &json!({ "code": "address-limited" }), retry),
            Sent::Limited { wait, address: true } if wait == retry
        ));
    }

    #[test]
    fn a_report_limited_on_its_own_waits_while_the_others_go() {
        let outbox = tempfile::tempdir().unwrap();
        for id in ["first", "second"] {
            std::fs::create_dir(outbox.path().join(id)).unwrap();
            std::fs::write(outbox.path().join(id).join("report.json"), "{}").unwrap();
        }
        let first = outbox.path().join("first");
        let second = outbox.path().join("second");
        assert_eq!(waiting(outbox.path()).len(), 2);
        assert_eq!(next_due(outbox.path()), None);

        defer(&first, Duration::from_hours(9));
        assert_eq!(waiting(outbox.path()), std::slice::from_ref(&second));
        let due = next_due(outbox.path()).expect("the first is due later");
        let wait = due.duration_since(SystemTime::now()).unwrap();
        assert!(wait > Duration::from_hours(8) && wait <= Duration::from_hours(9));

        defer(&first, Duration::ZERO);
        let mut both = waiting(outbox.path());
        both.sort();
        assert_eq!(both, [first, second]);
    }

    #[test]
    fn base64_decodes_standard_and_data_urls() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(
            base64_decode("data:image/webp;base64,aGVsbG8").as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(base64_decode("not base64!"), None);
    }

    #[test]
    fn kept_voice_notes_go_in_order_with_their_list() {
        let dir = tempfile::tempdir().unwrap();
        let report = json!({
            "id": "r1",
            "voiceNotes": [{ "transcript": "first" }, {}],
        })
        .to_string()
        .into_bytes();
        std::fs::write(dir.path().join("voice-1"), b"one").unwrap();
        std::fs::write(dir.path().join("voice-2"), b"two").unwrap();
        let (sent, voices) = voice_notes(dir.path(), report.clone()).unwrap();
        assert_eq!(sent, report, "the kept report goes as it is");
        assert_eq!(
            voices,
            [
                ("voice-1".to_owned(), b"one".to_vec()),
                ("voice-2".to_owned(), b"two".to_vec())
            ]
        );

        std::fs::remove_file(dir.path().join("voice-2")).unwrap();
        assert_eq!(
            voice_notes(dir.path(), report).unwrap_err(),
            "voice note 2 is missing from the outbox"
        );
    }

    #[test]
    fn a_report_kept_by_an_earlier_version_sends_its_one_voice_note() {
        let dir = tempfile::tempdir().unwrap();
        let report = json!({ "id": "r1", "text": "" }).to_string().into_bytes();
        let (sent, voices) = voice_notes(dir.path(), report.clone()).unwrap();
        assert_eq!(sent, report);
        assert!(voices.is_empty(), "no voice note, none sent");

        std::fs::write(dir.path().join("voice"), b"old").unwrap();
        let (sent, voices) = voice_notes(dir.path(), report).unwrap();
        let sent: Value = serde_json::from_slice(&sent).unwrap();
        assert_eq!(sent["voiceNotes"], json!([{}]));
        assert_eq!(sent["id"], "r1");
        assert_eq!(voices, [("voice".to_owned(), b"old".to_vec())]);
    }

    #[tokio::test]
    async fn an_upload_is_given_up_once_it_stops_and_not_while_it_waits_for_its_answer() {
        let handed = AtomicUsize::new(0);
        let stalled = Duration::from_millis(600);
        let started = Instant::now();
        let mut told = Vec::new();
        let moving = async {
            for sent in [100, 200, 300] {
                tokio::time::sleep(Duration::from_millis(300)).await;
                handed.store(sent, Ordering::Relaxed);
            }
        };
        tokio::join!(
            watch(&handed, 1000, stalled, UPLOAD_WAKE, |sent| told.push(sent)),
            moving
        );
        assert!(
            started.elapsed() >= Duration::from_millis(900) + stalled,
            "not given up while it moved"
        );
        assert_eq!(told.last(), Some(&300), "{told:?}");
        assert!(told.is_sorted(), "{told:?}");

        handed.store(1000, Ordering::Relaxed);
        let waited = tokio::time::timeout(
            Duration::from_secs(2),
            watch(&handed, 1000, stalled, UPLOAD_WAKE, |_| {}),
        )
        .await;
        assert!(
            waited.is_err(),
            "all of it handed over, the request waits for the answer"
        );
        assert_eq!(
            upload_timeout(25 * 1024 * 1024),
            Duration::from_secs(60 + 800),
            "25 MB may take 14 minutes"
        );
    }

    #[test]
    fn the_flight_is_the_vehicles_last_five_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let frame = |sysid: u8| {
            // A minimal v2 HEARTBEAT header with an empty payload.
            vec![0xFD, 0, 0, 0, 0, sysid, 1, 0, 0, 0, 0xAA, 0xBB]
        };
        let now = 1_000 * 60 * 1_000_000;
        let mut log = Vec::new();
        uxduck_mavlink::tlog::record(&mut log, now - 6 * 60 * 1_000_000, &frame(1)).unwrap();
        uxduck_mavlink::tlog::record(&mut log, now - 60 * 1_000_000, &frame(1)).unwrap();
        uxduck_mavlink::tlog::record(&mut log, now - 30 * 1_000_000, &frame(2)).unwrap();
        std::fs::write(dir.path().join("li1-100.tlog"), &log).unwrap();
        std::fs::write(dir.path().join("li2-200.tlog"), &log).unwrap();
        let cut = last_minutes(dir.path(), &vehicle_one(), now, MAX_FLIGHT).unwrap();
        let records = uxduck_mavlink::tlog::read(&cut.log);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, now - 60 * 1_000_000);
        assert_eq!(cut.left_out, None, "all of it went");
    }

    fn vehicle_one() -> FlightRef {
        FlightRef {
            link_id: "li1".into(),
            system_id: 1,
        }
    }

    /// A v2 frame of 20 bytes from `sysid`, so a record of 28.
    fn frame(sysid: u8) -> Vec<u8> {
        let mut frame = vec![0xFD, 8, 0, 0, 0, sysid, 1, 0, 0, 0];
        frame.extend([7; 8]);
        frame.extend([0xAA, 0xBB]);
        frame
    }

    const SECOND: u64 = 1_000_000;

    /// More flight data than the servers take: the newest of it goes, as
    /// what is nearest the report, and what did not is counted. Until
    /// 2026-11-15 the oldest went, the newest was left out and nothing
    /// said so.
    #[test]
    fn flight_data_over_what_is_taken_keeps_the_newest_and_says_what_went() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000 * 60 * SECOND;
        let mut log = Vec::new();
        // One frame a second for the last 100 seconds, another vehicle's
        // between them.
        for n in (1..=100).rev() {
            uxduck_mavlink::tlog::record(&mut log, now - n * SECOND, &frame(1)).unwrap();
            uxduck_mavlink::tlog::record(&mut log, now - n * SECOND + 1, &frame(2)).unwrap();
        }
        std::fs::write(dir.path().join("li1-100.tlog"), &log).unwrap();
        // Room for ten records and a half.
        let most = 10 * 28 + 14;
        let cut = last_minutes(dir.path(), &vehicle_one(), now, most).unwrap();
        let records = uxduck_mavlink::tlog::read(&cut.log);
        assert_eq!(
            records.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
            (1..=10).rev().map(|n| now - n * SECOND).collect::<Vec<_>>(),
            "the last ten seconds, in the order recorded"
        );
        assert!(records.iter().all(|(_, f)| *f == frame(1).as_slice()));
        assert_eq!(
            cut.left_out,
            Some(LeftOut {
                count: 90,
                bytes: 90 * 28,
                from_us: Some(now - 100 * SECOND),
                to_us: Some(now - 11 * SECOND),
                limit_bytes: most as u64,
            })
        );
        // With no bound, as a plugin's test reads it: all of it.
        let all = last_minutes(dir.path(), &vehicle_one(), now, usize::MAX).unwrap();
        assert_eq!(uxduck_mavlink::tlog::read(&all.log).len(), 100);
        assert_eq!(all.left_out, None);
    }

    /// A link that reopened inside the five minutes wrote two logs: the
    /// flight data goes on into the earlier one. Until 2026-11-15 only the
    /// newest log was read.
    #[test]
    fn the_flight_goes_on_into_the_log_before_and_a_long_log_is_read_from_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000 * 60 * SECOND;
        // An earlier log of 4 MB, most of it older than the five minutes.
        let mut earlier = Vec::new();
        let old = 150_000;
        for n in 0..old {
            uxduck_mavlink::tlog::record(&mut earlier, now - 3_600 * SECOND + n, &frame(1))
                .unwrap();
        }
        for n in (101..=200).rev() {
            uxduck_mavlink::tlog::record(&mut earlier, now - n * SECOND, &frame(1)).unwrap();
        }
        let mut newest = Vec::new();
        for n in (1..=50).rev() {
            uxduck_mavlink::tlog::record(&mut newest, now - n * SECOND, &frame(1)).unwrap();
        }
        std::fs::write(dir.path().join("li1-100.tlog"), &earlier).unwrap();
        std::fs::write(dir.path().join("li1-200.tlog"), &newest).unwrap();
        let cut = last_minutes(dir.path(), &vehicle_one(), now, MAX_FLIGHT).unwrap();
        let stamps: Vec<u64> = uxduck_mavlink::tlog::read(&cut.log)
            .iter()
            .map(|(at, _)| *at)
            .collect();
        assert_eq!(stamps.len(), 150);
        assert!(stamps.is_sorted(), "in the order recorded");
        assert_eq!(stamps[0], now - 200 * SECOND);
        assert_eq!(stamps[149], now - SECOND);
        assert_eq!(cut.left_out, None);
    }

    /// A log that is not records is an error with the log's name, which
    /// the person sending the report is told. Until 2026-11-15 the report
    /// went without its flight data and nothing said so.
    #[test]
    fn a_log_that_cannot_be_read_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000 * 60 * SECOND;
        let mut log = vec![0x55; 4_000];
        uxduck_mavlink::tlog::record(&mut log, now - SECOND, &frame(1)).unwrap();
        log.extend([0x55; 4_000]);
        std::fs::write(dir.path().join("li1-100.tlog"), &log).unwrap();
        let error = last_minutes(dir.path(), &vehicle_one(), now, MAX_FLIGHT).unwrap_err();
        assert_eq!(
            error.to_string(),
            "li1-100.tlog: no MAVLink record ends at byte 8028"
        );
        // No log at all, and one with nothing from the vehicle, are not
        // errors: there is nothing to send, which the caller says.
        let none = tempfile::tempdir().unwrap();
        assert_eq!(
            last_minutes(none.path(), &vehicle_one(), now, MAX_FLIGHT).unwrap(),
            Flight::default()
        );
    }

    /// Flight data that was asked for and cannot go is said in words, and
    /// the report goes without it. Until 2026-11-15 nothing said so.
    #[test]
    fn flight_data_that_cannot_go_is_said_in_words() {
        let none = asked_flight(Ok(Flight::default()));
        assert!(none.0.is_empty());
        assert_eq!(
            none.1.no_flight.as_deref(),
            Some("UXDuck's logs hold no telemetry from this vehicle in the last 5 minutes.")
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("li1-100.tlog"), [0x55; 100]).unwrap();
        let unread = asked_flight(last_minutes(dir.path(), &vehicle_one(), 0, MAX_FLIGHT));
        assert!(unread.0.is_empty());
        assert_eq!(
            unread.1.no_flight.as_deref(),
            Some("UXDuck could not read its log li1-100.tlog: no MAVLink record ends at byte 100.")
        );
        let mut log = Vec::new();
        uxduck_mavlink::tlog::record(&mut log, 5, &frame(1)).unwrap();
        let whole = asked_flight(Ok(Flight {
            log: log.clone(),
            left_out: None,
        }));
        assert_eq!(whole, (log, FeedbackLeftOut::default()));
    }

    #[test]
    fn the_context_line_says_what_of_the_flight_data_did_not_go() {
        let left = |bytes| LeftOut {
            count: 1,
            bytes,
            from_us: None,
            to_us: None,
            limit_bytes: MAX_FLIGHT as u64,
        };
        let cut = FeedbackLeftOut {
            flight: Some(left(11 * 1024 * 1024 + 200_000)),
            ..FeedbackLeftOut::default()
        };
        assert!(
            context("UXDuck 0.17.0 · no vehicle", &cut)
                .ends_with(" · telemetry without its oldest 11.2 MB · UXDuck 0.17.0 · no vehicle")
        );
        let none = FeedbackLeftOut {
            no_flight: Some("why".into()),
            ..FeedbackLeftOut::default()
        };
        assert!(
            context("UXDuck 0.17.0", &none)
                .ends_with(" · telemetry asked for and not sent · UXDuck 0.17.0")
        );
        let whole = context("UXDuck 0.17.0", &FeedbackLeftOut::default());
        assert!(!whole.contains("telemetry"), "{whole}");
    }
}

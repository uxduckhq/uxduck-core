//! Recordings made on the vehicle (S429, ADR-0066 R12): the engine's
//! [`RecordingsPort`], over each camera computer's pinned SSH with the
//! system's own `ssh`, as checks, camera changes and terminals go
//! ([`companions::ssh`]). No `rsync`: Windows has none, and one way to a
//! computer is easier to keep right than two.
//!
//! A listing runs the camera adapter (`companions/camera.py`) with
//! `{"recordings": true}`, which reads `MediaMTX`'s `recordPath` for each
//! record path and groups its segments into recordings.
//!
//! A download copies each segment with `tail -c +<offset>` into its own
//! `.part` file under `<data dir>/vehicle-recordings/<transfer id>/`, so
//! a dropped link goes on from what arrived, and a restart of UXDuck from
//! the files on disk. It tries again while the computer cannot be reached,
//! waiting longer each time, and tries the computer's addresses in the
//! order the person gave them each time, so a fast link that comes back is
//! used again. Each segment is checked against `sha256sum` on the
//! computer, and one that does not match is copied again. Only when every
//! segment matches are they joined, without re-encoding, into one file in
//! the media folder, and the engine catalogues it. Nothing partial is ever
//! kept as finished. While this computer's connection is metered and Wait
//! for unmetered networks is on, a download waits unless a person chose to
//! go on (ADR-0046 B12).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;
use uxduck_core::domain::companion::CompanionComputer;
use uxduck_core::domain::companion_cameras::VehicleRecordMode;
use uxduck_core::domain::files::{FromComputer, RemoteSegment};
use uxduck_engine::vehicle_recordings::{
    DownloadReport, KeptRecording, ListedCamera, ListedRecording, RecordingOrder, RecordingsPort,
};

use crate::companions::{self, HostKeys, Session};
use crate::daemon::engine_task::EngineHandle;

/// The first wait before trying a computer again, doubled after each miss.
const AGAIN_FIRST: Duration = Duration::from_secs(5);
/// The longest wait between tries: a computer back in range is found within
/// a minute, and one out of range is asked at most once a minute.
const AGAIN_MOST: Duration = Duration::from_secs(60);
/// How often a download held by a metered connection looks again.
const METERED_AGAIN: Duration = Duration::from_secs(30);
/// How often progress goes to the engine, which journals once a second.
const REPORT_EVERY: Duration = Duration::from_millis(500);
/// A copy that receives nothing for this long has lost its link, which
/// TCP can take minutes to say: it is stopped and tried again.
const STALLED: Duration = Duration::from_secs(30);
/// How long the computer may take to hash a segment or list its
/// recordings, which reads every segment's fragments: minutes at worst for
/// a large card on a Raspberry Pi's SD card.
const SLOW_ANSWER: Duration = Duration::from_secs(600);
/// What one read of `ssh`'s output takes at most.
const READ: usize = 256 * 1024;

/// Where downloads keep their part files.
#[must_use]
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("vehicle-recordings")
}

/// A running download: how to stop it, and the lock it holds while it
/// touches its part files.
struct Running {
    stop: watch::Sender<bool>,
    files: Arc<tokio::sync::Mutex<()>>,
}

pub struct Host {
    data_dir: PathBuf,
    media: PathBuf,
    engine: EngineHandle,
    running: Arc<Mutex<HashMap<String, Running>>>,
    /// One download at a time from each computer, in the order asked, so
    /// the link is not split between them (ADR-0027 F4).
    computers: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, media: &Path, engine: EngineHandle) -> Self {
        // Sign-in files of a daemon that was killed: no key outlives the
        // run it was written for by more than a start.
        let _ = std::fs::remove_dir_all(data_dir.join("companions").join(RUNS));
        Self {
            data_dir: data_dir.to_owned(),
            media: media.to_owned(),
            engine,
            running: Arc::default(),
            computers: HashMap::new(),
        }
    }

    fn ctx(&self) -> Result<Ctx, String> {
        Ok(Ctx {
            dir: self.data_dir.join("companions"),
            program: companions::ssh_program().ok_or_else(companions::ssh_missing)?,
            data_dir: self.data_dir.clone(),
        })
    }
}

/// The folder of each `ssh`'s own files, under `<data dir>/companions/`.
const RUNS: &str = "recording-runs";

#[derive(Clone)]
struct Ctx {
    dir: PathBuf,
    program: PathBuf,
    data_dir: PathBuf,
}

impl RecordingsPort for Host {
    #[allow(
        clippy::too_many_lines,
        reason = "each order and its task in one place"
    )]
    fn request(&mut self, order: RecordingOrder) {
        match order {
            RecordingOrder::List {
                id,
                computer,
                through,
            } => {
                let engine = self.engine.clone();
                let ctx = self.ctx();
                tokio::spawn(async move {
                    let result = match ctx {
                        Ok(ctx) => list(&ctx, &computer, through.as_deref()).await,
                        Err(why) => Err(why),
                    };
                    engine.post(move |e| e.recordings_listed(&id, result));
                });
            }
            RecordingOrder::Download {
                transfer_id,
                computer,
                through,
                from,
            } => {
                let (stop, stopped) = watch::channel(false);
                let files = Arc::new(tokio::sync::Mutex::new(()));
                let earlier = self
                    .running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        transfer_id.clone(),
                        Running {
                            stop,
                            files: files.clone(),
                        },
                    );
                if let Some(earlier) = earlier {
                    let _ = earlier.stop.send(true);
                }
                let lane = self
                    .computers
                    .entry(computer.id.clone())
                    .or_default()
                    .clone();
                let (reports, mut reported) = tokio::sync::mpsc::unbounded_channel();
                let engine = self.engine.clone();
                let id = transfer_id.clone();
                tokio::spawn(async move {
                    while let Some(report) = reported.recv().await {
                        let id = id.clone();
                        engine.post(move |e| e.recording_download_reported(&id, report));
                    }
                });
                let download = Download {
                    ctx: self.ctx(),
                    parts: dir(&self.data_dir).join(&transfer_id),
                    media: self.media.clone(),
                    engine: self.engine.clone(),
                    reports,
                    computer: *computer,
                    through: through.map(|t| *t),
                    from,
                    stopped,
                };
                let running = self.running.clone();
                tokio::spawn(async move {
                    let _lane = lane.lock().await;
                    let _files = files.lock().await;
                    download.run().await;
                    let mut running = running.lock().unwrap_or_else(PoisonError::into_inner);
                    if running
                        .get(&transfer_id)
                        .is_some_and(|r| Arc::ptr_eq(&r.files, &files))
                    {
                        running.remove(&transfer_id);
                    }
                });
            }
            RecordingOrder::Stop {
                transfer_id,
                discard,
            } => {
                let running = self
                    .running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&transfer_id);
                let parts = dir(&self.data_dir).join(&transfer_id);
                let files = running.map(|running| {
                    let _ = running.stop.send(true);
                    running.files
                });
                if discard {
                    tokio::spawn(async move {
                        // After the download has let go of its files.
                        let _held = match &files {
                            Some(files) => Some(files.lock().await),
                            None => None,
                        };
                        if let Err(error) = std::fs::remove_dir_all(&parts)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!(%error, path = %parts.display(), "cannot remove a cancelled download's part files");
                        }
                    });
                }
            }
        }
    }
}

/// Until someone stops it.
async fn stop_asked(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

/// One word for a shell, whatever it holds.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Bytes as a person reads them: "450 MB", "1.4 GB".
fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a size to read, not to count")]
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1000.0 {
        format!("{:.1} GB", mb / 1000.0)
    } else if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

/// Why one `ssh` did not finish.
#[derive(Debug)]
enum Missed {
    /// Someone stopped it.
    Stopped,
    /// It may work later: the computer was not reached, the link dropped
    /// or went quiet.
    Again(String),
    /// Trying again will not help, such as a changed host key.
    Never(String),
}

/// What a signed-in `ssh` ended with.
struct Finished {
    status: Option<i32>,
    stderr: String,
}

/// Runs `remote` on `computer` through its exact pinned host key, its
/// output written to `out` and counted with `on_bytes`, at each route in
/// turn until one reaches it, signing in each pinned way. A route is left
/// for the next only when `ssh` never signed in, so nothing runs twice.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one ssh, its routes and ways in one scope"
)]
async fn over_ssh(
    ctx: &Ctx,
    computer: &CompanionComputer,
    through: Option<&CompanionComputer>,
    remote: &str,
    input: Option<&[u8]>,
    out: &mut (dyn AsyncWrite + Unpin + Send),
    on_bytes: &mut (dyn FnMut(u64) + Send),
    stopped: &mut watch::Receiver<bool>,
    quiet: Duration,
    then: &str,
) -> Result<Finished, Missed> {
    let pinned = computer
        .check
        .as_ref()
        .and_then(|c| c.host_key.clone())
        .ok_or_else(|| {
            Missed::Never(format!(
                "Check {} first, so UXDuck knows its host key.",
                computer.name
            ))
        })?;
    let (pasted, own) =
        companions::saved_sign_in(&ctx.data_dir, computer).map_err(Missed::Never)?;
    let through_key =
        companions::saved_through_key(&ctx.data_dir, through).map_err(Missed::Never)?;
    let root = ctx.dir.join(RUNS);
    let files = std::fs::create_dir_all(&root)
        .and_then(|()| tempfile::Builder::new().tempdir_in(&root))
        .map_err(|e| Missed::Never(format!("Cannot make a folder in {}: {e}", root.display())))?;
    let trusted = files.path().join("known_hosts");
    match companions::trust_pinned(&ctx.dir, computer, &pinned, &trusted).await {
        Err(why) => return Err(Missed::Never(why)),
        Ok(false) => {
            return Err(Missed::Never(format!(
                "UXDuck no longer has the host key it pinned for {}. Check it again on its vehicle's page.",
                computer.name
            )));
        }
        Ok(true) => {}
    }
    let key = files.path().join("key");
    let log = files.path().join("ssh.log");
    let hop_key = files.path().join("through_key");
    let hop_hosts = files.path().join("through_known_hosts");
    let hop_log = files.path().join("through.log");
    let through = through.map(|computer| companions::Through {
        computer,
        pasted: through_key.as_deref(),
        own: Some(&own),
    });
    let mut missed = Vec::new();
    let mut trusted_hop = false;
    'routes: for route in companions::routes(
        computer,
        through.as_ref(),
        &hop_key,
        &hop_hosts,
        &hop_log,
        false,
    ) {
        if let Some(hop) = route.hop
            && !trusted_hop
        {
            if let Err(why) = companions::trust_through(&ctx.dir, hop).await {
                missed.push(why);
                break;
            }
            trusted_hop = true;
        }
        let mut said = String::new();
        for way in companions::pinned_ways(pasted.as_deref(), Some(&own)) {
            let _ = std::fs::remove_file(&log);
            let _ = std::fs::remove_file(&hop_log);
            let built = companions::ssh(
                &key,
                &files.path().join("askpass"),
                None,
                computer,
                route,
                way,
                HostKeys::Pinned(&trusted),
                Session::Terminal {
                    tty: false,
                    log: &log,
                },
                Some(remote),
            )
            .map_err(Missed::Never)?;
            let mut child = tokio::process::Command::new(&ctx.program)
                .args(&built.args)
                .envs(built.envs.iter().map(|(name, value)| (*name, value)))
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| Missed::Never(format!("Cannot run ssh: {e}")))?;
            if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
                let input = input.to_vec();
                tokio::spawn(async move {
                    let _ = stdin.write_all(&input).await;
                    let _ = stdin.shutdown().await;
                });
            }
            let mut stderr = child.stderr.take();
            let errors = tokio::spawn(async move {
                let mut text = String::new();
                if let Some(stderr) = stderr.as_mut() {
                    let _ = stderr.read_to_string(&mut text).await;
                }
                text
            });
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| Missed::Never("ssh has no output".into()))?;
            let mut buffer = vec![0u8; READ];
            loop {
                tokio::select! {
                    () = stop_asked(stopped) => {
                        let _ = child.kill().await;
                        return Err(Missed::Stopped);
                    }
                    read = tokio::time::timeout(quiet, stdout.read(&mut buffer)) => match read {
                        Err(_) => {
                            let _ = child.kill().await;
                            return Err(Missed::Again(format!(
                                "{} sent nothing for {} s",
                                computer.name,
                                quiet.as_secs()
                            )));
                        }
                        Ok(Err(error)) => {
                            let _ = child.kill().await;
                            return Err(Missed::Again(format!("The copy from {} broke off: {error}", computer.name)));
                        }
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            out.write_all(&buffer[..n]).await.map_err(|e| {
                                Missed::Never(format!("UXDuck could not write the copy on this computer: {e}"))
                            })?;
                            on_bytes(n as u64);
                        }
                    }
                }
            }
            let status = child.wait().await.ok().and_then(|s| s.code());
            let stderr = errors.await.unwrap_or_default();
            let _ = std::fs::remove_file(&key);
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                || text.contains("Host key verification failed")
            {
                return Err(Missed::Never(format!(
                    "{}'s SSH host key no longer matches the one UXDuck pinned. Check it again on its vehicle's page.",
                    computer.name
                )));
            }
            if companions::signed_in(&text) {
                return Ok(Finished { status, stderr });
            }
            if companions::refused(&text) {
                said = text;
                continue;
            }
            let hop_said = std::fs::read_to_string(&hop_log).unwrap_or_default();
            if companions::not_reached(status, &text, &hop_said) {
                missed.push(format!(
                    "{route}: {}",
                    companions::unreached_why(
                        &format!("{text}\n{hop_said}"),
                        route.hop.map(|hop| hop.through.computer.name.as_str()),
                    )
                ));
                continue 'routes;
            }
            return Err(Missed::Again(companions::terminal_failure(computer, &text)));
        }
        return Err(Missed::Never(companions::terminal_failure(computer, &said)));
    }
    Err(Missed::Again(companions::cannot_reach(
        computer, &missed, then,
    )))
}

/// The camera adapter's own reason, when it gave one.
fn adapter_failure(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("uxduck-camera: "))
        .map_or_else(
            || "Listing the recordings failed. The camera computer needs Linux with systemd, Python 3 and MediaMTX's API.".to_owned(),
            |reason| reason.trim().to_owned(),
        )
}

async fn list(
    ctx: &Ctx,
    computer: &CompanionComputer,
    through: Option<&CompanionComputer>,
) -> Result<Vec<ListedCamera>, String> {
    let script = include_str!("companions/camera.py").replace('\'', "'\"'\"'");
    let remote = format!("python3 -c '{script}'");
    let mut out = Vec::new();
    let (_kept, mut never) = watch::channel(false);
    let finished = over_ssh(
        ctx,
        computer,
        through,
        &remote,
        Some(br#"{"recordings": true}"#),
        &mut out,
        &mut |_| {},
        &mut never,
        SLOW_ANSWER,
        "Nothing was listed.",
    )
    .await
    .map_err(|missed| match missed {
        Missed::Stopped => "Stopped.".to_owned(),
        Missed::Again(why) | Missed::Never(why) => why,
    })?;
    if finished.status != Some(0) {
        return Err(adapter_failure(&finished.stderr));
    }
    let text = String::from_utf8_lossy(&out);
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("recordings="))
        .ok_or_else(|| adapter_failure(&finished.stderr))?;
    listed(json)
}

/// The camera adapter's listing, as the engine takes it.
fn listed(json: &str) -> Result<Vec<ListedCamera>, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Answer {
        cameras: Vec<Camera>,
        problem: Option<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(clippy::struct_field_names, reason = "the camera adapter's own names")]
    struct Camera {
        camera: String,
        path: String,
        mode: Option<VehicleRecordMode>,
        folder: Option<String>,
        #[serde(default)]
        recordings: Vec<Recording>,
        problem: Option<String>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(clippy::struct_field_names, reason = "the camera adapter's own names")]
    struct Recording {
        id: String,
        started_at_us: u64,
        duration_us: u64,
        segments: Vec<RemoteSegment>,
        #[serde(default)]
        recording: bool,
        not_downloadable: Option<String>,
    }
    let answer: Answer = serde_json::from_str(json)
        .map_err(|e| format!("The camera computer's list of recordings did not read: {e}"))?;
    if answer.cameras.is_empty()
        && let Some(problem) = answer.problem
    {
        return Err(problem);
    }
    Ok(answer
        .cameras
        .into_iter()
        .map(|c| ListedCamera {
            camera: c.camera,
            path: c.path,
            folder: c.folder,
            mode: c.mode,
            problem: c.problem,
            recordings: c
                .recordings
                .into_iter()
                .map(|r| ListedRecording {
                    id: r.id,
                    started_at_us: r.started_at_us,
                    duration_us: r.duration_us,
                    segments: r.segments,
                    recording: r.recording,
                    not_downloadable: r.not_downloadable,
                })
                .collect(),
        })
        .collect())
}

/// Why one try at a download ended.
enum Ended {
    Stopped,
    /// Try again after a wait.
    Again(String),
    /// Try again now: a segment did not match and was removed.
    Now(String),
    Failed(String),
}

impl From<Missed> for Ended {
    fn from(missed: Missed) -> Self {
        match missed {
            Missed::Stopped => Self::Stopped,
            Missed::Again(why) => Self::Again(why),
            Missed::Never(why) => Self::Failed(why),
        }
    }
}

struct Download {
    ctx: Result<Ctx, String>,
    parts: PathBuf,
    media: PathBuf,
    /// Asked whether Wait for unmetered networks is on.
    engine: EngineHandle,
    reports: tokio::sync::mpsc::UnboundedSender<DownloadReport>,
    computer: CompanionComputer,
    through: Option<CompanionComputer>,
    from: FromComputer,
    stopped: watch::Receiver<bool>,
}

impl Download {
    fn report(&self, report: DownloadReport) {
        let _ = self.reports.send(report);
    }

    fn part(&self, index: usize) -> PathBuf {
        self.parts.join(format!("{index}.part"))
    }

    /// Bytes kept so far across the segments.
    fn done(&self) -> u64 {
        self.from
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                std::fs::metadata(self.part(i))
                    .map_or(0, |m| m.len())
                    .min(s.size)
            })
            .sum()
    }

    /// Waits `for`, or until stopped: true when stopped.
    async fn wait(&mut self, r#for: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(r#for) => false,
            () = stop_asked(&mut self.stopped) => true,
        }
    }

    async fn run(mut self) {
        let ctx = match self.ctx.clone() {
            Ok(ctx) => ctx,
            Err(why) => return self.report(DownloadReport::Failed { detail: why }),
        };
        let mut again = AGAIN_FIRST;
        let mut mismatched = std::collections::HashSet::new();
        loop {
            if *self.stopped.borrow() {
                return;
            }
            if !self.from.over_metered && self.metered().await {
                self.report(DownloadReport::Waiting {
                    detail: "This computer's connection is metered, so the download waits for an unmetered one.".into(),
                    done: self.done(),
                    metered: true,
                });
                if self.wait(METERED_AGAIN).await {
                    return;
                }
                continue;
            }
            match self.attempt(&ctx, &mut mismatched).await {
                Ok(kept) => {
                    self.report(DownloadReport::Kept(kept));
                    if let Err(error) = std::fs::remove_dir_all(&self.parts) {
                        tracing::warn!(%error, path = %self.parts.display(), "cannot remove a finished download's part files");
                    }
                    return;
                }
                Err(Ended::Stopped) => return,
                Err(Ended::Failed(detail)) => {
                    return self.report(DownloadReport::Failed { detail });
                }
                Err(Ended::Now(detail)) => self.report(DownloadReport::Waiting {
                    detail,
                    done: self.done(),
                    metered: false,
                }),
                Err(Ended::Again(why)) => {
                    self.report(DownloadReport::Waiting {
                        detail: format!(
                            "{} Trying again in {} s.",
                            why.trim_end_matches('.').to_owned() + ".",
                            again.as_secs()
                        ),
                        done: self.done(),
                        metered: false,
                    });
                    if self.wait(again).await {
                        return;
                    }
                    again = (again * 2).min(AGAIN_MOST);
                }
            }
        }
    }

    /// Whether to wait for an unmetered connection now.
    async fn metered(&self) -> bool {
        let waits = self
            .engine
            .run(|e| e.waits_for_unmetered())
            .await
            .unwrap_or(true);
        waits
            && tokio::task::spawn_blocking(crate::metered::metered)
                .await
                .ok()
                .flatten()
                == Some(true)
    }

    #[allow(clippy::too_many_lines, reason = "one try at every segment in order")]
    async fn attempt(
        &mut self,
        ctx: &Ctx,
        mismatched: &mut std::collections::HashSet<usize>,
    ) -> Result<KeptRecording, Ended> {
        std::fs::create_dir_all(&self.parts).map_err(|e| {
            Ended::Failed(format!(
                "Cannot keep the download in {}: {e}",
                self.parts.display()
            ))
        })?;
        let total: u64 = self.from.segments.iter().map(|s| s.size).sum();
        let mut done = self.done();
        self.report(DownloadReport::Progress { done });
        for (index, segment) in self.from.segments.clone().iter().enumerate() {
            let part = self.part(index);
            let checked = self.parts.join(format!("{index}.sha256"));
            let mut have = std::fs::metadata(&part).map_or(0, |m| m.len());
            if have > segment.size {
                let _ = std::fs::remove_file(&part);
                let _ = std::fs::remove_file(&checked);
                done -= segment.size;
                have = 0;
            }
            if have < segment.size {
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part)
                    .await
                    .map_err(|e| Ended::Failed(format!("Cannot write {}: {e}", part.display())))?;
                let remote = format!(
                    "tail -c +{} -- {} | head -c {}",
                    have + 1,
                    quoted(&segment.path),
                    segment.size - have
                );
                let reports = self.reports.clone();
                let mut reported = Instant::now();
                let mut on_bytes = |n: u64| {
                    done += n;
                    if reported.elapsed() >= REPORT_EVERY {
                        reported = Instant::now();
                        let _ = reports.send(DownloadReport::Progress { done });
                    }
                };
                let finished = over_ssh(
                    ctx,
                    &self.computer,
                    self.through.as_ref(),
                    &remote,
                    None,
                    &mut file,
                    &mut on_bytes,
                    &mut self.stopped,
                    STALLED,
                    "What arrived is kept.",
                )
                .await;
                let _ = file.flush().await;
                drop(file);
                let finished = finished.map_err(Ended::from)?;
                let have = std::fs::metadata(&part).map_or(0, |m| m.len());
                if have < segment.size {
                    let why = finished
                        .stderr
                        .lines()
                        .rfind(|line| !line.trim().is_empty())
                        .map(|line| format!(" It said: {}", line.trim()))
                        .unwrap_or_default();
                    return Err(Ended::Again(format!(
                        "The copy from {} stopped with {} of {} here.{why}",
                        self.computer.name,
                        size(self.done()),
                        size(total)
                    )));
                }
                self.report(DownloadReport::Progress { done: self.done() });
            }
            if checked.exists() {
                continue;
            }
            let theirs = self.their_hash(ctx, segment).await?;
            let path = part.clone();
            let ours = tokio::task::spawn_blocking(move || sha256(&path))
                .await
                .map_err(|e| Ended::Failed(e.to_string()))?
                .map_err(|e| Ended::Failed(format!("Cannot read {}: {e}", part.display())))?;
            if ours != theirs {
                let _ = std::fs::remove_file(&part);
                let name = name_of(&segment.path);
                if !mismatched.insert(index) {
                    return Err(Ended::Failed(format!(
                        "{name} did not match {}'s copy twice. The file may be changing on the vehicle or its card may be failing, so UXDuck stopped. Download it again to start over.",
                        self.computer.name
                    )));
                }
                return Err(Ended::Now(format!(
                    "{name} did not match {}'s copy, so UXDuck is downloading it again.",
                    self.computer.name
                )));
            }
            std::fs::write(&checked, &ours)
                .map_err(|e| Ended::Failed(format!("Cannot write {}: {e}", checked.display())))?;
        }
        self.keep().await
    }

    /// The computer's SHA-256 of a segment, after checking it is still the
    /// size it was listed at.
    async fn their_hash(&mut self, ctx: &Ctx, segment: &RemoteSegment) -> Result<String, Ended> {
        let path = quoted(&segment.path);
        let remote = format!("stat -c %s -- {path} && sha256sum -- {path}");
        let mut out = Vec::new();
        let finished = over_ssh(
            ctx,
            &self.computer,
            self.through.as_ref(),
            &remote,
            None,
            &mut out,
            &mut |_| {},
            &mut self.stopped,
            SLOW_ANSWER,
            "What arrived is kept.",
        )
        .await?;
        let text = String::from_utf8_lossy(&out);
        let mut lines = text.lines();
        let (Some(listed), Some(hash)) = (lines.next(), lines.next()) else {
            return Err(Ended::Again(format!(
                "{} did not hash {}. {}",
                self.computer.name,
                name_of(&segment.path),
                finished.stderr.trim()
            )));
        };
        if listed.trim().parse::<u64>().ok() != Some(segment.size) {
            return Err(Ended::Failed(format!(
                "{} changed on {} since it was listed. List the recordings again and download it.",
                name_of(&segment.path),
                self.computer.name
            )));
        }
        hash.split_whitespace()
            .next()
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| {
                Ended::Again(format!(
                    "{} gave no SHA-256 for {}",
                    self.computer.name,
                    name_of(&segment.path)
                ))
            })
    }

    /// Joins the checked segments into one file in the media folder.
    async fn keep(&self) -> Result<KeptRecording, Ended> {
        let parts: Vec<PathBuf> = (0..self.from.segments.len())
            .map(|i| self.part(i))
            .collect();
        let joined = self.parts.join("joined.mp4");
        let media = self.media.clone();
        let stem = self.from.stem.clone();
        let target = joined.clone();
        tokio::task::spawn_blocking(move || -> Result<KeptRecording, String> {
            let mut inputs = parts
                .iter()
                .map(|p| std::fs::File::open(p).map(std::io::BufReader::new))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Cannot read a downloaded segment: {e}"))?;
            {
                let mut out = std::io::BufWriter::new(
                    std::fs::File::create(&target)
                        .map_err(|e| format!("Cannot write {}: {e}", target.display()))?,
                );
                uxduck_media::mp4::join(&mut inputs, &mut out)
                    .map_err(|why| format!("The segments could not be joined: {why}"))?;
                std::io::Write::flush(&mut out).map_err(|e| e.to_string())?;
            }
            std::fs::create_dir_all(&media).map_err(|e| e.to_string())?;
            let (file_name, path) = (1..u32::MAX)
                .map(|n| {
                    let name = if n == 1 {
                        format!("{stem}.mp4")
                    } else {
                        format!("{stem} {n}.mp4")
                    };
                    let path = media.join(&name);
                    (name, path)
                })
                .find(|(_, path)| !path.exists())
                .unwrap_or_default();
            if std::fs::rename(&target, &path).is_err() {
                std::fs::copy(&target, &path)
                    .map_err(|e| format!("Cannot put the recording in {}: {e}", media.display()))?;
                let _ = std::fs::remove_file(&target);
            }
            let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
            let probed = crate::video::created::probe(&path).ok();
            // A fragmented MP4's own header says nothing of its length: its
            // fragments do.
            let played = std::fs::File::open(&path)
                .map(std::io::BufReader::new)
                .and_then(|mut file| uxduck_media::mp4::layout_from(&mut file, 0, bytes))
                .ok()
                .and_then(|(_, fragments)| fragments.last().map(|f| f.start_us + f.duration_us));
            Ok(KeptRecording {
                file_name,
                bytes,
                duration_us: played.or_else(|| probed.as_ref().and_then(|p| p.duration_us)),
                codec: probed.and_then(|p| p.codec),
            })
        })
        .await
        .map_err(|e| Ended::Failed(e.to_string()))?
        .map_err(Ended::Failed)
    }
}

fn sha256(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
        hex
    }))
}

#[cfg(all(test, unix))]
mod tests;

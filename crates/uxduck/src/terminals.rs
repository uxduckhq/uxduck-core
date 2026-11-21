//! Terminals on companion computers (S422, ADR-0065): the engine's
//! [`TerminalPort`] and the `/terminal` socket's other end.
//!
//! A terminal is the daemon's own OpenSSH `ssh`, with the options a check
//! and a camera change use ([`companions::ssh`]): only the computer's
//! pinned host key, `StrictHostKeyChecking=yes`, `BatchMode=yes` and never
//! a password. An interactive one runs `ssh -tt` under a pseudo-terminal
//! from `portable-pty` (T3), so resizing reaches the computer. One opened
//! to run a command runs it with no pseudo-terminal and no input.
//!
//! A command an agent runs (S425, T14) has no socket: what it prints is
//! kept here only as far as its answer holds, first and last bytes of each
//! stream, and handed to the engine when `ssh` ends. Ending the session
//! must end the command on the computer too, which hanging up alone does
//! not do without a pseudo-terminal, so it runs under [`LIFELINE`].
//!
//! What is typed and shown passes through bounded queues between `ssh` and
//! the socket and is kept nowhere (T9). `ssh` writes what it says about
//! itself, which never holds those bytes, to a private file that says when it
//! signed in and why it could not. That file, the pinned key's file and any
//! private key written for the session live in a folder of the session's own,
//! removed when it ends however it ends, and swept at the next start after a
//! daemon that was killed.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{mpsc, oneshot, watch};
use uxduck_contracts::dto::terminal::{TerminalEnd as EndDto, TerminalEnded};
use uxduck_core::domain::terminal::TerminalEnd;
use uxduck_engine::terminals::{Collected, RunReport, TerminalOrder, TerminalPort};

use crate::companions::{self, HostKeys, OwnKey, Session, Ssh};
use crate::daemon::engine_task::EngineHandle;

/// How many reads of `ssh`'s output wait for the socket, and how many
/// frames of input wait for `ssh`. A queue has to end somewhere for a slow
/// side to slow the other: when one is full its reader stops, which stops
/// `ssh` and so the program on the computer, or stops the socket and so
/// the client. Nothing is dropped. 32 keeps a burst moving and holds at most
/// 32 reads of [`READ`] bytes for each terminal.
const QUEUED: usize = 32;

/// The most one read of `ssh`'s output takes, which becomes one frame on
/// the socket. More output takes more reads.
const READ: usize = 8 * 1024;

/// How often `ssh`'s own file is read until it says it signed in. It says
/// so nowhere else, and input waits for it so that nothing typed is lost to
/// a way of signing in the computer refuses.
const SIGN_IN_POLL: Duration = Duration::from_millis(50);

/// The terminal `ssh` asks the computer for. The client's emulator draws
/// the session, never this daemon's own terminal, which it may not have. The
/// app's is xterm, and the terminals `uxduck ssh` runs in understand it.
const TERM: &str = "xterm-256color";

/// What an agent's command runs under on the computer, as `sh -c`'s
/// script with the command as its `$1`. Without a pseudo-terminal, a
/// computer's SSH server leaves a command running when its client goes
/// away, so stopping `ssh` here would leave `journalctl -f` running there.
/// The script keeps the session's standard input, which this daemon holds
/// open and never writes to, and gives the command none. When that input
/// ends, because this daemon stopped the session or stopped itself, it ends
/// the command and everything the command started: by its process group,
/// which `setsid` gives it, or job control where there is no `setsid`, and
/// failing both the command's own process alone. It asks first and insists
/// two seconds later. The command runs in the user's own shell, as `ssh
/// host command` runs it, and its exit status is the session's.
const LIFELINE: &str = r#"exec 3<&0 </dev/null
if command -v setsid >/dev/null 2>&1; then setsid "${SHELL:-/bin/sh}" -c "$1" 3<&- &
else set -m 2>/dev/null; "${SHELL:-/bin/sh}" -c "$1" 3<&- & fi
c=$!
( cat <&3 >/dev/null 2>&1
  kill -TERM "-$c" 2>/dev/null || kill -TERM "$c" 2>/dev/null
  sleep 2
  kill -KILL "-$c" 2>/dev/null || kill -KILL "$c" 2>/dev/null ) >/dev/null 2>&1 &
w=$!
exec 3<&-
wait "$c"
s=$?
kill -TERM "-$w" 2>/dev/null || kill -TERM "$w" 2>/dev/null
exit "$s""#;

/// One word for a shell, whatever it holds.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The line `ssh` gives the computer to run `command` under [`LIFELINE`].
fn under_lifeline(command: &str) -> String {
    format!(
        "exec sh -c {} uxduck-run {}",
        quoted(LIFELINE),
        quoted(command)
    )
}

/// Keeps the first and the last bytes of one stream and counts them all.
/// It always reads on, so a command that prints without end is never held
/// up and never fills this daemon's memory.
async fn collect(mut from: mpsc::Receiver<Vec<u8>>, first: usize, last: usize) -> Collected {
    let mut kept = Collected::default();
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(last);
    while let Some(bytes) = from.recv().await {
        kept.bytes += bytes.len() as u64;
        let room = first.saturating_sub(kept.first.len()).min(bytes.len());
        let (head, rest) = bytes.split_at(room);
        kept.first.extend_from_slice(head);
        ring.extend(rest);
        let over = ring.len().saturating_sub(last);
        ring.drain(..over);
    }
    kept.last = ring.into();
    kept
}

/// The folder under `<data dir>/companions` for sessions' own folders.
const FILES: &str = "terminals";

/// A session's ends for its socket.
pub struct Attached {
    /// What the client types, to `ssh`.
    pub input: mpsc::Sender<Vec<u8>>,
    /// What `ssh` shows, to the client.
    pub output: mpsc::Receiver<Vec<u8>>,
    /// How it ended, once it has.
    pub ended: oneshot::Receiver<TerminalEnded>,
}

struct Entry {
    /// Taken by the one socket a ticket lets in.
    attached: Option<Attached>,
    size: watch::Sender<(u16, u16)>,
    /// How the engine ended it, once it has.
    end: watch::Sender<Option<TerminalEnd>>,
}

/// The sessions the daemon runs, shared by the engine's port and the
/// `/terminal` socket.
#[derive(Default)]
pub struct Hub {
    sessions: Mutex<HashMap<String, Entry>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

impl Hub {
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session's ends, for the socket the engine let in. Once.
    #[must_use]
    pub fn attach(&self, terminal_id: &str) -> Option<Attached> {
        self.sessions().get_mut(terminal_id)?.attached.take()
    }
}

pub struct Host {
    dir: PathBuf,
    data_dir: PathBuf,
    engine: EngineHandle,
    hub: Arc<Hub>,
}

impl Host {
    #[must_use]
    pub fn new(data_dir: &Path, engine: EngineHandle, hub: Arc<Hub>) -> Self {
        let dir = data_dir.join("companions");
        // A daemon that was killed could not remove its sessions' files: no
        // key outlives the session it was written for by more than a start.
        if let Err(error) = std::fs::remove_dir_all(dir.join(FILES))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "cannot remove the files of terminals a stopped daemon left");
        }
        Self {
            dir,
            data_dir: data_dir.to_owned(),
            engine,
            hub,
        }
    }
}

impl TerminalPort for Host {
    fn open(&mut self, order: TerminalOrder) -> Result<(), String> {
        // Before anything else (T16).
        let program = companions::ssh_program().ok_or_else(companions::ssh_missing)?;
        let (pasted, own) = companions::saved_sign_in(&self.data_dir, &order.computer)?;
        if let Some(keeps) = order.collect {
            self.run(program, order, pasted, own, keeps);
            return Ok(());
        }
        let (input, typed) = mpsc::channel(QUEUED);
        let (shown, output) = mpsc::channel(QUEUED);
        let (ended_tx, ended) = oneshot::channel();
        let (size_tx, size) = watch::channel((order.cols, order.rows));
        let (end_tx, end) = watch::channel(None);
        let terminal_id = order.terminal_id.clone();
        self.hub.sessions().insert(
            terminal_id.clone(),
            Entry {
                attached: Some(Attached {
                    input,
                    output,
                    ended,
                }),
                size: size_tx,
                end: end_tx,
            },
        );
        let mut task = Task {
            dir: self.dir.clone(),
            program,
            order,
            pasted,
            own,
            typed: Some(typed),
            shown,
            shown_err: None,
            size,
            end,
        };
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let ended = match task.run().await {
                Outcome::Own(ended) => {
                    let (how, exit_status) = (core_end(ended.how), ended.exit_status);
                    engine.post(move |e| e.terminal_ended(&terminal_id, how, exit_status));
                    ended
                }
                // The engine ended it and has journaled how. When someone
                // else ended it, its holder is told so in these words.
                Outcome::Told(how) => TerminalEnded {
                    how: how.into(),
                    exit_status: None,
                    problem: how.told().map(str::to_owned),
                },
            };
            // Whatever is still queued reaches the socket before this does.
            drop(task);
            let _ = ended_tx.send(ended);
        });
        Ok(())
    }

    fn resize(&mut self, terminal_id: &str, cols: u16, rows: u16) {
        if let Some(entry) = self.hub.sessions().get(terminal_id) {
            let _ = entry.size.send((cols, rows));
        }
    }

    fn close(&mut self, terminal_id: &str, end: TerminalEnd) {
        // Dropping the entry drops a socket's ends nobody took.
        if let Some(entry) = self.hub.sessions().remove(terminal_id) {
            let _ = entry.end.send(Some(end));
        }
    }
}

impl Host {
    /// An agent's one command (T14): no socket, its output kept as far as
    /// its answer holds and reported to the engine when `ssh` has ended.
    fn run(
        &mut self,
        program: PathBuf,
        order: TerminalOrder,
        pasted: Option<String>,
        own: OwnKey,
        keeps: uxduck_engine::terminals::Keeps,
    ) {
        let (shown, out) = mpsc::channel(QUEUED);
        let (shown_err, err) = mpsc::channel(QUEUED);
        let (size_tx, size) = watch::channel((order.cols, order.rows));
        let (end_tx, end) = watch::channel(None);
        let terminal_id = order.terminal_id.clone();
        self.hub.sessions().insert(
            terminal_id.clone(),
            Entry {
                attached: None,
                size: size_tx,
                end: end_tx,
            },
        );
        let stdout = tokio::spawn(collect(out, keeps.stdout_first, keeps.stdout_last));
        let stderr = tokio::spawn(collect(err, keeps.stderr_first, keeps.stderr_last));
        let mut task = Task {
            dir: self.dir.clone(),
            program,
            order,
            pasted,
            own,
            typed: None,
            shown,
            shown_err: Some(shown_err),
            size,
            end,
        };
        let (engine, hub) = (self.engine.clone(), self.hub.clone());
        tokio::spawn(async move {
            let own = match task.run().await {
                Outcome::Own(ended) => {
                    Some((core_end(ended.how), ended.exit_status, ended.problem))
                }
                // The engine ended it and has journaled how.
                Outcome::Told(_) => None,
            };
            // Its ends of the two streams go, and the rest end with `ssh`.
            drop(task);
            let report = RunReport {
                stdout: stdout.await.unwrap_or_default(),
                stderr: stderr.await.unwrap_or_default(),
                own,
            };
            hub.sessions().remove(&terminal_id);
            engine.post(move |e| e.computer_ran(&terminal_id, report));
        });
    }
}

fn core_end(end: EndDto) -> TerminalEnd {
    match end {
        EndDto::Exited => TerminalEnd::Exited,
        EndDto::Failed => TerminalEnd::Failed,
        EndDto::Closed => TerminalEnd::Closed,
        EndDto::HungUp => TerminalEnd::HungUp,
        EndDto::Unclaimed => TerminalEnd::Unclaimed,
        EndDto::Stopped => TerminalEnd::Stopped,
        EndDto::EndedByOperator => TerminalEnd::EndedByOperator,
        EndDto::CapabilityRemoved => TerminalEnd::CapabilityRemoved,
        EndDto::Deadline => TerminalEnd::Deadline,
    }
}

/// How a session's task ended.
enum Outcome {
    /// `ssh` ended on its own, or never ran.
    Own(TerminalEnded),
    /// The engine ended it.
    Told(TerminalEnd),
}

fn failed(problem: String) -> Outcome {
    Outcome::Own(TerminalEnded {
        how: EndDto::Failed,
        // What `ssh` itself exits with when it fails.
        exit_status: Some(255),
        problem: Some(problem),
    })
}

/// One `ssh` that ran to its end.
enum Attended {
    /// The computer refused this way of signing in: what `ssh` said.
    Refused(String),
    Done(Outcome),
}

/// One session, from its order to its end.
struct Task {
    /// `<data dir>/companions`, where checks pin host keys.
    dir: PathBuf,
    program: PathBuf,
    order: TerminalOrder,
    pasted: Option<String>,
    own: OwnKey,
    typed: Option<mpsc::Receiver<Vec<u8>>>,
    shown: mpsc::Sender<Vec<u8>>,
    /// Where a run's standard error goes, apart from its standard output.
    /// A terminal's two streams go to its socket together.
    shown_err: Option<mpsc::Sender<Vec<u8>>>,
    size: watch::Receiver<(u16, u16)>,
    end: watch::Receiver<Option<TerminalEnd>>,
}

impl Task {
    async fn run(&mut self) -> Outcome {
        let computer = self.order.computer.clone();
        let Some(pinned) = computer.check.as_ref().and_then(|c| c.host_key.clone()) else {
            return failed(format!(
                "{} has not been checked yet, so UXDuck does not know its host key. Check it first on its vehicle's page.",
                computer.name
            ));
        };
        let root = self.dir.join(FILES);
        let files = match std::fs::create_dir_all(&root).and_then(|()| {
            tempfile::Builder::new()
                .prefix(&format!("{}-", self.order.terminal_id))
                .tempdir_in(&root)
        }) {
            Ok(files) => files,
            Err(error) => {
                return failed(format!(
                    "Cannot make the terminal's folder in {}: {error}",
                    root.display()
                ));
            }
        };
        let trusted = files.path().join("known_hosts");
        let key = files.path().join("key");
        let log = files.path().join("ssh.log");
        match companions::trust_pinned(&self.dir, &computer, &pinned, &trusted).await {
            Err(why) => return failed(why),
            Ok(false) => {
                return failed(format!(
                    "UXDuck no longer has the host key it pinned for {}. Check it again on its vehicle's page.",
                    computer.name
                ));
            }
            Ok(true) => {}
        }
        let run = self.order.collect.is_some();
        // What the computer is given to run: the command, or for a run the
        // command under its lifeline.
        let command = self.order.command.as_ref().map(|command| {
            if run {
                under_lifeline(&command.0)
            } else {
                command.0.clone()
            }
        });
        let (pasted, own) = (self.pasted.take(), self.own.clone());
        let mut said = String::new();
        for way in companions::pinned_ways(pasted.as_deref(), Some(&own)) {
            // `ssh` adds to its file: each try reads its own words alone.
            let _ = std::fs::remove_file(&log);
            let built = match companions::ssh(
                &key,
                &files.path().join("askpass"),
                None,
                &computer,
                way,
                HostKeys::Pinned(&trusted),
                Session::Terminal {
                    tty: command.is_none(),
                    log: &log,
                },
                command.as_deref(),
            ) {
                Ok(built) => built,
                Err(why) => return failed(why),
            };
            let (cols, rows) = *self.size.borrow_and_update();
            let running = if command.is_some() {
                let err = self.shown_err.clone().unwrap_or_else(|| self.shown.clone());
                spawn_piped(&self.program, &built, self.shown.clone(), err, run)
            } else {
                spawn_pty(&self.program, &built, (cols, rows), self.shown.clone())
            };
            let running = match running {
                Ok(running) => running,
                Err(why) => return failed(why),
            };
            match self.attend(running, &log, &key).await {
                Attended::Refused(text) => said = text,
                Attended::Done(outcome) => return outcome,
            }
        }
        failed(companions::terminal_failure(&computer, &said))
    }

    /// Stays with one `ssh` until it ends or the engine ends it.
    async fn attend(&mut self, mut running: Running, log: &Path, key: &Path) -> Attended {
        let said = || std::fs::read_to_string(log).unwrap_or_default();
        let mut signed = false;
        let mut poll = tokio::time::interval(SIGN_IN_POLL);
        let status = loop {
            tokio::select! {
                status = &mut running.exited => break status.ok().flatten(),
                end = told(&mut self.end) => {
                    running.kill();
                    let _ = (&mut running.exited).await;
                    running.reaped = true;
                    return Attended::Done(Outcome::Told(end));
                }
                Ok(()) = self.size.changed() => {
                    let (cols, rows) = *self.size.borrow_and_update();
                    running.resize(cols, rows);
                }
                _ = poll.tick(), if !signed => {
                    if companions::signed_in(&said()) {
                        signed = true;
                        // `ssh` has read the key it signed in with.
                        let _ = std::fs::remove_file(key);
                        running.type_from(self.typed.take());
                    }
                }
            }
        };
        running.reaped = true;
        let _ = std::fs::remove_file(key);
        let said = said();
        if !companions::signed_in(&said) {
            if companions::refused(&said) {
                return Attended::Refused(said);
            }
            return Attended::Done(failed(companions::terminal_failure(
                &self.order.computer,
                &said,
            )));
        }
        // Everything it showed reaches the socket before its end does.
        running.finish();
        tokio::select! {
            _ = &mut running.pumped => {}
            end = told(&mut self.end) => return Attended::Done(Outcome::Told(end)),
        }
        Attended::Done(Outcome::Own(TerminalEnded {
            how: EndDto::Exited,
            exit_status: status,
            // 255 is `ssh`'s own: why it lost the computer, when it said.
            problem: (status == Some(255))
                .then(|| companions::last_said(&said))
                .flatten(),
        }))
    }
}

/// How the engine ended the session, once it has. An engine that went away
/// without a word has stopped.
async fn told(end: &mut watch::Receiver<Option<TerminalEnd>>) -> TerminalEnd {
    end.wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|end| *end)
        .unwrap_or(TerminalEnd::Stopped)
}

/// A running `ssh`.
struct Running {
    /// Its exit status, or none when a signal ended it.
    exited: oneshot::Receiver<Option<i32>>,
    /// Everything it wrote has been queued for the socket.
    pumped: oneshot::Receiver<()>,
    reaped: bool,
    kind: Kind,
}

enum Kind {
    Pty {
        master: Option<Box<dyn MasterPty + Send>>,
        writer: Option<Box<dyn std::io::Write + Send>>,
        killer: Box<dyn ChildKiller + Send + Sync>,
    },
    Piped {
        kill: Option<oneshot::Sender<()>>,
    },
}

impl Running {
    fn resize(&self, cols: u16, rows: u16) {
        if let Kind::Pty {
            master: Some(master),
            ..
        } = &self.kind
            && let Err(error) = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
        {
            tracing::warn!(%error, "cannot resize a terminal");
        }
    }

    /// Starts sending what is typed, once `ssh` has signed in. A command
    /// takes no input: what arrives for it is not read.
    fn type_from(&mut self, typed: Option<mpsc::Receiver<Vec<u8>>>) {
        let Kind::Pty { writer, .. } = &mut self.kind else {
            return;
        };
        let (Some(mut writer), Some(mut typed)) = (writer.take(), typed) else {
            return;
        };
        std::thread::spawn(move || {
            while let Some(bytes) = typed.blocking_recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    /// Hangs up, unless it has ended: its process id may be another's by
    /// then.
    fn kill(&mut self) {
        if self.reaped {
            return;
        }
        match &mut self.kind {
            Kind::Pty { killer, master, .. } => {
                let _ = killer.kill();
                // And its terminal goes away, which hangs up too.
                master.take();
            }
            Kind::Piped { kill } => {
                if let Some(kill) = kill.take() {
                    let _ = kill.send(());
                }
            }
        }
    }

    /// Lets the last of its output end: `ConPTY` ends it only when the
    /// pseudo-terminal closes.
    fn finish(&mut self) {
        if let Kind::Pty { master, writer, .. } = &mut self.kind {
            writer.take();
            master.take();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_pty(
    program: &Path,
    built: &Ssh,
    (cols, rows): (u16, u16),
    shown: mpsc::Sender<Vec<u8>>,
) -> Result<Running, String> {
    let cannot =
        |error: &dyn std::fmt::Display| format!("Cannot open a pseudo-terminal for ssh: {error}");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| cannot(&e))?;
    let mut command = CommandBuilder::new(program);
    command.args(&built.args);
    for (name, value) in &built.envs {
        command.env(name, value);
    }
    command.env("TERM", TERM);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| format!("Cannot run ssh: {e}"))?;
    // Only `ssh` holds the terminal's other end now, so its end is seen.
    drop(pair.slave);
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader().map_err(|e| cannot(&e))?;
    let writer = pair.master.take_writer().map_err(|e| cannot(&e))?;
    let (exited_tx, exited) = oneshot::channel();
    std::thread::spawn(move || {
        let status = child.wait().ok().and_then(|status| {
            status
                .signal()
                .is_none()
                .then(|| i32::try_from(status.exit_code()).ok())
                .flatten()
        });
        let _ = exited_tx.send(status);
    });
    let (pumped_tx, pumped) = oneshot::channel();
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; READ];
        // Linux ends a pseudo-terminal's output with an error, macOS and
        // Windows with nothing more to read.
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || shown.blocking_send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
        let _ = pumped_tx.send(());
    });
    Ok(Running {
        exited,
        pumped,
        reaped: false,
        kind: Kind::Pty {
            master: Some(pair.master),
            writer: Some(writer),
            killer,
        },
    })
}

/// `ssh` with no pseudo-terminal. With `lifeline` its standard input is a
/// pipe this daemon holds open and never writes to, which ends when `ssh`
/// is stopped or this daemon is ([`LIFELINE`]). Without, it has no input.
fn spawn_piped(
    program: &Path,
    built: &Ssh,
    shown: mpsc::Sender<Vec<u8>>,
    shown_err: mpsc::Sender<Vec<u8>>,
    lifeline: bool,
) -> Result<Running, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(&built.args)
        .envs(built.envs.iter().map(|(name, value)| (*name, value)))
        .stdin(if lifeline {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("Cannot run ssh: {e}"))?;
    let (held, out, err) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (pumped_tx, pumped) = oneshot::channel();
    tokio::spawn(async move {
        tokio::join!(pump(out, shown), pump(err, shown_err));
        let _ = pumped_tx.send(());
    });
    let (kill, killed) = oneshot::channel::<()>();
    let (exited_tx, exited) = oneshot::channel();
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status.ok().and_then(|status| status.code()),
            _ = killed => {
                let _ = child.kill().await;
                None
            }
        };
        // The lifeline ends with `ssh`.
        drop(held);
        let _ = exited_tx.send(status);
    });
    Ok(Running {
        exited,
        pumped,
        reaped: false,
        kind: Kind::Piped { kill: Some(kill) },
    })
}

async fn pump(from: Option<impl tokio::io::AsyncRead + Unpin>, shown: mpsc::Sender<Vec<u8>>) {
    let Some(mut from) = from else {
        return;
    };
    let mut buffer = vec![0_u8; READ];
    while let Ok(read) = from.read(&mut buffer).await {
        if read == 0 || shown.send(buffer[..read].to_vec()).await.is_err() {
            break;
        }
    }
}

/// A `/terminal` socket (ADR-0065 T4): raw bytes as binary frames both
/// ways, then one text frame, a [`TerminalEnded`] as JSON, when the session
/// ends. The socket closing hangs the session up (T13).
pub async fn serve(
    socket: WebSocket,
    terminal_id: String,
    attached: Attached,
    engine: EngineHandle,
) {
    let (mut sink, mut stream) = socket.split();
    let Attached {
        input,
        mut output,
        mut ended,
    } = attached;
    let mut writer = tokio::spawn(async move {
        let ended = loop {
            tokio::select! {
                // What it showed comes before how it ended.
                biased;
                bytes = output.recv() => match bytes {
                    Some(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            return;
                        }
                    }
                    None => break (&mut ended).await.ok(),
                },
                ended = &mut ended => break ended.ok(),
            }
        };
        if let Some(text) = ended.and_then(|ended| serde_json::to_string(&ended).ok()) {
            let _ = sink.send(Message::Text(text.into())).await;
        }
        let _ = sink.send(Message::Close(None)).await;
    });
    let reader = async {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                // A session that takes no input, or has ended, reads none.
                Message::Binary(bytes) => {
                    let _ = input.send(bytes.to_vec()).await;
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        () = reader => writer.abort(),
        _ = &mut writer => {}
    }
    engine.post(move |e| e.terminal_socket_closed(&terminal_id));
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]
mod tests {
    use uxduck_core::domain::companion::{CompanionComputer, CompanionRole};

    use super::*;
    use crate::companions::Way;

    const PINNED: &str = "SHA256:ld0pRnPj6iBw1VlXhDyqJMO6rdQ6axMIiQSimgq72bg";

    fn computer() -> CompanionComputer {
        CompanionComputer {
            connectivity: None,
            id: "cctest".into(),
            name: "Camera Pi".into(),
            host: "camera.local".into(),
            user: Some("pi".into()),
            port: Some(2222),
            role: CompanionRole::Report,
            component_id: 191,
            key: false,
            check: Some(uxduck_core::domain::companion::CompanionCheck {
                state: uxduck_core::domain::companion::CheckState::Reached,
                at_us: 1,
                os: None,
                arch: None,
                uxduck: None,
                host_key: Some(PINNED.into()),
                problem: None,
                problem_kind: None,
                detail: None,
                clock: None,
                reports_as: None,
                network: None,
                cameras: None,
            }),
        }
    }

    fn own() -> OwnKey {
        OwnKey {
            private: "-----BEGIN OPENSSH PRIVATE KEY-----\nown\n-----END OPENSSH PRIVATE KEY-----"
                .into(),
            public: "ssh-ed25519 AAAA uxduck".into(),
        }
    }

    fn words(built: &Ssh) -> Vec<String> {
        built
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_terminal_signs_in_as_a_check_does_and_trusts_only_the_pinned_key() {
        let dir = tempfile::tempdir().unwrap();
        let (key, trusted, log) = (
            dir.path().join("key"),
            dir.path().join("known_hosts"),
            dir.path().join("ssh.log"),
        );
        let own = own();
        // Every way a terminal signs in: the person's config and agent,
        // UXDuck's own key, and a key given for the computer. Never a
        // password, which only a check with one typed uses.
        let ways = [
            companions::pinned_ways(None, Some(&own)),
            companions::pinned_ways(Some("pasted"), Some(&own)),
        ]
        .concat_ways();
        assert_eq!(ways.len(), 3);
        for way in ways {
            assert!(!matches!(way, Way::Password(..)));
            for (tty, remote) in [(true, None), (false, Some("uname -m"))] {
                let built = companions::ssh(
                    &key,
                    &dir.path().join("askpass"),
                    None,
                    &computer(),
                    way,
                    HostKeys::Pinned(&trusted),
                    Session::Terminal { tty, log: &log },
                    remote,
                )
                .unwrap();
                let words = words(&built);
                let has = |word: &str| words.iter().any(|w| w == word);
                // The pinned key alone, strictly, and no prompt of any kind.
                assert!(has("StrictHostKeyChecking=yes"), "{words:?}");
                assert!(has("BatchMode=yes"));
                assert!(has("HostKeyAlias=uxduck-network"));
                assert!(has("GlobalKnownHostsFile=none"));
                assert!(has(&format!(
                    "UserKnownHostsFile=\"{}\" ",
                    trusted.display()
                )));
                assert!(has("AddKeysToAgent=no"));
                assert!(!words.iter().any(|w| w.contains("accept-new")));
                assert!(!has("BatchMode=no"));
                assert!(built.envs.is_empty(), "nothing answers a password prompt");
                assert!(!dir.path().join("askpass").exists());
                // What ssh says about itself goes to its own file, quietly
                // enough that a command it runs is never in it.
                assert!(has("LogLevel=VERBOSE") && !has("-v"));
                let at = words.iter().position(|w| w == "-E").unwrap();
                assert_eq!(words[at + 1], log.display().to_string());
                // A pseudo-terminal to type into, or none for one command.
                assert_eq!(has("-tt"), tty);
                assert_eq!(has("-T"), !tty);
                assert_eq!(has("EscapeChar=none"), tty);
                let host = words.iter().position(|w| w == "camera.local").unwrap();
                assert_eq!(words[host - 1], "--");
                assert_eq!(
                    words[host + 1..],
                    remote.map(str::to_owned).into_iter().collect::<Vec<_>>()[..]
                );
                assert!(has("-p") && has("2222") && has("-l") && has("pi"));
            }
        }
        // The same builder is a check's, which alone may pin a new key.
        let check = companions::ssh(
            &key,
            &dir.path().join("askpass"),
            None,
            &computer(),
            Way::Person,
            HostKeys::AcceptNew {
                known_hosts: &trusted,
                theirs: &[],
            },
            Session::Batch,
            Some("true"),
        )
        .unwrap();
        let check = words(&check);
        assert!(check.contains(&"StrictHostKeyChecking=accept-new".to_owned()));
        assert!(check.contains(&"-v".to_owned()) && !check.contains(&"-E".to_owned()));
    }

    trait ConcatWays<'a> {
        fn concat_ways(self) -> Vec<Way<'a>>;
    }

    impl<'a> ConcatWays<'a> for [Vec<Way<'a>>; 2] {
        fn concat_ways(self) -> Vec<Way<'a>> {
            self.into_iter().flatten().collect()
        }
    }

    #[test]
    fn ssh_is_looked_for_before_anything_runs_and_its_absence_says_how_to_get_it() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("ssh{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(companions::program_in(None, "ssh"), None);
        assert_eq!(
            companions::program_in(Some(dir.path().as_os_str()), "ssh"),
            None
        );
        std::fs::write(dir.path().join(&name), "").unwrap();
        let path = std::env::join_paths([Path::new("/nowhere-at-all"), dir.path()]).unwrap();
        assert_eq!(
            companions::program_in(Some(&path), "ssh"),
            Some(dir.path().join(&name))
        );
        let missing = companions::ssh_missing();
        assert!(
            missing.starts_with(
                "UXDuck reaches companion computers with OpenSSH's ssh command, and it is not installed on this computer. "
            ),
            "{missing}"
        );
        assert!(!missing.contains(';'));
    }

    #[test]
    fn what_ssh_says_about_itself_tells_a_sign_in_from_each_refusal() {
        let signed = "Authenticated to camera.local ([192.168.1.42]:2222) using \"publickey\".\n";
        assert!(companions::signed_in(signed));
        assert!(!companions::signed_in("debug1: Authenticated to nowhere\n"));

        // A changed host key, in a check's words, with both fingerprints.
        let changed = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\nIT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\nThe fingerprint for the ED25519 key sent by the remote host is\nSHA256:9f2QvYc1rQ0m3o8J5s2Jc0l1c1o5V8e3wq7Jm1Yb0aE.\nPlease contact your system administrator.\nHost key verification failed.\n";
        assert!(!companions::signed_in(changed));
        assert_eq!(
            companions::terminal_failure(&computer(), changed),
            format!(
                "camera.local's host key changed: UXDuck pinned {PINNED}, and it now sends SHA256:9f2QvYc1rQ0m3o8J5s2Jc0l1c1o5V8e3wq7Jm1Yb0aE. Someone may be pretending to be it. If it was reinstalled, remove it and add it again."
            )
        );
        // It no longer has a key of the pinned kind at all.
        assert_eq!(
            companions::terminal_failure(
                &computer(),
                "No ED25519 host key is known for uxduck-network and you have requested strict checking.\nHost key verification failed.\n"
            ),
            format!(
                "camera.local's host key changed: UXDuck pinned {PINNED}, and it now sends another. Someone may be pretending to be it. If it was reinstalled, remove it and add it again."
            )
        );
        let refused = "pi@camera.local: Permission denied (publickey).\n";
        assert!(companions::refused(refused));
        assert_eq!(
            companions::terminal_failure(&computer(), refused),
            "camera.local refused the sign-in: add your key to ssh-agent, or check once with its password so UXDuck adds its own key"
        );
        // What it said last, once signed in, and never its counters.
        assert_eq!(
            companions::last_said(&format!(
                "{signed}client_loop: send disconnect: Broken pipe\nTransferred: sent 3120, received 3004 bytes, in 4.1 seconds\nBytes per second: sent 760.1, received 731.8\n"
            )),
            Some("client_loop: send disconnect: Broken pipe".to_owned())
        );
        assert_eq!(companions::last_said(signed), None);
    }
}

//! Terminals on companion computers (S422, ADR-0065): the daemon's own
//! `ssh` session, started through [`TerminalPort`] and attached to on the
//! daemon's `/terminal` socket with a ticket that works once.
//!
//! The engine keeps who holds each terminal and journals that it opened and
//! how it ended. It never sees a byte typed or shown, and a command a
//! terminal was opened to run goes to the port and nowhere else (T9).
//!
//! Opening one takes the `terminal` capability (S424, T7). The person's own
//! clients on the computer UXDuck runs on hold it. Every other client's
//! `terminal/open` is held as an approval for an operator (ADR-0005), and an
//! agent or a plugin is refused until agents get a way of their own (S425).
//! There is no arming gate (T8), and a terminal neither waits for nor holds
//! the one operation per computer that checks and changes take (T12).
//!
//! An agent runs one command at a time with `computer/run` (S425, T14), in
//! a session of its own kind with no socket: the host collects what the
//! command prints, bounded, and the engine answers the request with it when
//! the command ends, is stopped at its deadline or is ended by an operator.
//! The command is journaled in its run's record. What it printed goes into
//! that one answer and is kept nowhere.

use std::collections::BTreeMap;

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::dto::terminal::{
    ComputerRun as ComputerRunDto, ComputerRunParams, ComputerRunResult, ComputerRunsParams,
    ComputerRunsResult, PrintedLeftOut, Terminal as TerminalDto, TerminalCloseParams,
    TerminalOpenParams, TerminalOpenResult, TerminalResizeParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::{Capability, ClientKind};
use uxduck_core::domain::companion::CompanionComputer;
use uxduck_core::domain::terminal::{ComputerRun, Terminal, TerminalEnd, TerminalKind};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::{Actor, Change};
use uxduck_core::policy::admission::Trust;

use crate::approvals::Asked;
use crate::rpc::handlers::to_value;
use crate::{Connection, ConnectionId, Engine, upsert};

/// How long a ticket waits for its socket. Long enough for a client to open
/// `/terminal` after `terminal/open` answers, and short because a session
/// nobody attached to must not stay signed in on a vehicle (ADR-0065 T13).
/// The same 30 s a `video/watch` token has.
const TICKET_LAPSES_US: u64 = 30_000_000;

/// What an agent and a plugin get, whatever they were allowed, until S425.
const NOT_FOR_AGENTS: &str = "An agent or a plugin cannot open a terminal on a vehicle's computer.";

/// One command with no terminal is `uxduck ssh`'s, on this computer.
const COMMAND_ONLY_HERE: &str =
    "Only uxduck ssh on the computer UXDuck runs on runs one command. Open a terminal instead.";

/// A ticket presented by anyone but the client that opened its terminal.
const NOT_THE_OPENER: &str = "Only the client that opened this terminal can attach to it.";

/// A client that neither holds a terminal nor is an operator.
const NOT_THE_HOLDER: &str =
    "Only the client that opened this terminal, or an operator, can change or end it.";

/// What only an agent gets (T14): a person on another computer has the
/// Terminal tab, and a plugin has no way to a vehicle's computer.
const RUN_FOR_AGENTS: &str = "Only an agent, such as the assistant or an MCP client, runs a command with computer/run. A person opens a terminal on the computer instead.";

/// How long a command runs when its caller gives no deadline. Long enough
/// for a status, a log read or a file listing, and short enough that an
/// agent given a command that never ends, such as `journalctl -f`, has its
/// answer while the person is still waiting for the agent's.
pub const RUN_DEADLINE_S: u64 = 30;

/// How much of what a command printed its answer holds. A tool answer is
/// what bounds it: the assistant's chat takes 12,000 characters of one
/// tool result (`uxduck_agent::turn::MAX_TOOL_OUTPUT_CHARS`) and keeps
/// every result for the rest of the chat, and MCP clients cut long results
/// in ways of their own. Both streams at these bounds, with their notes,
/// stay inside the 12,000. The end is kept as well as the start, because
/// the end of a log or of a failing command is usually what was asked for.
pub const RUN_KEEPS: Keeps = Keeps {
    stdout_first: 4_000,
    stdout_last: 4_000,
    stderr_first: 1_000,
    stderr_last: 1_000,
};

/// How many bytes of a command's two streams are kept for its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keeps {
    pub stdout_first: usize,
    pub stdout_last: usize,
    pub stderr_first: usize,
    pub stderr_last: usize,
}

/// What the host kept of one stream: its first bytes, its last bytes, and
/// how many there were in all. When nothing was left out, `first` then
/// `last` is the whole stream. Never printed (T9).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Collected {
    pub first: Vec<u8>,
    pub last: Vec<u8>,
    pub bytes: u64,
}

impl std::fmt::Debug for Collected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Collected({} bytes)", self.bytes)
    }
}

/// What the host says when a run's `ssh` has ended, however it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub stdout: Collected,
    pub stderr: Collected,
    /// How `ssh` ended on its own, with the exit status and what it said
    /// when it failed. None when the engine ended it.
    pub own: Option<(TerminalEnd, Option<i32>, Option<String>)>,
}

/// A command on its way to `ssh`, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateCommand(pub String);

impl std::fmt::Debug for PrivateCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateCommand(..)")
    }
}

/// What the engine asks the host to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalOrder {
    pub terminal_id: String,
    pub computer: Box<CompanionComputer>,
    /// The computer `computer` is reached through when none of its own
    /// addresses answers (`computer/route`).
    pub through: Option<Box<CompanionComputer>>,
    pub cols: u16,
    pub rows: u16,
    /// One command with no pseudo-terminal, instead of a session to type
    /// into.
    pub command: Option<PrivateCommand>,
    /// An agent's run (T14): no socket. The host keeps this much of what
    /// `command` prints and reports it through [`Engine::computer_ran`],
    /// and makes sure the command is gone from the computer when the
    /// session is closed.
    pub collect: Option<Keeps>,
}

/// Where terminals run: the daemon's `ssh`.
pub trait TerminalPort: Send {
    /// Starts the session. Whatever it finds once `ssh` runs arrives on
    /// the socket and through [`Engine::terminal_ended`].
    ///
    /// # Errors
    /// Why nothing was started, for people: no `ssh` on this computer.
    fn open(&mut self, order: TerminalOrder) -> Result<(), String>;
    fn resize(&mut self, terminal_id: &str, cols: u16, rows: u16);
    /// Hangs up and forgets the session. `end` is what its socket is told.
    fn close(&mut self, terminal_id: &str, end: TerminalEnd);
}

/// A ticket not used yet.
#[derive(Debug)]
struct Ticket {
    terminal_id: String,
    /// The connection that opened the terminal: only it may attach.
    connection: ConnectionId,
    lapses_us: u64,
}

/// A run whose answer is still owed.
#[derive(Debug)]
struct Run {
    connection: ConnectionId,
    request_id: Value,
    deadline_us: u64,
    /// Whether its caller still waits. Not after it cancelled the request.
    answers: bool,
    /// Its record as last journaled. The answer is built from this copy,
    /// so it never depends on the record still being among the latest the
    /// state holds.
    record: ComputerRun,
}

#[derive(Default)]
pub(crate) struct Terminals {
    port: Option<Box<dyn TerminalPort>>,
    /// The connection holding each open terminal.
    held: BTreeMap<String, ConnectionId>,
    tickets: BTreeMap<String, Ticket>,
    /// The runs the host has not reported on yet, by their session's id.
    runs: BTreeMap<String, Run>,
}

impl std::fmt::Debug for Terminals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminals")
            .field("port", &self.port.is_some())
            .field("held", &self.held)
            .field("tickets", &self.tickets.len())
            .field("runs", &self.runs.len())
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn denied(message: &str) -> ApiError {
    ApiError::new(ErrorKind::CapabilityDenied, message)
}

/// What stops a `terminal/*` call before it runs or is held as an approval
/// (ADR-0065 T7), or `None`. An operator is never stopped here. An agent
/// and a plugin are refused whatever capabilities they hold: agents get a
/// way of their own in S425. A client that asks is refused one command with
/// no terminal, which stays `uxduck ssh`'s, and whatever `terminal/open`
/// would refuse anyway, so an operator is never asked to allow a terminal
/// that cannot open.
pub(crate) fn refusal(
    engine: &Engine,
    connection: ConnectionId,
    request: &ClientRequest,
) -> Option<ApiError> {
    if !matches!(
        request,
        ClientRequest::TerminalOpen(_)
            | ClientRequest::TerminalResize(_)
            | ClientRequest::TerminalClose(_)
            | ClientRequest::ComputerRun(_)
    ) {
        return None;
    }
    let admitted = engine.connections.get(&connection)?.admitted.as_ref()?;
    if admitted.is_operator() {
        return None;
    }
    // One command at a time is an agent's (T14), whatever a client of
    // another kind was allowed. And whatever `computer/run` would refuse
    // anyway is refused before an operator is asked to read a command.
    if let ClientRequest::ComputerRun(params) = request {
        if admitted.kind != ClientKind::Agent {
            return Some(denied(RUN_FOR_AGENTS));
        }
        return engine.run_target(params).err();
    }
    if matches!(admitted.kind, ClientKind::Agent | ClientKind::Plugin) {
        return Some(denied(NOT_FOR_AGENTS));
    }
    let ClientRequest::TerminalOpen(params) = request else {
        return None;
    };
    if params.command.is_some() {
        return Some(denied(COMMAND_ONLY_HERE));
    }
    engine.terminal_target(params).err()
}

/// The words a client's `terminal/open` is put to an operator in (T7):
/// who asks, the vehicle and the computer, and nothing else (T9).
pub(crate) fn asked(engine: &Engine, request: &ClientRequest, client: &str) -> Option<Asked> {
    if let ClientRequest::ComputerRun(params) = request {
        return asked_run(engine, params, client);
    }
    let ClientRequest::TerminalOpen(params) = request else {
        return None;
    };
    let (vehicle, computer) = engine.terminal_target(params).ok()?;
    let on = format!("{} › {}", vehicle.name, computer.name);
    let deadline = crate::approvals::APPROVAL_DEADLINE_US / 1_000_000;
    Some(Asked {
        summary: format!("open a terminal on {on}"),
        question: format!("{client} wants a terminal on {on}"),
        body: format!(
            "A terminal signs in to {} over SSH, as UXDuck does to check it. Not allowed in \
             {deadline} s if nobody answers.",
            computer.name
        ),
    })
}

/// The words an agent's `computer/run` is put to an operator in (T14): who
/// asks and where, that what the command prints goes to the agent and its
/// model, and what the two lasting answers mean. The command is not in
/// these words, which a notification and so a feedback bundle carry: it is
/// the approval's own `shell_command`, shown whole beside them.
fn asked_run(engine: &Engine, params: &ComputerRunParams, client: &str) -> Option<Asked> {
    let (vehicle, computer, _) = engine.run_target(params).ok()?;
    let on = format!("{} › {}", vehicle.name, computer.name);
    let deadline = crate::approvals::APPROVAL_DEADLINE_US / 1_000_000;
    Some(Asked {
        summary: format!("run a command on {on}"),
        question: format!("{client} wants to run a command on {on}"),
        body: format!(
            "Read all of the command first. It runs as {}'s SSH user, and what it prints goes \
             to {client} and its AI model. Allow for this connection and Always allow also let \
             {client} run later commands on any companion computer without showing them to you \
             first. Each one is listed on its computer's card. Not allowed in {deadline} s if \
             nobody answers.",
            computer.name
        ),
    })
}

/// The command an approval is for, when its request is `computer/run`.
pub(crate) fn asked_command(request: &ClientRequest) -> Option<String> {
    match request {
        ClientRequest::ComputerRun(params) => Some(params.command.clone()),
        _ => None,
    }
}

/// What the asking client is told when its `terminal/open` or
/// `computer/run` was not allowed, in plain words, or `None` for any other
/// request.
pub(crate) fn not_allowed(request: &ClientRequest, expired: bool) -> Option<&'static str> {
    match request {
        ClientRequest::TerminalOpen(_) => Some(if expired {
            "Nobody answered on the computer UXDuck runs on in time, so the terminal was not opened."
        } else {
            "The operator did not allow this terminal."
        }),
        ClientRequest::ComputerRun(_) => Some(if expired {
            "Nobody answered on the computer UXDuck runs on in time, so the command was not run. Tell the person, and do not ask again unless they ask."
        } else {
            "The operator did not allow this command, so it was not run. Tell the person, and do not ask again unless they ask."
        }),
        _ => None,
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::TerminalOpen(params) => to_value(&engine.open_terminal(connection, params)?),
        ClientRequest::TerminalResize(params) => {
            engine.resize_terminal(connection, &params)?;
            to_value(&Empty {})
        }
        ClientRequest::TerminalClose(params) => {
            engine.close_terminal(connection, &params)?;
            to_value(&Empty {})
        }
        ClientRequest::ComputerRun(params) => engine.run_command(connection, params),
        ClientRequest::ComputerRuns(params) => to_value(&engine.runs_listed(&params)),
        _ => Err(ApiError::new(ErrorKind::Internal, "not a terminal method")),
    }
}

/// The open terminals, for a snapshot.
pub(crate) fn listed(engine: &Engine) -> Vec<TerminalDto> {
    engine.state.terminals.values().map(Into::into).collect()
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::TerminalOpened { terminal } => vec![EntityChange {
            terminal: Some(terminal.into()),
            ..upsert(EntityType::Terminal, &terminal.id)
        }],
        Change::TerminalEnded { id, .. } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Terminal, id)
        }],
        _ => Vec::new(),
    }
}

impl Engine {
    pub fn set_terminals(&mut self, port: Box<dyn TerminalPort>) {
        self.terminals.port = Some(port);
    }

    /// No session outlives the daemon that ran it: the terminals a stopped
    /// or killed daemon left open are ended on the next start.
    pub(crate) fn restore_terminals(&mut self) {
        let left: Vec<String> = self.state.terminals.keys().cloned().collect();
        let now = self.clock.now_us();
        let running: Vec<ComputerRun> = self
            .state
            .computer_runs
            .values()
            .filter(|run| run.end.is_none())
            .cloned()
            .collect();
        for mut run in running {
            run.end = Some(TerminalEnd::Stopped);
            run.ended_at_us = Some(now);
            self.commit_as(Actor::default(), Change::ComputerRunUpserted { run });
        }
        for id in left {
            self.commit_as(
                Actor::default(),
                Change::TerminalEnded {
                    id,
                    end: TerminalEnd::Stopped,
                    exit_status: None,
                },
            );
        }
    }

    /// UXDuck is stopping: every terminal is hung up and journaled as
    /// stopped, so none is left signed in on a vehicle.
    pub fn stop_terminals(&mut self) {
        let open: Vec<String> = self.state.terminals.keys().cloned().collect();
        for id in open {
            self.end_terminal(&id, TerminalEnd::Stopped, Actor::default());
        }
    }

    fn computer_of(&self, computer_id: &str) -> Option<(&Vehicle, &CompanionComputer)> {
        self.state.vehicles.values().find_map(|vehicle| {
            vehicle
                .companions
                .iter()
                .find(|c| c.id == computer_id)
                .map(|computer| (vehicle, computer))
        })
    }

    /// The computer a `computer/run` names, its vehicle and the deadline
    /// in seconds, once everything a run needs before `ssh` runs is there.
    /// An unknown computer is answered with the ones there are, since the
    /// caller is an agent that can pick one.
    fn run_target(
        &self,
        params: &ComputerRunParams,
    ) -> Result<(&Vehicle, &CompanionComputer, u64), ApiError> {
        let Some((vehicle, computer)) = self.computer_of(&params.computer_id) else {
            let there: Vec<String> = self
                .state
                .vehicles
                .values()
                .flat_map(|vehicle| {
                    vehicle
                        .companions
                        .iter()
                        .map(move |c| format!("{} ({}) on {}", c.name, c.id, vehicle.name))
                })
                .collect();
            let there = if there.is_empty() {
                "No vehicle has a companion computer yet. A person adds one on the vehicle's page."
                    .to_owned()
            } else {
                format!("The companion computers are: {}.", there.join(", "))
            };
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!(
                    "No companion computer has the id {}. {there}",
                    params.computer_id
                ),
            ));
        };
        if self.terminals.port.is_none() {
            return Err(invalid(
                "Use UXDuck Desktop or the uxduck daemon to run a command on a companion computer. They have SSH.",
            ));
        }
        if params.command.trim().is_empty() {
            return Err(invalid("Give the command to run."));
        }
        // `ssh` takes the command as one argument, which cannot hold one.
        if params.command.contains('\0') {
            return Err(invalid(
                "A command cannot hold a NUL character, because ssh cannot be given one.",
            ));
        }
        let deadline_s = params.deadline_s.unwrap_or(RUN_DEADLINE_S);
        if deadline_s == 0 {
            return Err(invalid("deadlineS is at least 1 second."));
        }
        if computer
            .check
            .as_ref()
            .and_then(|c| c.host_key.as_deref())
            .is_none()
        {
            return Err(invalid(format!(
                "{} has not been checked yet, so UXDuck does not know its host key. A person checks it first on {}'s page.",
                computer.name, vehicle.name
            )));
        }
        Ok((vehicle, computer, deadline_s))
    }

    /// `computer/run` (T14), for a client that holds `terminal` or was
    /// allowed this once: starts the command and answers when the host
    /// reports on it. No arming gate (T8), and no wait behind a check or a
    /// change (T12).
    fn run_command(
        &mut self,
        connection: ConnectionId,
        params: ComputerRunParams,
    ) -> Result<Value, ApiError> {
        let (vehicle, computer, deadline_s) = {
            let (vehicle, computer, deadline_s) = self.run_target(&params)?;
            (vehicle.clone(), computer.clone(), deadline_s)
        };
        let Some((_, request_id)) = self.replying_to.take() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "computer/run is answered as a request",
            ));
        };
        let (client_id, client_name) = self.terminal_client(connection);
        let now = self.clock.now_us();
        let id = format!("tm{}", &self.random.token()[..12]);
        let order = TerminalOrder {
            terminal_id: id.clone(),
            computer: Box::new(computer.clone()),
            through: crate::companions::through(&vehicle, &computer.id),
            cols: 0,
            rows: 0,
            command: Some(PrivateCommand(params.command.clone())),
            collect: Some(RUN_KEEPS),
        };
        if let Some(port) = self.terminals.port.as_mut()
            && let Err(why) = port.open(order)
        {
            self.replying_to = Some((connection, request_id));
            return Err(invalid(why));
        }
        self.terminals.held.insert(id.clone(), connection);
        let terminal = Terminal {
            id: id.clone(),
            vehicle_id: vehicle.id.clone(),
            computer_id: computer.id.clone(),
            client_id: client_id.clone(),
            client_name: client_name.clone(),
            kind: TerminalKind::Run,
            opened_at_us: now,
        };
        let run = ComputerRun {
            id,
            vehicle_id: vehicle.id,
            vehicle_name: vehicle.name,
            computer_id: computer.id,
            computer_name: computer.name,
            client_id,
            client_name,
            shell_command: params.command,
            // Held, read and allowed by an operator just now, or not.
            shown_first: self.running_approved,
            deadline_s,
            started_at_us: now,
            ended_at_us: None,
            end: None,
            exit_status: None,
            left_out_bytes: 0,
        };
        self.terminals.runs.insert(
            run.id.clone(),
            Run {
                connection,
                request_id,
                deadline_us: now.saturating_add(deadline_s.saturating_mul(1_000_000)),
                answers: true,
                record: run.clone(),
            },
        );
        self.commit(connection, Change::TerminalOpened { terminal });
        self.commit(connection, Change::ComputerRunUpserted { run });
        Ok(Value::Null)
    }

    /// The host's report on a run whose `ssh` has ended: the run's record
    /// is finished and its request answered with what the command printed,
    /// which goes nowhere else.
    pub fn computer_ran(&mut self, id: &str, report: RunReport) {
        let Some(connection) = self.terminals.runs.get(id).map(|run| run.connection) else {
            return;
        };
        // It ended on its own unless the engine ended it first, which
        // finished its record then.
        if let Some((end, exit_status, _)) = &report.own
            && self.state.terminals.contains_key(id)
        {
            let actor = self.actor(connection);
            self.terminal_ended(id, *end, *exit_status);
            self.finish_run(id, *end, *exit_status, actor);
        }
        let Some(waiting) = self.terminals.runs.remove(id) else {
            return;
        };
        let mut run = waiting.record;
        let end = run.end.unwrap_or(TerminalEnd::Stopped);
        let (stdout, stdout_left_out, stdout_replaced) = said(&report.stdout);
        let (stderr, stderr_left_out, stderr_replaced) = said(&report.stderr);
        let left_out =
            stdout_left_out.map_or(0, |l| l.bytes) + stderr_left_out.map_or(0, |l| l.bytes);
        if left_out != run.left_out_bytes {
            run.left_out_bytes = left_out;
            let actor = self.actor(waiting.connection);
            self.commit_as(actor, Change::ComputerRunUpserted { run: run.clone() });
        }
        if !waiting.answers {
            return;
        }
        let problem = report.own.and_then(|(_, _, problem)| problem);
        let frame = if end == TerminalEnd::Failed {
            let why = problem.unwrap_or_else(|| {
                format!("UXDuck could not run the command on {}.", run.computer_name)
            });
            crate::rpc::router::error(&waiting.request_id, &invalid(why))
        } else {
            let mut notes: Vec<String> = Vec::new();
            match end {
                TerminalEnd::Deadline => notes.push(format!(
                    "Stopped at its deadline of {} s, and ended on the computer. What it printed until then is here.",
                    run.deadline_s
                )),
                TerminalEnd::EndedByOperator => notes.push(
                    "The operator stopped it. What it printed until then is here.".into(),
                ),
                TerminalEnd::CapabilityRemoved => notes.push(
                    "The operator stopped it and no longer allows commands from this client without asking. What it printed until then is here.".into(),
                ),
                TerminalEnd::Stopped => notes.push("UXDuck stopped while it ran.".into()),
                _ => {}
            }
            notes.extend(problem);
            for (name, collected, left, replaced) in [
                (
                    "Standard output",
                    &report.stdout,
                    stdout_left_out,
                    stdout_replaced,
                ),
                (
                    "Standard error",
                    &report.stderr,
                    stderr_left_out,
                    stderr_replaced,
                ),
            ] {
                if let Some(left) = left {
                    notes.push(format!(
                        "{name} was {} bytes. Its first {} and its last {} are here, and the {} between them were left out, because a tool answer holds only so much. Run it again with less output, such as through head, tail or grep.",
                        collected.bytes,
                        left.after_bytes,
                        collected.bytes - left.after_bytes - left.bytes,
                        left.bytes
                    ));
                }
                if replaced {
                    notes.push(format!(
                        "{name} held bytes that are not UTF-8 text. Each is shown as \u{fffd}."
                    ));
                }
            }
            let result = ComputerRunResult {
                run_id: run.id.clone(),
                ended: end.into(),
                exit_status: run.exit_status,
                duration_ms: run
                    .ended_at_us
                    .unwrap_or(run.started_at_us)
                    .saturating_sub(run.started_at_us)
                    / 1_000,
                stdout,
                stderr,
                stdout_bytes: report.stdout.bytes,
                stderr_bytes: report.stderr.bytes,
                stdout_left_out,
                stderr_left_out,
                notes,
            };
            serde_json::json!({ "jsonrpc": "2.0", "id": waiting.request_id, "result": result })
        };
        self.send(waiting.connection, &frame);
    }

    /// Finishes a run's record: when and how it ended.
    fn finish_run(&mut self, id: &str, end: TerminalEnd, exit_status: Option<i32>, actor: Actor) {
        let now = self.clock.now_us();
        let Some(waiting) = self.terminals.runs.get_mut(id) else {
            return;
        };
        if waiting.record.end.is_some() {
            return;
        }
        waiting.record.end = Some(end);
        waiting.record.exit_status = exit_status;
        waiting.record.ended_at_us = Some(now);
        let run = waiting.record.clone();
        self.commit_as(actor, Change::ComputerRunUpserted { run });
    }

    /// Runs still going at their deadlines are stopped, so a command that
    /// never ends still answers (T14).
    pub fn stop_runs_at_deadline(&mut self, now: u64) {
        let due: Vec<(String, ConnectionId)> = self
            .terminals
            .runs
            .iter()
            .filter(|(id, run)| run.deadline_us <= now && self.state.terminals.contains_key(*id))
            .map(|(id, run)| (id.clone(), run.connection))
            .collect();
        for (id, connection) in due {
            let actor = self.actor(connection);
            self.end_terminal(&id, TerminalEnd::Deadline, actor);
        }
    }

    /// The runs a cancelled request was waiting for are stopped, and nobody
    /// waits for their answer.
    pub(crate) fn stop_runs(&mut self, connection: ConnectionId, request_id: &Value) {
        let stopped: Vec<String> = self
            .terminals
            .runs
            .iter_mut()
            .filter(|(_, run)| run.connection == connection && run.request_id == *request_id)
            .map(|(id, run)| {
                run.answers = false;
                id.clone()
            })
            .collect();
        let actor = self.actor(connection);
        for id in stopped {
            self.end_terminal(&id, TerminalEnd::HungUp, actor.clone());
        }
    }

    /// `computer/runs`: the records an operator reads, newest first.
    fn runs_listed(&self, params: &ComputerRunsParams) -> ComputerRunsResult {
        let mut runs: Vec<&ComputerRun> = self
            .state
            .computer_runs
            .values()
            .filter(|run| {
                params
                    .computer_id
                    .as_ref()
                    .is_none_or(|id| *id == run.computer_id)
            })
            .collect();
        runs.sort_by(|a, b| (b.started_at_us, &b.id).cmp(&(a.started_at_us, &a.id)));
        ComputerRunsResult {
            runs: runs.into_iter().map(ComputerRunDto::from).collect(),
            kept_per_computer: crate::state::RUNS_PER_COMPUTER as u64,
        }
    }

    /// The computer a `terminal/open` names and its vehicle, once
    /// everything a terminal needs before `ssh` runs is there.
    fn terminal_target(
        &self,
        params: &TerminalOpenParams,
    ) -> Result<(&Vehicle, &CompanionComputer), ApiError> {
        let Some((vehicle, computer)) = self.computer_of(&params.computer_id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("No companion computer has the id {}.", params.computer_id),
            ));
        };
        if self.terminals.port.is_none() {
            return Err(invalid(
                "Use UXDuck Desktop or the uxduck daemon to open a terminal. They have SSH.",
            ));
        }
        if params.cols == 0 || params.rows == 0 {
            return Err(invalid(
                "A terminal is at least one column wide and one row high.",
            ));
        }
        if computer
            .check
            .as_ref()
            .and_then(|c| c.host_key.as_deref())
            .is_none()
        {
            return Err(invalid(format!(
                "{} has not been checked yet, so UXDuck does not know its host key. Check it first on {}'s page.",
                computer.name, vehicle.name
            )));
        }
        Ok((vehicle, computer))
    }

    /// `terminal/open`, for a client that holds `terminal` or was allowed
    /// this once.
    fn open_terminal(
        &mut self,
        connection: ConnectionId,
        params: TerminalOpenParams,
    ) -> Result<TerminalOpenResult, ApiError> {
        let (vehicle_id, computer, through) = {
            let (vehicle, computer) = self.terminal_target(&params)?;
            (
                vehicle.id.clone(),
                computer.clone(),
                crate::companions::through(vehicle, &computer.id),
            )
        };
        let (client_id, client_name) = self.terminal_client(connection);
        let terminal = Terminal {
            id: format!("tm{}", &self.random.token()[..12]),
            vehicle_id,
            computer_id: computer.id.clone(),
            client_id,
            client_name,
            kind: if params.command.is_some() {
                TerminalKind::Command
            } else {
                TerminalKind::Interactive
            },
            opened_at_us: self.clock.now_us(),
        };
        let order = TerminalOrder {
            terminal_id: terminal.id.clone(),
            computer: Box::new(computer),
            through,
            cols: params.cols,
            rows: params.rows,
            command: params.command.map(PrivateCommand),
            collect: None,
        };
        let ticket = format!("tk{}", &self.random.token()[..32]);
        if let Some(port) = self.terminals.port.as_mut() {
            port.open(order).map_err(invalid)?;
        }
        self.terminals.held.insert(terminal.id.clone(), connection);
        self.terminals.tickets.insert(
            ticket.clone(),
            Ticket {
                terminal_id: terminal.id.clone(),
                connection,
                lapses_us: self.clock.now_us() + TICKET_LAPSES_US,
            },
        );
        let terminal_id = terminal.id.clone();
        self.commit(connection, Change::TerminalOpened { terminal });
        Ok(TerminalOpenResult {
            terminal_id,
            ticket,
        })
    }

    /// A connection's client as the client list names it: its record, or
    /// for an operator, who has none kept, what it called itself.
    fn terminal_client(&self, connection: ConnectionId) -> (String, String) {
        self.connections
            .get(&connection)
            .and_then(|c| Some((c, c.admitted.as_ref()?)))
            .map(|(c, admitted)| {
                let name = self
                    .state
                    .clients
                    .get(&admitted.record)
                    .map_or_else(|| c.name.clone(), |record| record.name.clone());
                (admitted.record.clone(), name)
            })
            .unwrap_or_default()
    }

    fn open_terminal_id(&self, terminal_id: &str) -> Result<(), ApiError> {
        if self.state.terminals.contains_key(terminal_id) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::NotFound,
                format!("No terminal with the id {terminal_id} is open."),
            ))
        }
    }

    /// Whether a connection may resize or end a terminal: the connection
    /// holding it, or any operator (T7). True for the holder.
    fn changes_terminal(
        &self,
        connection: ConnectionId,
        terminal_id: &str,
    ) -> Result<bool, ApiError> {
        self.open_terminal_id(terminal_id)?;
        let holder = self.terminals.held.get(terminal_id) == Some(&connection);
        let operator = self
            .connections
            .get(&connection)
            .is_some_and(Connection::is_local_operator);
        if holder || operator {
            Ok(holder)
        } else {
            Err(denied(NOT_THE_HOLDER))
        }
    }

    /// `terminal/resize`.
    fn resize_terminal(
        &mut self,
        connection: ConnectionId,
        params: &TerminalResizeParams,
    ) -> Result<(), ApiError> {
        self.changes_terminal(connection, &params.terminal_id)?;
        if params.cols == 0 || params.rows == 0 {
            return Err(invalid(
                "A terminal is at least one column wide and one row high.",
            ));
        }
        if let Some(port) = self.terminals.port.as_mut() {
            port.resize(&params.terminal_id, params.cols, params.rows);
        }
        Ok(())
    }

    /// `terminal/close`: the client holding a terminal closes it, and any
    /// operator ends any terminal, which its holder is told (T7).
    fn close_terminal(
        &mut self,
        connection: ConnectionId,
        params: &TerminalCloseParams,
    ) -> Result<(), ApiError> {
        let end = if self.changes_terminal(connection, &params.terminal_id)? {
            TerminalEnd::Closed
        } else {
            TerminalEnd::EndedByOperator
        };
        let actor = self.actor(connection);
        self.end_terminal(&params.terminal_id, end, actor);
        Ok(())
    }

    /// An operator's edit left a client without `terminal`, in its record
    /// or as allowed for a connection: that client's open terminals end at
    /// once (T7). `held` is whether it had it before the edit.
    pub(crate) fn terminal_capability_set(
        &mut self,
        record: &str,
        held: bool,
        capabilities: &[Capability],
        actor: &Actor,
    ) {
        if !held || capabilities.contains(&Capability::Terminal) {
            return;
        }
        let open: Vec<String> = self
            .state
            .terminals
            .values()
            .filter(|terminal| terminal.client_id == record)
            .map(|terminal| terminal.id.clone())
            .collect();
        for id in open {
            self.end_terminal(&id, TerminalEnd::CapabilityRemoved, actor.clone());
        }
    }

    /// Whether a client holds `terminal` now: in its record, or allowed for
    /// one of its connections.
    pub(crate) fn holds_terminal(&self, record: &str) -> bool {
        let stored = self
            .state
            .clients
            .get(record)
            .is_some_and(|client| client.capabilities.contains(&Capability::Terminal));
        stored
            || self.connections.values().any(|c| {
                c.admitted.as_ref().is_some_and(|a| {
                    a.record == record && !a.operator && a.extra.contains(&Capability::Terminal)
                })
            })
    }

    /// Ends an open terminal: hangs it up at the host and journals how.
    fn end_terminal(&mut self, terminal_id: &str, end: TerminalEnd, actor: Actor) {
        if let Some(port) = self.terminals.port.as_mut() {
            port.close(terminal_id, end);
        }
        self.terminals.held.remove(terminal_id);
        self.terminals
            .tickets
            .retain(|_, ticket| ticket.terminal_id != terminal_id);
        if self.state.terminals.contains_key(terminal_id) {
            self.commit_as(
                actor.clone(),
                Change::TerminalEnded {
                    id: terminal_id.to_owned(),
                    end,
                    exit_status: None,
                },
            );
            // A run's record says so at once. Its answer waits for what
            // the host kept of its output.
            self.finish_run(terminal_id, end, None, actor);
        }
    }

    /// A client presents a ticket on the `/terminal` socket. `presented` is
    /// how the daemon knows who is asking, as for `/rpc`: for a page on the
    /// network, its paired record and origin. The ticket works once, whoever
    /// presents it, and only for the client that opened the terminal while
    /// that client is still connected: a ticket alone opens nothing. A
    /// terminal that already ended still answers its opener, which reads
    /// from the socket what it showed and how it ended. Answers with the
    /// terminal to attach to.
    ///
    /// # Errors
    /// Why the socket is refused, for the HTTP answer.
    pub fn terminal_attach(&mut self, ticket: &str, presented: &Trust) -> Result<String, String> {
        let Some(ticket) = self.terminals.tickets.remove(ticket) else {
            return Err("This ticket was already used, or no terminal gave it.".into());
        };
        let opener = self.connections.get(&ticket.connection);
        let same = opener.is_some_and(|c| c.trust == *presented)
            && self
                .terminals
                .held
                .get(&ticket.terminal_id)
                .is_none_or(|holder| *holder == ticket.connection);
        if !same {
            // Its one ticket is spent, so nothing can attach to it now.
            let actor = self.actor(ticket.connection);
            self.end_terminal(&ticket.terminal_id, TerminalEnd::Unclaimed, actor);
            return Err(NOT_THE_OPENER.into());
        }
        Ok(ticket.terminal_id)
    }

    /// The `/terminal` socket of a terminal closed: the session is hung up
    /// (T13).
    pub fn terminal_socket_closed(&mut self, terminal_id: &str) {
        let actor = self
            .terminals
            .held
            .get(terminal_id)
            .map_or_else(Actor::default, |connection| self.actor(*connection));
        self.end_terminal(terminal_id, TerminalEnd::HungUp, actor);
    }

    /// The host's `ssh` ended on its own: it exited, or could not start or
    /// sign in.
    pub fn terminal_ended(
        &mut self,
        terminal_id: &str,
        end: TerminalEnd,
        exit_status: Option<i32>,
    ) {
        let Some(connection) = self.terminals.held.remove(terminal_id) else {
            return;
        };
        if self.state.terminals.contains_key(terminal_id) {
            let actor = self.actor(connection);
            self.commit_as(
                actor,
                Change::TerminalEnded {
                    id: terminal_id.to_owned(),
                    end,
                    exit_status,
                },
            );
        }
    }

    /// The connection that opened a terminal went away: so does the
    /// terminal (T13).
    pub(crate) fn terminals_disconnected(&mut self, connection: ConnectionId) {
        let held: Vec<String> = self
            .terminals
            .held
            .iter()
            .filter(|(_, by)| **by == connection)
            .map(|(id, _)| id.clone())
            .collect();
        let actor = self.actor(connection);
        for id in held {
            self.end_terminal(&id, TerminalEnd::HungUp, actor.clone());
        }
        self.terminals
            .tickets
            .retain(|_, ticket| ticket.connection != connection);
    }

    /// Tickets nobody used in time: their sessions are hung up, or
    /// forgotten at the host when they had already ended.
    pub fn expire_terminal_tickets(&mut self, now: u64) {
        let lapsed: Vec<Ticket> = {
            let tickets = &mut self.terminals.tickets;
            let due: Vec<String> = tickets
                .iter()
                .filter(|(_, ticket)| ticket.lapses_us <= now)
                .map(|(key, _)| key.clone())
                .collect();
            due.into_iter()
                .filter_map(|key| tickets.remove(&key))
                .collect()
        };
        for ticket in lapsed {
            let actor = self.actor(ticket.connection);
            self.end_terminal(&ticket.terminal_id, TerminalEnd::Unclaimed, actor);
        }
    }
}

/// One stream as its answer holds it: the text, what was left out of its
/// middle, and whether bytes that are not UTF-8 were replaced. Where the
/// host cut through a character, the pieces of it are left out too, so a
/// cut is never taken for bytes that are not text.
fn said(collected: &Collected) -> (String, Option<PrintedLeftOut>, bool) {
    let kept = (collected.first.len() + collected.last.len()) as u64;
    if collected.bytes <= kept {
        let whole = [collected.first.as_slice(), collected.last.as_slice()].concat();
        let text = String::from_utf8_lossy(&whole);
        let replaced = matches!(text, std::borrow::Cow::Owned(_));
        return (text.into_owned(), None, replaced);
    }
    let first = match std::str::from_utf8(&collected.first) {
        Err(error) if error.error_len().is_none() => &collected.first[..error.valid_up_to()],
        _ => collected.first.as_slice(),
    };
    let skip = collected
        .last
        .iter()
        .take(3)
        .take_while(|byte| *byte & 0xC0 == 0x80)
        .count();
    let last = &collected.last[skip..];
    let left_out = PrintedLeftOut {
        bytes: collected.bytes - first.len() as u64 - last.len() as u64,
        after_bytes: first.len() as u64,
    };
    let (first, last) = (
        String::from_utf8_lossy(first),
        String::from_utf8_lossy(last),
    );
    let replaced =
        matches!(first, std::borrow::Cow::Owned(_)) || matches!(last, std::borrow::Cow::Owned(_));
    let text = format!(
        "{first}\n[UXDuck left out {} bytes here]\n{last}",
        left_out.bytes
    );
    (text, Some(left_out), replaced)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "in-memory admission fixtures")]
mod tests {
    use serde_json::json;
    use uxduck_core::domain::client::{Capability, ClientKind};

    use super::RUN_FOR_AGENTS;
    use crate::mission_reads::fixtures;

    /// A plugin's own connection sends only `command/send` today, so it has
    /// no way to ask for `computer/run`. Should it or any other kind of
    /// client get one, it is refused whatever it was granted, and no
    /// operator is asked.
    #[test]
    fn only_an_agent_runs_a_command_whatever_another_kind_was_granted() {
        let (mut engine, _ui, _orders, _plan) = fixtures::setup();
        let mut client = fixtures::connect(&mut engine, "agent", None);
        let params = json!({ "computerId": "cc-nothing", "command": "id" });
        for kind in [
            ClientKind::Plugin,
            ClientKind::NetworkUi,
            ClientKind::ThirdParty,
            ClientKind::Exporter,
            ClientKind::RemoteOperator,
        ] {
            let admitted = engine
                .connections
                .get_mut(&client.id)
                .unwrap()
                .admitted
                .as_mut()
                .unwrap();
            admitted.kind = kind;
            admitted.granted = Capability::ALL.to_vec();
            let reply = fixtures::call(&mut engine, &mut client, "computer/run", params.clone());
            assert_eq!(
                reply["error"]["data"]["kind"], "capability-denied",
                "{kind:?}"
            );
            assert_eq!(reply["error"]["message"], RUN_FOR_AGENTS, "{kind:?}");
            assert!(engine.state.approvals.is_empty(), "{kind:?}");
        }
        // An agent is past that rule, at what its command needs.
        engine
            .connections
            .get_mut(&client.id)
            .unwrap()
            .admitted
            .as_mut()
            .unwrap()
            .kind = ClientKind::Agent;
        let reply = fixtures::call(&mut engine, &mut client, "computer/run", params);
        assert_eq!(reply["error"]["data"]["kind"], "not-found", "{reply}");
    }
}

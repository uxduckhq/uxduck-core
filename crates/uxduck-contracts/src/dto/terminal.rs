//! Terminals on a vehicle's companion computers on the wire (ADR-0065): the
//! daemon's own `ssh` session, opened with `terminal/open` and carried as
//! bytes on the daemon's `/terminal` WebSocket.
//!
//! The socket: `GET /terminal?ticket=<ticket>` on the listener the client
//! is connected through, from the client that opened the terminal. A page
//! sends its paired session as it does for `/rpc`, and a program on the
//! daemon's own computer sends no `Origin`. The ticket works once. Binary frames carry raw bytes both ways. When the
//! session ends the daemon sends one text frame, a [`TerminalEnded`] as
//! JSON, and closes. Closing the socket ends the session (T13).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a terminal runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalKind {
    /// What the computer gives someone who signs in, under a
    /// pseudo-terminal, to type into.
    #[default]
    Interactive,
    /// One command with no pseudo-terminal and no input.
    Command,
    /// One command an agent runs with `computer/run`: no pseudo-terminal,
    /// no input and no socket. `terminal/close` stops it.
    Run,
}

/// An open terminal: its computer, the client holding it and when it
/// opened. Never what is typed or shown in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    pub id: String,
    pub vehicle_id: String,
    pub computer_id: String,
    /// The record of the client holding it, as in `client/list`.
    pub client_id: String,
    pub client_name: String,
    pub kind: TerminalKind,
    /// Microseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub opened_at_us: u64,
}

/// `terminal/open`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenParams {
    /// The companion computer, by its id on its vehicle.
    pub computer_id: String,
    /// The terminal's width in columns, at least 1.
    pub cols: u16,
    /// The terminal's height in rows, at least 1.
    pub rows: u16,
    /// Runs this one command with no pseudo-terminal and no input instead
    /// of an interactive session, as `ssh host command` does. Its output
    /// and exit status arrive on the socket. Its text is private, as what
    /// is typed in a terminal is: never journaled, logged or traced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Never prints the command (ADR-0065 T9).
impl std::fmt::Debug for TerminalOpenParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalOpenParams")
            .field("computer_id", &self.computer_id)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("command", &self.command.as_ref().map(|_| ".."))
            .finish()
    }
}

/// `terminal/open`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenResult {
    pub terminal_id: String,
    /// Opens `/terminal?ticket=<ticket>` once, from the client that asked.
    pub ticket: String,
}

/// `terminal/resize`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeParams {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

/// `terminal/close`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCloseParams {
    pub terminal_id: String,
}

/// How a terminal ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalEnd {
    /// `ssh` signed in and then exited: `exitStatus` is the remote one.
    Exited,
    /// `ssh` could not be started or could not sign in: `problem` says why.
    Failed,
    /// A client ended it with `terminal/close`.
    Closed,
    /// Its socket or the connection that opened it went away.
    HungUp,
    /// Nothing attached to it with its ticket in time.
    Unclaimed,
    /// UXDuck stopped while it was open.
    Stopped,
    /// An operator ended it from another client: `problem` says so.
    EndedByOperator,
    /// An operator took `terminal` away from the client holding it:
    /// `problem` says so.
    CapabilityRemoved,
    /// A command run with `computer/run` was still running at its deadline
    /// and was stopped.
    Deadline,
}

/// `computer/run`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRunParams {
    /// The companion computer, by its id: `vehicle/get` lists a vehicle's
    /// in `companions`.
    pub computer_id: String,
    /// The one command, as the computer's shell takes it after `ssh host`.
    /// It gets no input and no terminal. The operator is shown all of it,
    /// with every character they could not otherwise see made visible.
    pub command: String,
    /// How many seconds it may run, 30 if left out and at least 1. A command
    /// still running then is stopped on the computer, and what it printed
    /// until then is returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub deadline_s: Option<u64>,
}

/// Never prints the command: it is kept in its run's record only.
impl std::fmt::Debug for ComputerRunParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerRunParams")
            .field("computer_id", &self.computer_id)
            .field("command", &"..")
            .field("deadline_s", &self.deadline_s)
            .finish()
    }
}

/// What a command's answer left out of one of its two streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PrintedLeftOut {
    /// How many bytes were left out.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// They came after this many bytes from the start, which are kept. The
    /// end is kept too.
    #[schemars(schema_with = "crate::safe::integer")]
    pub after_bytes: u64,
}

/// `computer/run`'s answer.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRunResult {
    /// The run's id, as in its record on the computer UXDuck runs on.
    pub run_id: String,
    /// How it ended: `exited` on its own, `deadline` when it was stopped at
    /// its deadline, `ended-by-operator` or `capability-removed` when the
    /// operator stopped it, `stopped` when UXDuck stopped.
    pub ended: TerminalEnd,
    /// The command's exit status when it exited on its own. 255 is `ssh`'s
    /// own, for a connection lost while it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub exit_status: Option<i32>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub duration_ms: u64,
    /// What it printed on standard output, with `[UXDuck left out N bytes
    /// here]` on a line of its own where the middle was left out.
    pub stdout: String,
    /// What it printed on standard error, the same way.
    pub stderr: String,
    /// How many bytes it printed on standard output, kept or not.
    #[schemars(schema_with = "crate::safe::integer")]
    pub stdout_bytes: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub stderr_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_left_out: Option<PrintedLeftOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_left_out: Option<PrintedLeftOut>,
    /// In plain words, everything about this answer that is not the
    /// command's own doing: that it was stopped and why, what was left out
    /// and from where, and that bytes that were not UTF-8 were replaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Never prints what the command printed (ADR-0065 T9).
impl std::fmt::Debug for ComputerRunResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputerRunResult")
            .field("run_id", &self.run_id)
            .field("ended", &self.ended)
            .field("exit_status", &self.exit_status)
            .field("stdout_bytes", &self.stdout_bytes)
            .field("stderr_bytes", &self.stderr_bytes)
            .finish_non_exhaustive()
    }
}

/// A command an agent ran on a companion computer: the record an operator
/// reads. Never what the command printed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRun {
    pub id: String,
    pub vehicle_id: String,
    /// The vehicle and the computer as they were named then.
    pub vehicle_name: String,
    pub computer_id: String,
    pub computer_name: String,
    /// The record of the client that ran it, as in `client/list`.
    pub client_id: String,
    pub client_name: String,
    /// The command as a person reads it: whole, with every character they
    /// could not otherwise see made visible (a newline as `⏎`, a tab as `⇥`,
    /// and any other control, invisible or bidirectional character as its
    /// code point, `⟨U+202E⟩`).
    pub shell_command_shown: String,
    /// Whether an operator was shown this command and allowed it. False
    /// when it ran under an earlier "Allow for this connection" or "Always
    /// allow", or from an operator's own client.
    pub shown_first: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub deadline_s: u64,
    /// Microseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    /// How it ended. Absent while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<TerminalEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub exit_status: Option<i32>,
    /// The bytes of what it printed that its answer left out.
    #[schemars(schema_with = "crate::safe::integer")]
    pub left_out_bytes: u64,
}

/// `computer/runs`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRunsParams {
    /// Only this computer's. Every computer's if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_id: Option<String>,
}

/// `computer/runs`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerRunsResult {
    /// Newest first.
    pub runs: Vec<ComputerRun>,
    /// How many finished runs of each computer this list holds at most.
    /// UXDuck's journal keeps every one.
    #[schemars(schema_with = "crate::safe::integer")]
    pub kept_per_computer: u64,
}

/// The last frame on a `/terminal` socket, as JSON text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEnded {
    pub how: TerminalEnd,
    /// The remote exit status, or 255 as `ssh` gives when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_signed_integer")]
    pub exit_status: Option<i32>,
    /// Why, for people: when `ssh` failed or lost its connection, or when an
    /// operator ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

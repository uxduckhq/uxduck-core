//! Content-free tool traces shared by the native and browser engines.

use std::collections::HashMap;

use serde_json::Value;
use uxduck_contracts::dto::analytics::{
    DaemonEvent, ToolInterruption, ToolTraceContext, ToolTraceSource,
};
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::trace::{RequestId, Transport};

use crate::{ConnectionId, Engine};

/// PostHog's one-client session inactivity boundary, not a call-count limit.
const SESSION_IDLE_US: u64 = 30 * 60 * 1_000_000;

#[derive(Debug)]
pub(crate) struct ToolSession {
    context: ToolTraceContext,
    started_us: u64,
    active_us: u64,
    calling: HashMap<RequestId, Call>,
    started: u64,
    completed: u64,
    errors: u64,
    cancelled: u64,
    disconnected: u64,
}

#[derive(Debug)]
struct Call {
    id: String,
    sequence: u64,
    method: String,
    started_us: u64,
    approval_since_us: Option<u64>,
    approval_wait_us: u64,
}

impl Call {
    fn timing(&self, now: u64) -> (u64, u64, u64) {
        let duration = now.saturating_sub(self.started_us);
        let waiting = self.approval_wait_us
            + self
                .approval_since_us
                .map_or(0, |since| now.saturating_sub(since));
        (duration, waiting, duration.saturating_sub(waiting))
    }
}

impl ToolSession {
    fn new(context: ToolTraceContext, now: u64) -> Self {
        Self {
            context,
            started_us: now,
            active_us: now,
            calling: HashMap::new(),
            started: 0,
            completed: 0,
            errors: 0,
            cancelled: 0,
            disconnected: 0,
        }
    }
}

fn client_category(name: &str) -> &'static str {
    match name.trim().to_ascii_lowercase().as_str() {
        "codex" | "codex-cli" | "openai-codex" => "codex",
        "claude code" | "claude-code" => "claude-code",
        "claude" | "claude-desktop" | "claude-ai" => "claude",
        "cursor" => "cursor",
        "vscode" | "visual studio code" => "vscode",
        "uxduck-agent" | "uxduck" => "uxduck-agent",
        _ => "other",
    }
}

impl Engine {
    /// Browser consent gates tracing independently of the engine's saved settings.
    /// Withdrawing consent discards in-flight traces and rotates every identifier.
    pub fn set_tool_tracing_allowed(&mut self, allowed: bool) {
        if self.tool_tracing_allowed != allowed {
            self.tool_tracing_allowed = allowed;
            self.reset_tool_sessions();
        }
    }

    /// `UUIDv7` with independent OS/browser randomness. Never a stored chat ID.
    pub(crate) fn tool_trace_id(&mut self) -> String {
        let mut bytes = [0u8; 16];
        self.random.fill(&mut bytes);
        let time = (self.clock.now_us() / 1000).to_be_bytes();
        bytes[..6].copy_from_slice(&time[2..]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
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

    pub(crate) fn tool_session_connected(&mut self, connection: ConnectionId) {
        if !self.state.settings.usage_analytics || !self.tool_tracing_allowed {
            return;
        }
        let Some(state) = self.connections.get(&connection) else {
            return;
        };
        if state.tool_session.is_some() {
            return;
        }
        let context = ToolTraceContext {
            tool_connection_id: self.tool_trace_id(),
            tool_session_id: self.tool_trace_id(),
            tool_source: if self.connections[&connection].transport == Transport::Internal {
                ToolTraceSource::UxduckAgent
            } else {
                ToolTraceSource::ExternalMcp
            },
            runtime: if self.config.capabilities.runs_in == RunsIn::Browser {
                "browser"
            } else {
                "native"
            }
            .into(),
            client: if self.connections[&connection].transport == Transport::Internal {
                "uxduck-agent"
            } else {
                client_category(&self.connections[&connection].name)
            }
            .into(),
            server_version: self.config.daemon_version.clone(),
            engine_build: self
                .config
                .about
                .commit
                .as_ref()
                .filter(|sha| {
                    (7..=40).contains(&sha.len()) && sha.bytes().all(|c| c.is_ascii_hexdigit())
                })
                .cloned(),
        };
        let now = self.clock.now_us();
        if let Some(state) = self.connections.get_mut(&connection) {
            state.tool_session = Some(ToolSession::new(context.clone(), now));
        }
        self.track(DaemonEvent::ToolSessionStarted { context });
    }

    pub(crate) fn reset_tool_sessions(&mut self) {
        let agents: Vec<_> = self
            .connections
            .iter_mut()
            .filter_map(|(id, state)| {
                state.tool_session = None;
                state
                    .admitted
                    .as_ref()
                    .filter(|a| a.kind == uxduck_core::domain::client::ClientKind::Agent)
                    .map(|_| *id)
            })
            .collect();
        for id in agents {
            self.tool_session_connected(id);
        }
    }

    pub(crate) fn tool_started(
        &mut self,
        connection: ConnectionId,
        request: RequestId,
        method: &str,
    ) {
        self.activity_started(connection, request.clone(), method);
        let now = self.clock.now_us();
        let rotate = self
            .connections
            .get(&connection)
            .and_then(|c| c.tool_session.as_ref())
            .filter(|s| s.calling.is_empty() && now.saturating_sub(s.active_us) >= SESSION_IDLE_US)
            .map(|s| s.context.clone());
        if let Some(mut context) = rotate {
            self.tool_session_closed(connection, "idle");
            context.tool_session_id = self.tool_trace_id();
            if let Some(c) = self.connections.get_mut(&connection) {
                c.tool_session = Some(ToolSession::new(context.clone(), now));
            }
            self.track(DaemonEvent::ToolSessionStarted { context });
        }
        let Some(session) = self
            .connections
            .get_mut(&connection)
            .and_then(|c| c.tool_session.as_mut())
        else {
            return;
        };
        if session.calling.contains_key(&request) {
            return;
        }
        session.started += 1;
        session.active_us = now;
        // Method names are UXDuck's registry, never arbitrary client input.
        let method = uxduck_contracts::rpc::ClientRequest::METHODS
            .iter()
            .find(|known| **known == method)
            .copied()
            .unwrap_or("other")
            .to_owned();
        let call = Call {
            id: format!("{}:{}", session.context.tool_session_id, session.started),
            sequence: session.started,
            method,
            started_us: now,
            approval_since_us: None,
            approval_wait_us: 0,
        };
        let event = DaemonEvent::ToolCallStarted {
            context: session.context.clone(),
            call_id: call.id.clone(),
            sequence: call.sequence,
            method: call.method.clone(),
        };
        session.calling.insert(request, call);
        self.track(event);
    }

    pub(crate) fn tool_finished(
        &mut self,
        connection: ConnectionId,
        request: &RequestId,
        error: Option<&Value>,
    ) {
        self.activity_finished(
            connection,
            request,
            if error.is_some() {
                uxduck_contracts::dto::client::ToolActivityStatus::Failed
            } else {
                uxduck_contracts::dto::client::ToolActivityStatus::Completed
            },
        );
        let now = self.clock.now_us();
        let Some(session) = self
            .connections
            .get_mut(&connection)
            .and_then(|c| c.tool_session.as_mut())
        else {
            return;
        };
        let Some(call) = session.calling.remove(request) else {
            return;
        };
        let error = error.map(|error| {
            error
                .pointer("/data/kind")
                .or_else(|| error.get("kind"))
                .and_then(|kind| {
                    serde_json::from_value::<uxduck_contracts::errors::ErrorKind>(kind.clone()).ok()
                })
                .and_then(|kind| serde_json::to_value(kind).ok())
                .and_then(|kind| kind.as_str().map(str::to_owned))
                .unwrap_or_else(|| "other".into())
        });
        session.completed += 1;
        session.errors += u64::from(error.is_some());
        session.active_us = now;
        let (duration_us, approval_wait_us, execution_us) = call.timing(now);
        let event = DaemonEvent::ToolCallFinished {
            context: session.context.clone(),
            call_id: call.id,
            sequence: call.sequence,
            method: call.method,
            ok: error.is_none(),
            error,
            duration_us,
            approval_wait_us,
            execution_us,
        };
        self.track(event);
    }

    pub(crate) fn tool_interrupted(
        &mut self,
        connection: ConnectionId,
        request: &RequestId,
        reason: ToolInterruption,
    ) {
        self.activity_interrupted(connection, request, reason);
        let now = self.clock.now_us();
        let Some(session) = self
            .connections
            .get_mut(&connection)
            .and_then(|c| c.tool_session.as_mut())
        else {
            return;
        };
        let Some(call) = session.calling.remove(request) else {
            return;
        };
        match reason {
            ToolInterruption::Cancelled => session.cancelled += 1,
            ToolInterruption::Disconnected => session.disconnected += 1,
        }
        session.active_us = now;
        let (duration_us, approval_wait_us, execution_us) = call.timing(now);
        let event = DaemonEvent::ToolCallInterrupted {
            context: session.context.clone(),
            call_id: call.id,
            sequence: call.sequence,
            method: call.method,
            reason,
            duration_us,
            approval_wait_us,
            execution_us,
        };
        self.track(event);
    }

    pub(crate) fn tool_session_closed(&mut self, connection: ConnectionId, reason: &str) {
        let requests: Vec<_> = self
            .connections
            .get(&connection)
            .and_then(|c| c.tool_session.as_ref())
            .map(|s| s.calling.keys().cloned().collect())
            .unwrap_or_default();
        for request in requests {
            self.tool_interrupted(connection, &request, ToolInterruption::Disconnected);
        }
        let Some(session) = self
            .connections
            .get_mut(&connection)
            .and_then(|c| c.tool_session.take())
        else {
            return;
        };
        self.track(DaemonEvent::ToolSessionEnded {
            context: session.context,
            duration_us: self.clock.now_us().saturating_sub(session.started_us),
            reason: reason.into(),
            started: session.started,
            completed: session.completed,
            errors: session.errors,
            cancelled: session.cancelled,
            disconnected: session.disconnected,
        });
    }

    /// Approval RPC IDs and in-app item IDs stay local. Only elapsed time leaves.
    pub(crate) fn tool_approval(
        &mut self,
        connection: ConnectionId,
        request: &Value,
        item: Option<&str>,
        waiting: bool,
    ) {
        self.activity_approval(connection, request, item, waiting);
        let now = self.clock.now_us();
        let Some(session) = self
            .connections
            .get_mut(&connection)
            .and_then(|c| c.tool_session.as_mut())
        else {
            return;
        };
        let key = if session.context.tool_source == ToolTraceSource::UxduckAgent {
            item.map(|id| RequestId::String(id.into()))
        } else {
            serde_json::from_value(request.clone()).ok()
        };
        let Some(call) = key.and_then(|key| session.calling.get_mut(&key)) else {
            return;
        };
        if waiting {
            if call.approval_since_us.is_some() {
                return;
            }
            call.approval_since_us = Some(now);
        } else if let Some(since) = call.approval_since_us.take() {
            call.approval_wait_us += now.saturating_sub(since);
        } else {
            return;
        }
        let (duration_us, approval_wait_us, execution_us) = call.timing(now);
        let event = DaemonEvent::ToolApprovalChanged {
            context: session.context.clone(),
            call_id: call.id.clone(),
            sequence: call.sequence,
            method: call.method.clone(),
            waiting,
            duration_us,
            approval_wait_us,
            execution_us,
        };
        self.track(event);
    }
}

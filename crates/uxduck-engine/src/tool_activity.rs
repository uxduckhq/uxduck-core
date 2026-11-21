//! Local metadata for the operator's Activity view, independent of analytics.
use crate::{ConnectionId, Engine};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use uxduck_contracts::dto::analytics::{ToolInterruption, ToolTraceSource};
use uxduck_contracts::dto::client::{ToolActivityCall, ToolActivityResult, ToolActivityStatus};
use uxduck_contracts::dto::trace::{RequestId, Transport};

/// Bound diagnostic memory on small browser devices. Active calls are never evicted.
/// The API and view report evictions and this budget instead of silently truncating.
const RECENT_BYTES: u32 = 256 * 1024;
#[derive(Default)]
pub(crate) struct Activity {
    sessions: HashMap<ConnectionId, String>,
    active: HashMap<(ConnectionId, RequestId), Pending>,
    recent: VecDeque<ToolActivityCall>,
    bytes: usize,
    discarded: u64,
}
struct Pending {
    row: ToolActivityCall,
    waiting_since: Option<u64>,
}
impl Pending {
    fn snapshot(&self, now: u64) -> ToolActivityCall {
        let mut row = self.row.clone();
        row.duration_us = now.saturating_sub(row.started_us);
        row.approval_wait_us += self
            .waiting_since
            .map_or(0, |since| now.saturating_sub(since));
        row
    }
}
fn size(row: &ToolActivityCall) -> usize {
    std::mem::size_of::<ToolActivityCall>()
        + row.id.len()
        + row.session_id.len()
        + row.client_id.len()
        + row.method.len()
}
impl Engine {
    pub(crate) fn activity_started(
        &mut self,
        connection: ConnectionId,
        request: RequestId,
        method: &str,
    ) {
        let Some(client) = self.connections.get(&connection) else {
            return;
        };
        let Some(admitted) = client.admitted.as_ref() else {
            return;
        };
        let key = (connection, request);
        if self.tool_activity.active.contains_key(&key) {
            return;
        }
        let client_id = admitted.record.clone();
        let source = if client.transport == Transport::Internal {
            ToolTraceSource::UxduckAgent
        } else {
            ToolTraceSource::ExternalMcp
        };
        let id = self.tool_trace_id();
        let session_id = if let Some(id) = self.tool_activity.sessions.get(&connection) {
            id.clone()
        } else {
            let id = self.tool_trace_id();
            self.tool_activity.sessions.insert(connection, id.clone());
            id
        };
        self.tool_activity.active.insert(
            key,
            Pending {
                row: ToolActivityCall {
                    id,
                    session_id,
                    client_id,
                    source,
                    method: uxduck_contracts::rpc::ClientRequest::METHODS
                        .iter()
                        .find(|name| **name == method)
                        .copied()
                        .unwrap_or("other")
                        .into(),
                    status: ToolActivityStatus::Running,
                    started_us: self.clock.now_us(),
                    duration_us: 0,
                    approval_wait_us: 0,
                },
                waiting_since: None,
            },
        );
    }
    pub(crate) fn activity_finished(
        &mut self,
        connection: ConnectionId,
        request: &RequestId,
        status: ToolActivityStatus,
    ) {
        let Some(pending) = self
            .tool_activity
            .active
            .remove(&(connection, request.clone()))
        else {
            return;
        };
        let mut row = pending.snapshot(self.clock.now_us());
        row.status = status;
        self.tool_activity.bytes += size(&row);
        self.tool_activity.recent.push_back(row);
        while self.tool_activity.bytes > RECENT_BYTES as usize {
            let Some(old) = self.tool_activity.recent.pop_front() else {
                break;
            };
            self.tool_activity.bytes -= size(&old);
            self.tool_activity.discarded += 1;
        }
    }
    pub(crate) fn activity_interrupted(
        &mut self,
        connection: ConnectionId,
        request: &RequestId,
        reason: ToolInterruption,
    ) {
        self.activity_finished(
            connection,
            request,
            match reason {
                ToolInterruption::Cancelled => ToolActivityStatus::Cancelled,
                ToolInterruption::Disconnected => ToolActivityStatus::Disconnected,
            },
        );
    }
    pub(crate) fn activity_closed(&mut self, connection: ConnectionId) {
        self.tool_activity.sessions.remove(&connection);
        let requests: Vec<_> = self
            .tool_activity
            .active
            .keys()
            .filter(|(id, _)| *id == connection)
            .map(|(_, request)| request.clone())
            .collect();
        for request in requests {
            self.activity_finished(connection, &request, ToolActivityStatus::Disconnected);
        }
    }
    pub(crate) fn activity_approval(
        &mut self,
        connection: ConnectionId,
        request: &Value,
        item: Option<&str>,
        waiting: bool,
    ) {
        let key = if self
            .connections
            .get(&connection)
            .is_some_and(|c| c.transport == Transport::Internal)
        {
            item.map(|id| RequestId::String(id.into()))
        } else {
            serde_json::from_value(request.clone()).ok()
        };
        let Some(key) = key else { return };
        let Some(pending) = self.tool_activity.active.get_mut(&(connection, key)) else {
            return;
        };
        let now = self.clock.now_us();
        if waiting {
            pending.waiting_since.get_or_insert(now);
            pending.row.status = ToolActivityStatus::WaitingForApproval;
        } else {
            if let Some(since) = pending.waiting_since.take() {
                pending.row.approval_wait_us += now.saturating_sub(since);
            }
            pending.row.status = ToolActivityStatus::Running;
        }
    }
    pub(crate) fn activity_snapshot(&self) -> ToolActivityResult {
        let now = self.clock.now_us();
        let mut calls: Vec<_> = self
            .tool_activity
            .active
            .values()
            .map(|p| p.snapshot(now))
            .chain(self.tool_activity.recent.iter().cloned())
            .collect();
        calls.sort_by(|a, b| {
            b.started_us
                .cmp(&a.started_us)
                .then_with(|| b.id.cmp(&a.id))
        });
        ToolActivityResult {
            calls,
            discarded: self.tool_activity.discarded,
            retained_bytes: RECENT_BYTES,
        }
    }
}

//! What a feedback bundle keeps of a vehicle's routes and a link's socket:
//! their state and failures, never an address (ADR-0016, "The diagnostics
//! bundle").

use uxduck_core::domain::vehicle as core;

use crate::dto::diagnostics::{TransportDiagnostics, TransportError};
use crate::dto::feedback::{
    BundleConnections, BundleRoute, BundleSocketSend, BundleTransport, BundleTransportError,
};

impl From<&core::VehicleConnections> for BundleConnections {
    fn from(c: &core::VehicleConnections) -> Self {
        Self {
            command_available: c.command_available,
            reason: c.reason.clone(),
            epoch: c.epoch,
            routes: c
                .routes
                .iter()
                .map(|r| BundleRoute {
                    link_id: r.link_id.clone(),
                    system_id: r.system_id,
                    priority: r.priority,
                    reachable: r.reachable,
                    writable: r.writable,
                    selected: r.selected,
                    telemetry: r.telemetry,
                    last_heartbeat_us: r.last_heartbeat_us,
                    round_trip_us: r.round_trip_us,
                    loss_per_mille: r.loss_per_mille,
                    last_receive_age_ms: r.last_receive_age_ms,
                    reason: r.reason.clone(),
                })
                .collect(),
        }
    }
}

impl From<&TransportError> for BundleTransportError {
    fn from(e: &TransportError) -> Self {
        Self {
            at_us: e.at_us,
            operation: e.operation.clone(),
            kind: e.kind.clone(),
            os_code: e.os_code,
            message: e.message.clone(),
            active: e.active,
        }
    }
}

impl From<&TransportDiagnostics> for BundleTransport {
    fn from(t: &TransportDiagnostics) -> Self {
        Self {
            generation: t.generation,
            socket_open: t.socket_open,
            wildcard_bind: t.wildcard_bind,
            local_network_permission: t.local_network_permission.clone(),
            last_receive_us: t.last_receive_us,
            last_valid_mavlink_us: t.last_valid_mavlink_us,
            last_heartbeat_us: t.last_heartbeat_us,
            last_send_attempt_us: t.last_send_attempt_us,
            last_socket_send_us: t.last_socket_send_us,
            send_error: t.send_error.as_ref().map(Into::into),
            receive_error: t.receive_error.as_ref().map(Into::into),
            recovery_error: t.recovery_error.as_ref().map(Into::into),
            recovery_reason: t.recovery_reason.clone(),
            registration_attempts: t.registration_attempts,
            registration_suppressed: t.registration_suppressed,
            recreation_attempts: t.recreation_attempts,
            backoff_ms: t.backoff_ms,
            next_retry_us: t.next_retry_us,
            dropped_receive_events: t.dropped_receive_events,
            sends: t
                .sends
                .iter()
                .map(|s| BundleSocketSend {
                    at_us: s.at_us,
                    registration: s.registration,
                    outcome: s.outcome.clone(),
                    os_code: s.error.as_ref().and_then(|e| e.os_code),
                })
                .collect(),
        }
    }
}

//! Read-only diagnostic queries. Evidence is kept by the composition root;
//! identity, permissions and bounds are checked here before touching the host.
use crate::Engine;
use serde_json::Value;
use uxduck_contracts::dto::diagnostics::{LinkDiagnoseResult, RouterEventsParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkKind;

/// Resolve a diagnostic time range.
/// # Errors
/// Rejects a reversed range or a window longer than ten minutes.
pub fn window(from: Option<u64>, to: Option<u64>, now: u64) -> Result<(u64, u64), ApiError> {
    let end = to.unwrap_or(now);
    let start = from.unwrap_or_else(|| end.saturating_sub(60_000_000));
    if start > end || end.saturating_sub(start) > 600_000_000 {
        return Err(invalid(
            "Choose an ordered time window of at most ten minutes (Unix microseconds).",
        ));
    }
    Ok((start, end))
}

/// Validate the bounded diagnostic page size.
/// # Errors
/// Rejects limits outside 1–200.
pub fn page_size(limit: Option<u32>) -> Result<usize, ApiError> {
    let n = limit.unwrap_or(50);
    if !(1..=200).contains(&n) {
        return Err(invalid("A diagnostic page contains 1 to 200 entries."));
    }
    Ok(n as usize)
}

#[must_use]
pub fn invalid(text: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, text)
}

pub(crate) fn handle(
    engine: &Engine,
    request: &ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let now = engine.clock.now_us();
    match request {
        ClientRequest::RouterEvents(p) => {
            if p.before.is_none() || p.to_us.is_some() {
                window(p.from_us, p.to_us, now)?;
            }
            page_size(p.limit)?;
            super::rpc::handlers::to_value(&engine.system.router_events(p, now)?)
        }
        ClientRequest::DaemonLogs(p) => {
            if p.before.is_none() || p.to_us.is_some() {
                window(p.from_us, p.to_us, now)?;
            }
            page_size(p.limit)?;
            if p.level.as_ref().is_some_and(|s| {
                !["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
                    .contains(&s.to_ascii_uppercase().as_str())
            }) {
                return Err(invalid(
                    "Log level must be trace, debug, info, warn or error.",
                ));
            }
            super::rpc::handlers::to_value(&engine.system.daemon_logs(p, now)?)
        }
        ClientRequest::SocketInspect(p) => {
            if p.port == 0 {
                return Err(invalid("Choose a socket port from 1 to 65535."));
            }
            if p.host
                .as_ref()
                .is_some_and(|h| h.parse::<std::net::IpAddr>().is_err())
            {
                return Err(invalid("The socket host filter must be an IP address."));
            }
            super::rpc::handlers::to_value(&engine.system.socket_inspect(p)?)
        }
        ClientRequest::LinkDiagnose(p) => diagnose(engine, &p.query),
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a diagnostics method",
        )),
    }
}

fn diagnose(engine: &Engine, query: &str) -> Result<Value, ApiError> {
    let link = resolve_link(engine, query)?;
    let mut vehicles: Vec<uxduck_contracts::dto::vehicle::Vehicle> = engine
        .state
        .vehicles
        .values()
        .filter(|v| {
            if link.kind.is_forward() {
                link.from_link.as_ref().map_or_else(
                    || link.vehicle_filter.is_empty() || link.vehicle_filter.contains(&v.id),
                    |source| v.link_id == *source,
                )
            } else {
                v.link_id == link.id
            }
        })
        .map(Into::into)
        .take(201)
        .collect();
    let source = link
        .from_link
        .as_ref()
        .and_then(|id| engine.state.links.get(id))
        .map(Into::into);
    let mut forwards: Vec<uxduck_contracts::dto::link::Link> = engine
        .state
        .links
        .values()
        .filter(|l| {
            if link.kind.is_forward() || !l.kind.is_forward() || l.id == link.id {
                return false;
            }
            if let Some(from) = &l.from_link {
                return from == &link.id;
            }
            l.vehicle_filter.is_empty()
                || engine
                    .state
                    .vehicles
                    .values()
                    .any(|v| v.link_id == link.id && l.vehicle_filter.contains(&v.id))
        })
        .map(Into::into)
        .take(201)
        .collect();
    let relationships_truncated = vehicles.len() > 200 || forwards.len() > 200;
    vehicles.truncate(200);
    forwards.truncate(200);
    let routing = match &link.kind {
        LinkKind::UdpForward { host, port } => format!(
            "Sends to UDP {host}:{port}; {}. {}",
            if link.policy == uxduck_core::domain::link::LinkPolicy::Read {
                "telemetry only"
            } else {
                "the other program can also command its vehicles"
            },
            if link.from_link.is_some() {
                "Only its source simulation's traffic is carried."
            } else if link.vehicle_filter.is_empty() {
                "All vehicles' traffic is carried."
            } else {
                "Only the configured vehicle filter is carried."
            }
        ),
        _ => "Vehicle traffic arrives on this connection and goes to its eligible forwards.".into(),
    };
    let recent_events = engine.system.router_events(
        &RouterEventsParams {
            link_id: Some(link.id.clone()),
            ..Default::default()
        },
        engine.clock.now_us(),
    )?;
    super::rpc::handlers::to_value(&LinkDiagnoseResult {
        link: link.into(),
        vehicles,
        source,
        forwards,
        relationships_truncated,
        routing,
        recent_events,
        last_incident: engine.system.last_router_incident(&link.id),
    })
}

fn resolve_link<'a>(
    engine: &'a Engine,
    query: &str,
) -> Result<&'a uxduck_core::domain::link::Link, ApiError> {
    if let Some(link) = engine.state.links.get(query.trim()) {
        return Ok(link);
    }
    if let Some(vehicle) = engine.vehicle(query.trim()) {
        return engine.state.links.get(&vehicle.link_id).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                "That vehicle has no configured connection.",
            )
        });
    }
    let query = query.trim();
    if query.is_empty() {
        return Err(invalid("Supply a link or vehicle id or exact name."));
    }
    let mut candidates = engine
        .state
        .links
        .values()
        .filter(|l| l.id == query || l.name.eq_ignore_ascii_case(query))
        .map(|l| l.id.clone())
        .collect::<Vec<_>>();
    for v in engine.state.vehicles.values() {
        if v.id == query
            || v.aliases.iter().any(|id| id == query)
            || v.name.eq_ignore_ascii_case(query)
        {
            candidates.push(v.link_id.clone());
        }
    }
    candidates.sort();
    candidates.dedup();
    if candidates.len() > 1 {
        return Err(invalid(
            "That name identifies several connections. Use a link or vehicle id.",
        ));
    }
    let link = candidates
        .first()
        .and_then(|id| engine.state.links.get(id))
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("No link or vehicle named {query}"),
            )
        })?;
    Ok(link)
}

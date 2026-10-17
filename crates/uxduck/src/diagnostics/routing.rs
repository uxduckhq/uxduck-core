use super::{
    Capture, EVENTS, INCIDENTS, bounds, capture, cursor, endpoint, evidence, limit,
    persisted_packet,
};
use uxduck_contracts::dto::diagnostics::{
    DiagnosticCoverage, RouterDecision, RouterEvent, RouterEventsParams, RouterEventsResult,
};
use uxduck_contracts::errors::ApiError;
use uxduck_mavlink::framing::Header;
use uxduck_mavlink::router::Routed;
pub fn record_routing(
    link: &str,
    peer: &str,
    bytes: &[u8],
    header: Header,
    routed: &Routed,
    at_us: u64,
) {
    if !routed.looped && routed.sends.is_empty() {
        return;
    }
    let mut data = capture();
    data.seq += 1;
    let matched_packet = routed.matched.as_ref().map(|matched| {
        let (source, peer) = matched
            .from
            .as_ref()
            .map_or((matched.to_link.as_str(), None), |(source, peer)| {
                (source.as_str(), Some(peer.as_str()))
            });
        persisted_packet(
            &data,
            evidence(&data, source, peer, bytes, header, matched.sent_at_us),
        )
    });
    let event = RouterEvent {
        id: format!("{}-{:020}", data.run, data.seq),
        packet: persisted_packet(
            &data,
            evidence(&data, link, Some(peer), bytes, header, at_us),
        ),
        decision: if routed.looped {
            RouterDecision::DroppedLoop
        } else {
            RouterDecision::Forwarded
        },
        destinations: routed
            .sends
            .iter()
            .take(64)
            .map(|sent| endpoint(&data, &sent.link_id, sent.peer.as_deref()))
            .collect(),
        destinations_truncated: routed.sends.len() > 64,
        reason: if routed.looped {
            "An exact-wire fingerprint returned through a different source."
        } else {
            "The router selected these destinations. Network delivery is not acknowledged."
        }
        .into(),
        matched_packet,
        forward_link_id: routed
            .matched
            .as_ref()
            .and_then(|m| m.forward_link_id.clone()),
        disabled: !routed.disable.is_empty(),
        threshold: routed
            .matched
            .as_ref()
            .map(|m| u32::try_from(m.threshold).unwrap_or(u32::MAX)),
        window_us: routed.matched.as_ref().map(|m| m.window_us),
    };
    if data.events.len() == EVENTS {
        data.events.pop_front();
        data.event_loss = true;
    }
    data.events.push_back(event.clone());
    if event.disabled {
        preserve(&mut data, &event);
    }
}
fn preserve(data: &mut Capture, latest: &RouterEvent) {
    let evidence = data
        .events
        .iter()
        .filter(|e| {
            e.decision == RouterDecision::DroppedLoop
                && e.forward_link_id == latest.forward_link_id
                && latest.packet.at_us.saturating_sub(e.packet.at_us)
                    <= latest.window_us.unwrap_or(0)
        })
        .cloned()
        .collect::<Vec<_>>();
    for event in evidence {
        if !data.incidents.iter().any(|old| old.id == event.id) {
            data.incidents.push_back(event);
        }
    }
    while data.incidents.len() > INCIDENTS {
        data.incidents.pop_front();
    }
    let Some(path) = &data.path else {
        return;
    };
    let result = serde_json::to_vec(&data.incidents)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(tmp, path)
        });
    if let Err(error) = result {
        data.persistence_error = Some(error.to_string());
        tracing::warn!(%error, "Could not preserve router incident evidence");
    } else {
        data.persistence_error = None;
    }
}
fn involves(event: &RouterEvent, link: &str) -> bool {
    event.packet.endpoint.link_id == link
        || event.forward_link_id.as_deref() == Some(link)
        || event.destinations.iter().any(|d| d.link_id == link)
        || event
            .matched_packet
            .as_ref()
            .is_some_and(|p| p.endpoint.link_id == link)
}
fn project(mut event: RouterEvent, raw: bool) -> RouterEvent {
    if !raw {
        event.packet.raw_hex = None;
        if let Some(matched) = &mut event.matched_packet {
            matched.raw_hex = None;
        }
    }
    event
}
pub fn last_incident(link: &str) -> Option<RouterEvent> {
    capture()
        .incidents
        .iter()
        .rev()
        .find(|e| involves(e, link) && e.disabled)
        .cloned()
        .map(|e| project(e, false))
}
pub fn events(params: &RouterEventsParams, now: u64) -> Result<RouterEventsResult, ApiError> {
    let (from, to, key, before) = bounds(
        "router",
        params,
        params.from_us,
        params.to_us,
        params.before.as_deref(),
        now,
    )?;
    let size = limit(params.limit)?;
    let data = capture();
    let mut all = data
        .events
        .iter()
        .chain(&data.incidents)
        .collect::<Vec<_>>();
    all.sort_by(|a, b| (b.packet.at_us, &b.id).cmp(&(a.packet.at_us, &a.id)));
    all.dedup_by(|a, b| a.id == b.id);
    let cutoff = if let Some(before) = before {
        Some(
            all.iter()
                .find(|e| e.id == before)
                .map(|e| (e.packet.at_us, e.id.clone()))
                .ok_or_else(|| {
                    uxduck_engine::diagnostics::invalid("That router cursor is no longer retained.")
                })?,
        )
    } else {
        None
    };
    let mut found = all
        .iter()
        .filter(|e| {
            e.packet.at_us >= from
                && e.packet.at_us <= to
                && cutoff
                    .as_ref()
                    .is_none_or(|(at, id)| (e.packet.at_us, &e.id) < (*at, id))
                && params.link_id.as_ref().is_none_or(|id| involves(e, id))
                && params.decision.is_none_or(|d| e.decision == d)
        })
        .take(size + 1)
        .map(|e| (*e).clone())
        .collect::<Vec<_>>();
    let more = found.len() > size;
    found.truncate(size);
    let older = if more {
        found
            .last()
            .map(|e| cursor("router", from, to, &key, &e.id))
    } else {
        None
    };
    let mut notes = vec!["Normal decisions are kept in a bounded memory ring. Loop incidents survive daemon restart.".into()];
    if let Some(error) = &data.persistence_error {
        notes.push(format!("Incident persistence failed: {error}"));
    }
    let coverage = DiagnosticCoverage {
        requested_from_us: from,
        requested_to_us: to,
        available_from_us: all.iter().map(|e| e.packet.at_us).min(),
        available_to_us: all.iter().map(|e| e.packet.at_us).max(),
        truncated: data.event_loss
            || data.persistence_error.is_some()
            || all.iter().any(|e| e.destinations_truncated),
        source: "memory-and-preserved-incidents".into(),
        notes,
    };
    Ok(RouterEventsResult {
        events: found
            .into_iter()
            .map(|e| project(e, params.include_raw))
            .collect(),
        older,
        coverage,
    })
}

use super::{bounds, capture, cursor, hex, limit};
use std::path::Path;
use uxduck_contracts::dto::diagnostics::DiagnosticCoverage;
use uxduck_contracts::dto::inspect::{
    InspectDirection, MavlinkHistoryParams, MavlinkHistoryResult, PastMessage,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_mavlink::framing::{header, payload};

fn message(
    bytes: &[u8],
    at_us: u64,
    link: &str,
    peer: Option<String>,
    raw: bool,
) -> Option<PastMessage> {
    let h = header(bytes)?;
    let decoded = uxduck_mavlink::dialect::decode(h.message_id, payload(bytes)?);
    Some(PastMessage {
        at_us,
        system_id: h.system_id,
        component_id: h.component_id,
        id: Some(h.message_id),
        name: decoded.as_ref().map(|d| d.name.to_owned()),
        sequence: Some(h.sequence),
        link_id: Some(link.to_owned()),
        peer,
        fingerprint: Some(uxduck_mavlink::router::fingerprint(bytes)),
        raw_hex: raw.then(|| hex(bytes)),
        fields: decoded.map(|d| d.fields),
    })
}

pub fn history(
    dir: &Path,
    link: &str,
    system: Option<u8>,
    params: &MavlinkHistoryParams,
    now: u64,
) -> Result<MavlinkHistoryResult, ApiError> {
    let (from, to, key, before) = bounds(
        "packets",
        params,
        params.from_us,
        params.to_us,
        params.before.as_deref(),
        now,
    )?;
    let size = limit(params.limit)?;
    if params.direction == InspectDirection::Sent {
        return sent(
            link,
            system,
            params,
            (from, to),
            &key,
            before.as_deref(),
            size,
        );
    }
    let page = crate::tlog_index::packet_page(
        dir,
        link,
        system,
        (!params.all_messages).then_some(params.id),
        (from, to),
        before.as_deref(),
        size,
    )
    .map_err(|e| {
        ApiError::new(
            ErrorKind::InvalidValue,
            format!("Could not read recorded packets: {e}"),
        )
    })?;
    let data = capture();
    let messages = page
        .records
        .iter()
        .filter_map(|r| {
            let fp = uxduck_mavlink::router::fingerprint(&r.frame);
            let peer = data
                .frames
                .iter()
                .rev()
                .find(|f| {
                    f.direction == InspectDirection::Received
                        && f.packet.at_us == r.at_us
                        && f.packet.endpoint.link_id == link
                        && f.packet.fingerprint == fp
                })
                .and_then(|f| f.packet.endpoint.peer.clone());
            message(&r.frame, r.at_us, link, peer, params.include_raw)
        })
        .collect::<Vec<_>>();
    let unknown_peer = messages.iter().any(|m| m.peer.is_none());
    let mut notes = page.notes;
    if unknown_peer {
        notes.push("Historical tlogs do not store peer addresses; peer is only filled when the exact packet is still captured in this daemon run.".into());
    }
    if messages.is_empty() {
        notes.push(
            "No matching received packets were found in the retained/scan-covered interval.".into(),
        );
    }
    Ok(MavlinkHistoryResult {
        messages,
        older: page.older.map(|p| cursor("packets", from, to, &key, &p)),
        from_log: true,
        coverage: Some(DiagnosticCoverage {
            requested_from_us: from,
            requested_to_us: to,
            available_from_us: page.available_from_us,
            available_to_us: page.available_to_us,
            truncated: page.truncated,
            source: "tlog-with-recent-provenance".into(),
            notes,
        }),
    })
}

fn sent(
    link: &str,
    system: Option<u8>,
    params: &MavlinkHistoryParams,
    range: (u64, u64),
    key: &str,
    before: Option<&str>,
    size: usize,
) -> Result<MavlinkHistoryResult, ApiError> {
    let data = capture();
    let cutoff = if let Some(before) = before {
        let (run, seq) = before
            .split_once('-')
            .ok_or_else(|| uxduck_engine::diagnostics::invalid("Invalid sent-packet cursor."))?;
        if run.parse::<u64>().ok() != Some(data.run) {
            return Err(uxduck_engine::diagnostics::invalid(
                "That sent-packet cursor belongs to an earlier daemon run.",
            ));
        }
        let seq = seq
            .parse::<u64>()
            .map_err(|_| uxduck_engine::diagnostics::invalid("Invalid sent-packet cursor."))?;
        if !data.frames.iter().any(|f| f.seq == seq) {
            return Err(uxduck_engine::diagnostics::invalid(
                "That sent-packet cursor is no longer retained.",
            ));
        }
        seq
    } else {
        u64::MAX
    };
    let all = data
        .frames
        .iter()
        .filter(|f| f.packet.endpoint.link_id == link && f.direction == InspectDirection::Sent)
        .collect::<Vec<_>>();
    let mut found = all
        .iter()
        .rev()
        .filter(|f| {
            f.seq < cutoff
                && f.packet.at_us >= range.0
                && f.packet.at_us <= range.1
                && system.is_none_or(|id| id == f.packet.system_id)
                && (params.all_messages || params.id == f.packet.message_id)
        })
        .take(size + 1)
        .collect::<Vec<_>>();
    let more = found.len() > size;
    found.truncate(size);
    let older = if more {
        found.last().map(|f| {
            cursor(
                "packets",
                range.0,
                range.1,
                key,
                &format!("{}-{}", data.run, f.seq),
            )
        })
    } else {
        None
    };
    Ok(MavlinkHistoryResult {
        messages: found
            .into_iter()
            .filter_map(|f| message(
                &f.bytes,
                f.packet.at_us,
                link,
                f.packet.endpoint.peer.clone(),
                params.include_raw,
            ))
            .collect(),
        older,
        from_log: false,
        coverage: Some(DiagnosticCoverage {
            requested_from_us: range.0,
            requested_to_us: range.1,
            available_from_us: all.first().map(|f| f.packet.at_us),
            available_to_us: all.last().map(|f| f.packet.at_us),
            truncated: data.frame_loss || range.0 < data.run,
            source: "memory".into(),
            notes: vec!["Outbound packets record queued sends, not delivery acknowledgements, and are retained only in this daemon run's bounded capture ring.".into()],
        }),
    })
}

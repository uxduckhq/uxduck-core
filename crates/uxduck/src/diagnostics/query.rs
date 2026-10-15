//! Query-bound cursors freeze the default time window across pages.
use serde::Serialize;
use uxduck_contracts::errors::{ApiError, ErrorKind};
pub fn limit(value: Option<u32>) -> Result<usize, ApiError> {
    uxduck_engine::diagnostics::page_size(value)
}
type QueryBounds = (u64, u64, String, Option<String>);

pub fn bounds<T: Serialize>(
    prefix: &str,
    params: &T,
    from: Option<u64>,
    to: Option<u64>,
    before: Option<&str>,
    now: u64,
) -> Result<QueryBounds, ApiError> {
    let mut json = serde_json::to_value(params)
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
    if let Some(obj) = json.as_object_mut() {
        for field in ["fromUs", "toUs", "before", "limit", "includeRaw"] {
            obj.remove(field);
        }
    }
    let key = uxduck_mavlink::router::fingerprint(
        &serde_json::to_vec(&json)
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?,
    );
    if let Some(before) = before {
        let fields = before.splitn(5, '/').collect::<Vec<_>>();
        if fields.len() != 5 || fields[0] != prefix || fields[3] != key {
            return Err(uxduck_engine::diagnostics::invalid(
                "Not a cursor from this diagnostic query.",
            ));
        }
        let start = fields[1]
            .parse::<u64>()
            .map_err(|_| uxduck_engine::diagnostics::invalid("Invalid diagnostic cursor."))?;
        let end = fields[2]
            .parse::<u64>()
            .map_err(|_| uxduck_engine::diagnostics::invalid("Invalid diagnostic cursor."))?;
        uxduck_engine::diagnostics::window(Some(start), Some(end), now)?;
        if from.is_some_and(|v| v != start) || to.is_some_and(|v| v != end) {
            return Err(uxduck_engine::diagnostics::invalid(
                "Keep the same time window when paging diagnostics.",
            ));
        }
        return Ok((start, end, key, Some(fields[4].to_owned())));
    }
    let (start, end) = uxduck_engine::diagnostics::window(from, to, now)?;
    Ok((start, end, key, None))
}
pub fn cursor(prefix: &str, from: u64, to: u64, key: &str, position: &str) -> String {
    format!("{prefix}/{from}/{to}/{key}/{position}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_contracts::dto::diagnostics::RouterEventsParams;
    #[test]
    fn cursors_freeze_default_windows_and_reject_changed_filters() {
        let mut params = RouterEventsParams {
            link_id: Some("wire".into()),
            ..Default::default()
        };
        let (from, to, key, _) = bounds("router", &params, None, None, None, 100_000_000).unwrap();
        params.before = Some(cursor("router", from, to, &key, "record-1"));
        params.include_raw = true;
        params.limit = Some(1);
        let page = bounds(
            "router",
            &params,
            None,
            None,
            params.before.as_deref(),
            200_000_000,
        )
        .unwrap();
        assert_eq!((page.0, page.1), (from, to));
        params.link_id = Some("another-wire".into());
        assert!(
            bounds(
                "router",
                &params,
                None,
                None,
                params.before.as_deref(),
                200_000_000
            )
            .is_err()
        );
        assert!(
            bounds(
                "logs",
                &params,
                None,
                None,
                params.before.as_deref(),
                200_000_000
            )
            .is_err()
        );
    }
}

//! Recent telemetry deltas, bounded globally by time, observations and bytes.
//! No timer, subscription or carry-forward snapshots: only real producer
//! updates are retained. Core MAVLink updates have already been coalesced.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use uxduck_contracts::dto::telemetry_window::{
    PositionWindowSeries, TelemetryCoverage, TelemetryNumericSummary, TelemetryTextState,
    TelemetryWindowParams, TelemetryWindowResult, TelemetryWindowSample, TelemetryWindowSeries,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::telemetry::{PositionEstimate, TelemetryItem, VehicleTelemetry};

use crate::Engine;

const RETENTION_US: u64 = 300_000_000;
const MAX_OBSERVATIONS: usize = 100_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_SERIES: usize = 64;
const SAMPLE_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_TEXT_STATES: usize = 8;
/// Points per series: a chart's five minutes at 10 Hz (S113).
const MAX_SAMPLES: u16 = 3000;

#[derive(Debug)]
struct Batch {
    received_us: u64,
    bytes: usize,
    telemetry: VehicleTelemetry,
}

#[derive(Debug, Default)]
pub(crate) struct Window {
    batches: VecDeque<Batch>,
    observations: usize,
    bytes: usize,
    /// Conservative global boundary, including size-limit evictions.
    dropped_through_us: Option<u64>,
}

impl Window {
    pub(crate) fn observe(&mut self, received_us: u64, telemetry: &VehicleTelemetry) {
        self.prune(received_us);
        let observations = telemetry.items.len() + telemetry.positions.len();
        if observations == 0 {
            return;
        }
        let bytes = batch_bytes(telemetry);
        if observations > MAX_OBSERVATIONS || bytes > MAX_BYTES || !bounded_payload(telemetry) {
            // Never clone an oversized update. Signal incomplete retention.
            self.dropped_through_us = Some(self.dropped_through_us.unwrap_or(0).max(received_us));
            return;
        }
        while self.observations + observations > MAX_OBSERVATIONS || self.bytes + bytes > MAX_BYTES
        {
            self.pop();
        }
        self.observations += observations;
        self.bytes += bytes;
        self.batches.push_back(Batch {
            received_us,
            bytes,
            telemetry: telemetry.clone(),
        });
    }

    fn pop(&mut self) {
        if let Some(batch) = self.batches.pop_front() {
            self.observations -= batch.telemetry.items.len() + batch.telemetry.positions.len();
            self.bytes -= batch.bytes;
            self.dropped_through_us =
                Some(self.dropped_through_us.unwrap_or(0).max(batch.received_us));
        }
    }

    fn prune(&mut self, now_us: u64) {
        let cutoff = now_us.saturating_sub(RETENTION_US);
        while self.batches.front().is_some_and(|b| b.received_us < cutoff) {
            self.pop();
        }
    }

    fn retained_from(&self, now_us: u64, started_us: u64) -> u64 {
        now_us.saturating_sub(RETENTION_US).max(started_us).max(
            self.dropped_through_us
                .map_or(0, |time| time.saturating_add(1)),
        )
    }

    fn query(
        &mut self,
        params: &TelemetryWindowParams,
        ids: &BTreeSet<String>,
        now_us: u64,
        started_us: u64,
    ) -> Result<TelemetryWindowResult, ApiError> {
        let (start, end, limit) = bounds(params, now_us)?;
        self.prune(now_us);
        let retained_from = self.retained_from(now_us, started_us);
        let mut items = BTreeMap::<(String, String, u8, u8), Vec<&TelemetryItem>>::new();
        let mut positions = BTreeMap::<(String, u8), Vec<&PositionEstimate>>::new();
        for batch in &self.batches {
            if !ids.contains(&batch.telemetry.vehicle_id) {
                continue;
            }
            for item in &batch.telemetry.items {
                if (start..=end).contains(&item.time_us) && params.keys.contains(&item.key) {
                    items
                        .entry((
                            item.key.clone(),
                            item.source.clone(),
                            item.component_id,
                            item.unit as u8,
                        ))
                        .or_default()
                        .push(item);
                }
            }
            for position in &batch.telemetry.positions {
                if (start..=end).contains(&position.time_us)
                    && params.position_sources.contains(&position.source)
                {
                    positions
                        .entry((position.source.clone(), position.component_id))
                        .or_default()
                        .push(position);
                }
            }
        }
        let seen_keys: BTreeSet<&str> = items.keys().map(|(key, _, _, _)| key.as_str()).collect();
        let seen_sources: BTreeSet<&str> = positions
            .keys()
            .map(|(source, _)| source.as_str())
            .collect();
        let missing_keys = params
            .keys
            .iter()
            .filter(|k| !seen_keys.contains(k.as_str()))
            .cloned()
            .collect();
        let missing_position_sources = params
            .position_sources
            .iter()
            .filter(|s| !seen_sources.contains(s.as_str()))
            .cloned()
            .collect();
        let omitted_series_count =
            count((items.len() + positions.len()).saturating_sub(MAX_SERIES));
        let position_limit = MAX_SERIES.saturating_sub(items.len());
        let mut result = TelemetryWindowResult {
            vehicle_id: params.vehicle_id.clone(),
            start_time_us: start,
            end_time_us: end,
            retained_from_us: retained_from,
            history_truncated: start < retained_from,
            capture: "producer-updates; core-mavlink-coalesced-up-to-10hz".into(),
            missing_keys,
            missing_position_sources,
            omitted_series_count,
            samples_limited: false,
            items: items
                .into_values()
                .take(MAX_SERIES)
                .map(|mut samples| {
                    samples.sort_by_key(|s| s.time_us);
                    item_series(&samples, limit)
                })
                .collect(),
            positions: positions
                .into_values()
                .take(position_limit)
                .map(|mut samples| {
                    samples.sort_by_key(|s| s.time_us);
                    position_series(&samples, limit)
                })
                .collect(),
        };
        limit_sample_output(&mut result);
        Ok(result)
    }
}

/// An untrusted plugin cannot fill metadata or a response with arbitrary
/// strings. Oversized batches are skipped and advance the incomplete-history
/// boundary; neither their text nor their numeric evidence is silently clipped.
fn bounded_payload(telemetry: &VehicleTelemetry) -> bool {
    telemetry.vehicle_id.len() <= 256
        && telemetry.items.iter().all(|item| {
            item.key.len() <= 256
                && item.label.len() <= 256
                && item.source.len() <= 256
                && item.text.as_ref().is_none_or(|text| text.len() <= 1024)
        })
        && telemetry
            .positions
            .iter()
            .all(|position| position.source.len() <= 256 && position.altitudes.len() <= 4)
}

/// Metadata and all-window summaries remain even when sample points exhaust
/// the shared response budget. Serialize selected points to account for JSON
/// escaping exactly; at most `MAX_SERIES * MAX_SAMPLES` points are considered.
fn limit_sample_output(result: &mut TelemetryWindowResult) {
    let mut budget = SAMPLE_OUTPUT_BYTES;
    // Preserve observed state changes before sampled points consume budget.
    for series in &mut result.items {
        series.text_states.retain(|state| {
            if within_budget(state, &mut budget) {
                true
            } else {
                series.text_states_truncated = true;
                result.samples_limited = true;
                false
            }
        });
    }
    for series in &mut result.items {
        retain_sample_output(
            &mut series.samples,
            &mut series.coverage,
            &mut budget,
            &mut result.samples_limited,
        );
    }
    for series in &mut result.positions {
        retain_sample_output(
            &mut series.samples,
            &mut series.coverage,
            &mut budget,
            &mut result.samples_limited,
        );
    }
}

/// Keeps the newest points that fit, so a chart's history meets its live
/// data rather than stopping short of it (S113).
fn retain_sample_output<T: serde::Serialize>(
    samples: &mut Vec<T>,
    coverage: &mut TelemetryCoverage,
    budget: &mut usize,
    limited: &mut bool,
) {
    let mut first_kept = samples.len();
    while first_kept > 0 && within_budget(&samples[first_kept - 1], budget) {
        first_kept -= 1;
    }
    if first_kept > 0 {
        *limited = true;
        coverage.downsampled = true;
        samples.drain(..first_kept);
    }
}

fn within_budget<T: serde::Serialize>(value: &T, budget: &mut usize) -> bool {
    let bytes = serde_json::to_vec(value).map_or(usize::MAX, |json| json.len().saturating_add(1));
    if bytes <= *budget {
        *budget -= bytes;
        true
    } else {
        false
    }
}

/// Estimate the retained allocation, including all dynamic payloads. The
/// observation ceiling also bounds allocation/container overhead.
fn batch_bytes(t: &VehicleTelemetry) -> usize {
    std::mem::size_of::<Batch>()
        + t.vehicle_id.len()
        + t.items
            .iter()
            .map(|i| {
                std::mem::size_of::<TelemetryItem>()
                    + i.key.len()
                    + i.label.len()
                    + i.source.len()
                    + i.text.as_ref().map_or(0, String::len)
            })
            .sum::<usize>()
        + t.positions
            .iter()
            .map(|p| {
                std::mem::size_of::<PositionEstimate>()
                    + p.source.len()
                    + p.altitudes.len()
                        * std::mem::size_of::<uxduck_core::domain::telemetry::Altitude>()
            })
            .sum::<usize>()
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn bounds(params: &TelemetryWindowParams, now_us: u64) -> Result<(u64, u64, usize), ApiError> {
    if params.keys.len() > 32
        || params.position_sources.len() > 8
        || (params.keys.is_empty() && params.position_sources.is_empty())
        || params
            .keys
            .iter()
            .chain(&params.position_sources)
            .any(|s| s.is_empty() || s.len() > 256)
    {
        return Err(invalid(
            "select 1–32 keys and/or 1–8 positionSources. Names must be 1–256 bytes",
        ));
    }
    if params.start_time_us.is_some() && params.duration_ms.is_some() {
        return Err(invalid("startTimeUs and durationMs cannot be combined"));
    }
    let end = params.end_time_us.unwrap_or(now_us);
    if end > now_us {
        return Err(invalid("endTimeUs must be no later than now"));
    }
    let start = if let Some(start) = params.start_time_us {
        start
    } else {
        let duration = params.duration_ms.unwrap_or(120_000);
        if !(1..=300_000).contains(&duration) {
            return Err(invalid("durationMs must be between 1 and 300000"));
        }
        end.saturating_sub(duration * 1_000)
    };
    if start > end || end - start > RETENTION_US {
        return Err(invalid(
            "window must be ordered and span at most five minutes",
        ));
    }
    let limit = params.max_samples.unwrap_or(20);
    if !(1..=MAX_SAMPLES).contains(&limit) {
        return Err(invalid("maxSamples must be between 1 and 3000 per series"));
    }
    Ok((start, end, usize::from(limit)))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn coverage(times: impl Iterator<Item = u64>, limit: usize) -> TelemetryCoverage {
    let mut coverage = TelemetryCoverage::default();
    for time in times {
        if let Some(last) = coverage.last_time_us {
            coverage.max_gap_us = coverage.max_gap_us.max(time.saturating_sub(last));
        }
        coverage.first_time_us.get_or_insert(time);
        coverage.last_time_us = Some(time);
        coverage.count += 1;
    }
    coverage.downsampled = coverage.count > count(limit);
    coverage
}

/// Evenly spaced observation indices, including the endpoints when possible.
/// Extrema are separately retained in the summary, including their timestamps.
fn indices(len: usize, limit: usize) -> impl Iterator<Item = usize> {
    let take = len.min(limit);
    (0..take).map(move |index| {
        if take <= 1 {
            len - 1
        } else {
            index * (len - 1) / (take - 1)
        }
    })
}

fn item_series(samples: &[&TelemetryItem], limit: usize) -> TelemetryWindowSeries {
    // Called only for nonempty groups.
    let metadata = samples[0];
    let mut numeric: Option<TelemetryNumericSummary> = None;
    let mut text_states: Vec<TelemetryTextState> = vec![];
    let mut text_states_truncated = false;
    for sample in samples {
        if let Some(text) = &sample.text {
            if let Some(state) = text_states.iter_mut().find(|state| &state.text == text) {
                state.count += 1;
                state.last_time_us = sample.time_us;
            } else if text_states.len() < MAX_TEXT_STATES {
                text_states.push(TelemetryTextState {
                    text: text.clone(),
                    count: 1,
                    first_time_us: sample.time_us,
                    last_time_us: sample.time_us,
                });
            } else {
                text_states_truncated = true;
            }
        }
        if let Some(value) = sample.value.filter(|v| v.is_finite()) {
            if let Some(summary) = &mut numeric {
                summary.count += 1;
                if value < summary.min {
                    summary.min = value;
                    summary.min_time_us = sample.time_us;
                }
                if value > summary.max {
                    summary.max = value;
                    summary.max_time_us = sample.time_us;
                }
            } else {
                numeric = Some(TelemetryNumericSummary {
                    count: 1,
                    missing_numeric_count: 0,
                    min: value,
                    min_time_us: sample.time_us,
                    max: value,
                    max_time_us: sample.time_us,
                });
            }
        }
    }
    if let Some(summary) = &mut numeric {
        summary.missing_numeric_count = count(samples.len()) - summary.count;
    }
    TelemetryWindowSeries {
        key: metadata.key.clone(),
        label: metadata.label.clone(),
        unit: metadata.unit.into(),
        source: metadata.source.clone(),
        component_id: metadata.component_id,
        coverage: coverage(samples.iter().map(|s| s.time_us), limit),
        numeric,
        text_states,
        text_states_truncated,
        samples: indices(samples.len(), limit)
            .map(|index| {
                let sample = samples[index];
                TelemetryWindowSample {
                    time_us: sample.time_us,
                    value: sample.value.filter(|v| v.is_finite()),
                    text: sample.text.clone(),
                }
            })
            .collect(),
    }
}

fn position_series(samples: &[&PositionEstimate], limit: usize) -> PositionWindowSeries {
    PositionWindowSeries {
        source: samples[0].source.clone(),
        component_id: samples[0].component_id,
        coverage: coverage(samples.iter().map(|s| s.time_us), limit),
        samples: indices(samples.len(), limit)
            .map(|index| samples[index].into())
            .collect(),
    }
}

impl Engine {
    pub(crate) fn telemetry_window(
        &mut self,
        params: &TelemetryWindowParams,
    ) -> Result<TelemetryWindowResult, ApiError> {
        let vehicle = self.vehicle(&params.vehicle_id).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            )
        })?;
        let mut ids: BTreeSet<String> = vehicle.aliases.iter().cloned().collect();
        ids.insert(vehicle.id.clone());
        let canonical_id = vehicle.id.clone();
        let mut result =
            self.telemetry_history
                .query(params, &ids, self.clock.now_us(), self.started_us)?;
        result.vehicle_id = canonical_id;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        reason = "deterministic telemetry fixtures"
    )]
    use super::*;
    use uxduck_core::domain::telemetry::Unit;

    fn params() -> TelemetryWindowParams {
        TelemetryWindowParams {
            vehicle_id: "v".into(),
            keys: vec!["vibration.x_mps2".into()],
            position_sources: vec![],
            duration_ms: None,
            start_time_us: Some(1_000_000),
            end_time_us: Some(2_000_000),
            max_samples: Some(2),
        }
    }

    fn update(time_us: u64, value: Option<f64>) -> VehicleTelemetry {
        VehicleTelemetry {
            vehicle_id: "v".into(),
            positions: vec![],
            items: vec![TelemetryItem {
                key: "vibration.x_mps2".into(),
                label: "Vibration X".into(),
                unit: Unit::MeterPerSecondSquared,
                value,
                text: None,
                source: "autopilot".into(),
                component_id: 1,
                time_us,
            }],
        }
    }

    fn query(
        window: &mut Window,
        params: &TelemetryWindowParams,
        now_us: u64,
    ) -> TelemetryWindowResult {
        window
            .query(params, &BTreeSet::from(["v".into()]), now_us, 0)
            .unwrap()
    }

    #[test]
    fn bounded_sampling_retains_transient_peak_and_missing_numeric_evidence() {
        let mut window = Window::default();
        for (time, value) in [
            (1_000_000, Some(1.0)),
            (1_100_000, Some(99.0)),
            (1_200_000, None),
            (2_000_000, Some(2.0)),
        ] {
            window.observe(time, &update(time, value));
        }
        let mut p = params();
        p.keys.push("gps.unknown".into());
        let result = query(&mut window, &p, 2_000_000);
        assert_eq!(result.missing_keys, ["gps.unknown"]);
        let series = &result.items[0];
        assert_eq!(
            series.samples.iter().map(|s| s.value).collect::<Vec<_>>(),
            [Some(1.0), Some(2.0)]
        );
        assert_eq!(series.coverage.count, 4);
        assert_eq!(series.coverage.max_gap_us, 800_000);
        assert!(series.coverage.downsampled);
        let summary = series.numeric.as_ref().unwrap();
        assert_eq!((summary.max, summary.max_time_us), (99.0, 1_100_000));
        assert_eq!((summary.count, summary.missing_numeric_count), (3, 1));
    }

    #[test]
    fn time_bounds_are_inclusive_and_no_latest_value_is_carried_forward() {
        let mut window = Window::default();
        for time in [999_999, 1_000_000, 1_000_001, 2_000_000, 2_000_001] {
            window.observe(time, &update(time, Some(3.0)));
        }
        let result = query(&mut window, &params(), 2_000_001);
        assert_eq!(result.items[0].coverage.count, 3);
        assert_eq!(result.items[0].coverage.first_time_us, Some(1_000_000));
        assert_eq!(result.items[0].coverage.last_time_us, Some(2_000_000));
        let mut p = params();
        p.start_time_us = Some(3_000_000);
        p.end_time_us = Some(4_000_000);
        let result = query(&mut window, &p, 4_000_000);
        assert!(result.items.is_empty());
        assert_eq!(result.missing_keys, ["vibration.x_mps2"]);
    }

    #[test]
    fn expiry_and_daemon_startup_are_reported_as_partial_history() {
        let mut window = Window::default();
        window.observe(1_000_000, &update(1_000_000, Some(1.0)));
        window.observe(301_000_001, &update(301_000_001, Some(2.0)));
        assert_eq!(window.observations, 1);
        let mut p = params();
        p.start_time_us = None;
        p.end_time_us = None;
        p.duration_ms = Some(300_000);
        let result = query(&mut window, &p, 301_000_001);
        assert_eq!(result.retained_from_us, 1_000_001);
        assert!(!result.history_truncated);
        let result = window
            .query(&p, &BTreeSet::from(["v".into()]), 301_000_001, 200_000_000)
            .unwrap();
        assert!(result.history_truncated);
        assert_eq!(result.retained_from_us, 200_000_000);
        // An idle query also expires history, without adding a timer.
        let result = query(&mut window, &p, 700_000_000);
        assert!(result.items.is_empty());
        assert_eq!(window.bytes, 0);
    }

    #[test]
    fn byte_and_observation_limits_evict_without_unbounded_growth() {
        let mut window = Window::default();
        let mut large = update(1_000_000, Some(1.0));
        large.items[0].text = Some("x".repeat(1024));
        large.items = vec![large.items[0].clone(); 1000];
        let fitting = MAX_BYTES / batch_bytes(&large);
        for index in 0..=fitting {
            window.observe(1_000_000 + u64::try_from(index).unwrap(), &large);
        }
        assert!(window.bytes <= MAX_BYTES);
        assert_eq!(window.batches.len(), fitting);
        assert_eq!(window.retained_from(1_100_000, 0), 1_000_001);

        let mut window = Window::default();
        let mut many = update(1_000_000, Some(1.0));
        many.items = vec![many.items[0].clone(); MAX_OBSERVATIONS];
        window.observe(1_000_000, &many);
        assert_eq!(window.observations, MAX_OBSERVATIONS);
        window.observe(1_100_000, &update(1_100_000, Some(2.0)));
        assert_eq!(window.observations, 1);
        assert!(window.bytes <= MAX_BYTES);
    }

    #[test]
    fn oversized_plugin_payload_is_reported_without_cloning_or_silent_clipping() {
        let mut window = Window::default();
        let mut batch = update(1_000_000, Some(42.0));
        batch.items[0].text = Some("x".repeat(1025));
        window.observe(1_000_000, &batch);
        let result = query(&mut window, &params(), 2_000_000);
        assert_eq!(window.bytes, 0);
        assert!(result.history_truncated);
        assert_eq!(result.retained_from_us, 1_000_001);
        assert_eq!(result.missing_keys, ["vibration.x_mps2"]);
    }

    #[test]
    fn escaped_text_cannot_exceed_shared_sample_budget_and_summaries_survive() {
        let mut window = Window::default();
        for component_id in [1, 2] {
            for index in 0_u32..100 {
                let mut batch = update(1_000_000 + u64::from(index) * 1000, Some(f64::from(index)));
                batch.items[0].component_id = component_id;
                batch.items[0].text = Some("\0".repeat(1024));
                window.observe(batch.items[0].time_us, &batch);
            }
        }
        let mut p = params();
        p.max_samples = Some(100);
        let result = query(&mut window, &p, 2_000_000);
        assert!(result.samples_limited);
        assert_eq!(result.items.len(), 2);
        assert!(result.items.iter().all(|series| {
            series.coverage.count == 100
                && series.coverage.downsampled
                && series.numeric.as_ref().unwrap().max == 99.0
        }));
        assert!(serde_json::to_vec(&result).unwrap().len() < SAMPLE_OUTPUT_BYTES + 32 * 1024);
    }

    #[test]
    fn impossible_windows_and_unbounded_selectors_are_rejected() {
        let mut p = params();
        p.duration_ms = Some(1);
        assert!(bounds(&p, 2_000_000).is_err());
        p.duration_ms = None;
        p.max_samples = Some(3000);
        assert!(bounds(&p, 2_000_000).is_ok());
        p.max_samples = Some(3001);
        assert!(bounds(&p, 2_000_000).is_err());
        p.max_samples = Some(1);
        p.end_time_us = Some(2_000_001);
        assert!(bounds(&p, 2_000_000).is_err());
        p.end_time_us = Some(2_000_000);
        p.keys.clear();
        assert!(bounds(&p, 2_000_000).is_err());
        p.keys = vec!["key".into(); 33];
        assert!(bounds(&p, 2_000_000).is_err());
        p.keys = vec!["key".into()];
        p.start_time_us = Some(0);
        p.end_time_us = Some(RETENTION_US + 1);
        assert!(bounds(&p, RETENTION_US + 1).is_err());
    }

    #[test]
    fn aliases_components_positions_and_out_of_order_times_are_kept_distinct() {
        let mut window = Window::default();
        let mut batch = update(1_500_000, Some(3.0));
        window.observe(1_500_000, &batch);
        batch.vehicle_id = "old-v".into();
        batch.items[0].time_us = 1_100_000;
        window.observe(1_600_000, &batch);
        batch.items[0].component_id = 2;
        batch.positions.push(PositionEstimate {
            source: "fused".into(),
            component_id: 1,
            latitude_deg: 1.0,
            longitude_deg: 2.0,
            altitudes: vec![],
            heading_deg: None,
            course_deg: None,
            ground_speed_mps: None,
            climb_mps: None,
            horizontal_accuracy_m: None,
            vertical_accuracy_m: None,
            time_us: 1_600_000,
        });
        window.observe(1_600_000, &batch);
        let mut p = params();
        p.position_sources = vec!["fused".into(), "gps".into()];
        let result = window
            .query(
                &p,
                &BTreeSet::from(["v".into(), "old-v".into()]),
                2_000_000,
                0,
            )
            .unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].coverage.count, 2);
        assert_eq!(result.items[0].samples[0].time_us, 1_100_000);
        assert_eq!(result.items[1].component_id, 2);
        assert_eq!(result.positions.len(), 1);
        assert_eq!(result.positions[0].samples[0].latitude_deg, 1.0);
        assert_eq!(result.missing_position_sources, ["gps"]);
    }

    #[test]
    fn changing_units_and_sources_do_not_produce_misleading_extrema() {
        let mut window = Window::default();
        let mut batch = update(1_100_000, Some(5.0));
        window.observe(1_100_000, &batch);
        batch.items[0].time_us = 1_200_000;
        batch.items[0].unit = Unit::None;
        batch.items[0].value = Some(99.0);
        window.observe(1_200_000, &batch);
        batch.items[0].source = "plugin".into();
        window.observe(1_300_000, &batch);
        let result = query(&mut window, &params(), 2_000_000);
        assert_eq!(result.items.len(), 3);
        assert!(result.items.iter().all(|series| series.coverage.count == 1));
        let measured = result
            .items
            .iter()
            .find(|series| {
                series.unit == uxduck_contracts::dto::telemetry::Unit::MeterPerSecondSquared
            })
            .unwrap();
        assert_eq!(measured.numeric.as_ref().unwrap().max, 5.0);
    }

    #[test]
    fn returned_series_limit_is_explicit_without_misreporting_missing_keys() {
        let mut window = Window::default();
        for component_id in 0..65 {
            let mut batch = update(1_000_000, Some(1.0));
            batch.items[0].component_id = component_id;
            window.observe(1_000_000, &batch);
        }
        let result = query(&mut window, &params(), 2_000_000);
        assert_eq!(result.items.len(), 64);
        assert_eq!(result.omitted_series_count, 1);
        assert!(result.missing_keys.is_empty());
    }

    #[test]
    fn brief_bad_text_state_survives_good_endpoints_and_one_point_limit() {
        let mut window = Window::default();
        for (time_us, text) in [(1_000_000, "Good"), (1_100_000, "Bad"), (1_200_000, "Good")] {
            let mut batch = update(time_us, None);
            batch.items[0].text = Some(text.into());
            window.observe(time_us, &batch);
        }
        for limit in [1, 2] {
            let mut p = params();
            p.max_samples = Some(limit);
            let result = query(&mut window, &p, 2_000_000);
            let series = &result.items[0];
            assert!(
                series
                    .samples
                    .iter()
                    .all(|sample| sample.text.as_deref() == Some("Good"))
            );
            assert_eq!(series.text_states.len(), 2);
            assert_eq!(
                series.text_states[0],
                TelemetryTextState {
                    text: "Good".into(),
                    count: 2,
                    first_time_us: 1_000_000,
                    last_time_us: 1_200_000,
                }
            );
            assert_eq!(
                series.text_states[1],
                TelemetryTextState {
                    text: "Bad".into(),
                    count: 1,
                    first_time_us: 1_100_000,
                    last_time_us: 1_100_000,
                }
            );
            assert!(!series.text_states_truncated);
            assert!(!result.samples_limited);
        }
    }

    #[test]
    fn distinct_text_state_cap_reports_omissions_and_keeps_complete_known_counts() {
        let mut window = Window::default();
        for index in 0..10 {
            let time_us = 1_000_000 + index * 1000;
            let mut batch = update(time_us, None);
            // Ninth distinct state is omitted; a later repeat of the first
            // state must still contribute to its complete count and last time.
            batch.items[0].text = Some(format!("state{}", index % 9));
            window.observe(time_us, &batch);
        }
        let result = query(&mut window, &params(), 2_000_000);
        let series = &result.items[0];
        assert_eq!(series.text_states.len(), 8);
        assert!(series.text_states_truncated);
        assert_eq!(series.text_states[0].count, 2);
        assert_eq!(series.text_states[0].last_time_us, 1_009_000);
        assert_eq!(series.coverage.count, 10);
        assert!(
            !result.samples_limited,
            "the state cap, rather than byte budget, caused omission"
        );
    }

    #[test]
    fn text_state_summaries_share_escaped_byte_budget_and_report_late_omissions() {
        let mut window = Window::default();
        for component_id in 0..64 {
            for index in 0..8 {
                let time_us = 1_000_000 + index * 1000;
                let mut batch = update(time_us, Some(1.0));
                batch.items[0].component_id = component_id;
                batch.items[0].text = Some(format!("{}{}", "\0".repeat(1000), index));
                window.observe(time_us, &batch);
            }
        }
        let mut p = params();
        p.max_samples = Some(1);
        let result = query(&mut window, &p, 2_000_000);
        assert!(result.samples_limited);
        assert_eq!(
            result.items[0].text_states.len(),
            8,
            "text summaries receive budget before sample points"
        );
        assert!(
            result
                .items
                .iter()
                .any(|series| series.text_states_truncated)
        );
        assert!(
            result
                .items
                .iter()
                .all(|series| series.numeric.as_ref().unwrap().count == 8)
        );
        assert!(serde_json::to_vec(&result).unwrap().len() < SAMPLE_OUTPUT_BYTES + 64 * 1024);
    }
}

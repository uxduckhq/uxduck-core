//! Bounded recent telemetry, captured by the engine before an agent polls it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::telemetry::{PositionEstimate, Unit};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryWindowParams {
    pub vehicle_id: String,
    /// Up to 32 exact telemetry keys. Empty selects no item series.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Up to eight exact position sources, such as `fused` or `gps`.
    /// Empty selects no positions. At least one selector is required.
    #[serde(default)]
    pub position_sources: Vec<String>,
    /// Last N milliseconds ending at `endTimeUs` (default: now), from 1 to
    /// 300000. Defaults to 120000. Cannot be combined with `startTimeUs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub duration_ms: Option<u64>,
    /// Inclusive daemon receive time, in Unix microseconds. An explicit
    /// window may span no more than five minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub start_time_us: Option<u64>,
    /// Inclusive daemon receive time, in Unix microseconds, at most now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub end_time_us: Option<u64>,
    /// Returned points per series, 1–3000 (default 20). Summaries always use
    /// every retained observation in the requested interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryCoverage {
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub first_time_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_time_us: Option<u64>,
    /// Largest interval between retained observations, not a packet-loss
    /// measurement: the expected sending rate is unknown.
    #[schemars(schema_with = "crate::safe::integer")]
    pub max_gap_us: u64,
    pub downsampled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryWindowSample {
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryNumericSummary {
    pub count: u32,
    /// Observations without a finite numeric value, including text values.
    pub missing_numeric_count: u32,
    pub min: f64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub min_time_us: u64,
    pub max: f64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub max_time_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryTextState {
    pub text: String,
    /// Occurrences among all retained observations in the requested window.
    pub count: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_time_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_time_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryWindowSeries {
    pub key: String,
    pub label: String,
    pub unit: Unit,
    pub source: String,
    pub component_id: u8,
    pub coverage: TelemetryCoverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric: Option<TelemetryNumericSummary>,
    /// First eight distinct observed text states, with full-window counts
    /// and timestamps, so a brief bad state survives sampled point limits.
    /// These summaries share the response byte budget with sample points.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_states: Vec<TelemetryTextState>,
    /// Additional distinct states or byte-budget omissions exist. Returned
    /// states retain their complete counts; absence is then inconclusive.
    #[serde(default)]
    pub text_states_truncated: bool,
    pub samples: Vec<TelemetryWindowSample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PositionWindowSeries {
    pub source: String,
    pub component_id: u8,
    pub coverage: TelemetryCoverage,
    pub samples: Vec<PositionEstimate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryWindowResult {
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub start_time_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub end_time_us: u64,
    /// Conservative retention boundary. The buffer lasts five minutes or
    /// less under its global 32 MiB / 100000-observation memory limits.
    #[schemars(schema_with = "crate::safe::integer")]
    pub retained_from_us: u64,
    /// This request extends before daemon startup or retained history.
    pub history_truncated: bool,
    /// Core MAVLink observations are coalesced at up to 10 Hz before capture;
    /// these are not raw flight-controller log samples.
    pub capture: String,
    /// Requested selectors with no retained observations in the interval.
    pub missing_keys: Vec<String>,
    pub missing_position_sources: Vec<String>,
    /// At most 64 item and position series are returned, in stable order.
    pub omitted_series_count: u32,
    /// Sample points or text-state summaries exceeded their shared 128 KiB
    /// serialized budget. Numeric summaries still cover every retained
    /// observation; affected series set `coverage.downsampled` for points
    /// and/or `textStatesTruncated` for text-state omissions.
    pub samples_limited: bool,
    pub items: Vec<TelemetryWindowSeries>,
    pub positions: Vec<PositionWindowSeries>,
}

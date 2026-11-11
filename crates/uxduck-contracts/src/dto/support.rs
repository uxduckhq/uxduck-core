//! A bounded support snapshot shared by About and feedback. It deliberately
//! contains no addresses, data-directory paths, device IDs or flight values.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SupportMode {
    Web,
    Desktop,
    BrowserDesktop,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportBuild {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportRuntime {
    pub build: SupportBuild,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub uptime_us: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportPerformance {
    pub window_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_task_ms: Option<u32>,
    pub max_input_delay_ms: u32,
    pub controller_gaps: u32,
    pub max_controller_gap_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heap_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_cpu_percent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_memory_mb: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportDetails {
    pub captured_at: String,
    pub mode: SupportMode,
    /// The client's state name, without an address or free-form error message.
    pub connection: String,
    pub page: SupportBuild,
    pub interface_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_interface_id: Option<String>,
    pub protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<SupportRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<SupportBuild>,
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    pub window: String,
    pub locale: String,
    pub performance: SupportPerformance,
    /// Page-owned diagnostics remain available when the browser runtime cannot answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_runtime: Option<SupportBrowserRuntime>,
    /// Named missing measurements, so unsupported and failed reads are explicit.
    #[serde(default)]
    pub unavailable: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportBrowserRuntime {
    pub page_age_ms: u32,
    pub navigation: String,
    pub discarded: bool,
    pub used_before: bool,
    pub visibility: String,
    pub visibility_changes: u32,
    pub worker_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_age_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply_age_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_telemetry_age_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation_age_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<String>,
    /// Last known simulator counts survive loss of the engine connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_simulation_count: Option<u32>,
    /// At most 80 metadata-only events; never request/response bodies or user text.
    #[serde(default)]
    pub events: Vec<SupportBrowserEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupportBrowserEvent {
    pub at_ms: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u32>,
}

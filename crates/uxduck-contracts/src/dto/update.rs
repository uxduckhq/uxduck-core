//! Full product updates, owned by the background process, independently of Electron.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePhase {
    #[default]
    Unavailable,
    Checking,
    Current,
    Downloading,
    Ready,
    Installing,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent update consent, presentation and compatibility flags on the wire"
)]
pub struct AppUpdateStatus {
    pub phase: UpdatePhase,
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub received: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub size: Option<u64>,
    /// This engine pushes changed status after `update/status` is requested.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub push_updates: bool,
    pub blockers: Vec<String>,
    /// True only when running simulations are the sole work blocking an install.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_install_with_simulations: bool,
    pub install_when_idle: bool,
    pub dismissed: bool,
}

/// Consent names the exact downloaded version. It never covers future releases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInstallParams {
    pub version: String,
    pub when_idle: bool,
    /// Explicit consent to stop running simulations during this version's restart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_simulations: bool,
}

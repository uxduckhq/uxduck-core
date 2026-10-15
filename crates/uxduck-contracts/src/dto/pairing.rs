//! Browser pairing and bootstrap tokens (ADR-0001, Transports and authentication).
//! Pairing becomes an approval kind with S10; until then an operator answers
//! it with `pairing/approve`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A browser waiting to be paired.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairingRequest {
    /// The six-digit code the browser shows.
    pub code: String,
    /// The page's origin, for example `http://localhost:5173`.
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Microseconds since the Unix epoch.
    #[schemars(schema_with = "crate::safe::integer")]
    pub requested_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairingListResult {
    pub requests: Vec<PairingRequest>,
}

/// Params of `pairing/approve` and `pairing/deny`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PairingAnswerParams {
    pub code: String,
}

/// Result of `session/createBootstrap`: a one-time token that a page served
/// by this daemon exchanges for a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapToken {
    pub token: String,
    /// Where to open the UI with the token appended as `?bootstrap=`.
    pub ui_url: String,
}

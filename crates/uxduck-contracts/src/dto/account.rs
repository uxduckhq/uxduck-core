//! A UXDuck account, signed in by the daemon (ADR-0013 P9, S27): the page
//! never holds the token.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// API choices for a development daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AccountEnvironment {
    Local,
    Staging,
    Production,
}

/// The active server and the choice saved for the daemon's next start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountEnvironmentState {
    pub environment: Option<AccountEnvironment>,
    pub api_url: Option<String>,
    pub selected: Option<AccountEnvironment>,
    pub can_change: bool,
    pub restart_required: bool,
}

/// Saves a development server choice; null restores startup configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountEnvironmentParams {
    pub environment: Option<AccountEnvironment>,
}

/// Whether this UXDuck is signed in, and as whom.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent account and analytics flags in the wire contract"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    pub signed_in: bool,
    /// Stable, server-verified identity; absent for legacy/offline unknowns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The local operator associated with this fresh sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    /// This account owns the computer's shared cloud dataset. A different
    /// person's sign-in must never transfer the existing dataset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owns_workspace: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub analytics_excluded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What suggestions are credited to, when they choose to be (S73).
    /// Profiles are private: nothing else shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// On the product updates list; unknown while offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_updates: Option<bool>,
    /// Signed in, but the server could not be reached: what was known last.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub offline: bool,
    /// This sign-in made the account (S114): "Account created".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
    /// Said where it heard of UXDuck, or skipped it (S251); `false` asks it
    /// once. Unknown from an older API, which asks nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_answered: Option<bool>,
    /// The Terms of Service version the account last agreed to (ADR-0047),
    /// none if it never has; unknown from an older API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_version: Option<String>,
}

/// Recorded telemetry logs stored in this account, without session credentials.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountFlights {
    pub flights: Vec<AccountFlight>,
}

/// A log's recorded coverage, not an arm-to-disarm flight duration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountFlight {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<String>,
    pub id: String,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub first_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub chunks: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

/// The account's video (ADR-0069 P14): the camera recordings its computers
/// stored, and the live recordings kept while a camera is shared from away,
/// newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecordings {
    pub recordings: Vec<AccountRecording>,
}

/// One recording in the account, by the computer that stored it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecording {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<String>,
    pub id: String,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub first_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// Kept while its camera was shared from away, until the camera's own
    /// recording replaces it.
    #[serde(default)]
    pub live: bool,
}

/// `account/deleteRecording`: one of the account's recordings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecordingParams {
    pub device_id: String,
    pub id: String,
}

/// What deleting a recording freed in the account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecordingDeleted {
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

/// Emails a one-time code to sign in with; an account is made on the first
/// sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSignInParams {
    pub email: String,
}

/// Signs in with the emailed code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountVerifyParams {
    pub email: String,
    pub code: String,
    /// "Send me product updates", off unless ticked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub product_updates: bool,
    /// The Terms of Service version the form said continuing agrees to
    /// (ADR-0047); the account records it. None from an older interface,
    /// which said nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_version: Option<String>,
}

/// Chooses or changes the account's username (S73): 3 to 20 lower-case
/// letters, digits, `-` and `_`, starting with a letter, not one reserved
/// and not one another account has, or had in the last 30 days.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsernameParams {
    pub username: String,
}

/// Sets the account's name, which the team sees with its feedback: spaces
/// collapsed, up to 80 characters; empty removes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountNameParams {
    pub name: String,
}

/// Where a new account heard of UXDuck (S251).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SignupSource {
    Youtube,
    Search,
    Reddit,
    /// The ArduPilot or PX4 forums, or their Discord.
    Forum,
    Friend,
    /// An AI assistant, such as `ChatGPT` or Claude.
    Ai,
    Other,
    /// The person passed on the question.
    Skipped,
}

/// Says where a new account heard of UXDuck (S251), once, after the sign-in
/// that made it. `other` is kept only with `Other`: spaces collapsed, up
/// to 80 characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSourceParams {
    pub source: SignupSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// Joins or leaves the product updates list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountUpdatesParams {
    pub subscribed: bool,
}

/// What this account shared to improve the agent (ADR-0013 P15).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SharedData {
    #[schemars(schema_with = "crate::safe::integer")]
    pub count: u64,
    /// How long a shared turn is kept.
    #[schemars(schema_with = "crate::safe::integer")]
    pub kept_for_days: u64,
    /// The newest, up to 50.
    pub turns: Vec<SharedTurn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SharedTurn {
    pub id: String,
    /// RFC 3339.
    pub created_at: String,
    pub model: String,
    /// The start of what was asked.
    pub preview: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SharedDeleted {
    #[schemars(schema_with = "crate::safe::integer")]
    pub deleted: u64,
}

/// Where the account is signed in (S114, ADR-0013 P20 to P22): each
/// UXDuck Desktop by its computer's name, and each UXDuck Web.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSessions {
    /// Most recently used first.
    pub sessions: Vec<AccountSession>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSession {
    pub id: String,
    /// This computer's own session.
    pub current: bool,
    /// The computer's name, given by UXDuck Desktop; none for a browser.
    #[serde(default)]
    pub computer: Option<String>,
    /// The browser's or UXDuck's user agent, as the server saw it.
    #[serde(default)]
    pub user_agent: Option<String>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339: the day it was last used.
    pub last_active_at: String,
}

/// Signs out one of the account's sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSessionParams {
    pub id: String,
}

/// An explicit account program action. Only operator clients may call it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCreditParams {
    pub action: ToolCreditAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_id: Option<String>,
    #[serde(default)]
    pub observations: Vec<ToolCreditObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCreditAction {
    Status,
    Join,
    Claim,
    Leave,
    Contribute,
}

/// Only completed call metadata, never a local client id, arguments or results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCreditObservation {
    pub id: String,
    pub session_id: String,
    pub method: String,
    pub source: crate::dto::analytics::ToolTraceSource,
    pub status: crate::dto::client::ToolActivityStatus,
    pub runtime: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub duration_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub approval_wait_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCreditState {
    pub active: bool,
    pub version: String,
    pub enrollment_id: Option<String>,
    pub monthly_usd: f64,
    pub granted_usd: f64,
    pub available_usd: f64,
    pub starter_remaining_usd: f64,
    pub claimed_month: Option<String>,
}

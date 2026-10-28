//! Feedback from the app (ADR-0016 O8, O11): the report a person writes,
//! and the diagnostics bundle, `uxduck.feedback/1`, that goes with it as
//! `feedback-bundle.json`. The bundle holds what a developer or an agent
//! needs to act without asking back, scrubbed as ADR-0013 says.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::analytics::Surface;
use crate::dto::trace::TraceFrame;

/// The bundle's schema version.
pub const BUNDLE_SCHEMA: &str = "uxduck.feedback/1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackKind {
    Problem,
    Idea,
    Other,
    /// Something UXDuck cannot do yet, written up from a chat (S73).
    Suggestion,
}

/// What the page knows and UXDuck does not: where the person was.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackUi {
    /// The route, such as `/chats/$threadId` (ids removed).
    pub route: String,
    /// The side pane's tabs, when it is open.
    #[serde(default)]
    pub panes: Vec<String>,
    /// The vehicle the UI is focused on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub theme: String,
    pub density: String,
    pub map_engine: String,
    /// The last named things the person did, such as `open:map`; never text.
    #[serde(default)]
    pub actions: Vec<String>,
    pub app_version: String,
    pub locale: String,
    /// The browser and its major version, such as `Chrome 141`; absent in
    /// UXDuck Desktop's own window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
    /// The window's size in CSS pixels, such as `1512×945`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// The desktop app's version and capabilities, inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellInfo>,
    /// The same bounded snapshot About copies; absent when support details are excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<super::support::SupportDetails>,
    /// Each map on the page and what it drew under the vehicles (FB-23).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maps: Vec<FeedbackMap>,
    /// Where the page's map keys came from, and why the last fetch failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub map_keys: Vec<FeedbackMapKey>,
}

/// A map on the page: a blank map looks the same whether it had no key, its
/// requests were refused or unreachable, or WebGL lost its context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackMap {
    /// `maplibre` (the 2D map) or `cesium` (the 3D map).
    pub engine: String,
    /// `map`, or `globe` for the home's decorative one.
    pub variant: String,
    /// The map's size in CSS pixels, such as `822×165`.
    pub size: String,
    /// What it draws under the vehicles: `esri`, `esri-loading`,
    /// `esri-failed`, `no-key`, `google`, `tiles` or `none`.
    pub basemap: String,
    /// The GPU as the browser names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lost: Option<bool>,
    /// Drawing threw and the map stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<bool>,
    /// The last requests refused or failed, such as `failed
    /// basemapstyles-api.arcgis.com/arcgis/rest/services/styles/v2/styles/arcgis/imagery`:
    /// no query, and `{n}` for tile numbers.
    #[serde(default)]
    pub problems: Vec<String>,
}

/// A map key the page fetches from the UXDuck API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackMapKey {
    /// `arcgis` or `google`.
    pub name: String,
    /// `fetched`, `saved` (kept from an earlier fetch), `none`,
    /// `development` or `off`.
    pub state: String,
    /// The last fetch's failure, such as `503` or `unreachable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShellInfo {
    pub version: String,
    pub capabilities: Vec<String>,
}

/// An error the page saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageError {
    /// A known code, such as `map-unavailable`.
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
}

/// Sends a report to the team's Linear, with UXDuck's part of the bundle
/// added and scrubbed. While offline it waits in the outbox and the answer
/// is `queued`. Operators only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSendParams {
    /// A UUID made by the client, so a retry files nothing twice.
    pub report_id: String,
    pub kind: FeedbackKind,
    /// Up to 5,000 characters.
    pub text: String,
    /// A PNG, JPEG or WebP screenshot, base64, up to 3 MB decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
    /// The diagnostics bundle (on unless the person turned it off).
    pub include_details: bool,
    /// The last five minutes of this vehicle's raw tlog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_data_vehicle_id: Option<String>,
    /// This chat's messages, when opened from it and ticked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_thread_id: Option<String>,
    /// For a reply. Signed in, the API replies at the account's own address
    /// whatever this says; it only tells it that a reply is wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_email: Option<String>,
    /// The name the signed-in person goes by here, as Settings shows it, for
    /// the issue; the API cuts it at 80 characters and ignores it signed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporter_name: Option<String>,
    pub surface: Surface,
    pub ui: FeedbackUi,
    #[serde(default)]
    pub errors: Vec<PageError>,
    /// Credit the signed-in account by its username, and tell it when the
    /// suggestion ships (S73). Signed out, UXDuck tells this install instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub credit: bool,
    /// Written by the agent from a chat, as a suggestion card (S73).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_agent: bool,
    /// Files the person attached: up to 10, 25 MB together decoded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<FeedbackAttachment>,
    /// Recorded voice notes, up to 5, each up to 5 minutes (10 MB decoded).
    /// With one, the text may be empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voice_notes: Vec<FeedbackVoiceNote>,
    /// A random key the page keeps to add words and files to the report
    /// later; the API keeps only its hash. Redacted from traces and bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_key: Option<String>,
}

/// A file the person attached to a report: any type, checked by the API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackAttachment {
    /// The file's name on the person's computer.
    pub name: String,
    /// The file, base64.
    pub data: String,
}

/// A voice note the person recorded, with what the app wrote down as they
/// spoke.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackVoiceNote {
    /// A name for the recording, such as `voice-1.wav`.
    pub name: String,
    /// The recording, base64: a WAV or another audio format.
    pub data: String,
    /// The words the app transcribed live, up to 5,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// What the agent offers to suggest (S73): a card the person reads, edits
/// and sends, or not. Nothing is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSuggestParams {
    /// A short name for it, such as "Airspeed for rovers".
    pub title: String,
    /// What the person wants and why, in their words where possible, up to
    /// 2,000 characters.
    pub text: String,
}

/// The card shown in the chat; the person sends it with `feedback/send`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionCard {
    pub title: String,
    pub text: String,
    /// Always false: the card waits for the person.
    pub sent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackStatus {
    /// A new issue.
    Filed,
    /// This report, or the same words within the hour, was filed already.
    Duplicate,
    /// Kept for a person to file.
    Held,
    /// Offline: kept here and sent when UXDuck is online again.
    Queued,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSendResult {
    pub status: FeedbackStatus,
    /// The Linear issue, such as `FB-12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// What the voice note said, once the API has transcribed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// The account's feedback, newest first (`GET /v1/feedback/mine`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountFeedbackList {
    pub reports: Vec<AccountFeedbackReport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountFeedbackReport {
    pub id: String,
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Milliseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at: u64,
    /// `filed`, or `held` for a person to file.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// The attached files' names.
    pub files: Vec<String>,
    /// Adds to it from any computer the account is on.
    pub add_key: String,
    pub additions: Vec<AccountFeedbackAddition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountFeedbackAddition {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    pub files: Vec<String>,
}

/// Reports sent signed out, by the keys sent with them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimFeedbackParams {
    /// Up to 50.
    pub reports: Vec<FeedbackKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackKey {
    pub id: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimFeedbackResult {
    #[schemars(schema_with = "crate::safe::integer")]
    pub claimed: u64,
}

/// `feedback-bundle.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackBundle {
    /// Always [`BUNDLE_SCHEMA`].
    pub schema: String,
    pub report: BundleReport,
    pub ui: FeedbackUi,
    /// Absent when sent from a page with nothing installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<BundleDaemon>,
    /// This client's recent RPC frames, safe side only (ADR-0001 D9).
    #[serde(default)]
    pub trace: Vec<TraceFrame>,
    #[serde(default)]
    pub errors: Vec<PageError>,
    /// UXDuck's last log lines.
    #[serde(default)]
    pub log: Vec<String>,
    /// Only when "Include this chat" was ticked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<Vec<BundleChatItem>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleReport {
    pub id: String,
    pub kind: FeedbackKind,
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    pub surface: Surface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleDaemon {
    #[schemars(schema_with = "crate::safe::integer")]
    pub seq: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub uptime_s: u64,
    pub vehicles: Vec<BundleVehicle>,
    pub links: Vec<BundleLink>,
    pub notifications: Vec<BundleNotification>,
    /// The daemon's settings, as the Settings page shows them.
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleVehicle {
    /// So the UI's `vehicleId` can be matched; not the UID.
    pub id: String,
    pub autopilot: String,
    pub vehicle_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    pub presence: String,
    pub armed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The kind of link it is heard on, such as `udp-listen`.
    pub link: String,
}

/// A link's kind and state; never its address, port or device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleLink {
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleNotification {
    pub severity: String,
    pub title: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleChatItem {
    /// `user-message`, `agent-message`, `tool-call` or `error`.
    pub kind: String,
    pub text: String,
}

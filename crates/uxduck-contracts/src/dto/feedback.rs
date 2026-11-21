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
    /// The map's size in CSS pixels, such as `822×165`; a closed map's as
    /// it was last laid out.
    pub size: String,
    /// Its canvas: one whose size differs from `size` was not resized with
    /// the map and draws a stale picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<FeedbackCanvas>,
    /// What it draws under the vehicles: `esri`, `esri-loading`,
    /// `esri-failed`, `no-key`, `google`, `tiles`, `custom` (a tile server
    /// the person added) or `none`.
    pub basemap: String,
    /// The GPU as the browser names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    /// WebGL lost the map's context and has not given it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lost: Option<bool>,
    /// When WebGL last lost the map's context, in microseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub context_lost_at_us: Option<u64>,
    /// When the browser last gave the context back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub context_restored_at_us: Option<u64>,
    /// Drawing threw and the map stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<bool>,
    /// When the map last drew a frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_frame_at_us: Option<u64>,
    /// Frames drawn in the five seconds before the report, or before it
    /// closed. A healthy 2D map draws none while nothing on it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_frames: Option<u32>,
    /// The last failures, as `MapFailure` and `MapProvider` name them with
    /// the HTTP status, such as `tile-load esri 401`. No address or error
    /// text.
    #[serde(default)]
    pub problems: Vec<String>,
    /// The last failure, by kind only (FB-285).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<FeedbackMapError>,
    /// When the map was closed: a map closed in the five minutes before the
    /// report keeps its last record, so one closed to recover from a
    /// problem still says what it was (FB-285).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub closed_at_us: Option<u64>,
}

/// A map's canvas: its size in CSS pixels, and its drawing buffer's in
/// device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackCanvas {
    pub width: u32,
    pub height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// A map's failure as feedback keeps it: its kind, whose it was and the
/// HTTP status. Never an address or an error's text, which can hold a key
/// or a place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackMapError {
    pub code: MapFailure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<MapProvider>,
    /// An HTTP status, 100 to 599. The daemon leaves out any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 100, max = 599))]
    pub status: Option<u16>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
}

/// What failed on a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MapFailure {
    /// A tile, or ground heights.
    TileLoad,
    /// The basemap's style or service.
    StyleLoad,
    WebglContext,
    /// Drawing threw and the map stopped.
    Render,
    Worker,
    /// Anything else, or a kind this UXDuck does not know.
    #[serde(other)]
    Other,
}

/// Whose request failed, by its host alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MapProvider {
    Esri,
    Google,
    Osm,
    /// Ground heights.
    Terrain,
    /// A tile server the person added.
    Custom,
    /// Anyone else, or one this UXDuck does not know.
    #[serde(other)]
    Other,
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
    /// The last five minutes of this vehicle's raw tlog: the newest of them
    /// when they are more than UXDuck's servers take, with what did not go
    /// in the result's `left_out`.
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

/// A report the agent files by itself (S294), from a chat or over MCP: what
/// is broken, missing or awkward in UXDuck, in words about UXDuck alone.
/// Only these words are sent, anonymously: no screenshot, chat, technical
/// details, flight data or account go with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackFileParams {
    /// problem, suggestion (missing) or idea (a change).
    pub kind: FeedbackKind,
    /// A short name for it.
    pub title: String,
    /// What happened or what they want, and why; up to 2,000 characters.
    pub text: String,
    /// The chat it is filed from; the agent's host fills it in. An agent
    /// outside UXDuck, such as an MCP client, has none: the report is its
    /// connection's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A report the agent filed (S294).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackFiled {
    /// The report, for `feedback/add`.
    pub report_id: String,
    pub status: FeedbackStatus,
    /// The Linear issue, such as `FB-12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
}

/// More for a report filed from the same chat or MCP connection (S294), as
/// a comment on its issue: what the person said since, or a file they chose
/// to add.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackAddParams {
    /// From `feedback/file`.
    pub report_id: String,
    /// Up to 2,000 characters.
    #[serde(default)]
    pub text: String,
    /// The chat; the agent's host fills it in. An MCP client has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Files the person added from the chat's card, such as a screenshot.
    /// Operators only: the agent cannot attach anything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<FeedbackAttachment>,
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
    /// Safely stored by UXDuck; background delivery to Linear is pending.
    Received,
    /// A new issue.
    Filed,
    /// This report, or the same words within the hour, was filed already.
    Duplicate,
    /// Kept for a person to file.
    Held,
    /// Kept here and sent once UXDuck's servers can take it: offline, or
    /// limited (see `FeedbackSendResult::limited`).
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
    /// Queued because UXDuck's servers take no more reports from this
    /// network for now, not because they could not be reached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limited: bool,
    /// What UXDuck left out of the report to fit what its servers take,
    /// for the person who sent it. Absent when all of it went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<FeedbackLeftOut>,
}

/// What UXDuck left out of a report, always the oldest of a part, because
/// its servers take no more of that part. Told to the person who sent the
/// report (`FeedbackSendResult`) and kept in the bundle for whoever reads
/// it (`FeedbackBundle`). A part absent here went whole.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackLeftOut {
    /// The bundle's oldest trace frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<LeftOut>,
    /// The bundle's oldest log lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LeftOut>,
    /// The bundle's oldest chat items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<LeftOut>,
    /// The bundle's oldest page errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<LeftOut>,
    /// The bundle's oldest notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notifications: Option<LeftOut>,
    /// The oldest frames of the flight data, `flight.tlog`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight: Option<LeftOut>,
    /// Why the flight data the person asked for did not go at all, in
    /// words for them: UXDuck's logs held none of the vehicle's last five
    /// minutes, or one of them could not be read. The rest of the report
    /// went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_flight: Option<String>,
}

/// The oldest of one part of a report, left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeftOut {
    /// How many: frames, lines, items, errors or notifications.
    #[schemars(schema_with = "crate::safe::integer")]
    pub count: u64,
    /// Their size in bytes.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// When the oldest and the newest of them were, in microseconds since
    /// the Unix epoch. Absent for log lines and chat items, which carry
    /// their times in their text or not at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub from_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub to_us: Option<u64>,
    /// The most UXDuck's servers take, in bytes, of what this is part of:
    /// the bundle, or the flight data.
    #[schemars(schema_with = "crate::safe::integer")]
    pub limit_bytes: u64,
}

/// How much of a report UXDuck has sent to its servers so far
/// (`feedback/progress`), for the page waiting on its `feedback/send`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackProgress {
    pub report_id: String,
    /// Bytes sent.
    #[schemars(schema_with = "crate::safe::integer")]
    pub sent: u64,
    /// The whole upload's bytes: the report, the bundle and every file.
    #[schemars(schema_with = "crate::safe::integer")]
    pub total: u64,
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
    /// What UXDuck left out of this report to fit what its servers take:
    /// how much of each part, and from when to when. Absent when all of it
    /// went.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<FeedbackLeftOut>,
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
    /// Every route it is heard on, and which one carries its commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<BundleConnections>,
}

/// A vehicle's routes and why one is selected. Never a route's peer address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleConnections {
    pub command_available: bool,
    pub reason: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub epoch: u64,
    pub routes: Vec<BundleRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent transport capability and selection evidence"
)]
pub struct BundleRoute {
    /// The link's `id` in `links`.
    pub link_id: String,
    pub system_id: u8,
    pub priority: u32,
    pub reachable: bool,
    pub writable: bool,
    pub selected: bool,
    pub telemetry: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_heartbeat_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub round_trip_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_per_mille: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_receive_age_ms: Option<u64>,
    pub reason: String,
}

/// A link's kind and state; never its address, port or device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleLink {
    /// So the trace and the vehicles' routes can be matched.
    #[serde(default)]
    pub id: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A UDP link's socket, as `link/diagnose` shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<BundleTransport>,
}

/// A socket's state, attempts and failures. Never its configured
/// destination, its endpoints or the endpoint a failure names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleTransport {
    #[schemars(schema_with = "crate::safe::integer")]
    pub generation: u64,
    pub socket_open: bool,
    pub wildcard_bind: bool,
    /// Confirmed by Network.framework in the daemon process, or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_network_permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_receive_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_valid_mavlink_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_heartbeat_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_send_attempt_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_socket_send_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_error: Option<BundleTransportError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_error: Option<BundleTransportError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_error: Option<BundleTransportError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_reason: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub registration_attempts: u64,
    pub registration_suppressed: bool,
    #[schemars(schema_with = "crate::safe::integer")]
    pub recreation_attempts: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub backoff_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub next_retry_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub dropped_receive_events: u64,
    /// The recent socket sends, oldest first.
    pub sends: Vec<BundleSocketSend>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleTransportError {
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub operation: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
    pub message: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleSocketSend {
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub registration: bool,
    /// attempted, accepted-by-os, or failed. Never claims peer delivery.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// FB-285: what a page says about a map's failure reaches a report only
    /// as a kind, whose it was and a status. Words or an address in their
    /// place are no kind UXDuck knows, and a canvas is numbers.
    #[test]
    fn a_maps_failure_and_canvas_keep_their_shape() {
        let map: FeedbackMap = serde_json::from_value(serde_json::json!({
            "engine": "maplibre",
            "variant": "map",
            "size": "822×165",
            "basemap": "esri",
            "lastError": {
                "code": "Forbidden: https://tiles.example.com/abc123/1/2/3.png?key=secret",
                "provider": "San Francisco",
                "status": 403,
                "atUs": 1_763_700_000_000_000_u64,
            },
        }))
        .unwrap();
        let error = map.last_error.unwrap();
        assert_eq!(error.code, MapFailure::Other);
        assert_eq!(error.provider, Some(MapProvider::Other));
        assert_eq!(error.status, Some(403));
        assert!(!serde_json::to_string(&error).unwrap().contains("secret"));

        for wrong in [
            serde_json::json!({ "code": "tile-load", "atUs": 1, "status": "403 https://x" }),
            serde_json::json!({ "code": { "text": "secret" }, "atUs": 1 }),
        ] {
            assert!(serde_json::from_value::<FeedbackMapError>(wrong).is_err());
        }
        assert!(
            serde_json::from_value::<FeedbackCanvas>(serde_json::json!({
                "width": "822 at San Francisco", "height": 165, "pixelWidth": 1644, "pixelHeight": 330,
            }))
            .is_err()
        );
    }
}

//! A flight played back on one clock (ADR-0039): what is open, where the
//! clock is and what is on the bar. Live state: kept in memory, never
//! journaled; pushed as `replay` about four times a second while it plays.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `replay/open`: one of a flight, a file or a log's bytes. Replaces an
/// open replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayOpenParams {
    /// A flight from `flight/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// Several flights played together on one clock (R17).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flight_ids: Vec<String>,
    /// An operation's flights, played together (R16, R17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// A tlog or `.bin` file on this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A log's bytes in base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// A vehicle's file copied in Files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_file: Option<ReplayVehicleFile>,
    /// A flight log in the account, from `account/flights`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<ReplayAccountLog>,
    /// A tlog or `.bin` the operator attached to a chat message (S246): its
    /// attachment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
    /// The name shown, with `data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Opens at this time on its clock rather than at its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub time_us: Option<u64>,
    /// A flight with its disarmed time (ADR-0050 L10): from after the
    /// vehicle's previous flight or its log's start, to the next flight's
    /// lead-in or its log's end. Else from 10 s before arming to 10 s after
    /// disarming, which the app calls "Armed only".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ground: bool,
}

/// A flight log in the account (S82): joined from its chunks on this
/// computer, kept for 14 days unused (R12).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayAccountLog {
    pub device_id: String,
    pub id: String,
}

/// A copy of a vehicle's file, made in Files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayVehicleFile {
    pub vehicle_id: String,
    /// Its path on the vehicle, such as `/APM/LOGS/00000012.BIN`.
    pub path: String,
}

/// A vehicle's SD card plugged into this computer, with logs in its
/// `APM/LOGS` folder (R15).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogCard {
    /// Where it is mounted, as `logs/copyCard` takes it.
    pub path: String,
    pub name: String,
    pub logs: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

/// A log copied off a card, to replay by its `path`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CopiedLog {
    pub path: String,
    pub name: String,
    /// The card and the day it was copied, such as "NO NAME-2026-10-28".
    pub card: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
}

/// Result of `logs/cards` and `logs/copyCard`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogCardsResult {
    pub cards: Vec<LogCard>,
    /// Newest first.
    pub copied: Vec<CopiedLog>,
    /// The card being copied, or copied last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copying: Option<String>,
    pub done: u32,
    pub of: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `logs/copyCard`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogCopyCardParams {
    /// The card, as `logs/cards` names it.
    pub path: String,
}

/// `flight/series`: a flight's values over its whole range, decoded from its
/// log without opening a replay (R13). A flight, or a vehicle of the replay
/// open now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightSeriesParams {
    /// A flight from `flight/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// A replay vehicle of the replay open now: its whole range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Telemetry keys, such as `battery.voltage_v` or `vibration.z`, and
    /// `position.fused.<frame>` for a height (`above-home`, `amsl`,
    /// `above-ground`). A replay vehicle from a DataFlash log also takes the
    /// log's own fields as `replay/logFields` lists them, at the rate they
    /// were logged: `df.<MESSAGE>.<Column>`, such as `df.XKF4.SV`, or
    /// `df.<MESSAGE>[<instance>].<Column>`, such as `df.VIBE[1].VibeZ`. At
    /// most 16.
    pub keys: Vec<String>,
    /// At most this many points a series, the highs and lows kept: 1 to
    /// 3000, 1000 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_points: Option<u32>,
}

/// One value over time.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightSample {
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    pub value: f64,
}

/// One key's values, with its lowest and highest and when.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightSeries {
    pub key: String,
    pub samples: Vec<FlightSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<FlightSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<FlightSample>,
}

/// Result of `flight/series`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightSeriesResult {
    #[schemars(schema_with = "crate::safe::integer")]
    pub start_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub end_us: u64,
    pub series: Vec<FlightSeries>,
    /// Keys the log never gave a value for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_keys: Vec<String>,
}

/// `replay/logFields`: what a replay vehicle's DataFlash log holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayLogFieldsParams {
    /// A replay vehicle of the replay open now; the focused one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// One column of a logged message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogColumn {
    pub name: String,
    /// Its unit as the log names it, such as "m/s", "deg" or "V".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// One message type a DataFlash log holds, as logged.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogMessage {
    /// Such as `VIBE` or `XKF4`.
    pub name: String,
    pub columns: Vec<LogColumn>,
    /// Its instances (two IMUs: `[0, 1]`), each a key of its own as
    /// `df.VIBE[1].VibeZ`; empty for a message of one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<u32>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub records: u64,
    /// Records a second over the time it was written, its instances together.
    pub rate_hz: f64,
}

/// Result of `replay/logFields`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayLogFieldsResult {
    /// The log's name, such as `00000012.BIN`.
    pub name: String,
    pub messages: Vec<LogMessage>,
}

/// `replay/motion` (maintainer, 2026-11-02): where each replay vehicle
/// was from `fromUs` to `toUs` on the replay's clock, every position as
/// logged, so a map draws it between the two around any moment, at any
/// speed, rather than carrying it on from its last telemetry update.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayMotionParams {
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub to_us: u64,
}

/// A rotation from the vehicle's body axes (forward, right, down) to
/// north, east and down, as a unit quaternion: the attitude its model is
/// turned to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayAttitude {
    pub w: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A replay vehicle as logged at one moment: where it was, as the maps
/// place it, and which way it faced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayMotionSample {
    /// When its position was logged, on the replay's clock.
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Its height in each frame the position gives.
    pub altitudes: Vec<crate::dto::telemetry::Altitude>,
    /// Degrees from true north: the position's own, else the attitude's yaw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// Its attitude then, when the log gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attitude: Option<ReplayAttitude>,
}

/// One replay vehicle's samples, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayMotion {
    pub vehicle_id: String,
    pub samples: Vec<ReplayMotionSample>,
}

/// Result of `replay/motion`: each vehicle's samples from `fromUs` to
/// `toUs`, with the last before `fromUs` and the first after `toUs`, so a
/// moment at either end lies between two as well. `toUs` is earlier than
/// asked for when more than ten minutes were asked for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayMotionResult {
    /// The replay's generation (R4): samples of another are not this replay's.
    #[schemars(schema_with = "crate::safe::integer")]
    pub generation: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub to_us: u64,
    pub vehicles: Vec<ReplayMotion>,
}

/// A folder another ground station keeps its tlogs in, looked in for
/// flights (R18).
#[allow(
    clippy::struct_excessive_bools,
    reason = "a folder's switches and states"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightFolder {
    /// `qgroundcontrol`, `mission-planner`, or the folder's path for one
    /// added with "Also look in…".
    pub id: String,
    /// "QGroundControl", "Mission Planner", or the folder's name.
    pub name: String,
    pub path: String,
    /// Looked in.
    pub on: bool,
    /// It is there on this computer.
    pub exists: bool,
    /// Added by the person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added: bool,
    /// Looked through with all its subfolders: an added folder's choice, for
    /// a custom ground station's folders or a disk of logs (maintainer,
    /// 2026-10-29). The apps' own folders are looked through as the apps
    /// keep them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deep: bool,
    /// The last look through its whole tree stopped at the most entries a
    /// look reads (250,000): a smaller folder is needed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
}

/// A flight found in another app's tlog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoundFlight {
    /// The folder it was found in, by id.
    pub folder: String,
    /// The tlog, to replay with `replay/open`.
    pub path: String,
    pub file: String,
    pub system_id: u8,
    /// `MAV_TYPE`, such as `Quadrotor`.
    pub vehicle_type: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub armed_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    /// The other app is still writing it: not replayed until it closes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
}

/// Result of `flights/found` and `flights/folder`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoundFlightsResult {
    pub folders: Vec<FlightFolder>,
    /// Newest first.
    pub flights: Vec<FoundFlight>,
    /// A look through the folders is under way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scanning: bool,
}

/// `flights/folder`: a folder looked in or not, added or removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlightFolderParams {
    /// A folder's id from `flights/found`, or a path to add.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    /// Removes a folder the person added.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remove: bool,
    /// Looks through all of an added folder's subfolders (true), or four
    /// levels as the apps' folders are (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep: Option<bool>,
}

/// `replay/speed`: how fast the clock runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySpeedParams {
    /// 0.1 to 16 times real time; the bar offers 0.25, 0.5, 1, 2, 5, 10
    /// and 16.
    pub speed: f64,
}

/// `replay/rename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRenameParams {
    /// Its new name; empty goes back to the one it is given.
    pub name: String,
}

/// `replay/seek`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySeekParams {
    /// On the replay's clock, between `startUs` and `endUs`.
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
}

/// What the replay's times count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayClock {
    /// Microseconds since the Unix epoch on the computer that recorded it.
    #[default]
    Wall,
    /// The log's stamps were nonsense: microseconds since the vehicle
    /// booted, and the bar shows "+m:ss" alone (R11).
    Boot,
}

/// Where a replay's records come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReplaySourceKind {
    /// A flight this computer recorded.
    #[default]
    Flight,
    /// A tlog file.
    Tlog,
    /// The autopilot's own DataFlash log (`.bin`), R15.
    Log,
}

/// How a source's clock was matched to the replay's (R17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayMatch {
    /// Recorded on this computer, on its clock: nothing to match.
    #[default]
    SameComputer,
    /// Its vehicles sent GPS time (`SYSTEM_TIME`): matched by it.
    Gps,
    /// From elsewhere with no GPS time: on its own stamps until nudged.
    NotMatched,
}

/// A stretch of the range no source covers, drawn and skipped (R17).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGap {
    #[schemars(schema_with = "crate::safe::integer")]
    pub from_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub to_us: u64,
}

/// `replay/nudge`: moves one source's clock against the others.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayNudgeParams {
    /// The source, by its place in `sources`.
    pub source: u32,
    /// Microseconds later (positive) or earlier.
    #[schemars(schema_with = "crate::safe::signed_integer")]
    pub by_us: i64,
}

/// One source of a replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplaySource {
    pub kind: ReplaySourceKind,
    /// The flight's vehicle, or the file's name.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flight_id: Option<String>,
    /// A flight's vehicle, live: where a video added to it plays (R8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What a DataFlash log said of its vehicle: its system id and its
    /// firmware, such as "`System 1 · ArduCopter V4.5.1 (abcdef12)`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub start_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub end_us: u64,
    /// How its clock was matched to the replay's; absent, recorded on this
    /// computer.
    #[serde(default, skip_serializing_if = "same_computer")]
    pub matched: ReplayMatch,
    /// Added to its stamps: by GPS time and any nudge.
    #[serde(default, skip_serializing_if = "is_zero")]
    #[schemars(schema_with = "crate::safe::signed_integer")]
    pub offset_us: i64,
    /// Of which its nudges.
    #[serde(default, skip_serializing_if = "is_zero")]
    #[schemars(schema_with = "crate::safe::signed_integer")]
    pub nudge_us: i64,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip test")]
fn same_computer(matched: &ReplayMatch) -> bool {
    *matched == ReplayMatch::SameComputer
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's skip test")]
fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A vehicle heard in the replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayVehicle {
    /// The replay vehicle's id: the same for the same vehicle every time
    /// its flights are opened.
    pub id: String,
    /// The vehicle it was, live.
    pub original_id: String,
    pub name: String,
}

/// What a mark on the scrubber is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayMarkKind {
    #[default]
    Armed,
    Disarmed,
    /// Off the ground, `atUs` to `untilUs`: the solid part.
    Air,
    /// A mode, `atUs` to `untilUs`, named in `text`.
    Mode,
    /// A message of warning or worse.
    Message,
    /// A setting changed in the log (ADR-0050 L9), described in `text`:
    /// "`RTL_ALT` 1500 → 3000 by UXDuck". Statistics are not marked.
    Parameter,
    /// A mission step reached (`MISSION_ITEM_REACHED`, S343), said in
    /// `text`: "Reached step 3", counted as the Cockpit counts steps.
    Step,
}

/// One mark on the scrubber (R6), from the pass over the range at open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayMark {
    pub kind: ReplayMarkKind,
    /// The replay vehicle it is of.
    pub vehicle_id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub until_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A message's `MAV_SEVERITY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u8>,
}

/// A camera recording whose range meets the replay's (R7).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRecording {
    pub id: String,
    pub source_id: String,
    /// The replay vehicle it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// On the replay's clock.
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub ended_at_us: u64,
    /// A video added from elsewhere (R8): its file's name and how its start
    /// was placed, so the Camera tab offers to align it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aligned_by: Option<crate::dto::video::AlignedBy>,
    /// "H.264", "HEVC"…, when known: said if the browser cannot play it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

/// A point of a vehicle's whole path, drawn faint on both maps.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayPathPoint {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Above mean sea level, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amsl_m: Option<f64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
}

/// One replay vehicle's whole path, at most a couple of thousand points.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayPath {
    pub vehicle_id: String,
    pub points: Vec<ReplayPathPoint>,
}

/// The replay: pushed as `replay` while one is open and when it closes,
/// and the result of every `replay/*` method.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplayState {
    /// False once closed, or when none was opened.
    pub open: bool,
    /// Its name, when a person gave one (ADR-0039 R21): an operation's
    /// name, or one given with `replay/rename`. Absent, a client names it
    /// after what it plays: its vehicle and when, or the day and its
    /// vehicles; never a file's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub clock: ReplayClock,
    /// The range: the flight with 10 s before and after.
    #[schemars(schema_with = "crate::safe::integer")]
    pub start_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub end_us: u64,
    /// The clock, T: everything shows each vehicle as it was then.
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
    /// When T was read, on this computer's clock, so a client runs the
    /// clock on between pushes.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub speed: f64,
    pub playing: bool,
    /// Raised when the clock went back and the vehicles were rebuilt: a
    /// client clears what it gathered itself, such as the track.
    #[schemars(schema_with = "crate::safe::integer")]
    pub generation: u64,
    pub sources: Vec<ReplaySource>,
    pub vehicles: Vec<ReplayVehicle>,
    /// The vehicle to select: the flight's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_vehicle_id: Option<String>,
    pub marks: Vec<ReplayMark>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recordings: Vec<ReplayRecording>,
    /// Stretches between flights no source covers (R17).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<ReplayGap>,
    /// The flights of a log saved to the account (`replay/save`), by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub saved_flight_ids: Vec<String>,
    /// Each vehicle's whole path: in the methods' results, never pushed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<ReplayPath>>,
}
